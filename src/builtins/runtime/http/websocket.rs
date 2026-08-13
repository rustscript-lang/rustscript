use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::{Sink, Stream};
use pd_host_function::pd_host_function;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Message, WebSocketConfig};

use super::super::typed::VmMapHandle;
use super::super::{HostCallResult, VmCallable, VmMap};
use super::HttpRequestContext;
use super::config::HttpConfig;
use super::policy::{ConnectionPermit, SchemeFamily, request_deadline, resolve_url, with_deadline};
use crate::vm::{
    CallOutcome, HostStreamAction, HostStreamDriver, HostStreamPoll, Value, Vm, VmError, VmResult,
};

trait WebSocketIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> WebSocketIo for T {}
type BoxIo = Box<dyn WebSocketIo>;
type Socket = WebSocketStream<CloseAckIo>;
type ConnectFuture = Pin<Box<dyn Future<Output = VmResult<ConnectedSocket>> + Send>>;

struct CloseAckIo {
    inner: BoxIo,
    override_frame: Option<CloseFrame>,
    pending_replacement: Option<PendingCloseAck>,
}

struct PendingCloseAck {
    original: Vec<u8>,
    replacement: Vec<u8>,
    written: usize,
}

impl CloseAckIo {
    fn new(inner: BoxIo) -> Self {
        Self {
            inner,
            override_frame: None,
            pending_replacement: None,
        }
    }

    fn set_override(&mut self, frame: CloseFrame) {
        self.override_frame = Some(frame);
    }

    fn poll_replacement(
        &mut self,
        cx: &mut Context<'_>,
        original: &[u8],
    ) -> Poll<io::Result<usize>> {
        let pending = self
            .pending_replacement
            .as_mut()
            .expect("pending close acknowledgment replacement");
        if pending.original != original {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "WebSocket close acknowledgment retry changed buffered bytes",
            )));
        }
        match Pin::new(&mut self.inner).poll_write(cx, &pending.replacement[pending.written..]) {
            Poll::Ready(Ok(0)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write WebSocket close acknowledgment",
            ))),
            Poll::Ready(Ok(written)) => {
                pending.written += written;
                if pending.written == pending.replacement.len() {
                    let consumed = pending.original.len();
                    self.pending_replacement = None;
                    self.override_frame = None;
                    Poll::Ready(Ok(consumed))
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn replacement_frame(frame: CloseFrame, tungstenite_frame: &[u8]) -> io::Result<Vec<u8>> {
        if tungstenite_frame.len() < 6
            || tungstenite_frame[0] != 0x88
            || tungstenite_frame[1] & 0x80 == 0
            || tungstenite_frame[1] & 0x7f >= 126
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected a complete masked WebSocket close acknowledgment",
            ));
        }
        let original_len = usize::from(tungstenite_frame[1] & 0x7f);
        if tungstenite_frame.len() != 6 + original_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected a complete masked WebSocket close acknowledgment",
            ));
        }

        let mut payload = Vec::with_capacity(2 + frame.reason.len());
        payload.extend_from_slice(&u16::from(frame.code).to_be_bytes());
        payload.extend_from_slice(frame.reason.as_bytes());
        let mask: [u8; 4] = tungstenite_frame[2..6]
            .try_into()
            .expect("four-byte WebSocket mask");
        let mut replacement = Vec::with_capacity(6 + payload.len());
        replacement.extend_from_slice(&[0x88, 0x80 | payload.len() as u8]);
        replacement.extend_from_slice(&mask);
        replacement.extend(
            payload
                .into_iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index & 3]),
        );
        Ok(replacement)
    }
}

impl AsyncRead for CloseAckIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for CloseAckIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.pending_replacement.is_some() {
            return self.poll_replacement(cx, buf);
        }
        let Some(frame) = self.override_frame.clone() else {
            return Pin::new(&mut self.inner).poll_write(cx, buf);
        };
        let replacement = Self::replacement_frame(frame.clone(), buf)?;
        self.pending_replacement = Some(PendingCloseAck {
            original: buf.to_vec(),
            replacement,
            written: 0,
        });
        self.poll_replacement(cx, buf)
    }

    fn is_write_vectored(&self) -> bool {
        false
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[derive(Clone)]
struct WebSocketRequest {
    url: url::Url,
    headers: Vec<(HeaderName, HeaderValue)>,
    protocols: Vec<String>,
}

struct ConnectedSocket {
    socket: Socket,
    status: u16,
    headers: VmMap,
    protocol: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemKind {
    Open,
    Text,
    Binary,
    Ping,
    Pong,
    Close,
}

struct ActiveSocket {
    socket: Socket,
    status: u16,
    headers: VmMap,
    protocol: Option<String>,
    open_pending: bool,
    current_item: Option<ItemKind>,
    outbound: Option<Message>,
    flush_required: bool,
    local_closing: bool,
    complete_after_flush: bool,
    close_deadline: Option<Instant>,
    close_sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    idle_sleep: Option<Pin<Box<tokio::time::Sleep>>>,
}

enum DriverState {
    Connecting(ConnectFuture),
    Active(Box<ActiveSocket>),
    Finished,
}

struct WebSocketDriver {
    config: HttpConfig,
    request: WebSocketRequest,
    _permit: ConnectionPermit,
    state: DriverState,
    items: usize,
    bytes_received: usize,
    bytes_sent: usize,
    call_deadline: Option<Instant>,
    call_sleep: Option<Pin<Box<tokio::time::Sleep>>>,
}

/// Opens a bounded WebSocket session and serializes callback actions with socket polling.
#[pd_host_function(name = "http::client::websocket")]
pub(super) fn builtin_http_client_websocket_impl(
    vm: &mut Vm,
    request: VmMapHandle,
    callback: VmCallable<fn(VmMap) -> VmMap>,
) -> VmResult<HostCallResult<VmMap>> {
    let callback = callback.into_value();
    vm.validate_stream_callback_value(&callback)?;

    let script_timeout = parse_websocket_timeout(request.as_ref())?;
    let (context, deadline) = HttpRequestContext::capture_stream(vm, script_timeout, "WebSocket")?;
    let request = parse_websocket_request(request.as_ref(), &context.config)?;
    let (config, permit) = context.into_parts();
    let driver = WebSocketDriver::new(config, request, permit, Some(deadline))?;
    match vm.submit_callable_stream(callback, driver)? {
        CallOutcome::Pending(op_id) => Ok(HostCallResult::Pending(op_id)),
        _ => Err(VmError::InvalidFrameState(
            "WebSocket callable stream did not suspend",
        )),
    }
}

impl WebSocketDriver {
    fn new(
        config: HttpConfig,
        request: WebSocketRequest,
        permit: ConnectionPermit,
        externally_bounded_deadline: Option<Instant>,
    ) -> VmResult<Self> {
        let connect_deadline = request_deadline(config.connect_timeout)?;
        let connect_deadline = externally_bounded_deadline
            .map_or(connect_deadline, |deadline| deadline.min(connect_deadline));
        let future = connect_socket(config.clone(), request.clone(), connect_deadline);
        Ok(Self {
            config,
            request,
            _permit: permit,
            state: DriverState::Connecting(Box::pin(future)),
            items: 0,
            bytes_received: 0,
            bytes_sent: 0,
            call_deadline: externally_bounded_deadline,
            call_sleep: None,
        })
    }

    fn summary(&self, outcome: &str, active: &ActiveSocket) -> Value {
        Value::Map(Arc::new(VmMap::from_entries(vec![
            (Value::string("outcome"), Value::string(outcome)),
            (
                Value::string("status"),
                Value::Int(i64::from(active.status)),
            ),
            (
                Value::string("headers"),
                Value::Map(Arc::new(active.headers.clone())),
            ),
            (
                Value::string("url"),
                Value::string(self.request.url.as_str()),
            ),
            (Value::string("items"), Value::Int(self.items as i64)),
            (
                Value::string("bytes_received"),
                Value::Int(self.bytes_received as i64),
            ),
            (
                Value::string("bytes_sent"),
                Value::Int(self.bytes_sent as i64),
            ),
        ])))
    }

    fn check_total_deadline(&self) -> VmResult<()> {
        if self
            .call_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(VmError::HostError(
                "WebSocket call deadline exceeded".to_string(),
            ));
        }
        Ok(())
    }

    fn poll_total_deadline(&mut self, cx: &mut Context<'_>) -> VmResult<()> {
        self.check_total_deadline()?;
        if self.call_sleep.is_none()
            && let Some(deadline) = self.call_deadline
        {
            self.call_sleep = Some(Box::pin(tokio::time::sleep_until(
                tokio::time::Instant::from_std(deadline),
            )));
        }
        if self
            .call_sleep
            .as_mut()
            .is_some_and(|sleep| sleep.as_mut().poll(cx).is_ready())
        {
            return Err(VmError::HostError(
                "WebSocket call deadline exceeded".to_string(),
            ));
        }
        Ok(())
    }

    fn complete(&self, outcome: &str, active: &ActiveSocket) -> HostStreamPoll {
        HostStreamPoll::Complete(self.summary(outcome, active))
    }

    fn poll_active(
        &mut self,
        cx: &mut Context<'_>,
        active: &mut ActiveSocket,
    ) -> Poll<VmResult<HostStreamPoll>> {
        if active.open_pending {
            active.open_pending = false;
            active.current_item = Some(ItemKind::Open);
            self.items += 1;
            return Poll::Ready(Ok(HostStreamPoll::Item(open_item(
                active.status,
                &active.headers,
                &self.request.url,
                active.protocol.as_deref(),
            ))));
        }

        if active.local_closing || active.complete_after_flush {
            let deadline = active.close_deadline.ok_or(VmError::InvalidFrameState(
                "WebSocket close handshake has no deadline",
            ))?;
            if Instant::now() >= deadline {
                return Poll::Ready(Err(VmError::HostError(
                    "WebSocket close handshake timed out".to_string(),
                )));
            }
            let sleep = active.close_sleep.get_or_insert_with(|| {
                Box::pin(tokio::time::sleep_until(tokio::time::Instant::from_std(
                    deadline,
                )))
            });
            if sleep.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Err(VmError::HostError(
                    "WebSocket close handshake timed out".to_string(),
                )));
            }
        }

        if let Some(message) = active.outbound.take() {
            match Pin::new(&mut active.socket).poll_ready(cx) {
                Poll::Pending => {
                    active.outbound = Some(message);
                    return Poll::Pending;
                }
                Poll::Ready(Err(error)) => {
                    return Poll::Ready(Err(socket_error("WebSocket write failed", error)));
                }
                Poll::Ready(Ok(())) => {}
            }
            if let Err(error) = Pin::new(&mut active.socket).start_send(message) {
                return Poll::Ready(Err(socket_error("WebSocket write failed", error)));
            }
            active.flush_required = true;
        }
        if active.flush_required {
            match Pin::new(&mut active.socket).poll_flush(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => {
                    return Poll::Ready(Err(socket_error("WebSocket write failed", error)));
                }
                Poll::Ready(Ok(())) => active.flush_required = false,
            }
        }
        if active.complete_after_flush {
            return Poll::Ready(Ok(self.complete("closed", active)));
        }

        if !active.local_closing {
            if active.idle_sleep.is_none() {
                active.idle_sleep = Some(Box::pin(tokio::time::sleep(
                    self.config.stream_idle_timeout,
                )));
            }
            if active
                .idle_sleep
                .as_mut()
                .is_some_and(|sleep| sleep.as_mut().poll(cx).is_ready())
            {
                return Poll::Ready(Err(VmError::HostError(
                    "WebSocket stream idle timeout exceeded".to_string(),
                )));
            }
        }

        const MAX_DISCARDED_MESSAGES_PER_POLL: usize = 32;
        let mut discarded_messages = 0;
        loop {
            let message = match Pin::new(&mut active.socket).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    return Poll::Ready(Err(VmError::HostError(
                        "WebSocket transport ended without a close handshake".to_string(),
                    )));
                }
                Poll::Ready(Some(Err(error))) => {
                    return Poll::Ready(Err(socket_error("WebSocket receive failed", error)));
                }
                Poll::Ready(Some(Ok(message))) => message,
            };
            active.idle_sleep = None;

            if active.local_closing {
                match message {
                    Message::Close(_) => {
                        return Poll::Ready(Ok(self.complete("closed", active)));
                    }
                    Message::Text(_) | Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {
                        discarded_messages += 1;
                        if discarded_messages == MAX_DISCARDED_MESSAGES_PER_POLL {
                            cx.waker().wake_by_ref();
                            return Poll::Pending;
                        }
                        continue;
                    }
                    Message::Frame(_) => {
                        return Poll::Ready(Err(VmError::HostError(
                            "WebSocket exposed an unexpected raw frame".to_string(),
                        )));
                    }
                }
            }

            if matches!(&message, Message::Text(_) | Message::Binary(_)) {
                self.bytes_received = checked_application_counter(
                    self.bytes_received,
                    self.bytes_sent,
                    message.len(),
                    self.config.max_stream_total_bytes,
                )?;
            }
            self.items += 1;
            let (kind, item) = match message {
                Message::Text(text) => (
                    ItemKind::Text,
                    item_map(vec![
                        ("kind", Value::string("text")),
                        ("text", Value::string(text.as_str())),
                    ]),
                ),
                Message::Binary(data) => (
                    ItemKind::Binary,
                    item_map(vec![
                        ("kind", Value::string("binary")),
                        ("data", Value::bytes(data.to_vec())),
                    ]),
                ),
                Message::Ping(data) => (
                    ItemKind::Ping,
                    item_map(vec![
                        ("kind", Value::string("ping")),
                        ("data", Value::bytes(data.to_vec())),
                    ]),
                ),
                Message::Pong(data) => (
                    ItemKind::Pong,
                    item_map(vec![
                        ("kind", Value::string("pong")),
                        ("data", Value::bytes(data.to_vec())),
                    ]),
                ),
                Message::Close(frame) => {
                    let (code, reason) = frame.map_or((Value::Null, String::new()), |frame| {
                        (
                            Value::Int(u16::from(frame.code) as i64),
                            frame.reason.to_string(),
                        )
                    });
                    (
                        ItemKind::Close,
                        item_map(vec![
                            ("kind", Value::string("close")),
                            ("code", code),
                            ("reason", Value::string(reason)),
                        ]),
                    )
                }
                Message::Frame(_) => {
                    return Poll::Ready(Err(VmError::HostError(
                        "WebSocket exposed an unexpected raw frame".to_string(),
                    )));
                }
            };
            active.current_item = Some(kind);
            return Poll::Ready(Ok(HostStreamPoll::Item(item)));
        }
    }
}

impl HostStreamDriver for WebSocketDriver {
    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<HostStreamPoll>> {
        if let Err(error) = self.poll_total_deadline(cx) {
            self.state = DriverState::Finished;
            return Poll::Ready(Err(error));
        }
        loop {
            match &mut self.state {
                DriverState::Connecting(future) => match future.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => {
                        self.state = DriverState::Finished;
                        return Poll::Ready(Err(error));
                    }
                    Poll::Ready(Ok(connected)) => {
                        self.state = DriverState::Active(Box::new(ActiveSocket {
                            socket: connected.socket,
                            status: connected.status,
                            headers: connected.headers,
                            protocol: connected.protocol,
                            open_pending: true,
                            current_item: None,
                            outbound: None,
                            flush_required: false,
                            local_closing: false,
                            complete_after_flush: false,
                            close_deadline: None,
                            close_sleep: None,
                            idle_sleep: None,
                        }));
                    }
                },
                DriverState::Active(_) => {
                    let mut active = match std::mem::replace(&mut self.state, DriverState::Finished)
                    {
                        DriverState::Active(active) => active,
                        _ => unreachable!(),
                    };
                    let result = self.poll_active(cx, &mut active);
                    if matches!(self.state, DriverState::Finished) {
                        if !matches!(result, Poll::Ready(Ok(HostStreamPoll::Complete(_)))) {
                            self.state = DriverState::Active(active);
                        }
                    } else {
                        self.state = DriverState::Active(active);
                    }
                    return result;
                }
                DriverState::Finished => {
                    return Poll::Ready(Err(VmError::InvalidFrameState(
                        "WebSocket driver polled after completion",
                    )));
                }
            }
        }
    }

    fn apply_action(&mut self, action: Value) -> VmResult<HostStreamAction> {
        self.check_total_deadline()?;
        let summary_url = self.request.url.clone();
        let summary_items = self.items;
        let summary_received = self.bytes_received;
        let summary_sent = self.bytes_sent;
        let close_timeout = self.config.websocket_close_timeout;
        let call_deadline = self.call_deadline;
        let DriverState::Active(active) = &mut self.state else {
            return Err(VmError::InvalidFrameState(
                "WebSocket action applied without an active connection",
            ));
        };
        let kind = active
            .current_item
            .take()
            .ok_or(VmError::InvalidFrameState(
                "WebSocket action applied without an item",
            ))?;
        let action_map = value_map(&action, "WebSocket callback action")?;
        let action_name = required_string(action_map, "action", "WebSocket callback action")?;

        if action_name == "stop" {
            let summary = summary_value(
                "stopped",
                active,
                &summary_url,
                summary_items,
                summary_received,
                summary_sent,
            );
            self.state = DriverState::Finished;
            return Ok(HostStreamAction::Complete(summary));
        }

        let outbound = match action_name.as_str() {
            "continue" => match kind {
                ItemKind::Ping => {
                    active.flush_required = true;
                    None
                }
                ItemKind::Close => {
                    active.complete_after_flush = true;
                    active.flush_required = true;
                    start_close_deadline(
                        &mut active.close_deadline,
                        Instant::now(),
                        close_timeout,
                        call_deadline,
                    )?;
                    None
                }
                _ => None,
            },
            "send_text" if matches!(kind, ItemKind::Open | ItemKind::Text | ItemKind::Binary) => {
                let text = required_string(action_map, "text", "WebSocket send_text action")?;
                self.bytes_sent = validate_send_size(
                    &self.config,
                    self.bytes_received,
                    self.bytes_sent,
                    text.len(),
                )?;
                Some(Message::text(text))
            }
            "send_binary" if matches!(kind, ItemKind::Open | ItemKind::Text | ItemKind::Binary) => {
                let data = required_bytes(action_map, "data", "WebSocket send_binary action")?;
                self.bytes_sent = validate_send_size(
                    &self.config,
                    self.bytes_received,
                    self.bytes_sent,
                    data.len(),
                )?;
                Some(Message::binary(data))
            }
            "ping" if !matches!(kind, ItemKind::Ping | ItemKind::Close) => {
                let data = required_bytes(action_map, "data", "WebSocket ping action")?;
                validate_control_payload(&self.config, &data)?;
                Some(Message::Ping(data.into()))
            }
            "pong"
                if matches!(
                    kind,
                    ItemKind::Open | ItemKind::Text | ItemKind::Binary | ItemKind::Ping
                ) =>
            {
                let data = required_bytes(action_map, "data", "WebSocket pong action")?;
                validate_control_payload(&self.config, &data)?;
                Some(Message::Pong(data.into()))
            }
            "close" => {
                let frame = parse_close_action(action_map)?;
                let payload_len = 2 + frame.reason.len();
                validate_control_size(&self.config, payload_len)?;
                if kind == ItemKind::Close {
                    active.socket.get_mut().set_override(frame);
                    active.complete_after_flush = true;
                    active.flush_required = true;
                    start_close_deadline(
                        &mut active.close_deadline,
                        Instant::now(),
                        close_timeout,
                        call_deadline,
                    )?;
                    None
                } else {
                    active.local_closing = true;
                    start_close_deadline(
                        &mut active.close_deadline,
                        Instant::now(),
                        close_timeout,
                        call_deadline,
                    )?;
                    Some(Message::Close(Some(frame)))
                }
            }
            _ => {
                return Err(VmError::HostError(format!(
                    "WebSocket action '{action_name}' is invalid for the current item"
                )));
            }
        };
        active.outbound = outbound;
        Ok(HostStreamAction::Continue)
    }
}

fn start_close_deadline(
    close_deadline: &mut Option<Instant>,
    now: Instant,
    close_timeout: Duration,
    call_deadline: Option<Instant>,
) -> VmResult<()> {
    if close_deadline.is_some() {
        return Ok(());
    }
    let deadline = now.checked_add(close_timeout).ok_or_else(|| {
        VmError::HostError("WebSocket close timeout cannot form a deadline".to_string())
    })?;
    *close_deadline = Some(call_deadline.map_or(deadline, |call| call.min(deadline)));
    Ok(())
}

async fn connect_socket(
    config: HttpConfig,
    request: WebSocketRequest,
    deadline: Instant,
) -> VmResult<ConnectedSocket> {
    connect_socket_with_tls_config(config, request, deadline, None).await
}

async fn connect_socket_with_tls_config(
    config: HttpConfig,
    request: WebSocketRequest,
    deadline: Instant,
    test_tls_config: Option<Arc<rustls::ClientConfig>>,
) -> VmResult<ConnectedSocket> {
    with_deadline(deadline, async move {
        let resolved = resolve_url(&config, SchemeFamily::WebSocket, &request.url).await?;
        let stream = tokio::net::TcpStream::connect(resolved.address)
            .await
            .map_err(|error| VmError::HostError(format!("WebSocket connect failed: {error}")))?;
        stream
            .set_nodelay(true)
            .map_err(|error| VmError::HostError(format!("WebSocket connect failed: {error}")))?;
        let io: BoxIo = if request.url.scheme() == "wss" {
            let mut tls_config = if let Some(config) = test_tls_config {
                (*config).clone()
            } else {
                let mut roots = rustls::RootCertStore::empty();
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                rustls::ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth()
            };
            tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
            let server_name = rustls::pki_types::ServerName::try_from(resolved.host.clone())
                .map_err(|_| {
                    VmError::HostError("WebSocket TLS server name is invalid".to_string())
                })?;
            let tls = tokio_rustls::TlsConnector::from(Arc::new(tls_config))
                .connect(server_name, stream)
                .await
                .map_err(|error| VmError::HostError(format!("WebSocket TLS failed: {error}")))?;
            Box::new(tls)
        } else {
            Box::new(stream)
        };

        let mut handshake = request
            .url
            .as_str()
            .into_client_request()
            .map_err(|error| {
                VmError::HostError(format!("WebSocket handshake setup failed: {error}"))
            })?;
        for (name, value) in &request.headers {
            handshake.headers_mut().append(name, value.clone());
        }
        if !request.protocols.is_empty() {
            handshake.headers_mut().insert(
                "sec-websocket-protocol",
                HeaderValue::from_str(&request.protocols.join(", ")).map_err(|_| {
                    VmError::HostError("WebSocket protocols are invalid".to_string())
                })?,
            );
        }
        let ws_config = WebSocketConfig::default()
            .read_buffer_size(config.max_websocket_frame_bytes.min(128 * 1024))
            .write_buffer_size(0)
            .max_write_buffer_size(config.max_websocket_send_bytes.saturating_add(1024))
            .max_message_size(Some(config.max_stream_item_bytes))
            .max_frame_size(Some(config.max_websocket_frame_bytes));
        let io = CloseAckIo::new(io);
        let (socket, response) =
            tokio_tungstenite::client_async_with_config(handshake, io, Some(ws_config))
                .await
                .map_err(|error| {
                    VmError::HostError(format!("WebSocket handshake failed: {error}"))
                })?;
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Err(VmError::HostError(format!(
                "WebSocket handshake returned status {}",
                response.status()
            )));
        }
        let selected = response
            .headers()
            .get("sec-websocket-protocol")
            .map(|value| {
                value.to_str().map(str::to_string).map_err(|_| {
                    VmError::HostError("WebSocket selected protocol is invalid".to_string())
                })
            })
            .transpose()?;
        if selected
            .as_ref()
            .is_some_and(|selected| !request.protocols.iter().any(|offered| offered == selected))
        {
            return Err(VmError::HostError(
                "WebSocket server selected an unoffered protocol".to_string(),
            ));
        }
        let headers = VmMap::from_entries(
            response
                .headers()
                .iter()
                .map(|(name, value)| {
                    let value = value
                        .to_str()
                        .map(Value::string)
                        .unwrap_or_else(|_| Value::bytes(value.as_bytes().to_vec()));
                    (Value::string(name.as_str()), value)
                })
                .collect(),
        );
        Ok(ConnectedSocket {
            socket,
            status: response.status().as_u16(),
            headers,
            protocol: selected,
        })
    })
    .await
}

fn parse_websocket_request(map: &VmMap, config: &HttpConfig) -> VmResult<WebSocketRequest> {
    let url = required_string(map, "url", "WebSocket request")?
        .parse::<url::Url>()
        .map_err(|error| VmError::HostError(format!("invalid WebSocket URL: {error}")))?;
    super::policy::validate_url_policy(config, SchemeFamily::WebSocket, &url)?;

    let mut headers = Vec::new();
    match map.get(&Value::string("headers")) {
        None | Some(Value::Null) => {}
        Some(Value::Map(entries)) => {
            for (name, value) in entries.iter() {
                let Value::String(name) = name else {
                    return Err(VmError::TypeMismatch("WebSocket header name"));
                };
                let Value::String(value) = value else {
                    return Err(VmError::TypeMismatch("WebSocket header value"));
                };
                let lower = name.to_ascii_lowercase();
                if lower == "host"
                    || lower == "upgrade"
                    || lower == "connection"
                    || lower.starts_with("sec-websocket-")
                {
                    return Err(VmError::HostError(format!(
                        "WebSocket header '{name}' is managed by the client"
                    )));
                }
                let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                    VmError::HostError(format!("invalid WebSocket header name '{name}'"))
                })?;
                let value = HeaderValue::from_str(value).map_err(|_| {
                    VmError::HostError(format!("invalid WebSocket header value for '{name}'"))
                })?;
                headers.push((name, value));
            }
        }
        Some(_) => return Err(VmError::TypeMismatch("WebSocket headers")),
    }

    let protocols = match map.get(&Value::string("protocols")) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(values)) => {
            let mut protocols = Vec::with_capacity(values.len());
            for value in values.iter() {
                let Value::String(protocol) = value else {
                    return Err(VmError::TypeMismatch("WebSocket protocol"));
                };
                if protocol.is_empty()
                    || !protocol.bytes().all(|byte| {
                        matches!(byte, 0x21..=0x7e)
                            && !matches!(
                                byte,
                                b'(' | b')'
                                    | b'<'
                                    | b'>'
                                    | b'@'
                                    | b','
                                    | b';'
                                    | b':'
                                    | b'\\'
                                    | b'"'
                                    | b'/'
                                    | b'['
                                    | b']'
                                    | b'?'
                                    | b'='
                                    | b'{'
                                    | b'}'
                            )
                    })
                    || protocols
                        .iter()
                        .any(|existing| existing == protocol.as_str())
                {
                    return Err(VmError::HostError(
                        "WebSocket protocols must be unique HTTP tokens".to_string(),
                    ));
                }
                protocols.push(protocol.to_string());
            }
            protocols
        }
        Some(_) => return Err(VmError::TypeMismatch("WebSocket protocols")),
    };

    Ok(WebSocketRequest {
        url,
        headers,
        protocols,
    })
}

fn parse_websocket_timeout(request: &VmMap) -> VmResult<Option<Duration>> {
    let Some(value) = request.get(&Value::string("timeout_ms")) else {
        return Ok(None);
    };
    let Value::Int(milliseconds) = value else {
        return Err(VmError::TypeMismatch("WebSocket timeout_ms"));
    };
    let milliseconds = u64::try_from(*milliseconds)
        .ok()
        .filter(|milliseconds| *milliseconds > 0)
        .ok_or_else(|| VmError::HostError("WebSocket timeout_ms must be positive".to_string()))?;
    Ok(Some(Duration::from_millis(milliseconds)))
}

fn open_item(status: u16, headers: &VmMap, url: &url::Url, protocol: Option<&str>) -> Value {
    item_map(vec![
        ("kind", Value::string("open")),
        ("status", Value::Int(i64::from(status))),
        ("headers", Value::Map(Arc::new(headers.clone()))),
        ("url", Value::string(url.as_str())),
        ("protocol", protocol.map_or(Value::Null, Value::string)),
    ])
}

fn item_map(entries: Vec<(&str, Value)>) -> Value {
    Value::Map(Arc::new(VmMap::from_entries(
        entries
            .into_iter()
            .map(|(key, value)| (Value::string(key), value))
            .collect(),
    )))
}

fn summary_value(
    outcome: &str,
    active: &ActiveSocket,
    url: &url::Url,
    items: usize,
    bytes_received: usize,
    bytes_sent: usize,
) -> Value {
    Value::Map(Arc::new(VmMap::from_entries(vec![
        (Value::string("outcome"), Value::string(outcome)),
        (
            Value::string("status"),
            Value::Int(i64::from(active.status)),
        ),
        (
            Value::string("headers"),
            Value::Map(Arc::new(active.headers.clone())),
        ),
        (Value::string("url"), Value::string(url.as_str())),
        (Value::string("items"), Value::Int(items as i64)),
        (
            Value::string("bytes_received"),
            Value::Int(bytes_received as i64),
        ),
        (Value::string("bytes_sent"), Value::Int(bytes_sent as i64)),
    ])))
}

fn value_map<'a>(value: &'a Value, label: &'static str) -> VmResult<&'a VmMap> {
    match value {
        Value::Map(map) => Ok(map),
        _ => Err(VmError::TypeMismatch(label)),
    }
}

fn required_string(map: &VmMap, key: &str, label: &'static str) -> VmResult<String> {
    match map.get(&Value::string(key)) {
        Some(Value::String(value)) => Ok(value.to_string()),
        Some(_) => Err(VmError::TypeMismatch(label)),
        None => Err(VmError::HostError(format!("missing {label} field '{key}'"))),
    }
}

fn required_bytes(map: &VmMap, key: &str, label: &'static str) -> VmResult<Vec<u8>> {
    match map.get(&Value::string(key)) {
        Some(Value::Bytes(value)) => Ok(value.as_ref().clone()),
        Some(_) => Err(VmError::TypeMismatch(label)),
        None => Err(VmError::HostError(format!("missing {label} field '{key}'"))),
    }
}

fn validate_send_size(
    config: &HttpConfig,
    bytes_received: usize,
    bytes_sent: usize,
    size: usize,
) -> VmResult<usize> {
    if size > config.max_websocket_send_bytes {
        return Err(VmError::HostError(
            "WebSocket send payload exceeds limit".to_string(),
        ));
    }
    checked_application_counter(
        bytes_sent,
        bytes_received,
        size,
        config.max_stream_total_bytes,
    )
}

fn checked_application_counter(
    current: usize,
    other: usize,
    additional: usize,
    maximum: usize,
) -> VmResult<usize> {
    let updated = current.checked_add(additional).ok_or_else(|| {
        VmError::HostError("WebSocket stream application byte count overflowed".to_string())
    })?;
    let total = updated.checked_add(other).ok_or_else(|| {
        VmError::HostError("WebSocket stream application byte count overflowed".to_string())
    })?;
    if total > maximum {
        return Err(VmError::HostError(
            "WebSocket stream exceeds total byte limit".to_string(),
        ));
    }
    Ok(updated)
}

fn validate_control_payload(config: &HttpConfig, data: &[u8]) -> VmResult<()> {
    validate_control_size(config, data.len())
}

fn validate_control_size(config: &HttpConfig, size: usize) -> VmResult<()> {
    if size > config.max_websocket_send_bytes {
        return Err(VmError::HostError(
            "WebSocket send payload exceeds limit".to_string(),
        ));
    }
    if size > 125 {
        return Err(VmError::HostError(
            "WebSocket control payload exceeds 125 bytes".to_string(),
        ));
    }
    Ok(())
}

fn parse_close_action(map: &VmMap) -> VmResult<CloseFrame> {
    let code = match map.get(&Value::string("code")) {
        Some(Value::Int(code)) => u16::try_from(*code).ok(),
        Some(_) => return Err(VmError::TypeMismatch("WebSocket close code")),
        None => None,
    }
    .filter(|code| valid_close_code(*code))
    .ok_or_else(|| VmError::HostError("WebSocket close code is invalid".to_string()))?;
    let reason = required_string(map, "reason", "WebSocket close action")?;
    if reason.len() > 123 {
        return Err(VmError::HostError(
            "WebSocket close reason exceeds 123 bytes".to_string(),
        ));
    }
    Ok(CloseFrame {
        code: CloseCode::from(code),
        reason: reason.into(),
    })
}

fn valid_close_code(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999)
}

fn socket_error(prefix: &str, error: tokio_tungstenite::tungstenite::Error) -> VmError {
    VmError::HostError(format!("{prefix}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn config() -> HttpConfig {
        HttpConfig {
            max_websocket_send_bytes: 5,
            max_stream_total_bytes: 9,
            ..HttpConfig::default()
        }
    }

    #[test]
    fn send_and_total_limits_accept_exact_boundaries_and_reject_overflow() {
        let config = config();
        assert_eq!(validate_send_size(&config, 2, 2, 5).unwrap(), 7);
        assert!(
            validate_send_size(&config, 2, 2, 6)
                .unwrap_err()
                .to_string()
                .contains("send payload exceeds limit")
        );
        assert!(
            validate_send_size(&config, 3, 2, 5)
                .unwrap_err()
                .to_string()
                .contains("total byte limit")
        );
    }

    #[test]
    fn control_payload_and_close_code_boundaries_are_explicit() {
        let mut config = config();
        config.max_websocket_send_bytes = 126;
        config.max_stream_total_bytes = 1024;
        assert!(validate_control_payload(&config, &[0; 125]).is_ok());
        assert!(
            validate_control_payload(&config, &[0; 126])
                .unwrap_err()
                .to_string()
                .contains("125 bytes")
        );
        for code in [1000, 1003, 1007, 1014, 3000, 4999] {
            assert!(valid_close_code(code), "code {code} should be valid");
        }
        for code in [0, 999, 1004, 1005, 1006, 1015, 2999, 5000] {
            assert!(!valid_close_code(code), "code {code} should be invalid");
        }
    }

    #[test]
    fn application_byte_counter_is_checked_at_the_limit_and_on_overflow() {
        assert_eq!(checked_application_counter(4, 5, 0, 9).unwrap(), 4);
        assert!(
            checked_application_counter(4, 5, 1, 9)
                .unwrap_err()
                .to_string()
                .contains("total byte limit")
        );
        assert!(
            checked_application_counter(usize::MAX, 0, 1, usize::MAX)
                .unwrap_err()
                .to_string()
                .contains("overflowed")
        );
        assert!(
            checked_application_counter(usize::MAX, 1, 0, usize::MAX)
                .unwrap_err()
                .to_string()
                .contains("overflowed")
        );
    }

    #[test]
    fn close_deadline_starts_once_at_transition_and_is_bounded_by_call_deadline() {
        let started = Instant::now();
        let call_deadline = started + Duration::from_millis(80);
        let mut close_deadline = None;

        start_close_deadline(
            &mut close_deadline,
            started,
            Duration::from_millis(100),
            Some(call_deadline),
        )
        .expect("close deadline should initialize");
        assert_eq!(close_deadline, Some(call_deadline));

        start_close_deadline(
            &mut close_deadline,
            started + Duration::from_millis(20),
            Duration::from_millis(5),
            None,
        )
        .expect("repeated close transition should be a no-op");
        assert_eq!(close_deadline, Some(call_deadline));
    }

    #[test]
    fn wss_uses_http11_alpn_sni_original_host_and_pinned_address() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build");
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("certificate should generate");
        let cert_der = certified.cert.der().clone();
        let key_der =
            rustls::pki_types::PrivateKeyDer::Pkcs8(certified.key_pair.serialize_der().into());
        let mut server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .expect("server TLS should configure");
        server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert_der).expect("certificate should be trusted");
        let client_config = Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener address");
        let server = runtime.spawn(async move {
            let (stream, peer) = listener.accept().await.expect("client should connect");
            assert!(peer.ip().is_loopback());
            let tls = tokio_rustls::TlsAcceptor::from(Arc::new(server_config))
                .accept(stream)
                .await
                .expect("TLS should succeed");
            assert_eq!(tls.get_ref().1.alpn_protocol(), Some(b"http/1.1".as_slice()));
            assert_eq!(tls.get_ref().1.server_name(), Some("localhost"));
            #[allow(clippy::result_large_err)]
            let callback = move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                                 response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                assert_eq!(
                    request.headers()["host"].to_str().unwrap(),
                    format!("localhost:{}", address.port())
                );
                Ok(response)
            };
            tokio_tungstenite::accept_hdr_async(tls, callback)
                .await
                .expect("WebSocket handshake should succeed")
        });
        let config = HttpConfig {
            allowed_schemes: vec!["wss".to_string()],
            allowed_hosts: vec!["localhost".to_string()],
            allowed_ports: vec![address.port()],
            allow_private_ips: true,
            ..HttpConfig::default()
        };
        let request_map = VmMap::from_entries(vec![(
            Value::string("url"),
            Value::string(format!("wss://localhost:{}/socket?q=1", address.port())),
        )]);
        let request = parse_websocket_request(&request_map, &config).expect("request should parse");
        let connected = runtime
            .block_on(connect_socket_with_tls_config(
                config,
                request,
                request_deadline(Duration::from_secs(2)).expect("deadline"),
                Some(client_config),
            ))
            .expect("WSS should connect");
        assert_eq!(connected.status, 101);
        drop(connected);
        runtime.block_on(server).expect("server should complete");
    }
}
