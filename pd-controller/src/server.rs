use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::{Path as FsPath, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{
        Path, Query, Request, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{StatusCode, header::CONTENT_TYPE},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use edge::{
    CommandResultPayload, ControlPlaneCommand, DebugSessionMode, EdgeCommandResult,
    EdgePollRequest, EdgePollResponse, EdgeTrafficSample, RemoteDebugCommand, TelemetrySnapshot,
};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{oneshot, watch},
    time::{Duration, timeout},
};
use tracing::{info, warn};
use uuid::Uuid;
use vm::{
    SourceFlavor, VmRecording, VmRecordingReplayState, compile_source_with_flavor, encode_program,
    run_recording_replay_command,
};

const MAX_UPLOAD_BYTES: usize = 8 * 1024 * 1024;
const MAX_UI_BLOCKS: usize = 256;
const MAX_TRAFFIC_POINTS: usize = 720;
const PERSISTENCE_SCHEMA_VERSION: u32 = 1;
const RECORDINGS_SCHEMA_VERSION: u32 = 1;
const DEBUG_SESSIONS_SCHEMA_VERSION: u32 = 1;
const TIMESERIES_SCHEMA_VERSION_V1: u32 = 1;
const TIMESERIES_SCHEMA_VERSION_V2: u32 = 2;
const TIMESERIES_SCHEMA_VERSION: u32 = 3;
const TIMESERIES_BINARY_MAGIC: [u8; 4] = *b"PDTS";
const DEBUG_RESUME_GRACE_MS: u64 = 1_500;
const DEFAULT_RECORDING_COUNT: u32 = 1;

type DebugCommandWaiters =
    tokio::sync::Mutex<HashMap<String, oneshot::Sender<Result<DebugCommandResponse, String>>>>;
type SnapshotLoadResult = (
    ControllerStore,
    u64,
    u64,
    HashMap<String, DebugSessionRecord>,
    HashMap<String, Vec<StoredDebugRecording>>,
);

mod handlers;
mod ui_codegen;

use self::handlers::*;
use self::ui_codegen::{parse_ui_flavor, render_ui_sources, source_for_flavor, ui_block_catalog};

mod embedded_webui {
    include!(concat!(env!("OUT_DIR"), "/embedded_webui.rs"));
}

#[derive(Clone, Debug)]
pub struct ControllerConfig {
    pub default_poll_interval_ms: u64,
    pub max_result_history: usize,
    pub state_path: Option<PathBuf>,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            default_poll_interval_ms: 1_000,
            max_result_history: 200,
            state_path: None,
        }
    }
}

#[derive(Clone)]
pub struct ControllerState {
    inner: Arc<tokio::sync::RwLock<ControllerStore>>,
    metrics: Arc<ControllerMetrics>,
    command_sequence: Arc<AtomicU64>,
    program_sequence: Arc<AtomicU64>,
    debug_sessions: Arc<tokio::sync::RwLock<HashMap<String, DebugSessionRecord>>>,
    debug_recordings: Arc<tokio::sync::RwLock<HashMap<String, Vec<StoredDebugRecording>>>>,
    debug_start_lookup: Arc<tokio::sync::RwLock<HashMap<String, String>>>,
    debug_command_waiters: Arc<DebugCommandWaiters>,
    debug_sessions_revision: Arc<AtomicU64>,
    debug_sessions_notify: watch::Sender<u64>,
    persist_lock: Arc<tokio::sync::Mutex<()>>,
    config: ControllerConfig,
}

#[derive(Default)]
struct ControllerStore {
    edges: HashMap<String, EdgeRecord>,
    edge_lookup: HashMap<String, String>,
    programs: HashMap<String, StoredProgram>,
}

#[derive(Default)]
struct EdgeRecord {
    edge_name: String,
    pending_commands: VecDeque<ControlPlaneCommand>,
    recent_results: VecDeque<EdgeCommandResult>,
    pending_apply_programs: HashMap<String, AppliedProgramRef>,
    applied_program: Option<AppliedProgramRef>,
    traffic_points: VecDeque<EdgeTrafficPoint>,
    last_traffic_cumulative: Option<EdgeTrafficSample>,
    last_poll_unix_ms: Option<u64>,
    last_result_unix_ms: Option<u64>,
    last_telemetry: Option<TelemetrySnapshot>,
    total_polls: u64,
    total_results: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DebugSessionPhase {
    Queued,
    WaitingForStartResult,
    WaitingForAttach,
    WaitingForRecordings,
    Attached,
    ReplayReady,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DebugRecordingSummary {
    pub recording_id: String,
    pub sequence: u32,
    pub created_unix_ms: u64,
    pub frame_count: u32,
    pub terminal_status: Option<String>,
    pub request_id: Option<String>,
    pub request_path: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DebugSessionSummary {
    pub session_id: String,
    pub edge_id: String,
    pub edge_name: String,
    pub phase: DebugSessionPhase,
    pub mode: DebugSessionMode,
    pub header_name: Option<String>,
    pub nonce_header_value: Option<String>,
    pub request_id: Option<String>,
    pub request_path: Option<String>,
    pub recording_target_count: Option<u32>,
    pub recording_count: u32,
    pub current_line: Option<u32>,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DebugSessionDetail {
    pub session_id: String,
    pub edge_id: String,
    pub edge_name: String,
    pub phase: DebugSessionPhase,
    pub mode: DebugSessionMode,
    pub header_name: Option<String>,
    pub nonce_header_value: Option<String>,
    pub request_id: Option<String>,
    pub tcp_addr: String,
    pub request_path: Option<String>,
    pub recording_target_count: Option<u32>,
    pub recordings: Vec<DebugRecordingSummary>,
    pub selected_recording_id: Option<String>,
    pub start_command_id: String,
    pub stop_command_id: Option<String>,
    pub current_line: Option<u32>,
    pub source_flavor: Option<String>,
    pub source_code: Option<String>,
    pub breakpoints: Vec<u32>,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
    pub attached_unix_ms: Option<u64>,
    pub message: Option<String>,
    pub last_output: Option<String>,
}

#[derive(Clone, Debug)]
struct DebugSessionRecord {
    session_id: String,
    edge_id: String,
    edge_name: String,
    phase: DebugSessionPhase,
    mode: DebugSessionMode,
    requested_header_name: Option<String>,
    header_name: Option<String>,
    nonce_header_value: Option<String>,
    request_id: Option<String>,
    tcp_addr: String,
    request_path: Option<String>,
    recording_target_count: Option<u32>,
    recordings: Vec<DebugRecordingSummary>,
    selected_recording_id: Option<String>,
    start_command_id: String,
    stop_command_id: Option<String>,
    current_line: Option<u32>,
    source_flavor: Option<String>,
    source_code: Option<String>,
    breakpoints: HashSet<u32>,
    created_unix_ms: u64,
    updated_unix_ms: u64,
    attached_unix_ms: Option<u64>,
    last_resume_command_unix_ms: Option<u64>,
    message: Option<String>,
    last_output: Option<String>,
    replay_states: HashMap<String, VmRecordingReplayState>,
}

impl DebugSessionRecord {
    fn to_summary(&self) -> DebugSessionSummary {
        DebugSessionSummary {
            session_id: self.session_id.clone(),
            edge_id: self.edge_id.clone(),
            edge_name: self.edge_name.clone(),
            phase: self.phase.clone(),
            mode: self.mode.clone(),
            header_name: self.header_name.clone(),
            nonce_header_value: self.nonce_header_value.clone(),
            request_id: self.request_id.clone(),
            request_path: self.request_path.clone(),
            recording_target_count: self.recording_target_count,
            recording_count: self.recordings.len() as u32,
            current_line: self.current_line,
            created_unix_ms: self.created_unix_ms,
            updated_unix_ms: self.updated_unix_ms,
            message: self.message.clone(),
        }
    }

    fn to_detail(&self) -> DebugSessionDetail {
        let mut breakpoints = self.breakpoints.iter().copied().collect::<Vec<_>>();
        breakpoints.sort_unstable();
        DebugSessionDetail {
            session_id: self.session_id.clone(),
            edge_id: self.edge_id.clone(),
            edge_name: self.edge_name.clone(),
            phase: self.phase.clone(),
            mode: self.mode.clone(),
            header_name: self.header_name.clone(),
            nonce_header_value: self.nonce_header_value.clone(),
            request_id: self.request_id.clone(),
            tcp_addr: self.tcp_addr.clone(),
            request_path: self.request_path.clone(),
            recording_target_count: self.recording_target_count,
            recordings: self.recordings.clone(),
            selected_recording_id: self.selected_recording_id.clone(),
            start_command_id: self.start_command_id.clone(),
            stop_command_id: self.stop_command_id.clone(),
            current_line: self.current_line,
            source_flavor: self.source_flavor.clone(),
            source_code: self.source_code.clone(),
            breakpoints,
            created_unix_ms: self.created_unix_ms,
            updated_unix_ms: self.updated_unix_ms,
            attached_unix_ms: self.attached_unix_ms,
            message: self.message.clone(),
            last_output: self.last_output.clone(),
        }
    }

    fn to_persisted(&self) -> PersistedDebugSessionRecord {
        let mut breakpoints = self.breakpoints.iter().copied().collect::<Vec<_>>();
        breakpoints.sort_unstable();
        let replay_states = self
            .replay_states
            .iter()
            .map(|(recording_id, state)| {
                let mut offset_breakpoints =
                    state.offset_breakpoints.iter().copied().collect::<Vec<_>>();
                offset_breakpoints.sort_unstable();
                let mut line_breakpoints =
                    state.line_breakpoints.iter().copied().collect::<Vec<_>>();
                line_breakpoints.sort_unstable();
                (
                    recording_id.clone(),
                    PersistedReplayState {
                        cursor: state.cursor,
                        offset_breakpoints,
                        line_breakpoints,
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        PersistedDebugSessionRecord {
            session_id: self.session_id.clone(),
            edge_id: self.edge_id.clone(),
            edge_name: self.edge_name.clone(),
            phase: self.phase.clone(),
            mode: self.mode.clone(),
            requested_header_name: self.requested_header_name.clone(),
            header_name: self.header_name.clone(),
            nonce_header_value: self.nonce_header_value.clone(),
            request_id: self.request_id.clone(),
            tcp_addr: self.tcp_addr.clone(),
            request_path: self.request_path.clone(),
            recording_target_count: self.recording_target_count,
            recordings: self.recordings.clone(),
            selected_recording_id: self.selected_recording_id.clone(),
            start_command_id: self.start_command_id.clone(),
            stop_command_id: self.stop_command_id.clone(),
            current_line: self.current_line,
            source_flavor: self.source_flavor.clone(),
            source_code: self.source_code.clone(),
            breakpoints,
            created_unix_ms: self.created_unix_ms,
            updated_unix_ms: self.updated_unix_ms,
            attached_unix_ms: self.attached_unix_ms,
            last_resume_command_unix_ms: self.last_resume_command_unix_ms,
            message: self.message.clone(),
            last_output: self.last_output.clone(),
            replay_states,
        }
    }

    fn from_persisted(value: PersistedDebugSessionRecord) -> Self {
        let replay_states = value
            .replay_states
            .into_iter()
            .map(|(recording_id, state)| {
                (
                    recording_id,
                    VmRecordingReplayState {
                        cursor: state.cursor,
                        offset_breakpoints: state.offset_breakpoints.into_iter().collect(),
                        line_breakpoints: state.line_breakpoints.into_iter().collect(),
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        Self {
            session_id: value.session_id,
            edge_id: value.edge_id,
            edge_name: value.edge_name,
            phase: value.phase,
            mode: value.mode,
            requested_header_name: value.requested_header_name,
            header_name: value.header_name,
            nonce_header_value: value.nonce_header_value,
            request_id: value.request_id,
            tcp_addr: value.tcp_addr,
            request_path: value.request_path,
            recording_target_count: value.recording_target_count,
            recordings: value.recordings,
            selected_recording_id: value.selected_recording_id,
            start_command_id: value.start_command_id,
            stop_command_id: value.stop_command_id,
            current_line: value.current_line,
            source_flavor: value.source_flavor,
            source_code: value.source_code,
            breakpoints: value.breakpoints.into_iter().collect(),
            created_unix_ms: value.created_unix_ms,
            updated_unix_ms: value.updated_unix_ms,
            attached_unix_ms: value.attached_unix_ms,
            last_resume_command_unix_ms: value.last_resume_command_unix_ms,
            message: value.message,
            last_output: value.last_output,
            replay_states,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredDebugRecording {
    recording_id: String,
    session_id: String,
    edge_id: String,
    edge_name: String,
    sequence: u32,
    created_unix_ms: u64,
    frame_count: u32,
    terminal_status: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
    request_path: Option<String>,
    recording_base64: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ControllerRecordingsSnapshot {
    #[serde(default = "recordings_schema_version")]
    schema_version: u32,
    #[serde(default)]
    recordings: HashMap<String, Vec<StoredDebugRecording>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedReplayState {
    #[serde(default)]
    cursor: usize,
    #[serde(default)]
    offset_breakpoints: Vec<usize>,
    #[serde(default)]
    line_breakpoints: Vec<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedDebugSessionRecord {
    session_id: String,
    edge_id: String,
    edge_name: String,
    phase: DebugSessionPhase,
    mode: DebugSessionMode,
    #[serde(default)]
    requested_header_name: Option<String>,
    #[serde(default)]
    header_name: Option<String>,
    #[serde(default)]
    nonce_header_value: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    tcp_addr: String,
    #[serde(default)]
    request_path: Option<String>,
    #[serde(default)]
    recording_target_count: Option<u32>,
    #[serde(default)]
    recordings: Vec<DebugRecordingSummary>,
    #[serde(default)]
    selected_recording_id: Option<String>,
    start_command_id: String,
    #[serde(default)]
    stop_command_id: Option<String>,
    #[serde(default)]
    current_line: Option<u32>,
    #[serde(default)]
    source_flavor: Option<String>,
    #[serde(default)]
    source_code: Option<String>,
    #[serde(default)]
    breakpoints: Vec<u32>,
    created_unix_ms: u64,
    updated_unix_ms: u64,
    #[serde(default)]
    attached_unix_ms: Option<u64>,
    #[serde(default)]
    last_resume_command_unix_ms: Option<u64>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    last_output: Option<String>,
    #[serde(default)]
    replay_states: HashMap<String, PersistedReplayState>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ControllerDebugSessionsSnapshot {
    #[serde(default = "debug_sessions_schema_version")]
    schema_version: u32,
    #[serde(default)]
    sessions: HashMap<String, PersistedDebugSessionRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ControllerCoreSnapshot {
    #[serde(default = "snapshot_schema_version")]
    schema_version: u32,
    #[serde(default)]
    command_sequence: u64,
    #[serde(default)]
    program_sequence: u64,
    #[serde(default)]
    edges: HashMap<String, PersistedEdgeCoreRecord>,
    #[serde(default)]
    edge_lookup: HashMap<String, String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ControllerProgramsSnapshot {
    #[serde(default = "snapshot_schema_version")]
    schema_version: u32,
    #[serde(default)]
    programs: HashMap<String, StoredProgram>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ControllerTimeseriesSnapshot {
    #[serde(default = "timeseries_schema_version")]
    schema_version: u32,
    #[serde(default)]
    edges: HashMap<String, PersistedEdgeTimeseriesRecord>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PersistedControllerStore {
    #[serde(default)]
    edges: HashMap<String, PersistedEdgeMergedRecord>,
    #[serde(default)]
    edge_lookup: HashMap<String, String>,
    #[serde(default)]
    programs: HashMap<String, StoredProgram>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PersistedEdgeCoreRecord {
    #[serde(default)]
    edge_id: Option<String>,
    #[serde(default)]
    edge_name: Option<String>,
    #[serde(default)]
    applied_program: Option<AppliedProgramRef>,
    #[serde(default)]
    last_poll_unix_ms: Option<u64>,
    #[serde(default)]
    last_result_unix_ms: Option<u64>,
    #[serde(default)]
    last_telemetry: Option<TelemetrySnapshot>,
    #[serde(default)]
    total_polls: u64,
    #[serde(default)]
    total_results: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PersistedEdgeTimeseriesRecord {
    #[serde(default)]
    traffic_points: VecDeque<EdgeTrafficPoint>,
    #[serde(default)]
    last_traffic_cumulative: Option<EdgeTrafficSample>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PersistedEdgeMergedRecord {
    #[serde(default)]
    edge_id: Option<String>,
    #[serde(default)]
    edge_name: Option<String>,
    #[serde(default)]
    applied_program: Option<AppliedProgramRef>,
    #[serde(default)]
    traffic_points: VecDeque<EdgeTrafficPoint>,
    #[serde(default)]
    last_traffic_cumulative: Option<EdgeTrafficSample>,
    #[serde(default)]
    last_poll_unix_ms: Option<u64>,
    #[serde(default)]
    last_result_unix_ms: Option<u64>,
    #[serde(default)]
    last_telemetry: Option<TelemetrySnapshot>,
    #[serde(default)]
    total_polls: u64,
    #[serde(default)]
    total_results: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ControllerSnapshotLegacy {
    #[serde(default = "snapshot_schema_version")]
    schema_version: u32,
    #[serde(default)]
    command_sequence: u64,
    #[serde(default)]
    program_sequence: u64,
    #[serde(default)]
    store: PersistedControllerStore,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppliedProgramRef {
    pub program_id: String,
    pub name: String,
    pub version: u32,
}

struct ControllerMetrics {
    started_at: Instant,
    poll_requests_total: AtomicU64,
    result_posts_total: AtomicU64,
    commands_enqueued_total: AtomicU64,
    commands_delivered_total: AtomicU64,
    command_results_ok_total: AtomicU64,
    command_results_error_total: AtomicU64,
}

impl Default for ControllerMetrics {
    fn default() -> Self {
        Self {
            started_at: Instant::now(),
            poll_requests_total: AtomicU64::new(0),
            result_posts_total: AtomicU64::new(0),
            commands_enqueued_total: AtomicU64::new(0),
            commands_delivered_total: AtomicU64::new(0),
            command_results_ok_total: AtomicU64::new(0),
            command_results_error_total: AtomicU64::new(0),
        }
    }
}

impl ControllerState {
    pub fn new(config: ControllerConfig) -> Self {
        let (store, command_sequence, program_sequence, debug_sessions, debug_recordings) =
            load_snapshot_from_disk(config.state_path.as_deref(), config.max_result_history);
        let (debug_sessions_notify, _debug_sessions_rx) = watch::channel(0_u64);
        let debug_start_lookup = debug_sessions
            .values()
            .filter(|session| session.phase == DebugSessionPhase::WaitingForStartResult)
            .map(|session| (session.start_command_id.clone(), session.session_id.clone()))
            .collect::<HashMap<_, _>>();
        Self {
            inner: Arc::new(tokio::sync::RwLock::new(store)),
            metrics: Arc::new(ControllerMetrics::default()),
            command_sequence: Arc::new(AtomicU64::new(command_sequence)),
            program_sequence: Arc::new(AtomicU64::new(program_sequence)),
            debug_sessions: Arc::new(tokio::sync::RwLock::new(debug_sessions)),
            debug_recordings: Arc::new(tokio::sync::RwLock::new(debug_recordings)),
            debug_start_lookup: Arc::new(tokio::sync::RwLock::new(debug_start_lookup)),
            debug_command_waiters: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            debug_sessions_revision: Arc::new(AtomicU64::new(0)),
            debug_sessions_notify,
            persist_lock: Arc::new(tokio::sync::Mutex::new(())),
            config,
        }
    }

    fn next_command_id(&self) -> String {
        let id = self.command_sequence.fetch_add(1, Ordering::Relaxed) + 1;
        format!("cmd-{id}")
    }

    fn next_program_id(&self) -> String {
        Uuid::new_v4().to_string()
    }

    fn notify_debug_sessions_changed(&self) {
        let revision = self
            .debug_sessions_revision
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let _ = self.debug_sessions_notify.send(revision);
    }

    fn subscribe_debug_sessions(&self) -> watch::Receiver<u64> {
        self.debug_sessions_notify.subscribe()
    }

    async fn debug_sessions_snapshot(
        &self,
        selected_session_id: Option<&str>,
    ) -> DebugSessionsStreamSnapshot {
        let (mut sessions, selected_session) = {
            let guard = self.debug_sessions.read().await;
            let sessions = guard
                .values()
                .map(DebugSessionRecord::to_summary)
                .collect::<Vec<_>>();
            let selected_session = selected_session_id
                .and_then(|session_id| guard.get(session_id).map(DebugSessionRecord::to_detail));
            (sessions, selected_session)
        };
        sessions.sort_by(|lhs, rhs| rhs.updated_unix_ms.cmp(&lhs.updated_unix_ms));
        DebugSessionsStreamSnapshot {
            kind: "snapshot",
            sessions,
            selected_session,
        }
    }

    async fn enqueue_command(
        &self,
        edge_identifier: String,
        command: ControlPlaneCommand,
    ) -> EnqueueCommandResponse {
        self.enqueue_command_tracked(edge_identifier, command, None)
            .await
    }

    async fn enqueue_command_tracked(
        &self,
        edge_identifier: String,
        command: ControlPlaneCommand,
        apply_program: Option<AppliedProgramRef>,
    ) -> EnqueueCommandResponse {
        let command_id = command.command_id().to_string();
        let pending_commands = {
            let mut guard = self.inner.write().await;
            let edge_id = guard.resolve_or_create_edge_id(&edge_identifier);
            let record = guard.edges.entry(edge_id).or_default();
            if record.edge_name.is_empty() {
                record.edge_name = edge_identifier.clone();
            }
            record.pending_commands.push_back(command);
            if let Some(program_ref) = apply_program {
                record
                    .pending_apply_programs
                    .insert(command_id.clone(), program_ref);
            }
            record.pending_commands.len()
        };
        self.metrics
            .commands_enqueued_total
            .fetch_add(1, Ordering::Relaxed);
        EnqueueCommandResponse {
            command_id,
            pending_commands,
        }
    }

    async fn persist_snapshot(&self) -> Result<(), String> {
        let Some(path) = self.config.state_path.clone() else {
            return Ok(());
        };
        let (programs_path, timeseries_path, recordings_path, debug_sessions_path) =
            sidecar_snapshot_paths(path.as_path());

        let _save_guard = self.persist_lock.lock().await;
        let (
            core_snapshot,
            programs_snapshot,
            timeseries_snapshot,
            recordings_snapshot,
            debug_sessions_snapshot,
        ) = {
            let guard = self.inner.read().await;
            let debug_sessions = self.debug_sessions.read().await;
            let recordings = self.debug_recordings.read().await;
            let persisted = guard.to_persisted();
            let core_edges = persisted
                .edges
                .iter()
                .map(|(edge_id, record)| {
                    (
                        edge_id.clone(),
                        PersistedEdgeCoreRecord {
                            edge_id: record.edge_id.clone(),
                            edge_name: record.edge_name.clone(),
                            applied_program: record.applied_program.clone(),
                            last_poll_unix_ms: record.last_poll_unix_ms,
                            last_result_unix_ms: record.last_result_unix_ms,
                            last_telemetry: record.last_telemetry.clone(),
                            total_polls: record.total_polls,
                            total_results: record.total_results,
                        },
                    )
                })
                .collect::<HashMap<_, _>>();
            let timeseries_edges = persisted
                .edges
                .iter()
                .map(|(edge_id, record)| {
                    (
                        edge_id.clone(),
                        PersistedEdgeTimeseriesRecord {
                            traffic_points: record.traffic_points.clone(),
                            last_traffic_cumulative: record.last_traffic_cumulative.clone(),
                        },
                    )
                })
                .collect::<HashMap<_, _>>();
            (
                ControllerCoreSnapshot {
                    schema_version: PERSISTENCE_SCHEMA_VERSION,
                    command_sequence: self.command_sequence.load(Ordering::Relaxed),
                    program_sequence: self.program_sequence.load(Ordering::Relaxed),
                    edges: core_edges,
                    edge_lookup: persisted.edge_lookup.clone(),
                },
                ControllerProgramsSnapshot {
                    schema_version: PERSISTENCE_SCHEMA_VERSION,
                    programs: persisted.programs.clone(),
                },
                ControllerTimeseriesSnapshot {
                    schema_version: TIMESERIES_SCHEMA_VERSION,
                    edges: timeseries_edges,
                },
                ControllerRecordingsSnapshot {
                    schema_version: RECORDINGS_SCHEMA_VERSION,
                    recordings: recordings.clone(),
                },
                ControllerDebugSessionsSnapshot {
                    schema_version: DEBUG_SESSIONS_SCHEMA_VERSION,
                    sessions: debug_sessions
                        .iter()
                        .map(|(session_id, session)| (session_id.clone(), session.to_persisted()))
                        .collect(),
                },
            )
        };
        write_snapshot_to_disk(path.as_path(), &core_snapshot)?;
        write_snapshot_to_disk(programs_path.as_path(), &programs_snapshot)?;
        write_timeseries_snapshot_to_disk(timeseries_path.as_path(), &timeseries_snapshot)?;
        write_snapshot_to_disk(recordings_path.as_path(), &recordings_snapshot)?;
        write_snapshot_to_disk(debug_sessions_path.as_path(), &debug_sessions_snapshot)?;
        Ok(())
    }
}

impl ControllerStore {
    fn resolve_edge_id(&self, identifier: &str) -> Option<String> {
        if self.edges.contains_key(identifier) {
            return Some(identifier.to_string());
        }
        self.edge_lookup.get(identifier).cloned()
    }

    fn resolve_or_create_edge_id(&mut self, identifier: &str) -> String {
        if let Some(existing) = self.resolve_edge_id(identifier) {
            return existing;
        }
        let edge_id = Uuid::new_v4().to_string();
        self.edge_lookup
            .insert(identifier.to_string(), edge_id.clone());
        self.edges.insert(
            edge_id.clone(),
            EdgeRecord {
                edge_name: identifier.to_string(),
                ..EdgeRecord::default()
            },
        );
        edge_id
    }

    fn to_persisted(&self) -> PersistedControllerStore {
        PersistedControllerStore {
            edges: self
                .edges
                .iter()
                .map(|(edge_id, record)| {
                    (edge_id.clone(), record.to_persisted(Some(edge_id.clone())))
                })
                .collect(),
            edge_lookup: self.edge_lookup.clone(),
            programs: self.programs.clone(),
        }
    }

    fn from_persisted(
        store: PersistedControllerStore,
        max_result_history: usize,
    ) -> ControllerStore {
        let mut edge_lookup = store.edge_lookup;
        let mut edges = HashMap::new();

        for (stored_key, record) in store.edges {
            let edge_id = record.edge_id.clone().unwrap_or_else(|| {
                if Uuid::parse_str(&stored_key).is_ok() {
                    stored_key.clone()
                } else {
                    Uuid::new_v4().to_string()
                }
            });

            let edge_name = record.edge_name.clone().unwrap_or_else(|| {
                if Uuid::parse_str(&stored_key).is_ok() {
                    edge_lookup
                        .iter()
                        .find_map(|(name, id)| (id == &edge_id).then(|| name.clone()))
                        .unwrap_or_else(|| stored_key.clone())
                } else {
                    stored_key.clone()
                }
            });

            edge_lookup.insert(edge_name.clone(), edge_id.clone());
            edges.insert(
                edge_id,
                EdgeRecord::from_persisted(record, max_result_history, edge_name),
            );
        }

        ControllerStore {
            edges,
            edge_lookup,
            programs: store.programs,
        }
    }
}

impl EdgeRecord {
    fn to_persisted(&self, edge_id: Option<String>) -> PersistedEdgeMergedRecord {
        PersistedEdgeMergedRecord {
            edge_id,
            edge_name: Some(self.edge_name.clone()),
            applied_program: self.applied_program.clone(),
            traffic_points: self.traffic_points.clone(),
            last_traffic_cumulative: self.last_traffic_cumulative.clone(),
            last_poll_unix_ms: self.last_poll_unix_ms,
            last_result_unix_ms: self.last_result_unix_ms,
            last_telemetry: self.last_telemetry.clone(),
            total_polls: self.total_polls,
            total_results: self.total_results,
        }
    }

    fn from_persisted(
        store: PersistedEdgeMergedRecord,
        max_result_history: usize,
        edge_name: String,
    ) -> EdgeRecord {
        let mut record = EdgeRecord {
            edge_name,
            applied_program: store.applied_program,
            traffic_points: store.traffic_points,
            last_traffic_cumulative: store.last_traffic_cumulative,
            last_poll_unix_ms: store.last_poll_unix_ms,
            last_result_unix_ms: store.last_result_unix_ms,
            last_telemetry: store.last_telemetry,
            total_polls: store.total_polls,
            total_results: store.total_results,
            ..EdgeRecord::default()
        };
        record.recent_results.truncate(max_result_history.max(1));
        record
    }
}

fn snapshot_schema_version() -> u32 {
    PERSISTENCE_SCHEMA_VERSION
}

fn timeseries_schema_version() -> u32 {
    TIMESERIES_SCHEMA_VERSION
}

fn recordings_schema_version() -> u32 {
    RECORDINGS_SCHEMA_VERSION
}

fn debug_sessions_schema_version() -> u32 {
    DEBUG_SESSIONS_SCHEMA_VERSION
}

fn default_true() -> bool {
    true
}

fn is_supported_timeseries_schema_version(value: u32) -> bool {
    value == TIMESERIES_SCHEMA_VERSION_V1
        || value == TIMESERIES_SCHEMA_VERSION_V2
        || value == TIMESERIES_SCHEMA_VERSION
}

fn load_snapshot_from_disk(
    state_path: Option<&FsPath>,
    max_result_history: usize,
) -> SnapshotLoadResult {
    let Some(path) = state_path else {
        return (
            ControllerStore::default(),
            0,
            0,
            HashMap::new(),
            HashMap::new(),
        );
    };
    if !path.exists() {
        return (
            ControllerStore::default(),
            0,
            0,
            HashMap::new(),
            HashMap::new(),
        );
    }
    let (programs_path, timeseries_path, recordings_path, debug_sessions_path) =
        sidecar_snapshot_paths(path);

    let data = match fs::read(path) {
        Ok(data) => data,
        Err(err) => {
            warn!(
                "failed to read controller snapshot path={} err={err}",
                path.display()
            );
            return (
                ControllerStore::default(),
                0,
                0,
                HashMap::new(),
                HashMap::new(),
            );
        }
    };
    let snapshot = match serde_json::from_slice::<ControllerCoreSnapshot>(&data) {
        Ok(snapshot) => snapshot,
        Err(_) => {
            // Backward compatibility with previous monolithic state file format.
            let legacy = match serde_json::from_slice::<ControllerSnapshotLegacy>(&data) {
                Ok(snapshot) => snapshot,
                Err(err) => {
                    warn!(
                        "failed to parse controller snapshot path={} err={err}",
                        path.display()
                    );
                    return (
                        ControllerStore::default(),
                        0,
                        0,
                        HashMap::new(),
                        HashMap::new(),
                    );
                }
            };
            if legacy.schema_version != PERSISTENCE_SCHEMA_VERSION {
                warn!(
                    "ignoring controller snapshot path={} unsupported schema_version={}",
                    path.display(),
                    legacy.schema_version
                );
                return (
                    ControllerStore::default(),
                    0,
                    0,
                    HashMap::new(),
                    HashMap::new(),
                );
            }
            return (
                ControllerStore::from_persisted(legacy.store, max_result_history),
                legacy.command_sequence,
                legacy.program_sequence,
                HashMap::new(),
                HashMap::new(),
            );
        }
    };
    if snapshot.schema_version != PERSISTENCE_SCHEMA_VERSION {
        warn!(
            "ignoring controller snapshot path={} unsupported schema_version={}",
            path.display(),
            snapshot.schema_version
        );
        return (
            ControllerStore::default(),
            0,
            0,
            HashMap::new(),
            HashMap::new(),
        );
    }

    let programs = if programs_path.exists() {
        match fs::read(programs_path.as_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ControllerProgramsSnapshot>(&bytes).ok())
        {
            Some(parsed) if parsed.schema_version == PERSISTENCE_SCHEMA_VERSION => parsed.programs,
            Some(_) => {
                warn!(
                    "ignoring controller programs snapshot path={} unsupported schema_version",
                    programs_path.display()
                );
                HashMap::new()
            }
            None => HashMap::new(),
        }
    } else {
        HashMap::new()
    };

    let timeseries = load_timeseries_snapshot(timeseries_path.as_path());

    let mut merged_edges = HashMap::new();
    for (edge_id, core) in snapshot.edges {
        let traffic = timeseries.get(&edge_id);
        merged_edges.insert(
            edge_id.clone(),
            PersistedEdgeMergedRecord {
                edge_id: core.edge_id,
                edge_name: core.edge_name,
                applied_program: core.applied_program,
                traffic_points: traffic
                    .map(|item| item.traffic_points.clone())
                    .unwrap_or_default(),
                last_traffic_cumulative: traffic
                    .and_then(|item| item.last_traffic_cumulative.clone()),
                last_poll_unix_ms: core.last_poll_unix_ms,
                last_result_unix_ms: core.last_result_unix_ms,
                last_telemetry: core.last_telemetry,
                total_polls: core.total_polls,
                total_results: core.total_results,
            },
        );
    }

    let store = PersistedControllerStore {
        edges: merged_edges,
        edge_lookup: snapshot.edge_lookup,
        programs,
    };

    let recordings = if recordings_path.exists() {
        match fs::read(recordings_path.as_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ControllerRecordingsSnapshot>(&bytes).ok())
        {
            Some(parsed) if parsed.schema_version == RECORDINGS_SCHEMA_VERSION => parsed.recordings,
            Some(_) => {
                warn!(
                    "ignoring controller recordings snapshot path={} unsupported schema_version",
                    recordings_path.display()
                );
                HashMap::new()
            }
            None => HashMap::new(),
        }
    } else {
        HashMap::new()
    };

    let debug_sessions = if debug_sessions_path.exists() {
        match fs::read(debug_sessions_path.as_path())
            .ok()
            .and_then(|bytes| {
                serde_json::from_slice::<ControllerDebugSessionsSnapshot>(&bytes).ok()
            }) {
            Some(parsed) if parsed.schema_version == DEBUG_SESSIONS_SCHEMA_VERSION => parsed
                .sessions
                .into_iter()
                .map(|(session_id, value)| (session_id, DebugSessionRecord::from_persisted(value)))
                .collect::<HashMap<_, _>>(),
            Some(_) => {
                warn!(
                    "ignoring controller debug sessions snapshot path={} unsupported schema_version",
                    debug_sessions_path.display()
                );
                HashMap::new()
            }
            None => HashMap::new(),
        }
    } else {
        HashMap::new()
    };

    (
        ControllerStore::from_persisted(store, max_result_history),
        snapshot.command_sequence,
        snapshot.program_sequence,
        debug_sessions,
        recordings,
    )
}

fn sidecar_snapshot_paths(state_path: &FsPath) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let parent = state_path.parent().unwrap_or_else(|| FsPath::new(""));
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

fn write_snapshot_to_disk<T: Serialize>(path: &FsPath, snapshot: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(snapshot)
        .map_err(|err| format!("failed to serialize controller snapshot: {err}"))?;
    write_bytes_to_disk(path, &bytes)
}

fn write_timeseries_snapshot_to_disk(
    path: &FsPath,
    snapshot: &ControllerTimeseriesSnapshot,
) -> Result<(), String> {
    let bytes = encode_timeseries_snapshot(snapshot)?;
    write_bytes_to_disk(path, &bytes)
}

fn write_bytes_to_disk(path: &FsPath, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create controller state directory {}: {err}",
                parent.display()
            )
        })?;
    }

    let mut temp_name = path.as_os_str().to_os_string();
    temp_name.push(".tmp");
    let temp_path = PathBuf::from(temp_name);
    fs::write(&temp_path, bytes).map_err(|err| {
        format!(
            "failed to write temporary controller snapshot {}: {err}",
            temp_path.display()
        )
    })?;

    if path.exists() {
        let _ = fs::remove_file(path);
    }
    fs::rename(&temp_path, path).map_err(|err| {
        format!(
            "failed to move controller snapshot {} => {}: {err}",
            temp_path.display(),
            path.display()
        )
    })
}

fn load_timeseries_snapshot(
    timeseries_path: &FsPath,
) -> HashMap<String, PersistedEdgeTimeseriesRecord> {
    if timeseries_path.exists() {
        return match fs::read(timeseries_path) {
            Ok(bytes) => match decode_timeseries_snapshot(&bytes) {
                Ok(snapshot) if is_supported_timeseries_schema_version(snapshot.schema_version) => {
                    snapshot.edges
                }
                Ok(_) => {
                    warn!(
                        "ignoring controller timeseries snapshot path={} unsupported schema_version",
                        timeseries_path.display()
                    );
                    HashMap::new()
                }
                Err(err) => {
                    warn!(
                        "failed to parse controller timeseries snapshot path={} err={err}",
                        timeseries_path.display()
                    );
                    HashMap::new()
                }
            },
            Err(err) => {
                warn!(
                    "failed to read controller timeseries snapshot path={} err={err}",
                    timeseries_path.display()
                );
                HashMap::new()
            }
        };
    }

    HashMap::new()
}

fn encode_timeseries_snapshot(snapshot: &ControllerTimeseriesSnapshot) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&TIMESERIES_BINARY_MAGIC);
    put_u32(&mut bytes, snapshot.schema_version);
    put_u32(
        &mut bytes,
        u32::try_from(snapshot.edges.len())
            .map_err(|_| "too many edges in timeseries snapshot".to_string())?,
    );

    for (edge_id, record) in &snapshot.edges {
        put_string(&mut bytes, edge_id)?;
        put_u32(
            &mut bytes,
            u32::try_from(record.traffic_points.len())
                .map_err(|_| format!("too many traffic points for edge {edge_id}"))?,
        );

        for point in &record.traffic_points {
            put_u64(&mut bytes, point.unix_ms);
            put_u64(&mut bytes, point.requests);
            put_u64(&mut bytes, point.status_2xx);
            put_u64(&mut bytes, point.status_3xx);
            put_u64(&mut bytes, point.status_4xx);
            put_u64(&mut bytes, point.status_5xx);
            put_u64(&mut bytes, point.latency_p50_ms);
            put_u64(&mut bytes, point.latency_p90_ms);
            put_u64(&mut bytes, point.latency_p99_ms);
            put_u64(&mut bytes, point.upstream_latency_p50_ms);
            put_u64(&mut bytes, point.upstream_latency_p90_ms);
            put_u64(&mut bytes, point.upstream_latency_p99_ms);
            put_u64(&mut bytes, point.edge_latency_p50_ms);
            put_u64(&mut bytes, point.edge_latency_p90_ms);
            put_u64(&mut bytes, point.edge_latency_p99_ms);
        }

        match &record.last_traffic_cumulative {
            Some(sample) => {
                put_u8(&mut bytes, 1);
                put_u64(&mut bytes, sample.requests_total);
                put_u64(&mut bytes, sample.status_2xx_total);
                put_u64(&mut bytes, sample.status_3xx_total);
                put_u64(&mut bytes, sample.status_4xx_total);
                put_u64(&mut bytes, sample.status_5xx_total);
                put_u64(&mut bytes, sample.latency_p50_ms);
                put_u64(&mut bytes, sample.latency_p90_ms);
                put_u64(&mut bytes, sample.latency_p99_ms);
                put_u64(&mut bytes, sample.upstream_latency_p50_ms);
                put_u64(&mut bytes, sample.upstream_latency_p90_ms);
                put_u64(&mut bytes, sample.upstream_latency_p99_ms);
                put_u64(&mut bytes, sample.edge_latency_p50_ms);
                put_u64(&mut bytes, sample.edge_latency_p90_ms);
                put_u64(&mut bytes, sample.edge_latency_p99_ms);
            }
            None => put_u8(&mut bytes, 0),
        }
    }

    Ok(bytes)
}

fn decode_timeseries_snapshot(bytes: &[u8]) -> Result<ControllerTimeseriesSnapshot, String> {
    let mut cursor = BytesCursor::new(bytes);
    let magic = cursor.read_bytes(TIMESERIES_BINARY_MAGIC.len())?;
    if magic != TIMESERIES_BINARY_MAGIC {
        return Err("unexpected timeseries binary magic".to_string());
    }
    let schema_version = cursor.read_u32()?;
    let edge_count = cursor.read_u32()?;

    let mut edges = HashMap::with_capacity(edge_count as usize);
    for _ in 0..edge_count {
        let edge_id = cursor.read_string()?;
        let point_count = cursor.read_u32()?;
        let mut traffic_points = VecDeque::with_capacity(point_count as usize);
        for _ in 0..point_count {
            let mut point = EdgeTrafficPoint {
                unix_ms: cursor.read_u64()?,
                requests: cursor.read_u64()?,
                status_2xx: cursor.read_u64()?,
                status_3xx: cursor.read_u64()?,
                status_4xx: cursor.read_u64()?,
                status_5xx: cursor.read_u64()?,
                latency_p50_ms: 0,
                latency_p90_ms: 0,
                latency_p99_ms: 0,
                upstream_latency_p50_ms: 0,
                upstream_latency_p90_ms: 0,
                upstream_latency_p99_ms: 0,
                edge_latency_p50_ms: 0,
                edge_latency_p90_ms: 0,
                edge_latency_p99_ms: 0,
            };
            if schema_version >= TIMESERIES_SCHEMA_VERSION_V2 {
                point.latency_p50_ms = cursor.read_u64()?;
                point.latency_p90_ms = cursor.read_u64()?;
                point.latency_p99_ms = cursor.read_u64()?;
            }
            if schema_version >= TIMESERIES_SCHEMA_VERSION {
                point.upstream_latency_p50_ms = cursor.read_u64()?;
                point.upstream_latency_p90_ms = cursor.read_u64()?;
                point.upstream_latency_p99_ms = cursor.read_u64()?;
                point.edge_latency_p50_ms = cursor.read_u64()?;
                point.edge_latency_p90_ms = cursor.read_u64()?;
                point.edge_latency_p99_ms = cursor.read_u64()?;
            }
            traffic_points.push_back(point);
        }

        let last_traffic_cumulative = match cursor.read_u8()? {
            0 => None,
            1 => {
                let mut sample = EdgeTrafficSample {
                    requests_total: cursor.read_u64()?,
                    status_2xx_total: cursor.read_u64()?,
                    status_3xx_total: cursor.read_u64()?,
                    status_4xx_total: cursor.read_u64()?,
                    status_5xx_total: cursor.read_u64()?,
                    latency_p50_ms: 0,
                    latency_p90_ms: 0,
                    latency_p99_ms: 0,
                    upstream_latency_p50_ms: 0,
                    upstream_latency_p90_ms: 0,
                    upstream_latency_p99_ms: 0,
                    edge_latency_p50_ms: 0,
                    edge_latency_p90_ms: 0,
                    edge_latency_p99_ms: 0,
                };
                if schema_version >= TIMESERIES_SCHEMA_VERSION_V2 {
                    sample.latency_p50_ms = cursor.read_u64()?;
                    sample.latency_p90_ms = cursor.read_u64()?;
                    sample.latency_p99_ms = cursor.read_u64()?;
                }
                if schema_version >= TIMESERIES_SCHEMA_VERSION {
                    sample.upstream_latency_p50_ms = cursor.read_u64()?;
                    sample.upstream_latency_p90_ms = cursor.read_u64()?;
                    sample.upstream_latency_p99_ms = cursor.read_u64()?;
                    sample.edge_latency_p50_ms = cursor.read_u64()?;
                    sample.edge_latency_p90_ms = cursor.read_u64()?;
                    sample.edge_latency_p99_ms = cursor.read_u64()?;
                }
                Some(sample)
            }
            value => {
                return Err(format!(
                    "invalid last_traffic_cumulative marker for edge {edge_id}: {value}"
                ));
            }
        };

        edges.insert(
            edge_id,
            PersistedEdgeTimeseriesRecord {
                traffic_points,
                last_traffic_cumulative,
            },
        );
    }

    if !cursor.is_eof() {
        return Err("unexpected trailing bytes in timeseries snapshot".to_string());
    }

    Ok(ControllerTimeseriesSnapshot {
        schema_version,
        edges,
    })
}

fn put_u8(bytes: &mut Vec<u8>, value: u8) {
    bytes.push(value);
}

fn put_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_string(bytes: &mut Vec<u8>, value: &str) -> Result<(), String> {
    let raw = value.as_bytes();
    put_u32(
        bytes,
        u32::try_from(raw.len()).map_err(|_| "timeseries string length overflow".to_string())?,
    );
    bytes.extend_from_slice(raw);
    Ok(())
}

struct BytesCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> BytesCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn is_eof(&self) -> bool {
        self.offset == self.bytes.len()
    }

    fn read_u8(&mut self) -> Result<u8, String> {
        let chunk = self.read_bytes(1)?;
        Ok(chunk[0])
    }

    fn read_u32(&mut self) -> Result<u32, String> {
        let chunk = self.read_bytes(4)?;
        let mut raw = [0u8; 4];
        raw.copy_from_slice(chunk);
        Ok(u32::from_le_bytes(raw))
    }

    fn read_u64(&mut self) -> Result<u64, String> {
        let chunk = self.read_bytes(8)?;
        let mut raw = [0u8; 8];
        raw.copy_from_slice(chunk);
        Ok(u64::from_le_bytes(raw))
    }

    fn read_string(&mut self) -> Result<String, String> {
        let len = self.read_u32()? as usize;
        let raw = self.read_bytes(len)?;
        String::from_utf8(raw.to_vec())
            .map_err(|err| format!("invalid utf8 in timeseries snapshot: {err}"))
    }

    fn read_bytes(&mut self, len: usize) -> Result<&'a [u8], String> {
        let next = self
            .offset
            .checked_add(len)
            .ok_or_else(|| "timeseries snapshot offset overflow".to_string())?;
        if next > self.bytes.len() {
            return Err("unexpected end of timeseries snapshot".to_string());
        }
        let slice = &self.bytes[self.offset..next];
        self.offset = next;
        Ok(slice)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnqueueCommandResponse {
    pub command_id: String,
    pub pending_commands: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EdgeSummary {
    pub edge_id: String,
    pub edge_name: String,
    pub sync_status: String,
    pub last_seen_unix_ms: Option<u64>,
    pub pending_commands: usize,
    pub recent_results: usize,
    pub applied_program: Option<AppliedProgramRef>,
    pub last_poll_unix_ms: Option<u64>,
    pub last_result_unix_ms: Option<u64>,
    pub total_polls: u64,
    pub total_results: u64,
    pub last_telemetry: Option<TelemetrySnapshot>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EdgeDetailResponse {
    pub summary: EdgeSummary,
    pub pending_command_types: Vec<String>,
    pub traffic_series: Vec<EdgeTrafficPoint>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EdgeTrafficPoint {
    pub unix_ms: u64,
    pub requests: u64,
    pub status_2xx: u64,
    pub status_3xx: u64,
    pub status_4xx: u64,
    pub status_5xx: u64,
    #[serde(default)]
    pub latency_p50_ms: u64,
    #[serde(default)]
    pub latency_p90_ms: u64,
    #[serde(default)]
    pub latency_p99_ms: u64,
    #[serde(default)]
    pub upstream_latency_p50_ms: u64,
    #[serde(default)]
    pub upstream_latency_p90_ms: u64,
    #[serde(default)]
    pub upstream_latency_p99_ms: u64,
    #[serde(default)]
    pub edge_latency_p50_ms: u64,
    #[serde(default)]
    pub edge_latency_p90_ms: u64,
    #[serde(default)]
    pub edge_latency_p99_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct EdgeListResponse {
    edges: Vec<EdgeSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct EdgeResultsResponse {
    results: Vec<EdgeCommandResult>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DebugSessionListResponse {
    sessions: Vec<DebugSessionSummary>,
}

#[derive(Clone, Debug, Serialize)]
struct DebugSessionsStreamSnapshot {
    kind: &'static str,
    sessions: Vec<DebugSessionSummary>,
    selected_session: Option<DebugSessionDetail>,
}

#[derive(Clone, Debug, Deserialize)]
struct CreateDebugSessionRequest {
    edge_id: String,
    #[serde(default)]
    mode: DebugSessionMode,
    #[serde(default)]
    tcp_addr: Option<String>,
    #[serde(default)]
    header_name: Option<String>,
    #[serde(default)]
    stop_on_entry: Option<bool>,
    #[serde(default)]
    request_path: Option<String>,
    #[serde(default)]
    record_count: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DebugCommandResponse {
    phase: DebugSessionPhase,
    output: String,
    current_line: Option<u32>,
    attached: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DebugCommandRequest {
    Where,
    Step,
    Next,
    Continue,
    Out,
    SelectRecording { recording_id: String },
    BreakLine { line: u32 },
    ClearLine { line: u32 },
    PrintVar { name: String },
    Locals,
    Stack,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProgramSummary {
    program_id: String,
    name: String,
    latest_version: u32,
    versions: usize,
    created_unix_ms: u64,
    updated_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProgramVersionSummary {
    version: u32,
    created_unix_ms: u64,
    flavor: String,
    flow_synced: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProgramVersionDetail {
    version: u32,
    created_unix_ms: u64,
    flavor: String,
    flow_synced: bool,
    nodes: Vec<UiGraphNode>,
    edges: Vec<UiGraphEdge>,
    source: UiSourceBundle,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProgramDetailResponse {
    program_id: String,
    name: String,
    latest_version: u32,
    created_unix_ms: u64,
    updated_unix_ms: u64,
    versions: Vec<ProgramVersionSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProgramVersionResponse {
    program_id: String,
    name: String,
    detail: ProgramVersionDetail,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProgramListResponse {
    programs: Vec<ProgramSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StatusResponse {
    status: &'static str,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Clone, Debug, Deserialize)]
struct EnqueueApplyProgramRequest {
    command_id: Option<String>,
    program_base64: String,
}

#[derive(Clone, Debug, Deserialize)]
struct EnqueueStartDebugRequest {
    command_id: Option<String>,
    #[serde(default)]
    mode: DebugSessionMode,
    #[serde(default)]
    tcp_addr: Option<String>,
    #[serde(default)]
    header_name: Option<String>,
    #[serde(default)]
    stop_on_entry: Option<bool>,
    #[serde(default)]
    request_path: Option<String>,
    #[serde(default)]
    record_count: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
struct EnqueuePingRequest {
    command_id: Option<String>,
    payload: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct OptionalCommandIdRequest {
    command_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ResultsQuery {
    limit: Option<usize>,
}

#[derive(Clone, Debug, Deserialize)]
struct CreateProgramRequest {
    name: String,
}

#[derive(Clone, Debug, Deserialize)]
struct RenameProgramRequest {
    name: String,
}

#[derive(Clone, Debug, Deserialize)]
struct CreateProgramVersionRequest {
    #[serde(default)]
    flavor: Option<String>,
    #[serde(default)]
    nodes: Vec<UiGraphNode>,
    #[serde(default)]
    edges: Vec<UiGraphEdge>,
    #[serde(default)]
    blocks: Vec<UiBlockInstance>,
    #[serde(default)]
    source: Option<UiSourceBundle>,
    #[serde(default = "default_true")]
    flow_synced: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct ApplyProgramVersionRequest {
    program_id: String,
    #[serde(default)]
    version: Option<u32>,
    #[serde(default)]
    flavor: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum UiInputType {
    Text,
    Number,
}

#[derive(Clone, Debug, Serialize)]
struct UiBlockInput {
    key: &'static str,
    label: &'static str,
    input_type: UiInputType,
    default_value: &'static str,
    placeholder: &'static str,
    connectable: bool,
}

#[derive(Clone, Debug, Serialize)]
struct UiBlockOutput {
    key: &'static str,
    label: &'static str,
    expr_from_input: Option<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
struct UiBlockDefinition {
    id: &'static str,
    title: &'static str,
    category: &'static str,
    description: &'static str,
    inputs: Vec<UiBlockInput>,
    outputs: Vec<UiBlockOutput>,
    accepts_flow: bool,
}

#[derive(Clone, Debug, Serialize)]
struct UiBlocksResponse {
    blocks: Vec<UiBlockDefinition>,
}

#[derive(Clone, Debug, Deserialize)]
struct UiBlockInstance {
    block_id: String,
    #[serde(default)]
    values: HashMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
struct UiRenderRequest {
    #[serde(default)]
    blocks: Vec<UiBlockInstance>,
    #[serde(default)]
    nodes: Vec<UiGraphNode>,
    #[serde(default)]
    edges: Vec<UiGraphEdge>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct UiSourceBundle {
    rustscript: String,
    javascript: String,
    lua: String,
    scheme: String,
}

#[derive(Clone, Debug, Serialize)]
struct UiRenderResponse {
    source: UiSourceBundle,
}

#[derive(Clone, Debug, Deserialize)]
struct UiDeployRequest {
    edge_id: String,
    #[serde(default)]
    flavor: Option<String>,
    #[serde(default)]
    blocks: Vec<UiBlockInstance>,
    #[serde(default)]
    nodes: Vec<UiGraphNode>,
    #[serde(default)]
    edges: Vec<UiGraphEdge>,
}

#[derive(Clone, Debug, Serialize)]
struct UiDeployResponse {
    command_id: String,
    pending_commands: usize,
    flavor: String,
    source: UiSourceBundle,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct UiGraphNode {
    id: String,
    block_id: String,
    #[serde(default)]
    values: HashMap<String, String>,
    #[serde(default)]
    position: Option<UiGraphNodePosition>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct UiGraphNodePosition {
    x: f64,
    y: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct UiGraphEdge {
    source: String,
    source_output: String,
    target: String,
    target_input: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredProgramVersion {
    version: u32,
    created_unix_ms: u64,
    flavor: String,
    #[serde(default = "default_true")]
    flow_synced: bool,
    nodes: Vec<UiGraphNode>,
    edges: Vec<UiGraphEdge>,
    source: UiSourceBundle,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredProgram {
    program_id: String,
    name: String,
    created_unix_ms: u64,
    updated_unix_ms: u64,
    versions: Vec<StoredProgramVersion>,
}

pub fn build_controller_app(state: ControllerState) -> Router {
    Router::new()
        .route("/healthz", get(healthz_handler))
        .route("/metrics", get(metrics_handler))
        .route("/ui", get(ui_index_handler))
        .route("/ui/", get(ui_index_handler))
        .route("/ui/{*path}", get(ui_asset_handler))
        .route("/rpc/v1/edge/poll", post(rpc_poll_handler))
        .route("/rpc/v1/edge/result", post(rpc_result_handler))
        .route("/v1/ui/blocks", get(ui_blocks_handler))
        .route("/v1/ui/render", post(ui_render_handler))
        .route("/v1/ui/deploy", post(ui_deploy_handler))
        .route(
            "/v1/programs",
            get(list_programs_handler).post(create_program_handler),
        )
        .route(
            "/v1/programs/{program_id}",
            get(get_program_handler)
                .patch(rename_program_handler)
                .delete(delete_program_handler),
        )
        .route(
            "/v1/programs/{program_id}/versions",
            post(create_program_version_handler),
        )
        .route(
            "/v1/programs/{program_id}/versions/{version}",
            get(get_program_version_handler),
        )
        .route("/v1/edges", get(list_edges_handler))
        .route("/v1/edges/{edge_id}", get(get_edge_handler))
        .route("/v1/edges/{edge_id}/results", get(get_edge_results_handler))
        .route(
            "/v1/edges/{edge_id}/program",
            put(enqueue_program_binary_handler),
        )
        .route(
            "/v1/edges/{edge_id}/commands/apply-program",
            post(enqueue_apply_program_handler),
        )
        .route(
            "/v1/edges/{edge_id}/commands/apply-program-version",
            post(enqueue_apply_program_version_handler),
        )
        .route(
            "/v1/edges/{edge_id}/commands/start-debug",
            post(enqueue_start_debug_handler),
        )
        .route(
            "/v1/edges/{edge_id}/commands/stop-debug",
            post(enqueue_stop_debug_handler),
        )
        .route(
            "/v1/edges/{edge_id}/commands/get-health",
            post(enqueue_get_health_handler),
        )
        .route(
            "/v1/edges/{edge_id}/commands/get-metrics",
            post(enqueue_get_metrics_handler),
        )
        .route(
            "/v1/edges/{edge_id}/commands/get-telemetry",
            post(enqueue_get_telemetry_handler),
        )
        .route(
            "/v1/edges/{edge_id}/commands/ping",
            post(enqueue_ping_handler),
        )
        .route(
            "/v1/debug-sessions",
            get(list_debug_sessions_handler).post(create_debug_session_handler),
        )
        .route(
            "/v1/debug-sessions/stream",
            get(debug_sessions_stream_handler),
        )
        .route(
            "/v1/debug-sessions/{session_id}",
            get(get_debug_session_handler).delete(delete_debug_session_handler),
        )
        .route(
            "/v1/debug-sessions/{session_id}/stop",
            post(stop_debug_session_handler),
        )
        .route(
            "/v1/debug-sessions/{session_id}/command",
            post(run_debug_command_handler),
        )
        .layer(axum::middleware::from_fn(access_log_middleware))
        .with_state(state)
}

fn resolve_edge_debug_source(
    store: &ControllerStore,
    edge_id: &str,
) -> (Option<String>, Option<String>) {
    let Some(edge_record) = store.edges.get(edge_id) else {
        return (None, None);
    };
    let Some(applied) = edge_record.applied_program.as_ref() else {
        return (None, None);
    };
    let Some(program) = store.programs.get(&applied.program_id) else {
        return (None, None);
    };
    let Some(version) = program
        .versions
        .iter()
        .find(|item| item.version == applied.version)
    else {
        return (None, None);
    };
    let flavor = version.flavor.clone();
    let parsed_flavor = parse_ui_flavor(Some(flavor.as_str()))
        .map(|(item, _)| item)
        .unwrap_or(SourceFlavor::RustScript);
    (
        Some(flavor),
        Some(source_for_flavor(&version.source, parsed_flavor)),
    )
}

fn map_summary(edge_id: &str, record: &EdgeRecord) -> EdgeSummary {
    let has_pending_apply = record
        .pending_commands
        .iter()
        .any(|command| matches!(command, ControlPlaneCommand::ApplyProgram { .. }));
    let sync_status = if record.applied_program.is_none() {
        "not_synced"
    } else if has_pending_apply {
        "out_of_sync"
    } else {
        "synced"
    };
    EdgeSummary {
        edge_id: edge_id.to_string(),
        edge_name: if record.edge_name.trim().is_empty() {
            edge_id.to_string()
        } else {
            record.edge_name.clone()
        },
        sync_status: sync_status.to_string(),
        last_seen_unix_ms: record.last_poll_unix_ms,
        pending_commands: record.pending_commands.len(),
        recent_results: record.recent_results.len(),
        applied_program: record.applied_program.clone(),
        last_poll_unix_ms: record.last_poll_unix_ms,
        last_result_unix_ms: record.last_result_unix_ms,
        total_polls: record.total_polls,
        total_results: record.total_results,
        last_telemetry: record.last_telemetry.clone(),
    }
}

fn append_traffic_sample(record: &mut EdgeRecord, sample: EdgeTrafficSample, unix_ms: u64) {
    if let Some(previous) = record.last_traffic_cumulative.as_ref()
        && previous.requests_total == sample.requests_total
        && previous.status_2xx_total == sample.status_2xx_total
        && previous.status_3xx_total == sample.status_3xx_total
        && previous.status_4xx_total == sample.status_4xx_total
        && previous.status_5xx_total == sample.status_5xx_total
    {
        record.last_traffic_cumulative = Some(sample);
        return;
    }

    let previous = record.last_traffic_cumulative.as_ref();
    let point = EdgeTrafficPoint {
        unix_ms,
        requests: previous
            .map(|prev| sample.requests_total.saturating_sub(prev.requests_total))
            .unwrap_or(0),
        status_2xx: previous
            .map(|prev| {
                sample
                    .status_2xx_total
                    .saturating_sub(prev.status_2xx_total)
            })
            .unwrap_or(0),
        status_3xx: previous
            .map(|prev| {
                sample
                    .status_3xx_total
                    .saturating_sub(prev.status_3xx_total)
            })
            .unwrap_or(0),
        status_4xx: previous
            .map(|prev| {
                sample
                    .status_4xx_total
                    .saturating_sub(prev.status_4xx_total)
            })
            .unwrap_or(0),
        status_5xx: previous
            .map(|prev| {
                sample
                    .status_5xx_total
                    .saturating_sub(prev.status_5xx_total)
            })
            .unwrap_or(0),
        latency_p50_ms: sample.latency_p50_ms,
        latency_p90_ms: sample.latency_p90_ms,
        latency_p99_ms: sample.latency_p99_ms,
        upstream_latency_p50_ms: sample.upstream_latency_p50_ms,
        upstream_latency_p90_ms: sample.upstream_latency_p90_ms,
        upstream_latency_p99_ms: sample.upstream_latency_p99_ms,
        edge_latency_p50_ms: sample.edge_latency_p50_ms,
        edge_latency_p90_ms: sample.edge_latency_p90_ms,
        edge_latency_p99_ms: sample.edge_latency_p99_ms,
    };
    record.traffic_points.push_back(point);
    while record.traffic_points.len() > MAX_TRAFFIC_POINTS {
        let _ = record.traffic_points.pop_front();
    }
    record.last_traffic_cumulative = Some(sample);
}

fn map_program_summary(program: &StoredProgram) -> ProgramSummary {
    ProgramSummary {
        program_id: program.program_id.clone(),
        name: program.name.clone(),
        latest_version: program
            .versions
            .last()
            .map(|item| item.version)
            .unwrap_or(0),
        versions: program.versions.len(),
        created_unix_ms: program.created_unix_ms,
        updated_unix_ms: program.updated_unix_ms,
    }
}

fn map_program_detail(program: &StoredProgram) -> ProgramDetailResponse {
    ProgramDetailResponse {
        program_id: program.program_id.clone(),
        name: program.name.clone(),
        latest_version: program
            .versions
            .last()
            .map(|item| item.version)
            .unwrap_or(0),
        created_unix_ms: program.created_unix_ms,
        updated_unix_ms: program.updated_unix_ms,
        versions: program
            .versions
            .iter()
            .map(|item| ProgramVersionSummary {
                version: item.version,
                created_unix_ms: item.created_unix_ms,
                flavor: item.flavor.clone(),
                flow_synced: item.flow_synced,
            })
            .collect(),
    }
}

fn map_program_version_detail(version: &StoredProgramVersion) -> ProgramVersionDetail {
    ProgramVersionDetail {
        version: version.version,
        created_unix_ms: version.created_unix_ms,
        flavor: version.flavor.clone(),
        flow_synced: version.flow_synced,
        nodes: version.nodes.clone(),
        edges: version.edges.clone(),
        source: version.source.clone(),
    }
}

fn command_kind(command: &ControlPlaneCommand) -> &'static str {
    match command {
        ControlPlaneCommand::ApplyProgram { .. } => "apply_program",
        ControlPlaneCommand::StartDebugSession { .. } => "start_debug_session",
        ControlPlaneCommand::DebugCommand { .. } => "debug_command",
        ControlPlaneCommand::StopDebugSession { .. } => "stop_debug_session",
        ControlPlaneCommand::GetHealth { .. } => "get_health",
        ControlPlaneCommand::GetMetrics { .. } => "get_metrics",
        ControlPlaneCommand::GetTelemetry { .. } => "get_telemetry",
        ControlPlaneCommand::Ping { .. } => "ping",
    }
}

fn webui_content_type(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") {
        "text/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".json") {
        "application/json; charset=utf-8"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        "image/jpeg"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".webp") {
        "image/webp"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else if path.ends_with(".woff") {
        "font/woff"
    } else if path.ends_with(".ttf") {
        "font/ttf"
    } else if path.ends_with(".map") {
        "application/json; charset=utf-8"
    } else if path.ends_with(".wasm") {
        "application/wasm"
    } else {
        "application/octet-stream"
    }
}

fn is_octet_stream(value: Option<&axum::http::HeaderValue>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    value
        .split(';')
        .next()
        .map(|item| item.trim().eq_ignore_ascii_case("application/octet-stream"))
        .unwrap_or(false)
}

fn bad_request(message: &str) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse {
            error: message.to_string(),
        }),
    )
}

fn not_found(message: &str) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: message.to_string(),
        }),
    )
}

fn internal_error(message: String) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse { error: message }),
    )
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
