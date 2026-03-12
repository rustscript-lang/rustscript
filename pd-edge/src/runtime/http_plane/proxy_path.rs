use std::{sync::Arc, time::Instant};

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, Response, StatusCode, Uri, header::HOST},
    middleware,
    routing::any,
};
use tracing::warn;
use uuid::Uuid;

use super::super::SharedState;
use super::super::vm_runner::{VmDebugInvocation, VmExecutionError, execute_vm_with_context};
use super::shared::access_log_middleware;
use crate::{
    abi_impl::http::resolve_http_graph_response,
    abi_impl::{HttpRequestContext, ProxyVmContext, register_http_plane_host_module},
    debug_session::{request_uses_blocking_debugger, request_will_attach_debugger},
    logging::category_program,
};

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

    let vm_context = {
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
            force_threading: request_uses_blocking_debugger(
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
        {
            let mut guard = vm_context.lock().expect("vm context lock poisoned");
            guard.attach_upstream_client(state.client.clone());
            guard.attach_tls_session_cache(state.tls_session_cache.clone());
        }
        match execute_vm_with_context(
            &program,
            vm_context.clone(),
            state.debug_session.clone(),
            debug,
            register_http_plane_host_module,
            state.vm_execution,
        )
        .await
        {
            Ok(()) => {}
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
        }

        vm_context
    };

    let resolved = resolve_http_graph_response(&vm_context).await;
    finalize_data_plane_response(
        &state,
        started,
        resolved.response,
        resolved.upstream_latency_ms,
    )
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

fn text_response(status: StatusCode, text: &str) -> Response<Body> {
    let mut response = Response::new(Body::from(text.to_string()));
    *response.status_mut() = status;
    response
}
