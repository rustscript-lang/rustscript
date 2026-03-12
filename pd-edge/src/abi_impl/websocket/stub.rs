#![cfg_attr(not(feature = "websocket"), allow(dead_code))]

use vm::VmError;

use super::super::SharedProxyVmContext;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum WebSocketPhase {
    #[default]
    Inactive,
}

impl WebSocketPhase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Inactive => "inactive",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct WebSocketConnectionState;

impl WebSocketConnectionState {
    pub(crate) fn for_http_request(_headers: &axum::http::HeaderMap) -> Self {
        Self
    }

    pub(crate) fn phase(&self) -> WebSocketPhase {
        WebSocketPhase::Inactive
    }

    pub(crate) fn is_present(&self) -> bool {
        false
    }

    pub(crate) fn is_websocket_mode(&self) -> bool {
        false
    }

    pub(crate) fn is_open(&self) -> bool {
        false
    }

    pub(crate) fn prepare_outbound(&mut self) {}

    pub(crate) fn set_requested_subprotocols(&mut self, _requested_subprotocols: Vec<String>) {}

    pub(crate) fn requested_subprotocols(&self) -> &[String] {
        &[]
    }

    pub(crate) fn negotiated_subprotocol(&self) -> &str {
        ""
    }

    pub(crate) fn note_handshake_started(&mut self) {}

    pub(crate) fn mark_open(&mut self, _io: (), _negotiated_subprotocol: Option<String>) {}

    pub(crate) fn note_closing(&mut self) {}

    pub(crate) fn refresh_close_state(&mut self) {}

    pub(crate) fn mark_closed(&mut self, _code: Option<u16>, _reason: Option<String>) {}

    pub(crate) fn mark_failed(&mut self, _message: impl Into<String>) {}

    pub(crate) fn eof(&mut self) -> bool {
        false
    }
}

fn websocket_disabled() -> VmError {
    VmError::HostError("websocket feature is disabled in this build".to_string())
}

pub(crate) fn websocket_connection_mode(_context: &SharedProxyVmContext, _connection: i64) -> bool {
    false
}

pub(crate) fn validate_outbound_websocket_binary_connection(
    _context: &SharedProxyVmContext,
    _connection: i64,
) -> Result<(), VmError> {
    Err(websocket_disabled())
}

pub(crate) async fn ensure_outbound_websocket_connection_open(
    _context: &SharedProxyVmContext,
    _connection: i64,
) -> Result<(), VmError> {
    Err(websocket_disabled())
}

pub(crate) async fn write_websocket_binary_bytes(
    _context: &SharedProxyVmContext,
    _connection: i64,
    _payload: &[u8],
) -> Result<usize, VmError> {
    Err(websocket_disabled())
}

pub(crate) async fn read_websocket_binary_bytes(
    _context: &SharedProxyVmContext,
    _connection: i64,
) -> Result<Option<Vec<u8>>, VmError> {
    Err(websocket_disabled())
}

pub(crate) async fn close_websocket_binary_stream(
    _context: &SharedProxyVmContext,
    _connection: i64,
) -> Result<(), VmError> {
    Err(websocket_disabled())
}
