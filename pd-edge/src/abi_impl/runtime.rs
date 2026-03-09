use edge_abi::symbols::{rate_limit, runtime};
use std::time::Duration;

use vm::{CallOutcome, Value, Vm, VmError};

use super::{
    SharedProxyVmContext, SharedVmAsyncOps, bind_async_host_handler, expect_arg_count, expect_int,
    expect_string, schedule_future_call,
};

pub(super) fn register_runtime_host_module(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) -> Result<(), VmError> {
    bind_runtime_sleep(vm, &async_ops);
    bind_rate_limit_allow(vm, &async_ops, context);
    Ok(())
}

fn bind_runtime_sleep(vm: &mut Vm, async_ops: &SharedVmAsyncOps) {
    let async_ops_for_bind = async_ops.clone();
    let async_ops_for_call = async_ops_for_bind.clone();
    bind_async_host_handler(
        vm,
        &async_ops_for_bind,
        runtime::SLEEP.name,
        move |vm, args| {
            expect_arg_count(args, 1)?;
            let millis = expect_int(args, 0)?;
            if millis < 0 {
                return Err(VmError::HostError(format!(
                    "runtime::sleep expects non-negative milliseconds, got {millis}",
                )));
            }
            let duration = Duration::from_millis(millis as u64);
            schedule_future_call(vm, &async_ops_for_call, async move {
                tokio::time::sleep(duration).await;
                Ok(vec![Value::Bool(true)])
            })
        },
    );
}

fn bind_rate_limit_allow(vm: &mut Vm, async_ops: &SharedVmAsyncOps, context: SharedProxyVmContext) {
    bind_async_host_handler(vm, async_ops, rate_limit::ALLOW.name, move |_vm, args| {
        expect_arg_count(args, 3)?;
        let key = expect_string(args, 0)?;
        let limit = expect_int(args, 1)?;
        let window_seconds = expect_int(args, 2)?;
        if limit <= 0 || window_seconds <= 0 {
            return Ok(CallOutcome::Return(vec![Value::Bool(false)]));
        }

        let rate_limiter = {
            let context = context.lock().expect("vm context lock poisoned");
            context.rate_limiter.clone()
        };
        let allowed = rate_limiter
            .lock()
            .expect("rate limiter lock poisoned")
            .allow(&key, limit as u64, window_seconds as u64);
        Ok(CallOutcome::Return(vec![Value::Bool(allowed)]))
    });
}
