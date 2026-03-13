use std::sync::Arc;

use axum::http::HeaderMap;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::protocol::{CloseFrame, Message, frame::coding::CloseCode},
};
use vm::VmError;

pub(crate) type SharedWebSocketIo = Arc<tokio::sync::Mutex<OutboundWebSocketIoState>>;

type ClientWebSocketStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type ServerTcpWebSocketStream = WebSocketStream<TcpStream>;
#[cfg(feature = "tls")]
type ServerTlsWebSocketStream =
    WebSocketStream<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>;

enum RuntimeWebSocketStream {
    Client(ClientWebSocketStream),
    ServerTcp(ServerTcpWebSocketStream),
    #[cfg(feature = "tls")]
    ServerTls(ServerTlsWebSocketStream),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum WebSocketPhase {
    #[default]
    Inactive,
    UpgradeObserved,
    UpgradePrepared,
    HandshakeStarted,
    Open,
    Closing,
    Closed,
    Failed,
}

impl WebSocketPhase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Inactive => "inactive",
            Self::UpgradeObserved => "upgrade-observed",
            Self::UpgradePrepared => "upgrade-prepared",
            Self::HandshakeStarted => "handshake-started",
            Self::Open => "open",
            Self::Closing => "closing",
            Self::Closed => "closed",
            Self::Failed => "failed",
        }
    }
}

pub(crate) struct OutboundWebSocketIoState {
    stream: RuntimeWebSocketStream,
    eof: bool,
    close_code: Option<u16>,
    close_reason: Option<String>,
}

impl std::fmt::Debug for OutboundWebSocketIoState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboundWebSocketIoState")
            .field("eof", &self.eof)
            .field("close_code", &self.close_code)
            .field("close_reason", &self.close_reason)
            .finish()
    }
}

impl OutboundWebSocketIoState {
    pub(crate) fn new_client(stream: ClientWebSocketStream) -> Self {
        Self {
            stream: RuntimeWebSocketStream::Client(stream),
            eof: false,
            close_code: None,
            close_reason: None,
        }
    }

    pub(crate) fn new_server_tcp(stream: ServerTcpWebSocketStream) -> Self {
        Self {
            stream: RuntimeWebSocketStream::ServerTcp(stream),
            eof: false,
            close_code: None,
            close_reason: None,
        }
    }

    #[cfg(feature = "tls")]
    pub(crate) fn new_server_tls(stream: ServerTlsWebSocketStream) -> Self {
        Self {
            stream: RuntimeWebSocketStream::ServerTls(stream),
            eof: false,
            close_code: None,
            close_reason: None,
        }
    }

    pub(crate) fn eof(&self) -> bool {
        self.eof
    }

    pub(crate) fn close_code(&self) -> Option<u16> {
        self.close_code
    }

    pub(crate) fn close_reason(&self) -> Option<&str> {
        self.close_reason.as_deref()
    }

    fn record_close_frame(&mut self, frame: Option<CloseFrame>) {
        self.eof = true;
        if let Some(frame) = frame {
            self.close_code = Some(u16::from(frame.code));
            self.close_reason = Some(frame.reason.to_string());
        }
    }

    pub(crate) async fn send_text(&mut self, text: String) -> Result<usize, VmError> {
        let frame = Message::Text(text.clone().into());
        match &mut self.stream {
            RuntimeWebSocketStream::Client(stream) => stream.send(frame).await,
            RuntimeWebSocketStream::ServerTcp(stream) => stream.send(frame).await,
            #[cfg(feature = "tls")]
            RuntimeWebSocketStream::ServerTls(stream) => stream.send(frame).await,
        }
        .map_err(|err| VmError::HostError(format!("failed to send websocket text frame: {err}")))?;
        Ok(text.len())
    }

    pub(crate) async fn send_binary_base64(&mut self, payload: String) -> Result<usize, VmError> {
        let bytes = STANDARD.decode(payload).map_err(|err| {
            VmError::HostError(format!(
                "websocket binary payload must be base64 encoded: {err}",
            ))
        })?;
        let sent = bytes.len();
        let frame = Message::Binary(bytes.into());
        match &mut self.stream {
            RuntimeWebSocketStream::Client(stream) => stream.send(frame).await,
            RuntimeWebSocketStream::ServerTcp(stream) => stream.send(frame).await,
            #[cfg(feature = "tls")]
            RuntimeWebSocketStream::ServerTls(stream) => stream.send(frame).await,
        }
        .map_err(|err| VmError::HostError(format!("failed to send websocket binary frame: {err}")))?;
        Ok(sent)
    }

    pub(crate) async fn send_binary_bytes(&mut self, payload: &[u8]) -> Result<usize, VmError> {
        let sent = payload.len();
        let frame = Message::Binary(payload.to_vec().into());
        match &mut self.stream {
            RuntimeWebSocketStream::Client(stream) => stream.send(frame).await,
            RuntimeWebSocketStream::ServerTcp(stream) => stream.send(frame).await,
            #[cfg(feature = "tls")]
            RuntimeWebSocketStream::ServerTls(stream) => stream.send(frame).await,
        }
        .map_err(|err| VmError::HostError(format!("failed to send websocket binary frame: {err}")))?;
        Ok(sent)
    }

    async fn read_next_frame(&mut self) -> Result<Option<OutboundWebSocketFrame>, VmError> {
        loop {
            let next = match &mut self.stream {
                RuntimeWebSocketStream::Client(stream) => stream.next().await,
                RuntimeWebSocketStream::ServerTcp(stream) => stream.next().await,
                #[cfg(feature = "tls")]
                RuntimeWebSocketStream::ServerTls(stream) => stream.next().await,
            };
            match next {
                Some(Ok(Message::Text(text))) => {
                    return Ok(Some(OutboundWebSocketFrame::Text(text.to_string())));
                }
                Some(Ok(Message::Binary(bytes))) => {
                    return Ok(Some(OutboundWebSocketFrame::Binary(bytes.to_vec())));
                }
                Some(Ok(Message::Ping(payload))) => {
                    let frame = Message::Pong(payload);
                    match &mut self.stream {
                        RuntimeWebSocketStream::Client(stream) => stream.send(frame).await,
                        RuntimeWebSocketStream::ServerTcp(stream) => stream.send(frame).await,
                        #[cfg(feature = "tls")]
                        RuntimeWebSocketStream::ServerTls(stream) => stream.send(frame).await,
                    }
                    .map_err(|err| {
                        VmError::HostError(format!("failed to reply to websocket ping: {err}",))
                    })?;
                }
                Some(Ok(Message::Pong(_))) => {}
                Some(Ok(Message::Close(frame))) => {
                    self.record_close_frame(frame);
                    return Ok(None);
                }
                Some(Ok(_)) => {}
                Some(Err(err)) => {
                    self.eof = true;
                    return Err(VmError::HostError(format!(
                        "failed to read websocket frame: {err}",
                    )));
                }
                None => {
                    self.eof = true;
                    return Ok(None);
                }
            }
        }
    }

    pub(crate) async fn read_text(&mut self) -> Result<Option<String>, VmError> {
        match self.read_next_frame().await? {
            Some(OutboundWebSocketFrame::Text(text)) => Ok(Some(text)),
            Some(OutboundWebSocketFrame::Binary(_)) => Err(VmError::HostError(
                "next websocket frame is binary; call websocket::connection::read_binary_base64"
                    .to_string(),
            )),
            None => Ok(None),
        }
    }

    pub(crate) async fn read_binary_bytes(&mut self) -> Result<Option<Vec<u8>>, VmError> {
        match self.read_next_frame().await? {
            Some(OutboundWebSocketFrame::Binary(bytes)) => Ok(Some(bytes)),
            Some(OutboundWebSocketFrame::Text(_)) => Err(VmError::HostError(
                "next websocket frame is text; binary byte-stream mode requires binary frames"
                    .to_string(),
            )),
            None => Ok(None),
        }
    }

    pub(crate) async fn read_binary_base64(&mut self) -> Result<Option<String>, VmError> {
        match self.read_next_frame().await? {
            Some(OutboundWebSocketFrame::Binary(bytes)) => Ok(Some(STANDARD.encode(bytes))),
            Some(OutboundWebSocketFrame::Text(_)) => Err(VmError::HostError(
                "next websocket frame is text; call websocket::connection::read_text".to_string(),
            )),
            None => Ok(None),
        }
    }

    pub(crate) async fn close(&mut self, code: u16, reason: String) -> Result<(), VmError> {
        let close_frame = Some(CloseFrame {
            code: CloseCode::from(code),
            reason: reason.clone().into(),
        });
        match &mut self.stream {
            RuntimeWebSocketStream::Client(stream) => stream.close(close_frame.clone()).await,
            RuntimeWebSocketStream::ServerTcp(stream) => stream.close(close_frame.clone()).await,
            #[cfg(feature = "tls")]
            RuntimeWebSocketStream::ServerTls(stream) => stream.close(close_frame.clone()).await,
        }
        .map_err(|err| {
            VmError::HostError(format!("failed to close websocket session: {err}"))
        })?;
        self.record_close_frame(close_frame);
        Ok(())
    }
}

enum OutboundWebSocketFrame {
    Text(String),
    Binary(Vec<u8>),
}

#[derive(Clone, Debug)]
pub(crate) struct WebSocketConnectionState {
    phase: WebSocketPhase,
    present: bool,
    requested_subprotocols: Vec<String>,
    negotiated_subprotocol: Option<String>,
    failure_message: Option<String>,
    close_code: Option<u16>,
    close_reason: Option<String>,
    io: Option<SharedWebSocketIo>,
}

impl Default for WebSocketConnectionState {
    fn default() -> Self {
        Self {
            phase: WebSocketPhase::Inactive,
            present: false,
            requested_subprotocols: Vec::new(),
            negotiated_subprotocol: None,
            failure_message: None,
            close_code: None,
            close_reason: None,
            io: None,
        }
    }
}

impl WebSocketConnectionState {
    pub(crate) fn for_http_request(headers: &HeaderMap) -> Self {
        if !is_downstream_websocket_upgrade(headers) {
            return Self::default();
        }
        Self {
            phase: WebSocketPhase::UpgradeObserved,
            present: true,
            requested_subprotocols: parse_subprotocols_header(headers),
            negotiated_subprotocol: None,
            failure_message: None,
            close_code: None,
            close_reason: None,
            io: None,
        }
    }

    pub(crate) fn phase(&self) -> WebSocketPhase {
        self.phase
    }

    pub(crate) fn is_present(&self) -> bool {
        self.present
    }

    pub(crate) fn is_websocket_mode(&self) -> bool {
        self.present || self.phase != WebSocketPhase::Inactive
    }

    pub(crate) fn is_open(&self) -> bool {
        self.phase == WebSocketPhase::Open && self.io.is_some()
    }

    pub(crate) fn prepare_outbound(&mut self) {
        self.present = true;
        self.failure_message = None;
        if matches!(
            self.phase,
            WebSocketPhase::Inactive | WebSocketPhase::UpgradeObserved
        ) {
            self.phase = WebSocketPhase::UpgradePrepared;
        }
    }

    pub(crate) fn set_requested_subprotocols(&mut self, requested_subprotocols: Vec<String>) {
        self.prepare_outbound();
        self.requested_subprotocols = requested_subprotocols;
    }

    pub(crate) fn requested_subprotocols(&self) -> &[String] {
        &self.requested_subprotocols
    }

    pub(crate) fn negotiated_subprotocol(&self) -> &str {
        self.negotiated_subprotocol.as_deref().unwrap_or("")
    }

    pub(crate) fn note_handshake_started(&mut self) {
        self.prepare_outbound();
        self.phase = WebSocketPhase::HandshakeStarted;
        self.negotiated_subprotocol = None;
        self.close_code = None;
        self.close_reason = None;
        self.io = None;
    }

    pub(crate) fn mark_open(
        &mut self,
        io: SharedWebSocketIo,
        negotiated_subprotocol: Option<String>,
    ) {
        self.present = true;
        self.phase = WebSocketPhase::Open;
        self.negotiated_subprotocol = negotiated_subprotocol;
        self.failure_message = None;
        self.io = Some(io);
    }

    pub(crate) fn note_closing(&mut self) {
        if self.phase == WebSocketPhase::Open {
            self.phase = WebSocketPhase::Closing;
        }
    }

    pub(crate) fn refresh_close_state(&mut self) {
        let Some(io) = self.io.as_ref() else {
            return;
        };
        if let Ok(io) = io.try_lock()
            && io.eof()
        {
            self.phase = WebSocketPhase::Closed;
            self.close_code = io.close_code();
            self.close_reason = io.close_reason().map(str::to_string);
        }
    }

    pub(crate) fn mark_closed(&mut self, code: Option<u16>, reason: Option<String>) {
        self.phase = WebSocketPhase::Closed;
        self.close_code = code;
        self.close_reason = reason;
    }

    pub(crate) fn mark_failed(&mut self, message: impl Into<String>) {
        self.phase = WebSocketPhase::Failed;
        self.present = true;
        self.failure_message = Some(message.into());
        self.io = None;
    }

    pub(crate) fn eof(&mut self) -> bool {
        self.refresh_close_state();
        matches!(self.phase, WebSocketPhase::Closed | WebSocketPhase::Failed)
    }

    pub(crate) fn io(&self) -> Option<SharedWebSocketIo> {
        self.io.clone()
    }
}

fn is_downstream_websocket_upgrade(headers: &HeaderMap) -> bool {
    let connection_has_upgrade = headers
        .get("connection")
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .any(|token| token.eq_ignore_ascii_case("upgrade"))
        })
        .unwrap_or(false);
    let upgrade_is_websocket = headers
        .get("upgrade")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    let has_key = headers.contains_key("sec-websocket-key");
    connection_has_upgrade && upgrade_is_websocket && has_key
}

fn parse_subprotocols_header(headers: &HeaderMap) -> Vec<String> {
    headers
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::{WebSocketConnectionState, WebSocketPhase};

    #[test]
    fn downstream_upgrade_detection_requires_websocket_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("connection", HeaderValue::from_static("Upgrade"));
        headers.insert("upgrade", HeaderValue::from_static("websocket"));
        headers.insert(
            "sec-websocket-key",
            HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
        );
        headers.insert(
            "sec-websocket-protocol",
            HeaderValue::from_static("chat, superchat"),
        );

        let state = WebSocketConnectionState::for_http_request(&headers);
        assert!(state.is_present());
        assert_eq!(state.phase(), WebSocketPhase::UpgradeObserved);
        assert_eq!(
            state.requested_subprotocols(),
            ["chat".to_string(), "superchat".to_string()]
        );
    }

    #[test]
    fn outbound_phase_progression_is_monotonic() {
        let mut state = WebSocketConnectionState::default();
        assert_eq!(state.phase(), WebSocketPhase::Inactive);
        assert!(!state.is_present());

        state.prepare_outbound();
        assert_eq!(state.phase(), WebSocketPhase::UpgradePrepared);
        assert!(state.is_present());

        state.note_handshake_started();
        assert_eq!(state.phase(), WebSocketPhase::HandshakeStarted);

        state.mark_failed("boom");
        assert_eq!(state.phase(), WebSocketPhase::Failed);
        assert!(state.eof());
    }
}
