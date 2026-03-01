use std::time::Duration;

use vm::{CallOutcome, HostFunction, Value, Vm, VmError};

use super::{
    SharedProxyVmContext, SharedVmAsyncOps, bind_async_host, expect_arg_count, expect_int,
    expect_string, schedule_future_call,
};

pub(super) fn register_runtime_host_module(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) -> Result<(), VmError> {
    bind_async_host(
        vm,
        &async_ops,
        "runtime::sleep",
        Box::new(RuntimeSleepFunction::new(async_ops.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "rate_limit::allow",
        Box::new(RuntimeRateLimitAllowFunction::new(context)),
    );
    Ok(())
}

struct RuntimeSleepFunction {
    async_ops: SharedVmAsyncOps,
}

impl RuntimeSleepFunction {
    fn new(async_ops: SharedVmAsyncOps) -> Self {
        Self { async_ops }
    }
}

impl HostFunction for RuntimeSleepFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let millis = expect_int(args, 0)?;
        if millis < 0 {
            return Err(VmError::HostError(format!(
                "runtime::sleep expects non-negative milliseconds, got {millis}",
            )));
        }
        let duration = Duration::from_millis(millis as u64);
        schedule_future_call(vm, &self.async_ops, async move {
            tokio::time::sleep(duration).await;
            Ok(vec![Value::Bool(true)])
        })
    }
}

struct RuntimeRateLimitAllowFunction {
    context: SharedProxyVmContext,
}

impl RuntimeRateLimitAllowFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for RuntimeRateLimitAllowFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 3)?;
        let key = expect_string(args, 0)?;
        let limit = expect_int(args, 1)?;
        let window_seconds = expect_int(args, 2)?;
        if limit <= 0 || window_seconds <= 0 {
            return Ok(CallOutcome::Return(vec![Value::Bool(false)]));
        }

        let rate_limiter = {
            let context = self.context.lock().expect("vm context lock poisoned");
            context.rate_limiter.clone()
        };
        let allowed = rate_limiter
            .lock()
            .expect("rate limiter lock poisoned")
            .allow(&key, limit as u64, window_seconds as u64);
        Ok(CallOutcome::Return(vec![Value::Bool(allowed)]))
    }
}
