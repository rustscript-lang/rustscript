use axum::http::HeaderName;
use vm::{CallOutcome, HostFunction, Value, Vm, VmError};

use super::super::{
    SharedProxyVmContext, SharedVmAsyncOps, bind_async_host, expect_arg_count, expect_int,
    expect_string, headers_to_value_map, query_to_value_map, request_path_with_query,
    schedule_ready_call,
};

fn bind_request_host(
    vm: &mut Vm,
    async_ops: &SharedVmAsyncOps,
    symbol: &'static str,
    function: Box<dyn HostFunction>,
) {
    bind_async_host(vm, async_ops, symbol, function);
}

fn bind_request_field(
    vm: &mut Vm,
    async_ops: &SharedVmAsyncOps,
    context: &SharedProxyVmContext,
    symbol: &'static str,
    field: RequestField,
) {
    bind_request_host(
        vm,
        async_ops,
        symbol,
        Box::new(GetRequestFieldFunction::new(context.clone(), field)),
    );
}

pub(super) fn register_0_to_6(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    let field_symbols = [
        ("http::request::get_id", RequestField::Id),
        ("http::request::get_method", RequestField::Method),
        ("http::request::get_path", RequestField::Path),
        ("http::request::get_query", RequestField::Query),
        ("http::request::get_scheme", RequestField::Scheme),
        ("http::request::get_host", RequestField::Host),
    ];
    for (symbol, field) in field_symbols {
        bind_request_field(vm, &async_ops, &context, symbol, field);
    }
    bind_request_host(
        vm,
        &async_ops,
        "http::request::get_header",
        Box::new(GetHeaderFunction::new(context)),
    );
}

pub(super) fn register_12(vm: &mut Vm, context: SharedProxyVmContext, async_ops: SharedVmAsyncOps) {
    bind_request_field(
        vm,
        &async_ops,
        &context,
        "http::request::get_client_ip",
        RequestField::ClientIp,
    );
}

pub(super) fn register_19_to_24(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    bind_request_host(
        vm,
        &async_ops,
        "http::request::get_headers",
        Box::new(GetRequestHeadersFunction::new(context.clone())),
    );
    bind_request_host(
        vm,
        &async_ops,
        "http::request::get_query_arg",
        Box::new(GetRequestQueryArgFunction::new(context.clone())),
    );
    bind_request_host(
        vm,
        &async_ops,
        "http::request::get_query_args",
        Box::new(GetRequestQueryArgsFunction::new(context.clone())),
    );
    bind_request_field(
        vm,
        &async_ops,
        &context,
        "http::request::get_path_with_query",
        RequestField::PathWithQuery,
    );
    bind_request_field(
        vm,
        &async_ops,
        &context,
        "http::request::get_raw_query",
        RequestField::RawQuery,
    );
    bind_request_host(
        vm,
        &async_ops,
        "http::request::get_body",
        Box::new(GetRequestBodyFunction::new(context, async_ops.clone())),
    );
}

pub(super) fn register_31_to_32(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    bind_request_field(
        vm,
        &async_ops,
        &context,
        "http::request::get_http_version",
        RequestField::HttpVersion,
    );
    bind_request_host(
        vm,
        &async_ops,
        "http::request::get_port",
        Box::new(GetRequestPortFunction::new(context)),
    );
}

pub(super) fn register_streaming_extensions(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    bind_request_host(
        vm,
        &async_ops,
        "http::request::body::next_chunk",
        Box::new(GetRequestBodyChunkFunction::new(
            context.clone(),
            async_ops.clone(),
        )),
    );
    bind_request_host(
        vm,
        &async_ops,
        "http::request::body::eof",
        Box::new(GetRequestBodyEofFunction::new(context)),
    );
}

#[derive(Clone, Copy)]
enum RequestField {
    Id,
    Method,
    Path,
    Query,
    RawQuery,
    PathWithQuery,
    HttpVersion,
    Scheme,
    Host,
    ClientIp,
}

struct GetRequestFieldFunction {
    context: SharedProxyVmContext,
    field: RequestField,
}

impl GetRequestFieldFunction {
    fn new(context: SharedProxyVmContext, field: RequestField) -> Self {
        Self { context, field }
    }
}

impl HostFunction for GetRequestFieldFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 0)?;
        let context = self.context.lock().expect("vm context lock poisoned");
        let value = match self.field {
            RequestField::Id => context.inbound_request_id.clone(),
            RequestField::Method => context.inbound_request_method.as_str().to_string(),
            RequestField::Path => context.inbound_request_path.clone(),
            RequestField::Query => context.inbound_request_query.clone(),
            RequestField::RawQuery => context.inbound_request_query.clone(),
            RequestField::PathWithQuery => request_path_with_query(
                context.inbound_request_path.as_str(),
                context.inbound_request_query.as_str(),
            ),
            RequestField::HttpVersion => context.inbound_request_http_version.clone(),
            RequestField::Scheme => context.inbound_request_scheme.clone(),
            RequestField::Host => context.inbound_request_host.clone(),
            RequestField::ClientIp => context.inbound_request_client_ip.clone(),
        };
        Ok(CallOutcome::Return(vec![Value::String(value)]))
    }
}

struct GetHeaderFunction {
    context: SharedProxyVmContext,
}

impl GetHeaderFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for GetHeaderFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let name = expect_string(args, 0)?;
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;

        let context = self.context.lock().expect("vm context lock poisoned");
        let value = context
            .inbound_request_headers
            .get(&header_name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        Ok(CallOutcome::Return(vec![Value::String(value.to_string())]))
    }
}

struct GetRequestPortFunction {
    context: SharedProxyVmContext,
}

impl GetRequestPortFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for GetRequestPortFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 0)?;
        let context = self.context.lock().expect("vm context lock poisoned");
        Ok(CallOutcome::Return(vec![Value::Int(
            context.inbound_request_port as i64,
        )]))
    }
}

struct GetRequestHeadersFunction {
    context: SharedProxyVmContext,
}

impl GetRequestHeadersFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for GetRequestHeadersFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 0)?;
        let context = self.context.lock().expect("vm context lock poisoned");
        Ok(CallOutcome::Return(vec![headers_to_value_map(
            &context.inbound_request_headers,
        )]))
    }
}

struct GetRequestQueryArgFunction {
    context: SharedProxyVmContext,
}

impl GetRequestQueryArgFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for GetRequestQueryArgFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let name = expect_string(args, 0)?;
        let context = self.context.lock().expect("vm context lock poisoned");
        let value = url::form_urlencoded::parse(context.inbound_request_query.as_bytes())
            .find_map(|(key, value)| {
                if key == name {
                    Some(value.into_owned())
                } else {
                    None
                }
            })
            .unwrap_or_default();
        Ok(CallOutcome::Return(vec![Value::String(value)]))
    }
}

struct GetRequestQueryArgsFunction {
    context: SharedProxyVmContext,
}

impl GetRequestQueryArgsFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for GetRequestQueryArgsFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 0)?;
        let context = self.context.lock().expect("vm context lock poisoned");
        Ok(CallOutcome::Return(vec![query_to_value_map(
            &context.inbound_request_query,
        )]))
    }
}

struct GetRequestBodyFunction {
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
}

impl GetRequestBodyFunction {
    fn new(context: SharedProxyVmContext, async_ops: SharedVmAsyncOps) -> Self {
        Self { context, async_ops }
    }
}

impl HostFunction for GetRequestBodyFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 0)?;
        let context = self.context.lock().expect("vm context lock poisoned");
        let body = String::from_utf8_lossy(&context.inbound_request_body).into_owned();
        drop(context);
        schedule_ready_call(vm, &self.async_ops, vec![Value::String(body)])
    }
}

struct GetRequestBodyChunkFunction {
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
}

impl GetRequestBodyChunkFunction {
    fn new(context: SharedProxyVmContext, async_ops: SharedVmAsyncOps) -> Self {
        Self { context, async_ops }
    }
}

impl HostFunction for GetRequestBodyChunkFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let max_bytes = expect_int(args, 0)?;
        if max_bytes <= 0 {
            return Err(VmError::HostError(format!(
                "body chunk size must be > 0, got '{max_bytes}'",
            )));
        }

        let mut context = self.context.lock().expect("vm context lock poisoned");
        let start = context
            .inbound_request_body_offset
            .min(context.inbound_request_body.len());
        let end = start
            .saturating_add(max_bytes as usize)
            .min(context.inbound_request_body.len());
        let chunk = String::from_utf8_lossy(&context.inbound_request_body[start..end]).into_owned();
        context.inbound_request_body_offset = end;
        drop(context);
        schedule_ready_call(vm, &self.async_ops, vec![Value::String(chunk)])
    }
}

struct GetRequestBodyEofFunction {
    context: SharedProxyVmContext,
}

impl GetRequestBodyEofFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for GetRequestBodyEofFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 0)?;
        let context = self.context.lock().expect("vm context lock poisoned");
        let eof = context.inbound_request_body_offset >= context.inbound_request_body.len();
        Ok(CallOutcome::Return(vec![Value::Bool(eof)]))
    }
}
