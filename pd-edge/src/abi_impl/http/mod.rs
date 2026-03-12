#![cfg_attr(not(feature = "http"), allow(unused_imports))]

use vm::Vm;

#[cfg(feature = "http")]
use self::helpers::{
    headers_to_value_map, is_valid_request_path, is_valid_upstream, parse_header,
    parse_header_name, parse_headers_map, query_to_value_map, request_path_with_query,
    serialize_query_pairs,
};
use super::SharedVmAsyncOps;
#[cfg(feature = "http")]
use super::registry;

#[cfg(feature = "http")]
mod exchange;
mod helpers;
#[cfg(feature = "http")]
mod request;
#[cfg(feature = "http")]
mod response;
mod state;
#[cfg(feature = "http")]
mod upstream;

#[cfg(feature = "tls")]
pub(crate) use state::attach_outbound_exchange_tls_transport;
#[cfg(feature = "tls")]
pub(crate) use state::upstream_response_available;
#[cfg(feature = "websocket")]
pub(crate) use state::{HttpOutboundRequestNode, build_upstream_url, is_hop_by_hop_header};
pub use state::{HttpRequestContext, ProxyVmContext, SharedProxyVmContext};
#[cfg(any(feature = "http", test))]
pub(crate) use state::{
    allocate_outbound_exchange_handle, ensure_outbound_exchange_response_started,
    ensure_upstream_response_started, read_request_body_all,
};
pub(crate) use state::{
    allocate_tcp_stream_handle, allocate_udp_socket_handle, append_outbound_exchange_body,
    append_outbound_exchange_body_bytes, append_response_output_body_bytes,
    attach_outbound_exchange_tcp_transport, consume_request_body_all,
    default_upstream_exchange_handle, default_upstream_udp_socket_handle, outbound_exchange_exists,
    new_shared_upstream_client_cache, outbound_exchange_response_available,
    outbound_exchange_response_eof, outbound_exchange_tls_flow,
    read_outbound_exchange_response_all, read_outbound_exchange_response_next_chunk,
    read_outbound_exchange_response_next_line, read_request_body_next_chunk,
    read_request_body_next_line, read_upstream_response_all, read_upstream_response_next_chunk,
    read_upstream_response_next_line, request_body_eof, resolve_http_graph_response,
    tcp_stream_exists, udp_socket_exists, upstream_response_eof, SharedUpstreamClientCache,
};
#[cfg(feature = "webrtc")]
pub(crate) use state::{
    allocate_webrtc_connection_handle, default_upstream_webrtc_connection_handle,
    webrtc_connection_exists,
};

#[cfg(feature = "http")]
pub(super) fn register_http_extensions(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    registry::register_host_scope(
        vm,
        &context,
        &async_ops,
        registry::EdgeHostScope::HttpExtension,
    );
}

#[cfg(not(feature = "http"))]
pub(super) fn register_http_extensions(
    _vm: &mut Vm,
    _context: SharedProxyVmContext,
    _async_ops: SharedVmAsyncOps,
) {
}
