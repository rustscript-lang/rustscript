use vm::Vm;

#[cfg(feature = "websocket")]
use super::registry;
use super::{SharedProxyVmContext, SharedVmAsyncOps};

#[cfg(feature = "websocket")]
mod connection;
#[cfg(feature = "websocket")]
pub(crate) mod state;
#[cfg(not(feature = "websocket"))]
mod stub;

#[cfg(feature = "websocket")]
pub(crate) use connection::{
    close_websocket_binary_stream, ensure_outbound_websocket_connection_open,
    read_websocket_binary_bytes, validate_outbound_websocket_binary_connection,
    websocket_connection_mode, websocket_negotiated_subprotocol, write_websocket_binary_bytes,
};
#[cfg(feature = "websocket")]
pub(crate) use state::WebSocketConnectionState;
#[cfg(not(feature = "websocket"))]
pub(crate) use stub::{
    WebSocketConnectionState, close_websocket_binary_stream,
    ensure_outbound_websocket_connection_open, read_websocket_binary_bytes,
    validate_outbound_websocket_binary_connection, websocket_connection_mode,
    websocket_negotiated_subprotocol, write_websocket_binary_bytes,
};

#[cfg(feature = "websocket")]
pub(super) fn register_websocket_extensions(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    registry::register_host_scope(vm, &context, &async_ops, registry::EdgeHostScope::WebSocket);
}

#[cfg(not(feature = "websocket"))]
pub(super) fn register_websocket_extensions(
    _vm: &mut Vm,
    _context: SharedProxyVmContext,
    _async_ops: SharedVmAsyncOps,
) {
}
