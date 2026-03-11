use vm::Vm;

use super::{SharedProxyVmContext, SharedVmAsyncOps, registry};

mod request;
mod response;
mod upstream;

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
