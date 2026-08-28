use pd_host_function::pd_host_function;

use super::AnyValue;
use crate::vm::{CallOutcome, Vm, VmResult};

/// Places one bounded event item on the active invocation stream and yields
/// control to the invocation poller. `stream::emit` still evaluates to `()`
/// inside RSS.
#[pd_host_function(name = "stream::emit")]
fn stream_emit_impl(vm: &mut Vm, value: AnyValue) -> VmResult<CallOutcome> {
    vm.emit_stream_item(value)
}
