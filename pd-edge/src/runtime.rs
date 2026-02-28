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
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode, Uri,
        header::{CONTENT_LENGTH, CONTENT_TYPE, HOST},
    },
    middleware::{self, Next},
    response::IntoResponse,
    routing::{any, get, put},
};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{info, warn};
use url::Url;
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
        HttpRequestContext, ProxyVmContext, RateLimiterStore, SharedRateLimiter,
        register_host_module, snapshot_execution_outcome,
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

    pub fn record_data_plane_latency_ms(&self, total_latency_ms: u64, upstream_latency_ms: u64) {
        self.runtime_metrics
            .record_latency_ms(total_latency_ms, upstream_latency_ms);
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
        let latencies = self.runtime_metrics.take_latency_percentiles_ms();
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
            latency_p50_ms: latencies.total.p50_ms,
            latency_p90_ms: latencies.total.p90_ms,
            latency_p99_ms: latencies.total.p99_ms,
            upstream_latency_p50_ms: latencies.upstream.p50_ms,
            upstream_latency_p90_ms: latencies.upstream.p90_ms,
            upstream_latency_p99_ms: latencies.upstream.p99_ms,
            edge_latency_p50_ms: latencies.edge_added.p50_ms,
            edge_latency_p90_ms: latencies.edge_added.p90_ms,
            edge_latency_p99_ms: latencies.edge_added.p99_ms,
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
    latency_total_samples_ms: Mutex<VecDeque<u64>>,
    latency_upstream_samples_ms: Mutex<VecDeque<u64>>,
    latency_edge_added_samples_ms: Mutex<VecDeque<u64>>,
}

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
            latency_total_samples_ms: Mutex::new(VecDeque::new()),
            latency_upstream_samples_ms: Mutex::new(VecDeque::new()),
            latency_edge_added_samples_ms: Mutex::new(VecDeque::new()),
        }
    }
}

impl RuntimeMetrics {
    fn record_latency_ms(&self, total_latency_ms: u64, upstream_latency_ms: u64) {
        let upstream_latency_ms = upstream_latency_ms.min(total_latency_ms);
        let edge_added_latency_ms = total_latency_ms.saturating_sub(upstream_latency_ms);
        self.push_latency_sample(&self.latency_total_samples_ms, total_latency_ms);
        self.push_latency_sample(&self.latency_upstream_samples_ms, upstream_latency_ms);
        self.push_latency_sample(&self.latency_edge_added_samples_ms, edge_added_latency_ms);
    }

    fn push_latency_sample(&self, target: &Mutex<VecDeque<u64>>, value: u64) {
        let mut samples = target.lock().expect("latency samples lock poisoned");
        samples.push_back(value);
        while samples.len() > MAX_LATENCY_SAMPLES {
            let _ = samples.pop_front();
        }
    }

    fn drain_latency_samples(&self, target: &Mutex<VecDeque<u64>>) -> Vec<u64> {
        let mut samples = target.lock().expect("latency samples lock poisoned");
        samples.drain(..).collect::<Vec<_>>()
    }

    fn take_latency_percentiles_ms(&self) -> LatencySampleGroup {
        let total = latency_percentiles_from_values(
            self.drain_latency_samples(&self.latency_total_samples_ms),
        );
        let upstream = latency_percentiles_from_values(
            self.drain_latency_samples(&self.latency_upstream_samples_ms),
        );
        let edge_added = latency_percentiles_from_values(
            self.drain_latency_samples(&self.latency_edge_added_samples_ms),
        );
        LatencySampleGroup {
            total,
            upstream,
            edge_added,
        }
    }
}

#[derive(Clone, Copy)]
struct LatencyPercentiles {
    p50_ms: u64,
    p90_ms: u64,
    p99_ms: u64,
}

#[derive(Clone, Copy)]
struct LatencySampleGroup {
    total: LatencyPercentiles,
    upstream: LatencyPercentiles,
    edge_added: LatencyPercentiles,
}

fn latency_percentiles_from_values(mut values: Vec<u64>) -> LatencyPercentiles {
    if values.is_empty() {
        return LatencyPercentiles {
            p50_ms: 0,
            p90_ms: 0,
            p99_ms: 0,
        };
    }
    values.sort_unstable();
    LatencyPercentiles {
        p50_ms: percentile_ms(&values, 50),
        p90_ms: percentile_ms(&values, 90),
        p99_ms: percentile_ms(&values, 99),
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
    let body_bytes = match to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            warn!("{} failed to read request body: {err}", category_program());
            return finalize_data_plane_response(
                &state,
                started,
                text_response(StatusCode::BAD_REQUEST, "invalid request body"),
                0,
            );
        }
    };

    let proxy_inputs = {
        let uri = parts.uri.clone();
        let request_headers = parts.headers.clone();
        let request_scheme = resolve_request_scheme(&uri, &request_headers);
        let vm_request = HttpRequestContext {
            request_id: Uuid::new_v4().to_string(),
            method: parts.method.clone(),
            path: uri.path().to_string(),
            query: uri.query().unwrap_or("").to_string(),
            http_version: http_version_label(parts.version),
            port: resolve_request_port(&uri, &request_headers, &request_scheme),
            scheme: request_scheme,
            host: resolve_request_host(&uri, &request_headers),
            client_ip: resolve_request_client_ip(&request_headers),
            body: body_bytes.to_vec(),
            headers: request_headers,
        };
        let vm_outcome = match execute_vm_for_request(
            &state,
            &program,
            vm_request,
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
            Err(VmExecutionError::NotHalted(status)) => {
                state.record_vm_execution_error();
                warn!(
                    "{} vm returned non-halted status {:?}",
                    category_program(),
                    status
                );
                return finalize_data_plane_response(
                    &state,
                    started,
                    text_response(StatusCode::INTERNAL_SERVER_ERROR, "internal server error"),
                    0,
                );
            }
            Err(VmExecutionError::TaskJoin(err)) => {
                state.record_vm_execution_error();
                warn!("{} vm execution task failed: {err}", category_program());
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

        ProxyUpstreamInputs {
            method: vm_outcome.request_method,
            request_path: vm_outcome.request_path,
            request_query: vm_outcome.request_query,
            request_headers: vm_outcome.request_headers,
            request_body: vm_outcome.request_body,
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
    request: HttpRequestContext,
) -> Result<crate::host_abi::VmExecutionOutcome, VmExecutionError> {
    let local_count = program.local_count;
    let program = program.program.clone();
    let rate_limiter = state.rate_limiter.clone();
    let debug_session = state.debug_session.clone();

    let request_headers = request.headers.clone();
    let request_path = request.path.clone();
    let request_id = request.request_id.clone();

    let task = tokio::task::spawn_blocking(move || {
        let vm_context = Arc::new(std::sync::Mutex::new(ProxyVmContext::from_http_request(
            request,
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
