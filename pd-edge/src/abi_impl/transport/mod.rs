#![cfg_attr(not(feature = "tls"), allow(dead_code))]

use axum::http::Version;
use vm::Vm;

use super::{SharedProxyVmContext, SharedVmAsyncOps, registry};

mod state;
mod tcp;
#[cfg(feature = "tls")]
mod tls;
mod udp;

#[cfg(feature = "tls")]
pub(crate) use state::{DownstreamTlsServerStart, SharedServerTlsStreamIo, SharedTlsStreamIo};
pub(crate) use state::TlsSessionCacheKey;
pub(crate) use state::{
    CachedTlsSession, FIRST_DYNAMIC_TCP_STREAM_HANDLE, SharedTcpStreamIo, SharedTlsSessionCache,
    SharedUdpSocketIo, TcpFlowState, TcpSocketPhase, TcpSocketState, TcpStreamRef, TcpTransportDag,
    TlsFlowState, TlsProtocolVersion, TlsSessionRef, TlsTransportDag, UdpSocketState,
    alpn_from_http_version, decode_tcp_stream_handle, decode_tls_session_handle,
    new_shared_tls_session_cache, tls_session_cache_key,
};

pub(super) fn register_transport_extensions(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    registry::register_host_scope(vm, &context, &async_ops, registry::EdgeHostScope::Transport);
}

#[cfg_attr(not(feature = "http"), allow(dead_code))]
pub(crate) fn configure_upstream_transport_for_target(
    context: &super::ProxyVmContext,
    target: &str,
) {
    let mut transport = context.lock_transport();
    transport.tcp_dag.default_upstream.configure();
    transport.tls_dag.default_upstream.observe_target(target);
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn mark_upstream_transport_started(
    context: &super::ProxyVmContext,
    response_version: Version,
) {
    let mut transport = context.lock_transport();
    transport.tcp_dag.default_upstream.mark_connected();
    transport
        .tls_dag
        .default_upstream
        .mark_handshake_complete(alpn_from_http_version(response_version));
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::http::{HeaderMap, Version};

    use super::{configure_upstream_transport_for_target, mark_upstream_transport_started};
    use crate::abi_impl::{ProxyVmContext, RateLimiterStore};

    fn test_context() -> ProxyVmContext {
        ProxyVmContext::from_request_headers(
            HeaderMap::new(),
            Arc::new(Mutex::new(RateLimiterStore::new())),
        )
    }

    #[test]
    fn configuring_https_target_marks_default_upstream_transport_capabilities() {
        let context = test_context();
        configure_upstream_transport_for_target(&context, "https://origin.example.com/api");

        let transport = context.lock_transport();
        assert!(transport.tcp_dag.default_upstream.is_configured());
        assert!(!transport.tcp_dag.default_upstream.is_connected());
        assert!(!transport.tcp_dag.default_upstream.saw_read());
        assert!(!transport.tcp_dag.default_upstream.saw_write());
        assert!(transport.tls_dag.default_upstream.is_present());
        assert!(!transport.tls_dag.default_upstream.handshake_complete());
        assert!(!transport.tls_dag.default_upstream.plaintext_ready());
        assert_eq!(
            transport.tls_dag.default_upstream.peer_name(),
            "origin.example.com"
        );
        assert_eq!(transport.tls_dag.default_upstream.alpn(), "");
    }

    #[test]
    fn starting_upstream_transport_publishes_connection_and_alpn() {
        let context = test_context();
        configure_upstream_transport_for_target(&context, "https://origin.example.com/api");
        mark_upstream_transport_started(&context, Version::HTTP_2);

        let transport = context.lock_transport();
        assert!(transport.tcp_dag.default_upstream.is_configured());
        assert!(transport.tcp_dag.default_upstream.is_connected());
        assert!(transport.tls_dag.default_upstream.is_present());
        assert!(transport.tls_dag.default_upstream.handshake_complete());
        assert!(transport.tls_dag.default_upstream.plaintext_ready());
        assert_eq!(
            transport.tls_dag.default_upstream.peer_name(),
            "origin.example.com"
        );
        assert_eq!(transport.tls_dag.default_upstream.alpn(), "h2");
    }
}
