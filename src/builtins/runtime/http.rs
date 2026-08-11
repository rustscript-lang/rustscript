use std::task::{Context, Poll};

#[cfg(feature = "http-client")]
use futures_util::StreamExt;
#[cfg(feature = "http-client")]
use futures_util::future::{AbortHandle, Abortable};

use pd_host_function::pd_host_function;

use super::{HostCallResult, Vm, VmMap, VmResult};
#[cfg(feature = "http-client")]
use crate::builtins::runtime::cancellation::{
    CancellationReason, CancellationToken, OperationId, OperationOwner,
};
#[cfg(feature = "http-client")]
use crate::builtins::runtime::error::{RuntimeError, RuntimeErrorCode};
#[cfg(feature = "http-client")]
use crate::builtins::runtime::resource::ResourceTypeId;
#[cfg(feature = "http-client")]
use crate::vm::Value;
use crate::vm::{CallReturn, HostOpId, VmError};

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

#[cfg(feature = "http-client")]
struct HttpCompletion {
    result: VmResult<CallReturn>,
}

#[cfg(feature = "http-client")]
struct HttpRequestResource {
    receiver: futures_channel::oneshot::Receiver<HttpCompletion>,
}

pub(crate) struct HttpState {
    #[cfg(feature = "http-client")]
    config: Option<HttpConfig>,
    pub(crate) max_in_flight: usize,
}

impl Default for HttpState {
    fn default() -> Self {
        Self {
            #[cfg(feature = "http-client")]
            config: None,
            max_in_flight: crate::builtins::runtime::cancellation::DEFAULT_MAX_PENDING_OPERATIONS,
        }
    }
}

impl HttpState {
    pub(crate) fn reset_for_reuse(&mut self) {}

    pub(crate) fn configure(&mut self, config: HttpConfig) {
        #[cfg(feature = "http-client")]
        {
            self.config = Some(config);
        }
        #[cfg(not(feature = "http-client"))]
        let _ = config;
    }

    pub(crate) fn clear_configuration(&mut self) {
        #[cfg(feature = "http-client")]
        {
            self.config = None;
        }
    }

    #[cfg(all(test, feature = "http-client"))]
    pub(crate) fn configuration(&self) -> Option<&HttpConfig> {
        self.config.as_ref()
    }

    pub(crate) fn is_configured(&self) -> bool {
        #[cfg(feature = "http-client")]
        {
            self.config.is_some()
        }
        #[cfg(not(feature = "http-client"))]
        false
    }
}

#[cfg(feature = "http-client")]
fn schedule_request(vm: &mut Vm, config: HttpConfig, request: HttpRequest) -> VmResult<HostOpId> {
    let max_in_flight = vm.host.http_state.max_in_flight;
    if vm
        .host
        .runtime_operations
        .operations_by_owner(OperationOwner::Http)
        .len()
        >= max_in_flight
    {
        return Err(VmError::HostError(format!(
            "HTTP in-flight request limit of {} has been reached",
            max_in_flight
        )));
    }

    let deadline = std::time::Instant::now() + config.request_timeout;
    let (sender, receiver) = futures_channel::oneshot::channel();
    let (abort_handle, abort_registration) = AbortHandle::new_pair();
    let operation = vm
        .host
        .runtime_operations
        .start_owned(
            OperationOwner::Http,
            Some(&vm.run_ctx.cancellation),
            Some(deadline),
            Some(Box::new(move |_| {
                abort_handle.abort();
                Ok(())
            })),
        )
        .map_err(runtime_host_error)?;
    let operation_id = operation.id();
    let op_id = operation_id.raw();
    let token = operation.token();
    let worker_operation = operation.clone();
    let resource = match vm.host.runtime_resources.insert(
        ResourceTypeId::HTTP_REQUEST,
        HttpRequestResource { receiver },
    ) {
        Ok(resource) => resource,
        Err(error) => {
            let _ = vm
                .host
                .runtime_operations
                .cancel(operation_id, CancellationReason::ResourceClosed);
            return Err(runtime_host_error(error));
        }
    };
    operation.set_payload(resource);

    let thread_name = format!("rustscript-http-{op_id}");
    if let Err(error) = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(async move {
                    match Abortable::new(
                        execute_request(&config, &request, &token, deadline),
                        abort_registration,
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => cancellation_error(&token),
                    }
                }),
                Err(error) => Err(VmError::HostError(format!(
                    "failed to create HTTP runtime: {error}"
                ))),
            };
            match &result {
                Ok(_) => {
                    let _ = worker_operation.complete();
                }
                Err(error) => {
                    let _ = worker_operation.fail(
                        RuntimeError::new(
                            RuntimeErrorCode::OperationFailed,
                            "http::request",
                            error.to_string(),
                        )
                        .with_value(op_id),
                    );
                }
            }
            let _ = sender.send(HttpCompletion { result });
        })
    {
        super::cancel_runtime_operation(vm, operation_id, CancellationReason::ResourceClosed);
        return Err(VmError::HostError(format!(
            "failed to start HTTP request: {error}"
        )));
    }

    Ok(op_id)
}

#[cfg(feature = "http-client")]
fn close_request_resource(vm: &mut Vm, op_id: HostOpId, reason: CancellationReason) {
    let Ok(operation_id) = OperationId::from_raw(op_id) else {
        return;
    };
    let Ok(operation) = vm.host.runtime_operations.get(operation_id) else {
        return;
    };
    let Some(resource) = operation.payload() else {
        return;
    };
    let _ = super::close_runtime_resource(vm, resource, reason);
}

#[cfg(feature = "http-client")]
fn runtime_host_error(error: impl std::fmt::Display) -> VmError {
    VmError::HostError(error.to_string())
}

#[cfg(feature = "http-client")]
fn cancellation_vm_error(token: &CancellationToken) -> VmError {
    token
        .check()
        .map(|()| VmError::HostError("HTTP request was cancelled".to_string()))
        .unwrap_or_else(runtime_host_error)
}

#[cfg(feature = "http-client")]
fn cancellation_error(token: &CancellationToken) -> VmResult<CallReturn> {
    Err(cancellation_vm_error(token))
}

/// Starts an HTTP request under the VM's configured network policy.
///
/// The request map accepts `method`, `url`, optional `headers`, and optional `body`.
/// The response map contains `status`, `headers`, `body`, and the final `url`.
#[pd_host_function(name = "http::client::request")]
pub(super) fn builtin_http_client_request(
    vm: &mut Vm,
    request: &VmMap,
) -> VmResult<HostCallResult<VmMap>> {
    #[cfg(not(feature = "http-client"))]
    {
        let _ = (vm, request);
        Err(VmError::HostError(
            "HTTP client support is disabled; enable the http-client feature".to_string(),
        ))
    }

    #[cfg(feature = "http-client")]
    {
        let config = vm
            .host
            .http_state
            .config
            .clone()
            .ok_or_else(|| VmError::HostError("HTTP host is not configured".to_string()))?;
        let request = parse_request(request, &config)?;
        let op_id = schedule_request(vm, config, request)?;
        Ok(HostCallResult::Pending(op_id))
    }
}

pub(super) fn poll_pending_op(
    vm: &mut Vm,
    op_id: HostOpId,
    cx: &mut Context<'_>,
) -> Poll<VmResult<CallReturn>> {
    #[cfg(feature = "http-client")]
    {
        use std::pin::Pin;

        let operation_id = match OperationId::from_raw(op_id) {
            Ok(operation_id) => operation_id,
            Err(error) => return Poll::Ready(Err(runtime_host_error(error))),
        };
        let operation = match vm.host.runtime_operations.get(operation_id) {
            Ok(operation) => operation,
            Err(error) => return Poll::Ready(Err(runtime_host_error(error))),
        };
        let Some(resource) = operation.payload() else {
            return Poll::Ready(Err(VmError::HostError(format!(
                "HTTP op {op_id} has no completion payload",
            ))));
        };
        let poll_result = match vm
            .host
            .runtime_resources
            .get_mut::<HttpRequestResource>(resource, ResourceTypeId::HTTP_REQUEST)
        {
            Ok(request) => Pin::new(&mut request.receiver).poll(cx),
            Err(error) => return Poll::Ready(Err(runtime_host_error(error))),
        };
        match poll_result {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(completion)) => {
                close_request_resource(vm, op_id, CancellationReason::ResourceClosed);
                Poll::Ready(completion.result)
            }
            Poll::Ready(Err(_)) => {
                close_request_resource(vm, op_id, CancellationReason::ResourceClosed);
                Poll::Ready(Err(VmError::HostError(format!(
                    "HTTP op {op_id} was cancelled",
                ))))
            }
        }
    }

    #[cfg(not(feature = "http-client"))]
    {
        let _ = (vm, cx);
        Poll::Ready(Err(VmError::HostError(format!(
            "HTTP support is disabled for op {op_id}",
        ))))
    }
}

#[cfg(feature = "http-client")]
struct HttpRequest {
    method: reqwest::Method,
    url: url::Url,
    headers: Vec<(reqwest::header::HeaderName, reqwest::header::HeaderValue)>,
    body: Option<Vec<u8>>,
}

#[cfg(feature = "http-client")]
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

#[cfg(feature = "http-client")]
fn map_string(map: &VmMap, key: &str) -> VmResult<String> {
    match map.get(&Value::string(key)) {
        Some(Value::String(value)) => Ok(value.as_ref().clone()),
        Some(_) => Err(VmError::TypeMismatch("HTTP request string field")),
        None => Err(VmError::HostError(format!(
            "missing HTTP request field '{key}'"
        ))),
    }
}

#[cfg(feature = "http-client")]
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

#[cfg(all(feature = "http-client", test))]
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

#[cfg(feature = "http-client")]
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

#[cfg(feature = "http-client")]
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

#[cfg(feature = "http-client")]
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

#[cfg(feature = "http-client")]
async fn execute_request(
    config: &HttpConfig,
    request: &HttpRequest,
    token: &CancellationToken,
    deadline: std::time::Instant,
) -> VmResult<CallReturn> {
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
        return Ok(CallReturn::one(Value::Map(std::sync::Arc::new(
            response_map,
        ))));
    }

    Err(VmError::HostError(
        "HTTP redirect processing failed".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::HttpConfig;
    #[cfg(feature = "http-client")]
    use super::{
        CancellationReason, HttpRequest, HttpRequestResource, OperationOwner, ResourceTypeId,
        execute_request, is_restricted_ip, schedule_request, validate_resolved_addresses,
        validate_url,
    };
    #[cfg(feature = "http-client")]
    use crate::builtins::runtime::cancellation::OperationId;

    #[test]
    fn default_http_policy_denies_all_hosts() {
        let config = HttpConfig::default();
        assert_eq!(config.allowed_schemes, ["https"]);
        assert!(config.allowed_hosts.is_empty());
        assert!(config.allowed_ports.is_empty());
        assert!(!config.allow_private_ips);
    }

    #[cfg(feature = "http-client")]
    #[test]
    fn request_uses_shared_operation_and_resource_lifecycle() {
        let mut vm = crate::vm::Vm::new(crate::vm::Program::new(Vec::new(), Vec::new()));
        vm.set_http_max_in_flight(1);
        let config = HttpConfig {
            allowed_schemes: vec!["http".to_string()],
            allowed_hosts: vec!["127.0.0.1".to_string()],
            allowed_ports: vec![1],
            allow_private_ips: true,
            ..HttpConfig::default()
        };
        let request = HttpRequest {
            method: reqwest::Method::GET,
            url: "http://127.0.0.1:1/".parse().expect("valid URL"),
            headers: Vec::new(),
            body: None,
        };

        let op_id = schedule_request(&mut vm, config, request).expect("request should schedule");
        let operation_id = OperationId::from_raw(op_id).expect("operation id should be valid");
        assert_eq!(
            vm.host
                .runtime_operations
                .get(operation_id)
                .expect("operation should be registered")
                .owner(),
            OperationOwner::Http
        );
        let operation = vm
            .host
            .runtime_operations
            .get(operation_id)
            .expect("request should remain registered");
        let resource = operation
            .payload()
            .expect("operation should reference the request resource");
        assert_eq!(resource.resource_type(), ResourceTypeId::HTTP_REQUEST);
        assert!(
            vm.host
                .runtime_resources
                .get::<HttpRequestResource>(resource, ResourceTypeId::HTTP_REQUEST)
                .is_ok()
        );

        let token = operation.token();
        vm.clear_http_configuration();
        assert_eq!(token.reason(), Some(CancellationReason::Requested));
        assert!(
            vm.host
                .runtime_resources
                .get::<HttpRequestResource>(resource, ResourceTypeId::HTTP_REQUEST)
                .is_err()
        );
        assert!(vm.host.runtime_operations.get(operation_id).is_err());
    }

    #[cfg(feature = "http-client")]
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

    #[cfg(feature = "http-client")]
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

    #[cfg(feature = "http-client")]
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

    #[cfg(feature = "http-client")]
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

    #[cfg(feature = "http-client")]
    #[test]
    fn ipv4_mapped_ipv6_loopback_is_restricted() {
        assert!(is_restricted_ip(
            "::ffff:127.0.0.1".parse().expect("valid IP")
        ));
    }
}
