use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::http::{HeaderMap, HeaderName, HeaderValue, Method};
use url::Url;
use vm::{CallOutcome, HostFunction, Value, Vm, VmError};

pub type SharedRateLimiter = Arc<Mutex<RateLimiterStore>>;

#[derive(Debug, Default)]
pub struct RateLimiterStore {
    buckets: HashMap<String, RateLimitBucket>,
}

#[derive(Debug)]
struct RateLimitBucket {
    window_start: Instant,
    count: u64,
}

impl RateLimiterStore {
    pub fn new() -> Self {
        Self {
            buckets: HashMap::new(),
        }
    }

    fn allow(&mut self, key: &str, limit: u64, window_seconds: u64) -> bool {
        if limit == 0 || window_seconds == 0 {
            return false;
        }

        let now = Instant::now();
        let window = Duration::from_secs(window_seconds);
        let bucket = self
            .buckets
            .entry(key.to_string())
            .or_insert_with(|| RateLimitBucket {
                window_start: now,
                count: 0,
            });

        if now.duration_since(bucket.window_start) >= window {
            bucket.window_start = now;
            bucket.count = 0;
        }

        if bucket.count < limit {
            bucket.count += 1;
            true
        } else {
            false
        }
    }
}

#[derive(Clone, Debug)]
pub struct HttpRequestContext {
    pub request_id: String,
    pub method: Method,
    pub path: String,
    pub query: String,
    pub http_version: String,
    pub port: u16,
    pub scheme: String,
    pub host: String,
    pub client_ip: String,
    pub body: Vec<u8>,
    pub headers: HeaderMap,
}

#[derive(Clone, Debug)]
pub struct ProxyVmContext {
    inbound_request_id: String,
    inbound_request_method: Method,
    inbound_request_path: String,
    inbound_request_query: String,
    inbound_request_http_version: String,
    inbound_request_port: u16,
    inbound_request_scheme: String,
    inbound_request_host: String,
    inbound_request_client_ip: String,
    inbound_request_body: Vec<u8>,
    inbound_request_headers: HeaderMap,
    outbound_request_method: Method,
    outbound_request_path: String,
    outbound_request_query: String,
    outbound_request_body: Vec<u8>,
    outbound_request_headers: HeaderMap,
    response_headers: HeaderMap,
    response_content: Option<String>,
    response_status: Option<u16>,
    upstream: Option<String>,
    upstream_response_headers: HeaderMap,
    upstream_response_content: Option<String>,
    upstream_response_status: Option<u16>,
    rate_limiter: SharedRateLimiter,
}

impl ProxyVmContext {
    pub fn from_http_request(request: HttpRequestContext, rate_limiter: SharedRateLimiter) -> Self {
        Self {
            inbound_request_id: request.request_id,
            inbound_request_method: request.method.clone(),
            inbound_request_path: request.path.clone(),
            inbound_request_query: request.query.clone(),
            inbound_request_http_version: request.http_version,
            inbound_request_port: request.port,
            inbound_request_scheme: request.scheme,
            inbound_request_host: request.host,
            inbound_request_client_ip: request.client_ip,
            inbound_request_body: request.body.clone(),
            inbound_request_headers: request.headers.clone(),
            outbound_request_method: request.method,
            outbound_request_path: request.path,
            outbound_request_query: request.query,
            outbound_request_body: request.body,
            outbound_request_headers: request.headers,
            response_headers: HeaderMap::new(),
            response_content: None,
            response_status: None,
            upstream: None,
            upstream_response_headers: HeaderMap::new(),
            upstream_response_content: None,
            upstream_response_status: None,
            rate_limiter,
        }
    }

    pub fn from_request_headers(
        request_headers: HeaderMap,
        rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self::from_http_request(
            HttpRequestContext {
                request_id: String::new(),
                method: Method::GET,
                path: "/".to_string(),
                query: String::new(),
                http_version: "1.1".to_string(),
                port: 80,
                scheme: "http".to_string(),
                host: String::new(),
                client_ip: String::new(),
                body: Vec::new(),
                headers: request_headers,
            },
            rate_limiter,
        )
    }
}

pub type SharedProxyVmContext = Arc<Mutex<ProxyVmContext>>;

#[derive(Clone, Debug)]
pub struct VmExecutionOutcome {
    pub response_headers: HeaderMap,
    pub response_content: Option<String>,
    pub response_status: Option<u16>,
    pub upstream: Option<String>,
    pub request_headers: HeaderMap,
    pub request_method: Method,
    pub request_path: String,
    pub request_query: String,
    pub request_body: Vec<u8>,
}

pub fn snapshot_execution_outcome(context: &SharedProxyVmContext) -> VmExecutionOutcome {
    let context = context.lock().expect("vm context lock poisoned");
    VmExecutionOutcome {
        response_headers: context.response_headers.clone(),
        response_content: context.response_content.clone(),
        response_status: context.response_status,
        upstream: context.upstream.clone(),
        request_headers: context.outbound_request_headers.clone(),
        request_method: context.outbound_request_method.clone(),
        request_path: context.outbound_request_path.clone(),
        request_query: context.outbound_request_query.clone(),
        request_body: context.outbound_request_body.clone(),
    }
}

pub fn register_host_module(vm: &mut Vm, context: SharedProxyVmContext) -> Result<(), VmError> {
    vm.bind_function(
        "http::request::get_id",
        Box::new(GetRequestFieldFunction::new(
            context.clone(),
            RequestField::Id,
        )),
    );
    vm.bind_function(
        "http::request::get_method",
        Box::new(GetRequestFieldFunction::new(
            context.clone(),
            RequestField::Method,
        )),
    );
    vm.bind_function(
        "http::request::get_path",
        Box::new(GetRequestFieldFunction::new(
            context.clone(),
            RequestField::Path,
        )),
    );
    vm.bind_function(
        "http::request::get_query",
        Box::new(GetRequestFieldFunction::new(
            context.clone(),
            RequestField::Query,
        )),
    );
    vm.bind_function(
        "http::request::get_scheme",
        Box::new(GetRequestFieldFunction::new(
            context.clone(),
            RequestField::Scheme,
        )),
    );
    vm.bind_function(
        "http::request::get_host",
        Box::new(GetRequestFieldFunction::new(
            context.clone(),
            RequestField::Host,
        )),
    );
    vm.bind_function(
        "http::request::get_header",
        Box::new(GetHeaderFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::upstream::request::set_header",
        Box::new(SetRequestHeaderFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::upstream::request::remove_header",
        Box::new(RemoveRequestHeaderFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::upstream::request::set_method",
        Box::new(SetRequestMethodFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::upstream::request::set_path",
        Box::new(SetRequestPathFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::upstream::request::set_query",
        Box::new(SetRequestQueryFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::request::get_client_ip",
        Box::new(GetRequestFieldFunction::new(
            context.clone(),
            RequestField::ClientIp,
        )),
    );
    vm.bind_function(
        "http::response::set_header",
        Box::new(SetResponseHeaderFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::response::remove_header",
        Box::new(RemoveResponseHeaderFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::response::set_body",
        Box::new(SetResponseContentFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::response::set_status",
        Box::new(SetResponseStatusFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::upstream::request::set_target",
        Box::new(SetUpstreamFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::rate_limit::allow",
        Box::new(RateLimitAllowFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::request::get_headers",
        Box::new(GetRequestHeadersFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::request::get_query_arg",
        Box::new(GetRequestQueryArgFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::request::get_query_args",
        Box::new(GetRequestQueryArgsFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::request::get_path_with_query",
        Box::new(GetRequestFieldFunction::new(
            context.clone(),
            RequestField::PathWithQuery,
        )),
    );
    vm.bind_function(
        "http::request::get_raw_query",
        Box::new(GetRequestFieldFunction::new(
            context.clone(),
            RequestField::RawQuery,
        )),
    );
    vm.bind_function(
        "http::request::get_body",
        Box::new(GetRequestBodyFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::upstream::request::set_body",
        Box::new(SetRequestBodyFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::upstream::request::add_header",
        Box::new(AddRequestHeaderFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::upstream::request::clear_header",
        Box::new(ClearRequestHeaderFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::upstream::request::set_headers",
        Box::new(SetRequestHeadersFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::upstream::request::set_raw_query",
        Box::new(SetRequestQueryFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::upstream::request::set_query_arg",
        Box::new(SetRequestQueryArgFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::request::get_http_version",
        Box::new(GetRequestFieldFunction::new(
            context.clone(),
            RequestField::HttpVersion,
        )),
    );
    vm.bind_function(
        "http::request::get_port",
        Box::new(GetRequestPortFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::response::get_status",
        Box::new(GetResponseStatusFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::response::get_body",
        Box::new(GetResponseBodyFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::response::get_header",
        Box::new(GetResponseHeaderFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::response::get_headers",
        Box::new(GetResponseHeadersFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::response::add_header",
        Box::new(AddResponseHeaderFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::response::clear_header",
        Box::new(ClearResponseHeaderFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::response::set_headers",
        Box::new(SetResponseHeadersFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::upstream::response::get_status",
        Box::new(GetUpstreamResponseStatusFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::upstream::response::get_header",
        Box::new(GetUpstreamResponseHeaderFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::upstream::response::get_headers",
        Box::new(GetUpstreamResponseHeadersFunction::new(context.clone())),
    );
    vm.bind_function(
        "http::upstream::response::get_body",
        Box::new(GetUpstreamResponseBodyFunction::new(context.clone())),
    );

    Ok(())
}

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
}

impl GetRequestBodyFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for GetRequestBodyFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 0)?;
        let context = self.context.lock().expect("vm context lock poisoned");
        let body = String::from_utf8_lossy(&context.inbound_request_body).into_owned();
        Ok(CallOutcome::Return(vec![Value::String(body)]))
    }
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

struct SetResponseHeaderFunction {
    context: SharedProxyVmContext,
}

impl SetResponseHeaderFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for SetResponseHeaderFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 2)?;
        let (header_name, header_value) = parse_header_args(args)?;

        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.response_headers.insert(header_name, header_value);
        Ok(CallOutcome::Return(vec![]))
    }
}

struct AddResponseHeaderFunction {
    context: SharedProxyVmContext,
}

impl AddResponseHeaderFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for AddResponseHeaderFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 2)?;
        let (header_name, header_value) = parse_header_args(args)?;

        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.response_headers.append(header_name, header_value);
        Ok(CallOutcome::Return(vec![]))
    }
}

struct SetResponseHeadersFunction {
    context: SharedProxyVmContext,
}

impl SetResponseHeadersFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for SetResponseHeadersFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let headers = parse_headers_map_arg(args, 0)?;

        let mut context = self.context.lock().expect("vm context lock poisoned");
        for (name, values) in headers {
            context.response_headers.remove(name.clone());
            for value in values {
                context.response_headers.append(name.clone(), value);
            }
        }
        Ok(CallOutcome::Return(vec![]))
    }
}

struct RemoveResponseHeaderFunction {
    context: SharedProxyVmContext,
}

impl RemoveResponseHeaderFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for RemoveResponseHeaderFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let header_name = parse_header_name_arg(args, 0)?;

        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.response_headers.remove(header_name);
        Ok(CallOutcome::Return(vec![]))
    }
}

struct ClearResponseHeaderFunction {
    context: SharedProxyVmContext,
}

impl ClearResponseHeaderFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for ClearResponseHeaderFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        RemoveResponseHeaderFunction::new(self.context.clone()).call(vm, args)
    }
}

struct GetResponseHeaderFunction {
    context: SharedProxyVmContext,
}

impl GetResponseHeaderFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for GetResponseHeaderFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let name = expect_string(args, 0)?;
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;

        let context = self.context.lock().expect("vm context lock poisoned");
        let value = context
            .response_headers
            .get(&header_name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        Ok(CallOutcome::Return(vec![Value::String(value.to_string())]))
    }
}

struct GetResponseHeadersFunction {
    context: SharedProxyVmContext,
}

impl GetResponseHeadersFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for GetResponseHeadersFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 0)?;
        let context = self.context.lock().expect("vm context lock poisoned");
        Ok(CallOutcome::Return(vec![headers_to_value_map(
            &context.response_headers,
        )]))
    }
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

struct SetResponseContentFunction {
    context: SharedProxyVmContext,
}

impl SetResponseContentFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for SetResponseContentFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let body = expect_string(args, 0)?;
        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.response_content = Some(body);
        Ok(CallOutcome::Return(vec![]))
    }
}

struct GetResponseBodyFunction {
    context: SharedProxyVmContext,
}

impl GetResponseBodyFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for GetResponseBodyFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 0)?;
        let context = self.context.lock().expect("vm context lock poisoned");
        let value = context.response_content.clone().unwrap_or_default();
        Ok(CallOutcome::Return(vec![Value::String(value)]))
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

struct SetResponseStatusFunction {
    context: SharedProxyVmContext,
}

impl SetResponseStatusFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for SetResponseStatusFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let status = expect_int(args, 0)?;
        if !(100..=599).contains(&status) {
            return Err(VmError::HostError(format!(
                "status code must be in range 100..=599, got '{status}'",
            )));
        }

        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.response_status = Some(status as u16);
        Ok(CallOutcome::Return(vec![]))
    }
}

struct GetResponseStatusFunction {
    context: SharedProxyVmContext,
}

impl GetResponseStatusFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for GetResponseStatusFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 0)?;
        let context = self.context.lock().expect("vm context lock poisoned");
        let status = context.response_status.unwrap_or(0);
        Ok(CallOutcome::Return(vec![Value::Int(status as i64)]))
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

struct RateLimitAllowFunction {
    context: SharedProxyVmContext,
}

impl RateLimitAllowFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for RateLimitAllowFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 3)?;
        let key = expect_string(args, 0)?;
        let limit = expect_int(args, 1)?;
        let window_seconds = expect_int(args, 2)?;
        if limit <= 0 || window_seconds <= 0 {
            return Ok(CallOutcome::Return(vec![Value::Bool(false)]));
        }

        let rate_limiter = {
            let context = self.context.lock().expect("vm context lock poisoned");
            context.rate_limiter.clone()
        };
        let allowed = rate_limiter
            .lock()
            .expect("rate limiter lock poisoned")
            .allow(&key, limit as u64, window_seconds as u64);
        Ok(CallOutcome::Return(vec![Value::Bool(allowed)]))
    }
}

fn expect_arg_count(args: &[Value], expected: usize) -> Result<(), VmError> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(VmError::HostError(format!(
            "expected {expected} arguments, got {}",
            args.len()
        )))
    }
}

fn expect_string(args: &[Value], index: usize) -> Result<String, VmError> {
    match args.get(index) {
        Some(Value::String(value)) => Ok(value.clone()),
        _ => Err(VmError::TypeMismatch("string")),
    }
}

fn expect_int(args: &[Value], index: usize) -> Result<i64, VmError> {
    match args.get(index) {
        Some(Value::Int(value)) => Ok(*value),
        _ => Err(VmError::TypeMismatch("int")),
    }
}

fn expect_map(args: &[Value], index: usize) -> Result<Vec<(Value, Value)>, VmError> {
    match args.get(index) {
        Some(Value::Map(entries)) => Ok(entries.clone()),
        _ => Err(VmError::TypeMismatch("map")),
    }
}

fn parse_header_name_arg(args: &[Value], index: usize) -> Result<HeaderName, VmError> {
    let name = expect_string(args, index)?;
    HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))
}

fn parse_header_args(args: &[Value]) -> Result<(HeaderName, HeaderValue), VmError> {
    let name = expect_string(args, 0)?;
    let value = expect_string(args, 1)?;
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;
    let header_value = HeaderValue::from_str(&value)
        .map_err(|_| VmError::HostError(format!("invalid header value '{value}'")))?;
    Ok((header_name, header_value))
}

fn parse_headers_map_arg(
    args: &[Value],
    index: usize,
) -> Result<Vec<(HeaderName, Vec<HeaderValue>)>, VmError> {
    let entries = expect_map(args, index)?;
    let mut parsed = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        let name = match key {
            Value::String(name) => name,
            _ => {
                return Err(VmError::HostError(
                    "header map keys must be strings".to_string(),
                ));
            }
        };
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;

        let values = match value {
            Value::String(single) => vec![single],
            Value::Array(values) => {
                let mut collected = Vec::with_capacity(values.len());
                for value in values {
                    match value {
                        Value::String(item) => collected.push(item),
                        _ => {
                            return Err(VmError::HostError(
                                "header map values must be strings or arrays of strings"
                                    .to_string(),
                            ));
                        }
                    }
                }
                collected
            }
            _ => {
                return Err(VmError::HostError(
                    "header map values must be strings or arrays of strings".to_string(),
                ));
            }
        };

        let mut header_values = Vec::with_capacity(values.len());
        for value in values {
            let header_value = HeaderValue::from_str(&value)
                .map_err(|_| VmError::HostError(format!("invalid header value '{value}'")))?;
            header_values.push(header_value);
        }
        parsed.push((header_name, header_values));
    }
    Ok(parsed)
}

fn request_path_with_query(path: &str, query: &str) -> String {
    if query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{query}")
    }
}

fn headers_to_value_map(headers: &HeaderMap) -> Value {
    let mut values = BTreeMap::<String, Vec<String>>::new();
    for (name, value) in headers {
        let header_name = name.as_str().to_string();
        let header_value = value.to_str().unwrap_or_default().to_string();
        values.entry(header_name).or_default().push(header_value);
    }
    Value::Map(
        values
            .into_iter()
            .map(|(name, values)| {
                let value = if values.len() == 1 {
                    Value::String(values[0].clone())
                } else {
                    Value::Array(values.into_iter().map(Value::String).collect())
                };
                (Value::String(name), value)
            })
            .collect(),
    )
}

fn query_to_value_map(query: &str) -> Value {
    let mut values = BTreeMap::<String, Vec<String>>::new();
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        values
            .entry(name.into_owned())
            .or_default()
            .push(value.into_owned());
    }
    Value::Map(
        values
            .into_iter()
            .map(|(name, values)| {
                let value = if values.len() == 1 {
                    Value::String(values[0].clone())
                } else {
                    Value::Array(values.into_iter().map(Value::String).collect())
                };
                (Value::String(name), value)
            })
            .collect(),
    )
}

fn serialize_query_pairs(pairs: Vec<(String, String)>) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(&key, &value);
    }
    serializer.finish()
}

fn is_valid_request_path(value: &str) -> bool {
    !value.is_empty()
        && value.starts_with('/')
        && !value.contains('?')
        && !value.contains('#')
        && !value.chars().any(|ch| ch.is_whitespace())
}

fn is_valid_upstream(value: &str) -> bool {
    if value.is_empty()
        || value.contains('/')
        || value.contains('?')
        || value.contains('#')
        || value.chars().any(|ch| ch.is_whitespace())
    {
        if let Ok(url) = Url::parse(value) {
            if url.scheme() != "http" && url.scheme() != "https" {
                return false;
            }
            if url.host_str().is_none() {
                return false;
            }
            if !url.username().is_empty() || url.password().is_some() {
                return false;
            }
            return true;
        }
        return false;
    }

    let Some((host, port)) = value.rsplit_once(':') else {
        return false;
    };
    if host.is_empty() || port.is_empty() || host.contains(':') {
        return false;
    }
    match port.parse::<u16>() {
        Ok(port) => port != 0,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopFunction;

    impl HostFunction for NoopFunction {
        fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, VmError> {
            Ok(CallOutcome::Return(vec![]))
        }
    }

    fn dummy_vm() -> Vm {
        Vm::new(vm::Program::new(vec![], vec![vm::OpCode::Ret as u8]))
    }

    fn empty_context() -> SharedProxyVmContext {
        Arc::new(Mutex::new(ProxyVmContext::from_request_headers(
            HeaderMap::new(),
            Arc::new(Mutex::new(RateLimiterStore::new())),
        )))
    }

    #[test]
    fn register_host_module_allows_preexisting_bindings() {
        let mut vm = dummy_vm();
        vm.register_function(Box::new(NoopFunction));

        let result = register_host_module(&mut vm, empty_context());
        assert!(result.is_ok());
    }

    #[test]
    fn get_header_reads_request_header_and_returns_empty_if_missing() {
        let mut headers = HeaderMap::new();
        headers.insert("x-hello", HeaderValue::from_static("world"));
        let context = Arc::new(Mutex::new(ProxyVmContext::from_request_headers(
            headers,
            Arc::new(Mutex::new(RateLimiterStore::new())),
        )));
        let mut function = GetHeaderFunction::new(context);
        let mut vm = dummy_vm();

        let present = function
            .call(&mut vm, &[Value::String("x-hello".to_string())])
            .expect("call should succeed");
        assert_eq!(
            present,
            CallOutcome::Return(vec![Value::String("world".to_string())])
        );

        let missing = function
            .call(&mut vm, &[Value::String("x-missing".to_string())])
            .expect("call should succeed");
        assert_eq!(
            missing,
            CallOutcome::Return(vec![Value::String(String::new())])
        );
    }

    #[test]
    fn set_response_header_stores_response_header() {
        let context = empty_context();
        let mut function = SetResponseHeaderFunction::new(context.clone());
        let mut vm = dummy_vm();

        let result = function.call(
            &mut vm,
            &[
                Value::String("x-set".to_string()),
                Value::String("ok".to_string()),
            ],
        );
        assert!(matches!(result, Ok(CallOutcome::Return(_))));

        let guard = context.lock().expect("vm context lock poisoned");
        let value = guard
            .response_headers
            .get("x-set")
            .and_then(|value| value.to_str().ok());
        assert_eq!(value, Some("ok"));
    }

    #[test]
    fn request_header_mutators_update_outbound_headers() {
        let context = empty_context();
        let mut setter = SetRequestHeaderFunction::new(context.clone());
        let mut remover = RemoveRequestHeaderFunction::new(context.clone());
        let mut vm = dummy_vm();

        let set = setter.call(
            &mut vm,
            &[
                Value::String("x-added".to_string()),
                Value::String("1".to_string()),
            ],
        );
        assert!(matches!(set, Ok(CallOutcome::Return(_))));
        let guard = context.lock().expect("vm context lock poisoned");
        assert_eq!(
            guard
                .outbound_request_headers
                .get("x-added")
                .and_then(|value| value.to_str().ok()),
            Some("1")
        );
        drop(guard);

        let removed = remover.call(&mut vm, &[Value::String("x-added".to_string())]);
        assert!(matches!(removed, Ok(CallOutcome::Return(_))));
        let guard = context.lock().expect("vm context lock poisoned");
        assert!(guard.outbound_request_headers.get("x-added").is_none());
    }

    #[test]
    fn set_upstream_accepts_valid_and_rejects_invalid_values() {
        let context = empty_context();
        let mut function = SetUpstreamFunction::new(context.clone());
        let mut vm = dummy_vm();

        let ok = function.call(&mut vm, &[Value::String("localhost:8080".to_string())]);
        assert!(matches!(ok, Ok(CallOutcome::Return(_))));
        {
            let guard = context.lock().expect("vm context lock poisoned");
            assert_eq!(guard.upstream.as_deref(), Some("localhost:8080"));
        }

        let ok = function.call(
            &mut vm,
            &[Value::String("https://example.com/path".to_string())],
        );
        assert!(matches!(ok, Ok(CallOutcome::Return(_))));
        {
            let guard = context.lock().expect("vm context lock poisoned");
            assert_eq!(guard.upstream.as_deref(), Some("https://example.com/path"));
        }

        let err = function.call(&mut vm, &[Value::String("ftp://localhost".to_string())]);
        assert!(matches!(err, Err(VmError::HostError(_))));
    }

    #[test]
    fn set_response_content_marks_short_circuit_body() {
        let context = empty_context();
        let mut function = SetResponseContentFunction::new(context.clone());
        let mut vm = dummy_vm();

        let result = function.call(&mut vm, &[Value::String("hello".to_string())]);
        assert!(matches!(result, Ok(CallOutcome::Return(_))));

        let guard = context.lock().expect("vm context lock poisoned");
        assert_eq!(guard.response_content.as_deref(), Some("hello"));
    }

    #[test]
    fn set_response_status_stores_status_code() {
        let context = empty_context();
        let mut function = SetResponseStatusFunction::new(context.clone());
        let mut vm = dummy_vm();

        let ok = function.call(&mut vm, &[Value::Int(429)]);
        assert!(matches!(ok, Ok(CallOutcome::Return(_))));
        {
            let guard = context.lock().expect("vm context lock poisoned");
            assert_eq!(guard.response_status, Some(429));
        }

        let err = function.call(&mut vm, &[Value::Int(42)]);
        assert!(matches!(err, Err(VmError::HostError(_))));
    }

    #[test]
    fn request_method_path_and_query_are_mutable() {
        let context = Arc::new(Mutex::new(ProxyVmContext::from_http_request(
            HttpRequestContext {
                request_id: "req-1".to_string(),
                method: Method::GET,
                path: "/old".to_string(),
                query: "a=1".to_string(),
                http_version: "1.1".to_string(),
                port: 80,
                scheme: "http".to_string(),
                host: "example.com".to_string(),
                client_ip: "127.0.0.1".to_string(),
                body: Vec::new(),
                headers: HeaderMap::new(),
            },
            Arc::new(Mutex::new(RateLimiterStore::new())),
        )));
        let mut method_fn = SetRequestMethodFunction::new(context.clone());
        let mut path_fn = SetRequestPathFunction::new(context.clone());
        let mut query_fn = SetRequestQueryFunction::new(context.clone());
        let mut vm = dummy_vm();

        method_fn
            .call(&mut vm, &[Value::String("POST".to_string())])
            .expect("method should update");
        path_fn
            .call(&mut vm, &[Value::String("/new".to_string())])
            .expect("path should update");
        query_fn
            .call(&mut vm, &[Value::String("?x=2".to_string())])
            .expect("query should update");

        let snapshot = snapshot_execution_outcome(&context);
        assert_eq!(snapshot.request_method, Method::POST);
        assert_eq!(snapshot.request_path, "/new");
        assert_eq!(snapshot.request_query, "x=2");
    }

    #[test]
    fn request_field_getters_return_http_metadata() {
        let context = Arc::new(Mutex::new(ProxyVmContext::from_http_request(
            HttpRequestContext {
                request_id: "r-123".to_string(),
                method: Method::PUT,
                path: "/v1/items".to_string(),
                query: "x=1".to_string(),
                http_version: "2".to_string(),
                port: 443,
                scheme: "https".to_string(),
                host: "api.example.com".to_string(),
                client_ip: "10.1.2.3".to_string(),
                body: Vec::new(),
                headers: HeaderMap::new(),
            },
            Arc::new(Mutex::new(RateLimiterStore::new())),
        )));
        let mut vm = dummy_vm();

        let mut id_fn = GetRequestFieldFunction::new(context.clone(), RequestField::Id);
        let id = id_fn.call(&mut vm, &[]).expect("id call should work");
        assert_eq!(
            id,
            CallOutcome::Return(vec![Value::String("r-123".to_string())])
        );

        let mut method_fn = GetRequestFieldFunction::new(context.clone(), RequestField::Method);
        let method = method_fn
            .call(&mut vm, &[])
            .expect("method call should work");
        assert_eq!(
            method,
            CallOutcome::Return(vec![Value::String("PUT".to_string())])
        );

        let mut ip_fn = GetRequestFieldFunction::new(context, RequestField::ClientIp);
        let client_ip = ip_fn.call(&mut vm, &[]).expect("ip call should work");
        assert_eq!(
            client_ip,
            CallOutcome::Return(vec![Value::String("10.1.2.3".to_string())])
        );
    }

    #[test]
    fn rate_limit_allow_limits_by_key_within_window() {
        let shared_limiter = Arc::new(Mutex::new(RateLimiterStore::new()));
        let context = Arc::new(Mutex::new(ProxyVmContext::from_request_headers(
            HeaderMap::new(),
            shared_limiter,
        )));
        let mut function = RateLimitAllowFunction::new(context);
        let mut vm = dummy_vm();

        let args = [
            Value::String("client-a".to_string()),
            Value::Int(2),
            Value::Int(60),
        ];

        let first = function
            .call(&mut vm, &args)
            .expect("first call should succeed");
        assert_eq!(first, CallOutcome::Return(vec![Value::Bool(true)]));

        let second = function
            .call(&mut vm, &args)
            .expect("second call should succeed");
        assert_eq!(second, CallOutcome::Return(vec![Value::Bool(true)]));

        let third = function
            .call(&mut vm, &args)
            .expect("third call should succeed");
        assert_eq!(third, CallOutcome::Return(vec![Value::Bool(false)]));
    }

    #[test]
    fn request_query_helpers_and_mutators_work() {
        let context = Arc::new(Mutex::new(ProxyVmContext::from_http_request(
            HttpRequestContext {
                request_id: "q-1".to_string(),
                method: Method::GET,
                path: "/items".to_string(),
                query: "a=1&a=2&b=3".to_string(),
                http_version: "1.1".to_string(),
                port: 80,
                scheme: "http".to_string(),
                host: "example.com".to_string(),
                client_ip: "127.0.0.1".to_string(),
                body: Vec::new(),
                headers: HeaderMap::new(),
            },
            Arc::new(Mutex::new(RateLimiterStore::new())),
        )));
        let mut vm = dummy_vm();

        let mut get_arg = GetRequestQueryArgFunction::new(context.clone());
        let arg = get_arg
            .call(&mut vm, &[Value::String("a".to_string())])
            .expect("query arg should return");
        assert_eq!(
            arg,
            CallOutcome::Return(vec![Value::String("1".to_string())])
        );

        let mut get_args = GetRequestQueryArgsFunction::new(context.clone());
        let args = get_args
            .call(&mut vm, &[])
            .expect("query args should return");
        let CallOutcome::Return(values) = args else {
            panic!("expected return");
        };
        let all = values.first().expect("map value should be returned");
        assert_eq!(
            map_get(all, "a"),
            Some(&Value::Array(vec![
                Value::String("1".to_string()),
                Value::String("2".to_string())
            ]))
        );
        assert_eq!(map_get(all, "b"), Some(&Value::String("3".to_string())));

        let mut set_arg = SetRequestQueryArgFunction::new(context.clone());
        set_arg
            .call(
                &mut vm,
                &[
                    Value::String("b".to_string()),
                    Value::String("9".to_string()),
                ],
            )
            .expect("query arg update should work");
        let snapshot = snapshot_execution_outcome(&context);
        assert_eq!(snapshot.request_query, "a=1&a=2&b=9");

        let mut path_with_query =
            GetRequestFieldFunction::new(context.clone(), RequestField::PathWithQuery);
        let value = path_with_query
            .call(&mut vm, &[])
            .expect("path with query should return");
        assert_eq!(
            value,
            CallOutcome::Return(vec![Value::String("/items?a=1&a=2&b=3".to_string())])
        );
    }

    #[test]
    fn request_and_response_body_status_getters_work() {
        let context = Arc::new(Mutex::new(ProxyVmContext::from_http_request(
            HttpRequestContext {
                request_id: "b-1".to_string(),
                method: Method::POST,
                path: "/submit".to_string(),
                query: String::new(),
                http_version: "2".to_string(),
                port: 443,
                scheme: "https".to_string(),
                host: "api.example.com".to_string(),
                client_ip: "10.0.0.1".to_string(),
                body: b"old".to_vec(),
                headers: HeaderMap::new(),
            },
            Arc::new(Mutex::new(RateLimiterStore::new())),
        )));
        let mut vm = dummy_vm();

        let mut get_body = GetRequestBodyFunction::new(context.clone());
        let before = get_body.call(&mut vm, &[]).expect("body should return");
        assert_eq!(
            before,
            CallOutcome::Return(vec![Value::String("old".to_string())])
        );

        let mut set_body = SetRequestBodyFunction::new(context.clone());
        set_body
            .call(&mut vm, &[Value::String("new".to_string())])
            .expect("body should set");

        let mut set_status = SetResponseStatusFunction::new(context.clone());
        set_status
            .call(&mut vm, &[Value::Int(201)])
            .expect("status should set");
        let mut set_response_body = SetResponseContentFunction::new(context.clone());
        set_response_body
            .call(&mut vm, &[Value::String("ok".to_string())])
            .expect("response body should set");

        let mut get_status = GetResponseStatusFunction::new(context.clone());
        let status = get_status.call(&mut vm, &[]).expect("status should return");
        assert_eq!(status, CallOutcome::Return(vec![Value::Int(201)]));

        let mut get_response_body = GetResponseBodyFunction::new(context.clone());
        let body = get_response_body
            .call(&mut vm, &[])
            .expect("response body should return");
        assert_eq!(
            body,
            CallOutcome::Return(vec![Value::String("ok".to_string())])
        );

        let snapshot = snapshot_execution_outcome(&context);
        assert_eq!(snapshot.request_body, b"new".to_vec());
    }

    #[test]
    fn set_headers_and_get_headers_map_round_trip() {
        let mut inbound_headers = HeaderMap::new();
        inbound_headers.insert("x-one", HeaderValue::from_static("1"));
        inbound_headers.append("x-many", HeaderValue::from_static("a"));
        inbound_headers.append("x-many", HeaderValue::from_static("b"));
        let context = Arc::new(Mutex::new(ProxyVmContext::from_request_headers(
            inbound_headers,
            Arc::new(Mutex::new(RateLimiterStore::new())),
        )));
        let mut set_request_headers = SetRequestHeadersFunction::new(context.clone());
        let mut get_request_headers = GetRequestHeadersFunction::new(context.clone());
        let mut vm = dummy_vm();

        let headers_map = Value::Map(vec![
            (
                Value::String("x-one".to_string()),
                Value::String("1".to_string()),
            ),
            (
                Value::String("x-many".to_string()),
                Value::Array(vec![
                    Value::String("a".to_string()),
                    Value::String("b".to_string()),
                ]),
            ),
        ]);
        set_request_headers
            .call(&mut vm, &[headers_map])
            .expect("set headers should succeed");
        let guard = context.lock().expect("vm context lock poisoned");
        assert!(guard.outbound_request_headers.contains_key("x-one"));
        drop(guard);
        let headers = get_request_headers
            .call(&mut vm, &[])
            .expect("get headers should succeed");
        let CallOutcome::Return(values) = headers else {
            panic!("expected return");
        };
        let all = values.first().expect("map value should be returned");
        assert_eq!(map_get(all, "x-one"), Some(&Value::String("1".to_string())));
        assert_eq!(
            map_get(all, "x-many"),
            Some(&Value::Array(vec![
                Value::String("a".to_string()),
                Value::String("b".to_string())
            ]))
        );
    }

    fn map_get<'a>(map_value: &'a Value, key: &str) -> Option<&'a Value> {
        let Value::Map(entries) = map_value else {
            return None;
        };
        entries.iter().find_map(|(entry_key, entry_value)| {
            let Value::String(name) = entry_key else {
                return None;
            };
            if name == key { Some(entry_value) } else { None }
        })
    }
}
