use vm::Vm;

use self::helpers::{
    headers_to_value_map, is_valid_request_path, is_valid_upstream, parse_header,
    parse_header_name, parse_headers_map, query_to_value_map, request_path_with_query,
    serialize_query_pairs,
};
use super::{SharedVmAsyncOps, registry};

mod exchange;
mod helpers;
mod request;
mod response;
mod state;
mod upstream;

pub(crate) use state::HttpOutboundRequestNode;
pub use state::{HttpRequestContext, ProxyVmContext, SharedProxyVmContext};
pub(crate) use state::{
    allocate_outbound_exchange_handle, append_outbound_exchange_body,
    append_outbound_exchange_body_bytes, append_response_output_body_bytes, build_upstream_url,
    consume_request_body_all, default_upstream_exchange_handle,
    ensure_outbound_exchange_response_started, ensure_upstream_response_started,
    is_hop_by_hop_header, outbound_exchange_exists, outbound_exchange_response_available,
    outbound_exchange_response_eof, outbound_exchange_tls_flow,
    read_outbound_exchange_response_all, read_outbound_exchange_response_next_chunk,
    read_outbound_exchange_response_next_line, read_request_body_all, read_request_body_next_chunk,
    read_request_body_next_line, read_upstream_response_all, read_upstream_response_next_chunk,
    read_upstream_response_next_line, request_body_eof, resolve_http_graph_response,
    upstream_response_available, upstream_response_eof,
};

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
