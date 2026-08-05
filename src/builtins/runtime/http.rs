use std::task::{Context, Poll};

#[cfg(feature = "http-client")]
use std::io::Read;

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
    cancel_flags:
        std::collections::HashMap<HostOpId, std::sync::Arc<std::sync::atomic::AtomicBool>>,
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
                cancel_flags: std::collections::HashMap::new(),
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
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        let (sender, receiver) = futures_channel::oneshot::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let thread_name = format!("rustscript-http-{op_id}");
        std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let result = if worker_cancelled.load(std::sync::atomic::Ordering::Acquire) {
                    Err(VmError::HostError("HTTP request was cancelled".to_string()))
                } else {
                    execute_request(&config, &request, &worker_cancelled)
                };
                let _ = sender.send(HttpCompletion { result });
            })
            .map_err(|error| {
                VmError::HostError(format!("failed to start HTTP request: {error}"))
            })?;
        self.pending_ops.insert(op_id, receiver);
        self.cancel_flags.insert(op_id, cancelled);
        Ok(op_id)
    }

    #[cfg(feature = "http-client")]
    fn has_pending_op(&self, op_id: HostOpId) -> bool {
        self.pending_ops.contains_key(&op_id)
    }

    #[cfg(feature = "http-client")]
    fn cancel_pending_op(&mut self, op_id: HostOpId) {
        if let Some(flag) = self.cancel_flags.remove(&op_id) {
            flag.store(true, std::sync::atomic::Ordering::Release);
        }
        self.pending_ops.remove(&op_id);
    }

    #[cfg(feature = "http-client")]
    fn cancel_all(&mut self) {
        for flag in self.cancel_flags.values() {
            flag.store(true, std::sync::atomic::Ordering::Release);
        }
        self.cancel_flags.clear();
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
                self.cancel_flags.remove(&op_id);
                Poll::Ready(completion.result)
            }
            Poll::Ready(Err(_)) => {
                self.pending_ops.remove(&op_id);
                self.cancel_flags.remove(&op_id);
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
        validate_url(&config, &request.url)?;
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
fn validate_url(config: &HttpConfig, url: &url::Url) -> VmResult<()> {
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
    if !config.allowed_ports.is_empty() && !config.allowed_ports.contains(&port) {
        return Err(VmError::HostError(format!(
            "HTTP target port {port} is not allowed",
        )));
    }
    if !config.allow_private_ips {
        let host_ip = host.parse::<std::net::IpAddr>().ok();
        if host_ip.is_some_and(is_restricted_ip) {
            return Err(VmError::HostError(
                "HTTP target resolves to a restricted IP".to_string(),
            ));
        }
        use std::net::ToSocketAddrs;
        let addresses = (host, port)
            .to_socket_addrs()
            .map_err(|error| VmError::HostError(format!("HTTP host resolution failed: {error}")))?;
        if addresses.map(|address| address.ip()).any(is_restricted_ip) {
            return Err(VmError::HostError(
                "HTTP target resolves to a restricted IP".to_string(),
            ));
        }
    }
    Ok(())
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
            ip.is_loopback()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_unspecified()
                || ip.is_multicast()
        }
    }
}

#[cfg(feature = "http-client")]
fn execute_request(
    config: &HttpConfig,
    request: &HttpRequest,
    cancelled: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> VmResult<CallReturn> {
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(config.connect_timeout)
        .timeout(config.request_timeout)
        .build()
        .map_err(|error| VmError::HostError(format!("HTTP client setup failed: {error}")))?;
    let mut method = request.method.clone();
    let mut url = request.url.clone();
    let mut body = request.body.clone();
    let mut headers = request.headers.clone();

    for redirect_index in 0..=config.max_redirects {
        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err(VmError::HostError("HTTP request was cancelled".to_string()));
        }
        validate_url(config, &url)?;
        let origin = request.url.origin();
        let mut builder = client.request(method.clone(), url.clone());
        for (name, value) in &headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = &body {
            builder = builder.body(body.clone());
        }
        let mut response = builder
            .send()
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
                .map_err(|_| VmError::HostError("HTTP redirect location is invalid".to_string()))?;
            let next_url = url
                .join(location)
                .map_err(|error| VmError::HostError(format!("invalid HTTP redirect: {error}")))?;
            validate_url(config, &next_url)?;
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

        let mut bytes = Vec::new();
        let mut limited_response =
            (&mut response).take(config.max_response_body_bytes.saturating_add(1) as u64);
        limited_response
            .read_to_end(&mut bytes)
            .map_err(|error| VmError::HostError(format!("HTTP response read failed: {error}")))?;
        if bytes.len() > config.max_response_body_bytes {
            return Err(VmError::HostError(
                "HTTP response body exceeds limit".to_string(),
            ));
        }
        let response_headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (Value::string(name.as_str()), Value::string(value)))
            })
            .collect::<Vec<_>>();
        let response_map = VmMap::from_entries(vec![
            (
                Value::string("status"),
                Value::Int(i64::from(response.status().as_u16())),
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

    #[test]
    fn default_http_policy_denies_all_hosts() {
        let config = HttpConfig::default();
        assert_eq!(config.allowed_schemes, ["https"]);
        assert!(config.allowed_hosts.is_empty());
        assert!(!config.allow_private_ips);
    }
}
