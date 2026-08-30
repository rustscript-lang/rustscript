use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

use futures_util::task::AtomicWaker;
use http_body_util::BodyExt;
use hyper::body::Body as _;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Notify;

use super::HttpRequestContext;
use super::config::HttpConfig;
use super::policy::{ConnectionPermit, SchemeFamily, request_deadline, resolve_url, with_deadline};
use crate::HostCallResult;
use crate::builtins::runtime::typed::{VmMap, VmMapHandle};
use crate::host_api::ResourceTypeKey;
use crate::vm::operation::{
    HostOperation, OperationCancelReason, OperationError, OperationErrorCode, OperationId,
    OperationOutcome, OperationResult, OperationSpec,
};
use crate::vm::resource::{
    CloseProgress, HostResource, ResourceCloseReason, ResourceError, ResourceErrorCode,
    ResourceResult,
};
use crate::vm::{CallReturn, Value, Vm, VmError, VmResult};

#[derive(Clone, Default)]
pub(super) struct ResponseReadObserver {
    inner: Arc<ResponseReadMetrics>,
}

#[derive(Default)]
struct ResponseReadMetrics {
    phase: AtomicU8,
    transport_waker: AtomicWaker,
    remaining_body_bytes: AtomicUsize,
}

impl ResponseReadObserver {
    fn mark_final_head(&self) {
        self.inner.phase.store(1, Ordering::Release);
    }

    pub(super) fn admit_body(&self, limit: usize) {
        self.inner
            .remaining_body_bytes
            .store(limit, Ordering::Release);
        self.inner.phase.store(2, Ordering::Release);
        self.inner.transport_waker.wake();
    }

    fn body_is_admitted(&self) -> bool {
        self.inner.phase.load(Ordering::Acquire) == 2
    }

    fn register_transport_waker(&self, waker: &std::task::Waker) {
        self.inner.transport_waker.register(waker);
    }

    fn transport_read_limit(&self) -> usize {
        if !self.body_is_admitted() {
            1
        } else {
            self.inner
                .remaining_body_bytes
                .load(Ordering::Acquire)
                .saturating_add(1)
        }
    }

    fn body_remaining(&self) -> usize {
        self.inner.remaining_body_bytes.load(Ordering::Acquire)
    }

    pub(super) fn observe_application_chunk(&self, bytes: usize) {
        let _ = self.inner.remaining_body_bytes.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |remaining| Some(remaining.saturating_sub(bytes)),
        );
    }
}

// Rustls accepts a 16 KiB TLS fragment plus at most 2 KiB of protocol
// expansion and the five-byte record header. Bounding the adapter below TLS
// makes raw socket reads explicit. Rustls may retain one such record after the
// final HTTP head; ReadCapIo still exposes only remaining application bytes
// plus one overflow sentinel to Hyper.
const TLS_MAX_WIRE_READ: usize = 16_384 + 2_048 + 5;
const HTTP_MAX_HEAD_BYTES: usize = 64 * 1024;

struct RawReadCapIo<T> {
    inner: T,
}

impl<T> RawReadCapIo<T> {
    fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for RawReadCapIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let mut bounded = buf.take(TLS_MAX_WIRE_READ);
        match Pin::new(&mut this.inner).poll_read(cx, &mut bounded) {
            Poll::Ready(Ok(())) => {
                let read = bounded.filled().len();
                let initialized = bounded.initialized().len();
                unsafe {
                    buf.assume_init(initialized);
                    buf.set_filled(before + read);
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for RawReadCapIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }
}

struct ReadCapIo<T> {
    inner: T,
    observer: ResponseReadObserver,
    header_suffix: [u8; 4],
    header_bytes: usize,
    head_total_bytes: usize,
    header_complete: bool,
    post_body_bytes: usize,
    status_prefix: [u8; 12],
    status_prefix_len: usize,
}

impl<T> ReadCapIo<T> {
    fn new(inner: T, observer: ResponseReadObserver) -> Self {
        Self {
            inner,
            observer,
            header_suffix: [0; 4],
            header_bytes: 0,
            head_total_bytes: 0,
            header_complete: false,
            post_body_bytes: 0,
            status_prefix: [0; 12],
            status_prefix_len: 0,
        }
    }

    fn observe_head_byte(&mut self, byte: u8) -> std::io::Result<()> {
        if self.header_complete {
            return Ok(());
        }
        if self.status_prefix_len < self.status_prefix.len() {
            self.status_prefix[self.status_prefix_len] = byte;
            self.status_prefix_len += 1;
        }
        self.header_suffix.rotate_left(1);
        self.header_suffix[3] = byte;
        self.head_total_bytes = self.head_total_bytes.checked_add(1).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "HTTP response head exceeds limit",
            )
        })?;
        self.header_bytes = self.header_bytes.checked_add(1).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "HTTP response head exceeds limit",
            )
        })?;
        if self.head_total_bytes > HTTP_MAX_HEAD_BYTES || self.header_bytes > HTTP_MAX_HEAD_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "HTTP response head exceeds limit",
            ));
        }
        if self.header_bytes < 4 || self.header_suffix != *b"\r\n\r\n" {
            return Ok(());
        }

        let status = std::str::from_utf8(&self.status_prefix[9..12])
            .ok()
            .and_then(|digits| digits.parse::<u16>().ok());
        if matches!(status, Some(100..=199)) && status != Some(101) {
            self.header_suffix = [0; 4];
            self.header_bytes = 0;
            self.status_prefix = [0; 12];
            self.status_prefix_len = 0;
        } else {
            self.header_complete = true;
            self.observer.mark_final_head();
        }
        Ok(())
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for ReadCapIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.header_complete && !this.observer.body_is_admitted() {
            this.observer.register_transport_waker(cx.waker());
            if !this.observer.body_is_admitted() {
                return Poll::Pending;
            }
        }
        let before = buf.filled().len();
        let post_body_phase = this.header_complete
            && this.observer.body_is_admitted()
            && this.observer.body_remaining() == 0;
        let read_limit = if post_body_phase {
            let remaining = HTTP_MAX_HEAD_BYTES.saturating_sub(this.post_body_bytes);
            if remaining == 0 {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "HTTP response trailers exceed limit",
                )));
            }
            remaining.min(1)
        } else {
            this.observer.transport_read_limit()
        };
        let mut bounded = buf.take(read_limit);
        match Pin::new(&mut this.inner).poll_read(cx, &mut bounded) {
            Poll::Ready(Ok(())) => {
                let read = bounded.filled().len();
                let initialized = bounded.initialized().len();
                if post_body_phase {
                    this.post_body_bytes =
                        this.post_body_bytes.checked_add(read).ok_or_else(|| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "HTTP response trailers exceed limit",
                            )
                        })?;
                }
                for byte in &bounded.filled()[..read] {
                    if let Err(error) = this.observe_head_byte(*byte) {
                        return Poll::Ready(Err(error));
                    }
                }
                unsafe {
                    buf.assume_init(initialized);
                    buf.set_filled(before + read);
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for ReadCapIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }
}

#[derive(Clone)]
pub(super) struct HttpRequest {
    pub(super) method: hyper::Method,
    pub(super) url: url::Url,
    pub(super) headers: Vec<(hyper::header::HeaderName, hyper::header::HeaderValue)>,
    pub(super) body: Option<Vec<u8>>,
}

/// Serialized request-header admission accounting.
///
/// The budget describes the caller-controlled header block, not transport
/// headers synthesized by Hyper. A field is counted as
/// `name + ": " + value + "\\r\\n"`, and the block's final `"\\r\\n"` is
/// included by [`RequestHeaderBudget::finish`]. All arithmetic is checked so
/// an oversized input is rejected before `HeaderName`/`HeaderValue`
/// conversion can allocate.
struct RequestHeaderBudget {
    max_count: usize,
    max_bytes: usize,
    count: usize,
    bytes: usize,
}

impl RequestHeaderBudget {
    const FIELD_OVERHEAD: usize = 4; // ": " + "\\r\\n"
    const BLOCK_TERMINATOR: usize = 2; // "\\r\\n"

    #[cfg(test)]
    fn new(max_count: usize, max_bytes: usize) -> Self {
        Self {
            max_count,
            max_bytes,
            count: 0,
            bytes: 0,
        }
    }

    fn from_config(config: &HttpConfig) -> Self {
        Self {
            max_count: config.max_request_header_count,
            max_bytes: config.max_request_header_bytes,
            count: 0,
            bytes: 0,
        }
    }

    fn admit(&mut self, name: &[u8], value: &[u8]) -> VmResult<()> {
        let count = self
            .count
            .checked_add(1)
            .filter(|count| *count <= self.max_count)
            .ok_or_else(|| {
                VmError::HostError("HTTP request header count exceeds limit".to_string())
            })?;
        let field_bytes = name
            .len()
            .checked_add(value.len())
            .and_then(|bytes| bytes.checked_add(Self::FIELD_OVERHEAD))
            .ok_or_else(|| {
                VmError::HostError("HTTP request header bytes exceed limit".to_string())
            })?;
        let bytes = self
            .bytes
            .checked_add(field_bytes)
            .filter(|bytes| *bytes <= self.max_bytes)
            .ok_or_else(|| {
                VmError::HostError("HTTP request header bytes exceed limit".to_string())
            })?;
        self.count = count;
        self.bytes = bytes;
        Ok(())
    }

    fn finish(&mut self) -> VmResult<()> {
        self.bytes = self
            .bytes
            .checked_add(Self::BLOCK_TERMINATOR)
            .filter(|bytes| *bytes <= self.max_bytes)
            .ok_or_else(|| {
                VmError::HostError("HTTP request header bytes exceed limit".to_string())
            })?;
        Ok(())
    }

    #[cfg(test)]
    fn count(&self) -> usize {
        self.count
    }

    #[cfg(test)]
    fn bytes(&self) -> usize {
        self.bytes
    }
}

pub(super) fn validate_request_header_budget(
    headers: &[(hyper::header::HeaderName, hyper::header::HeaderValue)],
    config: &HttpConfig,
) -> VmResult<()> {
    let mut budget = RequestHeaderBudget::from_config(config);
    for (name, value) in headers {
        budget.admit(name.as_str().as_bytes(), value.as_bytes())?;
    }
    budget.finish()
}

pub(super) fn parse_request(map: &VmMap, config: &HttpConfig) -> VmResult<HttpRequest> {
    let method = map_string(map, "method")?.to_ascii_uppercase();
    if !matches!(
        method.as_str(),
        "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
    ) {
        return Err(VmError::HostError(format!(
            "HTTP method '{method}' is not allowed"
        )));
    }
    let method = hyper::Method::from_bytes(method.as_bytes())
        .map_err(|_| VmError::HostError("invalid HTTP method".to_string()))?;
    let url = map_string(map, "url")?
        .parse::<url::Url>()
        .map_err(|error| VmError::HostError(format!("invalid HTTP URL: {error}")))?;

    let body = match map.get(&Value::string("body")) {
        None | Some(Value::Null) => None,
        Some(Value::Bytes(bytes)) => {
            if bytes.len() > config.max_request_body_bytes {
                return Err(VmError::HostError(
                    "HTTP request body exceeds limit".to_string(),
                ));
            }
            Some(bytes.as_ref().clone())
        }
        Some(Value::String(text)) => {
            if text.len() > config.max_request_body_bytes {
                return Err(VmError::HostError(
                    "HTTP request body exceeds limit".to_string(),
                ));
            }
            Some(text.as_bytes().to_vec())
        }
        Some(_) => return Err(VmError::TypeMismatch("HTTP request body")),
    };

    let mut headers = Vec::new();
    let mut header_budget = RequestHeaderBudget::from_config(config);
    if let Some(Value::Map(header_map)) = map.get(&Value::string("headers")) {
        for (key, value) in header_map.iter() {
            let Value::String(key) = key else {
                return Err(VmError::TypeMismatch("HTTP header name"));
            };
            let Value::String(value) = value else {
                return Err(VmError::TypeMismatch("HTTP header value"));
            };
            // Admit raw bytes before normalizing/converting either component.
            // This keeps a rejected value from triggering a HeaderValue copy.
            header_budget.admit(key.as_bytes(), value.as_bytes())?;
            if matches!(
                key.to_ascii_lowercase().as_str(),
                "host" | "content-length" | "transfer-encoding" | "connection"
            ) {
                return Err(VmError::HostError(format!(
                    "HTTP header '{key}' is managed by the client",
                )));
            }
            let name = hyper::header::HeaderName::from_bytes(key.as_bytes())
                .map_err(|_| VmError::HostError(format!("invalid HTTP header name '{key}'")))?;
            let value = hyper::header::HeaderValue::from_str(value).map_err(|_| {
                VmError::HostError(format!("invalid HTTP header value for '{key}'"))
            })?;
            headers.push((name, value));
        }
    } else if map.get(&Value::string("headers")).is_some() {
        return Err(VmError::TypeMismatch("HTTP headers"));
    }
    header_budget.finish()?;

    Ok(HttpRequest {
        method,
        url,
        headers,
        body,
    })
}

fn map_string(map: &VmMap, key: &str) -> VmResult<String> {
    match map.get(&Value::string(key)) {
        Some(Value::String(value)) => Ok(value.as_ref().clone()),
        Some(_) => Err(VmError::TypeMismatch("HTTP request string field")),
        None => Err(VmError::HostError(format!(
            "missing HTTP request field '{key}'"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Shared state for the buffered HTTP request lifecycle
// ---------------------------------------------------------------------------

/// Shared state that coordinates the buffered HTTP request worker thread,
/// the operation poller, and the resource close lifecycle.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerLifecycle {
    NotStarted = 0,
    Running = 1,
    Finished = 2,
}

struct BufferedRequestShared {
    /// Notified on cancel/close so the worker can break out of a blocking
    /// network read. Race-free: if notify_one() arrives before the worker
    /// starts waiting, the next notified() completes immediately.
    cancel: Notify,
    /// One-shot result from the worker thread.
    result: std::sync::Mutex<Option<VmResult<CallReturn>>>,
    /// Set by the worker after publishing `result`.
    done: std::sync::atomic::AtomicBool,
    /// Set by the spawned closure after the worker entry returns.
    thread_finished: std::sync::atomic::AtomicBool,
    /// Explicit worker lifecycle. `NotStarted` is also the only state from
    /// which workerless rollback may publish a terminal result.
    worker_lifecycle: std::sync::atomic::AtomicU8,
    /// Waker registered by a pending operation poll. `register` is followed by
    /// a result recheck by the operation driver.
    waker: AtomicWaker,
    /// The worker thread handle, taken during close to join.
    join_handle: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Waker registered by the close poll when the worker is still running.
    close_waker: AtomicWaker,
    /// Waker registered by the operation registry while waiting for worker quiescence.
    quiescence_waker: AtomicWaker,
    /// The connection permit, held until the shared state is dropped (after
    /// the worker exits and the resource is closed).
    _permit: ConnectionPermit,
    /// Set after a rollback has retired both the operation and resource. This
    /// makes repeated rollback calls no-ops without touching stale handles.
    rollback_finished: std::sync::atomic::AtomicBool,
}

impl BufferedRequestShared {
    fn mark_worker_running(&self) {
        let _ = self.worker_lifecycle.compare_exchange(
            WorkerLifecycle::NotStarted as u8,
            WorkerLifecycle::Running as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn mark_worker_finished(&self) {
        self.thread_finished.store(true, Ordering::Release);
        self.worker_lifecycle
            .store(WorkerLifecycle::Finished as u8, Ordering::Release);
        self.close_waker.wake();
        self.quiescence_waker.wake();
    }

    /// Publishes a terminal rollback result for a resource that never had a
    /// worker. The compare-exchange prevents this path from claiming a worker
    /// which successfully started between admission and rollback.
    fn terminalize_workerless(&self, result: VmResult<CallReturn>) -> bool {
        if self
            .worker_lifecycle
            .compare_exchange(
                WorkerLifecycle::NotStarted as u8,
                WorkerLifecycle::Finished as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        self.request_stop();
        self.publish(result);
        self.thread_finished.store(true, Ordering::Release);
        self.close_waker.wake();
        self.quiescence_waker.wake();
        true
    }

    fn request_stop(&self) {
        self.cancel.notify_one();
        self.waker.wake();
        self.close_waker.wake();
        self.quiescence_waker.wake();
    }

    fn publish(&self, result: VmResult<CallReturn>) {
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

    fn has_result(&self) -> bool {
        self.result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    fn try_join_finished(&self) -> ResourceResult<bool> {
        if self.worker_lifecycle.load(Ordering::Acquire) != WorkerLifecycle::Finished as u8
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
        handle.join().map(|_| true).map_err(|panic| {
            ResourceError::new(
                ResourceErrorCode::ResourceCleanupFailed,
                "http::request::resource",
                worker_panic_message(&panic),
            )
        })
    }

    fn is_quiescent(&self) -> bool {
        self.worker_lifecycle.load(Ordering::Acquire) == WorkerLifecycle::Finished as u8
            && self.done.load(Ordering::Acquire)
            && self.thread_finished.load(Ordering::Acquire)
            && self
                .join_handle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none()
    }
}

fn worker_panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else {
        "HTTP request worker thread panicked".to_string()
    }
}

#[cfg(test)]
static FAIL_NEXT_WORKER_SPAWN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

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

fn spawn_worker<F>(name: &str, function: F) -> std::io::Result<std::thread::JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    #[cfg(test)]
    if FAIL_NEXT_WORKER_SPAWN.swap(false, Ordering::AcqRel) {
        return Err(std::io::Error::other("injected HTTP worker spawn failure"));
    }
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(function)
}

// ---------------------------------------------------------------------------
// Generic scoped host resources and operations
// ---------------------------------------------------------------------------

/// An HTTP request being processed under the configured network policy.
///
/// The request resource is registered in the execution scope and associated
/// with the buffered HTTP operation. Its close is the terminal teardown;
/// the scope lifecycle closes the resource (and cancels the operation) on
/// reset/shutdown, ensuring the worker thread is retired.
pub struct HttpRequestResource {
    shared: Option<Arc<BufferedRequestShared>>,
}

impl HttpRequestResource {
    fn new(shared: Arc<BufferedRequestShared>) -> Self {
        Self {
            shared: Some(shared),
        }
    }
}

impl HostResource for HttpRequestResource {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        ResourceTypeKey::new("http.request").ok()
    }

    fn begin_close(&mut self, reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        let _ = reason;
        let Some(shared) = self.shared.as_ref() else {
            return Ok(CloseProgress::Ready);
        };
        // Notify the worker to stop promptly, even if it is blocked on a
        // network read. The operation's cancel also does this, but the
        // resource close is the authoritative teardown path.
        shared.request_stop();
        match shared.try_join_finished()? {
            true => Ok(CloseProgress::Ready),
            false => Ok(CloseProgress::Pending),
        }
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        let Some(shared) = self.shared.as_ref() else {
            return Poll::Ready(Ok(()));
        };
        match shared.try_join_finished() {
            Ok(true) => Poll::Ready(Ok(())),
            Ok(false) => {
                shared.close_waker.register(cx.waker());
                match shared.try_join_finished() {
                    Ok(true) => Poll::Ready(Ok(())),
                    Ok(false) => Poll::Pending,
                    Err(error) => Poll::Ready(Err(error)),
                }
            }
            Err(error) => Poll::Ready(Err(error)),
        }
    }
}

/// The open HTTP response body stream, used as the parent resource for SSE
/// reader children.
///
/// Closing it aborts the response stream (the child is closed first by the
/// generic child-first scope shutdown). The SSE reader is registered as a
/// child of this resource so the close order is deterministic: SSE reader
/// first, then the response stream parent.
pub struct HttpResponseResource;

impl HostResource for HttpResponseResource {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        ResourceTypeKey::new("http.response").ok()
    }

    fn begin_close(&mut self, reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        let _ = reason;
        Ok(CloseProgress::Ready)
    }
}

/// Driver for the *buffered* HTTP request operation: runs the request on a
/// worker thread and publishes the response map into a shared cell.
pub(super) struct HttpRequestOperation {
    shared: Arc<BufferedRequestShared>,
}

impl HttpRequestOperation {
    fn new(shared: Arc<BufferedRequestShared>) -> Self {
        Self { shared }
    }
}

impl HostOperation for HttpRequestOperation {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
        let result = self
            .shared
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match result.as_ref() {
            Some(Ok(_)) => Poll::Ready(Ok(())),
            Some(Err(error)) => Poll::Ready(Err(OperationError::new(
                OperationErrorCode::OperationDriverFailed,
                "http::client::request",
                error.to_string(),
            ))),
            None => {
                drop(result);
                self.shared.waker.register(cx.waker());
                let ready = self.shared.has_result();
                if ready { self.poll(cx) } else { Poll::Pending }
            }
        }
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

    fn cancel(&mut self, reason: OperationCancelReason) -> OperationResult<()> {
        let _ = reason;
        self.shared.request_stop();
        Ok(())
    }

    fn cancel_and_wait(&mut self, reason: OperationCancelReason) -> OperationResult<()> {
        self.cancel(reason)?;
        if !self.shared.is_quiescent() {
            return Err(OperationError::new(
                OperationErrorCode::OperationDriverFailed,
                "http::client::request",
                "HTTP request worker cancellation is still pending",
            ));
        }
        Ok(())
    }
}

impl BufferedRequestShared {
    /// Cancellation path used when admission has not yet installed an
    /// operation id. It synchronously joins the worker so rollback can close
    /// and reclaim the resource before returning the primary admission error.
    fn cancel_and_join(&self) -> VmResult<()> {
        self.request_stop();
        if !self.is_quiescent() {
            return Err(VmError::HostError(
                "HTTP request worker cancellation is still pending".to_string(),
            ));
        }
        Ok(())
    }
}

pub(super) fn host_boundary_error(error: crate::vm::HostContextError) -> VmError {
    VmError::HostError(error.to_string())
}

fn close_buffered_request_resource(
    vm: &mut Vm,
    handle: crate::vm::resource::ResourceHandle,
) -> VmResult<()> {
    match vm
        .host_context()
        .close_resource::<HttpRequestResource>(handle, ResourceCloseReason::Requested)
        .map_err(host_boundary_error)?
    {
        CloseProgress::Ready => Ok(()),
        CloseProgress::Pending => Err(VmError::HostError(
            "HTTP request resource close remained pending after worker quiescence".to_string(),
        )),
    }
}

fn preserve_cleanup_context(primary: VmError, cleanup: Vec<VmError>) -> VmError {
    if cleanup.is_empty() {
        return primary;
    }
    let mut message = primary.to_string();
    for error in cleanup {
        use std::fmt::Write as _;
        let _ = write!(message, "; cleanup failed: {error}");
    }
    VmError::HostError(message)
}

fn rollback_buffered_request(
    vm: &mut Vm,
    resource_handle: crate::vm::resource::ResourceHandle,
    shared: &Arc<BufferedRequestShared>,
    op_id: Option<OperationId>,
    primary: VmError,
) -> VmError {
    if shared.rollback_finished.load(Ordering::Acquire) {
        return primary;
    }
    let mut cleanup = Vec::new();
    let _ = shared.terminalize_workerless(Err(VmError::HostError(
        "HTTP request worker was not started".to_string(),
    )));
    if let Some(op_id) = op_id {
        vm.discard_scoped_operation_completion(op_id);
        if let Err(error) = vm
            .host_context()
            .abort_operation(op_id, OperationCancelReason::Requested)
            .map(|_| ())
            .map_err(host_boundary_error)
        {
            cleanup.push(error);
        }
    }
    if let Err(error) = shared.cancel_and_join() {
        cleanup.push(error);
    }
    if let Err(error) = close_buffered_request_resource(vm, resource_handle) {
        cleanup.push(error);
    }
    if cleanup.is_empty() {
        shared.rollback_finished.store(true, Ordering::Release);
    }
    preserve_cleanup_context(primary, cleanup)
}

// ---------------------------------------------------------------------------
// Buffered request
// ---------------------------------------------------------------------------

/// Performs one buffered HTTP request as a generic execution-scope operation.
pub(super) fn perform_buffered_request(
    vm: &mut Vm,
    request: VmMapHandle,
) -> VmResult<HostCallResult<VmMap>> {
    let (context, _) = HttpRequestContext::capture(vm, None, "HTTP")?;
    let config = context.config.clone();
    let permit = context.into_permit();
    let request = parse_request(&request, &config)?;
    let deadline = request_deadline(config.request_timeout)?;

    // Shared state that coordinates the worker thread, operation poll, and
    // resource close lifecycle. The permit is held here until the shared
    // state is dropped (after the worker exits and the resource is closed).
    let shared = Arc::new(BufferedRequestShared {
        cancel: Notify::new(),
        result: std::sync::Mutex::new(None),
        done: std::sync::atomic::AtomicBool::new(false),
        thread_finished: std::sync::atomic::AtomicBool::new(false),
        worker_lifecycle: std::sync::atomic::AtomicU8::new(WorkerLifecycle::NotStarted as u8),
        waker: AtomicWaker::new(),
        join_handle: std::sync::Mutex::new(None),
        close_waker: AtomicWaker::new(),
        quiescence_waker: AtomicWaker::new(),
        _permit: permit,
        rollback_finished: std::sync::atomic::AtomicBool::new(false),
    });

    // Register an HTTP request resource in the scope and associate the
    // operation with it. The scope lifecycle closes the resource (and
    // cancels the operation) on reset/shutdown.
    let request_resource = HttpRequestResource::new(Arc::clone(&shared));
    let resource_token = vm
        .host_context()
        .push_resource(request_resource)
        .map_err(host_boundary_error)?;
    let resource_handle = resource_token.handle();

    // Admit the operation before spawning the worker. Every later handoff
    // step can therefore use the operation id for deterministic rollback; a
    // failed spawn never leaves a workerless resource/operation pair behind.
    let op = HttpRequestOperation::new(Arc::clone(&shared));
    let op_id = match start_operation(vm, op) {
        Ok(op_id) => op_id,
        Err(error) => {
            return Err(rollback_buffered_request(
                vm,
                resource_handle,
                &shared,
                None,
                host_boundary_error(error),
            ));
        }
    };

    let pending_result = Arc::clone(&shared);
    if let Err(error) = vm.register_scoped_operation_completion(op_id, move |_vm, outcome| {
        let result = match outcome {
            OperationOutcome::Completed => pending_result
                .result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .unwrap_or_else(|| {
                    Err(VmError::HostError(
                        "HTTP request produced no result".to_string(),
                    ))
                }),
            // Cancellation is an internal teardown path. Resource cleanup is
            // still performed below, while callers that explicitly poll a
            // cancelled operation receive no guest value.
            OperationOutcome::Cancelled(_) => Ok(CallReturn::none()),
            OperationOutcome::Failed(error) => Err(VmError::HostError(error.to_string())),
        };
        let cleanup = close_buffered_request_resource(_vm, resource_handle);
        match (result, cleanup) {
            (Ok(values), Ok(())) => Ok(values),
            (Err(primary), Ok(())) => Err(primary),
            (Ok(_), Err(cleanup)) => Err(cleanup),
            (Err(primary), Err(cleanup)) => Err(preserve_cleanup_context(primary, vec![cleanup])),
        }
    }) {
        return Err(rollback_buffered_request(
            vm,
            resource_handle,
            &shared,
            Some(op_id),
            error,
        ));
    }

    // Run the request on a worker thread; the operation driver polls the
    // shared completion cell. The worker uses tokio::select! to respond
    // promptly to cancellation even while blocked on network I/O.
    let worker_config = config.clone();
    let worker_request = request.clone();
    let join_handle = match spawn_worker("rustscript-http-request", {
        let worker_shared = Arc::clone(&shared);
        move || {
            let worker_state = Arc::clone(&worker_shared);
            worker_state.mark_worker_running();
            let value = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match runtime_block_on(async {
                    tokio::select! {
                        biased;
                        _ = worker_shared.cancel.notified() => {
                            Err(VmError::HostError("HTTP request cancelled".to_string()))
                        }
                        result = with_deadline(
                            deadline,
                            execute_request_until(
                                &worker_config,
                                &worker_request,
                                ResponseReadObserver::default(),
                                deadline,
                                None,
                            ),
                        ) => {
                            result.map(|map| CallReturn::one(Value::Map(Arc::new(map))))
                        }
                    }
                }) {
                    Ok(value) => value,
                    Err(error) => Err(error),
                }
            })) {
                Ok(value) => value,
                Err(panic) => Err(VmError::HostError(format!(
                    "HTTP request worker panicked: {}",
                    worker_panic_message(&panic)
                ))),
            };
            worker_shared.publish(value);
            worker_state.mark_worker_finished();
            worker_state.waker.wake();
        }
    }) {
        Ok(join_handle) => join_handle,
        Err(error) => {
            return Err(rollback_buffered_request(
                vm,
                resource_handle,
                &shared,
                Some(op_id),
                VmError::HostError(format!("failed to start HTTP worker: {error}")),
            ));
        }
    };

    shared.mark_worker_running();

    // Store the join handle so the resource can join it during close.
    *shared
        .join_handle
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(join_handle);

    let raw = op_id.raw();
    Ok(HostCallResult::Pending(raw))
}

/// Builds a current-thread tokio runtime to run the blocking HTTP transport.
fn runtime_block_on<F: std::future::Future>(future: F) -> VmResult<F::Output> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            VmError::HostError(format!("HTTP worker runtime build failed: {error}"))
        })?;
    Ok(runtime.block_on(future))
}

async fn execute_request_until(
    config: &HttpConfig,
    request: &HttpRequest,
    observer: ResponseReadObserver,
    request_deadline: Instant,
    tls_config: Option<Arc<rustls::ClientConfig>>,
) -> VmResult<VmMap> {
    let mut method = request.method.clone();
    let mut url = request.url.clone();
    let mut body = request.body.clone();
    let mut headers = request.headers.clone();

    for redirect_index in 0..=config.max_redirects {
        let connect_deadline = request_deadline.min(
            Instant::now()
                .checked_add(config.connect_timeout)
                .ok_or_else(|| {
                    VmError::HostError("HTTP connect_timeout cannot form a deadline".to_string())
                })?,
        );
        let resolved = with_deadline(
            connect_deadline,
            resolve_url(config, SchemeFamily::Http, &url),
        )
        .await?;
        let mut response = send_request(
            &method,
            &url,
            &resolved,
            &headers,
            body.as_deref(),
            ConnectionStage {
                observer: observer.clone(),
                deadline: connect_deadline,
                response_deadline: None,
                tls_config: tls_config.clone(),
            },
        )
        .await?;
        validate_response_framing(response.response())?;
        if follows_location(response.response().status()) {
            if redirect_index == config.max_redirects {
                return Err(VmError::HostError(
                    "HTTP redirect limit exceeded".to_string(),
                ));
            }
            let location = response
                .response()
                .headers()
                .get(hyper::header::LOCATION)
                .ok_or_else(|| VmError::HostError("HTTP redirect has no location".to_string()))?
                .to_str()
                .map_err(|_| VmError::HostError("HTTP redirect location is invalid".to_string()))?
                .to_string();
            let next_url = url
                .join(&location)
                .map_err(|error| VmError::HostError(format!("invalid HTTP redirect: {error}")))?;
            super::policy::validate_url_policy(config, SchemeFamily::Http, &next_url)?;
            prepare_redirect(
                &url,
                &next_url,
                response.response().status(),
                &mut method,
                &mut body,
                &mut headers,
            );
            url = next_url;
            continue;
        }

        let status = response.response().status();
        let has_body = response_has_body(&method, status);
        if has_body {
            reject_declared_oversize(response.response(), config.max_response_body_bytes)?;
        }
        let response_headers = response_header_entries(response.response().headers());
        if !has_body {
            return Ok(response_map(status, response_headers, Vec::new(), &url));
        }
        observer.admit_body(config.max_response_body_bytes);
        let mut bytes = Vec::with_capacity(
            response
                .response()
                .body()
                .size_hint()
                .exact()
                .and_then(|length| usize::try_from(length).ok())
                .unwrap_or(0)
                .min(config.max_response_body_bytes),
        );
        while let Some(frame) = response.next_frame().await? {
            let Ok(chunk) = frame.into_data() else {
                continue;
            };
            observer.observe_application_chunk(chunk.len());
            if bytes.len().saturating_add(chunk.len()) > config.max_response_body_bytes {
                return Err(response_body_limit_error());
            }
            bytes.extend_from_slice(&chunk);
        }
        return Ok(response_map(status, response_headers, bytes, &url));
    }

    Err(VmError::HostError(
        "HTTP redirect processing failed".to_string(),
    ))
}

type BoxConnection =
    Pin<Box<dyn std::future::Future<Output = Result<(), hyper::Error>> + Send + 'static>>;

pub(super) struct OwnedResponse {
    connection: Option<BoxConnection>,
    response: hyper::Response<hyper::body::Incoming>,
}

impl OwnedResponse {
    pub(super) fn response(&self) -> &hyper::Response<hyper::body::Incoming> {
        &self.response
    }

    pub(super) async fn next_frame(
        &mut self,
    ) -> VmResult<Option<hyper::body::Frame<hyper::body::Bytes>>> {
        enum Progress {
            Frame(Option<Result<hyper::body::Frame<hyper::body::Bytes>, hyper::Error>>),
            Connection(Result<(), hyper::Error>),
        }

        loop {
            let Some(connection) = self.connection.as_mut() else {
                let frame =
                    self.response
                        .body_mut()
                        .frame()
                        .await
                        .transpose()
                        .map_err(|error| {
                            VmError::HostError(format!("HTTP response read failed: {error}"))
                        })?;
                if let Some(frame) = &frame {
                    validate_response_frame(frame)?;
                }
                return Ok(frame);
            };
            let progress = tokio::select! {
                biased;
                frame = self.response.body_mut().frame() => Progress::Frame(frame),
                result = connection.as_mut() => Progress::Connection(result),
            };
            match progress {
                Progress::Frame(frame) => {
                    let frame = frame.transpose().map_err(|error| {
                        VmError::HostError(format!("HTTP response read failed: {error}"))
                    })?;
                    if let Some(frame) = &frame {
                        validate_response_frame(frame)?;
                    }
                    return Ok(frame);
                }
                Progress::Connection(Ok(())) => self.connection = None,
                Progress::Connection(Err(error)) => {
                    return Err(VmError::HostError(format!(
                        "HTTP connection failed: {error}"
                    )));
                }
            }
        }
    }
}

fn response_has_body(method: &hyper::Method, status: hyper::StatusCode) -> bool {
    *method != hyper::Method::HEAD
        && !status.is_informational()
        && status != hyper::StatusCode::NO_CONTENT
        && status != hyper::StatusCode::NOT_MODIFIED
}

fn follows_location(status: hyper::StatusCode) -> bool {
    matches!(
        status,
        hyper::StatusCode::MOVED_PERMANENTLY
            | hyper::StatusCode::FOUND
            | hyper::StatusCode::SEE_OTHER
            | hyper::StatusCode::TEMPORARY_REDIRECT
            | hyper::StatusCode::PERMANENT_REDIRECT
    )
}

fn is_safe_cross_origin_redirect_header(name: &hyper::header::HeaderName) -> bool {
    matches!(
        name,
        &hyper::header::ACCEPT | &hyper::header::ACCEPT_LANGUAGE | &hyper::header::ACCEPT_ENCODING
    )
}

fn is_body_header(name: &hyper::header::HeaderName) -> bool {
    matches!(
        name,
        &hyper::header::CONTENT_LENGTH
            | &hyper::header::TRANSFER_ENCODING
            | &hyper::header::CONTENT_TYPE
            | &hyper::header::CONTENT_ENCODING
            | &hyper::header::CONTENT_RANGE
            | &hyper::header::TRAILER
            | &hyper::header::TE
            | &hyper::header::EXPECT
    )
}

fn redirect_rewrites_to_get(status: hyper::StatusCode, method: &hyper::Method) -> bool {
    (status == hyper::StatusCode::SEE_OTHER
        && method != hyper::Method::GET
        && method != hyper::Method::HEAD)
        || ((status == hyper::StatusCode::MOVED_PERMANENTLY || status == hyper::StatusCode::FOUND)
            && method == hyper::Method::POST)
}

fn prepare_redirect(
    current_url: &url::Url,
    next_url: &url::Url,
    status: hyper::StatusCode,
    method: &mut hyper::Method,
    body: &mut Option<Vec<u8>>,
    headers: &mut Vec<(hyper::header::HeaderName, hyper::header::HeaderValue)>,
) {
    if current_url.origin() != next_url.origin() {
        headers.retain(|(name, _)| is_safe_cross_origin_redirect_header(name));
    }
    if redirect_rewrites_to_get(status, method) {
        *method = hyper::Method::GET;
        *body = None;
        headers.retain(|(name, _)| !is_body_header(name));
    }
}

pub(super) fn response_header_entries(headers: &hyper::HeaderMap) -> Vec<(Value, Value)> {
    headers
        .iter()
        .map(|(name, value)| {
            let value = value
                .to_str()
                .map(Value::string)
                .unwrap_or_else(|_| Value::bytes(value.as_bytes().to_vec()));
            (Value::string(name.as_str()), value)
        })
        .collect()
}

pub(super) async fn open_stream_response(
    config: &HttpConfig,
    request: &HttpRequest,
    observer: ResponseReadObserver,
    opening_deadline: Instant,
    opening_response_deadline: Instant,
) -> VmResult<(OwnedResponse, url::Url)> {
    let mut method = request.method.clone();
    let mut url = request.url.clone();
    let mut body = request.body.clone();
    let mut headers = request.headers.clone();
    for redirect_index in 0..=config.max_redirects {
        // Each hop may use the connect phase limit, but never beyond the one
        // opening deadline supplied by the SSE lifecycle.
        let connect_deadline =
            super::policy::phase_deadline(opening_deadline, config.connect_timeout);
        let resolved = with_deadline(
            connect_deadline,
            resolve_url(config, SchemeFamily::Http, &url),
        )
        .await?;
        let response = send_request(
            &method,
            &url,
            &resolved,
            &headers,
            body.as_deref(),
            ConnectionStage {
                observer: observer.clone(),
                deadline: connect_deadline,
                response_deadline: Some(opening_response_deadline),
                tls_config: None,
            },
        )
        .await?;
        validate_response_framing(response.response())?;
        if follows_location(response.response().status()) {
            if redirect_index == config.max_redirects {
                return Err(VmError::HostError(
                    "HTTP redirect limit exceeded".to_string(),
                ));
            }
            let location = response
                .response()
                .headers()
                .get(hyper::header::LOCATION)
                .ok_or_else(|| VmError::HostError("HTTP redirect has no location".to_string()))?
                .to_str()
                .map_err(|_| VmError::HostError("HTTP redirect location is invalid".to_string()))?
                .to_string();
            let next_url = url
                .join(&location)
                .map_err(|error| VmError::HostError(format!("invalid HTTP redirect: {error}")))?;
            super::policy::validate_url_policy(config, SchemeFamily::Http, &next_url)?;
            prepare_redirect(
                &url,
                &next_url,
                response.response().status(),
                &mut method,
                &mut body,
                &mut headers,
            );
            url = next_url;
            continue;
        }
        return Ok((response, url));
    }
    Err(VmError::HostError(
        "HTTP redirect processing failed".to_string(),
    ))
}

fn response_map(
    status: hyper::StatusCode,
    headers: Vec<(Value, Value)>,
    body: Vec<u8>,
    url: &url::Url,
) -> VmMap {
    VmMap::from_entries(vec![
        (
            Value::string("status"),
            Value::Int(i64::from(status.as_u16())),
        ),
        (
            Value::string("headers"),
            Value::Map(std::sync::Arc::new(VmMap::from_entries(headers))),
        ),
        (Value::string("body"), Value::bytes(body)),
        (Value::string("url"), Value::string(url.as_str())),
    ])
}

fn validate_response_framing(response: &hyper::Response<hyper::body::Incoming>) -> VmResult<()> {
    let headers = response.headers();
    let content_lengths: Vec<_> = headers
        .get_all(hyper::header::CONTENT_LENGTH)
        .iter()
        .collect();
    let transfer_encodings: Vec<_> = headers
        .get_all(hyper::header::TRANSFER_ENCODING)
        .iter()
        .collect();
    if !content_lengths.is_empty() && !transfer_encodings.is_empty() {
        return Err(VmError::HostError(
            "HTTP response has ambiguous transfer framing".to_string(),
        ));
    }
    if content_lengths.len() > 1 {
        return Err(VmError::HostError(
            "HTTP response has ambiguous Content-Length".to_string(),
        ));
    }
    let mut declared_length = None;
    for value in content_lengths {
        let length = value
            .to_str()
            .ok()
            .and_then(|text| text.parse::<u64>().ok())
            .ok_or_else(|| {
                VmError::HostError("HTTP response Content-Length is invalid".to_string())
            })?;
        if declared_length.is_some_and(|previous| previous != length) {
            return Err(VmError::HostError(
                "HTTP response has ambiguous Content-Length".to_string(),
            ));
        }
        declared_length = Some(length);
    }
    if !transfer_encodings.is_empty() {
        let mut codings = transfer_encodings
            .iter()
            .flat_map(|value| value.to_str().unwrap_or("").split(','))
            .map(str::trim)
            .filter(|coding| !coding.is_empty());
        if !codings
            .next()
            .is_some_and(|coding| coding.eq_ignore_ascii_case("chunked"))
            || codings.next().is_some()
        {
            return Err(VmError::HostError(
                "HTTP response has invalid Transfer-Encoding".to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_response_trailers(headers: &hyper::HeaderMap) -> VmResult<()> {
    let mut bytes = 0_usize;
    for (name, value) in headers {
        bytes = bytes
            .checked_add(name.as_str().len())
            .and_then(|bytes| bytes.checked_add(value.len()))
            .and_then(|bytes| bytes.checked_add(4))
            .ok_or_else(|| VmError::HostError("HTTP response trailers exceed limit".to_string()))?;
        if bytes > HTTP_MAX_HEAD_BYTES {
            return Err(VmError::HostError(
                "HTTP response trailers exceed limit".to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_response_frame(frame: &hyper::body::Frame<hyper::body::Bytes>) -> VmResult<()> {
    if let Some(trailers) = frame.trailers_ref() {
        validate_response_trailers(trailers)?;
    }
    Ok(())
}

fn response_body_limit_error() -> VmError {
    VmError::HostError("HTTP response body exceeds limit".to_string())
}

fn reject_declared_oversize(
    response: &hyper::Response<hyper::body::Incoming>,
    limit: usize,
) -> VmResult<()> {
    let Some(value) = response.headers().get(hyper::header::CONTENT_LENGTH) else {
        return Ok(());
    };
    let length = value
        .to_str()
        .ok()
        .and_then(|text| text.parse::<u64>().ok())
        .ok_or_else(|| VmError::HostError("HTTP response Content-Length is invalid".to_string()))?;
    if length > limit as u64 {
        return Err(response_body_limit_error());
    }
    Ok(())
}

struct ConnectionStage {
    observer: ResponseReadObserver,
    deadline: Instant,
    /// Bounds the response-header wait after the request is written. Streaming
    /// adapters pass one absolute opening deadline; buffered requests leave
    /// this `None` because their outer request deadline covers the whole call.
    response_deadline: Option<Instant>,
    tls_config: Option<Arc<rustls::ClientConfig>>,
}

async fn send_request(
    method: &hyper::Method,
    url: &url::Url,
    resolved: &super::policy::ResolvedTarget,
    headers: &[(hyper::header::HeaderName, hyper::header::HeaderValue)],
    body: Option<&[u8]>,
    stage: ConnectionStage,
) -> VmResult<OwnedResponse> {
    let ConnectionStage {
        observer,
        deadline: connect_deadline,
        response_deadline,
        tls_config,
    } = stage;
    let stream = with_deadline(connect_deadline, async {
        tokio::net::TcpStream::connect(resolved.address)
            .await
            .map_err(|error| VmError::HostError(format!("HTTP request failed: {error}")))
    })
    .await?;
    let peer = stream
        .peer_addr()
        .map_err(|error| VmError::HostError(format!("HTTP request failed: {error}")))?;
    if peer != resolved.address {
        return Err(VmError::HostError(
            "HTTP connected peer does not match the validated address".to_string(),
        ));
    }
    stream
        .set_nodelay(true)
        .map_err(|error| VmError::HostError(format!("HTTP request failed: {error}")))?;

    let raw = RawReadCapIo::new(stream);
    if url.scheme() == "https" {
        let mut tls_config = tls_config.map_or_else(
            || {
                let mut roots = rustls::RootCertStore::empty();
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                rustls::ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth()
            },
            Arc::unwrap_or_clone,
        );
        tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let server_name = rustls::pki_types::ServerName::try_from(resolved.host.clone())
            .map_err(|_| VmError::HostError("HTTP TLS server name is invalid".to_string()))?;
        let stream = with_deadline(connect_deadline, async {
            tokio_rustls::TlsConnector::from(Arc::new(tls_config))
                .connect(server_name, raw)
                .await
                .map_err(|error| VmError::HostError(format!("HTTP request failed: {error}")))
        })
        .await?;
        send_over_io(
            method,
            url,
            headers,
            body,
            ReadCapIo::new(stream, observer),
            response_deadline,
        )
        .await
    } else {
        send_over_io(
            method,
            url,
            headers,
            body,
            ReadCapIo::new(raw, observer),
            response_deadline,
        )
        .await
    }
}

async fn send_over_io<T>(
    method: &hyper::Method,
    url: &url::Url,
    headers: &[(hyper::header::HeaderName, hyper::header::HeaderValue)],
    body: Option<&[u8]>,
    io: ReadCapIo<T>,
    response_deadline: Option<Instant>,
) -> VmResult<OwnedResponse>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut connection_builder = hyper::client::conn::http1::Builder::new();
    connection_builder
        .read_buf_exact_size(Some(8 * 1024))
        .max_buf_size(HTTP_MAX_HEAD_BYTES * 2)
        .max_headers(100);
    let (mut sender, connection) = connection_builder
        .handshake(hyper_util::rt::TokioIo::new(io))
        .await
        .map_err(|error| VmError::HostError(format!("HTTP request failed: {error}")))?;

    let path_and_query = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };
    let mut builder = hyper::Request::builder()
        .method(method.clone())
        .uri(path_and_query)
        .header(
            hyper::header::HOST,
            &url[url::Position::BeforeHost..url::Position::AfterPort],
        );
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    let request_body = http_body_util::Full::new(hyper::body::Bytes::copy_from_slice(
        body.unwrap_or_default(),
    ));
    let request = builder
        .body(request_body)
        .map_err(|error| VmError::HostError(format!("HTTP request setup failed: {error}")))?;
    let mut connection: BoxConnection = Box::pin(connection);
    // The response wait (including the request write) is bounded by the
    // absolute opening deadline when one is supplied. That deadline was
    // captured before stream admission and is never recreated per redirect;
    // buffered requests use their outer request deadline instead.
    let send_response = async {
        let response = sender.send_request(request);
        tokio::pin!(response);
        let (response, connection) = {
            tokio::select! {
                biased;
                response = &mut response => (
                    response.map_err(|error| {
                        VmError::HostError(format!("HTTP request failed: {error}"))
                    })?,
                    Some(connection),
                ),
                connection_result = connection.as_mut() => {
                    let response_result = response.await;
                    let response = match (connection_result, response_result) {
                        (_, Ok(response)) => response,
                        (Ok(()), Err(error)) => {
                            return Err(VmError::HostError(format!(
                                "HTTP request failed: {error}"
                            )));
                        }
                        (Err(connection_error), Err(request_error)) => {
                            return Err(VmError::HostError(format!(
                                "HTTP connection failed before the response: {connection_error}; request failed: {request_error}"
                            )));
                        }
                    };
                    (response, None)
                }
            }
        };
        Ok::<_, VmError>((response, connection))
    };
    let (response, connection) = match response_deadline {
        Some(deadline) => with_deadline(deadline, send_response).await?,
        None => send_response.await?,
    };
    Ok(OwnedResponse {
        connection,
        response,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

    use super::{
        BufferedRequestShared, FAIL_NEXT_WORKER_SPAWN, HTTP_MAX_HEAD_BYTES, HttpRequestOperation,
        HttpRequestResource, REJECT_NEXT_OPERATION_ADMISSION, ReadCapIo, RequestHeaderBudget,
        ResponseReadObserver, WorkerLifecycle, parse_request, rollback_buffered_request,
        spawn_worker, start_operation, validate_response_trailers,
    };
    use crate::builtins::runtime::typed::VmMap;
    use crate::vm::{Value, VmError};

    fn empty_vm() -> crate::vm::Vm {
        crate::vm::Vm::new(crate::vm::Program::new(
            Vec::new(),
            vec![crate::vm::OpCode::Ret as u8],
        ))
    }

    fn buffered_shared() -> std::sync::Arc<BufferedRequestShared> {
        let permit = crate::builtins::runtime::http::policy::ConnectionAdmission::new(1)
            .acquire()
            .expect("test permit");
        std::sync::Arc::new(BufferedRequestShared {
            cancel: tokio::sync::Notify::new(),
            result: std::sync::Mutex::new(None),
            done: AtomicBool::new(false),
            thread_finished: AtomicBool::new(false),
            worker_lifecycle: AtomicU8::new(WorkerLifecycle::NotStarted as u8),
            waker: futures_util::task::AtomicWaker::new(),
            join_handle: std::sync::Mutex::new(None),
            close_waker: futures_util::task::AtomicWaker::new(),
            quiescence_waker: futures_util::task::AtomicWaker::new(),
            _permit: permit,
            rollback_finished: AtomicBool::new(false),
        })
    }

    fn request_with_headers(headers: Vec<(&str, &str)>) -> VmMap {
        let mut request = VmMap::new();
        request.insert(Value::string("method"), Value::string("GET"));
        request.insert(Value::string("url"), Value::string("http://example.test/"));
        request.insert(
            Value::string("headers"),
            Value::Map(std::sync::Arc::new(VmMap::from_entries(
                headers
                    .into_iter()
                    .map(|(name, value)| (Value::string(name), Value::string(value)))
                    .collect(),
            ))),
        );
        request
    }

    #[test]
    fn request_header_budget_counts_wire_overhead_at_exact_boundary() {
        let mut budget = RequestHeaderBudget::new(1, 8);
        budget
            .admit(b"x", b"y")
            .expect("one header line is four bytes");
        budget.finish().expect("the final CRLF is two bytes");
        assert_eq!(budget.count(), 1);
        assert_eq!(budget.bytes(), 8);
    }

    #[test]
    fn request_header_budget_rejects_over_limit_before_header_conversion() {
        let config = crate::builtins::runtime::http::HttpConfig {
            max_request_header_count: 1,
            max_request_header_bytes: 8,
            ..Default::default()
        };
        let error = match parse_request(&request_with_headers(vec![("x", "yy")]), &config) {
            Ok(_) => panic!("header line plus terminator exceeds eight bytes"),
            Err(error) => error,
        };
        assert!(matches!(error, VmError::HostError(message) if message.contains("header bytes")));
    }

    #[test]
    fn request_header_budget_rejects_many_tiny_headers_by_count_and_bytes() {
        let mut count_limited = RequestHeaderBudget::new(2, 1024);
        count_limited.admit(b"a", b"b").unwrap();
        count_limited.admit(b"c", b"d").unwrap();
        let error = count_limited.admit(b"e", b"f").unwrap_err();
        assert!(error.to_string().contains("header count"));

        let mut bytes_limited = RequestHeaderBudget::new(16, 13);
        bytes_limited.admit(b"a", b"b").unwrap();
        bytes_limited.admit(b"c", b"d").unwrap();
        let error = bytes_limited.finish().unwrap_err();
        assert!(error.to_string().contains("header bytes"));
    }

    #[test]
    fn response_trailer_budget_rejects_aggregate_without_per_field_overflow() {
        let mut headers = hyper::HeaderMap::new();
        let value = hyper::header::HeaderValue::from_static(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        for index in 0..1_100 {
            let name =
                hyper::header::HeaderName::from_bytes(format!("x-trailer-{index}").as_bytes())
                    .unwrap();
            headers.append(name, value.clone());
        }
        let error = validate_response_trailers(&headers).unwrap_err();
        assert!(error.to_string().contains("trailers"));
    }

    #[test]
    fn response_head_budget_accepts_exact_limit_and_rejects_one_byte_over() {
        fn head_with_size(size: usize) -> Vec<u8> {
            let prefix = b"HTTP/1.1 204 No Content\r\nX-Pad: ";
            let suffix = b"\r\n\r\n";
            let value_len = size - prefix.len() - suffix.len();
            let mut head = Vec::with_capacity(size);
            head.extend_from_slice(prefix);
            head.extend(std::iter::repeat_n(b'a', value_len));
            head.extend_from_slice(suffix);
            assert_eq!(head.len(), size);
            head
        }

        let exact = head_with_size(HTTP_MAX_HEAD_BYTES);
        let mut exact_io = ReadCapIo::new(tokio::io::empty(), ResponseReadObserver::default());
        for byte in exact {
            exact_io
                .observe_head_byte(byte)
                .expect("exact response head should be admitted");
        }
        assert!(exact_io.header_complete);

        let over = head_with_size(HTTP_MAX_HEAD_BYTES + 1);
        let mut over_io = ReadCapIo::new(tokio::io::empty(), ResponseReadObserver::default());
        let error = over
            .into_iter()
            .try_for_each(|byte| over_io.observe_head_byte(byte))
            .expect_err("one byte over the response-head limit must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn admission_rollback_reclaims_workerless_request_resource() {
        let mut vm = empty_vm();
        let shared = buffered_shared();
        let token = vm
            .execution_scope()
            .push_resource(HttpRequestResource::new(std::sync::Arc::clone(&shared)))
            .expect("request resource");
        let primary = VmError::HostError("operation admission rejected".to_string());
        REJECT_NEXT_OPERATION_ADMISSION.store(true, Ordering::Release);
        let admission = start_operation(&mut vm, HttpRequestOperation::new(Arc::clone(&shared)));
        assert!(admission.is_err());

        let error = rollback_buffered_request(&mut vm, token.handle(), &shared, None, primary);

        assert!(error.to_string().contains("operation admission rejected"));
        assert_eq!(vm.execution_scope().resources().len(), 0);
        assert_eq!(
            shared.worker_lifecycle.load(Ordering::Acquire),
            WorkerLifecycle::Finished as u8
        );
        assert!(shared.done.load(Ordering::Acquire));
        assert!(shared.thread_finished.load(Ordering::Acquire));

        let repeated = rollback_buffered_request(
            &mut vm,
            token.handle(),
            &shared,
            None,
            VmError::HostError("repeated rollback".to_string()),
        );
        assert!(repeated.to_string().contains("repeated rollback"));
    }

    #[test]
    fn worker_spawn_abstraction_can_inject_a_builder_failure() {
        FAIL_NEXT_WORKER_SPAWN.store(true, Ordering::Release);
        let result = spawn_worker("injected-http-worker", || {});
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
                .contains("injected HTTP worker spawn failure")
        );
    }
}
