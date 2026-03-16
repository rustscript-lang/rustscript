use std::{future::Future, pin::Pin, time::Duration};

use axum::http::HeaderMap;
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
type VmExecutionModeFuture =
    Pin<Box<dyn Future<Output = Result<(), VmExecutionError>> + Send + 'static>>;

trait VmModeRunner {
    fn execute(
        vm_store: VmRunnerStore,
        debug_session: SharedDebugSession,
        debug: VmDebugInvocation,
        register_host_modules: HostModuleRegistrar,
        vm_execution: VmExecutionConfig,
    ) -> VmExecutionModeFuture;
}

struct AsyncModeRunner;
struct ThreadingModeRunner;

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

#[derive(Clone, Debug)]
pub struct VmDebugInvocation {
    pub attach_debugger: bool,
    pub force_threading: bool,
    pub request_headers: HeaderMap,
    pub request_path: String,
    pub request_id: String,
}

pub async fn execute_vm_with_context(
    program: &LoadedProgram,
    vm_context: SharedProxyVmContext,
    debug_session: SharedDebugSession,
    debug: VmDebugInvocation,
    register_host_modules: HostModuleRegistrar,
    vm_execution: VmExecutionConfig,
) -> Result<(), VmExecutionError> {
    let async_ops = new_shared_vm_async_ops();
    let vm_store = new_vm_runner_store(program.program.clone(), vm_context, async_ops);
    let execution_mode = if debug.force_threading {
        VmExecutionMode::Threading
    } else {
        vm_execution.execution_mode
    };

    match execution_mode {
        VmExecutionMode::Async => {
            AsyncModeRunner::execute(
                vm_store,
                debug_session,
                debug,
                register_host_modules,
                vm_execution,
            )
            .await
        }
        VmExecutionMode::Threading => {
            ThreadingModeRunner::execute(
                vm_store,
                debug_session,
                debug,
                register_host_modules,
                vm_execution,
            )
            .await
        }
    }
}

impl VmModeRunner for AsyncModeRunner {
    fn execute(
        vm_store: VmRunnerStore,
        debug_session: SharedDebugSession,
        debug: VmDebugInvocation,
        register_host_modules: HostModuleRegistrar,
        vm_execution: VmExecutionConfig,
    ) -> VmExecutionModeFuture {
        Box::pin(execute_vm_with_async_mode(
            vm_store,
            debug_session,
            debug,
            register_host_modules,
            vm_execution,
        ))
    }
}

impl VmModeRunner for ThreadingModeRunner {
    fn execute(
        vm_store: VmRunnerStore,
        debug_session: SharedDebugSession,
        debug: VmDebugInvocation,
        register_host_modules: HostModuleRegistrar,
        vm_execution: VmExecutionConfig,
    ) -> VmExecutionModeFuture {
        Box::pin(execute_vm_with_threading_mode(
            vm_store,
            debug_session,
            debug,
            register_host_modules,
            vm_execution,
        ))
    }
}

async fn execute_vm_with_async_mode(
    vm_store: VmRunnerStore,
    debug_session: SharedDebugSession,
    debug: VmDebugInvocation,
    register_host_modules: HostModuleRegistrar,
    vm_execution: VmExecutionConfig,
) -> Result<(), VmExecutionError> {
    let started = std::time::Instant::now();
    let request_id = debug.request_id.clone();
    let mut profile = run_vm_async(
        vm_store,
        debug_session,
        debug,
        register_host_modules,
        vm_execution,
    )
    .await?;

    profile.queue_wait_us = 0;
    profile.blocking_run_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    maybe_log_tail_profile(&request_id, &profile);
    Ok(())
}

async fn execute_vm_with_threading_mode(
    vm_store: VmRunnerStore,
    debug_session: SharedDebugSession,
    debug: VmDebugInvocation,
    register_host_modules: HostModuleRegistrar,
    vm_execution: VmExecutionConfig,
) -> Result<(), VmExecutionError> {
    let queued_at = std::time::Instant::now();
    let task = tokio::task::spawn_blocking(move || {
        let queue_wait_us = u64::try_from(queued_at.elapsed().as_micros()).unwrap_or(u64::MAX);
        let threading_started = std::time::Instant::now();
        let request_id = debug.request_id.clone();
        let result = run_vm_threading(
            vm_store,
            debug_session,
            debug,
            register_host_modules,
            vm_execution,
        );

        match result {
            Ok(mut profile) => {
                profile.queue_wait_us = queue_wait_us;
                profile.blocking_run_us =
                    u64::try_from(threading_started.elapsed().as_micros()).unwrap_or(u64::MAX);
                maybe_log_tail_profile(&request_id, &profile);
                Ok(())
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
    register_host_modules: HostModuleRegistrar,
    vm_execution: VmExecutionConfig,
) -> Result<VmExecutionProfile, VmExecutionError> {
    let interrupt = configure_vm_interrupt(&mut vm_store, vm_execution)?;
    register_host_modules_from_store(&mut vm_store, register_host_modules)?;
    let mut profile = VmExecutionProfile::default();

    loop {
        arm_epoch_interrupt_if_enabled(&mut vm_store, &interrupt, debug.attach_debugger)?;
        let status = {
            let _host_context = enter_edge_host_context(
                vm_store.data().vm_context.clone(),
                vm_store.data().async_ops.clone(),
            );
            if debug.attach_debugger {
                run_vm_with_optional_debugger(
                    &debug_session,
                    &debug.request_headers,
                    &debug.request_path,
                    &debug.request_id,
                    vm_store.vm_mut(),
                )
            } else {
                vm_store.run()
            }
        }
        .map_err(VmExecutionError::Vm)?;
        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                profile.vm_yield_count = profile.vm_yield_count.saturating_add(1);
                handle_interrupt_yield(&mut vm_store, &interrupt, &mut profile)?;
                tokio::task::yield_now().await;
            }
            VmStatus::Waiting(_op_id) => {
                let waiting_started = std::time::Instant::now();
                vm_store
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
    register_host_modules: HostModuleRegistrar,
    vm_execution: VmExecutionConfig,
) -> Result<VmExecutionProfile, VmExecutionError> {
    let interrupt = configure_vm_interrupt(&mut vm_store, vm_execution)?;
    register_host_modules_from_store(&mut vm_store, register_host_modules)?;
    let mut profile = VmExecutionProfile::default();

    loop {
        arm_epoch_interrupt_if_enabled(&mut vm_store, &interrupt, debug.attach_debugger)?;
        let status = {
            let _host_context = enter_edge_host_context(
                vm_store.data().vm_context.clone(),
                vm_store.data().async_ops.clone(),
            );
            if debug.attach_debugger {
                run_vm_with_optional_debugger(
                    &debug_session,
                    &debug.request_headers,
                    &debug.request_path,
                    &debug.request_id,
                    vm_store.vm_mut(),
                )
            } else {
                vm_store.run()
            }
        }
        .map_err(VmExecutionError::Vm)?;

        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                profile.vm_yield_count = profile.vm_yield_count.saturating_add(1);
                handle_interrupt_yield(&mut vm_store, &interrupt, &mut profile)?;
                std::thread::yield_now();
            }
            VmStatus::Waiting(_op_id) => {
                let waiting_started = std::time::Instant::now();
                tokio::runtime::Handle::current()
                    .block_on(vm_store.vm_mut().await_waiting_host_op())
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
    program: std::sync::Arc<vm::Program>,
    vm_context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) -> VmRunnerStore {
    let mut vm = Vm::new_shared(program);
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
    profile: &mut VmExecutionProfile,
) -> Result<(), VmExecutionError> {
    match (interrupt, vm_store.vm().last_yield_reason()) {
        (ActiveVmInterrupt::Fuel { fuel_per_yield }, Some(VmYieldReason::Fuel))
            if vm_store.get_fuel() == Some(0) =>
        {
            vm_store
                .recharge(*fuel_per_yield)
                .map_err(VmExecutionError::Vm)?;
            profile.vm_fuel_recharge_count = profile.vm_fuel_recharge_count.saturating_add(1);
        }
        (ActiveVmInterrupt::Epoch { .. }, Some(VmYieldReason::Epoch))
        | (ActiveVmInterrupt::None, _)
        | (ActiveVmInterrupt::Fuel { .. }, _)
        | (ActiveVmInterrupt::Epoch { .. }, _) => {}
    }
    Ok(())
}

#[derive(Debug, Default)]
struct VmExecutionProfile {
    queue_wait_us: u64,
    blocking_run_us: u64,
    waiting_host_us: u64,
    waiting_host_count: u32,
    vm_yield_count: u32,
    vm_fuel_recharge_count: u32,
}

fn maybe_log_tail_profile(request_id: &str, profile: &VmExecutionProfile) {
    if !tail_profile_enabled() {
        return;
    }
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
        let context = Arc::new(ProxyVmContext::from_request_headers(
            HeaderMap::new(),
            Arc::new(RateLimiterStore::new()),
        ));
        let async_ops = new_shared_vm_async_ops();
        let store = new_vm_runner_store(program, context, async_ops);
        let debug = VmDebugInvocation {
            attach_debugger: false,
            force_threading: false,
            request_headers: HeaderMap::new(),
            request_path: "/".to_string(),
            request_id: "epoch-test".to_string(),
        };
        let debug_session = new_debug_session_store();
        let execution = VmExecutionConfig {
            interrupt: VmInterruptConfig::Epoch {
                ticks_per_slice: 1,
                check_interval: 1,
            },
            execution_mode: VmExecutionMode::Threading,
        };

        let profile = tokio::task::spawn_blocking(move || {
            run_vm_threading(store, debug_session, debug, no_host_modules, execution)
        })
        .await
        .expect("threading task should complete")
        .expect("threading execution should succeed");

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
}
