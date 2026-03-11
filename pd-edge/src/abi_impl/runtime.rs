use std::time::Duration;

use edge_abi::symbols::{rate_limit as edge_rate_limit, runtime as edge_runtime};
use pd_edge_host_function::pd_edge_host_function;
use vm::{CallOutcome, Value, Vm, VmError};

use super::{SharedProxyVmContext, SharedVmAsyncOps, current_vm_context};

pub(super) fn register_runtime_host_module(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) -> Result<(), VmError> {
    super::registry::register_host_scope(
        vm,
        &context,
        &async_ops,
        super::registry::EdgeHostScope::Runtime,
    );
    Ok(())
}

#[pd_edge_host_function(name = edge_runtime::SLEEP.name, scope = runtime)]
async fn runtime_sleep(_vm: &mut Vm, millis: i64) -> Result<CallOutcome, VmError> {
    if millis < 0 {
        return Err(VmError::HostError(format!(
            "runtime::sleep expects non-negative milliseconds, got {millis}",
        )));
    }

    let duration = Duration::from_millis(millis as u64);
    tokio::time::sleep(duration).await;
    Ok(CallOutcome::Return(vec![Value::Bool(true)]))
}

#[pd_edge_host_function(name = edge_rate_limit::ALLOW.name, scope = runtime)]
async fn rate_limit_allow(
    _vm: &mut Vm,
    key: String,
    limit: i64,
    window_seconds: i64,
) -> Result<CallOutcome, VmError> {
    if limit <= 0 || window_seconds <= 0 {
        return Ok(CallOutcome::Return(vec![Value::Bool(false)]));
    }

    let context = current_vm_context()?;
    let rate_limiter = {
        let context = context.lock().expect("vm context lock poisoned");
        context.rate_limiter.clone()
    };
    let allowed = rate_limiter
        .lock()
        .expect("rate limiter lock poisoned")
        .allow(&key, limit as u64, window_seconds as u64);
    Ok(CallOutcome::Return(vec![Value::Bool(allowed)]))
}
