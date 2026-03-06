use std::{sync::Arc, time::Instant};

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode, Uri,
        header::{CONTENT_LENGTH, CONTENT_TYPE, HOST},
    },
    middleware,
    routing::any,
};
use tracing::{info, warn};
use url::Url;
use uuid::Uuid;

use super::super::SharedState;
use super::super::vm_runner::{VmDebugInvocation, VmExecutionError, execute_vm_with_context};
use super::shared::access_log_middleware;
use crate::{
    abi_impl::{
        HttpRequestContext, ProxyVmContext, register_http_plane_host_module,
        resolve_outbound_request_body,
    },
    debug_session::request_will_attach_debugger,
    logging::category_program,
};

struct ProxyUpstreamInputs {
    method: Method,
    request_path: String,
    request_query: String,
    request_headers: HeaderMap,
    request_body: Vec<u8>,
    upstream: String,
    vm_response_headers: HeaderMap,
    vm_response_status: Option<u16>,
}

pub fn build_http_proxy_app(state: SharedState) -> Router {
    Router::new()
        .fallback(any(data_plane_handler))
        .layer(middleware::from_fn(access_log_middleware))
        .with_state(state)
}

async fn data_plane_handler(State(state): State<SharedState>, request: Request) -> Response<Body> {
    let started = Instant::now();

    state.record_data_plane_request();

    let snapshot = {
        let guard = state.active_program.read().await;
        guard.clone()
    };

    let Some(program) = snapshot else {
        warn!("{} no program loaded; returning 404", category_program());
        return finalize_data_plane_response(
            &state,
            started,
            text_response(StatusCode::NOT_FOUND, "not found"),
            0,
        );
    };

    let (parts, body) = request.into_parts();

    let proxy_inputs = {
        let uri = parts.uri.clone();
        let request_headers = parts.headers.clone();
        let request_scheme = resolve_request_scheme(&uri, &request_headers);
        let request_id = Uuid::new_v4().to_string();
        let request_path = uri.path().to_string();
        let vm_request = HttpRequestContext {
            request_id: request_id.clone(),
            method: parts.method.clone(),
            path: request_path.clone(),
            query: uri.query().unwrap_or("").to_string(),
            http_version: http_version_label(parts.version),
            port: resolve_request_port(&uri, &request_headers, &request_scheme),
            scheme: request_scheme,
            host: resolve_request_host(&uri, &request_headers),
            client_ip: resolve_request_client_ip(&request_headers),
            body,
            headers: request_headers,
        };
        let debug = VmDebugInvocation {
            attach_debugger: request_will_attach_debugger(
                &state.debug_session,
                &vm_request.headers,
                &request_path,
            ),
            request_headers: vm_request.headers.clone(),
            request_path,
            request_id,
        };
        let vm_context = Arc::new(std::sync::Mutex::new(ProxyVmContext::from_http_request(
            vm_request,
            state.rate_limiter.clone(),
        )));
        let vm_outcome = match execute_vm_with_context(
            &program,
            vm_context.clone(),
            state.debug_session.clone(),
            debug,
            register_http_plane_host_module,
            state.vm_execution,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(VmExecutionError::HostRegistration(err)) => {
                state.record_vm_execution_error();
                warn!(
                    "{} failed to register host module: {err}",
                    category_program()
                );
                return finalize_data_plane_response(
                    &state,
                    started,
                    text_response(StatusCode::INTERNAL_SERVER_ERROR, "internal server error"),
                    0,
                );
            }
            Err(VmExecutionError::Vm(err)) => {
                state.record_vm_execution_error();
                warn!("{} vm execution error: {err}", category_program());
                return finalize_data_plane_response(
                    &state,
                    started,
                    text_response(StatusCode::INTERNAL_SERVER_ERROR, "internal server error"),
                    0,
                );
            }
        };

        if let Some(body) = vm_outcome.response_content {
            info!(
                "{} vm short-circuited response ({} bytes)",
                category_program(),
                body.len()
            );
            return finalize_data_plane_response(
                &state,
                started,
                short_circuit_response(
                    body,
                    vm_outcome.response_headers,
                    vm_outcome.response_status,
                ),
                0,
            );
        }

        let Some(upstream) = vm_outcome.upstream else {
            warn!(
                "{} vm did not set upstream or response content; returning 404",
                category_program()
            );
            return finalize_data_plane_response(
                &state,
                started,
                text_response(StatusCode::NOT_FOUND, "not found"),
                0,
            );
        };

        let request_body = match resolve_outbound_request_body(&vm_context).await {
            Ok(body) => body,
            Err(err) => {
                state.record_vm_execution_error();
                warn!(
                    "{} failed to resolve outbound request body: {err}",
                    category_program()
                );
                return finalize_data_plane_response(
                    &state,
                    started,
                    text_response(StatusCode::INTERNAL_SERVER_ERROR, "internal server error"),
                    0,
                );
            }
        };

        ProxyUpstreamInputs {
            method: vm_outcome.request_method,
            request_path: vm_outcome.request_path,
            request_query: vm_outcome.request_query,
            request_headers: vm_outcome.request_headers,
            request_body,
            upstream,
            vm_response_headers: vm_outcome.response_headers,
            vm_response_status: vm_outcome.response_status,
        }
    };

    let (response, upstream_latency_ms) = proxy_to_upstream(&state, proxy_inputs).await;
    finalize_data_plane_response(&state, started, response, upstream_latency_ms)
}

fn finalize_data_plane_response(
    state: &SharedState,
    started: Instant,
    response: Response<Body>,
    upstream_latency_ms: u64,
) -> Response<Body> {
    state.record_data_plane_status(response.status().as_u16());
    let elapsed_ms = started.elapsed().as_millis();
    let total_latency_ms = u64::try_from(elapsed_ms).unwrap_or(u64::MAX);
    state.record_data_plane_latency_ms(total_latency_ms, upstream_latency_ms);
    response
}

async fn proxy_to_upstream(
    state: &SharedState,
    inputs: ProxyUpstreamInputs,
) -> (Response<Body>, u64) {
    let upstream_started = Instant::now();
    let (upstream_url, host_header) = build_upstream_url(
        &inputs.upstream,
        &inputs.request_path,
        &inputs.request_query,
    );

    let mut outbound = state
        .client
        .request(inputs.method, upstream_url)
        .body(inputs.request_body);
    for (name, value) in &inputs.request_headers {
        if name != HOST && name != CONTENT_LENGTH && !is_hop_by_hop(name) {
            outbound = outbound.header(name, value);
        }
    }
    if let Some(host) = host_header {
        outbound = outbound.header(HOST, host);
    }

    let upstream_response = match outbound.send().await {
        Ok(response) => response,
        Err(err) => {
            warn!("{} upstream request failed: {err}", category_program());
            let elapsed_ms = upstream_started.elapsed().as_millis();
            let upstream_latency_ms = u64::try_from(elapsed_ms).unwrap_or(u64::MAX);
            return (
                text_response(StatusCode::BAD_GATEWAY, "bad gateway"),
                upstream_latency_ms,
            );
        }
    };

    let status = upstream_response.status();
    let upstream_headers = upstream_response.headers().clone();
    let body = match upstream_response.bytes().await {
        Ok(bytes) => bytes,
        Err(err) => {
            warn!(
                "{} failed reading upstream response body: {err}",
                category_program()
            );
            let elapsed_ms = upstream_started.elapsed().as_millis();
            let upstream_latency_ms = u64::try_from(elapsed_ms).unwrap_or(u64::MAX);
            return (
                text_response(StatusCode::BAD_GATEWAY, "bad gateway"),
                upstream_latency_ms,
            );
        }
    };

    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    for (name, value) in &upstream_headers {
        if !is_hop_by_hop(name) {
            response.headers_mut().insert(name, value.clone());
        }
    }

    if let Some(status) = inputs
        .vm_response_status
        .and_then(|code| StatusCode::from_u16(code).ok())
    {
        *response.status_mut() = status;
    }
    merge_headers(response.headers_mut(), &inputs.vm_response_headers);
    let elapsed_ms = upstream_started.elapsed().as_millis();
    let upstream_latency_ms = u64::try_from(elapsed_ms).unwrap_or(u64::MAX);
    (response, upstream_latency_ms)
}

fn build_upstream_url(
    upstream: &str,
    request_path: &str,
    request_query: &str,
) -> (String, Option<String>) {
    let path = if request_path.is_empty() {
        "/"
    } else {
        request_path
    };
    let path_and_query = if request_query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{request_query}")
    };

    if let Ok(url) = Url::parse(upstream) {
        let mut final_url = url;
        let needs_path = final_url.path() == "/" && final_url.query().is_none();
        if needs_path && path_and_query != "/" {
            let base = final_url[..url::Position::AfterPort].to_string();
            let merged = format!("{base}{path_and_query}");
            if let Ok(joined) = Url::parse(&merged) {
                final_url = joined;
            }
        }
        let host = final_url.host_str().map(|host| {
            if let Some(port) = final_url.port() {
                format!("{host}:{port}")
            } else {
                host.to_string()
            }
        });
        return (final_url.to_string(), host);
    }

    let upstream_url = format!("http://{}{path_and_query}", upstream);
    (upstream_url, Some(upstream.to_string()))
}

fn resolve_request_scheme(uri: &Uri, headers: &HeaderMap) -> String {
    if let Some(scheme) = uri.scheme_str() {
        return scheme.to_string();
    }
    if let Some(forwarded) = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return forwarded.to_string();
    }
    "http".to_string()
}

fn resolve_request_port(uri: &Uri, headers: &HeaderMap, scheme: &str) -> u16 {
    if let Some(port) = uri.port_u16() {
        return port;
    }
    if let Some(host_header) = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        && let Ok(authority) = host_header.parse::<axum::http::uri::Authority>()
        && let Some(port) = authority.port_u16()
    {
        return port;
    }
    if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    }
}

fn http_version_label(version: axum::http::Version) -> String {
    match version {
        axum::http::Version::HTTP_09 => "0.9".to_string(),
        axum::http::Version::HTTP_10 => "1.0".to_string(),
        axum::http::Version::HTTP_11 => "1.1".to_string(),
        axum::http::Version::HTTP_2 => "2".to_string(),
        axum::http::Version::HTTP_3 => "3".to_string(),
        _ => "1.1".to_string(),
    }
}

fn resolve_request_host(uri: &Uri, headers: &HeaderMap) -> String {
    if let Some(host) = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return host.to_string();
    }
    uri.authority()
        .map(|authority| authority.as_str().to_string())
        .unwrap_or_default()
}

fn resolve_request_client_ip(headers: &HeaderMap) -> String {
    if let Some(value) = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
    {
        let first = value
            .split(',')
            .map(str::trim)
            .find(|candidate| !candidate.is_empty())
            .unwrap_or_default();
        if !first.is_empty() {
            return first.to_string();
        }
    }
    headers
        .get("x-real-ip")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_default()
}

fn short_circuit_response(
    body: String,
    headers: HeaderMap,
    status_code: Option<u16>,
) -> Response<Body> {
    let mut response = Response::new(Body::from(body));
    let status = status_code
        .and_then(|code| StatusCode::from_u16(code).ok())
        .unwrap_or(StatusCode::OK);
    *response.status_mut() = status;
    merge_headers(response.headers_mut(), &headers);
    if !response.headers().contains_key(CONTENT_TYPE) {
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    }
    response
}

fn merge_headers(target: &mut HeaderMap, overlay: &HeaderMap) {
    for (name, value) in overlay {
        target.insert(name, value.clone());
    }
}

fn text_response(status: StatusCode, text: &str) -> Response<Body> {
    let mut response = Response::new(Body::from(text.to_string()));
    *response.status_mut() = status;
    response
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}
