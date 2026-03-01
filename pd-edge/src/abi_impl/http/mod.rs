use vm::{Vm, VmError};

use super::{SharedProxyVmContext, SharedVmAsyncOps};

mod request;
mod response;
mod upstream;

pub(super) fn register_http_host_module(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) -> Result<(), VmError> {
    request::register(vm, context.clone(), async_ops.clone());
    response::register(vm, context.clone(), async_ops.clone());
    upstream::register(vm, context, async_ops);
    Ok(())
}

pub(super) fn register_http_extensions(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    request::register_streaming_extensions(vm, context, async_ops);
}
