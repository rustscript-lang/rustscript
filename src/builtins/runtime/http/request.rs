use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::task::{Context, Poll};

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
    max_application_chunk: AtomicUsize,
}

impl ResponseReadObserver {
    fn mark_body(&self, limit: usize) {
        self.inner
            .remaining_body_bytes
            .store(limit, Ordering::Release);
        self.inner.phase.store(1, Ordering::Release);
        self.inner.transport_waker.wake();
    }

    fn body_is_admitted(&self) -> bool {
        self.inner.phase.load(Ordering::Acquire) == 1
    }

    fn register_transport_waker(&self, waker: &std::task::Waker) {
        self.inner.transport_waker.register(waker);
    }

    fn transport_read_limit(&self) -> usize {
        if self.inner.phase.load(Ordering::Acquire) == 0 {
            1
        } else {
            self.inner
                .remaining_body_bytes
                .load(Ordering::Acquire)
                .saturating_add(1)
        }
    }

    fn observe_transport_read(&self, bytes: usize) {
        if self.inner.phase.load(Ordering::Acquire) == 1 {
            self.inner.body_read_calls.fetch_add(1, Ordering::AcqRel);
            self.inner
                .max_body_transport_read
                .fetch_max(bytes, Ordering::AcqRel);
        }
    }

    fn observe_application_chunk(&self, bytes: usize) {
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
    pub(super) fn max_application_chunk(&self) -> usize {
        self.inner.max_application_chunk.load(Ordering::Acquire)
    }
}

struct ReadCapIo<T> {
    inner: T,
    observer: ResponseReadObserver,
    header_suffix: [u8; 4],
    header_bytes: usize,
    header_complete: bool,
}

impl<T> ReadCapIo<T> {
    fn new(inner: T, observer: ResponseReadObserver) -> Self {
        Self {
            inner,
            observer,
            header_suffix: [0; 4],
            header_bytes: 0,
            header_complete: false,
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
                    this.header_suffix.rotate_left(1);
                    this.header_suffix[3] = *byte;
                    this.header_bytes = this.header_bytes.saturating_add(1);
                    if this.header_bytes >= 4 && this.header_suffix == *b"\r\n\r\n" {
                        this.header_complete = true;
                    }
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
    with_deadline(deadline, execute_request(&context.config, &request)).await
}

pub(super) async fn execute_request(config: &HttpConfig, request: &HttpRequest) -> VmResult<VmMap> {
    execute_request_with_observer(config, request, ResponseReadObserver::default()).await
}

pub(super) async fn execute_request_with_observer(
    config: &HttpConfig,
    request: &HttpRequest,
    observer: ResponseReadObserver,
) -> VmResult<VmMap> {
    let mut method = request.method.clone();
    let mut url = request.url.clone();
    let mut body = request.body.clone();
    let mut headers = request.headers.clone();

    for redirect_index in 0..=config.max_redirects {
        let resolved = resolve_url(config, SchemeFamily::Http, &url).await?;
        let origin = url.origin();
        let mut response = send_request(
            config,
            &method,
            &url,
            &resolved,
            &headers,
            body.as_deref(),
            observer.clone(),
        )
        .await?;
        if response.status().is_redirection() {
            if redirect_index == config.max_redirects {
                return Err(VmError::HostError(
                    "HTTP redirect limit exceeded".to_string(),
                ));
            }
            let location = response
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
            if response.status() == hyper::StatusCode::SEE_OTHER
                || ((response.status() == hyper::StatusCode::MOVED_PERMANENTLY
                    || response.status() == hyper::StatusCode::FOUND)
                    && method != hyper::Method::GET
                    && method != hyper::Method::HEAD)
            {
                method = hyper::Method::GET;
                body = None;
            }
            url = next_url;
            continue;
        }

        reject_declared_oversize(&response, config.max_response_body_bytes)?;
        let response_headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                let value = value
                    .to_str()
                    .map(Value::string)
                    .unwrap_or_else(|_| Value::bytes(value.as_bytes().to_vec()));
                (Value::string(name.as_str()), value)
            })
            .collect::<Vec<_>>();
        let status = response.status();
        observer.mark_body(config.max_response_body_bytes);
        let mut bytes = Vec::with_capacity(
            response
                .body()
                .size_hint()
                .exact()
                .and_then(|length| usize::try_from(length).ok())
                .unwrap_or(0)
                .min(config.max_response_body_bytes),
        );
        while let Some(frame) = response.body_mut().frame().await {
            let frame = frame.map_err(|error| {
                VmError::HostError(format!("HTTP response read failed: {error}"))
            })?;
            let Ok(chunk) = frame.into_data() else {
                continue;
            };
            observer.observe_application_chunk(chunk.len());
            if bytes.len().saturating_add(chunk.len()) > config.max_response_body_bytes {
                return Err(response_body_limit_error());
            }
            bytes.extend_from_slice(&chunk);
        }
        return Ok(VmMap::from_entries(vec![
            (
                Value::string("status"),
                Value::Int(i64::from(status.as_u16())),
            ),
            (
                Value::string("headers"),
                Value::Map(std::sync::Arc::new(VmMap::from_entries(response_headers))),
            ),
            (Value::string("body"), Value::bytes(bytes)),
            (Value::string("url"), Value::string(url.as_str())),
        ]));
    }

    Err(VmError::HostError(
        "HTTP redirect processing failed".to_string(),
    ))
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

async fn send_request(
    config: &HttpConfig,
    method: &hyper::Method,
    url: &url::Url,
    resolved: &super::policy::ResolvedTarget,
    headers: &[(hyper::header::HeaderName, hyper::header::HeaderValue)],
    body: Option<&[u8]>,
    observer: ResponseReadObserver,
) -> VmResult<hyper::Response<hyper::body::Incoming>> {
    let stream = tokio::time::timeout(
        config.connect_timeout,
        tokio::net::TcpStream::connect(resolved.address),
    )
    .await
    .map_err(|_| VmError::HostError("HTTP request deadline exceeded".to_string()))?
    .map_err(|error| VmError::HostError(format!("HTTP request failed: {error}")))?;
    stream
        .set_nodelay(true)
        .map_err(|error| VmError::HostError(format!("HTTP request failed: {error}")))?;

    if url.scheme() == "https" {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls_config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let server_name = rustls::pki_types::ServerName::try_from(resolved.host.clone())
            .map_err(|_| VmError::HostError("HTTP TLS server name is invalid".to_string()))?;
        let stream = tokio_rustls::TlsConnector::from(Arc::new(tls_config))
            .connect(server_name, stream)
            .await
            .map_err(|error| VmError::HostError(format!("HTTP request failed: {error}")))?;
        send_over_io(method, url, headers, body, ReadCapIo::new(stream, observer)).await
    } else {
        send_over_io(method, url, headers, body, ReadCapIo::new(stream, observer)).await
    }
}

async fn send_over_io<T>(
    method: &hyper::Method,
    url: &url::Url,
    headers: &[(hyper::header::HeaderName, hyper::header::HeaderValue)],
    body: Option<&[u8]>,
    io: ReadCapIo<T>,
) -> VmResult<hyper::Response<hyper::body::Incoming>>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut connection_builder = hyper::client::conn::http1::Builder::new();
    connection_builder.read_buf_exact_size(Some(8 * 1024));
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
    tokio::spawn(async move {
        let _ = connection.await;
    });
    sender
        .send_request(request)
        .await
        .map_err(|error| VmError::HostError(format!("HTTP request failed: {error}")))
}
