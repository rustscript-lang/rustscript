pub(crate) use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) use base64::{Engine as _, engine::general_purpose::STANDARD};
pub(crate) use edge::{
    CommandResultPayload, ControlPlaneCommand, EdgeCommandResult, EdgePollRequest,
    EdgePollResponse, EdgeTrafficSample, ProgramApplyReport, TelemetrySnapshot,
};
pub(crate) use pd_controller::{
    ControllerConfig, ControllerState, EdgeDetailResponse, build_controller_app,
};
pub(crate) use tokio::task::JoinHandle;
pub(crate) use uuid::Uuid;
pub(crate) use vm::{SourceFlavor, compile_source_with_flavor, decode_program};

static TEST_STATE_PATH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) async fn spawn_controller(
    config: ControllerConfig,
) -> (SocketAddr, JoinHandle<()>, ControllerState) {
    let state = ControllerState::new(config);
    let app = build_controller_app(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("controller should run");
    });
    (addr, handle, state)
}

pub(crate) fn empty_telemetry() -> TelemetrySnapshot {
    TelemetrySnapshot {
        uptime_seconds: 0,
        program_loaded: false,
        debug_session_active: false,
        debug_session_attached: false,
        debug_session_current_line: None,
        debug_session_request_id: None,
        data_requests_total: 0,
        vm_execution_errors_total: 0,
        program_apply_success_total: 0,
        program_apply_failure_total: 0,
        control_rpc_polls_success_total: 0,
        control_rpc_polls_error_total: 0,
        control_rpc_results_success_total: 0,
        control_rpc_results_error_total: 0,
        lock_metrics: Vec::new(),
    }
}

pub(crate) fn unique_state_path(test_name: &str) -> PathBuf {
    let seq = TEST_STATE_PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("pd-controller-{test_name}-{now}-{seq}.json"))
}

pub(crate) fn snapshot_sidecar_paths(state_path: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let parent = state_path
        .parent()
        .map(ToOwned::to_owned)
        .unwrap_or_else(std::env::temp_dir);
    let stem = state_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("state");
    (
        parent.join(format!("{stem}.programs.json")),
        parent.join(format!("{stem}.timeseries.bin")),
        parent.join(format!("{stem}.recordings.json")),
        parent.join(format!("{stem}.debug-sessions.json")),
    )
}
