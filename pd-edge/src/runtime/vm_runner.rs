use parking_lot::Mutex;
use std::time::Duration;
use tokio::{
    runtime::Handle,
    task::JoinHandle,
    time::{Instant, MissedTickBehavior, interval_at},
};
use tracing::warn;
use vm::{EpochHandle, Store, Vm, VmStatus, VmYieldReason};

use super::{
    LoadedProgram, VM_EPOCH_TICK_INTERVAL_MS, VmExecutionConfig, VmExecutionMode, VmInterruptConfig,
};
use crate::{
    abi_impl::{
        SharedProxyVmContext, SharedVmAsyncOps, VmAsyncOpBridge, enter_edge_host_context,
        new_shared_vm_async_ops,
    },
    debug_session::{SharedDebugSession, run_vm_with_optional_debugger},
    logging::category_program,
};

pub type HostModuleRegistrar =
    fn(&mut Vm, SharedProxyVmContext, SharedVmAsyncOps) -> Result<(), vm::VmError>;

#[derive(Clone)]
struct VmRunnerStoreData {
    vm_context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
}

impl VmRunnerStoreData {
    fn new(vm_context: SharedProxyVmContext, async_ops: SharedVmAsyncOps) -> Self {
        Self {
            vm_context,
            async_ops,
        }
    }
}

type VmRunnerStore = Store<VmRunnerStoreData>;

const VM_POOL_MAX_PER_BUCKET: usize = 256;

#[derive(Default)]
struct VmPoolBuckets {
    aot_enabled: Vec<Vm>,
    baseline: Vec<Vm>,
    jit_disabled: Vec<Vm>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VmPoolKey {
    prefer_aot: bool,
    jit_enabled: bool,
}

impl VmPoolKey {
    const fn new(prefer_aot: bool, jit_enabled: bool) -> Self {
        Self {
            prefer_aot,
            jit_enabled,
        }
    }

    fn bucket_mut<'a>(&self, buckets: &'a mut VmPoolBuckets) -> &'a mut Vec<Vm> {
        match (self.jit_enabled, self.prefer_aot) {
            (false, _) => &mut buckets.jit_disabled,
            (true, true) => &mut buckets.aot_enabled,
            (true, false) => &mut buckets.baseline,
        }
    }
}

pub(crate) struct LoadedProgramVmPool {
    buckets: Mutex<VmPoolBuckets>,
}

impl LoadedProgramVmPool {
    pub(crate) fn new() -> Self {
        Self {
            buckets: Mutex::new(VmPoolBuckets::default()),
        }
    }

    fn take(&self, key: VmPoolKey) -> Option<Vm> {
        key.bucket_mut(&mut self.buckets.lock()).pop()
    }

    fn put(&self, key: VmPoolKey, vm: Vm) {
        let mut buckets = self.buckets.lock();
        let bucket = key.bucket_mut(&mut buckets);
        if bucket.len() < VM_POOL_MAX_PER_BUCKET {
            bucket.push(vm);
        }
    }
}

struct AcquiredVmRunnerStore {
    vm_store: VmRunnerStore,
    pool_key: Option<VmPoolKey>,
}

enum ActiveVmInterrupt {
    None,
    Fuel {
        fuel_per_yield: u64,
    },
    Epoch {
        ticks_per_slice: u64,
        driver: EpochInterruptionDriver,
    },
}

struct EpochInterruptionDriver {
    task: Option<JoinHandle<()>>,
}

impl EpochInterruptionDriver {
    fn new(epoch_handle: EpochHandle) -> Self {
        let task = Handle::current().spawn(async move {
            let period = Duration::from_millis(VM_EPOCH_TICK_INTERVAL_MS);
            let mut ticker = interval_at(Instant::now() + period, period);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Burst);
            loop {
                ticker.tick().await;
                epoch_handle.increment();
            }
        });
        Self { task: Some(task) }
    }
}

impl Drop for EpochInterruptionDriver {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Debug)]
pub enum VmExecutionError {
    HostRegistration(vm::VmError),
    Vm(vm::VmError),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct VmDebugInvocation {
    pub attach_debugger: bool,
    pub force_threading: bool,
}

pub async fn execute_vm_with_context(
    program: &LoadedProgram,
    vm_context: SharedProxyVmContext,
    debug_session: SharedDebugSession,
    debug: VmDebugInvocation,
    register_host_modules: HostModuleRegistrar,
    vm_execution: VmExecutionConfig,
) -> Result<(), VmExecutionError> {
    let prefer_aot =
        !debug.attach_debugger && matches!(vm_execution.interrupt, VmInterruptConfig::None);
    let AcquiredVmRunnerStore { vm_store, pool_key } = acquire_vm_runner_store(
        program,
        vm_context,
        register_host_modules,
        prefer_aot,
        vm_execution.jit_enabled,
        vm_execution.drop_contract_events_enabled,
        !debug.attach_debugger,
    )?;
    let execution_mode = if debug.force_threading {
        VmExecutionMode::Threading
    } else {
        vm_execution.execution_mode
    };

    let vm_store = match execution_mode {
        VmExecutionMode::Async => {
            execute_vm_with_async_mode(vm_store, debug_session, debug, vm_execution).await
        }
        VmExecutionMode::Threading => {
            execute_vm_with_threading_mode(vm_store, debug_session, debug, vm_execution).await
        }
    }?;

    if let Some(pool_key) = pool_key {
        recycle_vm_runner_store(program, pool_key, vm_store);
    }

    Ok(())
}

async fn execute_vm_with_async_mode(
    vm_store: VmRunnerStore,
    debug_session: SharedDebugSession,
    debug: VmDebugInvocation,
    vm_execution: VmExecutionConfig,
) -> Result<VmRunnerStore, VmExecutionError> {
    let started = std::time::Instant::now();
    let request_id = tail_profile_request_id(&vm_store);
    let mut profile = run_vm_async(vm_store, debug_session, debug, vm_execution).await?;

    profile.queue_wait_us = 0;
    profile.blocking_run_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    maybe_log_tail_profile(request_id.as_deref(), &profile);
    Ok(profile.vm_store)
}

async fn execute_vm_with_threading_mode(
    vm_store: VmRunnerStore,
    debug_session: SharedDebugSession,
    debug: VmDebugInvocation,
    vm_execution: VmExecutionConfig,
) -> Result<VmRunnerStore, VmExecutionError> {
    let queued_at = std::time::Instant::now();
    let request_id = tail_profile_request_id(&vm_store);
    let task = tokio::task::spawn_blocking(move || {
        let queue_wait_us = u64::try_from(queued_at.elapsed().as_micros()).unwrap_or(u64::MAX);
        let threading_started = std::time::Instant::now();
        let result = run_vm_threading(vm_store, debug_session, debug, vm_execution);

        match result {
            Ok(mut profile) => {
                profile.queue_wait_us = queue_wait_us;
                profile.blocking_run_us =
                    u64::try_from(threading_started.elapsed().as_micros()).unwrap_or(u64::MAX);
                maybe_log_tail_profile(request_id.as_deref(), &profile);
                Ok(profile.vm_store)
            }
            Err(err) => Err(err),
        }
    });

    task.await.map_err(|err| {
        VmExecutionError::Vm(vm::VmError::HostError(format!(
            "vm threading execution task failed: {err}"
        )))
    })?
}

async fn run_vm_async(
    mut vm_store: VmRunnerStore,
    debug_session: SharedDebugSession,
    debug: VmDebugInvocation,
    vm_execution: VmExecutionConfig,
) -> Result<VmExecutionProfile, VmExecutionError> {
    let interrupt = configure_vm_interrupt(&mut vm_store, vm_execution)?;
    let mut profile = VmExecutionProfile::default_with_store(vm_store);

    loop {
        arm_epoch_interrupt_if_enabled(&mut profile.vm_store, &interrupt, debug.attach_debugger)?;
        let status = {
            let _host_context = enter_edge_host_context(
                profile.vm_store.data().vm_context.clone(),
                profile.vm_store.data().async_ops.clone(),
            );
            if debug.attach_debugger {
                let vm_context = profile.vm_store.data().vm_context.clone();
                run_vm_with_optional_debugger(&debug_session, vm_context, profile.vm_store.vm_mut())
            } else {
                profile.vm_store.run()
            }
        }
        .map_err(VmExecutionError::Vm)?;
        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                profile.vm_yield_count = profile.vm_yield_count.saturating_add(1);
                if handle_interrupt_yield(&mut profile.vm_store, &interrupt)? {
                    profile.vm_fuel_recharge_count =
                        profile.vm_fuel_recharge_count.saturating_add(1);
                }
                tokio::task::yield_now().await;
            }
            VmStatus::Waiting(_op_id) => {
                let waiting_started = std::time::Instant::now();
                profile
                    .vm_store
                    .vm_mut()
                    .await_waiting_host_op()
                    .await
                    .map_err(VmExecutionError::Vm)?;
                let wait_us =
                    u64::try_from(waiting_started.elapsed().as_micros()).unwrap_or(u64::MAX);
                profile.waiting_host_us = profile.waiting_host_us.saturating_add(wait_us);
                profile.waiting_host_count = profile.waiting_host_count.saturating_add(1);
            }
        }
    }

    Ok(profile)
}

fn run_vm_threading(
    mut vm_store: VmRunnerStore,
    debug_session: SharedDebugSession,
    debug: VmDebugInvocation,
    vm_execution: VmExecutionConfig,
) -> Result<VmExecutionProfile, VmExecutionError> {
    let interrupt = configure_vm_interrupt(&mut vm_store, vm_execution)?;
    let mut profile = VmExecutionProfile::default_with_store(vm_store);

    loop {
        arm_epoch_interrupt_if_enabled(&mut profile.vm_store, &interrupt, debug.attach_debugger)?;
        let status = {
            let _host_context = enter_edge_host_context(
                profile.vm_store.data().vm_context.clone(),
                profile.vm_store.data().async_ops.clone(),
            );
            if debug.attach_debugger {
                let vm_context = profile.vm_store.data().vm_context.clone();
                run_vm_with_optional_debugger(&debug_session, vm_context, profile.vm_store.vm_mut())
            } else {
                profile.vm_store.run()
            }
        }
        .map_err(VmExecutionError::Vm)?;

        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                profile.vm_yield_count = profile.vm_yield_count.saturating_add(1);
                if handle_interrupt_yield(&mut profile.vm_store, &interrupt)? {
                    profile.vm_fuel_recharge_count =
                        profile.vm_fuel_recharge_count.saturating_add(1);
                }
                std::thread::yield_now();
            }
            VmStatus::Waiting(_op_id) => {
                let waiting_started = std::time::Instant::now();
                tokio::runtime::Handle::current()
                    .block_on(profile.vm_store.vm_mut().await_waiting_host_op())
                    .map_err(VmExecutionError::Vm)?;
                let wait_us =
                    u64::try_from(waiting_started.elapsed().as_micros()).unwrap_or(u64::MAX);
                profile.waiting_host_us = profile.waiting_host_us.saturating_add(wait_us);
                profile.waiting_host_count = profile.waiting_host_count.saturating_add(1);
            }
        }
    }

    Ok(profile)
}

fn new_vm_runner_store(
    program: &LoadedProgram,
    vm_context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
    prefer_aot: bool,
    jit_enabled: bool,
    drop_contract_events_enabled: bool,
) -> VmRunnerStore {
    let prefer_aot = prefer_aot
        && jit_enabled
        && !drop_contract_events_enabled
        && std::env::var_os("PD_EDGE_DISABLE_NO_INTERRUPT_AOT").is_none();
    let mut vm = if prefer_aot {
        if let Some(bundle) = program.no_interrupt_aot_bundle.as_ref() {
            match Vm::from_aot_bundle_bytes(bundle.as_ref().as_slice()) {
                Ok(vm) => vm,
                Err(_) => {
                    let mut vm = Vm::new_shared(program.program.clone());
                    let _ = vm.install_aot_bundle_bytes(bundle.as_ref().as_slice());
                    vm
                }
            }
        } else {
            Vm::new_shared(program.program.clone())
        }
    } else {
        Vm::new_shared(program.program.clone())
    };
    vm.set_drop_contract_events_enabled(drop_contract_events_enabled);
    if !jit_enabled {
        let mut jit_config = *vm.jit_config();
        jit_config.enabled = false;
        vm.set_jit_config(jit_config);
    }
    vm.set_async_bridge(Box::new(VmAsyncOpBridge::new(async_ops.clone())));
    Store::new(vm, VmRunnerStoreData::new(vm_context, async_ops))
}

fn register_host_modules_from_store(
    vm_store: &mut VmRunnerStore,
    register_host_modules: HostModuleRegistrar,
) -> Result<(), VmExecutionError> {
    let vm_context = vm_store.data().vm_context.clone();
    let async_ops = vm_store.data().async_ops.clone();
    register_host_modules(vm_store.vm_mut(), vm_context, async_ops)
        .map_err(VmExecutionError::HostRegistration)
}

fn acquire_vm_runner_store(
    program: &LoadedProgram,
    vm_context: SharedProxyVmContext,
    register_host_modules: HostModuleRegistrar,
    prefer_aot: bool,
    jit_enabled: bool,
    drop_contract_events_enabled: bool,
    pooling_enabled: bool,
) -> Result<AcquiredVmRunnerStore, VmExecutionError> {
    let async_ops = new_shared_vm_async_ops();
    let pool_key = pooling_enabled.then(|| VmPoolKey::new(prefer_aot, jit_enabled));
    if let Some(pool_key) = pool_key
        && let Some(mut vm) = program.vm_pool.take(pool_key)
    {
        vm.set_drop_contract_events_enabled(drop_contract_events_enabled);
        vm.set_async_bridge(Box::new(VmAsyncOpBridge::new(async_ops.clone())));
        return Ok(AcquiredVmRunnerStore {
            vm_store: Store::new(vm, VmRunnerStoreData::new(vm_context, async_ops)),
            pool_key: Some(pool_key),
        });
    }

    let mut vm_store = new_vm_runner_store(
        program,
        vm_context,
        async_ops,
        prefer_aot,
        jit_enabled,
        drop_contract_events_enabled,
    );
    register_host_modules_from_store(&mut vm_store, register_host_modules)?;
    Ok(AcquiredVmRunnerStore { vm_store, pool_key })
}

fn recycle_vm_runner_store(program: &LoadedProgram, pool_key: VmPoolKey, vm_store: VmRunnerStore) {
    let mut vm = vm_store.into_vm();
    vm.reset_for_reuse();
    vm.clear_async_bridge();
    vm.clear_runtime_print_sink();
    program.vm_pool.put(pool_key, vm);
}

fn configure_vm_interrupt(
    vm_store: &mut VmRunnerStore,
    vm_execution: VmExecutionConfig,
) -> Result<ActiveVmInterrupt, VmExecutionError> {
    match vm_execution.interrupt {
        VmInterruptConfig::None => {
            vm_store.clear_fuel();
            vm_store.clear_epoch_deadline();
            Ok(ActiveVmInterrupt::None)
        }
        VmInterruptConfig::Fuel {
            fuel_per_yield,
            check_interval,
        } => {
            vm_store
                .set_fuel_check_interval(check_interval)
                .map_err(VmExecutionError::Vm)?;
            vm_store.set_fuel(fuel_per_yield);
            Ok(ActiveVmInterrupt::Fuel { fuel_per_yield })
        }
        VmInterruptConfig::Epoch {
            ticks_per_slice,
            check_interval,
        } => {
            vm_store
                .set_epoch_check_interval(check_interval)
                .map_err(VmExecutionError::Vm)?;
            let driver = EpochInterruptionDriver::new(vm_store.vm().epoch_handle());
            Ok(ActiveVmInterrupt::Epoch {
                ticks_per_slice,
                driver,
            })
        }
    }
}

fn arm_epoch_interrupt_if_enabled(
    vm_store: &mut VmRunnerStore,
    interrupt: &ActiveVmInterrupt,
    debugger_attached: bool,
) -> Result<(), VmExecutionError> {
    let ActiveVmInterrupt::Epoch {
        ticks_per_slice,
        driver: _driver,
    } = interrupt
    else {
        return Ok(());
    };
    if debugger_attached {
        return Ok(());
    }
    vm_store
        .set_epoch_deadline(*ticks_per_slice)
        .map_err(VmExecutionError::Vm)?;
    Ok(())
}

fn handle_interrupt_yield(
    vm_store: &mut VmRunnerStore,
    interrupt: &ActiveVmInterrupt,
) -> Result<bool, VmExecutionError> {
    match (interrupt, vm_store.vm().last_yield_reason()) {
        (ActiveVmInterrupt::Fuel { fuel_per_yield }, Some(VmYieldReason::Fuel))
            if vm_store.get_fuel() == Some(0) =>
        {
            vm_store
                .recharge(*fuel_per_yield)
                .map_err(VmExecutionError::Vm)?;
            Ok(true)
        }
        (ActiveVmInterrupt::Epoch { .. }, Some(VmYieldReason::Epoch))
        | (ActiveVmInterrupt::None, _)
        | (ActiveVmInterrupt::Fuel { .. }, _)
        | (ActiveVmInterrupt::Epoch { .. }, _) => Ok(false),
    }
}

struct VmExecutionProfile {
    vm_store: VmRunnerStore,
    queue_wait_us: u64,
    blocking_run_us: u64,
    waiting_host_us: u64,
    waiting_host_count: u32,
    vm_yield_count: u32,
    vm_fuel_recharge_count: u32,
}

impl VmExecutionProfile {
    fn default_with_store(vm_store: VmRunnerStore) -> Self {
        Self {
            vm_store,
            queue_wait_us: 0,
            blocking_run_us: 0,
            waiting_host_us: 0,
            waiting_host_count: 0,
            vm_yield_count: 0,
            vm_fuel_recharge_count: 0,
        }
    }
}

fn maybe_log_tail_profile(request_id: Option<&str>, profile: &VmExecutionProfile) {
    if !tail_profile_enabled() {
        return;
    }
    let Some(request_id) = request_id else {
        return;
    };
    let total_us = profile
        .queue_wait_us
        .saturating_add(profile.blocking_run_us);
    let threshold_us = tail_profile_threshold_us();
    if total_us < threshold_us {
        return;
    }

    warn!(
        "{} vm tail profile request_id={} total_us={} queue_wait_us={} blocking_run_us={} waiting_host_us={} waiting_host_count={} vm_yield_count={} vm_fuel_recharge_count={}",
        category_program(),
        request_id,
        total_us,
        profile.queue_wait_us,
        profile.blocking_run_us,
        profile.waiting_host_us,
        profile.waiting_host_count,
        profile.vm_yield_count,
        profile.vm_fuel_recharge_count
    );
}

fn tail_profile_request_id(vm_store: &VmRunnerStore) -> Option<String> {
    if !tail_profile_enabled() {
        return None;
    }
    Some(
        vm_store
            .data()
            .vm_context
            .with_request_head(|request_head| request_head.request_id().to_string()),
    )
}

fn tail_profile_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("PD_EDGE_PROFILE_VM_TAIL")
            .map(|value| {
                let normalized = value.trim().to_ascii_lowercase();
                matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
            })
            .unwrap_or(false)
    })
}

fn tail_profile_threshold_us() -> u64 {
    static THRESHOLD_US: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *THRESHOLD_US.get_or_init(|| {
        std::env::var("PD_EDGE_PROFILE_VM_TAIL_THRESHOLD_US")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(5_000)
    })
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use axum::http::HeaderMap;
    use tokio::time::sleep;
    use vm::compile_source;

    use super::*;
    use crate::{
        ProxyVmContext, abi_impl::RateLimiterStore, debug_session::new_debug_session_store,
    };

    fn no_host_modules(
        _vm: &mut Vm,
        _context: crate::SharedProxyVmContext,
        _async_ops: crate::SharedVmAsyncOps,
    ) -> Result<(), vm::VmError> {
        Ok(())
    }

    #[tokio::test]
    async fn threading_epoch_interrupt_yields_and_completes() {
        let compiled = compile_source(
            r#"
                let mut total = 0;
                for (let mut i = 0; i < 200000; i = i + 1) {
                    total = total + i;
                }
                total;
            "#,
        )
        .expect("program should compile");
        let program = Arc::new(compiled.program.with_local_count(compiled.locals));
        let loaded_program = LoadedProgram {
            program,
            no_interrupt_aot_bundle: None,
            vm_pool: Arc::new(LoadedProgramVmPool::new()),
        };
        let context = Arc::new(ProxyVmContext::from_request_headers(
            HeaderMap::new(),
            Arc::new(RateLimiterStore::new()),
        ));
        let async_ops = new_shared_vm_async_ops();
        let store = new_vm_runner_store(&loaded_program, context, async_ops, false, true, false);
        let debug = VmDebugInvocation {
            attach_debugger: false,
            force_threading: false,
        };
        let debug_session = new_debug_session_store();
        let execution = VmExecutionConfig {
            interrupt: VmInterruptConfig::Epoch {
                ticks_per_slice: 1,
                check_interval: 1,
            },
            execution_mode: VmExecutionMode::Threading,
            jit_enabled: true,
            drop_contract_events_enabled: false,
        };

        let result = tokio::task::spawn_blocking(move || {
            run_vm_threading(store, debug_session, debug, execution)
        })
        .await
        .expect("threading task should complete");

        let profile = match result {
            Ok(profile) => profile,
            Err(VmExecutionError::Vm(vm::VmError::JitNative(message)))
                if message.contains("native JIT backend is disabled") =>
            {
                return;
            }
            Err(err) => panic!("threading execution should succeed: {err:?}"),
        };

        assert!(
            profile.vm_yield_count > 0,
            "epoch scheduling should yield at least once"
        );
    }

    #[tokio::test]
    async fn epoch_interrupt_driver_advances_epoch_by_wall_clock() {
        tokio::time::timeout(Duration::from_secs(1), async {
            let epoch_handle = EpochHandle::default();
            let _driver = EpochInterruptionDriver::new(epoch_handle.clone());

            loop {
                if epoch_handle.current() > 0 {
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("epoch ticker should advance without explicit wake arming");
    }

    #[tokio::test]
    async fn vm_pool_reuses_registered_vm() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static REGISTRATIONS: AtomicUsize = AtomicUsize::new(0);

        fn counting_host_modules(
            _vm: &mut Vm,
            _context: crate::SharedProxyVmContext,
            _async_ops: crate::SharedVmAsyncOps,
        ) -> Result<(), vm::VmError> {
            REGISTRATIONS.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        let program = Arc::new(vm::Program::new(vec![], vec![vm::OpCode::Ret as u8]));
        let loaded_program = LoadedProgram {
            program,
            no_interrupt_aot_bundle: None,
            vm_pool: Arc::new(LoadedProgramVmPool::new()),
        };
        let context = Arc::new(ProxyVmContext::from_request_headers(
            HeaderMap::new(),
            Arc::new(RateLimiterStore::new()),
        ));

        REGISTRATIONS.store(0, Ordering::Relaxed);

        let first = acquire_vm_runner_store(
            &loaded_program,
            context.clone(),
            counting_host_modules,
            false,
            true,
            false,
            true,
        )
        .expect("first vm checkout should succeed");
        assert_eq!(REGISTRATIONS.load(Ordering::Relaxed), 1);
        recycle_vm_runner_store(
            &loaded_program,
            first.pool_key.expect("pooling should be enabled"),
            first.vm_store,
        );

        let second = acquire_vm_runner_store(
            &loaded_program,
            context,
            counting_host_modules,
            false,
            true,
            false,
            true,
        )
        .expect("second vm checkout should reuse pooled vm");
        assert_eq!(
            REGISTRATIONS.load(Ordering::Relaxed),
            1,
            "reused VM should not re-register host modules"
        );
        recycle_vm_runner_store(
            &loaded_program,
            second.pool_key.expect("pooling should be enabled"),
            second.vm_store,
        );
    }
}
