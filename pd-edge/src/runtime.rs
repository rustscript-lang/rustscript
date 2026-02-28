use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{Request, State},
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode, Uri,
        header::{CONTENT_TYPE, HOST},
    },
    middleware::{self, Next},
    response::IntoResponse,
    routing::{any, get, put},
};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{info, warn};
use uuid::Uuid;
use vm::{Program, Vm, VmStatus, decode_program, infer_local_count, validate_program};

use crate::{
    HOST_FUNCTION_COUNT,
    control_plane_rpc::EdgeTrafficSample,
    debug_session::{
        DebugSessionStatus, SharedDebugSession, StartDebugSessionRequest, debug_session_status,
        new_debug_session_store, run_vm_with_optional_debugger, start_debug_session,
        stop_debug_session,
    },
    host_abi::{
        ProxyVmContext, RateLimiterStore, SharedRateLimiter, register_host_module,
        snapshot_execution_outcome,
    },
    logging::{category_access, category_debug, category_program, method_label, status_label},
};

const MAX_LATENCY_SAMPLES: usize = 4096;

#[derive(Clone)]
pub struct SharedState {
    pub active_program: Arc<RwLock<Option<Arc<LoadedProgram>>>>,
    pub max_program_bytes: usize,
    pub client: reqwest::Client,
    pub rate_limiter: SharedRateLimiter,
    pub debug_session: SharedDebugSession,
    runtime_metrics: Arc<RuntimeMetrics>,
}

#[derive(Clone)]
pub struct LoadedProgram {
    pub program: Arc<Program>,
    pub local_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthStatus {
    pub status: String,
    pub program_loaded: bool,
    pub debug_session_active: bool,
    pub debug_session_attached: bool,
    pub uptime_seconds: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TelemetrySnapshot {
    pub uptime_seconds: u64,
    pub program_loaded: bool,
    pub debug_session_active: bool,
    pub debug_session_attached: bool,
    #[serde(default)]
    pub debug_session_current_line: Option<u32>,
    #[serde(default)]
    pub debug_session_request_id: Option<String>,
    pub data_requests_total: u64,
    pub vm_execution_errors_total: u64,
    pub program_apply_success_total: u64,
    pub program_apply_failure_total: u64,
    pub control_rpc_polls_success_total: u64,
    pub control_rpc_polls_error_total: u64,
    pub control_rpc_results_success_total: u64,
    pub control_rpc_results_error_total: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProgramApplyReport {
    pub applied: bool,
    pub constants: Option<usize>,
    pub code_bytes: Option<usize>,
    pub local_count: Option<usize>,
    pub message: Option<String>,
}

impl SharedState {
    pub fn new(max_program_bytes: usize) -> Self {
        Self {
            active_program: Arc::new(RwLock::new(None)),
            max_program_bytes,
            client: reqwest::Client::new(),
            rate_limiter: Arc::new(std::sync::Mutex::new(RateLimiterStore::new())),
            debug_session: new_debug_session_store(),
            runtime_metrics: Arc::new(RuntimeMetrics::default()),
        }
    }

    pub fn record_data_plane_request(&self) {
        self.runtime_metrics
            .data_requests_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_data_plane_status(&self, status: u16) {
        let metrics = &self.runtime_metrics;
        match status {
            200..=299 => {
                metrics.status_2xx_total.fetch_add(1, Ordering::Relaxed);
            }
            300..=399 => {
                metrics.status_3xx_total.fetch_add(1, Ordering::Relaxed);
            }
            400..=499 => {
                metrics.status_4xx_total.fetch_add(1, Ordering::Relaxed);
            }
            500..=599 => {
                metrics.status_5xx_total.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    pub fn record_data_plane_latency_ms(&self, latency_ms: u64) {
        self.runtime_metrics.record_latency_ms(latency_ms);
    }

    pub fn record_vm_execution_error(&self) {
        self.runtime_metrics
            .vm_execution_errors_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_program_apply_success(&self) {
        self.runtime_metrics
            .program_apply_success_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_program_apply_failure(&self) {
        self.runtime_metrics
            .program_apply_failure_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_control_rpc_poll_success(&self) {
        self.runtime_metrics
            .control_rpc_polls_success_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_control_rpc_poll_error(&self) {
        self.runtime_metrics
            .control_rpc_polls_error_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_control_rpc_result_success(&self) {
        self.runtime_metrics
            .control_rpc_results_success_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_control_rpc_result_error(&self) {
        self.runtime_metrics
            .control_rpc_results_error_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub async fn health_status(&self) -> HealthStatus {
        let program_loaded = self.active_program.read().await.is_some();
        let debug_status = debug_session_status(&self.debug_session);
        HealthStatus {
            status: "ok".to_string(),
            program_loaded,
            debug_session_active: debug_status.active,
            debug_session_attached: debug_status.attached,
            uptime_seconds: self.runtime_metrics.started_at.elapsed().as_secs(),
        }
    }

    pub async fn telemetry_snapshot(&self) -> TelemetrySnapshot {
        let program_loaded = self.active_program.read().await.is_some();
        let debug_status = debug_session_status(&self.debug_session);
        TelemetrySnapshot {
            uptime_seconds: self.runtime_metrics.started_at.elapsed().as_secs(),
            program_loaded,
            debug_session_active: debug_status.active,
            debug_session_attached: debug_status.attached,
            debug_session_current_line: debug_status.current_line,
            debug_session_request_id: debug_status.request_id,
            data_requests_total: self
                .runtime_metrics
                .data_requests_total
                .load(Ordering::Relaxed),
            vm_execution_errors_total: self
                .runtime_metrics
                .vm_execution_errors_total
                .load(Ordering::Relaxed),
            program_apply_success_total: self
                .runtime_metrics
                .program_apply_success_total
                .load(Ordering::Relaxed),
            program_apply_failure_total: self
                .runtime_metrics
                .program_apply_failure_total
                .load(Ordering::Relaxed),
            control_rpc_polls_success_total: self
                .runtime_metrics
                .control_rpc_polls_success_total
                .load(Ordering::Relaxed),
            control_rpc_polls_error_total: self
                .runtime_metrics
                .control_rpc_polls_error_total
                .load(Ordering::Relaxed),
            control_rpc_results_success_total: self
                .runtime_metrics
                .control_rpc_results_success_total
                .load(Ordering::Relaxed),
            control_rpc_results_error_total: self
                .runtime_metrics
                .control_rpc_results_error_total
                .load(Ordering::Relaxed),
        }
    }

    pub fn traffic_sample(&self) -> EdgeTrafficSample {
        let (latency_p50_ms, latency_p90_ms, latency_p99_ms) =
            self.runtime_metrics.take_latency_percentiles_ms();
        EdgeTrafficSample {
            requests_total: self
                .runtime_metrics
                .data_requests_total
                .load(Ordering::Relaxed),
            status_2xx_total: self
                .runtime_metrics
                .status_2xx_total
                .load(Ordering::Relaxed),
            status_3xx_total: self
                .runtime_metrics
                .status_3xx_total
                .load(Ordering::Relaxed),
            status_4xx_total: self
                .runtime_metrics
                .status_4xx_total
                .load(Ordering::Relaxed),
            status_5xx_total: self
                .runtime_metrics
                .status_5xx_total
                .load(Ordering::Relaxed),
            latency_p50_ms,
            latency_p90_ms,
            latency_p99_ms,
        }
    }

    pub async fn metrics_text(&self) -> String {
        let telemetry = self.telemetry_snapshot().await;
        let debug_active = if telemetry.debug_session_active { 1 } else { 0 };
        let debug_attached = if telemetry.debug_session_attached {
            1
        } else {
            0
        };
        let program_loaded = if telemetry.program_loaded { 1 } else { 0 };

        format!(
            concat!(
                "pd_proxy_uptime_seconds {}\n",
                "pd_proxy_program_loaded {}\n",
                "pd_proxy_debug_session_active {}\n",
                "pd_proxy_debug_session_attached {}\n",
                "pd_proxy_data_requests_total {}\n",
                "pd_proxy_vm_execution_errors_total {}\n",
                "pd_proxy_program_apply_success_total {}\n",
                "pd_proxy_program_apply_failure_total {}\n",
                "pd_proxy_control_rpc_polls_success_total {}\n",
                "pd_proxy_control_rpc_polls_error_total {}\n",
                "pd_proxy_control_rpc_results_success_total {}\n",
                "pd_proxy_control_rpc_results_error_total {}\n"
            ),
            telemetry.uptime_seconds,
            program_loaded,
            debug_active,
            debug_attached,
            telemetry.data_requests_total,
            telemetry.vm_execution_errors_total,
            telemetry.program_apply_success_total,
            telemetry.program_apply_failure_total,
            telemetry.control_rpc_polls_success_total,
            telemetry.control_rpc_polls_error_total,
            telemetry.control_rpc_results_success_total,
            telemetry.control_rpc_results_error_total,
        )
    }
}

struct RuntimeMetrics {
    started_at: Instant,
    data_requests_total: AtomicU64,
    status_2xx_total: AtomicU64,
    status_3xx_total: AtomicU64,
    status_4xx_total: AtomicU64,
    status_5xx_total: AtomicU64,
    vm_execution_errors_total: AtomicU64,
    program_apply_success_total: AtomicU64,
    program_apply_failure_total: AtomicU64,
    control_rpc_polls_success_total: AtomicU64,
    control_rpc_polls_error_total: AtomicU64,
    control_rpc_results_success_total: AtomicU64,
    control_rpc_results_error_total: AtomicU64,
    latency_samples_ms: Mutex<VecDeque<u64>>,
}

struct ProxyUpstreamInputs {
    method: Method,
    uri: Uri,
    request_headers: HeaderMap,
    request_body: Bytes,
    upstream: String,
    vm_response_headers: HeaderMap,
    vm_response_status: Option<u16>,
}

impl Default for RuntimeMetrics {
    fn default() -> Self {
        Self {
            started_at: Instant::now(),
            data_requests_total: AtomicU64::new(0),
            status_2xx_total: AtomicU64::new(0),
            status_3xx_total: AtomicU64::new(0),
            status_4xx_total: AtomicU64::new(0),
            status_5xx_total: AtomicU64::new(0),
            vm_execution_errors_total: AtomicU64::new(0),
            program_apply_success_total: AtomicU64::new(0),
            program_apply_failure_total: AtomicU64::new(0),
            control_rpc_polls_success_total: AtomicU64::new(0),
            control_rpc_polls_error_total: AtomicU64::new(0),
            control_rpc_results_success_total: AtomicU64::new(0),
            control_rpc_results_error_total: AtomicU64::new(0),
            latency_samples_ms: Mutex::new(VecDeque::new()),
        }
    }
}

impl RuntimeMetrics {
    fn record_latency_ms(&self, latency_ms: u64) {
        let mut samples = self
            .latency_samples_ms
            .lock()
            .expect("latency samples lock poisoned");
        samples.push_back(latency_ms);
        while samples.len() > MAX_LATENCY_SAMPLES {
            let _ = samples.pop_front();
        }
    }

    fn take_latency_percentiles_ms(&self) -> (u64, u64, u64) {
        let mut values = {
            let mut samples = self
                .latency_samples_ms
                .lock()
                .expect("latency samples lock poisoned");
            samples.drain(..).collect::<Vec<_>>()
        };
        if values.is_empty() {
            return (0, 0, 0);
        }
        values.sort_unstable();
        (
            percentile_ms(&values, 50),
            percentile_ms(&values, 90),
            percentile_ms(&values, 99),
        )
    }
}

fn percentile_ms(sorted_values: &[u64], percentile: usize) -> u64 {
    let len = sorted_values.len();
    let idx = ((len - 1) * percentile) / 100;
    sorted_values[idx]
}

pub fn build_data_app(state: SharedState) -> Router {
    Router::new()
        .fallback(any(data_plane_handler))
        .layer(middleware::from_fn(access_log_middleware))
        .with_state(state)
}

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

async fn data_plane_handler(State(state): State<SharedState>, request: Request) -> Response<Body> {
    let started = Instant::now();
    let finalize = |state: &SharedState, response: Response<Body>| -> Response<Body> {
        state.record_data_plane_status(response.status().as_u16());
        let elapsed_ms = started.elapsed().as_millis();
        let latency_ms = u64::try_from(elapsed_ms).unwrap_or(u64::MAX);
        state.record_data_plane_latency_ms(latency_ms);
        response
    };

    state.record_data_plane_request();

    let snapshot = {
        let guard = state.active_program.read().await;
        guard.clone()
    };

    let Some(program) = snapshot else {
        warn!("{} no program loaded; returning 404", category_program());
        return finalize(&state, text_response(StatusCode::NOT_FOUND, "not found"));
    };

    let (parts, body) = request.into_parts();
    let body_bytes = match to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            warn!("{} failed to read request body: {err}", category_program());
            return finalize(
                &state,
                text_response(StatusCode::BAD_REQUEST, "invalid request body"),
            );
        }
    };

    let proxy_inputs = {
        let method = parts.method.clone();
        let uri = parts.uri.clone();
        let request_headers = parts.headers.clone();
        let request_path = uri.path().to_string();
        let request_id = Uuid::new_v4().to_string();
        let vm_outcome = match execute_vm_for_request(
            &state,
            &program,
            request_headers.clone(),
            request_path,
            request_id,
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
                return finalize(&state, text_response(StatusCode::NOT_FOUND, "not found"));
            }
            Err(VmExecutionError::Vm(err)) => {
                state.record_vm_execution_error();
                warn!("{} vm execution error: {err}", category_program());
                return finalize(&state, text_response(StatusCode::NOT_FOUND, "not found"));
            }
            Err(VmExecutionError::NotHalted(status)) => {
                state.record_vm_execution_error();
                warn!(
                    "{} vm returned non-halted status {:?}",
                    category_program(),
                    status
                );
                return finalize(&state, text_response(StatusCode::NOT_FOUND, "not found"));
            }
            Err(VmExecutionError::TaskJoin(err)) => {
                state.record_vm_execution_error();
                warn!("{} vm execution task failed: {err}", category_program());
                return finalize(
                    &state,
                    text_response(StatusCode::INTERNAL_SERVER_ERROR, "internal server error"),
                );
            }
        };

        if let Some(body) = vm_outcome.response_content {
            info!(
                "{} vm short-circuited response ({} bytes)",
                category_program(),
                body.len()
            );
            return finalize(
                &state,
                short_circuit_response(
                    body,
                    vm_outcome.response_headers,
                    vm_outcome.response_status,
                ),
            );
        }

        let Some(upstream) = vm_outcome.upstream else {
            warn!(
                "{} vm did not set upstream or response content; returning 404",
                category_program()
            );
            return finalize(&state, text_response(StatusCode::NOT_FOUND, "not found"));
        };

        ProxyUpstreamInputs {
            method,
            uri,
            request_headers: parts.headers,
            request_body: body_bytes,
            upstream,
            vm_response_headers: vm_outcome.response_headers,
            vm_response_status: vm_outcome.response_status,
        }
    };

    let response = proxy_to_upstream(&state, proxy_inputs).await;
    finalize(&state, response)
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

pub async fn apply_program_from_bytes(state: &SharedState, bytes: &[u8]) -> ProgramApplyReport {
    if bytes.len() > state.max_program_bytes {
        state.record_program_apply_failure();
        let message = format!(
            "payload too large: {} bytes (limit {})",
            bytes.len(),
            state.max_program_bytes
        );
        warn!(
            "{} rejected program payload that exceeds limit: {}",
            category_program(),
            message
        );
        return ProgramApplyReport {
            applied: false,
            constants: None,
            code_bytes: None,
            local_count: None,
            message: Some(message),
        };
    }

    let program = match decode_program(bytes) {
        Ok(program) => program,
        Err(err) => {
            state.record_program_apply_failure();
            let message = format!("invalid program: {err}");
            warn!("{} decode error: {err}", category_program());
            return ProgramApplyReport {
                applied: false,
                constants: None,
                code_bytes: None,
                local_count: None,
                message: Some(message),
            };
        }
    };
    if let Err(err) = validate_program(&program, HOST_FUNCTION_COUNT) {
        state.record_program_apply_failure();
        let message = format!("invalid bytecode: {err}");
        warn!("{} validation error: {err}", category_program());
        return ProgramApplyReport {
            applied: false,
            constants: None,
            code_bytes: None,
            local_count: None,
            message: Some(message),
        };
    }

    let local_count = match infer_local_count(&program) {
        Ok(local_count) => local_count,
        Err(err) => {
            state.record_program_apply_failure();
            let message = format!("invalid bytecode: {err}");
            warn!("{} local inference error: {err}", category_program());
            return ProgramApplyReport {
                applied: false,
                constants: None,
                code_bytes: None,
                local_count: None,
                message: Some(message),
            };
        }
    };

    let const_count = program.constants.len();
    let code_len = program.code.len();
    let mut guard = state.active_program.write().await;
    *guard = Some(Arc::new(LoadedProgram {
        program: Arc::new(program),
        local_count,
    }));
    state.record_program_apply_success();
    info!(
        "{} loaded program successfully (constants={}, code_bytes={}, locals={})",
        category_program(),
        const_count,
        code_len,
        local_count
    );

    ProgramApplyReport {
        applied: true,
        constants: Some(const_count),
        code_bytes: Some(code_len),
        local_count: Some(local_count),
        message: None,
    }
}

async fn proxy_to_upstream(state: &SharedState, inputs: ProxyUpstreamInputs) -> Response<Body> {
    let path_and_query = inputs
        .uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    let upstream_url = format!("http://{}{path_and_query}", inputs.upstream);

    let mut outbound = state
        .client
        .request(inputs.method, upstream_url)
        .body(inputs.request_body.to_vec());
    for (name, value) in &inputs.request_headers {
        if name != HOST && !is_hop_by_hop(name) {
            outbound = outbound.header(name, value);
        }
    }
    outbound = outbound.header(HOST, inputs.upstream.as_str());

    let upstream_response = match outbound.send().await {
        Ok(response) => response,
        Err(err) => {
            warn!("{} upstream request failed: {err}", category_program());
            return text_response(StatusCode::BAD_GATEWAY, "bad gateway");
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
            return text_response(StatusCode::BAD_GATEWAY, "bad gateway");
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
    response
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

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str().to_ascii_lowercase().as_str(),
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

async fn access_log_middleware(request: Request, next: Next) -> Response<Body> {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let started = Instant::now();
    let response = next.run(request).await;
    let elapsed_ms = started.elapsed().as_millis();
    let status = response.status();

    info!(
        "{} {} {} {} {}ms",
        category_access(),
        method_label(method.as_str()),
        status_label(status.as_u16()),
        uri,
        elapsed_ms
    );

    response
}

#[derive(Debug)]
enum VmExecutionError {
    HostRegistration(vm::VmError),
    Vm(vm::VmError),
    NotHalted(VmStatus),
    TaskJoin(tokio::task::JoinError),
}

async fn execute_vm_for_request(
    state: &SharedState,
    program: &LoadedProgram,
    request_headers: HeaderMap,
    request_path: String,
    request_id: String,
) -> Result<crate::host_abi::VmExecutionOutcome, VmExecutionError> {
    let local_count = program.local_count;
    let program = program.program.clone();
    let rate_limiter = state.rate_limiter.clone();
    let debug_session = state.debug_session.clone();

    let task = tokio::task::spawn_blocking(move || {
        let vm_context = Arc::new(std::sync::Mutex::new(ProxyVmContext::from_request_headers(
            request_headers.clone(),
            rate_limiter,
        )));

        let mut vm = Vm::with_locals((*program).clone(), local_count);
        register_host_module(&mut vm, vm_context.clone())
            .map_err(VmExecutionError::HostRegistration)?;

        let status = run_vm_with_optional_debugger(
            &debug_session,
            &request_headers,
            &request_path,
            &request_id,
            &mut vm,
        )
        .map_err(VmExecutionError::Vm)?;
        if status != VmStatus::Halted {
            return Err(VmExecutionError::NotHalted(status));
        }

        Ok(snapshot_execution_outcome(&vm_context))
    });

    task.await.map_err(VmExecutionError::TaskJoin)?
}
