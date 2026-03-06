use axum::http::HeaderMap;
use tracing::warn;
use vm::{Vm, VmStatus};

use super::{LoadedProgram, VmExecutionConfig};
use crate::{
    abi_impl::{
        SharedProxyVmContext, SharedVmAsyncOps, VmAsyncOpBridge, VmExecutionOutcome,
        new_shared_vm_async_ops, snapshot_execution_outcome,
    },
    debug_session::{SharedDebugSession, run_vm_with_optional_debugger},
    logging::category_program,
};

pub type HostModuleRegistrar =
    fn(&mut Vm, SharedProxyVmContext, SharedVmAsyncOps) -> Result<(), vm::VmError>;

#[derive(Debug)]
pub enum VmExecutionError {
    HostRegistration(vm::VmError),
    Vm(vm::VmError),
}

#[derive(Clone, Debug)]
pub struct VmDebugInvocation {
    pub attach_debugger: bool,
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
) -> Result<VmExecutionOutcome, VmExecutionError> {
    let program = program.program.clone();
    let async_ops = new_shared_vm_async_ops();
    let started = std::time::Instant::now();
    let request_id = debug.request_id.clone();
    let (outcome, mut profile) = run_vm_async(
        program,
        vm_context,
        debug_session,
        debug,
        async_ops,
        register_host_modules,
        vm_execution,
    )
    .await?;

    profile.queue_wait_us = 0;
    profile.blocking_run_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    maybe_log_tail_profile(&request_id, &profile);
    Ok(outcome)
}

async fn run_vm_async(
    program: std::sync::Arc<vm::Program>,
    vm_context: SharedProxyVmContext,
    debug_session: SharedDebugSession,
    debug: VmDebugInvocation,
    async_ops: SharedVmAsyncOps,
    register_host_modules: HostModuleRegistrar,
    vm_execution: VmExecutionConfig,
) -> Result<(VmExecutionOutcome, VmExecutionProfile), VmExecutionError> {
    let mut vm = Vm::new_shared(program);
    vm.set_async_bridge(Box::new(VmAsyncOpBridge::new(async_ops.clone())));
    if let Some(fuel) = vm_execution.fuel_per_yield {
        vm.set_fuel_check_interval(vm_execution.fuel_check_interval)
            .map_err(VmExecutionError::Vm)?;
        vm.set_fuel(fuel);
    }
    register_host_modules(&mut vm, vm_context.clone(), async_ops)
        .map_err(VmExecutionError::HostRegistration)?;
    let mut profile = VmExecutionProfile::default();

    loop {
        let status = if debug.attach_debugger {
            run_vm_with_optional_debugger(
                &debug_session,
                &debug.request_headers,
                &debug.request_path,
                &debug.request_id,
                &mut vm,
            )
        } else {
            vm.run()
        }
        .map_err(VmExecutionError::Vm)?;
        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                profile.vm_yield_count = profile.vm_yield_count.saturating_add(1);
                if let Some(fuel) = vm_execution.fuel_per_yield
                    && vm.get_fuel() == Some(0)
                {
                    vm.recharge_fuel(fuel).map_err(VmExecutionError::Vm)?;
                    profile.vm_fuel_recharge_count =
                        profile.vm_fuel_recharge_count.saturating_add(1);
                }
                tokio::task::yield_now().await;
            }
            VmStatus::Waiting(_op_id) => {
                let waiting_started = std::time::Instant::now();
                vm.await_waiting_host_op()
                    .await
                    .map_err(VmExecutionError::Vm)?;
                let wait_us =
                    u64::try_from(waiting_started.elapsed().as_micros()).unwrap_or(u64::MAX);
                profile.waiting_host_us = profile.waiting_host_us.saturating_add(wait_us);
                profile.waiting_host_count = profile.waiting_host_count.saturating_add(1);
            }
        }
    }

    Ok((snapshot_execution_outcome(&vm_context), profile))
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
