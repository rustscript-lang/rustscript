use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use http_body_util::Full;
use hyper::body::{Body as _, Bytes, Frame, Incoming};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use tower_service::Service;

use super::config::HttpConfig;
use super::policy::{PolicyResolver, SchemeFamily, phase_deadline, resolve_url, with_deadline};
use crate::builtins::runtime::typed::{VmMap, VmMapHandle};
use crate::vm::{Value, VmError, VmResult};

const HTTP_MAX_HEAD_BYTES: usize = 64 * 1024;
const HTTP_MAX_HEADERS: usize = 100;

type BoxConnectError = Box<dyn Error + Send + Sync>;

#[derive(Clone)]
pub(super) struct ConnectTimeout<C> {
    inner: C,
    timeout: std::time::Duration,
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
        let future = self.inner.call(destination);
        let timeout = self.timeout;
        Box::pin(async move {
            tokio::time::timeout(timeout, future)
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

/// Cloneable Hyper client retained in an embedding-owned worker resource.
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
    let connector = ConnectTimeout {
        inner: connector,
        timeout: config.connect_timeout,
    };
    let mut builder = Client::builder(TokioExecutor::new());
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
    let resolved = with_deadline(deadline, resolve_url(config, SchemeFamily::Http, url)).await?;
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

    let body = parse_request_body(map.get(&Value::string("body")), config)?;

    let mut headers = Vec::new();
    let mut header_budget = RequestHeaderBudget::from_config(config);
    match map.get(&Value::string("headers")) {
        None | Some(Value::Null) => {}
        Some(Value::Array(header_entries)) => {
            for entry in header_entries.iter() {
                let Value::Map(header) = entry else {
                    return Err(VmError::TypeMismatch("HTTP request header"));
                };
                reject_unexpected_fields(header, &["name", "value"], "HTTP request header")?;
                let key = required_string_field(header, "name", "HTTP header name")?;
                let value = required_string_field(header, "value", "HTTP header value")?;
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
                let value = hyper::header::HeaderValue::from_str(&value).map_err(|_| {
                    VmError::HostError(format!("invalid HTTP header value for '{key}'"))
                })?;
                headers.push((name, value));
            }
        }
        Some(_) => return Err(VmError::TypeMismatch("HTTP headers")),
    }
    header_budget.finish()?;

    Ok(HttpRequest {
        method,
        url,
        headers,
        body,
    })
}

fn parse_request_body(value: Option<&Value>, config: &HttpConfig) -> VmResult<Option<Vec<u8>>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let Value::Map(body) = value else {
        return Err(VmError::TypeMismatch("HTTP request body"));
    };
    reject_unexpected_fields(body, &["kind", "text", "bytes"], "HTTP request body")?;
    let kind = required_string_field(body, "kind", "HTTP request body kind")?;
    let payload = match kind.as_str() {
        "text" => {
            if body
                .get(&Value::string("bytes"))
                .is_some_and(|value| !matches!(value, Value::Null))
            {
                return Err(VmError::HostError(
                    "HTTP request body text variant cannot contain bytes".to_string(),
                ));
            }
            let Some(Value::String(text)) = body.get(&Value::string("text")) else {
                return Err(VmError::TypeMismatch("HTTP request body text payload"));
            };
            text.as_bytes()
        }
        "bytes" => {
            if body
                .get(&Value::string("text"))
                .is_some_and(|value| !matches!(value, Value::Null))
            {
                return Err(VmError::HostError(
                    "HTTP request body bytes variant cannot contain text".to_string(),
                ));
            }
            let Some(Value::Bytes(bytes)) = body.get(&Value::string("bytes")) else {
                return Err(VmError::TypeMismatch("HTTP request body bytes payload"));
            };
            bytes.as_ref()
        }
        _ => {
            return Err(VmError::HostError(
                "HTTP request body kind must be 'text' or 'bytes'".to_string(),
            ));
        }
    };
    if payload.len() > config.max_request_body_bytes {
        return Err(VmError::HostError(
            "HTTP request body exceeds limit".to_string(),
        ));
    }
    Ok(Some(payload.to_vec()))
}

fn reject_unexpected_fields(map: &VmMap, allowed: &[&str], context: &'static str) -> VmResult<()> {
    for (key, _) in map {
        let Value::String(key) = key else {
            return Err(VmError::TypeMismatch(context));
        };
        if !allowed.iter().any(|allowed| *allowed == key.as_str()) {
            return Err(VmError::HostError(format!(
                "{context} contains unknown field '{key}'"
            )));
        }
    }
    Ok(())
}

fn required_string_field(map: &VmMap, key: &str, context: &'static str) -> VmResult<String> {
    match map.get(&Value::string(key)) {
        Some(Value::String(value)) => Ok(value.as_ref().clone()),
        Some(_) => Err(VmError::TypeMismatch(context)),
        None => Err(VmError::HostError(format!("{context} is missing '{key}'"))),
    }
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

/// Parses and validates a buffered request before async-host submission.
pub(super) fn prepare_buffered_request(
    config: &HttpConfig,
    request: &VmMapHandle,
) -> VmResult<(HttpRequest, Instant)> {
    let request = parse_request(request, config)?;
    let deadline = super::policy::request_deadline(config.request_timeout)?;
    Ok((request, deadline))
}

/// Executes one bounded request with the shared Hyper client.
pub(super) async fn perform_buffered_request(
    client: &HttpClient,
    config: &HttpConfig,
    request: &HttpRequest,
    deadline: Instant,
) -> VmResult<VmMap> {
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
) -> VmResult<VmMap> {
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
        let mut response = send_request(client, &method, &url, &headers, body.as_deref()).await?;
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
) -> VmResult<(OwnedResponse, url::Url)> {
    let mut method = request.method.clone();
    let mut url = request.url.clone();
    let mut body = request.body.clone();
    let mut headers = request.headers.clone();
    for redirect_index in 0..=config.max_redirects {
        let connect_deadline = phase_deadline(opening_deadline, config.connect_timeout);
        validate_target_until(config, &url, connect_deadline).await?;
        let response = send_request(client, &method, &url, &headers, body.as_deref()).await?;
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
    let response = client.request(request).await.map_err(|error| {
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

pub(super) fn response_header_entries(headers: &hyper::HeaderMap) -> Vec<Value> {
    // HeaderMap iteration does not promise original cross-name wire order.
    // HeaderName::as_str() is normalized, and the stable sort retains the
    // HeaderMap-provided order among repeated values of the same name.
    let mut entries = headers.iter().collect::<Vec<_>>();
    entries.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));
    entries
        .into_iter()
        .map(|(name, value)| {
            let value = if let Ok(text) = value.to_str() {
                Value::Map(Arc::new(VmMap::from_entries(vec![
                    (Value::string("kind"), Value::string("text")),
                    (Value::string("text"), Value::string(text)),
                    (Value::string("bytes"), Value::Null),
                ])))
            } else {
                Value::Map(Arc::new(VmMap::from_entries(vec![
                    (Value::string("kind"), Value::string("bytes")),
                    (Value::string("text"), Value::Null),
                    (
                        Value::string("bytes"),
                        Value::bytes(value.as_bytes().to_vec()),
                    ),
                ])))
            };
            Value::Map(Arc::new(VmMap::from_entries(vec![
                (Value::string("name"), Value::string(name.as_str())),
                (Value::string("value"), value),
            ])))
        })
        .collect()
}

fn response_map(
    status: hyper::StatusCode,
    headers: Vec<Value>,
    body: Vec<u8>,
    url: &url::Url,
) -> VmMap {
    VmMap::from_entries(vec![
        (
            Value::string("status"),
            Value::Int(i64::from(status.as_u16())),
        ),
        (Value::string("headers"), Value::array(headers)),
        (Value::string("body"), Value::bytes(body)),
        (Value::string("url"), Value::string(url.as_str())),
    ])
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
mod tests {
    use std::sync::Arc;

    use super::{
        RequestHeaderBudget, parse_request, parse_request_body, validate_response_trailers,
    };
    use crate::builtins::runtime::typed::VmMap;
    use crate::vm::{Value, VmError};

    fn value_map(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
        Value::Map(Arc::new(VmMap::from_entries(
            entries
                .into_iter()
                .map(|(key, value)| (Value::string(key), value))
                .collect(),
        )))
    }

    fn request_with_body(body: Value) -> VmMap {
        VmMap::from_entries(vec![
            (Value::string("method"), Value::string("POST")),
            (Value::string("url"), Value::string("http://example.test/")),
            (Value::string("body"), body),
        ])
    }

    #[test]
    fn request_body_payload_checks_limit_before_copying() {
        let config = crate::builtins::runtime::http::HttpConfig {
            max_request_body_bytes: 6,
            ..Default::default()
        };
        let body = value_map([
            ("kind", Value::string("text")),
            ("text", Value::string("payload")),
        ]);
        let error = parse_request_body(Some(&body), &config).unwrap_err();
        assert!(
            matches!(error, VmError::HostError(message) if message == "HTTP request body exceeds limit")
        );
    }

    #[test]
    fn request_body_discriminator_rejects_invalid_variants() {
        let config = crate::builtins::runtime::http::HttpConfig::default();
        let body = value_map([
            ("kind", Value::string("text")),
            ("text", Value::string("payload")),
            ("bytes", Value::bytes(b"raw".to_vec())),
        ]);
        let error = parse_request(&request_with_body(body), &config).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("text variant cannot contain bytes")
        );
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
