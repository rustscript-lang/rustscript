use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::task::AtomicWaker;
use pd_host_function::pd_host_function;
use tokio::sync::{Notify, mpsc};

use super::request::{
    HttpRequest, OwnedResponse, ResponseReadObserver, open_stream_response, parse_request,
    response_header_entries, validate_request_header_budget,
};
use super::{HttpRequestContext, policy};
use crate::builtins::runtime::HostCallResult;
use crate::builtins::runtime::typed::{VmCallable, VmMap, VmMapHandle};
use crate::host_api::ResourceTypeKey;
use crate::vm::async_host::{
    HostStreamAction, HostStreamDriver, HostStreamPoll, HostStreamTermination,
};
use crate::vm::operation::{
    HostOperation, OperationCancelReason, OperationError, OperationErrorCode, OperationId,
    OperationResult, OperationSpec,
};
use crate::vm::resource::{
    CloseProgress, HostResource, ResourceCloseReason, ResourceError, ResourceErrorCode,
    ResourceHandle, ResourceResult,
};
use crate::vm::{
    CallOutcome, Value, Vm, VmError, VmResult,
    execution_scope::{ExecutionScope, ExecutionScopeError},
};

/// Maximum number of SSE items buffered between the worker and the stream
/// driver before publishing applies backpressure. A small bounded queue
/// preserves ordering without letting the worker run arbitrarily far ahead of
/// the per-item callback, and without unbounded memory growth on a slow or
/// stalled callback. The worker blocks on an under-capacity send, which keeps
/// it in sync with the driver and prevents both event loss and runaway queue
/// growth.
const SSE_CHANNEL_CAPACITY: usize = 1;

#[cfg(test)]
const _: () = assert!(SSE_CHANNEL_CAPACITY == 1);

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
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SseWorkerLifecycle {
    NotStarted = 0,
    Running = 1,
    Finished = 2,
}

pub(super) struct SseShared {
    /// Set on close/cancel; the worker stops polling the network.
    pub(super) stopping: AtomicBool,
    /// Notified on close/cancel so the worker can break out of a
    /// blocking network read. Race-free: if notify_one() arrives before
    /// the worker starts waiting, the next notified() completes immediately.
    pub(super) cancel: Notify,
    /// The first cancellation reason is retained for the producer and cleanup
    /// diagnostics. Later cancellation requests cannot overwrite it.
    pub(super) cancellation_reason: std::sync::Mutex<Option<OperationCancelReason>>,
    /// One acknowledgement is issued by the VM after each callback. The
    /// acknowledgement arrives.
    pub(super) item_ack: Notify,
    /// Waker registered by a pending stream poll. Channel readiness is handled
    /// by `Receiver::poll_recv`; this waker covers stop and terminal state.
    pub(super) waker: AtomicWaker,
    /// Bounded FIFO of published items awaiting the stream driver.
    /// The worker `send`s with backpressure; the driver `try_recv`s.
    /// This preserves item ordering and never drops events, unlike a
    /// single-slot overwrite slot.
    pub(super) items: mpsc::Sender<Value>,
    /// Set when the worker thread has finished running.
    pub(super) done: AtomicBool,
    /// Set by the spawned closure after the worker entry has returned.
    pub(super) thread_finished: AtomicBool,
    /// Explicit worker lifecycle. Workerless rollback may transition only from
    /// `NotStarted` to `Finished`.
    pub(super) worker_lifecycle: std::sync::atomic::AtomicU8,
    /// The final result from the worker thread (Ok or error).
    pub(super) result: std::sync::Mutex<Option<VmResult<()>>>,
    /// The worker thread handle, taken during close to join.
    pub(super) join_handle: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Waker registered by the close poll when the worker is still running.
    pub(super) close_waker: AtomicWaker,
    /// Waker registered by the scoped operation while the producer is still
    /// running.
    pub(super) quiescence_waker: AtomicWaker,
    /// The permit is owned by shared stream state, so dropping the driver
    /// cannot release admission while the worker or transport is alive.
    pub(super) _permit: super::ConnectionPermit,
    /// Set after rollback has retired both the operation and resource.
    pub(super) rollback_finished: AtomicBool,
}

impl SseShared {
    fn mark_worker_running(&self) {
        let _ = self.worker_lifecycle.compare_exchange(
            SseWorkerLifecycle::NotStarted as u8,
            SseWorkerLifecycle::Running as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn mark_worker_finished(&self) {
        self.thread_finished.store(true, Ordering::Release);
        self.worker_lifecycle
            .store(SseWorkerLifecycle::Finished as u8, Ordering::Release);
        self.close_waker.wake();
        self.quiescence_waker.wake();
    }

    fn terminalize_workerless(&self, reason: OperationCancelReason, result: VmResult<()>) -> bool {
        if self
            .worker_lifecycle
            .compare_exchange(
                SseWorkerLifecycle::NotStarted as u8,
                SseWorkerLifecycle::Finished as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        self.request_stop(reason);
        self.publish(result);
        self.thread_finished.store(true, Ordering::Release);
        self.close_waker.wake();
        self.quiescence_waker.wake();
        true
    }

    fn request_stop(&self, reason: OperationCancelReason) {
        self.stopping.store(true, Ordering::Release);
        let mut cancellation = self
            .cancellation_reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cancellation.is_none() {
            *cancellation = Some(reason);
        }
        self.cancel.notify_one();
        self.cancel.notify_waiters();
        self.waker.wake();
        self.close_waker.wake();
        self.quiescence_waker.wake();
    }

    fn publish(&self, result: VmResult<()>) {
        *self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
        // The result mutex write happens-before this release publication.
        self.done.store(true, Ordering::Release);
        self.waker.wake();
        self.close_waker.wake();
        self.quiescence_waker.wake();
    }

    fn take_result(&self) -> Option<VmResult<()>> {
        self.result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    fn is_quiescent(&self) -> bool {
        self.worker_lifecycle.load(Ordering::Acquire) == SseWorkerLifecycle::Finished as u8
            && self.done.load(Ordering::Acquire)
            && self.thread_finished.load(Ordering::Acquire)
            && self
                .join_handle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none()
    }

    fn join_worker(&self) -> Result<(), String> {
        if self.worker_lifecycle.load(Ordering::Acquire) != SseWorkerLifecycle::Finished as u8
            || !self.done.load(Ordering::Acquire)
            || !self.thread_finished.load(Ordering::Acquire)
        {
            return Err("SSE worker is still running".to_string());
        }
        if self
            .join_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
        {
            return Err("SSE worker thread has not exited".to_string());
        }
        let handle = self
            .join_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(handle) = handle else {
            return Ok(());
        };
        handle.join().map_err(|panic| {
            let message = worker_panic_message(&panic);
            match self.cancellation_reason() {
                Some(reason) => format!("{message} (cancellation reason: {reason})"),
                None => message,
            }
        })
    }

    fn try_join_finished(&self) -> ResourceResult<bool> {
        if self.worker_lifecycle.load(Ordering::Acquire) != SseWorkerLifecycle::Finished as u8
            || !self.done.load(Ordering::Acquire)
            || !self.thread_finished.load(Ordering::Acquire)
        {
            return Ok(false);
        }
        let handle = {
            let mut guard = self
                .join_handle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.as_ref().is_some_and(|handle| !handle.is_finished()) {
                return Ok(false);
            }
            guard.take()
        };
        let Some(handle) = handle else {
            return Ok(true);
        };
        handle
            .join()
            .map(|_| true)
            .map_err(|panic| resource_cleanup_error(&worker_panic_message(&panic)))
    }

    fn cancellation_reason(&self) -> Option<OperationCancelReason> {
        self.cancellation_reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .copied()
    }
}

fn worker_panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else {
        "SSE worker thread panicked".to_string()
    }
}

#[cfg(test)]
static FAIL_NEXT_WORKER_SPAWN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn spawn_worker<F>(name: &str, function: F) -> std::io::Result<std::thread::JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    #[cfg(test)]
    if FAIL_NEXT_WORKER_SPAWN.swap(false, Ordering::AcqRel) {
        return Err(std::io::Error::other("injected SSE worker spawn failure"));
    }
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(function)
}

#[cfg(test)]
static REJECT_NEXT_OPERATION_ADMISSION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[allow(clippy::result_large_err)]
fn start_operation<T: HostOperation>(
    vm: &mut Vm,
    operation: T,
) -> crate::vm::host_context::HostContextResult<OperationId> {
    #[cfg(test)]
    if REJECT_NEXT_OPERATION_ADMISSION.swap(false, Ordering::AcqRel) {
        return Err(crate::vm::host_context::HostContextError::new(
            "http::operation",
            "injected operation admission rejection",
        ));
    }
    vm.host_context()
        .start_operation(OperationSpec::new(operation))
}

fn resource_cleanup_error(message: &str) -> ResourceError {
    ResourceError::new(
        ResourceErrorCode::ResourceCleanupFailed,
        "http::sse::resource",
        message,
    )
}

/// Runs the whole SSE lifecycle on a worker thread: open the response stream
/// (following redirects), validate it, read body frames, parse events and
/// publish each item into the shared completion channel. The guest callback
/// is invoked by the VM between items via the pending-result adapter.
struct SseWorker {
    config: super::HttpConfig,
    request: HttpRequest,
    /// The one absolute stream deadline captured at stream admission. It is
    /// passed unchanged through opening, redirects, body reads, and delivery.
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
        // The permit is held by shared stream state until cleanup completes.
        let result =
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.run_inner())) {
                Ok(result) => result,
                Err(panic) => Err(VmError::HostError(format!(
                    "SSE worker panicked: {}",
                    worker_panic_message(&panic)
                ))),
            };
        self.shared.publish(result);
    }

    fn run_inner(&self) -> VmResult<()> {
        // The entire SSE network lifecycle (open the response stream, then
        // read every body frame) MUST run inside a single Tokio runtime. The
        // owned response ties the hyper connection future and body receiver to
        // one I/O driver; recreating a fresh current-thread runtime per frame
        // moves a live socket across reactors and corrupts the body framing,
        // surfacing hyper errors like "error reading a body from connection".
        runtime_block_on(self.stream_lifecycle())?
    }

    async fn stream_lifecycle(self: &SseWorker) -> VmResult<()> {
        let mut parser = SseParser::new(
            self.config.max_sse_line_bytes,
            self.config.max_stream_item_bytes,
            self.config.max_stream_total_bytes,
        );
        let observer = ResponseReadObserver::default();

        // The absolute deadline was captured before admission and is shared by
        // every opening hop, body read, and callback publication.
        let deadline = self.deadline;

        // Opening response headers must arrive before the earlier of the
        // captured total deadline and this opening phase's idle boundary.
        let opening_idle_deadline =
            super::policy::phase_deadline(deadline, self.config.stream_idle_timeout);
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
        *self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(status.as_u16());
        *self
            .headers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&headers));
        *self
            .url
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(url.to_string());
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
        let mut idle_deadline =
            super::policy::phase_deadline(deadline, self.config.stream_idle_timeout);
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
            idle_deadline =
                super::policy::phase_deadline(deadline, self.config.stream_idle_timeout);
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

    /// Opens the response stream with one absolute deadline shared by DNS,
    /// connection setup, TLS, request/response headers, and every redirect.
    /// The outer select also applies the opening idle phase limit; whichever
    /// boundary is earlier wins.
    ///
    /// Cancellation is selected alongside the opening deadlines. Dropping the
    /// whole opening future also drops every DNS/connect/TLS/header future and
    /// every redirect hop owned by `open_stream_response`.
    async fn open_response(
        &self,
        observer: ResponseReadObserver,
        opening_idle_deadline: Instant,
    ) -> VmResult<(OwnedResponse, url::Url)> {
        if self.shared.stopping.load(Ordering::Acquire) {
            return Err(VmError::HostError("SSE stream cancelled".to_string()));
        }
        tokio::select! {
            biased;
            _ = self.shared.cancel.notified() => {
                Err(VmError::HostError("SSE stream cancelled".to_string()))
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(opening_idle_deadline)) => {
                if self.deadline <= opening_idle_deadline {
                    Err(VmError::HostError(SSE_TOTAL_DEADLINE_ERROR.to_string()))
                } else {
                    Err(VmError::HostError(
                        "SSE stream idle timeout while opening response".to_string(),
                    ))
                }
            }
            opened = open_stream_response(
                &self.config,
                &self.request,
                observer,
                self.deadline,
                opening_idle_deadline,
            ) => {
                opened.map_err(|error| {
                    if error.to_string().contains("HTTP request deadline exceeded") {
                        let now = Instant::now();
                        if now >= self.deadline {
                            VmError::HostError(SSE_TOTAL_DEADLINE_ERROR.to_string())
                        } else if now >= opening_idle_deadline {
                            VmError::HostError(
                                "SSE stream idle timeout while opening response".to_string(),
                            )
                        } else {
                            error
                        }
                    } else {
                        error
                    }
                })
            }
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
        if self.shared.stopping.load(Ordering::Acquire) {
            return Err(VmError::HostError("SSE stream cancelled".to_string()));
        }
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
        if self.shared.stopping.load(Ordering::Acquire) {
            return Err(VmError::HostError("SSE stream cancelled".to_string()));
        }
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
                self.shared.waker.wake();
                if self.shared.stopping.load(Ordering::Acquire) {
                    return Err(VmError::HostError("SSE stream cancelled".to_string()));
                }
                tokio::select! {
                    biased;
                    _ = self.shared.cancel.notified() => {
                        Err(VmError::HostError("SSE stream cancelled".to_string()))
                    }
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                        Err(VmError::HostError(SSE_TOTAL_DEADLINE_ERROR.to_string()))
                    }
                    _ = self.shared.item_ack.notified() => Ok(()),
                }
            }
        }
    }
}

fn runtime_block_on<F: Future>(future: F) -> VmResult<F::Output> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| VmError::HostError(format!("SSE worker runtime build failed: {error}")))?;
    Ok(runtime.block_on(future))
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
    /// The absolute total deadline; the driver enforces it in
    /// [`apply_action`](Self::apply_action) so a slow callback cannot extend
    /// the stream past its deadline.
    deadline: Instant,
    scope_operation: crate::vm::operation::OperationId,
    resource: ResourceHandle,
    termination: Option<SseTerminationState>,
}

struct SseTerminationState {
    operation_done: bool,
    resource_done: bool,
    first_error: Option<VmError>,
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

impl HostStreamDriver for SseStreamDriver {
    fn acknowledge_item(&mut self) {
        self.shared.item_ack.notify_one();
    }

    fn terminate(
        &mut self,
        scope: &mut ExecutionScope,
        termination: HostStreamTermination,
    ) -> VmResult<()> {
        self.begin_termination(scope, termination)?;
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        match self.poll_termination(scope, termination, &mut cx) {
            Poll::Ready(result) => result,
            Poll::Pending => Err(VmError::HostError(
                "SSE stream termination is still pending".to_string(),
            )),
        }
    }

    fn begin_termination(
        &mut self,
        scope: &mut ExecutionScope,
        termination: HostStreamTermination,
    ) -> VmResult<()> {
        if self.termination.is_some() {
            return Ok(());
        }
        if let HostStreamTermination::Cancelled(reason) = termination {
            self.shared.request_stop(reason);
        }
        match termination {
            HostStreamTermination::Completed => scope
                .complete_operation(self.scope_operation)
                .map_err(VmError::ExecutionScope)?,
            HostStreamTermination::Cancelled(reason) => scope
                .cancel_operation(self.scope_operation, reason)
                .map_err(VmError::ExecutionScope)?,
        };
        let resource_reason = sse_resource_close_reason(termination);
        let resource_done = match scope
            .close_resource::<SseStreamResource>(self.resource, resource_reason)
            .map_err(VmError::ExecutionScope)?
        {
            CloseProgress::Ready => true,
            CloseProgress::Pending => false,
        };
        self.termination = Some(SseTerminationState {
            operation_done: false,
            resource_done,
            first_error: None,
        });
        Ok(())
    }

    fn poll_termination(
        &mut self,
        scope: &mut ExecutionScope,
        _termination: HostStreamTermination,
        cx: &mut Context<'_>,
    ) -> Poll<VmResult<()>> {
        let Some(state) = self.termination.as_mut() else {
            return Poll::Ready(Err(VmError::HostError(
                "SSE stream termination was not started".to_string(),
            )));
        };
        if !state.operation_done {
            match scope.poll_operation_quiescence(self.scope_operation, cx) {
                Poll::Pending => {}
                Poll::Ready(Ok(_)) => state.operation_done = true,
                Poll::Ready(Err(error)) => {
                    state.operation_done = true;
                    if state.first_error.is_none() {
                        state.first_error = Some(VmError::ExecutionScope(error));
                    }
                }
            }
        }
        if !state.resource_done {
            match scope.poll_resource_close::<SseStreamResource>(self.resource, cx) {
                Poll::Pending => {}
                Poll::Ready(Ok(())) => state.resource_done = true,
                Poll::Ready(Err(ExecutionScopeError::Resource(error)))
                    if error.code() == ResourceErrorCode::ResourceAlreadyClosed =>
                {
                    state.resource_done = true;
                }
                Poll::Ready(Err(error)) => {
                    state.resource_done = true;
                    if state.first_error.is_none() {
                        state.first_error = Some(VmError::ExecutionScope(error));
                    }
                }
            }
        }
        if state.operation_done && state.resource_done {
            let state = self.termination.take().expect("termination state exists");
            match state.first_error {
                Some(error) => Poll::Ready(Err(error)),
                None => Poll::Ready(Ok(())),
            }
        } else {
            Poll::Pending
        }
    }

    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<HostStreamPoll>> {
        match self.receiver.poll_recv(cx) {
            Poll::Ready(Some(item)) => {
                // Track items and capture metadata from the open item.
                if let Value::Map(ref map) = item {
                    match map.get(&Value::string("kind")) {
                        Some(Value::String(kind)) if kind.as_str() == "open" => {
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
                        _ => {}
                    }
                }
                self.items = self.items.saturating_add(1);
                Poll::Ready(Ok(HostStreamPoll::Item(item)))
            }
            Poll::Ready(None) => self.poll_terminal("eof"),
            Poll::Pending => {
                if self.shared.done.load(Ordering::Acquire) {
                    return self.poll_terminal(if self.shared.stopping.load(Ordering::Acquire) {
                        "stopped"
                    } else {
                        "eof"
                    });
                }
                self.shared.waker.register(cx.waker());
                if self.shared.done.load(Ordering::Acquire) {
                    self.poll_terminal(if self.shared.stopping.load(Ordering::Acquire) {
                        "stopped"
                    } else {
                        "eof"
                    })
                } else {
                    // A stop is terminal only after the worker publishes its
                    // result. The stop notification wakes this poll through
                    // the atomic waker while the worker is still unwinding.
                    Poll::Pending
                }
            }
        }
    }

    fn apply_action(&mut self, action: Value) -> VmResult<HostStreamAction> {
        // The absolute total deadline is enforced here too: a slow callback
        // (e.g. one awaiting a host future) must not extend the stream past
        // its deadline. Once the deadline has passed, every callback action
        // fails deterministically.
        if Instant::now() >= self.deadline {
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
            "stop" => Ok(HostStreamAction::Cancel(
                self.summary("stopped"),
                OperationCancelReason::Requested,
            )),
            other => Err(VmError::HostError(format!(
                "invalid SSE callback action '{other}'"
            ))),
        }
    }
}

impl SseStreamDriver {
    fn poll_terminal(&mut self, outcome: &str) -> Poll<VmResult<HostStreamPoll>> {
        match self.shared.take_result() {
            Some(Ok(())) => Poll::Ready(Ok(HostStreamPoll::Complete(self.summary(outcome)))),
            Some(Err(error)) => Poll::Ready(Err(error)),
            None => Poll::Ready(Err(VmError::HostError(
                "SSE worker completed without a terminal result".to_string(),
            ))),
        }
    }
}

fn sse_resource_close_reason(termination: HostStreamTermination) -> ResourceCloseReason {
    match termination {
        HostStreamTermination::Completed => ResourceCloseReason::ResourceClosed,
        HostStreamTermination::Cancelled(reason) => match reason {
            OperationCancelReason::Requested => ResourceCloseReason::Requested,
            OperationCancelReason::Deadline => ResourceCloseReason::Deadline,
            OperationCancelReason::VmReset => ResourceCloseReason::VmReset,
            OperationCancelReason::Parent => ResourceCloseReason::Parent,
            OperationCancelReason::ResourceClosed => ResourceCloseReason::ResourceClosed,
            OperationCancelReason::VmDrop => ResourceCloseReason::VmDrop,
        },
    }
}

fn close_sse_resource(vm: &mut Vm, resource: ResourceHandle) -> VmResult<()> {
    let progress = vm
        .host_context()
        .close_resource::<SseStreamResource>(resource, ResourceCloseReason::ResourceClosed)
        .map_err(|error| VmError::HostError(format!("failed to close SSE resource: {error}")))?;
    match progress {
        CloseProgress::Ready => Ok(()),
        CloseProgress::Pending => Err(VmError::HostError(
            "SSE resource close remained pending after producer retirement".to_string(),
        )),
    }
}

fn rollback_sse_admission(
    vm: &mut Vm,
    shared: &Arc<SseShared>,
    resource: ResourceHandle,
    operation: Option<OperationId>,
    primary: VmError,
) -> VmError {
    if shared.rollback_finished.load(Ordering::Acquire) {
        return primary;
    }
    let mut cleanup_errors = Vec::new();
    let _ = shared.terminalize_workerless(
        OperationCancelReason::Requested,
        Err(VmError::HostError("SSE worker was not started".to_string())),
    );
    if let Some(operation) = operation
        && let Err(error) = vm
            .host_context()
            .abort_operation(operation, OperationCancelReason::Requested)
    {
        cleanup_errors.push(VmError::HostError(format!(
            "failed to abort SSE operation: {error}"
        )));
    }
    if let Err(error) = shared.join_worker() {
        cleanup_errors.push(VmError::HostError(format!(
            "failed to join SSE worker: {error}"
        )));
    }
    if let Err(error) = close_sse_resource(vm, resource) {
        cleanup_errors.push(error);
    }
    if cleanup_errors.is_empty() {
        shared.rollback_finished.store(true, Ordering::Release);
    }
    cleanup_errors
        .into_iter()
        .fold(primary, |primary, cleanup| {
            crate::vm::async_host::preserve_stream_cleanup(primary, Err(cleanup))
        })
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
        self.shared.request_stop(match reason {
            ResourceCloseReason::Requested => OperationCancelReason::Requested,
            ResourceCloseReason::Deadline => OperationCancelReason::Deadline,
            ResourceCloseReason::VmReset => OperationCancelReason::VmReset,
            ResourceCloseReason::Parent => OperationCancelReason::Parent,
            ResourceCloseReason::ResourceClosed => OperationCancelReason::ResourceClosed,
            ResourceCloseReason::VmDrop => OperationCancelReason::VmDrop,
        });
        match self.shared.try_join_finished()? {
            true => Ok(CloseProgress::Ready),
            false => Ok(CloseProgress::Pending),
        }
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        match self.shared.try_join_finished() {
            Ok(true) => Poll::Ready(Ok(())),
            Ok(false) => {
                self.shared.close_waker.register(cx.waker());
                match self.shared.try_join_finished() {
                    Ok(true) => Poll::Ready(Ok(())),
                    Ok(false) => Poll::Pending,
                    Err(error) => Poll::Ready(Err(error)),
                }
            }
            Err(error) => Poll::Ready(Err(error)),
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
        self.shared.request_stop(reason);
        Ok(())
    }

    fn cancel_and_wait(&mut self, reason: OperationCancelReason) -> OperationResult<()> {
        self.shared.request_stop(reason);
        if !self.shared.is_quiescent() {
            return Err(OperationError::new(
                OperationErrorCode::OperationDriverFailed,
                "http::sse",
                "SSE worker cancellation is still pending",
            ));
        }
        self.shared.join_worker().map_err(|message| {
            OperationError::new(
                OperationErrorCode::OperationDriverFailed,
                "http::sse",
                message,
            )
        })
    }

    fn is_quiescent(&self) -> bool {
        self.shared.is_quiescent()
    }

    fn register_quiescence_waker(&mut self, cx: &Context<'_>) {
        self.shared.quiescence_waker.register(cx.waker());
    }

    fn poll_quiescent(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if self.shared.is_quiescent() {
            return Poll::Ready(());
        }
        self.shared.waker.register(cx.waker());
        self.shared.close_waker.register(cx.waker());
        self.shared.quiescence_waker.register(cx.waker());
        if self.shared.is_quiescent() {
            Poll::Ready(())
        } else {
            let _ = self.shared.try_join_finished();
            if self.shared.is_quiescent() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
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
    validate_request_header_budget(&request.headers, &context.config)?;

    let config = context.config.clone();
    let permit = context.into_permit();
    let (items, receiver) = mpsc::channel(SSE_CHANNEL_CAPACITY);
    let shared = Arc::new(SseShared {
        stopping: AtomicBool::new(false),
        cancel: Notify::new(),
        cancellation_reason: std::sync::Mutex::new(None),
        item_ack: Notify::new(),
        waker: AtomicWaker::new(),
        items,
        done: AtomicBool::new(false),
        thread_finished: AtomicBool::new(false),
        worker_lifecycle: std::sync::atomic::AtomicU8::new(SseWorkerLifecycle::NotStarted as u8),
        result: std::sync::Mutex::new(None),
        join_handle: std::sync::Mutex::new(None),
        close_waker: AtomicWaker::new(),
        quiescence_waker: AtomicWaker::new(),
        _permit: permit,
        rollback_finished: AtomicBool::new(false),
    });

    // The SSE stream itself is a typed scope resource. The underlying response
    // is owned by the stream worker and is closed after producer quiescence.
    let sse_token = vm
        .host_context()
        .push_resource(SseStreamResource {
            shared: Arc::clone(&shared),
        })
        .map_err(|error| {
            VmError::HostError(format!("failed to push SSE child resource: {error}"))
        })?;
    let resource = sse_token.handle();
    let op = SseScopeOperation {
        shared: Arc::clone(&shared),
    };
    let scope_operation = match start_operation(vm, op) {
        Ok(operation) => operation,
        Err(error) => {
            return Err(rollback_sse_admission(
                vm,
                &shared,
                resource,
                None,
                VmError::HostError(format!("failed to start SSE operation: {error}")),
            ));
        }
    };

    let worker = Arc::new(SseWorker {
        config: config.clone(),
        request,
        deadline,
        shared: Arc::clone(&shared),
        items: Arc::new(AtomicUsize::new(0)),
        bytes_received: Arc::new(AtomicUsize::new(0)),
        status: std::sync::Mutex::new(None),
        headers: std::sync::Mutex::new(None),
        url: std::sync::Mutex::new(None),
    });
    let bytes_received = worker.bytes_received.clone();

    let join_handle = match spawn_worker("rustscript-sse-worker", {
        let worker_shared = Arc::clone(&shared);
        move || {
            worker_shared.mark_worker_running();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                worker.run();
            }));
            if let Err(panic) = result {
                worker_shared.publish(Err(VmError::HostError(format!(
                    "SSE worker panicked: {}",
                    worker_panic_message(&panic)
                ))));
            }
            worker_shared.mark_worker_finished();
            worker_shared.waker.wake();
        }
    }) {
        Ok(handle) => handle,
        Err(error) => {
            return Err(rollback_sse_admission(
                vm,
                &shared,
                resource,
                Some(scope_operation),
                VmError::HostError(format!("failed to start SSE worker: {error}")),
            ));
        }
    };
    shared.mark_worker_running();
    *shared
        .join_handle
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(join_handle);

    let driver = SseStreamDriver {
        shared: Arc::clone(&shared),
        receiver,
        status: 0,
        headers: Arc::new(VmMap::default()),
        url: String::new(),
        items: 0,
        bytes_received,
        deadline,
        scope_operation,
        resource,
        termination: None,
    };

    match vm.submit_callable_stream(callback, driver) {
        Ok(CallOutcome::Pending(op_id)) => Ok(HostCallResult::Pending(op_id)),
        Ok(_) => Err(rollback_sse_admission(
            vm,
            &shared,
            resource,
            Some(scope_operation),
            VmError::InvalidFrameState("callable stream admission returned a non-pending outcome"),
        )),
        Err(rejection) => Err(vm.rollback_rejected_callable_stream(rejection)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{
        FAIL_NEXT_WORKER_SPAWN, REJECT_NEXT_OPERATION_ADMISSION, SseEvent, SseParser,
        SseScopeOperation, SseShared, SseStreamResource, SseWorkerLifecycle,
        rollback_sse_admission, spawn_worker, start_operation,
    };
    use crate::vm::VmError;

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
    fn parser_enforces_line_item_and_total_limits_across_one_byte_chunks() {
        let exact = b"data: x\n\n";
        let exact_fragments: Vec<&[u8]> = exact.chunks(1).collect();
        assert_eq!(
            parse_fragments(&exact_fragments, 7, 1, exact.len()).unwrap(),
            vec![event("x", None, None, None)]
        );

        let line_over = b"data: abc\n\n";
        let line_fragments: Vec<&[u8]> = line_over.chunks(1).collect();
        assert!(
            parse_fragments(&line_fragments, 8, 16, 64)
                .unwrap_err()
                .contains("line")
        );

        let item_over = b"data: ab\ndata: c\n\n";
        let item_fragments: Vec<&[u8]> = item_over.chunks(1).collect();
        assert!(
            parse_fragments(&item_fragments, 16, 3, 64)
                .unwrap_err()
                .contains("item")
        );

        let total_over = b"data: ab\n\n";
        let total_fragments: Vec<&[u8]> = total_over.chunks(1).collect();
        assert!(
            parse_fragments(&total_fragments, 16, 16, total_over.len() - 1)
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

    #[test]
    fn admission_rollback_reclaims_workerless_sse_resource() {
        let mut vm = crate::vm::Vm::new(crate::vm::Program::new(
            Vec::new(),
            vec![crate::vm::OpCode::Ret as u8],
        ));
        let permit = crate::builtins::runtime::http::policy::ConnectionAdmission::new(1)
            .acquire()
            .expect("test permit");
        let (items, _receiver) = tokio::sync::mpsc::channel(1);
        let shared = std::sync::Arc::new(SseShared {
            stopping: AtomicBool::new(false),
            cancel: tokio::sync::Notify::new(),
            cancellation_reason: std::sync::Mutex::new(None),
            item_ack: tokio::sync::Notify::new(),
            waker: futures_util::task::AtomicWaker::new(),
            items,
            done: AtomicBool::new(false),
            thread_finished: AtomicBool::new(false),
            worker_lifecycle: std::sync::atomic::AtomicU8::new(
                SseWorkerLifecycle::NotStarted as u8,
            ),
            result: std::sync::Mutex::new(None),
            join_handle: std::sync::Mutex::new(None),
            close_waker: futures_util::task::AtomicWaker::new(),
            quiescence_waker: futures_util::task::AtomicWaker::new(),
            _permit: permit,
            rollback_finished: AtomicBool::new(false),
        });
        let token = vm
            .execution_scope()
            .push_resource(SseStreamResource {
                shared: std::sync::Arc::clone(&shared),
            })
            .expect("SSE resource");
        let primary = crate::vm::VmError::HostError("operation admission rejected".to_string());
        REJECT_NEXT_OPERATION_ADMISSION.store(true, Ordering::Release);
        let admission = start_operation(
            &mut vm,
            SseScopeOperation {
                shared: std::sync::Arc::clone(&shared),
            },
        );
        assert!(admission.is_err());

        let error = rollback_sse_admission(&mut vm, &shared, token.handle(), None, primary);

        assert!(error.to_string().contains("operation admission rejected"));
        assert_eq!(vm.execution_scope().resources().len(), 0);
        assert_eq!(
            shared.worker_lifecycle.load(Ordering::Acquire),
            SseWorkerLifecycle::Finished as u8
        );
        assert!(shared.done.load(Ordering::Acquire));
        assert!(shared.thread_finished.load(Ordering::Acquire));

        let repeated = rollback_sse_admission(
            &mut vm,
            &shared,
            token.handle(),
            None,
            VmError::HostError("repeated rollback".to_string()),
        );
        assert!(repeated.to_string().contains("repeated rollback"));
    }

    #[test]
    fn worker_spawn_abstraction_can_inject_a_builder_failure() {
        FAIL_NEXT_WORKER_SPAWN.store(true, Ordering::Release);
        let result = spawn_worker("injected-sse-worker", || {});
        let error = match result {
            Ok(handle) => {
                handle.join().expect("unexpected worker");
                panic!("spawn should have been rejected")
            }
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("injected SSE worker spawn failure")
        );
    }
}
