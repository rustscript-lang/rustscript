use axum::http::Method;
use vm::{CallOutcome, HostFunction, Value, Vm, VmError};

use super::super::super::{
    SharedProxyVmContext, SharedVmAsyncOps, bind_async_host, expect_arg_count, expect_string,
    is_valid_request_path, is_valid_upstream, parse_header_args, parse_header_name_arg,
    parse_headers_map_arg, serialize_query_pairs,
};

pub(super) fn register_7_to_11(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    bind_async_host(
        vm,
        &async_ops,
        "http::upstream::request::set_header",
        Box::new(SetRequestHeaderFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::upstream::request::remove_header",
        Box::new(RemoveRequestHeaderFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::upstream::request::set_method",
        Box::new(SetRequestMethodFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::upstream::request::set_path",
        Box::new(SetRequestPathFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::upstream::request::set_query",
        Box::new(SetRequestQueryFunction::new(context.clone())),
    );
}

pub(super) fn register_17(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    bind_async_host(
        vm,
        &async_ops,
        "http::upstream::request::set_target",
        Box::new(SetUpstreamFunction::new(context)),
    );
}

pub(super) fn register_25_to_30(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    bind_async_host(
        vm,
        &async_ops,
        "http::upstream::request::set_body",
        Box::new(SetRequestBodyFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::upstream::request::add_header",
        Box::new(AddRequestHeaderFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::upstream::request::clear_header",
        Box::new(ClearRequestHeaderFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::upstream::request::set_headers",
        Box::new(SetRequestHeadersFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::upstream::request::set_raw_query",
        Box::new(SetRequestQueryFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::upstream::request::set_query_arg",
        Box::new(SetRequestQueryArgFunction::new(context)),
    );
}

struct SetRequestHeaderFunction {
    context: SharedProxyVmContext,
}

impl SetRequestHeaderFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for SetRequestHeaderFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 2)?;
        let (header_name, header_value) = parse_header_args(args)?;

        let mut context = self.context.lock().expect("vm context lock poisoned");
        context
            .outbound_request_headers
            .insert(header_name, header_value);
        Ok(CallOutcome::Return(vec![]))
    }
}

struct AddRequestHeaderFunction {
    context: SharedProxyVmContext,
}

impl AddRequestHeaderFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for AddRequestHeaderFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 2)?;
        let (header_name, header_value) = parse_header_args(args)?;

        let mut context = self.context.lock().expect("vm context lock poisoned");
        context
            .outbound_request_headers
            .append(header_name, header_value);
        Ok(CallOutcome::Return(vec![]))
    }
}

struct SetRequestHeadersFunction {
    context: SharedProxyVmContext,
}

impl SetRequestHeadersFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for SetRequestHeadersFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let headers = parse_headers_map_arg(args, 0)?;

        let mut context = self.context.lock().expect("vm context lock poisoned");
        for (name, values) in headers {
            context.outbound_request_headers.remove(name.clone());
            for value in values {
                context.outbound_request_headers.append(name.clone(), value);
            }
        }
        Ok(CallOutcome::Return(vec![]))
    }
}

struct RemoveRequestHeaderFunction {
    context: SharedProxyVmContext,
}

impl RemoveRequestHeaderFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for RemoveRequestHeaderFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let header_name = parse_header_name_arg(args, 0)?;

        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.outbound_request_headers.remove(header_name);
        Ok(CallOutcome::Return(vec![]))
    }
}

struct ClearRequestHeaderFunction {
    context: SharedProxyVmContext,
}

impl ClearRequestHeaderFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for ClearRequestHeaderFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        RemoveRequestHeaderFunction::new(self.context.clone()).call(vm, args)
    }
}

struct SetRequestMethodFunction {
    context: SharedProxyVmContext,
}

impl SetRequestMethodFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for SetRequestMethodFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let method = expect_string(args, 0)?;
        let parsed = Method::from_bytes(method.as_bytes())
            .map_err(|_| VmError::HostError(format!("invalid http method '{method}'")))?;

        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.outbound_request_method = parsed;
        Ok(CallOutcome::Return(vec![]))
    }
}

struct SetRequestPathFunction {
    context: SharedProxyVmContext,
}

impl SetRequestPathFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for SetRequestPathFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let path = expect_string(args, 0)?;
        if !is_valid_request_path(&path) {
            return Err(VmError::HostError(format!(
                "path must start with '/' and must not contain whitespace, '?', or '#', got '{path}'",
            )));
        }

        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.outbound_request_path = path;
        Ok(CallOutcome::Return(vec![]))
    }
}

struct SetRequestQueryFunction {
    context: SharedProxyVmContext,
}

impl SetRequestQueryFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for SetRequestQueryFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let raw_query = expect_string(args, 0)?;
        let query = raw_query.strip_prefix('?').unwrap_or(raw_query.as_str());
        if query.contains('#') || query.chars().any(|ch| ch.is_whitespace()) {
            return Err(VmError::HostError(format!(
                "query must not contain whitespace or '#', got '{raw_query}'",
            )));
        }

        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.outbound_request_query = query.to_string();
        Ok(CallOutcome::Return(vec![]))
    }
}

struct SetRequestQueryArgFunction {
    context: SharedProxyVmContext,
}

impl SetRequestQueryArgFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for SetRequestQueryArgFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 2)?;
        let key = expect_string(args, 0)?;
        let value = expect_string(args, 1)?;

        let mut context = self.context.lock().expect("vm context lock poisoned");
        let mut pairs = url::form_urlencoded::parse(context.outbound_request_query.as_bytes())
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        pairs.retain(|(name, _)| name != &key);
        pairs.push((key, value));
        context.outbound_request_query = serialize_query_pairs(pairs);
        Ok(CallOutcome::Return(vec![]))
    }
}

struct SetRequestBodyFunction {
    context: SharedProxyVmContext,
}

impl SetRequestBodyFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for SetRequestBodyFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let body = expect_string(args, 0)?;
        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.outbound_request_body = body.into_bytes();
        Ok(CallOutcome::Return(vec![]))
    }
}

struct SetUpstreamFunction {
    context: SharedProxyVmContext,
}

impl SetUpstreamFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for SetUpstreamFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let upstream = expect_string(args, 0)?;
        if !is_valid_upstream(&upstream) {
            return Err(VmError::HostError(format!(
                "upstream must be host:port or http(s)://host[:port][/path], got '{upstream}'",
            )));
        }

        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.upstream = Some(upstream);
        Ok(CallOutcome::Return(vec![]))
    }
}
