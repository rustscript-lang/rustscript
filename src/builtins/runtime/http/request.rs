use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use http_body_util::Full;
use hyper::body::{Body as _, Bytes, Frame, Incoming};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use tower_service::Service;

use super::config::HttpConfig;
use super::policy::{PolicyResolver, SchemeFamily, phase_deadline, resolve_url, with_deadline};
use super::resources::{HttpHeaders, HttpRequest as RequestBuilder, HttpResponse};
use crate::vm::{VmError, VmResult};

const HTTP_MAX_HEAD_BYTES: usize = 64 * 1024;
const HTTP_MAX_HEADERS: usize = 100;

type BoxConnectError = Box<dyn Error + Send + Sync>;

tokio::task_local! {
    /// Immutable per-hop budget, including policy preflight. Never keyed by host.
    static CONNECT_DEADLINE: Instant;
}

/// Hyper can move a started connection race into its executor when pool checkout
/// wins (hyper-util 0.1.20, legacy/client.rs). Capture at submission, not polling:
/// connector readiness/call may then run on another task. Dispatcher tasks carry
/// context only; response headers and bodies are not timed by this deadline.
#[derive(Clone, Copy)]
struct HttpExecutor;

impl<F> hyper::rt::Executor<F> for HttpExecutor
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, future: F) {
        let deadline = CONNECT_DEADLINE.try_with(|deadline| *deadline).ok();
        tokio::spawn(async move {
            match deadline {
                Some(deadline) => CONNECT_DEADLINE.scope(deadline, future).await,
                None => future.await,
            }
        });
    }
}

#[derive(Clone)]
pub(super) struct ConnectTimeout<C> {
    inner: C,
}

impl<C> Service<hyper::Uri> for ConnectTimeout<C>
where
    C: Service<hyper::Uri> + Send,
    C::Future: Send + 'static,
    C::Response: Send + 'static,
    C::Error: Into<BoxConnectError>,
{
    type Response = C::Response;
    type Error = BoxConnectError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, destination: hyper::Uri) -> Self::Future {
        let Ok(deadline) = CONNECT_DEADLINE.try_with(|deadline| *deadline) else {
            // Missing propagation must fail closed, never restart a full budget.
            return Box::pin(async {
                Err(BoxConnectError::from(std::io::Error::other(
                    "HTTP connect phase deadline context missing",
                )))
            });
        };
        let future = self.inner.call(destination);
        #[cfg(test)]
        let future = deadline_tests::delay_connect(future);
        Box::pin(async move {
            tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), future)
                .await
                .map_err(|_| {
                    BoxConnectError::from(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "HTTP connect phase deadline exceeded",
                    ))
                })?
                .map_err(Into::into)
        })
    }
}

type HttpsPolicyConnector = ConnectTimeout<HttpsConnector<HttpConnector<PolicyResolver>>>;

/// Cloneable Hyper client retained in per-VM HTTP module state.
pub(super) type HttpClient = Client<HttpsPolicyConnector, Full<Bytes>>;

pub(super) fn build_client(config: &HttpConfig) -> HttpClient {
    let mut http = HttpConnector::new_with_resolver(PolicyResolver::new(config));
    http.enforce_http(false);
    http.set_connect_timeout(Some(config.connect_timeout));
    http.set_nodelay(true);
    let connector = HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
    let connector = ConnectTimeout { inner: connector };
    let mut builder = Client::builder(HttpExecutor);
    builder
        .http1_max_buf_size(HTTP_MAX_HEAD_BYTES)
        .http1_max_headers(HTTP_MAX_HEADERS);
    builder.build(connector)
}

async fn validate_target_until(
    config: &HttpConfig,
    url: &url::Url,
    deadline: Instant,
) -> VmResult<()> {
    let resolved = with_deadline(deadline, async {
        #[cfg(test)]
        deadline_tests::delay_preflight().await;
        resolve_url(config, SchemeFamily::Http, url).await
    })
    .await?;
    debug_assert_eq!(resolved.host, url.host_str().unwrap_or_default());
    debug_assert_eq!(
        resolved.address.port(),
        url.port_or_known_default().unwrap_or(0)
    );
    Ok(())
}

#[derive(Clone, Debug)]
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

pub(super) fn host_boundary_error(error: impl std::fmt::Display) -> VmError {
    VmError::HostError(error.to_string())
}

fn request_handle(args: &[crate::vm::Value]) -> VmResult<crate::vm::resource::ResourceHandle> {
    let raw = match args.first() {
        Some(crate::vm::Value::Int(raw)) => *raw,
        _ => return Err(VmError::TypeMismatch("HTTP request resource")),
    };
    crate::vm::resource::ResourceHandle::from_raw(raw as u64).map_err(host_boundary_error)
}

/// Inspect the typed builder only during synchronous admission. The macro
/// transfers its concrete ownership after successful context capture.
pub(super) fn with_request<T>(
    vm: &mut crate::vm::Vm,
    args: &[crate::vm::Value],
    inspect: impl FnOnce(&RequestBuilder) -> VmResult<T>,
) -> VmResult<T> {
    let handle = request_handle(args)?;
    let context = vm.host_context();
    let key = crate::host_api::ResourceTypeKey::new("http.request").expect("static key");
    let token = context
        .typed_resource_with_key::<RequestBuilder>(handle, &key)
        .map_err(host_boundary_error)?;
    let request = context.resource(&token).map_err(host_boundary_error)?;
    inspect(&request)
}

/// A rejected TakeOwned admission still consumes the builder. Successful
/// admission leaves that transfer to the macro's ResourceOwned extraction.
pub(super) fn finish_capture<T>(
    vm: &mut crate::vm::Vm,
    args: &[crate::vm::Value],
    result: VmResult<T>,
) -> VmResult<T> {
    match result {
        Ok(context) => Ok(context),
        Err(primary) => {
            let cleanup = request_handle(args).and_then(|handle| {
                vm.host_context()
                    .take_resource::<RequestBuilder>(handle)
                    .map_err(host_boundary_error)
            });
            match cleanup {
                Ok(_) => Err(primary),
                Err(cleanup) => Err(VmError::HostError(format!(
                    "{primary}; request admission cleanup failed: {cleanup}"
                ))),
            }
        }
    }
}

pub(super) fn validate_resource_request(
    request: &RequestBuilder,
    config: &HttpConfig,
) -> VmResult<()> {
    super::policy::validate_url_policy(config, SchemeFamily::Http, &request.url)?;
    if request.body.len() > config.max_request_body_bytes {
        return Err(VmError::HostError("HTTP request body exceeds limit".into()));
    }
    validate_request_header_budget(&request.headers, config)
}

/// Move an already admitted builder into transport state without copying its body.
pub(super) fn into_request(request: RequestBuilder) -> HttpRequest {
    HttpRequest {
        method: request.method,
        url: request.url,
        headers: request.headers,
        body: request.body.into_bytes(),
    }
}

/// Executes one bounded request with the shared Hyper client.
pub(super) async fn perform_buffered_request(
    client: &HttpClient,
    config: &HttpConfig,
    request: &HttpRequest,
    deadline: Instant,
) -> VmResult<HttpResponse> {
    with_deadline(
        deadline,
        execute_request_until(client, config, request, deadline),
    )
    .await
}

async fn execute_request_until(
    client: &HttpClient,
    config: &HttpConfig,
    request: &HttpRequest,
    request_deadline: Instant,
) -> VmResult<HttpResponse> {
    let mut method = request.method.clone();
    let mut url = request.url.clone();
    let mut body = request.body.clone();
    let mut headers = request.headers.clone();

    for redirect_index in 0..=config.max_redirects {
        let connect_deadline = phase_deadline(request_deadline, config.connect_timeout);
        // Validate every current DNS answer before dispatch. The connector's
        // policy resolver repeats the private-address check for the address
        // used by any newly opened pooled connection.
        validate_target_until(config, &url, connect_deadline).await?;
        let mut response = send_request(
            client,
            &method,
            &url,
            &headers,
            body.as_deref(),
            connect_deadline,
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
        let response_headers = HttpHeaders::from_map(response.response().headers());
        if !has_body {
            return Ok(response_map(status, response_headers, Vec::new(), &url));
        }
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

/// Response body owned by Hyper's client and pooled connection lifecycle.
pub(super) struct OwnedResponse {
    response: hyper::Response<Incoming>,
}

impl OwnedResponse {
    pub(super) fn response(&self) -> &hyper::Response<Incoming> {
        &self.response
    }

    pub(super) fn poll_next_frame(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<VmResult<Option<Frame<Bytes>>>> {
        match Pin::new(self.response.body_mut()).poll_frame(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(frame))) => {
                if let Err(error) = validate_response_frame(&frame) {
                    return Poll::Ready(Err(error));
                }
                Poll::Ready(Ok(Some(frame)))
            }
            Poll::Ready(Some(Err(error))) => Poll::Ready(Err(VmError::HostError(format!(
                "HTTP response read failed: {error}"
            )))),
            Poll::Ready(None) => Poll::Ready(Ok(None)),
        }
    }

    pub(super) async fn next_frame(&mut self) -> VmResult<Option<Frame<Bytes>>> {
        std::future::poll_fn(|cx| self.poll_next_frame(cx)).await
    }
}

pub(super) async fn open_stream_response(
    client: &HttpClient,
    config: &HttpConfig,
    request: &HttpRequest,
    opening_deadline: Instant,
    bytes_sent: &mut usize,
) -> VmResult<(OwnedResponse, url::Url)> {
    let mut method = request.method.clone();
    let mut url = request.url.clone();
    let mut body = request.body.clone();
    let mut headers = request.headers.clone();
    for redirect_index in 0..=config.max_redirects {
        let connect_deadline = phase_deadline(opening_deadline, config.connect_timeout);
        validate_target_until(config, &url, connect_deadline).await?;
        let response = send_request(
            client,
            &method,
            &url,
            &headers,
            body.as_deref(),
            connect_deadline,
        )
        .await?;
        *bytes_sent = bytes_sent.saturating_add(body.as_ref().map_or(0, Vec::len));
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

async fn send_request(
    client: &HttpClient,
    method: &hyper::Method,
    url: &url::Url,
    headers: &[(hyper::header::HeaderName, hyper::header::HeaderValue)],
    body: Option<&[u8]>,
    connect_deadline: Instant,
) -> VmResult<OwnedResponse> {
    let uri = url
        .as_str()
        .parse::<hyper::Uri>()
        .map_err(|error| VmError::HostError(format!("HTTP request setup failed: {error}")))?;
    let mut builder = hyper::Request::builder().method(method.clone()).uri(uri);
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    let request = builder
        .body(Full::new(Bytes::copy_from_slice(body.unwrap_or_default())))
        .map_err(|error| VmError::HostError(format!("HTTP request setup failed: {error}")))?;
    let response = CONNECT_DEADLINE
        .scope(connect_deadline, client.request(request))
        .await
        .map_err(|error| {
            let mut message = format!("HTTP request failed: {error}");
            let mut source = error.source();
            while let Some(error) = source {
                message.push_str(": ");
                message.push_str(&error.to_string());
                source = error.source();
            }
            VmError::HostError(message)
        })?;
    Ok(OwnedResponse { response })
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

fn response_map(
    status: hyper::StatusCode,
    headers: HttpHeaders,
    body: Vec<u8>,
    url: &url::Url,
) -> HttpResponse {
    HttpResponse {
        status: i64::from(status.as_u16()),
        headers,
        body,
        url: url.to_string(),
    }
}

fn validate_response_head_size(response: &hyper::Response<Incoming>) -> VmResult<()> {
    let version = match response.version() {
        hyper::Version::HTTP_10 => "HTTP/1.0",
        _ => "HTTP/1.1",
    };
    let mut bytes = version
        .len()
        .checked_add(1 + 3 + 2)
        .and_then(|bytes| {
            response
                .status()
                .canonical_reason()
                .map_or(Some(bytes), |reason| bytes.checked_add(1 + reason.len()))
        })
        .ok_or_else(|| VmError::HostError("HTTP response head exceeds limit".to_string()))?;
    for (name, value) in response.headers() {
        bytes = bytes
            .checked_add(name.as_str().len())
            .and_then(|bytes| bytes.checked_add(value.len()))
            .and_then(|bytes| bytes.checked_add(4))
            .ok_or_else(|| VmError::HostError("HTTP response head exceeds limit".to_string()))?;
    }
    bytes = bytes
        .checked_add(2)
        .ok_or_else(|| VmError::HostError("HTTP response head exceeds limit".to_string()))?;
    if bytes > HTTP_MAX_HEAD_BYTES {
        return Err(VmError::HostError(
            "HTTP response head exceeds limit".to_string(),
        ));
    }
    Ok(())
}

fn validate_response_framing(response: &hyper::Response<hyper::body::Incoming>) -> VmResult<()> {
    validate_response_head_size(response)?;
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

#[cfg(test)]
mod deadline_tests;

#[cfg(test)]
mod tests {
    use super::{RequestHeaderBudget, validate_resource_request, validate_response_trailers};
    use crate::builtins::runtime::http::{
        HttpConfig,
        resources::{HttpBody, HttpRequest},
    };

    #[test]
    fn resource_request_revalidates_both_body_variants_against_current_policy() {
        let limits = HttpConfig {
            allowed_schemes: vec!["http".into()],
            allowed_hosts: vec!["example.test".into()],
            allowed_ports: vec![80],
            ..Default::default()
        };
        let current = HttpConfig {
            max_request_body_bytes: 6,
            ..limits.clone()
        };
        for body in [
            HttpBody::Text("payload".into()),
            HttpBody::Bytes(b"payload".to_vec()),
        ] {
            let request = HttpRequest {
                method: hyper::Method::POST,
                url: "http://example.test/".parse().unwrap(),
                headers: vec![],
                body,
                limits: limits.clone(),
            };
            let error = validate_resource_request(&request, &current).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("HTTP request body exceeds limit"),
                "{error}"
            );
            assert!(validate_resource_request(&request, &limits).is_ok());
        }
    }

    #[test]
    fn request_header_budget_counts_wire_overhead_at_exact_boundary() {
        let mut budget = RequestHeaderBudget::new(1, 8);
        budget.admit(b"x", b"y").unwrap();
        budget.finish().unwrap();
        assert_eq!(budget.count(), 1);
        assert_eq!(budget.bytes(), 8);
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
}
