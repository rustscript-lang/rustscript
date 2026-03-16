use std::sync::Arc;

use axum::{
    body::Body,
    http::{HeaderMap, Response, StatusCode},
};
use tokio::net::TcpListener;
#[cfg(feature = "tls")]
use tokio_rustls::rustls::ServerConfig;
use tracing::warn;
use uuid::Uuid;

use super::vm_runner::{VmDebugInvocation, VmExecutionError, execute_vm_with_context};
use super::{SharedState, maybe_auto_promote_downstream_listener_goal_into_http_request};
use crate::{
    abi_impl::{
        ProxyVmContext,
        http::{
            DownstreamHttpListenerGoal, InlineDownstreamHttpResponse, resolve_http_graph_response,
        },
        register_http_plane_host_module,
    },
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
            serve_transport_connection(stream, peer_addr, state).await;
        });
    }
}

pub(crate) async fn serve_transport_connection(
    stream: tokio::net::TcpStream,
    peer_addr: std::net::SocketAddr,
    state: SharedState,
) {
    serve_transport_connection_with_listener_goal(
        stream,
        peer_addr,
        state,
        DownstreamHttpListenerGoal::None,
        None,
    )
    .await;
}

pub(crate) async fn serve_transport_connection_with_listener_goal(
    stream: tokio::net::TcpStream,
    peer_addr: std::net::SocketAddr,
    state: SharedState,
    downstream_listener_goal: DownstreamHttpListenerGoal,
    #[cfg(feature = "tls")] downstream_tls_termination: Option<Arc<ServerConfig>>,
    #[cfg(not(feature = "tls"))] _downstream_tls_termination: Option<()>,
) {
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
        request_id: request_id.clone(),
    };

    let vm_context = match ProxyVmContext::from_downstream_tcp_stream_with_services(
        stream,
        request_id,
        state.runtime_services.clone(),
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
    vm_context.set_downstream_listener_goal(downstream_listener_goal);
    #[cfg(feature = "tls")]
    if let Some(downstream_tls_termination) = downstream_tls_termination {
        vm_context.attach_downstream_tls_termination(downstream_tls_termination);
    }
    let vm_context = Arc::new(vm_context);

    let execution = execute_vm_with_context(
        &program,
        vm_context.clone(),
        state.debug_session.clone(),
        debug,
        register_http_plane_host_module,
        state.vm_execution,
    )
    .await;

    match execution {
        Ok(()) => {
            if let Err(err) =
                maybe_auto_promote_downstream_listener_goal_into_http_request(&vm_context).await
            {
                warn!(
                    "{} downstream listener goal http promotion failed: {err}",
                    category_program()
                );
            }
            if let Some(sender) = vm_context.take_inline_downstream_http_response_sender() {
                let resolved = resolve_http_graph_response(&vm_context).await;
                let _ = sender.send(InlineDownstreamHttpResponse {
                    response: resolved.response,
                    post_response_plan: resolved.post_response_plan,
                });
            }
        }
        Err(err) => {
            if let Err(auto_err) =
                maybe_auto_promote_downstream_listener_goal_into_http_request(&vm_context).await
            {
                warn!(
                    "{} downstream listener goal http promotion failed during error handling: {auto_err}",
                    category_program()
                );
            }
            if let Some(sender) = vm_context.take_inline_downstream_http_response_sender() {
                let _ = sender.send(InlineDownstreamHttpResponse {
                    response: text_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal server error",
                    ),
                    post_response_plan: None,
                });
            }
            match err {
                VmExecutionError::HostRegistration(err) | VmExecutionError::Vm(err) => {
                    warn!(
                        "{} downstream transport vm execution failed: {err}",
                        category_program()
                    )
                }
            }
        }
    }
}

fn text_response(status: StatusCode, text: &str) -> Response<Body> {
    let mut response = Response::new(Body::from(text.to_string()));
    *response.status_mut() = status;
    response
}
