use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{info, warn};
use vm::{Program, decode_program, validate_program};

use crate::{
    HOST_FUNCTION_COUNT,
    abi_impl::{RateLimiterStore, SharedRateLimiter},
    control_plane_rpc::EdgeTrafficSample,
    debug_session::{SharedDebugSession, debug_session_status, new_debug_session_store},
    logging::category_program,
};

mod http_plane;
mod vm_runner;

const MAX_LATENCY_SAMPLES: usize = 4096;

pub use http_plane::{build_admin_app, build_http_proxy_app};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VmExecutionMode {
    #[default]
    Async,
    Threading,
}

impl VmExecutionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            VmExecutionMode::Async => "async",
            VmExecutionMode::Threading => "threading",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmExecutionConfig {
    pub fuel_per_yield: Option<u64>,
    pub fuel_check_interval: u32,
    pub execution_mode: VmExecutionMode,
}

impl Default for VmExecutionConfig {
    fn default() -> Self {
        Self {
            fuel_per_yield: None,
            fuel_check_interval: 1,
            execution_mode: VmExecutionMode::default(),
        }
    }
}

#[derive(Clone)]
pub struct SharedState {
    pub active_program: Arc<RwLock<Option<Arc<LoadedProgram>>>>,
    pub max_program_bytes: usize,
    pub client: reqwest::Client,
    pub rate_limiter: SharedRateLimiter,
    pub debug_session: SharedDebugSession,
    pub vm_execution: VmExecutionConfig,
    runtime_metrics: Arc<RuntimeMetrics>,
}

#[derive(Clone)]
pub struct LoadedProgram {
    pub program: Arc<Program>,
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
            vm_execution: VmExecutionConfig::default(),
            runtime_metrics: Arc::new(RuntimeMetrics::default()),
        }
    }

    pub fn with_vm_execution_config(mut self, vm_execution: VmExecutionConfig) -> Self {
        self.vm_execution = vm_execution;
        self
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

    let local_count = program.local_count;
    let const_count = program.constants.len();
    let code_len = program.code.len();
    let mut guard = state.active_program.write().await;
    *guard = Some(Arc::new(LoadedProgram {
        program: Arc::new(program),
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
