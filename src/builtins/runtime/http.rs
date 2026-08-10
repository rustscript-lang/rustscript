#[cfg(feature = "async")]
use futures_util::StreamExt;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
#[cfg(feature = "async")]
use std::sync::atomic::Ordering;

#[cfg(feature = "async")]
use pd_host_function::pd_host_function;

#[cfg(feature = "async")]
use super::{VmMap, VmResult};
#[cfg(feature = "async")]
use crate::builtins::runtime::cancellation::{CancellationReason, CancellationToken};
#[cfg(feature = "async")]
use crate::vm::CaptureAsyncHostContext;
#[cfg(feature = "async")]
use crate::vm::Value;
use crate::vm::Vm;
#[cfg(feature = "async")]
use crate::vm::VmError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpConfig {
    pub allowed_schemes: Vec<String>,
    pub allowed_hosts: Vec<String>,
    pub allowed_ports: Vec<u16>,
    pub max_redirects: usize,
    pub max_request_body_bytes: usize,
    pub max_response_body_bytes: usize,
    pub connect_timeout: std::time::Duration,
    pub request_timeout: std::time::Duration,
    pub allow_private_ips: bool,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            allowed_schemes: vec!["https".to_string()],
            allowed_hosts: Vec::new(),
            allowed_ports: Vec::new(),
            max_redirects: 5,
            max_request_body_bytes: 1024 * 1024,
            max_response_body_bytes: 8 * 1024 * 1024,
            connect_timeout: std::time::Duration::from_secs(10),
            request_timeout: std::time::Duration::from_secs(30),
            allow_private_ips: false,
        }
    }
}

#[derive(Default)]
struct HttpHostState {
    #[cfg(feature = "async")]
    config: Option<HttpConfig>,
    max_in_flight: usize,
    in_flight: Arc<AtomicUsize>,
}

/// HTTP host configuration owned by the HTTP host implementation.
pub trait HttpHostExt {
    fn configure_http(&mut self, config: HttpConfig);
    fn set_http_max_in_flight(&mut self, max_in_flight: usize);
    fn http_max_in_flight(&self) -> usize;
    fn clear_http_configuration(&mut self);
    fn http_is_configured(&self) -> bool;
}

impl HttpHostExt for Vm {
    fn configure_http(&mut self, config: HttpConfig) {
        let (max_in_flight, in_flight) = self
            .host
            .host_function_state::<HttpHostState>()
            .map_or_else(
                || {
                    (
                        crate::builtins::runtime::cancellation::DEFAULT_MAX_PENDING_OPERATIONS,
                        Arc::new(AtomicUsize::new(0)),
                    )
                },
                |state| (state.max_in_flight, Arc::clone(&state.in_flight)),
            );
        self.host.set_host_function_state(HttpHostState {
            #[cfg(feature = "async")]
            config: Some(config),
            max_in_flight,
            in_flight,
        });
        #[cfg(not(feature = "async"))]
        let _ = config;
    }

    fn set_http_max_in_flight(&mut self, max_in_flight: usize) {
        if self.host.host_function_state::<HttpHostState>().is_none() {
            self.host.set_host_function_state(HttpHostState {
                #[cfg(feature = "async")]
                config: None,
                max_in_flight:
                    crate::builtins::runtime::cancellation::DEFAULT_MAX_PENDING_OPERATIONS,
                in_flight: Arc::new(AtomicUsize::new(0)),
            });
        }
        self.host
            .host_function_state_mut::<HttpHostState>()
            .expect("HTTP host state was inserted")
            .max_in_flight = max_in_flight;
    }

    fn http_max_in_flight(&self) -> usize {
        self.host.host_function_state::<HttpHostState>().map_or(
            crate::builtins::runtime::cancellation::DEFAULT_MAX_PENDING_OPERATIONS,
            |state| state.max_in_flight,
        )
    }

    fn clear_http_configuration(&mut self) {
        crate::builtins::runtime::cancel_operations_by_owner(
            self,
            crate::builtins::runtime::cancellation::OperationOwner::Http,
            crate::builtins::runtime::cancellation::CancellationReason::Requested,
        );
        self.host.remove_host_function_state::<HttpHostState>();
    }

    fn http_is_configured(&self) -> bool {
        #[cfg(feature = "async")]
        {
            self.host
                .host_function_state::<HttpHostState>()
                .and_then(|state| state.config.as_ref())
                .is_some()
        }
        #[cfg(not(feature = "async"))]
        false
    }
}

#[cfg(feature = "async")]
fn runtime_host_error(error: impl std::fmt::Display) -> VmError {
    VmError::HostError(error.to_string())
}

#[cfg(feature = "async")]
fn cancellation_vm_error(token: &CancellationToken) -> VmError {
    token
        .check()
        .map(|()| VmError::HostError("HTTP request was cancelled".to_string()))
        .unwrap_or_else(runtime_host_error)
}

#[cfg(feature = "async")]
pub(super) struct HttpRequestContext {
    config: HttpConfig,
    cancellation: CancellationToken,
    _permit: HttpInFlightPermit,
}

#[cfg(feature = "async")]
struct HttpInFlightPermit {
    active: Arc<AtomicUsize>,
}

#[cfg(feature = "async")]
impl HttpInFlightPermit {
    fn acquire(state: &HttpHostState) -> VmResult<Self> {
        let mut active = state.in_flight.load(Ordering::Acquire);
        loop {
            if active >= state.max_in_flight {
                return Err(VmError::HostError(format!(
                    "HTTP in-flight request limit of {} was reached",
                    state.max_in_flight
                )));
            }
            match state.in_flight.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(Self {
                        active: Arc::clone(&state.in_flight),
                    });
                }
                Err(observed) => active = observed,
            }
        }
    }
}

#[cfg(feature = "async")]
impl Drop for HttpInFlightPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(feature = "async")]
impl CaptureAsyncHostContext for HttpRequestContext {
    fn capture(vm: &mut Vm) -> VmResult<Self> {
        let state = vm
            .host
            .host_function_state::<HttpHostState>()
            .ok_or_else(|| VmError::HostError("HTTP host is not configured".to_string()))?;
        let config = state
            .config
            .clone()
            .ok_or_else(|| VmError::HostError("HTTP host is not configured".to_string()))?;
        let permit = HttpInFlightPermit::acquire(state)?;
        Ok(Self {
            config,
            cancellation: CancellationToken::root(),
            _permit: permit,
        })
    }
}

/// Starts an HTTP request under the VM's configured network policy.
///
/// The request map accepts `method`, `url`, optional `headers`, and optional `body`.
/// The response map contains `status`, `headers`, `body`, and the final `url`.
#[cfg(feature = "async")]
#[pd_host_function(name = "http::client::request")]
pub(super) async fn builtin_http_client_request(
    #[pd_host_context] context: HttpRequestContext,
    request: VmMap,
) -> VmResult<VmMap> {
    let request = parse_request(&request, &context.config)?;
    let deadline = std::time::Instant::now() + context.config.request_timeout;
    execute_request(&context.config, &request, &context.cancellation, deadline).await
}

#[cfg(feature = "async")]
struct HttpRequest {
    method: reqwest::Method,
    url: url::Url,
    headers: Vec<(reqwest::header::HeaderName, reqwest::header::HeaderValue)>,
    body: Option<Vec<u8>>,
}

#[cfg(feature = "async")]
fn parse_request(map: &VmMap, config: &HttpConfig) -> VmResult<HttpRequest> {
    let method = map_string(map, "method")?.to_ascii_uppercase();
    if !matches!(
        method.as_str(),
        "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
    ) {
        return Err(VmError::HostError(format!(
            "HTTP method '{method}' is not allowed"
        )));
    }
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| VmError::HostError("invalid HTTP method".to_string()))?;
    let url = map_string(map, "url")?
        .parse::<url::Url>()
        .map_err(|error| VmError::HostError(format!("invalid HTTP URL: {error}")))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(VmError::HostError(
            "HTTP URL userinfo is not allowed".to_string(),
        ));
    }
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
            let name = reqwest::header::HeaderName::from_bytes(key.as_bytes())
                .map_err(|_| VmError::HostError(format!("invalid HTTP header name '{key}'")))?;
            let value = reqwest::header::HeaderValue::from_str(value).map_err(|_| {
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

#[cfg(feature = "async")]
fn map_string(map: &VmMap, key: &str) -> VmResult<String> {
    match map.get(&Value::string(key)) {
        Some(Value::String(value)) => Ok(value.as_ref().clone()),
        Some(_) => Err(VmError::TypeMismatch("HTTP request string field")),
        None => Err(VmError::HostError(format!(
            "missing HTTP request field '{key}'"
        ))),
    }
}

#[cfg(feature = "async")]
fn validate_url_policy<'a>(config: &HttpConfig, url: &'a url::Url) -> VmResult<(&'a str, u16)> {
    let scheme = url.scheme().to_ascii_lowercase();
    if !config
        .allowed_schemes
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(&scheme))
    {
        return Err(VmError::HostError(format!(
            "HTTP URL scheme '{scheme}' is not allowed",
        )));
    }
    let host = url
        .host_str()
        .ok_or_else(|| VmError::HostError("HTTP URL has no host".to_string()))?;
    if !config
        .allowed_hosts
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(host))
    {
        return Err(VmError::HostError(
            "HTTP target host is not allowed".to_string(),
        ));
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| VmError::HostError("HTTP URL has no known port".to_string()))?;
    if !config.allowed_ports.contains(&port) {
        return Err(VmError::HostError(format!(
            "HTTP target port {port} is not allowed",
        )));
    }
    Ok((host, port))
}

#[cfg(all(feature = "async", test))]
fn validate_url(config: &HttpConfig, url: &url::Url) -> VmResult<Option<std::net::SocketAddr>> {
    let (host, port) = validate_url_policy(config, url)?;
    if config.allow_private_ips {
        return Ok(None);
    }

    if let Ok(host_ip) = host.parse::<std::net::IpAddr>() {
        validate_resolved_addresses(config, &[std::net::SocketAddr::new(host_ip, port)])?;
        return Ok(None);
    }

    use std::net::ToSocketAddrs;
    let addresses = (host, port)
        .to_socket_addrs()
        .map_err(|error| VmError::HostError(format!("HTTP host resolution failed: {error}")))?
        .collect::<Vec<_>>();
    validate_resolved_addresses(config, &addresses)?;
    Ok(addresses.first().copied())
}

#[cfg(feature = "async")]
async fn resolve_url(
    config: &HttpConfig,
    url: &url::Url,
    token: &CancellationToken,
    deadline: std::time::Instant,
) -> VmResult<Option<std::net::SocketAddr>> {
    token.check().map_err(runtime_host_error)?;
    let (host, port) = validate_url_policy(config, url)?;
    if let Ok(host_ip) = host.parse::<std::net::IpAddr>() {
        let address = std::net::SocketAddr::new(host_ip, port);
        validate_resolved_addresses(config, &[address])?;
        return Ok(Some(address));
    }

    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        token.cancel(CancellationReason::Deadline);
        return Err(cancellation_vm_error(token));
    }
    let addresses = tokio::time::timeout(remaining, tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| {
            token.cancel(CancellationReason::Deadline);
            cancellation_vm_error(token)
        })?
        .map_err(|error| VmError::HostError(format!("HTTP host resolution failed: {error}")))?
        .collect::<Vec<_>>();
    token.check().map_err(runtime_host_error)?;
    validate_resolved_addresses(config, &addresses)?;
    addresses
        .first()
        .copied()
        .map(Some)
        .ok_or_else(|| VmError::HostError("HTTP target resolves to a restricted IP".to_string()))
}

#[cfg(feature = "async")]
fn validate_resolved_addresses(
    config: &HttpConfig,
    addresses: &[std::net::SocketAddr],
) -> VmResult<()> {
    if addresses.is_empty()
        || (!config.allow_private_ips
            && addresses
                .iter()
                .any(|address| is_restricted_ip(address.ip())))
    {
        return Err(VmError::HostError(
            "HTTP target resolves to a restricted IP".to_string(),
        ));
    }
    Ok(())
}

#[cfg(feature = "async")]
fn is_restricted_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let octets = ip.octets();
            matches!(octets[0], 0 | 10 | 127)
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 169 && octets[1] == 254)
                || (octets[0] == 172 && (16..=31).contains(&octets[1]))
                || (octets[0] == 192
                    && matches!(
                        (octets[1], octets[2]),
                        (0, 0) | (0, 2) | (31, 196) | (52, 193) | (88, 99) | (168, _) | (175, 48)
                    ))
                || (octets[0] == 198
                    && ((18..=19).contains(&octets[1]) || (octets[1] == 51 && octets[2] == 100)))
                || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
                || octets[0] >= 224
        }
        std::net::IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return is_restricted_ip(std::net::IpAddr::V4(mapped));
            }
            let segments = ip.segments();
            let outside_global_unicast = segments[0] & 0xe000 != 0x2000;
            let protocol_assignments = segments[0] == 0x2001 && segments[1] <= 0x01ff;
            let documentation = (segments[0] == 0x2001 && segments[1] == 0x0db8)
                || (segments[0] == 0x3fff && segments[1] & 0xf000 == 0);
            let six_to_four = segments[0] == 0x2002;
            let direct_delegation_as112 =
                segments[0] == 0x2620 && segments[1] == 0x004f && segments[2] == 0x8000;
            outside_global_unicast
                || protocol_assignments
                || documentation
                || six_to_four
                || direct_delegation_as112
        }
    }
}

#[cfg(feature = "async")]
async fn execute_request(
    config: &HttpConfig,
    request: &HttpRequest,
    token: &CancellationToken,
    deadline: std::time::Instant,
) -> VmResult<VmMap> {
    token.check().map_err(runtime_host_error)?;
    let mut method = request.method.clone();
    let mut url = request.url.clone();
    let mut body = request.body.clone();
    let mut headers = request.headers.clone();

    for redirect_index in 0..=config.max_redirects {
        token.check().map_err(runtime_host_error)?;
        let resolved_address = resolve_url(config, &url, token, deadline).await?;
        let host = url
            .host_str()
            .ok_or_else(|| VmError::HostError("HTTP URL has no host".to_string()))?;
        let mut client_builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(config.connect_timeout);
        if let Some(address) = resolved_address {
            client_builder = client_builder.resolve(host, address);
        }
        let client = client_builder
            .build()
            .map_err(|error| VmError::HostError(format!("HTTP client setup failed: {error}")))?;
        let origin = request.url.origin();
        let mut builder = client.request(method.clone(), url.clone());
        for (name, value) in &headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = &body {
            builder = builder.body(body.clone());
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            token.cancel(CancellationReason::Deadline);
            return Err(cancellation_vm_error(token));
        }
        let response = tokio::time::timeout(remaining, builder.send())
            .await
            .map_err(|_| {
                token.cancel(CancellationReason::Deadline);
                cancellation_vm_error(token)
            })?
            .map_err(|error| {
                if error.is_timeout() {
                    token.cancel(CancellationReason::Deadline);
                    cancellation_vm_error(token)
                } else {
                    VmError::HostError(format!("HTTP request failed: {error}"))
                }
            })?;
        token.check().map_err(runtime_host_error)?;
        if response.status().is_redirection() {
            if redirect_index == config.max_redirects {
                return Err(VmError::HostError(
                    "HTTP redirect limit exceeded".to_string(),
                ));
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .ok_or_else(|| VmError::HostError("HTTP redirect has no location".to_string()))?
                .to_str()
                .map_err(|_| VmError::HostError("HTTP redirect location is invalid".to_string()))?
                .to_string();
            let next_url = url
                .join(&location)
                .map_err(|error| VmError::HostError(format!("invalid HTTP redirect: {error}")))?;
            if next_url.origin() != origin {
                headers.retain(|(name, _)| {
                    name != reqwest::header::AUTHORIZATION && name != reqwest::header::COOKIE
                });
            }
            if response.status() == reqwest::StatusCode::SEE_OTHER
                || ((response.status() == reqwest::StatusCode::MOVED_PERMANENTLY
                    || response.status() == reqwest::StatusCode::FOUND)
                    && method != reqwest::Method::GET
                    && method != reqwest::Method::HEAD)
            {
                method = reqwest::Method::GET;
                body = None;
            }
            url = next_url;
            continue;
        }

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
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                token.cancel(CancellationReason::Deadline);
                return Err(cancellation_vm_error(token));
            }
            tokio::time::timeout(remaining, stream.next())
                .await
                .map_err(|_| {
                    token.cancel(CancellationReason::Deadline);
                    cancellation_vm_error(token)
                })?
        } {
            token.check().map_err(runtime_host_error)?;
            let chunk = chunk.map_err(|error| {
                VmError::HostError(format!("HTTP response read failed: {error}"))
            })?;
            if bytes.len().saturating_add(chunk.len()) > config.max_response_body_bytes {
                return Err(VmError::HostError(
                    "HTTP response body exceeds limit".to_string(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let response_map = VmMap::from_entries(vec![
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
        ]);
        return Ok(response_map);
    }

    Err(VmError::HostError(
        "HTTP redirect processing failed".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::HttpConfig;
    #[cfg(feature = "async")]
    use super::HttpHostExt;
    #[cfg(feature = "async")]
    use super::{
        CancellationReason, HttpRequest, VmMap, builtin_http_client_request, execute_request,
        is_restricted_ip, validate_resolved_addresses, validate_url,
    };
    #[cfg(feature = "async")]
    use crate::vm::{
        CallOutcome, CallReturn, HostAsyncBridge, HostFuture, HostOpId, Value, VmResult,
    };

    #[test]
    fn default_http_policy_denies_all_hosts() {
        let config = HttpConfig::default();
        assert_eq!(config.allowed_schemes, ["https"]);
        assert!(config.allowed_hosts.is_empty());
        assert!(config.allowed_ports.is_empty());
        assert!(!config.allow_private_ips);
    }

    #[cfg(feature = "async")]
    #[test]
    fn request_submits_future_to_host_driver_without_runtime_operation() {
        use std::sync::{Arc, Mutex};
        use std::task::{Context, Poll};

        struct RecordingBridge {
            submitted: Arc<Mutex<Option<(HostOpId, HostFuture)>>>,
        }

        impl HostAsyncBridge for RecordingBridge {
            fn submit_op(&mut self, op_id: HostOpId, future: HostFuture) -> VmResult<()> {
                *self.submitted.lock().expect("submission lock") = Some((op_id, future));
                Ok(())
            }

            fn poll_op(
                &mut self,
                _op_id: HostOpId,
                _cx: &mut Context<'_>,
            ) -> Poll<VmResult<CallReturn>> {
                Poll::Pending
            }
        }

        let submitted = Arc::new(Mutex::new(None));
        let mut vm = crate::vm::Vm::new(crate::vm::Program::new(Vec::new(), Vec::new()));
        vm.configure_http(HttpConfig::default());
        vm.set_async_bridge(Box::new(RecordingBridge {
            submitted: Arc::clone(&submitted),
        }));
        let args = [Value::Map(Arc::new(VmMap::default()))];

        let outcome = builtin_http_client_request(&mut vm, &args)
            .expect("HTTP async host call should submit");
        let CallOutcome::Pending(op_id) = outcome else {
            panic!("HTTP async host call should suspend");
        };
        assert_eq!(op_id, 1);
        assert_eq!(
            submitted
                .lock()
                .expect("submission lock")
                .as_ref()
                .map(|(submitted_id, _)| *submitted_id),
            Some(op_id)
        );
        assert_eq!(vm.host.runtime_operations.active_count(), 0);
    }

    #[cfg(feature = "async")]
    #[test]
    fn production_request_timeout_sets_structured_deadline_reason() {
        use std::time::{Duration, Instant};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let server = std::thread::spawn(move || {
            let (_socket, _) = listener.accept().expect("request should connect");
            std::thread::sleep(Duration::from_millis(100));
        });
        let config = HttpConfig {
            allowed_schemes: vec!["http".to_string()],
            allowed_hosts: vec!["127.0.0.1".to_string()],
            allowed_ports: vec![address.port()],
            allow_private_ips: true,
            connect_timeout: Duration::from_millis(50),
            request_timeout: Duration::from_millis(20),
            ..HttpConfig::default()
        };
        let request = HttpRequest {
            method: reqwest::Method::GET,
            url: format!("http://{address}/").parse().expect("valid URL"),
            headers: Vec::new(),
            body: None,
        };
        let token = crate::builtins::runtime::cancellation::CancellationToken::root();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build");

        runtime
            .block_on(execute_request(
                &config,
                &request,
                &token,
                Instant::now() + config.request_timeout,
            ))
            .expect_err("hanging server should time out");
        assert_eq!(token.reason(), Some(CancellationReason::Deadline));
        server.join().expect("server should exit");
    }

    #[cfg(feature = "async")]
    #[test]
    fn response_body_timeout_sets_structured_deadline_reason() {
        use std::io::{Read, Write};
        use std::time::{Duration, Instant};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("request should connect");
            let mut request = [0u8; 1024];
            let _ = socket
                .read(&mut request)
                .expect("request should be readable");
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n")
                .expect("headers should be written");
            socket.flush().expect("headers should flush");
            std::thread::sleep(Duration::from_millis(100));
        });
        let config = HttpConfig {
            allowed_schemes: vec!["http".to_string()],
            allowed_hosts: vec!["127.0.0.1".to_string()],
            allowed_ports: vec![address.port()],
            allow_private_ips: true,
            connect_timeout: Duration::from_millis(50),
            request_timeout: Duration::from_millis(20),
            ..HttpConfig::default()
        };
        let request = HttpRequest {
            method: reqwest::Method::GET,
            url: format!("http://{address}/").parse().expect("valid URL"),
            headers: Vec::new(),
            body: None,
        };
        let token = crate::builtins::runtime::cancellation::CancellationToken::root();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build");

        runtime
            .block_on(execute_request(
                &config,
                &request,
                &token,
                Instant::now() + config.request_timeout,
            ))
            .expect_err("stalled response body should time out");
        assert_eq!(token.reason(), Some(CancellationReason::Deadline));
        server.join().expect("server should exit");
    }

    #[cfg(feature = "async")]
    #[test]
    fn empty_port_allowlist_rejects_explicit_and_default_ports() {
        let config = HttpConfig {
            allowed_schemes: vec!["https".to_string()],
            allowed_hosts: vec!["example.com".to_string()],
            ..HttpConfig::default()
        };
        let explicit = "https://example.com:443/".parse().expect("valid URL");
        let default_port = "https://example.com/".parse().expect("valid URL");
        assert!(validate_url(&config, &explicit).is_err());
        assert!(validate_url(&config, &default_port).is_err());
    }

    #[cfg(feature = "async")]
    #[test]
    fn special_use_networks_and_mixed_dns_answers_are_restricted() {
        for address in [
            "0.1.2.3",
            "100.64.0.1",
            "192.0.0.8",
            "192.0.2.1",
            "192.31.196.1",
            "192.52.193.1",
            "192.88.99.1",
            "192.175.48.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "240.0.0.1",
            "100::1",
            "2001::1",
            "2001:db8::1",
            "2002::1",
            "2620:4f:8000::1",
            "3fff::1",
            "fc00::1",
        ] {
            assert!(
                is_restricted_ip(address.parse().expect("valid IP")),
                "{address} must be restricted"
            );
        }
        for address in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(
                !is_restricted_ip(address.parse().expect("valid IP")),
                "{address} must remain globally routable"
            );
        }

        let config = HttpConfig::default();
        let addresses = [
            "8.8.8.8:443".parse().expect("valid socket address"),
            "100.64.0.1:443".parse().expect("valid socket address"),
        ];
        assert!(validate_resolved_addresses(&config, &addresses).is_err());
    }

    #[cfg(feature = "async")]
    #[test]
    fn ipv4_mapped_ipv6_loopback_is_restricted() {
        assert!(is_restricted_ip(
            "::ffff:127.0.0.1".parse().expect("valid IP")
        ));
    }
}
