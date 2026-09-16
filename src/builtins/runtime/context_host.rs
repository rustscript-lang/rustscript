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

// ---- standard host module ownership ---------------------------------------

/// The standard `context` host module: one descriptor owner per host
/// function.
pub(super) fn context_host_module() -> super::host_modules::StandardHostModule {
    const OWNED: &[fn() -> crate::host_extension::HostFunctionDescriptor] =
        &[stream_emit_descriptor];
    super::host_modules::descriptor_only_module("context", OWNED)
}
