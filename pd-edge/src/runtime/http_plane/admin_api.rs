use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderValue, Response, StatusCode, header::CONTENT_TYPE},
    middleware,
    response::IntoResponse,
    routing::{get, put},
};
use tracing::{info, warn};

use super::super::{SharedState, apply_program_from_bytes};
use super::shared::access_log_middleware;
use crate::{
    debug_session::{
        DebugSessionStatus, StartDebugSessionRequest, debug_session_status, start_debug_session,
        stop_debug_session,
    },
    logging::{category_debug, category_program},
};

pub fn build_admin_app(state: SharedState) -> Router {
    Router::new()
        .route("/program", put(upload_program_handler))
        .route("/healthz", get(health_handler))
        .route("/metrics", get(metrics_handler))
        .route("/telemetry", get(telemetry_handler))
        .route(
            "/debug/session",
            put(start_debug_session_handler)
                .delete(stop_debug_session_handler)
                .get(debug_session_status_handler),
        )
        .layer(middleware::from_fn(access_log_middleware))
        .with_state(state)
}

async fn upload_program_handler(
    State(state): State<SharedState>,
    request: Request,
) -> Response<Body> {
    if !is_octet_stream(request.headers().get(CONTENT_TYPE)) {
        warn!(
            "{} rejected program upload with invalid content-type",
            category_program()
        );
        return text_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content-type must be application/octet-stream",
        );
    }

    let body = match to_bytes(request.into_body(), state.max_program_bytes + 1).await {
        Ok(body) => body,
        Err(err) => {
            warn!(
                "{} failed reading upload body or exceeded limit: {err}",
                category_program()
            );
            return text_response(StatusCode::PAYLOAD_TOO_LARGE, "payload too large");
        }
    };

    if body.len() > state.max_program_bytes {
        warn!(
            "{} upload too large: {} bytes (limit {})",
            category_program(),
            body.len(),
            state.max_program_bytes
        );
        return text_response(StatusCode::PAYLOAD_TOO_LARGE, "payload too large");
    }

    let report = apply_program_from_bytes(&state, &body).await;
    if report.applied {
        return no_content_response();
    }

    let message = report
        .message
        .as_deref()
        .unwrap_or("failed to apply program");
    text_response(StatusCode::BAD_REQUEST, message)
}

async fn start_debug_session_handler(
    State(state): State<SharedState>,
    Json(request): Json<StartDebugSessionRequest>,
) -> impl IntoResponse {
    match start_debug_session(&state.debug_session, request) {
        Ok(status) => {
            info!(
                "{} debug session started via admin endpoint",
                category_debug()
            );
            (StatusCode::CREATED, Json(status)).into_response()
        }
        Err(err) => {
            warn!("{} failed to start debug session: {err}", category_debug());
            (err.status_code(), err.to_string()).into_response()
        }
    }
}

async fn stop_debug_session_handler(State(state): State<SharedState>) -> impl IntoResponse {
    let stopped = stop_debug_session(&state.debug_session);
    if stopped {
        info!("{} debug session stopped", category_debug());
    } else {
        info!(
            "{} stop requested but no session was active",
            category_debug()
        );
    }
    StatusCode::NO_CONTENT
}

async fn debug_session_status_handler(State(state): State<SharedState>) -> impl IntoResponse {
    let status: DebugSessionStatus = debug_session_status(&state.debug_session);
    (StatusCode::OK, Json(status))
}

async fn health_handler(State(state): State<SharedState>) -> impl IntoResponse {
    let status = state.health_status().await;
    (StatusCode::OK, Json(status))
}

async fn telemetry_handler(State(state): State<SharedState>) -> impl IntoResponse {
    let telemetry = state.telemetry_snapshot().await;
    (StatusCode::OK, Json(telemetry))
}

async fn metrics_handler(State(state): State<SharedState>) -> Response<Body> {
    let metrics = state.metrics_text().await;
    let mut response = text_response(StatusCode::OK, &metrics);
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    response
}

fn no_content_response() -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    response
}

fn text_response(status: StatusCode, text: &str) -> Response<Body> {
    let mut response = Response::new(Body::from(text.to_string()));
    *response.status_mut() = status;
    response
}

fn is_octet_stream(value: Option<&HeaderValue>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    value
        .split(';')
        .next()
        .map(|value| {
            value
                .trim()
                .eq_ignore_ascii_case("application/octet-stream")
        })
        .unwrap_or(false)
}
