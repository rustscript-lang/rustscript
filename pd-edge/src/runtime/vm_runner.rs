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
    let local_count = program.local_count;
    let program = program.program.clone();
    let async_ops = new_shared_vm_async_ops();

    if debug.attach_debugger {
        let async_ops_for_debug = async_ops.clone();
        let vm_context_for_debug = vm_context.clone();
        let program_for_debug = program.clone();
        let task = tokio::task::spawn_blocking(move || {
            let mut vm = Vm::with_locals_shared(program_for_debug, local_count);
            vm.set_async_bridge(Box::new(VmAsyncOpBridge::new(async_ops_for_debug.clone())));
            register_host_modules(&mut vm, vm_context_for_debug.clone(), async_ops_for_debug)
                .map_err(VmExecutionError::HostRegistration)?;

            loop {
                let status = run_vm_with_optional_debugger(
                    &debug_session,
                    &debug.request_headers,
                    &debug.request_path,
                    &debug.request_id,
                    &mut vm,
                )
                .map_err(VmExecutionError::Vm)?;

                match status {
                    VmStatus::Halted => break,
                    VmStatus::Yielded => continue,
                    VmStatus::Waiting(_op_id) => tokio::runtime::Handle::current()
                        .block_on(vm.await_waiting_host_op())
                        .map_err(VmExecutionError::Vm)?,
                }
            }

            Ok(snapshot_execution_outcome(&vm_context_for_debug))
        });

        return task.await.map_err(|err| {
            VmExecutionError::Vm(vm::VmError::HostError(format!(
                "vm blocking execution task failed: {err}"
            )))
        })?;
    }

    let mut vm = Vm::with_locals_shared(program, local_count);
    vm.set_async_bridge(Box::new(VmAsyncOpBridge::new(async_ops.clone())));
    register_host_modules(&mut vm, vm_context.clone(), async_ops)
        .map_err(VmExecutionError::HostRegistration)?;

    loop {
        let status = run_vm_with_optional_debugger(
            &debug_session,
            &debug.request_headers,
            &debug.request_path,
            &debug.request_id,
            &mut vm,
        )
        .map_err(VmExecutionError::Vm)?;
        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                tokio::task::yield_now().await;
            }
            VmStatus::Waiting(_op_id) => {
                vm.await_waiting_host_op()
                    .await
                    .map_err(VmExecutionError::Vm)?;
            }
        }
    }

    Ok(snapshot_execution_outcome(&vm_context))
}
