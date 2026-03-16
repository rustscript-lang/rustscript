mod abi_impl;
mod active_control_plane;
mod build_info;
mod cache;
mod compile;
mod control_plane_rpc;
mod debug_session;
mod lock_metrics;
mod logging;
mod runtime;
pub mod sample_echo;

pub use edge_abi::*;

pub use abi_impl::{
    EdgeProtocolHostModule, HttpProtocolHostModule, ProxyVmContext, RateLimiterStore,
    RuntimeProtocolHostModule, SharedProxyVmContext, SharedRateLimiter, SharedVmAsyncOps,
    VmAsyncOpBridge, VmAsyncOps, enter_edge_host_context, new_shared_vm_async_ops,
    register_host_module, register_http_host_module, register_http_plane_host_module,
    register_protocol_modules, register_runtime_host_module,
};
pub use active_control_plane::{
    ActiveControlPlaneConfig, run_active_control_plane_client, spawn_active_control_plane_client,
};
pub use build_info::{
    binary_version_report, binary_version_text, enabled_feature_line, enabled_feature_list,
    enabled_feature_names,
};
pub use compile::{
    compile_edge_source_file, compile_edge_source_file_with_options,
    compile_edge_source_with_flavor,
};
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
#[cfg(feature = "http3")]
pub use runtime::serve_http3_proxy;
pub use runtime::{
    HealthStatus, ProgramApplyReport, RuntimeStoreLimits, SharedState, TelemetrySnapshot,
    VM_EPOCH_TICK_INTERVAL_MS, VmExecutionConfig, VmExecutionMode, VmInterruptConfig,
    apply_program_from_bytes, attach_http_plane_runtime_services, build_admin_app,
    build_http_proxy_app, serve_http_proxy, serve_https_proxy, serve_transport_proxy,
};
