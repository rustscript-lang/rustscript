use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use arc_swap::ArcSwapOption;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use vm::{Program, decode_program, validate_program};

use crate::{
    HOST_FUNCTION_COUNT,
    abi_impl::{
        ProxyVmContext, RateLimiterStore, SharedHttp3DownstreamSessions,
        SharedHttpDownstreamSessions, SharedRateLimiter,
        http::{
            SharedRuntimeServices, new_shared_http_plane_runtime_services,
            new_shared_upstream_client_cache, upstream_reqwest_client_builder,
        },
        new_shared_http_downstream_sessions, new_shared_http_upstream_sessions,
        new_shared_http3_downstream_sessions, new_shared_http3_upstream_sessions,
        new_shared_tls_session_cache,
    },
    cache::{
        DEFAULT_DOWNSTREAM_HTTP2_SESSION_STORE_CAPACITY,
        DEFAULT_DOWNSTREAM_HTTP3_SESSION_STORE_CAPACITY, DEFAULT_TLS_SESSION_REUSE_STORE_CAPACITY,
        DEFAULT_UPSTREAM_HTTP_REUSE_STORE_CAPACITY, DEFAULT_UPSTREAM_HTTP3_REUSE_STORE_CAPACITY,
    },
    control_plane_rpc::EdgeTrafficSample,
    debug_session::{SharedDebugSession, debug_session_status, new_debug_session_store},
    lock_metrics::{self, LockMetricSnapshot},
    logging::category_program,
};

mod http_plane;
mod transport_plane;
mod vm_runner;

const MAX_LATENCY_SAMPLES: usize = 4096;
pub const VM_EPOCH_TICK_INTERVAL_MS: u64 = 1;

#[cfg(feature = "http3")]
pub use http_plane::serve_http3_proxy;
pub(crate) use http_plane::{
    auto_promote_downstream_listener_goal_into_http_request,
    maybe_auto_promote_downstream_listener_goal_into_http_request,
    promote_transport_context_into_http_request, scoped_http_host_call_can_run_synchronously,
};
pub use http_plane::{build_admin_app, build_http_proxy_app, serve_http_proxy, serve_https_proxy};
pub use transport_plane::serve_transport_proxy;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VmInterruptConfig {
    #[default]
    None,
    Fuel {
        fuel_per_yield: u64,
        check_interval: u32,
    },
    Epoch {
        ticks_per_slice: u64,
        check_interval: u32,
    },
}

impl VmInterruptConfig {
    pub fn kind_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Fuel { .. } => "fuel",
            Self::Epoch { .. } => "epoch",
        }
    }

    pub fn check_interval(self) -> u32 {
        match self {
            Self::None => 1,
            Self::Fuel { check_interval, .. } | Self::Epoch { check_interval, .. } => {
                check_interval
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmExecutionConfig {
    pub interrupt: VmInterruptConfig,
    pub execution_mode: VmExecutionMode,
}

impl Default for VmExecutionConfig {
    fn default() -> Self {
        Self {
            interrupt: VmInterruptConfig::None,
            execution_mode: VmExecutionMode::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeStoreLimits {
    pub tls_session_reuse_entries: usize,
    pub upstream_http_reuse_entries: usize,
    pub downstream_http2_session_entries: usize,
    pub upstream_http3_reuse_entries: usize,
    pub downstream_http3_session_entries: usize,
}

impl Default for RuntimeStoreLimits {
    fn default() -> Self {
        Self {
            tls_session_reuse_entries: DEFAULT_TLS_SESSION_REUSE_STORE_CAPACITY,
            upstream_http_reuse_entries: DEFAULT_UPSTREAM_HTTP_REUSE_STORE_CAPACITY,
            downstream_http2_session_entries: DEFAULT_DOWNSTREAM_HTTP2_SESSION_STORE_CAPACITY,
            upstream_http3_reuse_entries: DEFAULT_UPSTREAM_HTTP3_REUSE_STORE_CAPACITY,
            downstream_http3_session_entries: DEFAULT_DOWNSTREAM_HTTP3_SESSION_STORE_CAPACITY,
        }
    }
}

#[derive(Clone)]
pub struct SharedState {
    pub active_program: Arc<ArcSwapOption<LoadedProgram>>,
    pub max_program_bytes: usize,
    pub client: reqwest::Client,
    pub(crate) downstream_http2_sessions: SharedHttpDownstreamSessions,
    #[cfg_attr(not(feature = "http3"), allow(dead_code))]
    pub(crate) downstream_http3_sessions: SharedHttp3DownstreamSessions,
    pub rate_limiter: SharedRateLimiter,
    pub(crate) runtime_services: SharedRuntimeServices,
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
    #[serde(default)]
    pub lock_metrics: Vec<LockMetricSnapshot>,
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
        Self::new_with_store_limits(max_program_bytes, RuntimeStoreLimits::default())
    }

    pub fn new_with_store_limits(
        max_program_bytes: usize,
        store_limits: RuntimeStoreLimits,
    ) -> Self {
        let client = upstream_reqwest_client_builder()
            .pool_max_idle_per_host(store_limits.upstream_http_reuse_entries.max(1))
            .build()
            .expect("default upstream client should build");
        let upstream_client_cache =
            new_shared_upstream_client_cache(store_limits.upstream_http_reuse_entries);
        let tls_session_cache =
            new_shared_tls_session_cache(store_limits.tls_session_reuse_entries);
        let upstream_http_sessions =
            new_shared_http_upstream_sessions(store_limits.upstream_http_reuse_entries);
        let upstream_http3_sessions =
            new_shared_http3_upstream_sessions(store_limits.upstream_http3_reuse_entries);
        let downstream_http2_sessions =
            new_shared_http_downstream_sessions(store_limits.downstream_http2_session_entries);
        let downstream_http3_sessions =
            new_shared_http3_downstream_sessions(store_limits.downstream_http3_session_entries);
        let rate_limiter = Arc::new(RateLimiterStore::new());
        let runtime_services = new_shared_http_plane_runtime_services(
            rate_limiter.clone(),
            client.clone(),
            upstream_client_cache.clone(),
            tls_session_cache.clone(),
            upstream_http_sessions.clone(),
            upstream_http3_sessions.clone(),
            downstream_http2_sessions.clone(),
            downstream_http3_sessions.clone(),
        );
        Self {
            active_program: Arc::new(ArcSwapOption::from(None::<Arc<LoadedProgram>>)),
            max_program_bytes,
            client,
            downstream_http2_sessions,
            downstream_http3_sessions,
            rate_limiter,
            runtime_services,
            debug_session: new_debug_session_store(),
            vm_execution: VmExecutionConfig::default(),
            runtime_metrics: Arc::new(RuntimeMetrics::default()),
        }
    }

    pub fn loaded_program_snapshot(&self) -> Option<Arc<LoadedProgram>> {
        self.active_program.load_full()
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
        let program_loaded = self.loaded_program_snapshot().is_some();
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
        let program_loaded = self.loaded_program_snapshot().is_some();
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
            lock_metrics: lock_metrics::snapshot(),
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

        let mut metrics = format!(
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
        );
        metrics.push_str(&lock_metrics::metrics_text());
        metrics
    }
}

pub fn attach_http_plane_runtime_services(state: &SharedState, vm_context: &mut ProxyVmContext) {
    vm_context.attach_runtime_services(state.runtime_services.clone());
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
    latency_samples_ms: Mutex<LatencySampleBuffers>,
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
            latency_samples_ms: Mutex::new(LatencySampleBuffers::default()),
        }
    }
}

impl RuntimeMetrics {
    fn record_latency_ms(&self, total_latency_ms: u64, upstream_latency_ms: u64) {
        let upstream_latency_ms = upstream_latency_ms.min(total_latency_ms);
        let edge_added_latency_ms = total_latency_ms.saturating_sub(upstream_latency_ms);
        let mut samples = self
            .latency_samples_ms
            .lock()
            .expect("latency samples lock poisoned");
        Self::push_latency_sample(&mut samples.total, total_latency_ms);
        Self::push_latency_sample(&mut samples.upstream, upstream_latency_ms);
        Self::push_latency_sample(&mut samples.edge_added, edge_added_latency_ms);
    }

    fn push_latency_sample(target: &mut VecDeque<u64>, value: u64) {
        target.push_back(value);
        while target.len() > MAX_LATENCY_SAMPLES {
            let _ = target.pop_front();
        }
    }

    fn take_latency_percentiles_ms(&self) -> LatencySampleGroup {
        let (total, upstream, edge_added) = {
            let mut samples = self
                .latency_samples_ms
                .lock()
                .expect("latency samples lock poisoned");
            (
                samples.total.drain(..).collect::<Vec<_>>(),
                samples.upstream.drain(..).collect::<Vec<_>>(),
                samples.edge_added.drain(..).collect::<Vec<_>>(),
            )
        };
        LatencySampleGroup {
            total: latency_percentiles_from_values(total),
            upstream: latency_percentiles_from_values(upstream),
            edge_added: latency_percentiles_from_values(edge_added),
        }
    }
}

#[derive(Default)]
struct LatencySampleBuffers {
    total: VecDeque<u64>,
    upstream: VecDeque<u64>,
    edge_added: VecDeque<u64>,
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
    state.active_program.store(Some(Arc::new(LoadedProgram {
        program: Arc::new(program),
    })));
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

#[cfg(test)]
mod tests {
    use super::{RuntimeStoreLimits, SharedState};

    #[test]
    fn shared_state_uses_default_store_limits() {
        let state = SharedState::new(1024);
        let services = state.runtime_services.as_ref();

        let tls_capacity = services
            .tls_session_cache()
            .expect("tls session cache should exist")
            .capacity();
        let upstream_capacity = services
            .upstream_http_sessions()
            .expect("http upstream session store should exist")
            .capacity();
        let downstream_capacity = state
            .downstream_http2_sessions
            .lock()
            .expect("http downstream session store lock poisoned")
            .capacity();
        let upstream_http3_capacity = services
            .upstream_http3_sessions()
            .expect("http3 upstream session store should exist")
            .lock()
            .expect("http3 upstream session store lock poisoned")
            .capacity();
        let downstream_http3_capacity = state
            .downstream_http3_sessions
            .lock()
            .expect("http3 downstream session store lock poisoned")
            .capacity();

        let defaults = RuntimeStoreLimits::default();
        assert_eq!(tls_capacity, defaults.tls_session_reuse_entries);
        assert_eq!(
            upstream_capacity,
            if cfg!(feature = "http2") {
                defaults.upstream_http_reuse_entries
            } else {
                0
            }
        );
        assert_eq!(
            downstream_capacity,
            defaults.downstream_http2_session_entries
        );
        assert_eq!(
            upstream_http3_capacity,
            if cfg!(feature = "http3") {
                defaults.upstream_http3_reuse_entries
            } else {
                0
            }
        );
        assert_eq!(
            downstream_http3_capacity,
            defaults.downstream_http3_session_entries
        );
    }

    #[test]
    fn shared_state_accepts_custom_store_limits() {
        let state = SharedState::new_with_store_limits(
            1024,
            RuntimeStoreLimits {
                tls_session_reuse_entries: 8,
                upstream_http_reuse_entries: 16,
                downstream_http2_session_entries: 4,
                upstream_http3_reuse_entries: 6,
                downstream_http3_session_entries: 5,
            },
        );

        let services = state.runtime_services.as_ref();
        assert_eq!(
            services
                .tls_session_cache()
                .expect("tls session cache should exist")
                .capacity(),
            8
        );
        assert_eq!(
            services
                .upstream_http_sessions()
                .expect("http upstream session store should exist")
                .capacity(),
            if cfg!(feature = "http2") { 16 } else { 0 }
        );
        assert_eq!(
            state
                .downstream_http2_sessions
                .lock()
                .expect("http downstream session store lock poisoned")
                .capacity(),
            4
        );
        assert_eq!(
            services
                .upstream_http3_sessions()
                .expect("http3 upstream session store should exist")
                .lock()
                .expect("http3 upstream session store lock poisoned")
                .capacity(),
            if cfg!(feature = "http3") { 6 } else { 0 }
        );
        assert_eq!(
            state
                .downstream_http3_sessions
                .lock()
                .expect("http3 downstream session store lock poisoned")
                .capacity(),
            5
        );
    }
}
