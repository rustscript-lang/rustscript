use vm::Vm;

use super::super::super::{SharedProxyVmContext, SharedVmAsyncOps};

mod request;
mod response;

pub(super) fn register(vm: &mut Vm, context: SharedProxyVmContext, async_ops: SharedVmAsyncOps) {
    request::register(vm, context.clone(), async_ops.clone());
    response::register(vm, context, async_ops);
}
