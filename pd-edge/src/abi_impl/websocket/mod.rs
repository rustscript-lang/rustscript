use vm::Vm;

use super::{SharedProxyVmContext, SharedVmAsyncOps, registry};

mod connection;
pub(crate) mod state;

pub(crate) use connection::{
    close_websocket_binary_stream, ensure_outbound_websocket_connection_open,
    read_websocket_binary_bytes, validate_outbound_websocket_binary_connection,
    websocket_connection_mode, write_websocket_binary_bytes,
};
pub(crate) use state::WebSocketConnectionState;

pub(super) fn register_websocket_extensions(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    registry::register_host_scope(vm, &context, &async_ops, registry::EdgeHostScope::WebSocket);
}
