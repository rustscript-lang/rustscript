use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use pd_host_function::pd_host_function;
use tokio::sync::{Notify, mpsc};

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
    ResourceResult, ResourceTypeKey,
};
use crate::vm::{
    CallOutcome, CallReturn, HostStreamAction, HostStreamDriver, HostStreamPoll, Value, Vm,
    VmError, VmResult,
};

/// Maximum number of SSE items buffered between the worker and the stream
/// driver before publishing applies backpressure. A small bounded queue
/// preserves ordering without letting the worker run arbitrarily far ahead of
/// the per-item callback, and without unbounded memory growth on a slow or
/// stalled callback. The worker blocks on an under-capacity send, which keeps
/// it in sync with the driver and prevents both event loss and runaway queue
/// growth.
const SSE_CHANNEL_CAPACITY: usize = 8;

/// The error surfaced when the absolute stream deadline (the minimum of the
/// host maximum stream duration and the script `timeout_ms`) is exceeded.
const SSE_TOTAL_DEADLINE_ERROR: &str = "SSE total deadline exceeded";

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
    /// Set on close/cancel; the worker stops polling the network.
    pub(super) stopping: AtomicBool,
    /// Notified on close/cancel so the worker can break out of a
    /// blocking network read. Race-free: if notify_one() arrives before
    /// the worker starts waiting, the next notified() completes immediately.
    pub(super) cancel: Notify,
    /// Waker registered by the latest pending SSE poll.
    pub(super) waker: std::sync::Mutex<Option<std::task::Waker>>,
    /// Bounded FIFO of published items awaiting the stream driver.
    /// The worker `send`s with backpressure; the driver `try_recv`s.
    /// This preserves item ordering and never drops events, unlike a
    /// single-slot overwrite slot.
    pub(super) items: mpsc::Sender<Value>,
    /// Set when the worker thread has finished running.
    pub(super) done: AtomicBool,
    /// The final result from the worker thread (Ok or error).
    pub(super) result: std::sync::Mutex<Option<VmResult<()>>>,
    /// The worker thread handle, taken during close to join.
    pub(super) join_handle: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Waker registered by the close poll when the worker is still running.
    pub(super) close_waker: std::sync::Mutex<Option<std::task::Waker>>,
    /// The single authoritative absolute stream deadline, derived once when the
    /// worker begins stream I/O (see [`SseWorker::stream_lifecycle`]) and
    /// shared with the stream driver so the callback path enforces the exact
    /// same clock as the network path. Initialized before the first `open`
    /// publish, which is the earliest point the driver can deliver a callback;
    /// a read before initialization is an internal error, never a panic.
    pub(super) deadline: std::sync::OnceLock<Instant>,
}

/// Runs the whole SSE lifecycle on a worker thread: open the response stream
/// (following redirects), validate it, read body frames, parse events and
/// publish each item into the shared completion channel. The guest callback
/// is invoked by the VM between items via the pending-result adapter.
struct SseWorker {
    config: super::HttpConfig,
    request: HttpRequest,
    /// The absolute stream duration (min of host max and script timeout). The
    /// absolute deadline is derived from this when the worker begins stream
    /// I/O, so OS thread-spawn/scheduling latency before the first network
    /// operation does not count against the stream duration.
    total_duration: Duration,
    shared: Arc<SseShared>,
    items: Arc<AtomicUsize>,
    bytes_received: Arc<AtomicUsize>,
    status: std::sync::Mutex<Option<u16>>,
    headers: std::sync::Mutex<Option<Arc<VmMap>>>,
    url: std::sync::Mutex<Option<String>>,
}

impl SseWorker {
    fn run(self: Arc<Self>) {
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
        // The entire SSE network lifecycle (open the response stream, then
        // read every body frame) MUST run inside a single Tokio runtime. The
        // owned response ties the hyper connection future and body receiver to
        // one I/O driver; recreating a fresh current-thread runtime per frame
        // moves a live socket across reactors and corrupts the body framing,
        // surfacing hyper errors like "error reading a body from connection".
        runtime_block_on(self.stream_lifecycle())
    }

    async fn stream_lifecycle(self: &SseWorker) -> VmResult<()> {
        let mut parser = SseParser::new(
            self.config.max_sse_line_bytes,
            self.config.max_stream_item_bytes,
            self.config.max_stream_total_bytes,
        );
        let observer = ResponseReadObserver::default();

        // The absolute total deadline is derived once, when the worker begins
        // stream I/O: OS thread-spawn and scheduling latency before the first
        // network operation is not part of the stream duration. It is never
        // reset by progress. The derived instant is stored in the shared state
        // so the stream driver's callback path enforces the exact same clock;
        // every subsequent read/publish uses this same value.
        let deadline = Instant::now() + self.total_duration;
        let _ = self.shared.deadline.set(deadline);

        // Opening phase: the response headers must arrive before both the
        // opening idle deadline and the absolute total deadline. Whichever
        // boundary is closer wins; when both expire at the same instant the
        // total deadline takes priority. Connection establishment is bounded
        // by the connect timeout inside `open_stream_response`, so slow
        // connects never count against either deadline.
        let opening_idle_deadline = Instant::now() + self.config.stream_idle_timeout;
        let (mut response, url) = self
            .open_response(observer.clone(), opening_idle_deadline)
            .await?;
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
        self.publish(
            map_value(vec![
                ("kind", Value::string("open")),
                ("status", Value::Int(i64::from(status.as_u16()))),
                ("headers", Value::Map(headers)),
                ("url", Value::string(url.as_str())),
            ]),
            deadline,
        )
        .await?;

        // Body phase: every delivered frame resets the idle deadline, while
        // the absolute total deadline is computed once and never reset by
        // progress.
        let mut idle_deadline = Instant::now() + self.config.stream_idle_timeout;
        loop {
            if self.shared.stopping.load(Ordering::SeqCst) {
                return Err(VmError::HostError("SSE stream closed".to_string()));
            }
            let frame = self
                .next_frame(&mut response, idle_deadline, deadline)
                .await?;
            let Some(frame) = frame else {
                break;
            };
            let Ok(data) = frame.into_data() else {
                continue;
            };
            // Any delivered body bytes count as progress: reset the idle
            // deadline, but never touch the absolute total deadline.
            idle_deadline = Instant::now() + self.config.stream_idle_timeout;
            parser.admit_chunk(data.len())?;
            observer.observe_application_chunk(data.len());
            self.bytes_received.fetch_add(data.len(), Ordering::SeqCst);
            let mut offset = 0;
            while offset < data.len() {
                let (consumed, event) = parser.push_until_event(&data[offset..])?;
                offset += consumed;
                if let Some(event) = event {
                    self.items.fetch_add(1, Ordering::SeqCst);
                    self.publish(
                        map_value(vec![
                            ("kind", Value::string("event")),
                            ("event", event.event.map_or(Value::Null, Value::string)),
                            ("data", Value::string(event.data)),
                            ("id", event.id.map_or(Value::Null, Value::string)),
                            ("retry_ms", event.retry_ms.map_or(Value::Null, Value::Int)),
                        ]),
                        deadline,
                    )
                    .await?;
                }
            }
        }
        parser.finish()?;
        self.publish(map_value(vec![("kind", Value::string("end"))]), deadline)
            .await
    }

    /// Opens the response stream bounded by the opening idle deadline and the
    /// absolute total deadline. Connection establishment is bounded by the
    /// connect timeout inside [`open_stream_response`]; the total deadline is
    /// enforced there from when the request is actually sent (so connect
    /// latency never eats into the stream budget), while this outer select
    /// bounds the opening idle deadline. The response-budget expiry carries the
    /// SSE total-deadline error structurally (via the typed
    /// [`ResponseBudget`](super::request::ResponseBudget)), so a connect
    /// timeout (a distinct error) is never mislabelled as a total deadline.
    ///
    /// Cancellation is deliberately NOT a branch here: the worker must always
    /// attempt the connection so a peer waiting on `accept()` is not stranded.
    /// A cancel that arrives during opening is consumed at the next checkpoint
    /// (the body-loop `stopping` check or the publish/next-frame cancel
    /// branches), which is race-free because [`Notify`] retains one
    /// notification until it is awaited.
    async fn open_response(
        &self,
        observer: ResponseReadObserver,
        opening_idle_deadline: Instant,
    ) -> VmResult<(OwnedResponse, url::Url)> {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(opening_idle_deadline)) => {
                Err(VmError::HostError(
                    "SSE stream idle timeout while opening response".to_string(),
                ))
            }
            opened = open_stream_response(
                &self.config,
                &self.request,
                observer,
                Some(super::request::ResponseBudget {
                    duration: self.total_duration,
                    deadline_error: SSE_TOTAL_DEADLINE_ERROR,
                }),
            ) => opened,
        }
    }

    /// Reads one body frame bounded by cancel, the absolute total deadline and
    /// the current idle deadline. Simultaneous boundary expiry is resolved
    /// deterministically in favour of the total deadline.
    async fn next_frame(
        &self,
        response: &mut OwnedResponse,
        idle_deadline: Instant,
        deadline: Instant,
    ) -> VmResult<Option<hyper::body::Frame<hyper::body::Bytes>>> {
        let boundary = deadline.min(idle_deadline);
        tokio::select! {
            biased;
            _ = self.shared.cancel.notified() => {
                Err(VmError::HostError("SSE stream cancelled".to_string()))
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(boundary)) => {
                if deadline <= idle_deadline {
                    Err(VmError::HostError(SSE_TOTAL_DEADLINE_ERROR.to_string()))
                } else {
                    Err(VmError::HostError("SSE stream idle timeout".to_string()))
                }
            }
            frame = response.next_frame() => frame,
        }
    }

    /// Publishes one item into the bounded FIFO with backpressure. The send is
    /// bounded by cancel and the absolute total deadline, so a stalled
    /// callback or full queue cannot extend the stream past its deadline.
    /// Wakes the stream driver's waker so the VM re-polls and drains the item.
    async fn publish(&self, item: Value, deadline: Instant) -> VmResult<()> {
        let sender = &self.shared.items;
        tokio::select! {
            biased;
            _ = self.shared.cancel.notified() => {
                Err(VmError::HostError("SSE stream cancelled".to_string()))
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                Err(VmError::HostError(SSE_TOTAL_DEADLINE_ERROR.to_string()))
            }
            sent = sender.send(item) => {
                sent.map_err(|_| VmError::HostError("SSE stream closed".to_string()))?;
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
    /// Bounded FIFO receiver for items published by the worker.
    receiver: mpsc::Receiver<Value>,
    status: u16,
    headers: Arc<VmMap>,
    url: String,
    items: usize,
    bytes_received: Arc<AtomicUsize>,
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
            (
                "bytes_received",
                Value::Int(self.bytes_received.load(Ordering::Acquire) as i64),
            ),
            ("bytes_sent", Value::Int(0)),
        ])
    }
}

impl SseStreamDriver {
    /// Returns the terminal poll when the stream is stopping or done, or
    /// `None` when neither has been reached.
    ///
    /// Callers drain queued FIFO items first so queued events are delivered
    /// before the terminal state is surfaced (queue-before-terminal ordering).
    ///
    /// ## Happens-before handshake
    ///
    /// The worker publishes its final `result` under the shared result Mutex,
    /// then sets `done` with a `SeqCst` store, then takes and wakes the waker
    /// slot ([`SseWorker::run`]). A `SeqCst` load of `stopping`/`done` here
    /// therefore happens-after every earlier result write, and the same Mutex
    /// guard that observed `done` guarantees the paired result is visible. The
    /// only arm that removes the result is this one (`.take()`), so at most one
    /// `poll_next` call can ever receive it — the driver never double-consumes
    /// the terminal result.
    fn take_terminal(&mut self) -> Option<Poll<VmResult<HostStreamPoll>>> {
        let stopping = self.shared.stopping.load(Ordering::SeqCst);
        let done = self.shared.done.load(Ordering::SeqCst);
        if !stopping && !done {
            return None;
        }
        let result = self
            .shared
            .result
            .lock()
            .expect("sse result lock should not be poisoned")
            .take();
        let outcome = if stopping { "stopped" } else { "eof" };
        Some(match result {
            None | Some(Ok(())) => {
                Poll::Ready(Ok(HostStreamPoll::Complete(self.summary(outcome))))
            }
            Some(Err(error)) => Poll::Ready(Err(error)),
        })
    }
}

impl HostStreamDriver for SseStreamDriver {
    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<HostStreamPoll>> {
        // Drain one published item, tracking metadata. Returns Some when the
        // FIFO has an item, None when it is empty.
        let drain_item = |driver: &mut Self| -> Option<HostStreamPoll> {
            let item = driver.receiver.try_recv().ok()?;
            // Track items and capture metadata from the open item.
            if let Value::Map(ref map) = item {
                if let Some(Value::String(kind)) = map.get(&Value::string("kind")) {
                    if kind.as_str() == "open" {
                        if let Some(Value::Int(status)) = map.get(&Value::string("status")) {
                            driver.status = *status as u16;
                        }
                        if let Some(Value::Map(headers)) = map.get(&Value::string("headers")) {
                            driver.headers = Arc::clone(headers);
                        }
                        if let Some(Value::String(url)) = map.get(&Value::string("url")) {
                            driver.url = url.as_ref().clone();
                        }
                    }
                }
            }
            driver.items = driver.items.saturating_add(1);
            Some(HostStreamPoll::Item(item))
        };
        // Queue first, terminal second: drain any published items before
        // surfacing the stopping/EOF state, so a worker that published events
        // and then terminated delivers those events before the terminal.
        if let Some(poll) = drain_item(self) {
            return Poll::Ready(Ok(poll));
        }
        if let Some(poll) = self.take_terminal() {
            return poll;
        }

        // Register this poll's waker, then re-check the FIFO. The worker's
        // publish does `send` then wakes the waker slot; if the send landed
        // between the first drain and this registration the wake would be
        // delivered to a stale/absent waker and lost. The re-check closes that
        // window: an item that arrived after the first drain is observed here,
        // so a publish can never be stranded in the FIFO with the driver
        // parked. The registered waker is simply replaced on the next poll.
        *self
            .shared
            .waker
            .lock()
            .expect("sse waker lock should not be poisoned") = Some(cx.waker().clone());
        if let Some(poll) = drain_item(self) {
            return Poll::Ready(Ok(poll));
        }
        // Re-check the terminal state after registration. This closes the
        // *completion* lost-wakeup: the worker's terminal epilogue (store the
        // result, set `done`, take+wake the empty waker slot) can land between
        // the first terminal check above and this waker registration, waking
        // nobody. If the worker completes there the driver would otherwise park
        // at Pending forever with `done == true`. Re-checking `stopping`/`done`
        // here (with the same queue-before-terminal ordering) catches that
        // completion deterministically before Pending is returned.
        if let Some(poll) = self.take_terminal() {
            return poll;
        }
        Poll::Pending
    }

    fn apply_action(&mut self, action: Value) -> VmResult<HostStreamAction> {
        // The absolute total deadline is enforced here too: a slow callback
        // (e.g. one awaiting a host future) must not extend the stream past
        // its deadline. Once the deadline has passed, every callback action
        // fails deterministically. The deadline is the same single authoritative
        // instant the worker derived when it began stream I/O and stored in the
        // shared state, so the network and callback paths share one clock. A
        // read before initialization is a driver-state invariant violation (the
        // driver only delivers a callback after the worker has published
        // `open`, which follows deadline initialization), so it is surfaced as
        // a typed internal error rather than an unwrap panic.
        let deadline = self
            .shared
            .deadline
            .get()
            .ok_or_else(|| VmError::HostError("SSE stream deadline not initialized".to_string()))?;
        if Instant::now() >= *deadline {
            return Err(VmError::HostError(SSE_TOTAL_DEADLINE_ERROR.to_string()));
        }
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
        self.shared.cancel.notify_one();
        // Wake the item waker so the stream driver sees the stop flag
        // promptly.
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
        self.shared.cancel.notify_one();
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
    let (context, _capture_deadline) = HttpRequestContext::capture(vm, script_timeout, "SSE")?;
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

    let (items, receiver) = mpsc::channel(SSE_CHANNEL_CAPACITY);
    let shared = Arc::new(SseShared {
        stopping: AtomicBool::new(false),
        cancel: Notify::new(),
        waker: std::sync::Mutex::new(None),
        items,
        done: AtomicBool::new(false),
        result: std::sync::Mutex::new(None),
        join_handle: std::sync::Mutex::new(None),
        close_waker: std::sync::Mutex::new(None),
        deadline: std::sync::OnceLock::new(),
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
    let op = SseScopeOperation {
        shared: Arc::clone(&shared),
    };
    let op_id = vm
        .host_context()
        .start_operation(OperationSpec::new(op).with_resource(sse_handle))
        .map_err(|error| VmError::HostError(format!("failed to start SSE operation: {error}")))?;
    let _ = op_id;

    // The absolute stream duration mirrors `HttpRequestContext::capture`:
    // the script `timeout_ms` caps the host maximum stream duration. The
    // worker derives its absolute deadline from this when it begins stream
    // I/O (see `SseWorker::stream_lifecycle`).
    let total_duration = script_timeout.map_or(context.config.max_stream_duration, |timeout| {
        timeout.min(context.config.max_stream_duration)
    });
    let worker = Arc::new(SseWorker {
        config: context.config.clone(),
        request,
        total_duration,
        shared: Arc::clone(&shared),
        items: Arc::new(AtomicUsize::new(0)),
        bytes_received: Arc::new(AtomicUsize::new(0)),
        status: std::sync::Mutex::new(None),
        headers: std::sync::Mutex::new(None),
        url: std::sync::Mutex::new(None),
    });
    let bytes_received = worker.bytes_received.clone();

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
        receiver,
        status: 0,
        headers: Arc::new(VmMap::default()),
        url: String::new(),
        items: 0,
        bytes_received,
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Wake, Waker};

    use super::{
        SSE_CHANNEL_CAPACITY, SseEvent, SseParser, SseShared, SseStreamDriver, VmMap,
    };
    use crate::vm::{HostStreamAction, HostStreamDriver, HostStreamPoll, Value, VmError};
    use tokio::sync::{Notify, mpsc};

    fn event(data: &str, event: Option<&str>, id: Option<&str>, retry_ms: Option<i64>) -> SseEvent {
        SseEvent {
            event: event.map(str::to_string),
            data: data.to_string(),
            id: id.map(str::to_string),
            retry_ms,
        }
    }

    fn test_shared() -> (Arc<SseShared>, mpsc::Receiver<Value>) {
        let (items, receiver) = mpsc::channel(SSE_CHANNEL_CAPACITY);
        let shared = Arc::new(SseShared {
            stopping: AtomicBool::new(false),
            cancel: Notify::new(),
            waker: std::sync::Mutex::new(None),
            items,
            done: AtomicBool::new(false),
            result: std::sync::Mutex::new(None),
            join_handle: std::sync::Mutex::new(None),
            close_waker: std::sync::Mutex::new(None),
            deadline: std::sync::OnceLock::new(),
        });
        (shared, receiver)
    }

    fn test_driver(shared: Arc<SseShared>, receiver: mpsc::Receiver<Value>) -> SseStreamDriver {
        SseStreamDriver {
            shared,
            receiver,
            status: 0,
            headers: Arc::new(VmMap::default()),
            url: String::new(),
            items: 0,
            bytes_received: Arc::new(AtomicUsize::new(0)),
            permit: super::super::policy::ConnectionAdmission::new(1)
                .acquire()
                .expect("test connection permit"),
        }
    }

    /// A `Wake`-based waker that counts how many times it was woken.
    #[derive(Default)]
    struct WakeCounter(Arc<AtomicUsize>);

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Simulates the worker's terminal epilogue (store result, set `done`,
    /// take+wake the waker slot) landing *exactly between* the driver's first
    /// terminal check and its waker registration.
    ///
    /// The hook runs from a custom `RawWaker` vtable's `clone`, which the
    /// driver invokes at `cx.waker().clone()` during registration — i.e.
    /// precisely inside the lost-wakeup window. The state is intentionally
    /// leaked (`Box::leak`) so the raw waker never needs refcount bookkeeping:
    /// it is a tiny fixed-size test struct, bounded and test-local, not a
    /// production global hook.
    struct TerminalRaceState {
        shared: Arc<SseShared>,
        woke: AtomicBool,
    }

    unsafe fn race_raw_waker(data: *const ()) -> RawWaker {
        RawWaker::new(data, &RACE_WAKER_VTABLE)
    }

    unsafe fn race_clone(data: *const ()) -> RawWaker {
        // SAFETY: `data` is the leaked `TerminalRaceState` pointer passed by
        // `race_waker`, valid for the whole test (see `race_drop`).
        let state = unsafe { &*(data as *const TerminalRaceState) };
        // Worker terminal completion landing between the first terminal check
        // and registration: publish the result, set done, then wake the (still
        // empty) waker slot. Pre-fix this wake is lost and the driver parks at
        // Pending with `done == true`; the post-registration re-check in
        // `poll_next` is what makes this return Complete instead.
        *state
            .shared
            .result
            .lock()
            .expect("sse result lock should not be poisoned") = Some(Ok(()));
        state.shared.done.store(true, Ordering::SeqCst);
        if let Some(waker) = state
            .shared
            .waker
            .lock()
            .expect("sse waker lock should not be poisoned")
            .take()
        {
            state.woke.store(true, Ordering::SeqCst);
            waker.wake();
        }
        // SAFETY: `data` is the same leaked `TerminalRaceState` pointer.
        unsafe { race_raw_waker(data) }
    }

    unsafe fn race_wake(data: *const ()) {
        // SAFETY: `data` is the leaked `TerminalRaceState` pointer; see
        // `race_clone`.
        let state = unsafe { &*(data as *const TerminalRaceState) };
        state.woke.store(true, Ordering::SeqCst);
    }

    unsafe fn race_wake_by_ref(data: *const ()) {
        // SAFETY: `data` is the leaked `TerminalRaceState` pointer; see
        // `race_clone`.
        unsafe { race_wake(data) };
    }

    unsafe fn race_drop(_data: *const ()) {
        // Intentionally leaked: the test owns the `TerminalRaceState` via
        // `Box::leak`; nothing to free here.
    }

    static RACE_WAKER_VTABLE: RawWakerVTable =
        RawWakerVTable::new(race_clone, race_wake, race_wake_by_ref, race_drop);

    fn race_waker(state: &'static TerminalRaceState) -> Waker {
        unsafe { Waker::from_raw(race_raw_waker(std::ptr::from_ref(state).cast())) }
    }

    /// A worker completion that lands *after* waker registration must wake the
    /// registered waker and drive the next poll to `Complete` — no timer, no
    /// self-poll, exactly what the async host's `await_waiting_host_op`
    /// `poll_fn` relies on.
    #[test]
    fn driver_completes_from_worker_wake_without_timer() {
        let (shared, receiver) = test_shared();
        let mut driver = test_driver(Arc::clone(&shared), receiver);
        let wakes = Arc::new(AtomicUsize::new(0));
        let waker = Waker::from(Arc::new(WakeCounter(Arc::clone(&wakes))));
        let mut cx = Context::from_waker(&waker);

        assert!(matches!(driver.poll_next(&mut cx), Poll::Pending));
        assert_eq!(wakes.load(Ordering::SeqCst), 0);

        // Worker epilogue: publish the terminal result, set done, then wake
        // the waker slot the driver just registered.
        *shared.result.lock().expect("sse result lock") = Some(Ok(()));
        shared.done.store(true, Ordering::SeqCst);
        let registered = shared
            .waker
            .lock()
            .expect("sse waker lock")
            .take()
            .expect("a pending poll must have registered its waker");
        registered.wake();
        assert_eq!(wakes.load(Ordering::SeqCst), 1);

        // The executor re-polls after the wake; the terminal is now visible.
        match driver.poll_next(&mut cx) {
            Poll::Ready(Ok(HostStreamPoll::Complete(summary))) => {
                let Value::Map(map) = summary else {
                    panic!("expected summary map, got {summary:?}");
                };
                assert_eq!(
                    map.get(&Value::string("outcome")),
                    Some(&Value::string("eof"))
                );
            }
            other => panic!("expected wake-driven Complete, got {other:?}"),
        }
    }

    /// Deterministic regression for the completion lost-wakeup: the worker's
    /// whole terminal epilogue lands *between* the driver's first terminal
    /// check and its waker registration, waking an empty slot. The driver must
    /// re-check the terminal state after registration and return `Complete`
    /// instead of parking at `Pending` forever with `done == true`.
    #[test]
    fn driver_rechecks_terminal_after_waker_registration() {
        let (shared, receiver) = test_shared();
        let mut driver = test_driver(Arc::clone(&shared), receiver);
        let state: &'static TerminalRaceState = Box::leak(Box::new(TerminalRaceState {
            shared: Arc::clone(&shared),
            woke: AtomicBool::new(false),
        }));
        let waker = race_waker(state);
        let mut cx = Context::from_waker(&waker);

        // The single poll_next call must observe the terminal completion that
        // its own waker registration triggered, and return Complete.
        match driver.poll_next(&mut cx) {
            Poll::Ready(Ok(HostStreamPoll::Complete(summary))) => {
                let Value::Map(map) = summary else {
                    panic!("expected summary map, got {summary:?}");
                };
                assert_eq!(
                    map.get(&Value::string("outcome")),
                    Some(&Value::string("eof"))
                );
            }
            other => panic!(
                "terminal completion during waker registration must be observed, got {other:?}"
            ),
        }
        // The completion landed before registration, so the epilogue's wake
        // found an empty slot (the bug: the wake is lost but must not matter).
        assert!(!state.woke.load(Ordering::SeqCst));
        assert!(shared.done.load(Ordering::SeqCst));
    }

    /// Queue-before-terminal ordering: events published before the worker
    /// terminates are delivered as items before the terminal `Complete`.
    #[test]
    fn driver_drains_queued_items_before_terminal() {
        let (shared, receiver) = test_shared();
        let mut driver = test_driver(Arc::clone(&shared), receiver);
        let waker = Waker::from(Arc::new(WakeCounter::default()));
        let mut cx = Context::from_waker(&waker);

        // Two items are already queued before the terminal is published.
        shared
            .items
            .try_send(Value::Int(1))
            .expect("test send");
        shared
            .items
            .try_send(Value::Int(2))
            .expect("test send");
        *shared.result.lock().expect("sse result lock") = Some(Ok(()));
        shared.done.store(true, Ordering::SeqCst);

        assert!(matches!(
            driver.poll_next(&mut cx),
            Poll::Ready(Ok(HostStreamPoll::Item(Value::Int(1))))
        ));
        assert!(matches!(
            driver.poll_next(&mut cx),
            Poll::Ready(Ok(HostStreamPoll::Item(Value::Int(2))))
        ));
        assert!(matches!(
            driver.poll_next(&mut cx),
            Poll::Ready(Ok(HostStreamPoll::Complete(_)))
        ));
        assert!(driver.items == 2);
    }

    /// The driver's `apply_action` must enforce the *shared* deadline the
    /// worker stored when it began stream I/O — the same single authoritative
    /// clock as the network reads/publishes. Injecting a known deadline proves
    /// the callback path uses exactly that value (not a separately captured
    /// admission-time instant), and that an uninitialized read is a typed
    /// internal error rather than an unwrap panic.
    #[test]
    fn driver_apply_action_enforces_the_shared_deadline() {
        // A deadline far in the future: the callback continues.
        let (shared, receiver) = test_shared();
        let mut driver = test_driver(Arc::clone(&shared), receiver);
        let known = std::time::Instant::now() + std::time::Duration::from_secs(60);
        shared
            .deadline
            .set(known)
            .expect("first deadline set must succeed");
        let action = super::map_value(vec![("action", Value::string("continue"))]);
        assert!(matches!(
            driver.apply_action(action),
            Ok(HostStreamAction::Continue)
        ));

        // A deadline in the past: the callback is rejected with the SSE total
        // deadline, deterministically, regardless of when this test runs.
        let (shared, receiver) = test_shared();
        let mut driver = test_driver(Arc::clone(&shared), receiver);
        shared
            .deadline
            .set(std::time::Instant::now() - std::time::Duration::from_secs(1))
            .expect("first deadline set must succeed");
        let action = super::map_value(vec![("action", Value::string("continue"))]);
        assert!(matches!(
            driver.apply_action(action),
            Err(VmError::HostError(ref message)) if message == super::SSE_TOTAL_DEADLINE_ERROR
        ));

        // Uninitialized deadline: a typed internal error, never a panic.
        let (shared, receiver) = test_shared();
        let mut driver = test_driver(Arc::clone(&shared), receiver);
        let action = super::map_value(vec![("action", Value::string("continue"))]);
        assert!(matches!(
            driver.apply_action(action),
            Err(VmError::HostError(ref message)) if message == "SSE stream deadline not initialized"
        ));
    }

    /// The deadline cell is derived exactly once: the worker's first `set`
    /// succeeds and a second `set` is rejected, so the driver can never observe
    /// a different deadline than the one the network path used.
    #[test]
    fn shared_deadline_is_derived_exactly_once() {
        let (shared, _receiver) = test_shared();
        let first = std::time::Instant::now() + std::time::Duration::from_secs(10);
        assert!(shared.deadline.set(first).is_ok());
        let second = first + std::time::Duration::from_secs(10);
        assert!(shared.deadline.set(second).is_err());
        assert_eq!(shared.deadline.get(), Some(&first));
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
