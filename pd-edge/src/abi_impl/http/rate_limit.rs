use vm::{CallOutcome, HostFunction, Value, Vm, VmError};

use super::super::{
    SharedProxyVmContext, SharedVmAsyncOps, bind_async_host, expect_arg_count, expect_int,
    expect_string,
};

pub(super) fn register_18(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    bind_async_host(
        vm,
        &async_ops,
        "http::rate_limit::allow",
        Box::new(RateLimitAllowFunction::new(context)),
    );
}

struct RateLimitAllowFunction {
    context: SharedProxyVmContext,
}

impl RateLimitAllowFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for RateLimitAllowFunction {
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
