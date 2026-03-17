#[cfg(feature = "http")]
use self::helpers::{
    headers_to_value_map, is_valid_request_path, lookup_cached_header_batch, parse_header,
    parse_header_name, query_to_value_map, request_path_with_query, serialize_query_pairs,
    store_cached_header_batch,
};

#[cfg(feature = "http")]
mod exchange;
mod fast_path;
mod helpers;
mod outbound_http1;
#[cfg(feature = "http")]
mod request;
#[cfg(feature = "http")]
mod response;
mod state;
mod version;

#[cfg(feature = "http")]
pub(crate) use exchange::prepare_default_upstream_request;
pub(crate) use fast_path::{
    DownstreamHttp1FastBodyKind, MAX_DOWNSTREAM_HTTP1_FAST_BODY_BYTES,
    classify_downstream_http1_fast_body, downstream_http1_fast_path_eligible,
    downstream_http1_fast_path_expects_continue, outbound_http1_fast_path_eligible,
};
pub(crate) use outbound_http1::new_shared_plain_http1_sender_pool;
pub(crate) use outbound_http1::{OutboundHttp1ForwardBody, OutboundHttp1ForwardResponse};
#[cfg(feature = "http")]
pub(crate) use response::parse_response_header_batch;
#[cfg(feature = "websocket")]
pub(crate) use state::DownstreamWebSocketTunnelPlan;
#[cfg(feature = "websocket")]
pub(crate) use state::HttpOutboundRequestNode;
#[cfg(feature = "tls")]
pub(crate) use state::attach_outbound_exchange_tls_transport;
pub(crate) use state::is_hop_by_hop_header;
#[cfg(feature = "tls")]
pub(crate) use state::upstream_response_available;
pub(crate) use state::{
    AttachedHttpTransport, DownstreamConnectTunnelPlan, DownstreamConnectTunnelTarget,
    DownstreamConnectionMetadata, DownstreamHttpListenerGoal, DownstreamPostResponsePlan,
    HttpPlaneRuntimeServicesConfig, HttpUpstreamScheme, InlineDownstreamHttpResponse,
    PromotedDownstreamTransport, ProxyStreamRegistry, ResolvedHttpGraphResponse,
    ResolvedNativeHttp1DownstreamResponse, ResolvedNativeLocalHttp1DownstreamResponse,
    SharedRuntimeServices, allocate_tcp_stream_handle, allocate_udp_socket_handle,
    append_outbound_exchange_body, append_outbound_exchange_body_bytes,
    append_response_output_body_bytes, attach_outbound_exchange_tcp_transport,
    build_downstream_http_request_context, consume_request_body_all,
    default_upstream_exchange_handle, default_upstream_udp_socket_handle,
    new_shared_http_plane_runtime_services, new_shared_upstream_client_cache,
    outbound_exchange_exists, outbound_exchange_response_available, outbound_exchange_response_eof,
    outbound_exchange_tls_flow, read_outbound_exchange_response_all,
    read_outbound_exchange_response_next_chunk, read_outbound_exchange_response_next_line,
    read_request_body_next_chunk, read_request_body_next_line, read_upstream_response_all,
    read_upstream_response_next_chunk, read_upstream_response_next_line, request_body_eof,
    resolve_http_graph_response, schedule_downstream_http_handoff,
    start_native_default_upstream_http_forward_response, take_promoted_downstream_transport,
    tcp_stream_exists, try_resolve_native_http1_downstream_response,
    try_take_native_local_http1_downstream_response, udp_socket_exists,
    upstream_reqwest_client_builder, upstream_response_eof,
};
pub use state::{HttpRequestContext, ProxyVmContext, SharedProxyVmContext};
#[cfg(any(feature = "http", test))]
pub(crate) use state::{
    allocate_outbound_exchange_handle, ensure_outbound_exchange_response_started,
    ensure_upstream_response_started, read_request_body_all,
};
#[cfg(feature = "webrtc")]
pub(crate) use state::{
    allocate_webrtc_connection_handle, default_upstream_webrtc_connection_handle,
    webrtc_connection_exists,
};
pub(crate) use version::HttpVersionPreference;
