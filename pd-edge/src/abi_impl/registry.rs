use vm::Vm;

use super::{SharedProxyVmContext, SharedVmAsyncOps};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EdgeHostScope {
    Runtime,
    Http,
    HttpExtension,
    Io,
}

pub(crate) struct EdgeHostRegistration {
    pub scope: EdgeHostScope,
    pub register: fn(&mut Vm, &SharedProxyVmContext, &SharedVmAsyncOps),
}

#[::linkme::distributed_slice]
pub(crate) static PD_EDGE_HOST_FUNCTIONS: [EdgeHostRegistration];

pub(crate) fn register_host_scope(
    vm: &mut Vm,
    context: &SharedProxyVmContext,
    async_ops: &SharedVmAsyncOps,
    scope: EdgeHostScope,
) {
    for registration in PD_EDGE_HOST_FUNCTIONS {
        if registration.scope == scope {
            (registration.register)(vm, context, async_ops);
        }
    }
}
