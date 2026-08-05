use std::task::{Context, Poll};

#[cfg(feature = "http-client")]
use futures_util::StreamExt;
#[cfg(feature = "http-client")]
use futures_util::future::{AbortHandle, Abortable};

use pd_host_function::pd_host_function;

use super::{HostCallResult, Vm, VmMap, VmResult};
#[cfg(feature = "http-client")]
use crate::vm::Value;
use crate::vm::{CallReturn, HostOpId, VmError};

#[derive(Clone, Debug)]
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
pub(crate) struct HttpState {
    config: Option<HttpConfig>,
    pending_ops:
        std::collections::HashMap<HostOpId, futures_channel::oneshot::Receiver<HttpCompletion>>,
    abort_handles: std::collections::HashMap<HostOpId, AbortHandle>,
}

#[cfg(not(feature = "http-client"))]
pub(crate) struct HttpState;

impl Default for HttpState {
    fn default() -> Self {
        #[cfg(feature = "http-client")]
        {
            return Self {
                config: None,
                pending_ops: std::collections::HashMap::new(),
                abort_handles: std::collections::HashMap::new(),
            };
        }

        #[cfg(not(feature = "http-client"))]
        Self
    }
}

impl HttpState {
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
            self.cancel_all();
            self.config = None;
        }
    }

    pub(crate) fn is_configured(&self) -> bool {
        #[cfg(feature = "http-client")]
        {
            return self.config.is_some();
        }
        #[cfg(not(feature = "http-client"))]
        false
    }

    #[cfg(feature = "http-client")]
    fn schedule(
        &mut self,
        op_id: HostOpId,
        config: HttpConfig,
        request: HttpRequest,
    ) -> VmResult<HostOpId> {
        let (sender, receiver) = futures_channel::oneshot::channel();
        let (abort_handle, abort_registration) = AbortHandle::new_pair();
        let thread_name = format!("rustscript-http-{op_id}");
        std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let result = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime.block_on(async move {
                        match Abortable::new(execute_request(&config, &request), abort_registration)
                            .await
                        {
                            Ok(result) => result,
                            Err(_) => {
                                Err(VmError::HostError("HTTP request was cancelled".to_string()))
                            }
                        }
                    }),
                    Err(error) => Err(VmError::HostError(format!(
                        "failed to create HTTP runtime: {error}"
                    ))),
                };
                let _ = sender.send(HttpCompletion { result });
            })
            .map_err(|error| {
                VmError::HostError(format!("failed to start HTTP request: {error}"))
            })?;
        self.pending_ops.insert(op_id, receiver);
        self.abort_handles.insert(op_id, abort_handle);
        Ok(op_id)
    }

    #[cfg(feature = "http-client")]
    fn has_pending_op(&self, op_id: HostOpId) -> bool {
        self.pending_ops.contains_key(&op_id)
    }

    #[cfg(feature = "http-client")]
    fn cancel_pending_op(&mut self, op_id: HostOpId) {
        if let Some(handle) = self.abort_handles.remove(&op_id) {
            handle.abort();
        }
        self.pending_ops.remove(&op_id);
    }

    #[cfg(feature = "http-client")]
    fn cancel_all(&mut self) {
        for handle in self.abort_handles.drain().map(|(_, handle)| handle) {
            handle.abort();
        }
        self.pending_ops.clear();
    }

    #[cfg(feature = "http-client")]
    fn poll_pending_op(
        &mut self,
        op_id: HostOpId,
        cx: &mut Context<'_>,
    ) -> Poll<VmResult<CallReturn>> {
        use std::pin::Pin;

        let poll_result = match self.pending_ops.get_mut(&op_id) {
            Some(receiver) => Pin::new(receiver).poll(cx),
            None => {
                return Poll::Ready(Err(VmError::HostError(format!("unknown HTTP op {op_id}",))));
            }
        };
        match poll_result {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(completion)) => {
                self.pending_ops.remove(&op_id);
                self.abort_handles.remove(&op_id);
                Poll::Ready(completion.result)
            }
            Poll::Ready(Err(_)) => {
                self.pending_ops.remove(&op_id);
                self.abort_handles.remove(&op_id);
                Poll::Ready(Err(VmError::HostError(format!(
                    "HTTP op {op_id} was cancelled",
                ))))
            }
        }
    }
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
        return Err(VmError::HostError(
            "HTTP client support is disabled; enable the http-client feature".to_string(),
        ));
    }

    #[cfg(feature = "http-client")]
    {
        let config = vm
            .http_state
            .config
            .clone()
            .ok_or_else(|| VmError::HostError("HTTP host is not configured".to_string()))?;
        let request = parse_request(request, &config)?;
        let op_id = vm.allocate_host_op_id();
        let op_id = vm.http_state.schedule(op_id, config, request)?;
        Ok(HostCallResult::Pending(op_id))
    }
}

#[cfg(feature = "http-client")]
pub(super) fn has_pending_op(vm: &Vm, op_id: HostOpId) -> bool {
    vm.http_state.has_pending_op(op_id)
}

#[cfg(not(feature = "http-client"))]
pub(super) fn has_pending_op(_vm: &Vm, _op_id: HostOpId) -> bool {
    false
}

pub(super) fn cancel_pending_op(vm: &mut Vm, op_id: HostOpId) {
    #[cfg(feature = "http-client")]
    vm.http_state.cancel_pending_op(op_id);
    #[cfg(not(feature = "http-client"))]
    let _ = (vm, op_id);
}

pub(super) fn cancel_all_pending_ops(vm: &mut Vm) {
    #[cfg(feature = "http-client")]
    vm.http_state.cancel_all();
    #[cfg(not(feature = "http-client"))]
    let _ = vm;
}

pub(super) fn poll_pending_op(
    vm: &mut Vm,
    op_id: HostOpId,
    cx: &mut Context<'_>,
) -> Poll<VmResult<CallReturn>> {
    #[cfg(feature = "http-client")]
    return vm.http_state.poll_pending_op(op_id, cx);

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
fn validate_url(config: &HttpConfig, url: &url::Url) -> VmResult<Option<std::net::SocketAddr>> {
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
    if config.allow_private_ips {
        return Ok(None);
    }

    if let Some(host_ip) = host.parse::<std::net::IpAddr>().ok() {
        if is_restricted_ip(host_ip) {
            return Err(VmError::HostError(
                "HTTP target resolves to a restricted IP".to_string(),
            ));
        }
        return Ok(None);
    }

    use std::net::ToSocketAddrs;
    let addresses = (host, port)
        .to_socket_addrs()
        .map_err(|error| VmError::HostError(format!("HTTP host resolution failed: {error}")))?
        .collect::<Vec<_>>();
    if addresses.is_empty()
        || addresses
            .iter()
            .any(|address| is_restricted_ip(address.ip()))
    {
        return Err(VmError::HostError(
            "HTTP target resolves to a restricted IP".to_string(),
        ));
    }
    Ok(addresses.first().copied())
}

#[cfg(feature = "http-client")]
fn is_restricted_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
        }
        std::net::IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return is_restricted_ip(std::net::IpAddr::V4(mapped));
            }
            ip.is_loopback()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_unspecified()
                || ip.is_multicast()
        }
    }
}

#[cfg(feature = "http-client")]
async fn execute_request(config: &HttpConfig, request: &HttpRequest) -> VmResult<CallReturn> {
    let deadline = std::time::Instant::now() + config.request_timeout;
    let mut method = request.method.clone();
    let mut url = request.url.clone();
    let mut body = request.body.clone();
    let mut headers = request.headers.clone();

    for redirect_index in 0..=config.max_redirects {
        let resolved_address = validate_url(config, &url)?;
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
            return Err(VmError::HostError("HTTP request timed out".to_string()));
        }
        let response = tokio::time::timeout(remaining, builder.send())
            .await
            .map_err(|_| VmError::HostError("HTTP request timed out".to_string()))?
            .map_err(|error| VmError::HostError(format!("HTTP request failed: {error}")))?;
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
                return Err(VmError::HostError(
                    "HTTP response read timed out".to_string(),
                ));
            }
            tokio::time::timeout(remaining, stream.next())
                .await
                .map_err(|_| VmError::HostError("HTTP response read timed out".to_string()))?
        } {
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
    use super::{is_restricted_ip, validate_url};

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
    fn ipv4_mapped_ipv6_loopback_is_restricted() {
        assert!(is_restricted_ip(
            "::ffff:127.0.0.1".parse().expect("valid IP")
        ));
    }
}
