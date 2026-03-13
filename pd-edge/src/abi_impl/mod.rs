use std::{
    cell::RefCell,
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use edge_abi::FUNCTIONS as EDGE_ABI_FUNCTIONS;
use tokio::sync::oneshot;
use vm::{CallOutcome, HostAsyncBridge, HostFunction, HostOpId, Value, Vm, VmError};

pub(crate) mod http;
mod http1;
mod http2;
mod io;
mod proxy;
mod registry;
mod runtime;
mod transport;
#[cfg(feature = "webrtc")]
mod webrtc;
mod websocket;

pub use self::http::{HttpRequestContext, ProxyVmContext, SharedProxyVmContext};
#[cfg(test)]
#[cfg(feature = "http2")]
pub(crate) use self::http2::Http2SessionFrontier;
pub(crate) use self::http2::{
    DownstreamHttp2ConnectionTracker, Http2DownstreamStreamAttachment,
    SharedHttpDownstreamSessions, SharedHttpUpstreamSessions, new_shared_http_downstream_sessions,
    new_shared_http_upstream_sessions,
};
pub(crate) use self::transport::{SharedTlsSessionCache, new_shared_tls_session_cache};

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
    next_op_id: HostOpId,
    runtime_handle: Option<tokio::runtime::Handle>,
}

impl VmAsyncOps {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            next_op_id: 1,
            runtime_handle: tokio::runtime::Handle::try_current().ok(),
        }
    }

    pub fn with_runtime_handle(runtime_handle: tokio::runtime::Handle) -> Self {
        Self {
            pending: HashMap::new(),
            next_op_id: 1,
            runtime_handle: Some(runtime_handle),
        }
    }

    fn allocate_op_id(&mut self) -> Result<HostOpId, VmError> {
        for _ in 0..u16::MAX {
            let op_id = self.next_op_id;
            self.next_op_id = self.next_op_id.wrapping_add(1).max(1);
            if !self.pending.contains_key(&op_id) {
                return Ok(op_id);
            }
        }
        Err(VmError::HostError(
            "exhausted edge async host op ids".to_string(),
        ))
    }

    pub fn schedule_ready(&mut self, result: AsyncOpResult) -> Result<HostOpId, VmError> {
        let op_id = self.allocate_op_id()?;
        let (sender, receiver) = oneshot::channel();
        self.insert_pending(op_id, PendingOp::Receiver(receiver))?;
        sender
            .send(result)
            .map_err(|_| VmError::HostError(format!("failed to complete host op {op_id}")))?;
        Ok(op_id)
    }

    pub fn schedule_future<F>(&mut self, future: F) -> Result<HostOpId, VmError>
    where
        F: Future<Output = AsyncOpResult> + Send + 'static,
    {
        let op_id = self.allocate_op_id()?;
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
            CallOutcome::Return(values) => schedule_ready_call(&self.async_ops, values),
            CallOutcome::Halt => Ok(CallOutcome::Halt),
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

    fn scope_mask(&self) -> Option<u8> {
        None
    }
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

    fn scope_mask(&self) -> Option<u8> {
        Some(1 << 0)
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

    fn scope_mask(&self) -> Option<u8> {
        Some(1 << 1)
    }
}

pub(crate) fn unbound_edge_abi_function(
    _vm: &mut Vm,
    _args: &[Value],
) -> Result<CallOutcome, VmError> {
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
    let mut scope_mask_bits = 0u8;
    for module in modules {
        let Some(scope_mask) = module.scope_mask() else {
            ensure_edge_abi_host_slots(vm)?;
            for module in modules {
                module.register(vm, context.clone(), async_ops.clone())?;
            }
            return Ok(());
        };
        scope_mask_bits |= scope_mask;
    }
    if scope_mask_bits != 0 && vm.bound_function_count() == 0 {
        let mut scopes = Vec::new();
        if scope_mask_bits & (1 << 0) != 0 {
            scopes.push(registry::EdgeHostScope::Runtime);
        }
        if scope_mask_bits & (1 << 1) != 0 {
            scopes.push(registry::EdgeHostScope::Http);
        }
        if scope_mask_bits & (1 << 2) != 0 {
            scopes.push(registry::EdgeHostScope::HttpExtension);
        }
        if scope_mask_bits & (1 << 3) != 0 {
            scopes.push(registry::EdgeHostScope::Io);
        }
        if scope_mask_bits & (1 << 4) != 0 {
            scopes.push(registry::EdgeHostScope::Transport);
        }
        return registry::bind_host_scopes(vm, &scopes);
    }
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

#[derive(Clone, Debug)]
pub(crate) enum EdgeVirtualIoHandle {
    BufferedRead { text: String, offset: usize },
    FileWrite { path: String, append: bool },
}

const EDGE_IO_HANDLE_DYNAMIC_BASE: i64 = 1_i64 << 48;

pub fn register_host_module(
    vm: &mut Vm,
    _context: SharedProxyVmContext,
    _async_ops: SharedVmAsyncOps,
) -> Result<(), VmError> {
    registry::bind_host_scopes(vm, &[registry::EdgeHostScope::Runtime])
}

pub fn register_http_plane_host_module(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) -> Result<(), VmError> {
    if vm.bound_function_count() == 0 {
        return registry::bind_host_scopes(
            vm,
            &[
                registry::EdgeHostScope::Http,
                registry::EdgeHostScope::Runtime,
                registry::EdgeHostScope::HttpExtension,
                registry::EdgeHostScope::Io,
                registry::EdgeHostScope::Transport,
                registry::EdgeHostScope::WebSocket,
                #[cfg(feature = "webrtc")]
                registry::EdgeHostScope::WebRtc,
                registry::EdgeHostScope::Proxy,
            ],
        );
    }
    static HTTP_PROTOCOL_MODULE: HttpProtocolHostModule = HttpProtocolHostModule;
    static RUNTIME_PROTOCOL_MODULE: RuntimeProtocolHostModule = RuntimeProtocolHostModule;
    register_protocol_modules(
        vm,
        context.clone(),
        async_ops.clone(),
        &[&HTTP_PROTOCOL_MODULE, &RUNTIME_PROTOCOL_MODULE],
    )?;
    http::register_http_extensions(vm, context.clone(), async_ops.clone());
    transport::register_transport_extensions(vm, context.clone(), async_ops.clone());
    websocket::register_websocket_extensions(vm, context.clone(), async_ops.clone());
    #[cfg(feature = "webrtc")]
    webrtc::register_webrtc_extensions(vm, context.clone(), async_ops.clone());
    proxy::register_proxy_extensions(vm, context.clone(), async_ops.clone());
    io::register_builtin_io_overrides(vm, context, async_ops)
}

pub fn register_runtime_host_module(
    vm: &mut Vm,
    _context: SharedProxyVmContext,
    _async_ops: SharedVmAsyncOps,
) -> Result<(), VmError> {
    registry::bind_host_scopes(vm, &[registry::EdgeHostScope::Runtime])
}

pub fn register_http_host_module(
    vm: &mut Vm,
    _context: SharedProxyVmContext,
    _async_ops: SharedVmAsyncOps,
) -> Result<(), VmError> {
    registry::bind_host_scopes(vm, &[registry::EdgeHostScope::Http])
}

fn schedule_future_call<F>(
    _vm: &mut Vm,
    async_ops: &SharedVmAsyncOps,
    future: F,
) -> Result<CallOutcome, VmError>
where
    F: Future<Output = AsyncOpResult> + Send + 'static,
{
    let mut ops = async_ops.lock().expect("vm async ops lock poisoned");
    let op_id = ops.schedule_future(future)?;
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

pub(crate) fn schedule_current_ready_call(
    _vm: &mut Vm,
    values: Vec<Value>,
) -> Result<CallOutcome, VmError> {
    let async_ops = current_async_ops()?;
    schedule_ready_call(&async_ops, values)
}

fn schedule_ready_call(
    async_ops: &SharedVmAsyncOps,
    values: Vec<Value>,
) -> Result<CallOutcome, VmError> {
    let mut ops = async_ops.lock().expect("vm async ops lock poisoned");
    let op_id = ops.schedule_ready(Ok(values))?;
    Ok(CallOutcome::Pending(op_id))
}

pub(crate) fn adapt_edge_call_outcome(
    vm: &mut Vm,
    outcome: CallOutcome,
) -> Result<CallOutcome, VmError> {
    match outcome {
        CallOutcome::Return(values) => schedule_current_ready_call(vm, values),
        CallOutcome::Halt => Ok(CallOutcome::Halt),
        CallOutcome::Yield => Ok(CallOutcome::Yield),
        CallOutcome::Pending(op_id) => Ok(CallOutcome::Pending(op_id)),
    }
}

#[allow(dead_code)]
pub(crate) fn adapt_edge_args_call_outcome(outcome: CallOutcome) -> Result<CallOutcome, VmError> {
    match outcome {
        CallOutcome::Return(values) => {
            let async_ops = current_async_ops()?;
            schedule_ready_call(&async_ops, values)
        }
        CallOutcome::Halt => Ok(CallOutcome::Halt),
        CallOutcome::Yield => Ok(CallOutcome::Yield),
        CallOutcome::Pending(op_id) => Ok(CallOutcome::Pending(op_id)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    #[cfg(feature = "http")]
    use std::task::{Wake, Waker};

    use axum::http::HeaderMap;
    #[cfg(feature = "http")]
    use edge_abi::symbols::http::{request as http_request, response as http_response};
    use edge_abi::symbols::runtime as edge_runtime;
    use edge_abi::symbols::tcp;
    #[cfg(feature = "http")]
    use pd_edge_host_function::pd_edge_host_function;
    #[cfg(feature = "http")]
    use vm::{BytecodeBuilder, CallOutcome, VmError, VmStatus};
    use vm::{HostImport, OpCode, Program, ValueType, Vm};

    use super::registry::PD_EDGE_HOST_FUNCTIONS;
    use super::{
        ProxyVmContext, RateLimiterStore, SharedProxyVmContext, new_shared_vm_async_ops,
        register_host_module,
    };
    #[cfg(feature = "http")]
    use super::{
        VmAsyncOpBridge, current_vm_context, enter_edge_host_context,
        register_http_plane_host_module,
    };
    #[cfg(feature = "http")]
    use std::task::{Context, Poll};

    #[cfg(feature = "http")]
    struct TestNoopWake;

    #[cfg(feature = "http")]
    impl Wake for TestNoopWake {
        fn wake(self: Arc<Self>) {}
    }

    #[cfg(feature = "http")]
    fn test_waker() -> Waker {
        Waker::from(Arc::new(TestNoopWake))
    }

    #[cfg(feature = "http")]
    /// Yields a pending TLS test operation.
    #[pd_edge_host_function(name = "test::yield_pending_tls", scope = http_extension)]
    async fn yield_pending_tls(_vm: &mut Vm) -> Result<CallOutcome, VmError> {
        tokio::task::yield_now().await;
        Ok(CallOutcome::Return(vec![]))
    }

    fn test_context() -> SharedProxyVmContext {
        Arc::new(ProxyVmContext::from_request_headers(
            HeaderMap::new(),
            Arc::new(Mutex::new(RateLimiterStore::new())),
        ))
    }

    #[test]
    fn register_host_module_binds_cached_runtime_scope_plan() {
        let imports = vec![HostImport {
            name: edge_runtime::SLEEP.name.to_string(),
            arity: 1,
            return_type: ValueType::Unknown,
        }];
        let program =
            Program::with_imports_and_debug(vec![], vec![OpCode::Ret as u8], imports, None);

        let mut first = Vm::new(program.clone());
        register_host_module(&mut first, test_context(), new_shared_vm_async_ops())
            .expect("first runtime vm should bind");
        assert_eq!(first.bound_function_count(), 1);

        let mut second = Vm::new(program);
        register_host_module(&mut second, test_context(), new_shared_vm_async_ops())
            .expect("second runtime vm should reuse cached plan");
        assert_eq!(second.bound_function_count(), 1);
    }

    #[test]
    fn edge_registration_docs_are_available() {
        let entry = PD_EDGE_HOST_FUNCTIONS
            .iter()
            .find(|entry| entry.name == edge_runtime::SLEEP.name)
            .expect("runtime::sleep registration should exist");
        assert!(
            !entry.docs.trim().is_empty(),
            "expected runtime::sleep edge registration docs to be populated"
        );
    }

    #[test]
    fn edge_registration_uses_function_doc_comments() {
        let entry = PD_EDGE_HOST_FUNCTIONS
            .iter()
            .find(|entry| entry.name == tcp::stream::GET_PHASE.name)
            .expect("tcp::stream::get_phase registration should exist");
        assert_eq!(
            entry.docs,
            "Reports the current lifecycle phase for a TCP stream handle."
        );
    }

    #[cfg(feature = "http")]
    #[test]
    fn register_http_plane_host_module_binds_cached_multi_scope_plan() {
        let imports = vec![
            HostImport {
                name: http_request::GET_METHOD.name.to_string(),
                arity: 0,
                return_type: ValueType::Unknown,
            },
            HostImport {
                name: "http::request::body::next_chunk".to_string(),
                arity: 1,
                return_type: ValueType::Unknown,
            },
            HostImport {
                name: "io::exists".to_string(),
                arity: 1,
                return_type: ValueType::Unknown,
            },
            HostImport {
                name: edge_runtime::SLEEP.name.to_string(),
                arity: 1,
                return_type: ValueType::Unknown,
            },
        ];
        let program =
            Program::with_imports_and_debug(vec![], vec![OpCode::Ret as u8], imports, None);
        let mut vm = Vm::new(program);
        register_http_plane_host_module(&mut vm, test_context(), new_shared_vm_async_ops())
            .expect("http plane vm should bind all cached scopes");

        assert_eq!(vm.bound_function_count(), 4);
    }

    #[cfg(feature = "http")]
    #[test]
    fn http_response_set_body_scoped_binding_runs_under_edge_context() {
        let imports = vec![HostImport {
            name: http_response::SET_BODY.name.to_string(),
            arity: 1,
            return_type: ValueType::Unknown,
        }];
        let mut bc = BytecodeBuilder::new();
        bc.ldc(0);
        bc.call(0, 1);
        bc.ret();
        let program = Program::with_imports_and_debug(
            vec![vm::Value::string("payload")],
            bc.finish(),
            imports,
            None,
        );
        let context = test_context();
        let async_ops = new_shared_vm_async_ops();
        let mut vm = Vm::new(program);
        vm.set_async_bridge(Box::new(VmAsyncOpBridge::new(async_ops.clone())));
        register_http_plane_host_module(&mut vm, context.clone(), async_ops.clone())
            .expect("http plane vm should bind");

        let status = {
            let _host_context = enter_edge_host_context(context.clone(), async_ops.clone());
            vm.run().expect("edge host call should execute")
        };
        if status == VmStatus::Waiting(vm.waiting_host_op_id().expect("waiting op id should exist"))
        {
            vm.wait_for_host_op_blocking()
                .expect("ready edge host op should complete");
            let _host_context = enter_edge_host_context(context.clone(), async_ops.clone());
            assert_eq!(vm.resume().expect("vm should resume"), VmStatus::Halted);
        } else {
            assert_eq!(status, VmStatus::Halted);
        }

        let guard = context.lock_downstream();
        assert_eq!(
            guard.response_output.body.as_deref(),
            Some("payload".as_bytes())
        );
    }

    #[cfg(feature = "http")]
    #[tokio::test(flavor = "current_thread")]
    async fn async_host_poll_does_not_leave_tls_context_installed() {
        let imports = vec![HostImport {
            name: "test::yield_pending_tls".to_string(),
            arity: 0,
            return_type: ValueType::Unknown,
        }];
        let mut bc = BytecodeBuilder::new();
        bc.call(0, 0);
        bc.ret();
        let program = Program::with_imports_and_debug(vec![], bc.finish(), imports, None);
        let context = test_context();
        let async_ops = new_shared_vm_async_ops();
        let mut vm = Vm::new(program);
        vm.set_async_bridge(Box::new(VmAsyncOpBridge::new(async_ops.clone())));
        register_http_plane_host_module(&mut vm, context.clone(), async_ops.clone())
            .expect("http plane vm should bind");

        let status = {
            let _host_context = enter_edge_host_context(context.clone(), async_ops.clone());
            vm.run().expect("async host call should start")
        };
        assert_eq!(
            status,
            VmStatus::Waiting(vm.waiting_host_op_id().expect("waiting op id should exist"))
        );

        let waker = test_waker();
        let mut poll_context = Context::from_waker(&waker);
        assert!(matches!(
            vm.poll_waiting_host_op(&mut poll_context),
            Poll::Pending
        ));
        assert!(
            current_vm_context().is_err(),
            "async host poll must not leak TLS context between polls"
        );

        vm.await_waiting_host_op()
            .await
            .expect("host op should complete on second poll");
        let _host_context = enter_edge_host_context(context.clone(), async_ops.clone());
        assert_eq!(vm.resume().expect("vm should halt"), VmStatus::Halted);
    }
}
