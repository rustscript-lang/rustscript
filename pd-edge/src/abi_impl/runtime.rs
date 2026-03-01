use std::time::Duration;

use vm::{CallOutcome, HostFunction, Value, Vm, VmError};

use super::{
    SharedVmAsyncOps, bind_async_host, expect_arg_count, expect_int, schedule_future_call,
};

pub(super) fn register_runtime_host_module(
    vm: &mut Vm,
    async_ops: SharedVmAsyncOps,
) -> Result<(), VmError> {
    bind_async_host(
        vm,
        &async_ops,
        "runtime::sleep",
        Box::new(RuntimeSleepFunction::new(async_ops.clone())),
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
