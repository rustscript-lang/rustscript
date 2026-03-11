use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use axum::{
    body::Body,
    http::{HeaderMap, HeaderName, HeaderValue, Method},
};
use edge_abi::FUNCTIONS as EDGE_ABI_FUNCTIONS;
use http_body_util::BodyExt;
use tokio::sync::oneshot;
use url::Url;
use vm::{
    CallOutcome, HostAsyncBridge, HostFunction, HostOpId, Value, Vm, VmError, bytecode::VmMap,
};

mod http;
mod io;
mod registry;
mod runtime;

pub type SharedRateLimiter = Arc<Mutex<RateLimiterStore>>;
pub type SharedVmAsyncOps = Arc<Mutex<VmAsyncOps>>;

type AsyncOpResult = Result<Vec<Value>, VmError>;
type PendingFuture = Pin<Box<dyn Future<Output = AsyncOpResult> + Send + 'static>>;
type HostCallResult = Result<CallOutcome, VmError>;
type HostCallHandler = dyn FnMut(&mut Vm, &[Value]) -> HostCallResult + Send + 'static;

#[derive(Clone)]
struct ActiveEdgeHostContext {
    vm_context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
}

std::thread_local! {
    static CURRENT_EDGE_HOST_CONTEXT: RefCell<Option<ActiveEdgeHostContext>> = const { RefCell::new(None) };
}

pub struct EdgeHostContextGuard {
    previous: Option<ActiveEdgeHostContext>,
}

impl Drop for EdgeHostContextGuard {
    fn drop(&mut self) {
        CURRENT_EDGE_HOST_CONTEXT.with(|slot| {
            *slot.borrow_mut() = self.previous.take();
        });
    }
}

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

pub fn enter_edge_host_context(
    vm_context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) -> EdgeHostContextGuard {
    let next = ActiveEdgeHostContext {
        vm_context,
        async_ops,
    };
    let previous = CURRENT_EDGE_HOST_CONTEXT.with(|slot| slot.borrow_mut().replace(next));
    EdgeHostContextGuard { previous }
}

pub(crate) fn current_vm_context() -> Result<SharedProxyVmContext, VmError> {
    CURRENT_EDGE_HOST_CONTEXT.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|context| context.vm_context.clone())
            .ok_or_else(|| {
                VmError::HostError(
                    "pd-edge host context is unavailable outside Store-backed execution"
                        .to_string(),
                )
            })
    })
}

pub(crate) fn current_async_ops() -> Result<SharedVmAsyncOps, VmError> {
    CURRENT_EDGE_HOST_CONTEXT.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|context| context.async_ops.clone())
            .ok_or_else(|| {
                VmError::HostError(
                    "pd-edge async ops are unavailable outside Store-backed execution".to_string(),
                )
            })
    })
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
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> HostCallResult {
        match self.inner.call(vm, args)? {
            CallOutcome::Return(values) => schedule_ready_call(vm, &self.async_ops, values),
            CallOutcome::Yield => Ok(CallOutcome::Yield),
            CallOutcome::Pending(op_id) => Ok(CallOutcome::Pending(op_id)),
        }
    }
}

struct ClosureHostFunction {
    handler: Box<HostCallHandler>,
}

impl ClosureHostFunction {
    fn new<F>(handler: F) -> Self
    where
        F: FnMut(&mut Vm, &[Value]) -> HostCallResult + Send + 'static,
    {
        Self {
            handler: Box::new(handler),
        }
    }
}

impl HostFunction for ClosureHostFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> HostCallResult {
        (self.handler)(vm, args)
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

pub(super) fn bind_async_host_handler<F>(
    vm: &mut Vm,
    async_ops: &SharedVmAsyncOps,
    name: impl Into<String>,
    handler: F,
) where
    F: FnMut(&mut Vm, &[Value]) -> HostCallResult + Send + 'static,
{
    bind_async_host(
        vm,
        async_ops,
        name,
        Box::new(ClosureHostFunction::new(handler)),
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
        context: SharedProxyVmContext,
        async_ops: SharedVmAsyncOps,
    ) -> Result<(), VmError> {
        register_runtime_host_module(vm, context, async_ops)
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

fn unbound_edge_abi_function(_vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, VmError> {
    Err(VmError::HostError(
        "edge ABI host function is not bound".to_string(),
    ))
}

fn ensure_edge_abi_host_slots(vm: &mut Vm) -> Result<(), VmError> {
    if EDGE_ABI_FUNCTIONS
        .iter()
        .all(|function| vm.has_bound_function(function.name))
    {
        return Ok(());
    }

    if vm.bound_function_count() != 0 {
        return Err(VmError::HostError(
            "edge ABI host slots must be initialized before registering custom host functions"
                .to_string(),
        ));
    }

    for function in EDGE_ABI_FUNCTIONS {
        vm.bind_static_function(function.name, unbound_edge_abi_function);
    }
    Ok(())
}

pub fn register_protocol_modules(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
    modules: &[&dyn EdgeProtocolHostModule],
) -> Result<(), VmError> {
    ensure_edge_abi_host_slots(vm)?;
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

#[derive(Debug)]
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
    pub body: Body,
    pub headers: HeaderMap,
}

#[derive(Clone, Debug)]
struct InboundRequestLineSource {
    request_id: String,
    method: Method,
    path: String,
    query: String,
    http_version: String,
    port: u16,
    scheme: String,
    host: String,
    client_ip: String,
}

struct InboundRequestBodyState {
    body: Option<Body>,
    buffered: Vec<u8>,
    read_offset: usize,
    eof: bool,
}

impl std::fmt::Debug for InboundRequestBodyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboundRequestBodyState")
            .field("buffered_len", &self.buffered.len())
            .field("read_offset", &self.read_offset)
            .field("eof", &self.eof)
            .finish()
    }
}

impl InboundRequestBodyState {
    fn new(body: Body) -> Self {
        Self {
            body: Some(body),
            buffered: Vec::new(),
            read_offset: 0,
            eof: false,
        }
    }

    async fn pull_next_frame(&mut self) -> Result<(), VmError> {
        if self.eof {
            return Ok(());
        }
        let Some(body) = self.body.as_mut() else {
            self.eof = true;
            return Ok(());
        };

        match body.frame().await {
            Some(Ok(frame)) => {
                if let Ok(chunk) = frame.into_data()
                    && !chunk.is_empty()
                {
                    self.buffered.extend_from_slice(&chunk);
                }
            }
            Some(Err(err)) => {
                return Err(VmError::HostError(format!(
                    "failed to read inbound request body frame: {err}",
                )));
            }
            None => {
                self.eof = true;
                self.body = None;
            }
        }
        Ok(())
    }

    async fn ensure_readable_byte(&mut self) -> Result<(), VmError> {
        while self.read_offset >= self.buffered.len() && !self.eof {
            self.pull_next_frame().await?;
        }
        Ok(())
    }

    async fn read_next_chunk(&mut self, max_bytes: usize) -> Result<Vec<u8>, VmError> {
        self.ensure_readable_byte().await?;
        if self.read_offset >= self.buffered.len() {
            return Ok(Vec::new());
        }
        let end = self
            .read_offset
            .saturating_add(max_bytes)
            .min(self.buffered.len());
        let chunk = self.buffered[self.read_offset..end].to_vec();
        self.read_offset = end;
        Ok(chunk)
    }

    async fn read_next_line(&mut self) -> Result<Vec<u8>, VmError> {
        loop {
            let start = self.read_offset.min(self.buffered.len());
            if start < self.buffered.len() {
                if let Some(rel_end) = self.buffered[start..]
                    .iter()
                    .position(|byte| *byte == b'\n')
                {
                    let end = start + rel_end;
                    let line = self.buffered[start..end].to_vec();
                    self.read_offset = end.saturating_add(1);
                    return Ok(line);
                }
                if self.eof {
                    let line = self.buffered[start..].to_vec();
                    self.read_offset = self.buffered.len();
                    return Ok(line);
                }
            } else if self.eof {
                return Ok(Vec::new());
            }

            self.pull_next_frame().await?;
        }
    }

    async fn read_all_and_consume(&mut self) -> Result<Vec<u8>, VmError> {
        let body = self.read_all().await?;
        self.read_offset = self.buffered.len();
        Ok(body)
    }

    async fn read_all(&mut self) -> Result<Vec<u8>, VmError> {
        while !self.eof {
            self.pull_next_frame().await?;
        }
        Ok(self.buffered.clone())
    }

    async fn eof(&mut self) -> Result<bool, VmError> {
        while self.read_offset >= self.buffered.len() && !self.eof {
            self.pull_next_frame().await?;
        }
        Ok(self.eof && self.read_offset >= self.buffered.len())
    }
}

type SharedInboundRequestBody = Arc<tokio::sync::Mutex<InboundRequestBodyState>>;

#[derive(Clone, Debug)]
enum EdgeVirtualIoHandle {
    BufferedRead { text: String, offset: usize },
    FileWrite { path: String, append: bool },
}

const EDGE_IO_HANDLE_DYNAMIC_BASE: i64 = 1024;

const ACCESS_REQUEST_LINE: u8 = 1 << 0;
const ACCESS_REQUEST_HEADERS: u8 = 1 << 1;
const ACCESS_REQUEST_BODY: u8 = 1 << 2;
const ACCESS_UPSTREAM_REQUEST: u8 = 1 << 3;
const ACCESS_RESPONSE_OUTPUT: u8 = 1 << 4;
const ACCESS_UPSTREAM_RESPONSE: u8 = 1 << 5;

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
    inbound_request_line_source: Option<InboundRequestLineSource>,
    inbound_request_headers_source: Option<HeaderMap>,
    inbound_request_body: SharedInboundRequestBody,
    inbound_request_headers: HeaderMap,
    outbound_request_initialized: bool,
    outbound_request_method: Method,
    outbound_request_path: String,
    outbound_request_query: String,
    outbound_request_body: Vec<u8>,
    outbound_request_body_overridden: bool,
    outbound_request_headers: HeaderMap,
    response_headers: HeaderMap,
    response_content: Option<String>,
    response_status: Option<u16>,
    upstream: Option<String>,
    upstream_response_headers: HeaderMap,
    upstream_response_content: Option<String>,
    upstream_response_status: Option<u16>,
    http_access_bits: u8,
    rate_limiter: SharedRateLimiter,
    edge_io_next_handle: i64,
    edge_io_handles: HashMap<i64, EdgeVirtualIoHandle>,
}

impl ProxyVmContext {
    pub fn from_http_request(request: HttpRequestContext, rate_limiter: SharedRateLimiter) -> Self {
        let line_source = InboundRequestLineSource {
            request_id: request.request_id,
            method: request.method,
            path: request.path,
            query: request.query,
            http_version: request.http_version,
            port: request.port,
            scheme: request.scheme,
            host: request.host,
            client_ip: request.client_ip,
        };
        Self {
            inbound_request_id: String::new(),
            inbound_request_method: Method::GET,
            inbound_request_path: String::new(),
            inbound_request_query: String::new(),
            inbound_request_http_version: String::new(),
            inbound_request_port: 0,
            inbound_request_scheme: String::new(),
            inbound_request_host: String::new(),
            inbound_request_client_ip: String::new(),
            inbound_request_line_source: Some(line_source),
            inbound_request_headers_source: Some(request.headers),
            inbound_request_body: Arc::new(tokio::sync::Mutex::new(InboundRequestBodyState::new(
                request.body,
            ))),
            inbound_request_headers: HeaderMap::new(),
            outbound_request_initialized: false,
            outbound_request_method: Method::GET,
            outbound_request_path: String::new(),
            outbound_request_query: String::new(),
            outbound_request_body: Vec::new(),
            outbound_request_body_overridden: false,
            outbound_request_headers: HeaderMap::new(),
            response_headers: HeaderMap::new(),
            response_content: None,
            response_status: None,
            upstream: None,
            upstream_response_headers: HeaderMap::new(),
            upstream_response_content: None,
            upstream_response_status: None,
            http_access_bits: 0,
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
                body: Body::empty(),
                headers: request_headers,
            },
            rate_limiter,
        )
    }

    fn ensure_request_line_loaded(&mut self) {
        if let Some(source) = self.inbound_request_line_source.take() {
            self.inbound_request_id = source.request_id;
            self.inbound_request_method = source.method;
            self.inbound_request_path = source.path;
            self.inbound_request_query = source.query;
            self.inbound_request_http_version = source.http_version;
            self.inbound_request_port = source.port;
            self.inbound_request_scheme = source.scheme;
            self.inbound_request_host = source.host;
            self.inbound_request_client_ip = source.client_ip;
        }
    }

    fn ensure_request_headers_loaded(&mut self) {
        self.ensure_request_line_loaded();
        if let Some(headers) = self.inbound_request_headers_source.take() {
            self.inbound_request_headers = headers;
        }
    }

    fn ensure_outbound_request_initialized(&mut self) {
        if self.outbound_request_initialized {
            return;
        }
        self.ensure_request_headers_loaded();
        self.outbound_request_method = self.inbound_request_method.clone();
        self.outbound_request_path = self.inbound_request_path.clone();
        self.outbound_request_query = self.inbound_request_query.clone();
        self.outbound_request_headers = self.inbound_request_headers.clone();
        self.outbound_request_initialized = true;
    }

    fn touch_request_line(&mut self) {
        self.ensure_request_line_loaded();
        self.mark_http_access(ACCESS_REQUEST_LINE);
    }

    fn touch_request_headers(&mut self) {
        self.ensure_request_headers_loaded();
        self.mark_http_access(ACCESS_REQUEST_LINE | ACCESS_REQUEST_HEADERS);
    }

    fn touch_request_body(&mut self) {
        self.ensure_request_headers_loaded();
        self.mark_http_access(ACCESS_REQUEST_LINE | ACCESS_REQUEST_HEADERS | ACCESS_REQUEST_BODY);
    }

    fn touch_upstream_request(&mut self) {
        self.ensure_outbound_request_initialized();
        self.mark_http_access(
            ACCESS_REQUEST_LINE
                | ACCESS_REQUEST_HEADERS
                | ACCESS_REQUEST_BODY
                | ACCESS_UPSTREAM_REQUEST,
        );
    }

    fn touch_response_output(&mut self) {
        self.mark_http_access(
            ACCESS_REQUEST_LINE
                | ACCESS_REQUEST_HEADERS
                | ACCESS_REQUEST_BODY
                | ACCESS_RESPONSE_OUTPUT,
        );
    }

    fn touch_upstream_response(&mut self) {
        self.mark_http_access(
            ACCESS_REQUEST_LINE
                | ACCESS_REQUEST_HEADERS
                | ACCESS_REQUEST_BODY
                | ACCESS_UPSTREAM_REQUEST
                | ACCESS_UPSTREAM_RESPONSE,
        );
    }

    fn mark_http_access(&mut self, bits: u8) {
        self.http_access_bits |= bits;
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
}

pub fn snapshot_execution_outcome(context: &SharedProxyVmContext) -> VmExecutionOutcome {
    let mut context = context.lock().expect("vm context lock poisoned");
    context.ensure_outbound_request_initialized();
    VmExecutionOutcome {
        response_headers: context.response_headers.clone(),
        response_content: context.response_content.clone(),
        response_status: context.response_status,
        upstream: context.upstream.clone(),
        request_headers: context.outbound_request_headers.clone(),
        request_method: context.outbound_request_method.clone(),
        request_path: context.outbound_request_path.clone(),
        request_query: context.outbound_request_query.clone(),
    }
}

pub async fn resolve_outbound_request_body(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let (overridden, explicit_body, inbound_body) = {
        let mut guard = context.lock().expect("vm context lock poisoned");
        guard.touch_upstream_request();
        (
            guard.outbound_request_body_overridden,
            guard.outbound_request_body.clone(),
            guard.inbound_request_body.clone(),
        )
    };

    if overridden {
        return Ok(explicit_body);
    }

    let mut inbound = inbound_body.lock().await;
    inbound.read_all().await
}

pub fn register_host_module(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) -> Result<(), VmError> {
    static RUNTIME_PROTOCOL_MODULE: RuntimeProtocolHostModule = RuntimeProtocolHostModule;
    register_protocol_modules(vm, context, async_ops, &[&RUNTIME_PROTOCOL_MODULE])
}

pub fn register_http_plane_host_module(
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
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) -> Result<(), VmError> {
    ensure_edge_abi_host_slots(vm)?;
    runtime::register_runtime_host_module(vm, context, async_ops)
}

pub fn register_http_host_module(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) -> Result<(), VmError> {
    ensure_edge_abi_host_slots(vm)?;
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

pub(crate) fn schedule_current_future_call<F>(
    vm: &mut Vm,
    future: F,
) -> Result<CallOutcome, VmError>
where
    F: Future<Output = AsyncOpResult> + Send + 'static,
{
    let async_ops = current_async_ops()?;
    schedule_future_call(vm, &async_ops, future)
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

async fn read_request_body_all(context: &SharedProxyVmContext) -> Result<Vec<u8>, VmError> {
    let body = {
        let mut guard = context.lock().expect("vm context lock poisoned");
        guard.touch_request_body();
        guard.inbound_request_body.clone()
    };
    let mut inbound = body.lock().await;
    inbound.read_all().await
}

async fn consume_request_body_all(context: &SharedProxyVmContext) -> Result<Vec<u8>, VmError> {
    let body = {
        let mut guard = context.lock().expect("vm context lock poisoned");
        guard.touch_request_body();
        guard.inbound_request_body.clone()
    };
    let mut inbound = body.lock().await;
    inbound.read_all_and_consume().await
}

async fn read_request_body_next_chunk(
    context: &SharedProxyVmContext,
    max_bytes: usize,
) -> Result<Vec<u8>, VmError> {
    let body = {
        let mut guard = context.lock().expect("vm context lock poisoned");
        guard.touch_request_body();
        guard.inbound_request_body.clone()
    };
    let mut inbound = body.lock().await;
    inbound.read_next_chunk(max_bytes).await
}

async fn read_request_body_next_line(context: &SharedProxyVmContext) -> Result<Vec<u8>, VmError> {
    let body = {
        let mut guard = context.lock().expect("vm context lock poisoned");
        guard.touch_request_body();
        guard.inbound_request_body.clone()
    };
    let mut inbound = body.lock().await;
    inbound.read_next_line().await
}

async fn request_body_eof(context: &SharedProxyVmContext) -> Result<bool, VmError> {
    let body = {
        let mut guard = context.lock().expect("vm context lock poisoned");
        guard.touch_request_body();
        guard.inbound_request_body.clone()
    };
    let mut inbound = body.lock().await;
    inbound.eof().await
}

fn parse_header_name(name: String) -> Result<HeaderName, VmError> {
    HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))
}

fn parse_header(name: String, value: String) -> Result<(HeaderName, HeaderValue), VmError> {
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;
    let header_value = HeaderValue::from_str(&value)
        .map_err(|_| VmError::HostError(format!("invalid header value '{value}'")))?;
    Ok((header_name, header_value))
}

fn parse_headers_map(entries: VmMap) -> Result<Vec<(HeaderName, Vec<HeaderValue>)>, VmError> {
    let mut parsed = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        let name = match key {
            Value::String(name) => name.to_string(),
            _ => {
                return Err(VmError::HostError(
                    "header map keys must be strings".to_string(),
                ));
            }
        };
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;

        let values = match value {
            Value::String(single) => vec![single.to_string()],
            Value::Array(values) => {
                let mut collected = Vec::with_capacity(values.len());
                for value in values.iter() {
                    match value {
                        Value::String(item) => collected.push(item.to_string()),
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
    Value::map(
        values
            .into_iter()
            .map(|(name, values)| {
                let value = if values.len() == 1 {
                    Value::string(values[0].clone())
                } else {
                    Value::array(values.into_iter().map(Value::string).collect())
                };
                (Value::string(name), value)
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
    Value::map(
        values
            .into_iter()
            .map(|(name, values)| {
                let value = if values.len() == 1 {
                    Value::string(values[0].clone())
                } else {
                    Value::array(values.into_iter().map(Value::string).collect())
                };
                (Value::string(name), value)
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
