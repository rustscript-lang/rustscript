use vm::Vm;

use self::helpers::{
    headers_to_value_map, is_valid_request_path, is_valid_upstream, parse_header,
    parse_header_name, parse_headers_map, query_to_value_map, request_path_with_query,
    serialize_query_pairs,
};
use super::{SharedVmAsyncOps, registry};

mod helpers;
mod request;
mod response;
mod state;
mod upstream;

pub use state::{HttpRequestContext, ProxyVmContext, SharedProxyVmContext};
pub(crate) use state::{
    consume_request_body_all, ensure_upstream_response_started, read_request_body_all,
    read_request_body_next_chunk, read_request_body_next_line, read_upstream_response_all,
    read_upstream_response_next_chunk, read_upstream_response_next_line, request_body_eof,
    resolve_http_graph_response, resolve_outbound_request_body, upstream_response_available,
    upstream_response_eof,
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
