use vm::{Vm, VmError};

use super::{SharedProxyVmContext, SharedVmAsyncOps};

mod rate_limit;
mod request;
mod response;
mod upstream;

pub(super) fn register_http_host_module(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) -> Result<(), VmError> {
    request::register_0_to_6(vm, context.clone(), async_ops.clone());
    upstream::register_7_to_11(vm, context.clone(), async_ops.clone());
    request::register_12(vm, context.clone(), async_ops.clone());
    response::register_13_to_16(vm, context.clone(), async_ops.clone());
    upstream::register_17(vm, context.clone(), async_ops.clone());
    rate_limit::register_18(vm, context.clone(), async_ops.clone());
    request::register_19_to_24(vm, context.clone(), async_ops.clone());
    upstream::register_25_to_30(vm, context.clone(), async_ops.clone());
    request::register_31_to_32(vm, context.clone(), async_ops.clone());
    response::register_33_to_39(vm, context.clone(), async_ops.clone());
    upstream::register_40_to_43(vm, context, async_ops);
    Ok(())
}

pub(super) fn register_http_extensions(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    request::register_streaming_extensions(vm, context, async_ops);
}
