use std::time::Duration;

use edge_abi::symbols::{rate_limit as edge_rate_limit, runtime as edge_runtime};
use pd_edge_host_function::pd_edge_host_function;
use vm::{CallOutcome, Value, Vm, VmError};

use super::SharedProxyVmContext;

/// Suspends execution for the requested number of milliseconds.
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

/// Halts the current VM invocation immediately.
#[pd_edge_host_function(name = edge_runtime::EXIT.name, scope = runtime)]
fn runtime_exit(_vm: &mut Vm) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Halt)
}

/// Checks whether a rate-limit bucket allows the current operation.
#[pd_edge_host_function(name = edge_rate_limit::ALLOW.name, scope = runtime)]
async fn rate_limit_allow(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    key: String,
    limit: i64,
    window_seconds: i64,
) -> Result<CallOutcome, VmError> {
    if limit <= 0 || window_seconds <= 0 {
        return Ok(CallOutcome::Return(vec![Value::Bool(false)]));
    }

    let rate_limiter = context.services().rate_limiter();
    let allowed = rate_limiter.allow(&key, limit as u64, window_seconds as u64);
    Ok(CallOutcome::Return(vec![Value::Bool(allowed)]))
}
