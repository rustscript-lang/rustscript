use std::sync::Arc;

use axum::http::HeaderMap;
use tokio::net::TcpListener;
use tracing::warn;
use uuid::Uuid;

use super::SharedState;
use super::vm_runner::{VmDebugInvocation, VmExecutionError, execute_vm_with_context};
use crate::{
    abi_impl::{ProxyVmContext, register_http_plane_host_module},
    logging::category_program,
};

pub async fn serve_transport_proxy(
    listener: TcpListener,
    state: SharedState,
) -> std::io::Result<()> {
    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            let snapshot = {
                let guard = state.active_program.read().await;
                guard.clone()
            };
            let Some(program) = snapshot else {
                return;
            };

            let request_id = Uuid::new_v4().to_string();
            let request_path = format!("tcp://{peer_addr}");
            let debug = VmDebugInvocation {
                attach_debugger: false,
                force_threading: false,
                request_headers: HeaderMap::new(),
                request_path,
                request_id,
            };

            let vm_context = match ProxyVmContext::from_downstream_tcp_stream(
                stream,
                state.rate_limiter.clone(),
            ) {
                Ok(vm_context) => vm_context,
                Err(err) => {
                    warn!(
                        "{} failed to attach downstream transport context: {err}",
                        category_program()
                    );
                    return;
                }
            };
            let mut vm_context = vm_context;
            vm_context.attach_upstream_client(state.client.clone());
            vm_context.attach_upstream_client_cache(state.upstream_client_cache.clone());
            vm_context.attach_tls_session_cache(state.tls_session_cache.clone());
            vm_context.attach_upstream_http_sessions(state.upstream_http_sessions.clone());
            let vm_context = Arc::new(vm_context);

            if let Err(err) = execute_vm_with_context(
                &program,
                vm_context,
                state.debug_session.clone(),
                debug,
                register_http_plane_host_module,
                state.vm_execution,
            )
            .await
            {
                match err {
                    VmExecutionError::HostRegistration(err) | VmExecutionError::Vm(err) => warn!(
                        "{} downstream transport vm execution failed: {err}",
                        category_program()
                    ),
                }
            }
        });
    }
}
