use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use axum::http::{HeaderMap, HeaderName, HeaderValue, Method};
use tokio::sync::oneshot;
use url::Url;
use vm::{CallOutcome, HostAsyncBridge, HostFunction, HostOpId, Value, Vm, VmError};

mod http;
mod io;
mod runtime;

pub type SharedRateLimiter = Arc<Mutex<RateLimiterStore>>;
pub type SharedVmAsyncOps = Arc<Mutex<VmAsyncOps>>;

type AsyncOpResult = Result<Vec<Value>, VmError>;
type PendingFuture = Pin<Box<dyn Future<Output = AsyncOpResult> + Send + 'static>>;

enum PendingOp {
    Receiver(oneshot::Receiver<AsyncOpResult>),
    Future(PendingFuture),
}

#[derive(Default)]
pub struct VmAsyncOps {
    pending: HashMap<HostOpId, PendingOp>,
    runtime_handle: Option<tokio::runtime::Handle>,
}

impl VmAsyncOps {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            runtime_handle: tokio::runtime::Handle::try_current().ok(),
        }
    }

    pub fn with_runtime_handle(runtime_handle: tokio::runtime::Handle) -> Self {
        Self {
            pending: HashMap::new(),
            runtime_handle: Some(runtime_handle),
        }
    }

    pub fn schedule_ready(
        &mut self,
        vm: &mut Vm,
        result: AsyncOpResult,
    ) -> Result<HostOpId, VmError> {
        let op_id = vm.allocate_host_op_id();
        let (sender, receiver) = oneshot::channel();
        self.insert_pending(op_id, PendingOp::Receiver(receiver))?;
        sender
            .send(result)
            .map_err(|_| VmError::HostError(format!("failed to complete host op {op_id}")))?;
        Ok(op_id)
    }

    pub fn schedule_future<F>(&mut self, vm: &mut Vm, future: F) -> Result<HostOpId, VmError>
    where
        F: Future<Output = AsyncOpResult> + Send + 'static,
    {
        let op_id = vm.allocate_host_op_id();
        if self.runtime_handle.is_none() {
            self.runtime_handle = tokio::runtime::Handle::try_current().ok();
        }
        if self.runtime_handle.is_some() {
            self.insert_pending(op_id, PendingOp::Future(Box::pin(future)))?;
            return Ok(op_id);
        }

        let (sender, receiver) = oneshot::channel();
        std::thread::Builder::new()
            .name(format!("pd-edge-host-op-{op_id}"))
            .spawn(move || {
                let result = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime.block_on(future),
                    Err(err) => Err(VmError::HostError(format!(
                        "failed to build async runtime for host op {op_id}: {err}",
                    ))),
                };
                let _ = sender.send(result);
            })
            .map_err(|err| {
                VmError::HostError(format!("failed to spawn async host op thread: {err}"))
            })?;
        self.insert_pending(op_id, PendingOp::Receiver(receiver))?;
        Ok(op_id)
    }

    fn insert_pending(&mut self, op_id: HostOpId, pending_op: PendingOp) -> Result<(), VmError> {
        if self.pending.contains_key(&op_id) {
            return Err(VmError::HostError(format!(
                "duplicate async host op id {op_id}"
            )));
        }
        self.pending.insert(op_id, pending_op);
        Ok(())
    }

    fn poll_op(&mut self, op_id: HostOpId, cx: &mut Context<'_>) -> Poll<AsyncOpResult> {
        let poll_state = {
            let pending_op = match self.pending.get_mut(&op_id) {
                Some(pending_op) => pending_op,
                None => {
                    return Poll::Ready(Err(VmError::HostError(format!(
                        "unknown async host op {op_id}",
                    ))));
                }
            };
            match pending_op {
                PendingOp::Receiver(receiver) => Pin::new(receiver).poll(cx),
                PendingOp::Future(future) => {
                    if tokio::runtime::Handle::try_current().is_ok() {
                        future.as_mut().poll(cx).map(Ok)
                    } else {
                        let _runtime_guard =
                            self.runtime_handle.as_ref().map(|handle| handle.enter());
                        future.as_mut().poll(cx).map(Ok)
                    }
                }
            }
        };

        match poll_state {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(result)) => {
                self.pending.remove(&op_id);
                Poll::Ready(result)
            }
            Poll::Ready(Err(_)) => {
                self.pending.remove(&op_id);
                Poll::Ready(Err(VmError::HostError(format!(
                    "async host op {op_id} was cancelled",
                ))))
            }
        }
    }
}

pub struct VmAsyncOpBridge {
    ops: SharedVmAsyncOps,
}

impl VmAsyncOpBridge {
    pub fn new(ops: SharedVmAsyncOps) -> Self {
        Self { ops }
    }
}

impl HostAsyncBridge for VmAsyncOpBridge {
    fn poll_op(&mut self, op_id: HostOpId, cx: &mut Context<'_>) -> Poll<AsyncOpResult> {
        let mut guard = self.ops.lock().expect("vm async ops lock poisoned");
        guard.poll_op(op_id, cx)
    }
}

pub fn new_shared_vm_async_ops() -> SharedVmAsyncOps {
    Arc::new(Mutex::new(VmAsyncOps::new()))
}

struct AsyncHostAdapter {
    inner: Box<dyn HostFunction>,
    async_ops: SharedVmAsyncOps,
}

impl AsyncHostAdapter {
    fn new(inner: Box<dyn HostFunction>, async_ops: SharedVmAsyncOps) -> Self {
        Self { inner, async_ops }
    }
}

impl HostFunction for AsyncHostAdapter {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        match self.inner.call(vm, args)? {
            CallOutcome::Return(values) => schedule_ready_call(vm, &self.async_ops, values),
            CallOutcome::Yield => Ok(CallOutcome::Yield),
            CallOutcome::Pending(op_id) => Ok(CallOutcome::Pending(op_id)),
        }
    }
}

fn bind_async_host(
    vm: &mut Vm,
    async_ops: &SharedVmAsyncOps,
    name: impl Into<String>,
    function: Box<dyn HostFunction>,
) {
    vm.bind_function(
        name,
        Box::new(AsyncHostAdapter::new(function, async_ops.clone())),
    );
}

pub trait EdgeProtocolHostModule {
    fn register(
        &self,
        vm: &mut Vm,
        context: SharedProxyVmContext,
        async_ops: SharedVmAsyncOps,
    ) -> Result<(), VmError>;
}

pub struct RuntimeProtocolHostModule;

impl EdgeProtocolHostModule for RuntimeProtocolHostModule {
    fn register(
        &self,
        vm: &mut Vm,
        _context: SharedProxyVmContext,
        async_ops: SharedVmAsyncOps,
    ) -> Result<(), VmError> {
        register_runtime_host_module(vm, async_ops)
    }
}

pub struct HttpProtocolHostModule;

impl EdgeProtocolHostModule for HttpProtocolHostModule {
    fn register(
        &self,
        vm: &mut Vm,
        context: SharedProxyVmContext,
        async_ops: SharedVmAsyncOps,
    ) -> Result<(), VmError> {
        register_http_host_module(vm, context, async_ops)
    }
}

pub fn register_protocol_modules(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
    modules: &[&dyn EdgeProtocolHostModule],
) -> Result<(), VmError> {
    for module in modules {
        module.register(vm, context.clone(), async_ops.clone())?;
    }
    Ok(())
}

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
enum EdgeVirtualIoHandle {
    BufferedRead { text: String, offset: usize },
    FileWrite { path: String, append: bool },
}

const EDGE_IO_HANDLE_DYNAMIC_BASE: i64 = 1024;

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
    inbound_request_body_offset: usize,
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
    edge_io_next_handle: i64,
    edge_io_handles: HashMap<i64, EdgeVirtualIoHandle>,
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
            inbound_request_body_offset: 0,
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
            edge_io_next_handle: EDGE_IO_HANDLE_DYNAMIC_BASE,
            edge_io_handles: HashMap::new(),
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

pub fn register_host_module(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) -> Result<(), VmError> {
    static HTTP_PROTOCOL_MODULE: HttpProtocolHostModule = HttpProtocolHostModule;
    static RUNTIME_PROTOCOL_MODULE: RuntimeProtocolHostModule = RuntimeProtocolHostModule;
    register_protocol_modules(
        vm,
        context.clone(),
        async_ops.clone(),
        &[&HTTP_PROTOCOL_MODULE, &RUNTIME_PROTOCOL_MODULE],
    )?;
    http::register_http_extensions(vm, context.clone(), async_ops.clone());
    io::register_builtin_io_overrides(vm, context, async_ops)
}

pub fn register_runtime_host_module(
    vm: &mut Vm,
    async_ops: SharedVmAsyncOps,
) -> Result<(), VmError> {
    runtime::register_runtime_host_module(vm, async_ops)
}

pub fn register_http_host_module(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) -> Result<(), VmError> {
    http::register_http_host_module(vm, context, async_ops)
}

fn schedule_future_call<F>(
    vm: &mut Vm,
    async_ops: &SharedVmAsyncOps,
    future: F,
) -> Result<CallOutcome, VmError>
where
    F: Future<Output = AsyncOpResult> + Send + 'static,
{
    let mut ops = async_ops.lock().expect("vm async ops lock poisoned");
    let op_id = ops.schedule_future(vm, future)?;
    Ok(CallOutcome::Pending(op_id))
}

fn schedule_ready_call(
    vm: &mut Vm,
    async_ops: &SharedVmAsyncOps,
    values: Vec<Value>,
) -> Result<CallOutcome, VmError> {
    let mut ops = async_ops.lock().expect("vm async ops lock poisoned");
    let op_id = ops.schedule_ready(vm, Ok(values))?;
    Ok(CallOutcome::Pending(op_id))
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
