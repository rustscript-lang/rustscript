use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

use futures_util::task::AtomicWaker;
use http_body_util::BodyExt;
use hyper::body::Body as _;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::HttpRequestContext;
use super::config::HttpConfig;
use super::policy::{SchemeFamily, request_deadline, resolve_url, with_deadline};
use crate::builtins::runtime::VmMap;
use crate::vm::{Value, VmError, VmResult};

#[derive(Clone, Default)]
pub(super) struct ResponseReadObserver {
    inner: Arc<ResponseReadMetrics>,
}

#[derive(Default)]
struct ResponseReadMetrics {
    phase: AtomicU8,
    transport_waker: AtomicWaker,
    remaining_body_bytes: AtomicUsize,
    body_read_calls: AtomicUsize,
    max_body_transport_read: AtomicUsize,
    max_raw_transport_read: AtomicUsize,
    max_application_chunk: AtomicUsize,
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

    /// Discards the body admission state of a redirect response before the
    /// next response is parsed. Redirect bodies are intentionally not exposed
    /// to the caller, so carrying phase `1` into the next response would make
    /// its body reader wait forever for an admission that can only happen after
    /// `execute_request_until` returns.
    fn discard_redirect_body(&self) {
        self.inner.remaining_body_bytes.store(0, Ordering::Release);
        self.inner.phase.store(0, Ordering::Release);
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

    fn observe_transport_read(&self, bytes: usize) {
        if self.body_is_admitted() {
            self.inner.body_read_calls.fetch_add(1, Ordering::AcqRel);
            self.inner
                .max_body_transport_read
                .fetch_max(bytes, Ordering::AcqRel);
        }
    }

    fn observe_raw_transport_read(&self, bytes: usize) {
        self.inner
            .max_raw_transport_read
            .fetch_max(bytes, Ordering::AcqRel);
    }

    pub(super) fn observe_application_chunk(&self, bytes: usize) {
        self.inner
            .max_application_chunk
            .fetch_max(bytes, Ordering::AcqRel);
        self.inner
            .remaining_body_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                Some(remaining.saturating_sub(bytes))
            })
            .expect("response body remaining-byte update cannot fail");
    }

    #[cfg(test)]
    pub(super) fn body_read_calls(&self) -> usize {
        self.inner.body_read_calls.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) fn max_body_transport_read(&self) -> usize {
        self.inner.max_body_transport_read.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) fn max_raw_transport_read(&self) -> usize {
        self.inner.max_raw_transport_read.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) fn max_application_chunk(&self) -> usize {
        self.inner.max_application_chunk.load(Ordering::Acquire)
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
    observer: ResponseReadObserver,
}

impl<T> RawReadCapIo<T> {
    fn new(inner: T, observer: ResponseReadObserver) -> Self {
        Self { inner, observer }
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
                this.observer.observe_raw_transport_read(read);
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
    header_complete: bool,
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
            header_complete: false,
            status_prefix: [0; 12],
            status_prefix_len: 0,
        }
    }

    fn observe_head_byte(&mut self, byte: u8) {
        if self.status_prefix_len < self.status_prefix.len() {
            self.status_prefix[self.status_prefix_len] = byte;
            self.status_prefix_len += 1;
        }
        self.header_suffix.rotate_left(1);
        self.header_suffix[3] = byte;
        self.header_bytes = self.header_bytes.saturating_add(1);
        if self.header_bytes < 4 || self.header_suffix != *b"\r\n\r\n" {
            return;
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
        let mut bounded = buf.take(this.observer.transport_read_limit());
        match Pin::new(&mut this.inner).poll_read(cx, &mut bounded) {
            Poll::Ready(Ok(())) => {
                let read = bounded.filled().len();
                let initialized = bounded.initialized().len();
                for byte in &bounded.filled()[..read] {
                    this.observe_head_byte(*byte);
                }
                unsafe {
                    buf.assume_init(initialized);
                    buf.set_filled(before + read);
                }
                this.observer.observe_transport_read(read);
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
    if let Some(Value::Map(header_map)) = map.get(&Value::string("headers")) {
        for (key, value) in header_map.iter() {
            let Value::String(key) = key else {
                return Err(VmError::TypeMismatch("HTTP header name"));
            };
            let Value::String(value) = value else {
                return Err(VmError::TypeMismatch("HTTP header value"));
            };
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

pub(super) async fn perform_buffered_request(
    context: HttpRequestContext,
    request: VmMap,
) -> VmResult<VmMap> {
    let request = parse_request(&request, &context.config)?;
    let deadline = request_deadline(context.config.request_timeout)?;
    with_deadline(
        deadline,
        execute_request_until(
            &context.config,
            &request,
            ResponseReadObserver::default(),
            deadline,
            None,
        ),
    )
    .await
}

#[cfg(test)]
pub(super) async fn execute_request(config: &HttpConfig, request: &HttpRequest) -> VmResult<VmMap> {
    let deadline = request_deadline(config.request_timeout)?;
    with_deadline(
        deadline,
        execute_request_until(
            config,
            request,
            ResponseReadObserver::default(),
            deadline,
            None,
        ),
    )
    .await
}

#[cfg(test)]
pub(super) async fn execute_request_with_observer(
    config: &HttpConfig,
    request: &HttpRequest,
    observer: ResponseReadObserver,
) -> VmResult<VmMap> {
    let deadline = request_deadline(config.request_timeout)?;
    with_deadline(
        deadline,
        execute_request_until(config, request, observer, deadline, None),
    )
    .await
}

#[cfg(test)]
pub(super) async fn execute_request_with_tls_config(
    config: &HttpConfig,
    request: &HttpRequest,
    observer: ResponseReadObserver,
    tls_config: Arc<rustls::ClientConfig>,
) -> VmResult<VmMap> {
    let deadline = request_deadline(config.request_timeout)?;
    with_deadline(
        deadline,
        execute_request_until(config, request, observer, deadline, Some(tls_config)),
    )
    .await
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
        let origin = url.origin();
        let mut response = send_request(
            &method,
            &url,
            &resolved,
            &headers,
            body.as_deref(),
            ConnectionStage {
                observer: observer.clone(),
                deadline: connect_deadline,
                tls_config: tls_config.clone(),
            },
        )
        .await?;
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
            if next_url.origin() != origin {
                headers.retain(|(name, _)| {
                    name != hyper::header::AUTHORIZATION && name != hyper::header::COOKIE
                });
            }
            rewrite_redirect_request(response.response().status(), &mut method, &mut body);
            drop(response);
            observer.discard_redirect_body();
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
                return self
                    .response
                    .body_mut()
                    .frame()
                    .await
                    .transpose()
                    .map_err(|error| {
                        VmError::HostError(format!("HTTP response read failed: {error}"))
                    });
            };
            let progress = tokio::select! {
                biased;
                frame = self.response.body_mut().frame() => Progress::Frame(frame),
                result = connection.as_mut() => Progress::Connection(result),
            };
            match progress {
                Progress::Frame(frame) => {
                    return frame.transpose().map_err(|error| {
                        VmError::HostError(format!("HTTP response read failed: {error}"))
                    });
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

fn rewrite_redirect_request(
    status: hyper::StatusCode,
    method: &mut hyper::Method,
    body: &mut Option<Vec<u8>>,
) {
    let rewrite_to_get = match status {
        hyper::StatusCode::SEE_OTHER => {
            *method != hyper::Method::GET && *method != hyper::Method::HEAD
        }
        hyper::StatusCode::MOVED_PERMANENTLY | hyper::StatusCode::FOUND => {
            *method == hyper::Method::POST
        }
        _ => false,
    };
    if rewrite_to_get {
        *method = hyper::Method::GET;
        *body = None;
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
    deadline: Option<Instant>,
) -> VmResult<(OwnedResponse, url::Url)> {
    let mut method = request.method.clone();
    let mut url = request.url.clone();
    let mut body = request.body.clone();
    let mut headers = request.headers.clone();
    for redirect_index in 0..=config.max_redirects {
        let mut connect_deadline = Instant::now()
            .checked_add(config.connect_timeout)
            .ok_or_else(|| {
                VmError::HostError("HTTP connect_timeout cannot form a deadline".to_string())
            })?;
        if let Some(deadline) = deadline {
            connect_deadline = connect_deadline.min(deadline);
        }
        let resolved = with_deadline(
            connect_deadline,
            resolve_url(config, SchemeFamily::Http, &url),
        )
        .await?;
        let origin = url.origin();
        let response = send_request(
            &method,
            &url,
            &resolved,
            &headers,
            body.as_deref(),
            ConnectionStage {
                observer: observer.clone(),
                deadline: connect_deadline,
                tls_config: None,
            },
        )
        .await?;
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
                .map_err(|_| VmError::HostError("HTTP redirect location is invalid".to_string()))?;
            let next_url = url
                .join(location)
                .map_err(|error| VmError::HostError(format!("invalid HTTP redirect: {error}")))?;
            super::policy::validate_url_policy(config, SchemeFamily::Http, &next_url)?;
            if next_url.origin() != origin {
                headers.retain(|(name, _)| {
                    name != hyper::header::AUTHORIZATION && name != hyper::header::COOKIE
                });
            }
            rewrite_redirect_request(response.response().status(), &mut method, &mut body);
            drop(response);
            observer.discard_redirect_body();
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
        tls_config,
    } = stage;
    let stream = with_deadline(connect_deadline, async {
        tokio::net::TcpStream::connect(resolved.address)
            .await
            .map_err(|error| VmError::HostError(format!("HTTP request failed: {error}")))
    })
    .await?;
    stream
        .set_nodelay(true)
        .map_err(|error| VmError::HostError(format!("HTTP request failed: {error}")))?;

    let raw = RawReadCapIo::new(stream, observer.clone());
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
        send_over_io(method, url, headers, body, ReadCapIo::new(stream, observer)).await
    } else {
        send_over_io(method, url, headers, body, ReadCapIo::new(raw, observer)).await
    }
}

#[cfg(test)]
pub(super) struct PendingConnectionTest {
    pub(super) future: Pin<Box<dyn std::future::Future<Output = VmResult<VmMap>>>>,
}

#[cfg(test)]
pub(super) fn pending_connection_test(
    io: tokio::io::DuplexStream,
    url: url::Url,
) -> PendingConnectionTest {
    let request = HttpRequest {
        method: hyper::Method::GET,
        url,
        headers: Vec::new(),
        body: None,
    };
    let observer = ResponseReadObserver::default();
    PendingConnectionTest {
        future: Box::pin(async move {
            let mut response = send_over_io(
                &request.method,
                &request.url,
                &request.headers,
                None,
                ReadCapIo::new(RawReadCapIo::new(io, observer.clone()), observer.clone()),
            )
            .await?;
            observer.admit_body(1024);
            while response.next_frame().await?.is_some() {}
            Ok(VmMap::default())
        }),
    }
}

async fn send_over_io<T>(
    method: &hyper::Method,
    url: &url::Url,
    headers: &[(hyper::header::HeaderName, hyper::header::HeaderValue)],
    body: Option<&[u8]>,
    io: ReadCapIo<T>,
) -> VmResult<OwnedResponse>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut connection_builder = hyper::client::conn::http1::Builder::new();
    connection_builder
        .read_buf_exact_size(Some(8 * 1024))
        .max_buf_size(HTTP_MAX_HEAD_BYTES)
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
    let (response, connection) = {
        let response = sender.send_request(request);
        tokio::pin!(response);
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
    Ok(OwnedResponse {
        connection,
        response,
    })
}
