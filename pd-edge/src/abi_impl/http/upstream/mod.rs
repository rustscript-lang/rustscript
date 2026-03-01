use vm::Vm;

use super::super::super::{SharedProxyVmContext, SharedVmAsyncOps};

mod request;
mod response;

pub(super) fn register_7_to_11(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    request::register_7_to_11(vm, context, async_ops);
}

pub(super) fn register_17(vm: &mut Vm, context: SharedProxyVmContext, async_ops: SharedVmAsyncOps) {
    request::register_17(vm, context, async_ops);
}

pub(super) fn register_25_to_30(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    request::register_25_to_30(vm, context, async_ops);
}

pub(super) fn register_40_to_43(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    response::register_40_to_43(vm, context, async_ops);
}
