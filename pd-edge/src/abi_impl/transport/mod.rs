#![cfg_attr(not(feature = "tls"), allow(dead_code))]

use axum::http::Version;
use vm::Vm;

use super::{SharedProxyVmContext, SharedVmAsyncOps, registry};

mod state;
mod tcp;
#[cfg(feature = "tls")]
mod tls;
mod udp;

#[cfg(feature = "http2")]
pub(crate) use state::TlsSessionCacheKey;
pub(crate) use state::{
    CachedTlsSession, SharedTlsSessionCache, SharedUdpSocketIo, TcpFlowState, TcpStreamRef,
    TcpTransportDag, TlsFlowState, TlsProtocolVersion, TlsSessionRef, TlsTransportDag,
    UdpSocketState, alpn_from_http_version, decode_tcp_stream_handle, decode_tls_session_handle,
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
    context: &mut super::ProxyVmContext,
    target: &str,
) {
    context.tcp_dag.default_upstream.configure();
    context.tls_dag.default_upstream.observe_target(target);
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn mark_upstream_transport_started(
    context: &mut super::ProxyVmContext,
    response_version: Version,
) {
    context.tcp_dag.default_upstream.mark_connected();
    context
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
        let mut context = test_context();
        configure_upstream_transport_for_target(&mut context, "https://origin.example.com/api");

        assert!(context.tcp_dag.default_upstream.is_configured());
        assert!(!context.tcp_dag.default_upstream.is_connected());
        assert!(!context.tcp_dag.default_upstream.saw_read());
        assert!(!context.tcp_dag.default_upstream.saw_write());
        assert!(context.tls_dag.default_upstream.is_present());
        assert!(!context.tls_dag.default_upstream.handshake_complete());
        assert!(!context.tls_dag.default_upstream.plaintext_ready());
        assert_eq!(
            context.tls_dag.default_upstream.peer_name(),
            "origin.example.com"
        );
        assert_eq!(context.tls_dag.default_upstream.alpn(), "");
    }

    #[test]
    fn starting_upstream_transport_publishes_connection_and_alpn() {
        let mut context = test_context();
        configure_upstream_transport_for_target(&mut context, "https://origin.example.com/api");
        mark_upstream_transport_started(&mut context, Version::HTTP_2);

        assert!(context.tcp_dag.default_upstream.is_configured());
        assert!(context.tcp_dag.default_upstream.is_connected());
        assert!(context.tls_dag.default_upstream.is_present());
        assert!(context.tls_dag.default_upstream.handshake_complete());
        assert!(context.tls_dag.default_upstream.plaintext_ready());
        assert_eq!(
            context.tls_dag.default_upstream.peer_name(),
            "origin.example.com"
        );
        assert_eq!(context.tls_dag.default_upstream.alpn(), "h2");
    }
}
