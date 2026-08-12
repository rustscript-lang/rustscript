use futures_util::StreamExt;

use super::HttpRequestContext;
use super::config::HttpConfig;
use super::policy::{SchemeFamily, resolve_url, with_deadline};
use crate::builtins::runtime::VmMap;
use crate::vm::{Value, VmError, VmResult};

#[derive(Clone)]
pub(super) struct HttpRequest {
    pub(super) method: reqwest::Method,
    pub(super) url: url::Url,
    pub(super) headers: Vec<(reqwest::header::HeaderName, reqwest::header::HeaderValue)>,
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
    let deadline = std::time::Instant::now() + context.config.request_timeout;
    with_deadline(deadline, execute_request(&context.config, &request)).await
}

pub(super) async fn execute_request(config: &HttpConfig, request: &HttpRequest) -> VmResult<VmMap> {
    let mut method = request.method.clone();
    let mut url = request.url.clone();
    let mut body = request.body.clone();
    let mut headers = request.headers.clone();

    for redirect_index in 0..=config.max_redirects {
        let resolved = resolve_url(config, SchemeFamily::Http, &url).await?;
        let mut client_builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(config.connect_timeout);
        client_builder = client_builder.resolve(&resolved.host, resolved.address);
        let client = client_builder
            .build()
            .map_err(|error| VmError::HostError(format!("HTTP client setup failed: {error}")))?;
        let origin = url.origin();
        let mut builder = client.request(method.clone(), url.clone());
        for (name, value) in &headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = &body {
            builder = builder.body(body.clone());
        }
        let response = builder.send().await.map_err(|error| {
            if error.is_timeout() {
                VmError::HostError("HTTP request deadline exceeded".to_string())
            } else {
                VmError::HostError(format!("HTTP request failed: {error}"))
            }
        })?;
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
        while let Some(chunk) = stream.next().await {
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
