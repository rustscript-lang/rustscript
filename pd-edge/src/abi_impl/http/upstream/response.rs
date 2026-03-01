use axum::http::HeaderName;
use vm::{CallOutcome, HostFunction, Value, Vm, VmError};

use super::super::super::{
    SharedProxyVmContext, SharedVmAsyncOps, bind_async_host, expect_arg_count, expect_string,
    headers_to_value_map,
};

pub(super) fn register_40_to_43(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    bind_async_host(
        vm,
        &async_ops,
        "http::upstream::response::get_status",
        Box::new(GetUpstreamResponseStatusFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::upstream::response::get_header",
        Box::new(GetUpstreamResponseHeaderFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::upstream::response::get_headers",
        Box::new(GetUpstreamResponseHeadersFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::upstream::response::get_body",
        Box::new(GetUpstreamResponseBodyFunction::new(context)),
    );
}

struct GetUpstreamResponseStatusFunction {
    context: SharedProxyVmContext,
}

impl GetUpstreamResponseStatusFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for GetUpstreamResponseStatusFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 0)?;
        let context = self.context.lock().expect("vm context lock poisoned");
        let status = context.upstream_response_status.unwrap_or(0);
        Ok(CallOutcome::Return(vec![Value::Int(status as i64)]))
    }
}

struct GetUpstreamResponseHeaderFunction {
    context: SharedProxyVmContext,
}

impl GetUpstreamResponseHeaderFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for GetUpstreamResponseHeaderFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let name = expect_string(args, 0)?;
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;

        let context = self.context.lock().expect("vm context lock poisoned");
        let value = context
            .upstream_response_headers
            .get(&header_name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        Ok(CallOutcome::Return(vec![Value::String(value.to_string())]))
    }
}

struct GetUpstreamResponseHeadersFunction {
    context: SharedProxyVmContext,
}

impl GetUpstreamResponseHeadersFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for GetUpstreamResponseHeadersFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 0)?;
        let context = self.context.lock().expect("vm context lock poisoned");
        Ok(CallOutcome::Return(vec![headers_to_value_map(
            &context.upstream_response_headers,
        )]))
    }
}

struct GetUpstreamResponseBodyFunction {
    context: SharedProxyVmContext,
}

impl GetUpstreamResponseBodyFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for GetUpstreamResponseBodyFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 0)?;
        let context = self.context.lock().expect("vm context lock poisoned");
        let value = context
            .upstream_response_content
            .clone()
            .unwrap_or_default();
        Ok(CallOutcome::Return(vec![Value::String(value)]))
    }
}
