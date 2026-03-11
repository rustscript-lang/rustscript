use vm::{Vm, VmError};

use super::{SharedProxyVmContext, SharedVmAsyncOps, registry};

mod request;
mod response;
mod upstream;

pub(super) fn register_http_host_module(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) -> Result<(), VmError> {
    registry::register_host_scope(vm, &context, &async_ops, registry::EdgeHostScope::Http);
    Ok(())
}

pub(super) fn register_http_extensions(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    registry::register_host_scope(
        vm,
        &context,
        &async_ops,
        registry::EdgeHostScope::HttpExtension,
    );
}
