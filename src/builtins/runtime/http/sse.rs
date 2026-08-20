use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use pd_host_function::pd_host_function;

use super::request::{
    HttpRequest, HttpResponseResource, OwnedResponse, ResponseReadObserver, open_stream_response,
    parse_request, response_header_entries,
};
use super::{HttpRequestContext, policy};
use crate::builtins::runtime::{HostCallResult, VmCallable, VmMap, VmMapHandle};
use crate::vm::operation::{
    HostOperation, OperationCancelReason, OperationError, OperationErrorCode, OperationResult,
    OperationSpec,
};
use crate::vm::resource::{
    CloseProgress, HostResource, ResourceCloseReason, ResourceError, ResourceErrorCode,
    ResourceHandle, ResourceResult, ResourceTypeKey,
};
use crate::vm::{
    CallOutcome, CallReturn, HostStreamAction, HostStreamDriver, HostStreamPoll, Value, Vm,
    VmError, VmResult,
};

#[derive(Debug, PartialEq, Eq)]
struct SseEvent {
    event: Option<String>,
    data: String,
    id: Option<String>,
    retry_ms: Option<i64>,
}

/// Incremental EventSource parser. `max_total_bytes` counts raw response-body
/// octets, including a BOM and line terminators. `max_item_bytes` counts the
/// UTF-8 bytes retained in data (including inserted joins), event, and id.
struct SseParser {
    max_line_bytes: usize,
    max_item_bytes: usize,
    max_total_bytes: usize,
    total_bytes: usize,
    prefix: Vec<u8>,
    bom_decided: bool,
    line: Vec<u8>,
    after_cr: bool,
    data: String,
    has_data: bool,
    event: Option<String>,
    id: Option<String>,
    retry_ms: Option<i64>,
    finished: bool,
}

impl SseParser {
    fn new(max_line_bytes: usize, max_item_bytes: usize, max_total_bytes: usize) -> Self {
        Self {
            max_line_bytes,
            max_item_bytes,
            max_total_bytes,
            total_bytes: 0,
            prefix: Vec::with_capacity(3),
            bom_decided: false,
            line: Vec::with_capacity(max_line_bytes.min(1024)),
            after_cr: false,
            data: String::new(),
            has_data: false,
            event: None,
            id: None,
            retry_ms: None,
            finished: false,
        }
    }

    #[cfg(test)]
    fn push(&mut self, bytes: &[u8]) -> VmResult<Vec<SseEvent>> {
        self.admit_chunk(bytes.len())?;
        let mut events = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let (consumed, event) = self.push_until_event(&bytes[offset..])?;
            offset += consumed;
            if let Some(event) = event {
                events.push(event);
            }
        }
        Ok(events)
    }

    fn admit_chunk(&mut self, bytes: usize) -> VmResult<()> {
        self.total_bytes = self
            .total_bytes
            .checked_add(bytes)
            .filter(|total| *total <= self.max_total_bytes)
            .ok_or_else(|| VmError::HostError("SSE stream exceeds total byte limit".to_string()))?;
        Ok(())
    }

    fn push_until_event(&mut self, bytes: &[u8]) -> VmResult<(usize, Option<SseEvent>)> {
        if self.finished {
            return Err(VmError::HostError(
                "SSE parser received bytes after EOF".to_string(),
            ));
        }
        let mut consumed = 0;
        while consumed < bytes.len() {
            let byte = bytes[consumed];
            consumed += 1;
            if !self.bom_decided {
                self.prefix.push(byte);
                if self.prefix == b"\xef\xbb\xbf" {
                    self.prefix.clear();
                    self.bom_decided = true;
                    continue;
                }
                if b"\xef\xbb\xbf".starts_with(&self.prefix) {
                    continue;
                }
                let prefix = std::mem::take(&mut self.prefix);
                self.bom_decided = true;
                for byte in prefix {
                    if let Some(event) = self.process_byte(byte)? {
                        return Ok((consumed, Some(event)));
                    }
                }
                continue;
            }
            if let Some(event) = self.process_byte(byte)? {
                return Ok((consumed, Some(event)));
            }
        }
        Ok((consumed, None))
    }

    fn finish(&mut self) -> VmResult<Vec<SseEvent>> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let mut events = Vec::new();
        if !self.prefix.is_empty() {
            let prefix = std::mem::take(&mut self.prefix);
            for byte in prefix {
                if let Some(event) = self.process_byte(byte)? {
                    events.push(event);
                }
            }
        }
        if !self.line.is_empty()
            && let Some(event) = self.process_line()?
        {
            events.push(event);
        }
        // EventSource dispatches only on a blank line. EOF discards a partial
        // event, including a final unterminated data line.
        self.data.clear();
        self.has_data = false;
        self.event = None;
        Ok(events)
    }

    fn process_byte(&mut self, byte: u8) -> VmResult<Option<SseEvent>> {
        if self.after_cr {
            self.after_cr = false;
            if byte == b'\n' {
                return Ok(None);
            }
        }
        match byte {
            b'\r' => {
                let event = self.process_line()?;
                self.after_cr = true;
                Ok(event)
            }
            b'\n' => self.process_line(),
            _ => {
                if self.line.len() == self.max_line_bytes {
                    return Err(VmError::HostError(
                        "SSE line exceeds byte limit".to_string(),
                    ));
                }
                self.line.push(byte);
                Ok(None)
            }
        }
    }

    fn process_line(&mut self) -> VmResult<Option<SseEvent>> {
        let bytes = std::mem::take(&mut self.line);
        let line = std::str::from_utf8(&bytes)
            .map_err(|_| VmError::HostError("SSE stream contains malformed UTF-8".to_string()))?;
        if line.is_empty() {
            if self.data_seen() {
                return Ok(Some(self.dispatch_event()));
            }
            // The WHATWG dispatch algorithm clears both data and event type
            // buffers even when empty data causes dispatch to return early.
            self.event = None;
            return Ok(None);
        }
        if line.starts_with(':') {
            return Ok(None);
        }
        let (field, mut value) = line.split_once(':').unwrap_or((line, ""));
        if let Some(rest) = value.strip_prefix(' ') {
            value = rest;
        }
        match field {
            "data" => {
                let added = value.len() + usize::from(self.has_data);
                self.ensure_item_growth(added, self.event.as_deref(), self.id.as_deref())?;
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "event" => {
                self.ensure_item_size(self.data.len(), Some(value), self.id.as_deref())?;
                self.event = Some(value.to_string());
            }
            "id" if !value.contains('\0') => {
                self.ensure_item_size(self.data.len(), self.event.as_deref(), Some(value))?;
                self.id = Some(value.to_string());
            }
            "retry" if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
                if let Ok(retry) = value.parse::<i64>() {
                    self.retry_ms = Some(retry);
                }
            }
            _ => {}
        }
        Ok(None)
    }

    fn data_seen(&self) -> bool {
        self.has_data
    }

    fn ensure_item_growth(
        &self,
        added: usize,
        event: Option<&str>,
        id: Option<&str>,
    ) -> VmResult<()> {
        let data = self
            .data
            .len()
            .checked_add(added)
            .ok_or_else(item_limit_error)?;
        self.ensure_item_size(data, event, id)
    }

    fn ensure_item_size(
        &self,
        data_bytes: usize,
        event: Option<&str>,
        id: Option<&str>,
    ) -> VmResult<()> {
        let size = data_bytes
            .checked_add(event.map_or(0, str::len))
            .and_then(|size| size.checked_add(id.map_or(0, str::len)))
            .ok_or_else(item_limit_error)?;
        if size > self.max_item_bytes {
            return Err(item_limit_error());
        }
        Ok(())
    }

    fn dispatch_event(&mut self) -> SseEvent {
        let data = std::mem::take(&mut self.data);
        self.has_data = false;
        SseEvent {
            event: self.event.take(),
            data,
            id: self.id.clone(),
            retry_ms: self.retry_ms,
        }
    }
}

fn item_limit_error() -> VmError {
    VmError::HostError("SSE item exceeds byte limit".to_string())
}

fn map_value(entries: Vec<(&'static str, Value)>) -> Value {
    Value::Map(std::sync::Arc::new(VmMap::from_entries(
        entries
            .into_iter()
            .map(|(key, value)| (Value::string(key), value))
            .collect(),
    )))
}

fn parse_stream_timeout(request: &VmMap) -> VmResult<Option<Duration>> {
    let Some(value) = request.get(&Value::string("timeout_ms")) else {
        return Ok(None);
    };
    let Value::Int(milliseconds) = value else {
        return Err(VmError::TypeMismatch("SSE timeout_ms"));
    };
    let milliseconds = u64::try_from(*milliseconds)
        .ok()
        .filter(|milliseconds| *milliseconds > 0)
        .ok_or_else(|| VmError::HostError("SSE timeout_ms must be positive".to_string()))?;
    Ok(Some(Duration::from_millis(milliseconds)))
}

/// Shared SSE stream state owned by the child [`SseStreamResource`].
///
/// The child resource is registered under the opened response stream
/// resource, so the generic child-first scope shutdown closes the SSE reader
/// before its underlying response stream. The stop flag is set by the child's
/// [`HostResource::begin_close`] and by the SSE poll operation's cancel; the
/// worker observes it between items and stops promptly.
pub(super) struct SseShared {
    /// The response stream parent this reader is a child of.
    /// Set after the resource is pushed into the scope.
    pub(super) parent: std::sync::Mutex<Option<ResourceHandle>>,
    /// Set on close/cancel; the worker stops polling the network.
    pub(super) stopping: AtomicBool,
    /// Waker registered by the latest pending SSE poll.
    pub(super) waker: std::sync::Mutex<Option<std::task::Waker>>,
    /// One published item ready for the stream driver to pick up.
    pub(super) item: std::sync::Mutex<Option<Value>>,
    /// Set when the worker thread has finished running.
    pub(super) done: AtomicBool,
    /// The final result from the worker thread (Ok or error).
    pub(super) result: std::sync::Mutex<Option<VmResult<()>>>,
    /// The worker thread handle, taken during close to join.
    pub(super) join_handle: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Waker registered by the close poll when the worker is still running.
    pub(super) close_waker: std::sync::Mutex<Option<std::task::Waker>>,
}

/// One SSE event or terminal summary delivered to the guest callback.
enum SseItem {
    Open(Value),
    Event(Value),
    End(Value),
}

/// Runs the whole SSE lifecycle on a worker thread: open the response stream
/// (following redirects), validate it, read body frames, parse events and
/// publish each item into the shared completion channel. The guest callback
/// is invoked by the VM between items via the pending-result adapter.
struct SseWorker {
    config: super::HttpConfig,
    request: HttpRequest,
    deadline: Instant,
    shared: Arc<SseShared>,
    items: Arc<AtomicUsize>,
    bytes_received: Arc<AtomicUsize>,
    status: std::sync::Mutex<Option<u16>>,
    headers: std::sync::Mutex<Option<Arc<VmMap>>>,
    url: std::sync::Mutex<Option<String>>,
}

impl SseWorker {
    fn run(self: Arc<Self>) {
        let permit_holder = None::<()>;
        let _ = permit_holder;
        // The shared permit was moved into the SSE operation driver below;
        // this worker only publishes items.
        let result = self.run_inner();
        *self.shared.result.lock().expect("sse result lock") = Some(result);
        self.shared.done.store(true, Ordering::SeqCst);
        // Wake the close waker before the item waker so the close poll
        // sees the thread is finished before the stream poll drains items.
        let close_wake = {
            let mut waker = self
                .shared
                .close_waker
                .lock()
                .expect("sse close waker lock should not be poisoned");
            waker.take()
        };
        let wake = {
            let mut waker = self
                .shared
                .waker
                .lock()
                .expect("sse waker lock should not be poisoned");
            waker.take()
        };
        if let Some(waker) = close_wake {
            waker.wake();
        }
        if let Some(waker) = wake {
            waker.wake();
        }
    }

    fn run_inner(&self) -> VmResult<()> {
        let mut parser = SseParser::new(
            self.config.max_sse_line_bytes,
            self.config.max_stream_item_bytes,
            self.config.max_stream_total_bytes,
        );
        let observer = ResponseReadObserver::default();
        let (mut response, url) = runtime_block_on(open_stream_response(
            &self.config,
            &self.request,
            observer.clone(),
            Some(self.deadline),
        ))?;
        let status = response.response().status();
        if !status.is_success() {
            return Err(VmError::HostError(format!(
                "SSE response status {} is not successful",
                status.as_u16()
            )));
        }
        let content_type = response
            .response()
            .headers()
            .get(hyper::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .filter(|value| value.eq_ignore_ascii_case("text/event-stream"))
            .ok_or_else(|| {
                VmError::HostError(
                    "SSE response Content-Type must be text/event-stream".to_string(),
                )
            })?;
        let _ = content_type;
        let headers = Arc::new(VmMap::from_entries(response_header_entries(
            response.response().headers(),
        )));
        *self.status.lock().expect("sse status lock") = Some(status.as_u16());
        *self.headers.lock().expect("sse headers lock") = Some(Arc::clone(&headers));
        *self.url.lock().expect("sse url lock") = Some(url.to_string());
        observer.admit_body(self.config.max_stream_total_bytes);
        self.publish(map_value(vec![
            ("kind", Value::string("open")),
            ("status", Value::Int(i64::from(status.as_u16()))),
            ("headers", Value::Map(headers)),
            ("url", Value::string(url.as_str())),
        ]))?;

        loop {
            if self.shared.stopping.load(Ordering::SeqCst) {
                return Err(VmError::HostError("SSE stream closed".to_string()));
            }
            let frame = runtime_block_on(response.next_frame())?;
            let Some(frame) = frame else {
                break;
            };
            let Ok(data) = frame.into_data() else {
                continue;
            };
            parser.admit_chunk(data.len())?;
            observer.observe_application_chunk(data.len());
            self.bytes_received.fetch_add(data.len(), Ordering::SeqCst);
            let mut offset = 0;
            while offset < data.len() {
                let (consumed, event) = parser.push_until_event(&data[offset..])?;
                offset += consumed;
                if let Some(event) = event {
                    self.items.fetch_add(1, Ordering::SeqCst);
                    self.publish(map_value(vec![
                        ("kind", Value::string("event")),
                        ("event", event.event.map_or(Value::Null, Value::string)),
                        ("data", Value::string(event.data)),
                        ("id", event.id.map_or(Value::Null, Value::string)),
                        ("retry_ms", event.retry_ms.map_or(Value::Null, Value::Int)),
                    ]))?;
                }
            }
        }
        parser.finish()?;
        self.publish(map_value(vec![("kind", Value::string("end"))]))
    }

    fn publish(&self, item: Value) -> VmResult<()> {
        *self
            .shared
            .item
            .lock()
            .expect("sse item lock should not be poisoned") = Some(item);
        let wake = self
            .shared
            .waker
            .lock()
            .expect("sse waker lock should not be poisoned")
            .take();
        if let Some(waker) = wake {
            waker.wake();
        }
        Ok(())
    }
}

fn runtime_block_on<F: Future>(future: F) -> F::Output {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("SSE worker tokio runtime should build");
    runtime.block_on(future)
}

/// Stream driver for the SSE stream: the VM's async host polls this driver
/// through [`submit_callable_stream`] for each item, then invokes the script
/// callback and calls [`apply_action`](Self::apply_action) with the result.
struct SseStreamDriver {
    shared: Arc<SseShared>,
    status: u16,
    headers: Arc<VmMap>,
    url: String,
    items: usize,
    permit: super::ConnectionPermit,
}

impl SseStreamDriver {
    fn summary(&self, outcome: &str) -> Value {
        map_value(vec![
            ("outcome", Value::string(outcome)),
            ("status", Value::Int(i64::from(self.status))),
            ("headers", Value::Map(Arc::clone(&self.headers))),
            ("url", Value::string(&self.url)),
            ("items", Value::Int(self.items as i64)),
            ("bytes_received", Value::Int(0)),
            ("bytes_sent", Value::Int(0)),
        ])
    }
}

impl HostStreamDriver for SseStreamDriver {
    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<HostStreamPoll>> {
        let mut item_guard = self
            .shared
            .item
            .lock()
            .expect("sse item lock should not be poisoned");
        if let Some(item) = item_guard.take() {
            // Track items and capture metadata from the open item.
            if let Value::Map(ref map) = item {
                if let Some(Value::String(kind)) = map.get(&Value::string("kind")) {
                    if kind.as_str() == "open" {
                        if let Some(Value::Int(status)) = map.get(&Value::string("status")) {
                            self.status = *status as u16;
                        }
                        if let Some(Value::Map(headers)) = map.get(&Value::string("headers")) {
                            self.headers = Arc::clone(headers);
                        }
                        if let Some(Value::String(url)) = map.get(&Value::string("url")) {
                            self.url = url.as_ref().clone();
                        }
                    }
                }
            }
            self.items = self.items.saturating_add(1);
            return Poll::Ready(Ok(HostStreamPoll::Item(item)));
        }
        drop(item_guard);

        if self.shared.stopping.load(Ordering::SeqCst) {
            // Take the result if the worker already published it, otherwise
            // return a clean stopped summary.
            let result = self
                .shared
                .result
                .lock()
                .expect("sse result lock should not be poisoned")
                .take();
            return match result {
                None | Some(Ok(())) => {
                    Poll::Ready(Ok(HostStreamPoll::Complete(self.summary("stopped"))))
                }
                Some(Err(error)) => Poll::Ready(Err(error)),
            };
        }

        if self.shared.done.load(Ordering::SeqCst) {
            let result = self
                .shared
                .result
                .lock()
                .expect("sse result lock should not be poisoned")
                .take();
            return match result {
                None | Some(Ok(())) => {
                    Poll::Ready(Ok(HostStreamPoll::Complete(self.summary("eof"))))
                }
                Some(Err(error)) => Poll::Ready(Err(error)),
            };
        }

        *self
            .shared
            .waker
            .lock()
            .expect("sse waker lock should not be poisoned") = Some(cx.waker().clone());
        Poll::Pending
    }

    fn apply_action(&mut self, action: Value) -> VmResult<HostStreamAction> {
        let Value::Map(action) = action else {
            return Err(VmError::HostError(
                "SSE callback action must be a map".to_string(),
            ));
        };
        let Some(Value::String(action)) = action.get(&Value::string("action")) else {
            return Err(VmError::HostError(
                "SSE callback action must contain string 'action'".to_string(),
            ));
        };
        match action.as_str() {
            "continue" => Ok(HostStreamAction::Continue),
            "stop" => Ok(HostStreamAction::Complete(self.summary("stopped"))),
            other => Err(VmError::HostError(format!(
                "invalid SSE callback action '{other}'"
            ))),
        }
    }
}

/// The SSE stream reader registered as a child resource in the execution
/// scope. Closing it via the scope lifecycle sets `stopping` on the shared
/// state, which the worker observes between items and stops promptly.
pub(crate) struct SseStreamResource {
    shared: Arc<SseShared>,
}

impl HostResource for SseStreamResource {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        ResourceTypeKey::new("http.sse").ok()
    }

    fn begin_close(&mut self, reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        let _ = reason;
        self.shared.stopping.store(true, Ordering::SeqCst);
        // Wake the item waker so the worker sees the stop flag promptly.
        if let Ok(mut waker) = self.shared.waker.lock() {
            if let Some(waker) = waker.take() {
                waker.wake();
            }
        }
        // Return Pending: the worker thread may still be running. The
        // scope's poll_close machinery will call poll_close below.
        Ok(CloseProgress::Pending)
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        let handle = {
            let mut guard = self
                .shared
                .join_handle
                .lock()
                .expect("sse join handle lock should not be poisoned");
            guard.take()
        };
        let Some(handle) = handle else {
            // Already joined or no worker was ever started.
            return Poll::Ready(Ok(()));
        };
        if !handle.is_finished() {
            // Worker is still running. Store the close waker and put the
            // handle back so we can try again next poll.
            *self
                .shared
                .close_waker
                .lock()
                .expect("sse close waker lock should not be poisoned") = Some(cx.waker().clone());
            *self
                .shared
                .join_handle
                .lock()
                .expect("sse join handle lock should not be poisoned") = Some(handle);
            return Poll::Pending;
        }
        // The worker thread has exited. Join to propagate any panic.
        match handle.join() {
            Ok(()) => Poll::Ready(Ok(())),
            Err(panic) => {
                let message = if let Some(message) = panic.downcast_ref::<&str>() {
                    message.to_string()
                } else if let Some(message) = panic.downcast_ref::<String>() {
                    message.clone()
                } else {
                    "SSE worker thread panicked".to_string()
                };
                Poll::Ready(Err(ResourceError::new(
                    ResourceErrorCode::ResourceCleanupFailed,
                    "http::sse::resource",
                    &message,
                )))
            }
        }
    }
}

/// Scope operation that tracks the pending SSE network poll. Cancel sets
/// `stopping` on the shared state so the worker stops promptly. The actual
/// item delivery is driven by the `SseStreamDriver` through the callable
/// stream path; this operation exists only for scope lifecycle management.
pub(super) struct SseScopeOperation {
    shared: Arc<SseShared>,
}

impl HostOperation for SseScopeOperation {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
        if self.shared.done.load(Ordering::SeqCst) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn cancel(&mut self, reason: OperationCancelReason) -> OperationResult<()> {
        let _ = reason;
        self.shared.stopping.store(true, Ordering::SeqCst);
        if let Ok(mut waker) = self.shared.waker.lock() {
            if let Some(waker) = waker.take() {
                waker.wake();
            }
        }
        Ok(())
    }
}

/// Streams one bounded SSE item into one script callback at a time.
#[pd_host_function(name = "http::client::sse")]
pub(super) fn builtin_http_client_sse(
    vm: &mut Vm,
    request: VmMapHandle,
    on_event: VmCallable<fn(VmMap) -> VmMap>,
) -> VmResult<HostCallResult<VmMap>> {
    let callback = on_event.into_value();
    vm.validate_stream_callback_value(&callback)?;
    let script_timeout = parse_stream_timeout(&request)?;
    let (context, deadline) = HttpRequestContext::capture(vm, script_timeout, "SSE")?;
    let mut request = parse_request(&request, &context.config)?;
    policy::validate_url_policy(&context.config, policy::SchemeFamily::Http, &request.url)?;
    if request.method != hyper::Method::GET && request.method != hyper::Method::POST {
        return Err(VmError::HostError(
            "SSE requests require GET or POST".to_string(),
        ));
    }
    if !request
        .headers
        .iter()
        .any(|(name, _)| name == hyper::header::ACCEPT)
    {
        request.headers.push((
            hyper::header::ACCEPT,
            hyper::header::HeaderValue::from_static("text/event-stream"),
        ));
    }

    let shared = Arc::new(SseShared {
        parent: std::sync::Mutex::new(None),
        stopping: AtomicBool::new(false),
        waker: std::sync::Mutex::new(None),
        item: std::sync::Mutex::new(None),
        done: AtomicBool::new(false),
        result: std::sync::Mutex::new(None),
        join_handle: std::sync::Mutex::new(None),
        close_waker: std::sync::Mutex::new(None),
    });

    // Register the HTTP response stream as a root resource, then register
    // the SSE stream as a child of it. The generic child-first scope shutdown
    // closes the SSE reader before its underlying response stream parent.
    let response_resource = HttpResponseResource;
    let response_token = vm
        .host_context()
        .push_resource(response_resource)
        .map_err(|error| {
            VmError::HostError(format!("failed to push HTTP response resource: {error}"))
        })?;
    let response_handle = response_token.handle();

    let sse_resource = SseStreamResource {
        shared: Arc::clone(&shared),
    };
    let sse_token = vm
        .host_context()
        .push_child_resource(sse_resource, &response_token)
        .map_err(|error| {
            VmError::HostError(format!("failed to push SSE child resource: {error}"))
        })?;
    let sse_handle = sse_token.handle();
    // Store the parent handle so the resource can reference it.
    *shared.parent.lock().expect("sse parent lock") = Some(response_handle);
    let op = SseScopeOperation {
        shared: Arc::clone(&shared),
    };
    let op_id = vm
        .host_context()
        .start_operation(OperationSpec::new(op).with_resource(sse_handle))
        .map_err(|error| VmError::HostError(format!("failed to start SSE operation: {error}")))?;
    let _raw = op_id.raw();

    let worker = Arc::new(SseWorker {
        config: context.config.clone(),
        request,
        deadline,
        shared: Arc::clone(&shared),
        items: Arc::new(AtomicUsize::new(0)),
        bytes_received: Arc::new(AtomicUsize::new(0)),
        status: std::sync::Mutex::new(None),
        headers: std::sync::Mutex::new(None),
        url: std::sync::Mutex::new(None),
    });

    let join_handle = std::thread::Builder::new()
        .name("rustscript-sse-worker".to_string())
        .spawn(move || {
            worker.run();
        })
        .map_err(|error| VmError::HostError(format!("failed to start SSE worker: {error}")))?;
    *shared.join_handle.lock().expect("sse join handle lock") = Some(join_handle);

    let permit = context.into_permit();
    let driver = SseStreamDriver {
        shared,
        status: 0,
        headers: Arc::new(VmMap::default()),
        url: String::new(),
        items: 0,
        permit,
    };

    match vm.submit_callable_stream(callback, driver)? {
        CallOutcome::Pending(op_id) => Ok(HostCallResult::Pending(op_id)),
        _ => Err(VmError::InvalidFrameState(
            "callable stream admission returned a non-pending outcome",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{SseEvent, SseParser};

    fn event(data: &str, event: Option<&str>, id: Option<&str>, retry_ms: Option<i64>) -> SseEvent {
        SseEvent {
            event: event.map(str::to_string),
            data: data.to_string(),
            id: id.map(str::to_string),
            retry_ms,
        }
    }

    fn parse_fragments(
        fragments: &[&[u8]],
        line: usize,
        item: usize,
        total: usize,
    ) -> Result<Vec<SseEvent>, String> {
        let mut parser = SseParser::new(line, item, total);
        let mut events = Vec::new();
        for fragment in fragments {
            events.extend(parser.push(fragment).map_err(|error| error.to_string())?);
        }
        events.extend(parser.finish().map_err(|error| error.to_string())?);
        Ok(events)
    }

    #[test]
    fn parser_accepts_fragmented_bom_utf8_and_every_line_ending() {
        let fragments: &[&[u8]] = &[
            b"\xef",
            b"\xbb\xbfdata: h\xc3",
            b"\xa9\r",
            b"data: two\n",
            b"event:first\r\nevent: final\r",
            b"id: 7\nretry: 25\n\n",
        ];
        assert_eq!(
            parse_fragments(fragments, 64, 128, 256).unwrap(),
            vec![event("hé\ntwo", Some("final"), Some("7"), Some(25))]
        );
    }

    #[test]
    fn parser_clears_event_type_at_empty_data_dispatch_boundary() {
        assert_eq!(
            parse_fragments(
                &[b"event: custom\nid: 7\nretry: 25\n\ndata: payload\n\n"],
                64,
                128,
                256
            )
            .unwrap(),
            vec![event("payload", None, Some("7"), Some(25))]
        );
    }

    #[test]
    fn parser_clears_fragmented_event_type_at_crlf_boundaries() {
        let fragments: &[&[u8]] = &[
            b"event: custom\r",
            b"\nid: 7\r\nretry: 25\r",
            b"\n\r\ndata: pay",
            b"load\r\n\r",
            b"\nevent: named\r\ndata: second\r\n\r\n",
            b"data: next\r\n\r\n",
        ];
        assert_eq!(
            parse_fragments(fragments, 64, 128, 256).unwrap(),
            vec![
                event("payload", None, Some("7"), Some(25)),
                event("second", Some("named"), Some("7"), Some(25)),
                event("next", None, Some("7"), Some(25)),
            ]
        );
    }

    #[test]
    fn parser_uses_first_colon_removes_one_space_and_ignores_comments_unknown_fields() {
        let input = b": comment\ndata:a:b\ndata:  two\ndata: \nunknown: value\n\n";
        assert_eq!(
            parse_fragments(&[input], 64, 128, 256).unwrap(),
            vec![event("a:b\n two\n", None, None, None)]
        );
    }

    #[test]
    fn parser_handles_empty_fields_id_nul_and_retry_rules() {
        let input = b"id: keep\nretry: 42\ndata: one\n\nretry: 99\n\nid:\nid: bad\0id\nretry: -1\nretry: 4x\nretry: 9223372036854775808\nevent:\ndata: two\n\n";
        assert_eq!(
            parse_fragments(&[input], 64, 128, 512).unwrap(),
            vec![
                event("one", None, Some("keep"), Some(42)),
                event("two", Some(""), Some(""), Some(99)),
            ]
        );
    }

    #[test]
    fn parser_persists_retry_state_across_empty_blocks_events_and_invalid_values() {
        let input = b"retry:5000\n\ndata:ready\n\ndata:next\n\nretry:\nretry: -1\nretry: 5x\nretry: 9223372036854775808\n\ndata:still\n\n";
        assert_eq!(
            parse_fragments(&[input], 64, 128, 512).unwrap(),
            vec![
                event("ready", None, None, Some(5000)),
                event("next", None, None, Some(5000)),
                event("still", None, None, Some(5000)),
            ]
        );
    }

    #[test]
    fn parser_discards_incomplete_event_at_eof_and_ignores_field_only_blocks() {
        assert!(
            parse_fragments(&[b"event: named\nid: x\n\ndata: tail"], 64, 128, 256)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            parse_fragments(&[b"id: x\n\ndata: complete\n\n"], 64, 128, 256).unwrap(),
            vec![event("complete", None, Some("x"), None)]
        );
        assert!(
            parse_fragments(&[b"event: unused"], 64, 128, 256)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn parser_rejects_malformed_and_incomplete_utf8() {
        for input in [
            b"data: \xff\n\n".as_slice(),
            b"data: \xc3".as_slice(),
            // A BOM prefix that never completes is still invalid UTF-8 and
            // must surface from `finish` at EOF instead of being dropped.
            b"\xef".as_slice(),
            b"\xef\xbb".as_slice(),
        ] {
            assert!(
                parse_fragments(&[input], 64, 128, 256)
                    .unwrap_err()
                    .contains("UTF-8")
            );
        }
    }

    #[test]
    fn parser_enforces_exact_line_item_and_total_boundaries() {
        assert_eq!(
            parse_fragments(&[b"data: ab\n\n"], 8, 2, 10).unwrap(),
            vec![event("ab", None, None, None)]
        );
        assert!(
            parse_fragments(&[b"data: abc\n\n"], 8, 3, 12)
                .unwrap_err()
                .contains("line")
        );
        assert!(
            parse_fragments(&[b"data: ab\ndata: c\n\n"], 16, 3, 64)
                .unwrap_err()
                .contains("item")
        );
        assert!(
            parse_fragments(&[b"data: ab\n\n"], 8, 2, 9)
                .unwrap_err()
                .contains("total")
        );
    }

    #[test]
    fn parser_rejects_a_single_fragment_before_unbounded_growth() {
        let mut parser = SseParser::new(4, 16, 64);
        assert!(parser.push(b"data: a very large fragment").is_err());
    }

    #[test]
    fn parser_only_strips_a_bom_at_the_start_of_the_stream() {
        assert_eq!(
            parse_fragments(
                &[b"data: first\n\ndata: \xef\xbb\xbfsecond\n\n"],
                64,
                128,
                256
            )
            .unwrap(),
            vec![
                event("first", None, None, None),
                event("\u{feff}second", None, None, None),
            ]
        );
    }
}
