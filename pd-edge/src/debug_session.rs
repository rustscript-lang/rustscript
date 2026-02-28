use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use axum::http::{HeaderMap, HeaderName, StatusCode};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use vm::{DebugCommandBridge, DebugCommandBridgeError, Debugger, Vm, VmResult, VmStatus};

use crate::{
    control_plane_rpc::{DebugSessionMode, RemoteDebugCommand, RemoteDebugCommandResponse},
    logging::category_debug,
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(8);

pub struct DebugSessionStore {
    session: RwLock<Option<Arc<DebugSession>>>,
}

pub type SharedDebugSession = Arc<DebugSessionStore>;

#[derive(Clone, Debug, Deserialize)]
pub struct StartDebugSessionRequest {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub header_name: Option<String>,
    #[serde(default)]
    pub header_value: Option<String>,
    #[serde(default)]
    pub tcp_addr: Option<String>,
    #[serde(default = "default_stop_on_entry")]
    pub stop_on_entry: bool,
    #[serde(default)]
    pub mode: DebugSessionMode,
    #[serde(default)]
    pub request_path: Option<String>,
    #[serde(default = "default_record_count")]
    pub record_count: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DebugSessionStatus {
    pub active: bool,
    pub attached: bool,
    pub current_line: Option<u32>,
    #[serde(default)]
    pub request_id: Option<String>,
    pub header_name: Option<String>,
    pub header_value: Option<String>,
    pub tcp_addr: Option<String>,
    pub stop_on_entry: Option<bool>,
    pub mode: Option<DebugSessionMode>,
    pub request_path: Option<String>,
    pub target_recordings: Option<u32>,
    pub captured_recordings: Option<u32>,
    pub completed: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct DebugRecordingArtifact {
    pub session_id: String,
    pub recording_id: String,
    pub request_id: Option<String>,
    pub request_path: Option<String>,
    pub recording_base64: String,
    pub frame_count: u32,
    pub terminal_status: Option<String>,
    pub sequence: u32,
    pub completed: bool,
}

#[derive(Debug)]
pub enum DebugSessionError {
    AlreadyActive,
    InvalidHeaderName,
    EmptyHeaderValue,
    InvalidTcpAddress(String),
    InvalidRequestPath,
    InvalidRecordCount,
    NotActive,
    NotAttached,
    RemoteCommandsUnavailable,
    CommandTimeout,
    BridgeClosed,
    InvalidCommand(String),
}

impl DebugSessionError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            DebugSessionError::AlreadyActive => StatusCode::CONFLICT,
            DebugSessionError::InvalidHeaderName
            | DebugSessionError::EmptyHeaderValue
            | DebugSessionError::InvalidTcpAddress(_)
            | DebugSessionError::InvalidRequestPath
            | DebugSessionError::InvalidRecordCount
            | DebugSessionError::CommandTimeout
            | DebugSessionError::InvalidCommand(_)
            | DebugSessionError::BridgeClosed => StatusCode::BAD_REQUEST,
            DebugSessionError::NotActive
            | DebugSessionError::NotAttached
            | DebugSessionError::RemoteCommandsUnavailable => StatusCode::CONFLICT,
        }
    }
}

impl std::fmt::Display for DebugSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DebugSessionError::AlreadyActive => write!(f, "debug session already active"),
            DebugSessionError::InvalidHeaderName => write!(f, "invalid debug header name"),
            DebugSessionError::EmptyHeaderValue => write!(f, "debug header value cannot be empty"),
            DebugSessionError::InvalidTcpAddress(message) => write!(f, "{message}"),
            DebugSessionError::InvalidRequestPath => {
                write!(f, "recording mode requires a non-empty request_path")
            }
            DebugSessionError::InvalidRecordCount => {
                write!(f, "recording mode requires record_count >= 1")
            }
            DebugSessionError::NotActive => write!(f, "debug session is not active"),
            DebugSessionError::NotAttached => {
                write!(f, "debugger is not attached to a matching request yet")
            }
            DebugSessionError::RemoteCommandsUnavailable => {
                write!(
                    f,
                    "remote debug commands are unavailable for tcp debugger sessions"
                )
            }
            DebugSessionError::CommandTimeout => {
                write!(f, "timed out waiting for debugger command response")
            }
            DebugSessionError::BridgeClosed => write!(f, "debugger bridge closed"),
            DebugSessionError::InvalidCommand(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for DebugSessionError {}

pub fn new_debug_session_store() -> SharedDebugSession {
    Arc::new(DebugSessionStore {
        session: RwLock::new(None),
    })
}

pub fn start_debug_session(
    store: &SharedDebugSession,
    request: StartDebugSessionRequest,
) -> Result<DebugSessionStatus, DebugSessionError> {
    let header_name = request
        .header_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|name| {
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                warn!(
                    "{} rejected start request: invalid header name",
                    category_debug()
                );
                DebugSessionError::InvalidHeaderName
            })
        })
        .transpose()?;
    let header_value = request
        .header_value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    let mut guard = store.session.write().expect("debug session lock poisoned");
    if guard.is_some() {
        warn!(
            "{} start requested while session already active",
            category_debug()
        );
        return Err(DebugSessionError::AlreadyActive);
    }

    let request_path = request
        .request_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let session_mode = match request.mode {
        DebugSessionMode::Interactive => {
            let header_value = header_value.ok_or_else(|| {
                warn!(
                    "{} rejected interactive debug start: empty header value",
                    category_debug()
                );
                DebugSessionError::EmptyHeaderValue
            })?;
            let header_name = header_name.clone().ok_or_else(|| {
                warn!(
                    "{} rejected interactive debug start: missing header name",
                    category_debug()
                );
                DebugSessionError::InvalidHeaderName
            })?;
            let (mut debugger, transport) = if let Some(addr) = request.tcp_addr.as_deref() {
                match Debugger::with_tcp(addr) {
                    Ok(debugger) => (
                        debugger,
                        InteractiveTransport::Tcp {
                            addr: addr.to_string(),
                        },
                    ),
                    Err(err) => {
                        return Err(DebugSessionError::InvalidTcpAddress(format!(
                            "failed to bind tcp debugger on {addr}: {err}"
                        )));
                    }
                }
            } else {
                let bridge = DebugCommandBridge::new();
                (
                    Debugger::with_command_bridge(bridge.clone()),
                    InteractiveTransport::Remote { bridge },
                )
            };
            if request.stop_on_entry {
                debugger.stop_on_entry();
            }
            (
                Some(header_name),
                Some(header_value),
                DebugSessionState::Interactive {
                    debugger: Box::new(Mutex::new(debugger)),
                    transport,
                },
            )
        }
        DebugSessionMode::Recording => {
            if request.record_count == 0 {
                return Err(DebugSessionError::InvalidRecordCount);
            }
            if request_path.is_none() {
                return Err(DebugSessionError::InvalidRequestPath);
            }
            (
                header_name,
                header_value,
                DebugSessionState::Recording {
                    runtime: Mutex::new(RecordingRuntime {
                        target_count: request.record_count,
                        captured_count: 0,
                        next_sequence: 1,
                        completed: false,
                        outbox: VecDeque::new(),
                    }),
                },
            )
        }
    };

    let session = Arc::new(DebugSession {
        session_id: request.session_id,
        header_name: session_mode.0,
        header_value: session_mode.1,
        stop_on_entry: request.stop_on_entry,
        mode: request.mode,
        request_path,
        request_id: RwLock::new(None),
        state: session_mode.2,
    });
    let status = DebugSessionStatus::from_session(&session);
    *guard = Some(session);
    info!(
        "{} started session header={} value={} stop_on_entry={}",
        category_debug(),
        status.header_name.as_deref().unwrap_or(""),
        status.header_value.as_deref().unwrap_or(""),
        status.stop_on_entry.unwrap_or(false)
    );
    Ok(status)
}

pub fn stop_debug_session(store: &SharedDebugSession) -> bool {
    let mut guard = store.session.write().expect("debug session lock poisoned");
    let stopped = guard.take();
    if let Some(session) = stopped {
        session.close();
        info!("{} session stopped", category_debug());
        true
    } else {
        info!("{} stop requested with no active session", category_debug());
        false
    }
}

pub fn run_debug_command(
    store: &SharedDebugSession,
    command: RemoteDebugCommand,
) -> Result<RemoteDebugCommandResponse, DebugSessionError> {
    let session = {
        let guard = store.session.read().expect("debug session lock poisoned");
        guard.clone().ok_or(DebugSessionError::NotActive)?
    };
    let (command_text, resume_mode) = debug_command_text(&command)?;
    let bridge = session
        .bridge()
        .ok_or(DebugSessionError::RemoteCommandsUnavailable)?;
    let response = bridge
        .execute(command_text.clone(), COMMAND_TIMEOUT)
        .map_err(map_bridge_error)?;
    if resume_mode {
        return Ok(RemoteDebugCommandResponse {
            output: format!("sent '{command_text}'"),
            current_line: None,
            attached: false,
        });
    }
    Ok(RemoteDebugCommandResponse {
        output: response.output,
        current_line: response.current_line,
        attached: response.attached,
    })
}

pub fn debug_session_status(store: &SharedDebugSession) -> DebugSessionStatus {
    let guard = store.session.read().expect("debug session lock poisoned");
    if let Some(session) = guard.as_ref() {
        DebugSessionStatus::from_session(session)
    } else {
        DebugSessionStatus::inactive()
    }
}

pub fn run_vm_with_optional_debugger(
    store: &SharedDebugSession,
    request_headers: &HeaderMap,
    request_path: &str,
    request_id: &str,
    vm: &mut Vm,
) -> VmResult<VmStatus> {
    let session = {
        let guard = store.session.read().expect("debug session lock poisoned");
        guard.clone()
    };

    if let Some(session) = session
        && request_matches_session(request_headers, request_path, &session)
    {
        match &session.state {
            DebugSessionState::Interactive { debugger, .. } => {
                session.set_request_id(Some(request_id.to_string()));
                info!(
                    "{} request matched interactive debug session header={}",
                    category_debug(),
                    session
                        .header_name
                        .as_ref()
                        .map(|value| value.as_str())
                        .unwrap_or("<none>")
                );
                let mut debugger = debugger.lock().expect("debugger lock poisoned");
                let result = vm.run_with_debugger(&mut debugger);
                let detached = debugger.take_detach_event();
                drop(debugger);

                if detached {
                    stop_debug_session_if_match(store, &session);
                }
                return result;
            }
            DebugSessionState::Recording { runtime } => {
                session.set_request_id(Some(request_id.to_string()));
                {
                    let guard = runtime.lock().expect("recording runtime lock poisoned");
                    if guard.completed {
                        return vm.run();
                    }
                }

                info!(
                    "{} request matched recording debug session path={}",
                    category_debug(),
                    request_path
                );
                let mut debugger = Debugger::with_recording(vm.program().clone());
                let result = vm.run_with_debugger(&mut debugger);

                let recording = debugger.take_recording();
                if let Some(recording) = recording {
                    let mut runtime = runtime.lock().expect("recording runtime lock poisoned");
                    runtime.captured_count = runtime.captured_count.saturating_add(1);
                    let sequence = runtime.next_sequence;
                    runtime.next_sequence = runtime.next_sequence.saturating_add(1);
                    if runtime.captured_count >= runtime.target_count {
                        runtime.completed = true;
                    }

                    let frame_count = u32::try_from(recording.frames.len()).unwrap_or(u32::MAX);
                    let terminal_status = recording.terminal_status.map(|status| match status {
                        VmStatus::Halted => "halted".to_string(),
                        VmStatus::Yielded => "yielded".to_string(),
                    });
                    match recording.encode() {
                        Ok(bytes) => {
                            let completed = runtime.completed;
                            runtime.outbox.push_back(DebugRecordingArtifact {
                                session_id: session.session_id.clone(),
                                recording_id: format!("{}-{}", session.session_id, sequence),
                                request_id: Some(request_id.to_string()),
                                request_path: session.request_path.clone(),
                                recording_base64: STANDARD.encode(bytes),
                                frame_count,
                                terminal_status,
                                sequence,
                                completed,
                            });
                        }
                        Err(err) => {
                            warn!(
                                "{} failed to encode vm recording for session={} sequence={} err={err}",
                                category_debug(),
                                session.session_id,
                                sequence
                            );
                        }
                    }
                }
                return result;
            }
        }
    }

    vm.run()
}

pub fn drain_recording_artifacts(store: &SharedDebugSession) -> Vec<DebugRecordingArtifact> {
    let session = {
        let guard = store.session.read().expect("debug session lock poisoned");
        guard.clone()
    };
    let Some(session) = session else {
        return Vec::new();
    };
    let DebugSessionState::Recording { runtime } = &session.state else {
        return Vec::new();
    };
    let mut runtime = runtime.lock().expect("recording runtime lock poisoned");
    let mut artifacts = Vec::with_capacity(runtime.outbox.len());
    while let Some(item) = runtime.outbox.pop_front() {
        artifacts.push(item);
    }
    artifacts
}

fn map_bridge_error(error: DebugCommandBridgeError) -> DebugSessionError {
    match error {
        DebugCommandBridgeError::NotAttached => DebugSessionError::NotAttached,
        DebugCommandBridgeError::Timeout => DebugSessionError::CommandTimeout,
        DebugCommandBridgeError::Closed => DebugSessionError::BridgeClosed,
    }
}

fn stop_debug_session_if_match(store: &SharedDebugSession, active: &Arc<DebugSession>) {
    let mut guard = store.session.write().expect("debug session lock poisoned");
    if let Some(current) = guard.as_ref()
        && Arc::ptr_eq(current, active)
    {
        current.close();
        *guard = None;
        info!(
            "{} session removed automatically after debugger detached",
            category_debug()
        );
    }
}

fn request_matches_session(
    request_headers: &HeaderMap,
    request_path: &str,
    session: &DebugSession,
) -> bool {
    let header_match = match (&session.header_name, &session.header_value) {
        (Some(name), Some(expected)) => request_headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(|value| value == expected)
            .unwrap_or(false),
        _ => true,
    };
    if !header_match {
        return false;
    }
    match session.request_path.as_deref() {
        Some(path) => path == request_path,
        None => true,
    }
}

fn default_stop_on_entry() -> bool {
    true
}

fn default_record_count() -> u32 {
    1
}

struct DebugSession {
    session_id: String,
    header_name: Option<HeaderName>,
    header_value: Option<String>,
    stop_on_entry: bool,
    mode: DebugSessionMode,
    request_path: Option<String>,
    request_id: RwLock<Option<String>>,
    state: DebugSessionState,
}

enum InteractiveTransport {
    Remote { bridge: DebugCommandBridge },
    Tcp { addr: String },
}

enum DebugSessionState {
    Interactive {
        debugger: Box<Mutex<Debugger>>,
        transport: InteractiveTransport,
    },
    Recording {
        runtime: Mutex<RecordingRuntime>,
    },
}

struct RecordingRuntime {
    target_count: u32,
    captured_count: u32,
    next_sequence: u32,
    completed: bool,
    outbox: VecDeque<DebugRecordingArtifact>,
}

fn debug_command_text(command: &RemoteDebugCommand) -> Result<(String, bool), DebugSessionError> {
    match command {
        RemoteDebugCommand::Where => Ok(("where".to_string(), false)),
        RemoteDebugCommand::Step => Ok(("step".to_string(), true)),
        RemoteDebugCommand::Next => Ok(("next".to_string(), true)),
        RemoteDebugCommand::Continue => Ok(("continue".to_string(), true)),
        RemoteDebugCommand::Out => Ok(("out".to_string(), true)),
        RemoteDebugCommand::BreakLine { line } => Ok((format!("break line {line}"), false)),
        RemoteDebugCommand::ClearLine { line } => Ok((format!("clear line {line}"), false)),
        RemoteDebugCommand::PrintVar { name } => {
            if name.trim().is_empty() {
                return Err(DebugSessionError::InvalidCommand(
                    "variable name cannot be empty".to_string(),
                ));
            }
            Ok((format!("print {}", name.trim()), false))
        }
        RemoteDebugCommand::Locals => Ok(("locals".to_string(), false)),
        RemoteDebugCommand::Stack => Ok(("stack".to_string(), false)),
    }
}

impl DebugSessionStatus {
    fn inactive() -> Self {
        Self {
            active: false,
            attached: false,
            current_line: None,
            request_id: None,
            header_name: None,
            header_value: None,
            tcp_addr: None,
            stop_on_entry: None,
            mode: None,
            request_path: None,
            target_recordings: None,
            captured_recordings: None,
            completed: None,
        }
    }

    fn from_session(session: &DebugSession) -> Self {
        match &session.state {
            DebugSessionState::Interactive { transport, .. } => {
                let (attached, current_line, tcp_addr) = match transport {
                    InteractiveTransport::Remote { bridge } => {
                        let bridge_status = bridge.status();
                        (bridge_status.attached, bridge_status.current_line, None)
                    }
                    InteractiveTransport::Tcp { addr } => (false, None, Some(addr.clone())),
                };
                Self {
                    active: true,
                    attached,
                    current_line,
                    request_id: session.request_id(),
                    header_name: session
                        .header_name
                        .as_ref()
                        .map(|value| value.as_str().to_string()),
                    header_value: session.header_value.clone(),
                    tcp_addr,
                    stop_on_entry: Some(session.stop_on_entry),
                    mode: Some(session.mode.clone()),
                    request_path: session.request_path.clone(),
                    target_recordings: None,
                    captured_recordings: None,
                    completed: None,
                }
            }
            DebugSessionState::Recording { runtime } => {
                let runtime = runtime.lock().expect("recording runtime lock poisoned");
                Self {
                    active: true,
                    attached: false,
                    current_line: None,
                    request_id: session.request_id(),
                    header_name: session
                        .header_name
                        .as_ref()
                        .map(|value| value.as_str().to_string()),
                    header_value: session.header_value.clone(),
                    tcp_addr: None,
                    stop_on_entry: Some(session.stop_on_entry),
                    mode: Some(session.mode.clone()),
                    request_path: session.request_path.clone(),
                    target_recordings: Some(runtime.target_count),
                    captured_recordings: Some(runtime.captured_count),
                    completed: Some(runtime.completed),
                }
            }
        }
    }
}

impl DebugSession {
    fn request_id(&self) -> Option<String> {
        self.request_id
            .read()
            .expect("debug request id lock poisoned")
            .clone()
    }

    fn set_request_id(&self, request_id: Option<String>) {
        let mut guard = self
            .request_id
            .write()
            .expect("debug request id lock poisoned");
        *guard = request_id;
    }

    fn bridge(&self) -> Option<&DebugCommandBridge> {
        match &self.state {
            DebugSessionState::Interactive { transport, .. } => match transport {
                InteractiveTransport::Remote { bridge } => Some(bridge),
                InteractiveTransport::Tcp { .. } => None,
            },
            DebugSessionState::Recording { .. } => None,
        }
    }

    fn close(&self) {
        if let DebugSessionState::Interactive { transport, .. } = &self.state
            && let InteractiveTransport::Remote { bridge } = transport
        {
            bridge.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_inactive_by_default() {
        let store = new_debug_session_store();
        let status = debug_session_status(&store);
        assert!(!status.active);
        assert!(!status.attached);
    }

    #[test]
    fn stop_noop_returns_false_when_not_active() {
        let store = new_debug_session_store();
        assert!(!stop_debug_session(&store));
    }

    #[test]
    fn invalid_session_request_is_rejected() {
        let store = new_debug_session_store();
        let request = StartDebugSessionRequest {
            session_id: "test".to_string(),
            header_name: Some("bad header".to_string()),
            header_value: Some("x".to_string()),
            tcp_addr: None,
            stop_on_entry: true,
            mode: DebugSessionMode::Interactive,
            request_path: None,
            record_count: 1,
        };
        let err = start_debug_session(&store, request).expect_err("request should be invalid");
        assert!(matches!(err, DebugSessionError::InvalidHeaderName));
    }

    #[test]
    fn empty_header_value_is_rejected() {
        let store = new_debug_session_store();
        let request = StartDebugSessionRequest {
            session_id: "test".to_string(),
            header_name: Some("x-debug".to_string()),
            header_value: Some("".to_string()),
            tcp_addr: None,
            stop_on_entry: true,
            mode: DebugSessionMode::Interactive,
            request_path: None,
            record_count: 1,
        };
        let err = start_debug_session(&store, request).expect_err("request should be invalid");
        assert!(matches!(err, DebugSessionError::EmptyHeaderValue));
    }
}
