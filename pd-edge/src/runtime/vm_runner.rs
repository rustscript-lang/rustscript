use axum::http::HeaderMap;
use vm::{Vm, VmStatus};

use super::LoadedProgram;
use crate::{
    abi_impl::{
        SharedProxyVmContext, SharedVmAsyncOps, VmAsyncOpBridge, VmExecutionOutcome,
        new_shared_vm_async_ops, snapshot_execution_outcome,
    },
    debug_session::{SharedDebugSession, run_vm_with_optional_debugger},
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
) -> Result<VmExecutionOutcome, VmExecutionError> {
    let program = program.program.clone();
    let async_ops = new_shared_vm_async_ops();

    let task = tokio::task::spawn_blocking(move || {
        run_vm_blocking(
            program,
            vm_context,
            debug_session,
            debug,
            async_ops,
            register_host_modules,
        )
    });

    task.await.map_err(|err| {
        VmExecutionError::Vm(vm::VmError::HostError(format!(
            "vm blocking execution task failed: {err}"
        )))
    })?
}

fn run_vm_blocking(
    program: std::sync::Arc<vm::Program>,
    vm_context: SharedProxyVmContext,
    debug_session: SharedDebugSession,
    debug: VmDebugInvocation,
    async_ops: SharedVmAsyncOps,
    register_host_modules: HostModuleRegistrar,
) -> Result<VmExecutionOutcome, VmExecutionError> {
    let mut vm = Vm::new_shared(program);
    vm.set_async_bridge(Box::new(VmAsyncOpBridge::new(async_ops.clone())));
    register_host_modules(&mut vm, vm_context.clone(), async_ops)
        .map_err(VmExecutionError::HostRegistration)?;

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
                std::thread::yield_now();
            }
            VmStatus::Waiting(_op_id) => tokio::runtime::Handle::current()
                .block_on(vm.await_waiting_host_op())
                .map_err(VmExecutionError::Vm)?,
        }
    }

    Ok(snapshot_execution_outcome(&vm_context))
}
