mod abi_impl;
mod active_control_plane;
mod compile;
mod control_plane_rpc;
mod debug_session;
mod logging;
mod runtime;

pub use edge_abi::*;

pub use abi_impl::{
    EdgeProtocolHostModule, HttpProtocolHostModule, ProxyVmContext, RateLimiterStore,
    RuntimeProtocolHostModule, SharedProxyVmContext, SharedRateLimiter, SharedVmAsyncOps,
    VmAsyncOpBridge, VmAsyncOps, VmExecutionOutcome, new_shared_vm_async_ops, register_host_module,
    register_http_host_module, register_http_plane_host_module, register_protocol_modules,
    register_runtime_host_module, snapshot_execution_outcome,
};
pub use active_control_plane::{
    ActiveControlPlaneConfig, run_active_control_plane_client, spawn_active_control_plane_client,
};
pub use compile::{compile_edge_source_file, compile_edge_source_file_with_options};
pub use control_plane_rpc::{
    CommandResultPayload, ControlPlaneCommand, DebugSessionMode, EdgeCommandResult,
    EdgePollRequest, EdgePollResponse, EdgeTrafficSample, RemoteDebugCommand,
    RemoteDebugCommandResponse,
};
pub use debug_session::{
    DebugRecordingArtifact, DebugSessionError, DebugSessionStatus, SharedDebugSession,
    StartDebugSessionRequest, debug_session_status, drain_recording_artifacts,
    new_debug_session_store, request_will_attach_debugger, run_debug_command,
    run_vm_with_optional_debugger, start_debug_session, stop_debug_session,
};
pub use logging::init as init_logging;
pub use runtime::{
    HealthStatus, ProgramApplyReport, SharedState, TelemetrySnapshot, VmExecutionConfig,
    VmExecutionMode, apply_program_from_bytes, build_admin_app, build_http_proxy_app,
};
