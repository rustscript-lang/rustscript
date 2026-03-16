#![cfg_attr(not(feature = "http"), allow(dead_code))]

use std::{
    collections::HashMap,
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};

use axum::{
    body::{Body, Bytes},
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode, Version,
        header::{CONTENT_LENGTH, CONTENT_TYPE, HOST},
    },
};
use futures_util::stream::try_unfold;
use http_body_util::{BodyExt, Full};
#[cfg(feature = "http3")]
use hyper::body::Buf;
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use tokio::io::copy_bidirectional;
use tokio::sync::oneshot;
use url::Url;
use vm::VmError;
#[cfg(feature = "websocket")]
use {
    futures_util::{SinkExt, StreamExt},
    tokio_tungstenite::{
        WebSocketStream,
        tungstenite::{
            Message,
            handshake::derive_accept_key,
            protocol::{CloseFrame, Role, frame::coding::CloseCode},
        },
    },
};

use super::super::{
    EDGE_IO_HANDLE_DYNAMIC_BASE, EdgeVirtualIoHandle, SharedHttp3UpstreamSessions,
    SharedHttpUpstreamSessions, SharedRateLimiter,
    proxy::ProxyByteStreamState,
    transport::{
        CachedTlsSession, FIRST_DYNAMIC_TCP_STREAM_HANDLE, HTTP11_ALPN_PROTOCOL, ReplayPrefixedIo,
        SharedTcpStreamIo, SharedTlsSessionCache, SharedUdpSocketIo, TcpFlowState, TcpSocketState,
        TcpTransportDag, TlsFlowState, TlsProtocolVersion, TlsSessionCacheKey, TlsTransportDag,
        UdpSocketState, alpn_from_http_version, tls_session_cache_key,
    },
    websocket::WebSocketConnectionState,
};
use super::version::HttpVersionPreference;
#[cfg(feature = "tls")]
use crate::abi_impl::transport::{
    DownstreamTlsServerStart, SharedServerTlsStreamIo, SharedTlsStreamIo,
};
#[cfg(feature = "webrtc")]
use crate::abi_impl::webrtc::WebRtcConnectionState;
#[cfg(feature = "websocket")]
use crate::abi_impl::websocket::{
    close_websocket_binary_stream, read_websocket_binary_bytes, write_websocket_binary_bytes,
};
use crate::abi_impl::{http2, http3};
use crate::cache::BoundedLruStore;

#[derive(Debug)]
pub struct HttpRequestContext {
    pub request_id: String,
    pub method: Method,
    pub path: String,
    pub query: String,
    pub http_version: String,
    pub port: u16,
    pub scheme: String,
    pub host: String,
    pub client_ip: String,
    pub body: Body,
    pub headers: HeaderMap,
}

#[derive(Clone, Debug)]
pub(crate) struct DownstreamConnectionMetadata {
    pub(crate) local_addr: SocketAddr,
    pub(crate) peer_addr: SocketAddr,
    pub(crate) secure: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DownstreamHttpListenerGoal {
    #[default]
    None,
    #[cfg(feature = "tls")]
    Https,
}

impl DownstreamHttpListenerGoal {
    pub(crate) fn promotes_into_http(self) -> bool {
        !matches!(self, Self::None)
    }

    #[cfg(feature = "tls")]
    pub(crate) fn requires_tls(self) -> bool {
        matches!(self, Self::Https)
    }

    #[cfg(not(feature = "tls"))]
    pub(crate) fn requires_tls(self) -> bool {
        false
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HttpRequestHead {
    request_id: String,
    method: Method,
    path: String,
    query: String,
    http_version: String,
    port: u16,
    scheme: String,
    host: String,
    client_ip: String,
    headers: HeaderMap,
}

impl HttpRequestHead {
    pub(crate) fn request_id(&self) -> &str {
        &self.request_id
    }

    pub(crate) fn method(&self) -> &Method {
        &self.method
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    pub(crate) fn http_version(&self) -> &str {
        &self.http_version
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    pub(crate) fn scheme(&self) -> &str {
        &self.scheme
    }

    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    pub(crate) fn client_ip(&self) -> &str {
        &self.client_ip
    }

    pub(crate) fn headers(&self) -> &HeaderMap {
        &self.headers
    }
}

type BufferedByteSourceFuture<'a> =
    Pin<Box<dyn Future<Output = Result<BufferedByteStreamPull, VmError>> + Send + 'a>>;

trait BufferedByteSource {
    fn pull_next<'a>(&'a mut self) -> BufferedByteSourceFuture<'a>;
}

enum BufferedByteStreamPull {
    Chunk(Bytes),
    Skip,
    Eof,
}

#[derive(Default)]
struct BufferedByteStream {
    buffered: Vec<u8>,
    read_offset: usize,
    eof: bool,
}

impl std::fmt::Debug for BufferedByteStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferedByteStream")
            .field("buffered_len", &self.buffered.len())
            .field("read_offset", &self.read_offset)
            .field("eof", &self.eof)
            .finish()
    }
}

impl BufferedByteStream {
    fn apply_pull(&mut self, pull: BufferedByteStreamPull) {
        match pull {
            BufferedByteStreamPull::Chunk(chunk) => {
                if !chunk.is_empty() {
                    self.buffered.extend_from_slice(&chunk);
                }
            }
            BufferedByteStreamPull::Skip => {}
            BufferedByteStreamPull::Eof => {
                self.eof = true;
            }
        }
    }

    async fn ensure_readable_byte<S: BufferedByteSource>(
        &mut self,
        source: &mut S,
    ) -> Result<(), VmError> {
        while self.read_offset >= self.buffered.len() && !self.eof {
            self.apply_pull(source.pull_next().await?);
        }
        Ok(())
    }

    async fn read_next_chunk<S: BufferedByteSource>(
        &mut self,
        source: &mut S,
        max_bytes: usize,
    ) -> Result<Vec<u8>, VmError> {
        self.ensure_readable_byte(source).await?;
        if self.read_offset >= self.buffered.len() {
            return Ok(Vec::new());
        }
        let end = self
            .read_offset
            .saturating_add(max_bytes)
            .min(self.buffered.len());
        let chunk = self.buffered[self.read_offset..end].to_vec();
        self.read_offset = end;
        Ok(chunk)
    }

    async fn read_next_line<S: BufferedByteSource>(
        &mut self,
        source: &mut S,
    ) -> Result<Vec<u8>, VmError> {
        loop {
            let start = self.read_offset.min(self.buffered.len());
            if start < self.buffered.len() {
                if let Some(rel_end) = self.buffered[start..]
                    .iter()
                    .position(|byte| *byte == b'\n')
                {
                    let end = start + rel_end;
                    let line = self.buffered[start..end].to_vec();
                    self.read_offset = end.saturating_add(1);
                    return Ok(line);
                }
                if self.eof {
                    let line = self.buffered[start..].to_vec();
                    self.read_offset = self.buffered.len();
                    return Ok(line);
                }
            } else if self.eof {
                return Ok(Vec::new());
            }

            self.apply_pull(source.pull_next().await?);
        }
    }

    async fn read_all<S: BufferedByteSource>(
        &mut self,
        source: &mut S,
    ) -> Result<Vec<u8>, VmError> {
        while !self.eof {
            self.apply_pull(source.pull_next().await?);
        }
        Ok(self.buffered.clone())
    }

    async fn read_all_and_consume<S: BufferedByteSource>(
        &mut self,
        source: &mut S,
    ) -> Result<Vec<u8>, VmError> {
        let body = self.read_all(source).await?;
        self.read_offset = self.buffered.len();
        Ok(body)
    }

    async fn eof<S: BufferedByteSource>(&mut self, source: &mut S) -> Result<bool, VmError> {
        while self.read_offset >= self.buffered.len() && !self.eof {
            self.apply_pull(source.pull_next().await?);
        }
        Ok(self.eof && self.read_offset >= self.buffered.len())
    }
}

struct InboundRequestBodySource {
    body: Option<Body>,
}

impl BufferedByteSource for InboundRequestBodySource {
    fn pull_next<'a>(&'a mut self) -> BufferedByteSourceFuture<'a> {
        Box::pin(async move {
            let Some(body) = self.body.as_mut() else {
                return Ok(BufferedByteStreamPull::Eof);
            };

            match body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(chunk) = frame.into_data() {
                        Ok(BufferedByteStreamPull::Chunk(chunk))
                    } else {
                        Ok(BufferedByteStreamPull::Skip)
                    }
                }
                Some(Err(err)) => Err(VmError::HostError(format!(
                    "failed to read inbound request body frame: {err}",
                ))),
                None => {
                    self.body = None;
                    Ok(BufferedByteStreamPull::Eof)
                }
            }
        })
    }
}

pub(crate) struct InboundRequestBodyState {
    source: InboundRequestBodySource,
    stream: BufferedByteStream,
}

impl std::fmt::Debug for InboundRequestBodyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboundRequestBodyState")
            .field("stream", &self.stream)
            .finish()
    }
}

impl InboundRequestBodyState {
    fn new(body: Body) -> Self {
        Self {
            source: InboundRequestBodySource { body: Some(body) },
            stream: BufferedByteStream::default(),
        }
    }

    async fn read_next_chunk(&mut self, max_bytes: usize) -> Result<Vec<u8>, VmError> {
        self.stream
            .read_next_chunk(&mut self.source, max_bytes)
            .await
    }

    async fn read_next_line(&mut self) -> Result<Vec<u8>, VmError> {
        self.stream.read_next_line(&mut self.source).await
    }

    async fn read_all_and_consume(&mut self) -> Result<Vec<u8>, VmError> {
        self.stream.read_all_and_consume(&mut self.source).await
    }

    async fn read_all(&mut self) -> Result<Vec<u8>, VmError> {
        self.stream.read_all(&mut self.source).await
    }

    async fn eof(&mut self) -> Result<bool, VmError> {
        self.stream.eof(&mut self.source).await
    }

    fn is_drained(&self) -> bool {
        self.stream.eof && self.stream.read_offset >= self.stream.buffered.len()
    }
}

type SharedInboundRequestBody = Arc<tokio::sync::Mutex<InboundRequestBodyState>>;
#[derive(Clone)]
pub(crate) struct DownstreamHttp1Upgrade {
    inner: Arc<tokio::sync::Mutex<Option<OnUpgrade>>>,
}

impl std::fmt::Debug for DownstreamHttp1Upgrade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DownstreamHttp1Upgrade")
    }
}

impl DownstreamHttp1Upgrade {
    fn new(upgrade: OnUpgrade) -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(Some(upgrade))),
        }
    }

    async fn take(&self) -> Result<OnUpgrade, VmError> {
        let mut guard = self.inner.lock().await;
        guard.take().ok_or_else(|| {
            VmError::HostError("downstream http/1 upgrade has already been consumed".to_string())
        })
    }
}

#[derive(Debug)]
pub(crate) enum DownstreamConnectTunnelTarget {
    Tcp {
        handle: i64,
        stream: tokio::net::TcpStream,
    },
    #[cfg(feature = "tls")]
    Tls {
        handle: i64,
        stream: Box<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>,
    },
}

pub(crate) struct InlineDownstreamHttpResponse {
    pub(crate) response: Response<Body>,
    pub(crate) post_response_plan: Option<DownstreamPostResponsePlan>,
}

impl std::fmt::Debug for InlineDownstreamHttpResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InlineDownstreamHttpResponse")
            .field("response_status", &self.response.status())
            .field("has_post_response_plan", &self.post_response_plan.is_some())
            .finish()
    }
}

struct InlineDownstreamHttpResponseSender(oneshot::Sender<InlineDownstreamHttpResponse>);

impl std::fmt::Debug for InlineDownstreamHttpResponseSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InlineDownstreamHttpResponseSender")
    }
}

#[derive(Debug)]
pub(crate) struct DownstreamConnectTunnelPlan {
    context: Arc<ProxyVmContext>,
    upgrade: DownstreamHttp1Upgrade,
    target: DownstreamConnectTunnelTarget,
}

impl DownstreamConnectTunnelPlan {
    pub(crate) fn new(
        context: Arc<ProxyVmContext>,
        upgrade: DownstreamHttp1Upgrade,
        target: DownstreamConnectTunnelTarget,
    ) -> Self {
        Self {
            context,
            upgrade,
            target,
        }
    }

    fn mark_closed(context: &Arc<ProxyVmContext>, handle: i64, tls_attached: bool) {
        let mut transport = context.lock_transport();
        transport.tcp_dag.downstream.mark_closed();
        transport.tls_dag.downstream.mark_closed();
        if let Some(state) = transport.tcp_streams.get_mut(&handle) {
            state.mark_closed();
        }
        if tls_attached && let Some(flow) = transport.dynamic_tls_sessions.get_mut(&handle) {
            flow.mark_closed();
        }
    }

    fn mark_failed(context: &Arc<ProxyVmContext>, handle: i64, tls_attached: bool, message: &str) {
        let mut transport = context.lock_transport();
        transport
            .tcp_dag
            .downstream
            .mark_failed(message.to_string());
        transport.tls_dag.downstream.mark_failed();
        if let Some(state) = transport.tcp_streams.get_mut(&handle) {
            state.mark_failed(message.to_string());
        }
        if tls_attached && let Some(flow) = transport.dynamic_tls_sessions.get_mut(&handle) {
            flow.mark_failed();
        }
    }

    pub(crate) async fn run(self) -> Result<(), VmError> {
        let Self {
            context,
            upgrade,
            target,
        } = self;
        let upgraded = upgrade.take().await?;
        let upgraded = upgraded.await.map_err(|err| {
            VmError::HostError(format!("downstream connect upgrade failed: {err}"))
        })?;
        let mut downstream = TokioIo::new(upgraded);

        match target {
            DownstreamConnectTunnelTarget::Tcp { handle, mut stream } => {
                match copy_bidirectional(&mut downstream, &mut stream).await {
                    Ok(_) => {
                        Self::mark_closed(&context, handle, false);
                        Ok(())
                    }
                    Err(err) => {
                        let message = format!("proxy connect tunnel failed: {err}");
                        Self::mark_failed(&context, handle, false, &message);
                        Err(VmError::HostError(message))
                    }
                }
            }
            #[cfg(feature = "tls")]
            DownstreamConnectTunnelTarget::Tls { handle, stream } => {
                let mut stream = *stream;
                match copy_bidirectional(&mut downstream, &mut stream).await {
                    Ok(_) => {
                        Self::mark_closed(&context, handle, true);
                        Ok(())
                    }
                    Err(err) => {
                        let message = format!("proxy connect tunnel failed: {err}");
                        Self::mark_failed(&context, handle, true, &message);
                        Err(VmError::HostError(message))
                    }
                }
            }
        }
    }
}

#[cfg(feature = "websocket")]
#[derive(Debug)]
pub(crate) struct DownstreamWebSocketTunnelPlan {
    context: Arc<ProxyVmContext>,
    upgrade: DownstreamHttp1Upgrade,
    connection: i64,
    selected_subprotocol: Option<String>,
}

#[cfg(feature = "websocket")]
impl DownstreamWebSocketTunnelPlan {
    pub(crate) fn new(
        context: Arc<ProxyVmContext>,
        upgrade: DownstreamHttp1Upgrade,
        connection: i64,
        selected_subprotocol: Option<String>,
    ) -> Self {
        Self {
            context,
            upgrade,
            connection,
            selected_subprotocol,
        }
    }

    fn mark_closed(
        context: &Arc<ProxyVmContext>,
        close_code: Option<u16>,
        close_reason: Option<String>,
    ) {
        let mut transport = context.lock_transport();
        transport.tcp_dag.downstream.mark_closed();
        transport.tls_dag.downstream.mark_closed();
        drop(transport);
        context.with_downstream_websocket_mut(|websocket| {
            websocket.mark_closed(close_code, close_reason);
        });
    }

    fn mark_failed(context: &Arc<ProxyVmContext>, message: &str) {
        let mut transport = context.lock_transport();
        transport
            .tcp_dag
            .downstream
            .mark_failed(message.to_string());
        transport.tls_dag.downstream.mark_failed();
        drop(transport);
        context.with_downstream_websocket_mut(|websocket| websocket.mark_failed(message));
    }

    pub(crate) async fn run(self) -> Result<(), VmError> {
        let Self {
            context,
            upgrade,
            connection,
            selected_subprotocol: _,
        } = self;
        let upgraded = upgrade.take().await?;
        let upgraded = upgraded.await.map_err(|err| {
            let message = format!("downstream websocket upgrade failed: {err}");
            Self::mark_failed(&context, &message);
            VmError::HostError(message)
        })?;
        let websocket =
            WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None).await;
        let (mut downstream_write, mut downstream_read) = websocket.split();

        loop {
            tokio::select! {
                downstream_frame = downstream_read.next() => {
                    match downstream_frame {
                        Some(Ok(Message::Binary(bytes))) => {
                            write_websocket_binary_bytes(&context, connection, &bytes).await?;
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            downstream_write.send(Message::Pong(payload)).await.map_err(|err| {
                                let message = format!("failed to reply to downstream websocket ping: {err}");
                                Self::mark_failed(&context, &message);
                                VmError::HostError(message)
                            })?;
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(Message::Close(frame))) => {
                            close_websocket_binary_stream(&context, connection).await?;
                            let close_code = frame.as_ref().map(|frame| u16::from(frame.code));
                            let close_reason = frame.as_ref().map(|frame| frame.reason.to_string());
                            let _ = downstream_write.send(Message::Close(frame)).await;
                            Self::mark_closed(&context, close_code, close_reason);
                            return Ok(());
                        }
                        Some(Ok(Message::Text(_))) => {
                            let message = "downstream websocket proxy tunnel only supports binary frames".to_string();
                            let _ = downstream_write.send(Message::Close(Some(CloseFrame {
                                code: CloseCode::Unsupported,
                                reason: "binary-only".into(),
                            }))).await;
                            Self::mark_failed(&context, &message);
                            return Err(VmError::HostError(message));
                        }
                        Some(Ok(_)) => {}
                        Some(Err(err)) => {
                            let message = format!("failed to read downstream websocket frame: {err}");
                            Self::mark_failed(&context, &message);
                            return Err(VmError::HostError(message));
                        }
                        None => {
                            close_websocket_binary_stream(&context, connection).await?;
                            Self::mark_closed(&context, Some(1000), Some("downstream-closed".to_string()));
                            return Ok(());
                        }
                    }
                }
                upstream_frame = read_websocket_binary_bytes(&context, connection) => {
                    match upstream_frame? {
                        Some(bytes) => {
                            downstream_write.send(Message::Binary(bytes.into())).await.map_err(|err| {
                                let message = format!("failed to write downstream websocket frame: {err}");
                                Self::mark_failed(&context, &message);
                                VmError::HostError(message)
                            })?;
                        }
                        None => {
                            let _ = downstream_write.send(Message::Close(Some(CloseFrame {
                                code: CloseCode::Normal,
                                reason: "upstream-closed".into(),
                            }))).await;
                            Self::mark_closed(&context, Some(1000), Some("upstream-closed".to_string()));
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum DownstreamPostResponsePlan {
    ConnectTunnel(Box<DownstreamConnectTunnelPlan>),
    #[cfg(feature = "websocket")]
    WebSocketTunnel(DownstreamWebSocketTunnelPlan),
}

impl DownstreamPostResponsePlan {
    pub(crate) async fn run(self) -> Result<(), VmError> {
        match self {
            Self::ConnectTunnel(plan) => plan.run().await,
            #[cfg(feature = "websocket")]
            Self::WebSocketTunnel(plan) => plan.run().await,
        }
    }
}

pub(crate) type SharedUpstreamClientCache =
    Arc<Mutex<BoundedLruStore<UpstreamClientCacheKey, reqwest::Client>>>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct UpstreamClientCacheKey {
    tls_key: Option<TlsSessionCacheKey>,
    http2_mode: http2::Http2UpstreamMode,
}

pub(crate) fn new_shared_upstream_client_cache(capacity: usize) -> SharedUpstreamClientCache {
    Arc::new(Mutex::new(BoundedLruStore::new(capacity)))
}

#[derive(Clone, Debug)]
pub(crate) struct HttpOutboundRequestNode {
    pub(crate) method: Method,
    pub(crate) path: String,
    pub(crate) query: String,
    pub(crate) headers: HeaderMap,
    pub(crate) body_override: Option<Vec<u8>>,
    pub(crate) target: Option<String>,
    pub(crate) version_preference: HttpVersionPreference,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct HttpResponseOutputNode {
    pub(crate) headers: HeaderMap,
    pub(crate) body: Option<Vec<u8>>,
    pub(crate) status: Option<u16>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum HttpCarrierKind {
    #[default]
    Http1,
    Http2,
    Http3,
}

impl HttpCarrierKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Http1 => "http1",
            Self::Http2 => "http2",
            Self::Http3 => "http3",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HttpCarrierRef {
    DownstreamHttp1,
    DownstreamHttp2Stream(http2::Http2StreamRef),
    DownstreamHttp3Stream(http3::Http3StreamRef),
    Http1DefaultUpstream,
    Http1DynamicExchange(i64),
    UpstreamHttp2Stream(http2::Http2StreamRef),
    UpstreamHttp3Stream(http3::Http3StreamRef),
}

impl HttpCarrierRef {
    fn kind(&self) -> HttpCarrierKind {
        match self {
            Self::DownstreamHttp1 | Self::Http1DefaultUpstream | Self::Http1DynamicExchange(_) => {
                HttpCarrierKind::Http1
            }
            Self::DownstreamHttp2Stream(_) | Self::UpstreamHttp2Stream(_) => HttpCarrierKind::Http2,
            Self::DownstreamHttp3Stream(_) | Self::UpstreamHttp3Stream(_) => HttpCarrierKind::Http3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttachedHttpTransport {
    Tcp(i64),
    #[cfg(feature = "tls")]
    Tls(i64),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct HttpExchangeTransportState {
    pub(crate) tcp_flow: TcpFlowState,
    pub(crate) tls_flow: TlsFlowState,
    pub(crate) carrier_kind: HttpCarrierKind,
    pub(crate) carrier_ref: Option<HttpCarrierRef>,
    pub(crate) http_version: Option<String>,
    pub(crate) peer_addr: Option<String>,
    pub(crate) attached_transport: Option<AttachedHttpTransport>,
}

impl HttpExchangeTransportState {
    fn note_write(&mut self) {
        self.tcp_flow.note_write();
    }

    fn mark_response_ready(&mut self, version: Version, carrier_ref: HttpCarrierRef) {
        self.carrier_kind = carrier_ref.kind();
        self.carrier_ref = Some(carrier_ref);
        self.http_version = Some(http_version_label(version).to_string());
    }

    fn set_peer_addr(&mut self, peer_addr: Option<String>) {
        self.peer_addr = peer_addr;
    }
}

#[cfg_attr(not(feature = "http2"), allow(dead_code))]
enum UpstreamResponseSource {
    Reqwest(reqwest::Response),
    #[cfg_attr(not(feature = "http2"), allow(dead_code))]
    Hyper(hyper::body::Incoming),
    #[cfg(feature = "http3")]
    Http3(Box<h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>>),
    Exhausted,
}

struct UpstreamResponseBodySource {
    source: UpstreamResponseSource,
    http2_tracker: Option<http2::Http2ResponseBodyTracker>,
    http3_tracker: Option<http3::Http3ResponseBodyTracker>,
    remaining_body_bytes: Option<u64>,
    body_started: bool,
    body_finished: bool,
}

impl Default for UpstreamResponseBodySource {
    fn default() -> Self {
        Self {
            source: UpstreamResponseSource::Exhausted,
            http2_tracker: None,
            http3_tracker: None,
            remaining_body_bytes: None,
            body_started: false,
            body_finished: false,
        }
    }
}

impl UpstreamResponseBodySource {
    fn note_body_ready(&mut self) {
        if !self.body_started {
            if let Some(tracker) = &self.http2_tracker {
                tracker.note_response_body_ready();
            }
            if let Some(tracker) = &self.http3_tracker {
                tracker.note_response_body_ready();
            }
            self.body_started = true;
        }
    }

    fn note_body_complete(&mut self) {
        self.note_body_ready();
        if !self.body_finished {
            if let Some(tracker) = &self.http2_tracker {
                tracker.note_body_eof();
            }
            if let Some(tracker) = &self.http3_tracker {
                tracker.note_body_eof();
            }
            self.body_finished = true;
        }
    }

    fn note_chunk_delivered(&mut self, chunk_len: usize) {
        if chunk_len == 0 {
            return;
        }
        self.note_body_ready();
        if let Some(remaining) = self.remaining_body_bytes.as_mut() {
            let consumed = u64::try_from(chunk_len).unwrap_or(u64::MAX);
            *remaining = remaining.saturating_sub(consumed);
            if *remaining == 0 {
                self.note_body_complete();
            }
        }
    }
}

impl BufferedByteSource for UpstreamResponseBodySource {
    fn pull_next<'a>(&'a mut self) -> BufferedByteSourceFuture<'a> {
        Box::pin(async move {
            match &mut self.source {
                UpstreamResponseSource::Reqwest(response) => match response.chunk().await {
                    Ok(Some(chunk)) => {
                        self.note_chunk_delivered(chunk.len());
                        Ok(BufferedByteStreamPull::Chunk(chunk))
                    }
                    Ok(None) => {
                        self.note_body_complete();
                        self.source = UpstreamResponseSource::Exhausted;
                        Ok(BufferedByteStreamPull::Eof)
                    }
                    Err(err) => Err(VmError::HostError(format!(
                        "failed to read upstream response chunk: {err}",
                    ))),
                },
                UpstreamResponseSource::Hyper(body) => match body.frame().await {
                    Some(Ok(frame)) => {
                        if let Ok(chunk) = frame.into_data() {
                            self.note_chunk_delivered(chunk.len());
                            Ok(BufferedByteStreamPull::Chunk(chunk))
                        } else {
                            Ok(BufferedByteStreamPull::Skip)
                        }
                    }
                    Some(Err(err)) => {
                        let observed = http2::classify_http2_error(&err);
                        if let Some(tracker) = &self.http2_tracker {
                            tracker.note_body_error(&observed);
                        }
                        Err(VmError::HostError(format!(
                            "failed to read upstream response frame: {}",
                            observed.message,
                        )))
                    }
                    None => {
                        self.note_body_complete();
                        self.source = UpstreamResponseSource::Exhausted;
                        Ok(BufferedByteStreamPull::Eof)
                    }
                },
                #[cfg(feature = "http3")]
                UpstreamResponseSource::Http3(request_stream) => {
                    match request_stream.as_mut().recv_data().await {
                        Ok(Some(mut chunk)) => {
                            let bytes = chunk.copy_to_bytes(chunk.remaining());
                            self.note_chunk_delivered(bytes.len());
                            Ok(BufferedByteStreamPull::Chunk(bytes))
                        }
                        Ok(None) => {
                            self.note_body_complete();
                            self.source = UpstreamResponseSource::Exhausted;
                            Ok(BufferedByteStreamPull::Eof)
                        }
                        Err(err) => {
                            let observed = http3::classify_http3_error(&err);
                            if let Some(tracker) = &self.http3_tracker {
                                tracker.note_body_error(&observed);
                            }
                            Err(VmError::HostError(format!(
                                "failed to read upstream http3 response frame: {}",
                                observed.message,
                            )))
                        }
                    }
                }
                UpstreamResponseSource::Exhausted => Ok(BufferedByteStreamPull::Eof),
            }
        })
    }
}

struct UpstreamResponseBodyState {
    source: UpstreamResponseBodySource,
    stream: BufferedByteStream,
}

struct StreamingUpstreamResponseBodyState {
    prefix: Option<Bytes>,
    source: UpstreamResponseBodySource,
}

impl StreamingUpstreamResponseBodyState {
    async fn next_chunk(&mut self) -> Result<Option<Bytes>, VmError> {
        if let Some(prefix) = self.prefix.take()
            && !prefix.is_empty()
        {
            return Ok(Some(prefix));
        }

        loop {
            match self.source.pull_next().await? {
                BufferedByteStreamPull::Chunk(chunk) => {
                    if !chunk.is_empty() {
                        return Ok(Some(chunk));
                    }
                }
                BufferedByteStreamPull::Skip => {}
                BufferedByteStreamPull::Eof => return Ok(None),
            }
        }
    }
}

impl std::fmt::Debug for UpstreamResponseBodyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamResponseBodyState")
            .field("stream", &self.stream)
            .finish()
    }
}

fn upstream_response_body_source(
    source: UpstreamResponseSource,
    http2_tracker: Option<http2::Http2ResponseBodyTracker>,
    http3_tracker: Option<http3::Http3ResponseBodyTracker>,
    content_length: Option<u64>,
) -> UpstreamResponseBodySource {
    let mut source = UpstreamResponseBodySource {
        source,
        http2_tracker,
        http3_tracker,
        remaining_body_bytes: content_length,
        body_started: false,
        body_finished: false,
    };
    if matches!(content_length, Some(0)) {
        source.note_body_complete();
    }
    source
}

impl UpstreamResponseBodyState {
    fn from_reqwest(response: reqwest::Response) -> Self {
        let content_length = response.content_length();
        Self {
            source: upstream_response_body_source(
                UpstreamResponseSource::Reqwest(response),
                None,
                None,
                content_length,
            ),
            stream: BufferedByteStream::default(),
        }
    }

    #[cfg_attr(not(feature = "http2"), allow(dead_code))]
    fn from_hyper(
        body: hyper::body::Incoming,
        http2_tracker: Option<http2::Http2ResponseBodyTracker>,
        content_length: Option<u64>,
    ) -> Self {
        Self {
            source: upstream_response_body_source(
                UpstreamResponseSource::Hyper(body),
                http2_tracker,
                None,
                content_length,
            ),
            stream: BufferedByteStream::default(),
        }
    }

    #[cfg(feature = "http3")]
    fn from_http3(
        request_stream: h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
        http3_tracker: Option<http3::Http3ResponseBodyTracker>,
        content_length: Option<u64>,
    ) -> Self {
        Self {
            source: upstream_response_body_source(
                UpstreamResponseSource::Http3(Box::new(request_stream)),
                None,
                http3_tracker,
                content_length,
            ),
            stream: BufferedByteStream::default(),
        }
    }

    async fn read_next_chunk(&mut self, max_bytes: usize) -> Result<Vec<u8>, VmError> {
        self.stream
            .read_next_chunk(&mut self.source, max_bytes)
            .await
    }

    async fn read_next_line(&mut self) -> Result<Vec<u8>, VmError> {
        self.stream.read_next_line(&mut self.source).await
    }

    async fn read_all(&mut self) -> Result<Vec<u8>, VmError> {
        self.stream.read_all(&mut self.source).await
    }

    async fn eof(&mut self) -> Result<bool, VmError> {
        self.stream.eof(&mut self.source).await
    }

    fn take_streaming_passthrough(&mut self) -> StreamingUpstreamResponseBodyState {
        let stream = std::mem::take(&mut self.stream);
        StreamingUpstreamResponseBodyState {
            prefix: if stream.buffered.is_empty() {
                None
            } else {
                Some(Bytes::from(stream.buffered))
            },
            source: std::mem::take(&mut self.source),
        }
    }
}

type SharedUpstreamResponseBody = Arc<tokio::sync::Mutex<UpstreamResponseBodyState>>;

pub(crate) const DEFAULT_UPSTREAM_EXCHANGE_HANDLE: i64 = 1;
const FIRST_DYNAMIC_EXCHANGE_HANDLE: i64 = 2;
pub(crate) const DEFAULT_UPSTREAM_UDP_SOCKET_HANDLE: i64 = 1;
const FIRST_DYNAMIC_UDP_SOCKET_HANDLE: i64 = 2;
#[cfg(feature = "webrtc")]
pub(crate) const DEFAULT_UPSTREAM_WEBRTC_CONNECTION_HANDLE: i64 = 1;
#[cfg(feature = "webrtc")]
const FIRST_DYNAMIC_WEBRTC_CONNECTION_HANDLE: i64 = 2;
const FIRST_PROXY_STREAM_HANDLE: i64 = 4096;

#[derive(Clone)]
pub(crate) struct HttpUpstreamResponseSnapshot {
    pub(crate) status: u16,
    pub(crate) headers: HeaderMap,
    pub(crate) http_version: String,
    pub(crate) carrier_kind: HttpCarrierKind,
    pub(crate) carrier_ref: HttpCarrierRef,
    body: SharedUpstreamResponseBody,
}

impl std::fmt::Debug for HttpUpstreamResponseSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpUpstreamResponseSnapshot")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("http_version", &self.http_version)
            .field("carrier_kind", &self.carrier_kind.as_str())
            .field("carrier_ref", &self.carrier_ref)
            .finish()
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) enum HttpUpstreamResponseNode {
    #[default]
    NotStarted,
    Ready(HttpUpstreamResponseSnapshot),
}

#[derive(Clone, Debug)]
struct StoredUpstreamResponse {
    snapshot: HttpUpstreamResponseSnapshot,
    latency_ms: u64,
}

impl StoredUpstreamResponse {
    fn new(snapshot: HttpUpstreamResponseSnapshot, latency_ms: u64) -> Self {
        Self {
            snapshot,
            latency_ms,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HttpOutboundExchangeState {
    pub(crate) request: HttpOutboundRequestNode,
    pub(crate) response: HttpUpstreamResponseNode,
    pub(crate) transport: HttpExchangeTransportState,
    pub(crate) websocket_dag: WebSocketConnectionState,
    pub(crate) upstream_latency_ms: u64,
}

impl HttpOutboundExchangeState {
    fn new() -> Self {
        Self {
            request: HttpOutboundRequestNode {
                method: Method::GET,
                path: "/".to_string(),
                query: String::new(),
                headers: HeaderMap::new(),
                body_override: None,
                target: None,
                version_preference: HttpVersionPreference::Auto,
            },
            response: HttpUpstreamResponseNode::NotStarted,
            transport: HttpExchangeTransportState::default(),
            websocket_dag: WebSocketConnectionState::default(),
            upstream_latency_ms: 0,
        }
    }

    fn default_upstream(request_head: &HttpRequestHead) -> Self {
        Self {
            request: HttpOutboundRequestNode {
                method: request_head.method.clone(),
                path: request_head.path.clone(),
                query: request_head.query.clone(),
                headers: request_head.headers.clone(),
                body_override: None,
                target: None,
                version_preference: HttpVersionPreference::Auto,
            },
            ..Self::new()
        }
    }

    fn response_snapshot(&self) -> Result<HttpUpstreamResponseSnapshot, VmError> {
        match &self.response {
            HttpUpstreamResponseNode::Ready(snapshot) => Ok(snapshot.clone()),
            HttpUpstreamResponseNode::NotStarted => Err(VmError::HostError(
                "outbound exchange response is unavailable before the exchange starts".to_string(),
            )),
        }
    }

    fn response_ready(&self) -> bool {
        matches!(self.response, HttpUpstreamResponseNode::Ready(_))
    }

    fn store_response(&mut self, response: StoredUpstreamResponse) {
        self.response = HttpUpstreamResponseNode::Ready(response.snapshot);
        self.upstream_latency_ms = response.latency_ms;
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeServices {
    upstream_client: Option<reqwest::Client>,
    upstream_client_cache: Option<SharedUpstreamClientCache>,
    tls_session_cache: Option<SharedTlsSessionCache>,
    upstream_http_sessions: Option<SharedHttpUpstreamSessions>,
    upstream_http3_sessions: Option<SharedHttp3UpstreamSessions>,
    downstream_http_sessions: Option<http2::SharedHttpDownstreamSessions>,
    downstream_http3_sessions: Option<http3::SharedHttp3DownstreamSessions>,
    #[cfg(feature = "tls")]
    downstream_tls_termination: Option<Arc<tokio_rustls::rustls::ServerConfig>>,
    rate_limiter: SharedRateLimiter,
}

impl RuntimeServices {
    fn new(rate_limiter: SharedRateLimiter) -> Self {
        Self {
            upstream_client: None,
            upstream_client_cache: None,
            tls_session_cache: None,
            upstream_http_sessions: None,
            upstream_http3_sessions: None,
            downstream_http_sessions: None,
            downstream_http3_sessions: None,
            #[cfg(feature = "tls")]
            downstream_tls_termination: None,
            rate_limiter,
        }
    }

    pub(crate) fn upstream_client(&self) -> Option<reqwest::Client> {
        self.upstream_client.clone()
    }

    pub(crate) fn upstream_client_cache(&self) -> Option<SharedUpstreamClientCache> {
        self.upstream_client_cache.clone()
    }

    pub(crate) fn tls_session_cache(&self) -> Option<SharedTlsSessionCache> {
        self.tls_session_cache.clone()
    }

    pub(crate) fn upstream_http_sessions(&self) -> Option<SharedHttpUpstreamSessions> {
        self.upstream_http_sessions.clone()
    }

    pub(crate) fn upstream_http3_sessions(&self) -> Option<SharedHttp3UpstreamSessions> {
        self.upstream_http3_sessions.clone()
    }

    pub(crate) fn downstream_http_sessions(&self) -> Option<http2::SharedHttpDownstreamSessions> {
        self.downstream_http_sessions.clone()
    }

    #[cfg(feature = "tls")]
    pub(crate) fn downstream_tls_termination(
        &self,
    ) -> Option<Arc<tokio_rustls::rustls::ServerConfig>> {
        self.downstream_tls_termination.clone()
    }

    pub(crate) fn rate_limiter(&self) -> SharedRateLimiter {
        self.rate_limiter.clone()
    }
}

#[derive(Debug)]
pub(crate) struct DownstreamState {
    pub(crate) inbound_request_body: SharedInboundRequestBody,
    #[cfg_attr(not(feature = "websocket"), allow(dead_code))]
    pub(crate) downstream_websocket: WebSocketConnectionState,
    pub(crate) response_output: HttpResponseOutputNode,
    pub(crate) downstream_carrier_ref: Option<HttpCarrierRef>,
    pub(crate) downstream_http1_upgrade: Option<DownstreamHttp1Upgrade>,
    pub(crate) post_response_plan: Option<DownstreamPostResponsePlan>,
    inline_http_response_sender: Option<InlineDownstreamHttpResponseSender>,
}

impl DownstreamState {
    fn from_http_request(request_head: &HttpRequestHead, body: Body) -> Self {
        Self {
            inbound_request_body: Arc::new(tokio::sync::Mutex::new(InboundRequestBodyState::new(
                body,
            ))),
            downstream_websocket: WebSocketConnectionState::for_http_request(&request_head.headers),
            response_output: HttpResponseOutputNode::default(),
            downstream_carrier_ref: Some(HttpCarrierRef::DownstreamHttp1),
            downstream_http1_upgrade: None,
            post_response_plan: None,
            inline_http_response_sender: None,
        }
    }

    fn attach_downstream_http2_stream(
        &mut self,
        attachment: &http2::Http2DownstreamStreamAttachment,
    ) {
        self.downstream_carrier_ref = Some(HttpCarrierRef::DownstreamHttp2Stream(
            http2::Http2StreamRef {
                session_id: attachment.session_id,
                stream_id: attachment.stream_id,
            },
        ));
    }

    fn attach_downstream_http3_stream(
        &mut self,
        attachment: &http3::Http3DownstreamStreamAttachment,
    ) {
        self.downstream_carrier_ref = Some(HttpCarrierRef::DownstreamHttp3Stream(
            http3::Http3StreamRef {
                session_id: attachment.session_id,
                stream_id: attachment.stream_id,
            },
        ));
    }

    fn for_transport_connection() -> Self {
        Self {
            inbound_request_body: Arc::new(tokio::sync::Mutex::new(InboundRequestBodyState::new(
                Body::empty(),
            ))),
            downstream_websocket: WebSocketConnectionState::default(),
            response_output: HttpResponseOutputNode::default(),
            downstream_carrier_ref: None,
            downstream_http1_upgrade: None,
            post_response_plan: None,
            inline_http_response_sender: None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ExchangeRegistry {
    pub(crate) next_outbound_exchange_handle: i64,
    pub(crate) exchanges: HashMap<i64, HttpOutboundExchangeState>,
}

impl ExchangeRegistry {
    fn from_http_request(request_head: &HttpRequestHead) -> Self {
        let mut exchanges = HashMap::new();
        exchanges.insert(
            DEFAULT_UPSTREAM_EXCHANGE_HANDLE,
            HttpOutboundExchangeState::default_upstream(request_head),
        );
        Self {
            next_outbound_exchange_handle: FIRST_DYNAMIC_EXCHANGE_HANDLE,
            exchanges,
        }
    }
}

#[derive(Debug)]
pub(crate) struct TransportState {
    pub(crate) tcp_dag: TcpTransportDag,
    pub(crate) tls_dag: TlsTransportDag,
    pub(crate) downstream_listener_goal: DownstreamHttpListenerGoal,
    pub(crate) downstream_transport_accessed: bool,
    pub(crate) downstream_tcp_io: Option<SharedTcpStreamIo>,
    pub(crate) downstream_preread_buffer: Vec<u8>,
    pub(crate) downstream_read_eof: bool,
    pub(crate) downstream_local_addr: Option<SocketAddr>,
    pub(crate) downstream_peer_addr: Option<SocketAddr>,
    #[cfg(feature = "tls")]
    pub(crate) downstream_tls_server_start: Option<DownstreamTlsServerStart>,
    #[cfg(feature = "tls")]
    pub(crate) downstream_tls_io: Option<SharedServerTlsStreamIo>,
    pub(crate) next_tcp_stream_handle: i64,
    pub(crate) tcp_streams: HashMap<i64, TcpSocketState>,
    pub(crate) tcp_stream_ios: HashMap<i64, SharedTcpStreamIo>,
    #[cfg(feature = "tls")]
    pub(crate) dynamic_tls_sessions: HashMap<i64, TlsFlowState>,
    #[cfg(feature = "tls")]
    pub(crate) dynamic_tls_session_ios: HashMap<i64, SharedTlsStreamIo>,
    pub(crate) default_upstream_udp_socket: UdpSocketState,
    pub(crate) default_upstream_udp_io: Option<SharedUdpSocketIo>,
    pub(crate) next_udp_socket_handle: i64,
    pub(crate) udp_sockets: HashMap<i64, UdpSocketState>,
    pub(crate) udp_socket_ios: HashMap<i64, SharedUdpSocketIo>,
}

impl TransportState {
    fn from_http_request(request_head: &HttpRequestHead) -> Self {
        Self {
            tcp_dag: TcpTransportDag::for_http_request(),
            tls_dag: TlsTransportDag::for_http_request(
                request_head.scheme.as_str(),
                request_head.host.as_str(),
                request_head.http_version.as_str(),
            ),
            downstream_listener_goal: DownstreamHttpListenerGoal::None,
            downstream_transport_accessed: false,
            downstream_tcp_io: None,
            downstream_preread_buffer: Vec::new(),
            downstream_read_eof: false,
            downstream_local_addr: None,
            downstream_peer_addr: None,
            #[cfg(feature = "tls")]
            downstream_tls_server_start: None,
            #[cfg(feature = "tls")]
            downstream_tls_io: None,
            next_tcp_stream_handle: FIRST_DYNAMIC_TCP_STREAM_HANDLE,
            tcp_streams: HashMap::new(),
            tcp_stream_ios: HashMap::new(),
            #[cfg(feature = "tls")]
            dynamic_tls_sessions: HashMap::new(),
            #[cfg(feature = "tls")]
            dynamic_tls_session_ios: HashMap::new(),
            default_upstream_udp_socket: UdpSocketState::default(),
            default_upstream_udp_io: None,
            next_udp_socket_handle: FIRST_DYNAMIC_UDP_SOCKET_HANDLE,
            udp_sockets: HashMap::new(),
            udp_socket_ios: HashMap::new(),
        }
    }

    fn from_downstream_tcp_stream(
        io: SharedTcpStreamIo,
        local_addr: SocketAddr,
        peer_addr: SocketAddr,
    ) -> Self {
        Self {
            tcp_dag: TcpTransportDag {
                downstream: TcpFlowState::downstream_ready(),
                default_upstream: TcpFlowState::default(),
            },
            tls_dag: TlsTransportDag::default(),
            downstream_listener_goal: DownstreamHttpListenerGoal::None,
            downstream_transport_accessed: false,
            downstream_tcp_io: Some(io),
            downstream_preread_buffer: Vec::new(),
            downstream_read_eof: false,
            downstream_local_addr: Some(local_addr),
            downstream_peer_addr: Some(peer_addr),
            #[cfg(feature = "tls")]
            downstream_tls_server_start: None,
            #[cfg(feature = "tls")]
            downstream_tls_io: None,
            next_tcp_stream_handle: FIRST_DYNAMIC_TCP_STREAM_HANDLE,
            tcp_streams: HashMap::new(),
            tcp_stream_ios: HashMap::new(),
            #[cfg(feature = "tls")]
            dynamic_tls_sessions: HashMap::new(),
            #[cfg(feature = "tls")]
            dynamic_tls_session_ios: HashMap::new(),
            default_upstream_udp_socket: UdpSocketState::default(),
            default_upstream_udp_io: None,
            next_udp_socket_handle: FIRST_DYNAMIC_UDP_SOCKET_HANDLE,
            udp_sockets: HashMap::new(),
            udp_socket_ios: HashMap::new(),
        }
    }
}

#[cfg(feature = "webrtc")]
#[derive(Debug)]
pub(crate) struct WebRtcRegistry {
    pub(crate) default_upstream_webrtc: WebRtcConnectionState,
    pub(crate) next_webrtc_connection_handle: i64,
    pub(crate) webrtc_connections: HashMap<i64, WebRtcConnectionState>,
}

#[cfg(feature = "webrtc")]
impl Default for WebRtcRegistry {
    fn default() -> Self {
        Self {
            default_upstream_webrtc: WebRtcConnectionState::default(),
            next_webrtc_connection_handle: FIRST_DYNAMIC_WEBRTC_CONNECTION_HANDLE,
            webrtc_connections: HashMap::new(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ProxyStreamRegistry {
    pub(crate) next_proxy_stream_handle: i64,
    pub(crate) proxy_stream_handles: HashMap<i64, ProxyByteStreamState>,
}

impl Default for ProxyStreamRegistry {
    fn default() -> Self {
        Self {
            next_proxy_stream_handle: FIRST_PROXY_STREAM_HANDLE,
            proxy_stream_handles: HashMap::new(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct EdgeIoRegistry {
    pub(crate) next_handle: i64,
    pub(crate) handles: HashMap<i64, EdgeVirtualIoHandle>,
}

impl Default for EdgeIoRegistry {
    fn default() -> Self {
        Self {
            next_handle: EDGE_IO_HANDLE_DYNAMIC_BASE,
            handles: HashMap::new(),
        }
    }
}

#[derive(Debug)]
pub struct ProxyVmContext {
    request_head: Mutex<HttpRequestHead>,
    services: RuntimeServices,
    downstream: Mutex<DownstreamState>,
    exchanges: Mutex<ExchangeRegistry>,
    transport: Mutex<TransportState>,
    #[cfg(feature = "webrtc")]
    webrtc: Mutex<WebRtcRegistry>,
    proxy: Mutex<ProxyStreamRegistry>,
    edge_io: Mutex<EdgeIoRegistry>,
}

impl ProxyVmContext {
    pub fn from_http_request(request: HttpRequestContext, rate_limiter: SharedRateLimiter) -> Self {
        let request_headers = request.headers;
        let request_head = HttpRequestHead {
            request_id: request.request_id,
            method: request.method,
            path: request.path,
            query: request.query,
            http_version: request.http_version,
            port: request.port,
            scheme: request.scheme,
            host: request.host,
            client_ip: request.client_ip,
            headers: request_headers,
        };
        Self {
            downstream: Mutex::new(DownstreamState::from_http_request(
                &request_head,
                request.body,
            )),
            exchanges: Mutex::new(ExchangeRegistry::from_http_request(&request_head)),
            transport: Mutex::new(TransportState::from_http_request(&request_head)),
            request_head: Mutex::new(request_head),
            services: RuntimeServices::new(rate_limiter),
            #[cfg(feature = "webrtc")]
            webrtc: Mutex::new(WebRtcRegistry::default()),
            proxy: Mutex::new(ProxyStreamRegistry::default()),
            edge_io: Mutex::new(EdgeIoRegistry::default()),
        }
    }

    pub fn from_request_headers(
        request_headers: HeaderMap,
        rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self::from_http_request(
            HttpRequestContext {
                request_id: String::new(),
                method: Method::GET,
                path: "/".to_string(),
                query: String::new(),
                http_version: "1.1".to_string(),
                port: 80,
                scheme: "http".to_string(),
                host: String::new(),
                client_ip: String::new(),
                body: Body::empty(),
                headers: request_headers,
            },
            rate_limiter,
        )
    }

    pub fn from_downstream_tcp_stream(
        stream: tokio::net::TcpStream,
        request_id: String,
        rate_limiter: SharedRateLimiter,
    ) -> Result<Self, VmError> {
        let local_addr = stream.local_addr().map_err(|err| {
            VmError::HostError(format!("failed to read downstream local address: {err}"))
        })?;
        let peer_addr = stream.peer_addr().map_err(|err| {
            VmError::HostError(format!("failed to read downstream peer address: {err}"))
        })?;
        let io = Arc::new(tokio::sync::Mutex::new(Some(stream)));
        let request_head = HttpRequestHead {
            request_id,
            method: Method::GET,
            path: "/".to_string(),
            query: String::new(),
            http_version: String::new(),
            port: peer_addr.port(),
            scheme: "tcp".to_string(),
            host: peer_addr.to_string(),
            client_ip: peer_addr.ip().to_string(),
            headers: HeaderMap::new(),
        };
        Ok(Self {
            downstream: Mutex::new(DownstreamState::for_transport_connection()),
            exchanges: Mutex::new(ExchangeRegistry::from_http_request(&request_head)),
            transport: Mutex::new(TransportState::from_downstream_tcp_stream(
                io, local_addr, peer_addr,
            )),
            request_head: Mutex::new(request_head),
            services: RuntimeServices::new(rate_limiter),
            #[cfg(feature = "webrtc")]
            webrtc: Mutex::new(WebRtcRegistry::default()),
            proxy: Mutex::new(ProxyStreamRegistry::default()),
            edge_io: Mutex::new(EdgeIoRegistry::default()),
        })
    }

    pub fn attach_upstream_client(&mut self, client: reqwest::Client) {
        self.services.upstream_client = Some(client);
    }

    pub(crate) fn attach_upstream_client_cache(&mut self, cache: SharedUpstreamClientCache) {
        self.services.upstream_client_cache = Some(cache);
    }

    pub(crate) fn attach_tls_session_cache(&mut self, cache: SharedTlsSessionCache) {
        self.services.tls_session_cache = Some(cache);
    }

    #[cfg(feature = "tls")]
    pub(crate) fn attach_downstream_tls_termination(
        &mut self,
        server_config: Arc<tokio_rustls::rustls::ServerConfig>,
    ) {
        self.services.downstream_tls_termination = Some(server_config);
    }

    pub(crate) fn attach_upstream_http_sessions(&mut self, sessions: SharedHttpUpstreamSessions) {
        self.services.upstream_http_sessions = Some(sessions);
    }

    pub(crate) fn attach_upstream_http3_sessions(&mut self, sessions: SharedHttp3UpstreamSessions) {
        self.services.upstream_http3_sessions = Some(sessions);
    }

    pub(crate) fn attach_downstream_http_sessions(
        &mut self,
        sessions: http2::SharedHttpDownstreamSessions,
    ) {
        self.services.downstream_http_sessions = Some(sessions);
    }

    pub(crate) fn attach_downstream_http3_sessions(
        &mut self,
        sessions: http3::SharedHttp3DownstreamSessions,
    ) {
        self.services.downstream_http3_sessions = Some(sessions);
    }

    pub(crate) fn attach_downstream_http2_stream(
        &mut self,
        attachment: &http2::Http2DownstreamStreamAttachment,
    ) {
        self.downstream
            .get_mut()
            .expect("downstream state lock poisoned")
            .attach_downstream_http2_stream(attachment);
    }

    pub(crate) fn attach_downstream_http3_stream(
        &mut self,
        attachment: &http3::Http3DownstreamStreamAttachment,
    ) {
        self.downstream
            .get_mut()
            .expect("downstream state lock poisoned")
            .attach_downstream_http3_stream(attachment);
    }

    pub(crate) fn attach_downstream_http1_upgrade(&mut self, upgrade: OnUpgrade) {
        self.downstream
            .get_mut()
            .expect("downstream state lock poisoned")
            .downstream_http1_upgrade = Some(DownstreamHttp1Upgrade::new(upgrade));
    }

    pub(crate) fn set_downstream_listener_goal(&mut self, goal: DownstreamHttpListenerGoal) {
        self.transport
            .get_mut()
            .expect("transport state lock poisoned")
            .downstream_listener_goal = goal;
    }

    pub(crate) fn with_request_head<T>(&self, read: impl FnOnce(&HttpRequestHead) -> T) -> T {
        let request_head = self
            .request_head
            .lock()
            .expect("vm request head lock poisoned");
        read(&request_head)
    }

    pub(crate) fn services(&self) -> &RuntimeServices {
        &self.services
    }

    pub(crate) fn note_downstream_transport_access(&self) {
        self.lock_transport().downstream_transport_accessed = true;
    }

    pub(crate) fn with_default_upstream_exchange<T>(
        &self,
        read: impl FnOnce(&HttpOutboundExchangeState) -> T,
    ) -> T {
        let exchanges = self.lock_exchanges();
        let exchange = exchanges
            .exchanges
            .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
            .expect("default upstream exchange should exist");
        read(exchange)
    }

    pub(crate) fn with_default_upstream_exchange_mut<T>(
        &self,
        mutate: impl FnOnce(&mut HttpOutboundExchangeState) -> T,
    ) -> T {
        let mut exchanges = self.lock_exchanges();
        let exchange = exchanges
            .exchanges
            .get_mut(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
            .expect("default upstream exchange should exist");
        mutate(exchange)
    }

    pub(crate) fn with_downstream_response<T>(
        &self,
        read: impl FnOnce(&HttpResponseOutputNode) -> T,
    ) -> T {
        let downstream = self.lock_downstream();
        read(&downstream.response_output)
    }

    pub(crate) fn with_downstream_response_mut<T>(
        &self,
        mutate: impl FnOnce(&mut HttpResponseOutputNode) -> T,
    ) -> T {
        let mut downstream = self.lock_downstream();
        mutate(&mut downstream.response_output)
    }

    pub(crate) fn downstream_websocket(&self) -> WebSocketConnectionState {
        self.lock_downstream().downstream_websocket.clone()
    }

    pub(crate) fn downstream_http1_upgrade(&self) -> Option<DownstreamHttp1Upgrade> {
        self.lock_downstream().downstream_http1_upgrade.clone()
    }

    pub(crate) fn schedule_downstream_post_response_plan(
        &self,
        plan: DownstreamPostResponsePlan,
    ) -> Result<(), VmError> {
        let mut downstream = self.lock_downstream();
        if downstream.post_response_plan.is_some() {
            return Err(VmError::HostError(
                "downstream post-response transport plan is already scheduled".to_string(),
            ));
        }
        downstream.post_response_plan = Some(plan);
        Ok(())
    }

    pub(crate) fn take_downstream_post_response_plan(&self) -> Option<DownstreamPostResponsePlan> {
        self.lock_downstream().post_response_plan.take()
    }

    pub(crate) fn begin_inline_downstream_http_response(
        &self,
        sender: oneshot::Sender<InlineDownstreamHttpResponse>,
    ) -> Result<(), VmError> {
        let mut downstream = self.lock_downstream();
        if downstream.inline_http_response_sender.is_some() {
            return Err(VmError::HostError(
                "downstream inline http response is already attached".to_string(),
            ));
        }
        downstream.inline_http_response_sender = Some(InlineDownstreamHttpResponseSender(sender));
        Ok(())
    }

    pub(crate) fn take_inline_downstream_http_response_sender(
        &self,
    ) -> Option<oneshot::Sender<InlineDownstreamHttpResponse>> {
        self.lock_downstream()
            .inline_http_response_sender
            .take()
            .map(|sender| sender.0)
    }

    pub(crate) fn downstream_connection_metadata(
        &self,
        secure: bool,
    ) -> Result<DownstreamConnectionMetadata, VmError> {
        let transport = self.lock_transport();
        let local_addr = transport.downstream_local_addr.ok_or_else(|| {
            VmError::HostError("downstream local address is unavailable".to_string())
        })?;
        let peer_addr = transport.downstream_peer_addr.ok_or_else(|| {
            VmError::HostError("downstream peer address is unavailable".to_string())
        })?;
        Ok(DownstreamConnectionMetadata {
            local_addr,
            peer_addr,
            secure,
        })
    }

    pub(crate) fn promote_downstream_http_request(
        &self,
        request: HttpRequestContext,
        http2_attachment: Option<http2::Http2DownstreamStreamAttachment>,
        downstream_http1_upgrade: Option<OnUpgrade>,
    ) {
        let request_headers = request.headers.clone();
        let request_head = HttpRequestHead {
            request_id: request.request_id,
            method: request.method,
            path: request.path,
            query: request.query,
            http_version: request.http_version,
            port: request.port,
            scheme: request.scheme,
            host: request.host,
            client_ip: request.client_ip,
            headers: request_headers.clone(),
        };
        *self
            .request_head
            .lock()
            .expect("vm request head lock poisoned") = request_head.clone();

        {
            let mut exchanges = self.lock_exchanges();
            let default_exchange = exchanges
                .exchanges
                .get_mut(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
                .expect("default upstream exchange should exist");
            default_exchange.request.method = request_head.method.clone();
            default_exchange.request.path = request_head.path.clone();
            default_exchange.request.query = request_head.query.clone();
            default_exchange.request.headers = request_head.headers.clone();
        }

        let mut downstream = self.lock_downstream();
        downstream.inbound_request_body = Arc::new(tokio::sync::Mutex::new(
            InboundRequestBodyState::new(request.body),
        ));
        downstream.downstream_websocket =
            WebSocketConnectionState::for_http_request(&request_headers);
        downstream.downstream_carrier_ref =
            http2_attachment.map_or(Some(HttpCarrierRef::DownstreamHttp1), |attachment| {
                Some(HttpCarrierRef::DownstreamHttp2Stream(
                    http2::Http2StreamRef {
                        session_id: attachment.session_id,
                        stream_id: attachment.stream_id,
                    },
                ))
            });
        downstream.downstream_http1_upgrade =
            downstream_http1_upgrade.map(DownstreamHttp1Upgrade::new);
    }

    pub(crate) fn with_downstream_websocket_mut<T>(
        &self,
        mutate: impl FnOnce(&mut WebSocketConnectionState) -> T,
    ) -> T {
        let mut downstream = self.lock_downstream();
        mutate(&mut downstream.downstream_websocket)
    }

    pub(crate) fn lock_downstream(&self) -> MutexGuard<'_, DownstreamState> {
        self.downstream
            .lock()
            .expect("vm downstream state lock poisoned")
    }

    pub(crate) fn lock_exchanges(&self) -> MutexGuard<'_, ExchangeRegistry> {
        self.exchanges
            .lock()
            .expect("vm exchange registry lock poisoned")
    }

    pub(crate) fn lock_transport(&self) -> MutexGuard<'_, TransportState> {
        self.transport
            .lock()
            .expect("vm transport state lock poisoned")
    }

    #[cfg(feature = "webrtc")]
    pub(crate) fn lock_webrtc(&self) -> MutexGuard<'_, WebRtcRegistry> {
        self.webrtc
            .lock()
            .expect("vm webrtc registry lock poisoned")
    }

    pub(crate) fn lock_proxy(&self) -> MutexGuard<'_, ProxyStreamRegistry> {
        self.proxy.lock().expect("vm proxy registry lock poisoned")
    }

    pub(crate) fn lock_edge_io(&self) -> MutexGuard<'_, EdgeIoRegistry> {
        self.edge_io
            .lock()
            .expect("vm edge io registry lock poisoned")
    }
}

pub type SharedProxyVmContext = Arc<ProxyVmContext>;

pub(crate) fn default_upstream_exchange_handle() -> i64 {
    DEFAULT_UPSTREAM_EXCHANGE_HANDLE
}

pub(crate) fn default_upstream_udp_socket_handle() -> i64 {
    DEFAULT_UPSTREAM_UDP_SOCKET_HANDLE
}

#[cfg(feature = "webrtc")]
pub(crate) fn default_upstream_webrtc_connection_handle() -> i64 {
    DEFAULT_UPSTREAM_WEBRTC_CONNECTION_HANDLE
}

pub(crate) fn allocate_outbound_exchange_handle(
    context: &SharedProxyVmContext,
) -> Result<i64, VmError> {
    let mut guard = context.lock_exchanges();
    let handle = guard.next_outbound_exchange_handle;
    if handle == i64::MAX {
        return Err(VmError::HostError(
            "outbound exchange handle space exhausted".to_string(),
        ));
    }
    guard.next_outbound_exchange_handle += 1;
    guard
        .exchanges
        .insert(handle, HttpOutboundExchangeState::new());
    Ok(handle)
}

pub(crate) fn outbound_exchange_exists(context: &SharedProxyVmContext, handle: i64) -> bool {
    context.lock_exchanges().exchanges.contains_key(&handle)
}

pub(crate) fn allocate_tcp_stream_handle(context: &SharedProxyVmContext) -> Result<i64, VmError> {
    let mut guard = context.lock_transport();
    let handle = guard.next_tcp_stream_handle;
    if handle == i64::MAX {
        return Err(VmError::HostError(
            "tcp stream handle space exhausted".to_string(),
        ));
    }
    guard.next_tcp_stream_handle += 1;
    guard.tcp_streams.insert(handle, TcpSocketState::default());
    Ok(handle)
}

pub(crate) fn tcp_stream_exists(context: &SharedProxyVmContext, handle: i64) -> bool {
    let guard = context.lock_transport();
    guard.tcp_streams.contains_key(&handle)
}

pub(crate) fn allocate_udp_socket_handle(context: &SharedProxyVmContext) -> Result<i64, VmError> {
    let mut guard = context.lock_transport();
    let handle = guard.next_udp_socket_handle;
    if handle == i64::MAX {
        return Err(VmError::HostError(
            "udp socket handle space exhausted".to_string(),
        ));
    }
    guard.next_udp_socket_handle += 1;
    guard.udp_sockets.insert(handle, UdpSocketState::default());
    Ok(handle)
}

pub(crate) fn udp_socket_exists(context: &SharedProxyVmContext, handle: i64) -> bool {
    if handle == DEFAULT_UPSTREAM_UDP_SOCKET_HANDLE {
        return true;
    }
    let guard = context.lock_transport();
    guard.udp_sockets.contains_key(&handle)
}

fn exchange_target_snapshot(guard: &ExchangeRegistry, handle: i64) -> Result<String, VmError> {
    guard
        .exchanges
        .get(&handle)
        .ok_or_else(|| VmError::HostError(format!("unknown outbound exchange handle {handle}")))?
        .request
        .target
        .clone()
        .ok_or_else(|| {
            VmError::HostError(
                "http exchange target must be configured before attaching a transport".to_string(),
            )
        })
}

pub(crate) fn attach_outbound_exchange_tcp_transport(
    context: &SharedProxyVmContext,
    exchange: i64,
    stream: i64,
) -> Result<(), VmError> {
    let mut exchanges = context.lock_exchanges();
    let _target = exchange_target_snapshot(&exchanges, exchange)?;
    let transport = context.lock_transport();
    let Some(socket) = transport.tcp_streams.get(&stream) else {
        return Err(VmError::HostError(format!(
            "http::exchange::attach_tcp requires a dynamic tcp stream handle, got {stream}",
        )));
    };
    if socket.phase() != crate::abi_impl::transport::TcpSocketPhase::Connected {
        return Err(VmError::HostError(format!(
            "tcp stream handle {stream} must be connected before it can be attached to an http exchange",
        )));
    }
    drop(transport);
    let exchange_state = exchanges
        .exchanges
        .get_mut(&exchange)
        .expect("checked exchange presence above");
    if exchange_state.response_ready() {
        return Err(VmError::HostError(format!(
            "outbound exchange handle {exchange} is read-only after the exchange has started",
        )));
    }
    exchange_state.transport.attached_transport = Some(AttachedHttpTransport::Tcp(stream));
    Ok(())
}

#[cfg(feature = "tls")]
pub(crate) fn attach_outbound_exchange_tls_transport(
    context: &SharedProxyVmContext,
    exchange: i64,
    session: i64,
) -> Result<(), VmError> {
    let mut exchanges = context.lock_exchanges();
    let target = exchange_target_snapshot(&exchanges, exchange)?;
    let mut transport = context.lock_transport();
    let tcp_state = transport.tcp_streams.get(&session).ok_or_else(|| {
        VmError::HostError(format!(
            "http::exchange::attach_tls_plaintext requires a dynamic tcp/tls handle, got {session}",
        ))
    })?;
    if !matches!(
        tcp_state.phase(),
        crate::abi_impl::transport::TcpSocketPhase::Connected
            | crate::abi_impl::transport::TcpSocketPhase::UpgradedTls
    ) {
        return Err(VmError::HostError(format!(
            "tls session handle {session} must be connected before it can be attached to an http exchange",
        )));
    }

    let tls_flow = transport
        .dynamic_tls_sessions
        .entry(session)
        .or_insert_with(TlsFlowState::for_dynamic_socket);
    if !tls_flow.handshake_complete() {
        tls_flow.observe_target(&target);
    }
    drop(transport);
    let exchange_state = exchanges
        .exchanges
        .get_mut(&exchange)
        .expect("checked exchange presence above");
    if exchange_state.response_ready() {
        return Err(VmError::HostError(format!(
            "outbound exchange handle {exchange} is read-only after the exchange has started",
        )));
    }
    exchange_state.transport.attached_transport = Some(AttachedHttpTransport::Tls(session));
    Ok(())
}

#[cfg(feature = "webrtc")]
pub(crate) fn allocate_webrtc_connection_handle(
    context: &SharedProxyVmContext,
) -> Result<i64, VmError> {
    let mut guard = context.lock_webrtc();
    let handle = guard.next_webrtc_connection_handle;
    if handle == i64::MAX {
        return Err(VmError::HostError(
            "webrtc connection handle space exhausted".to_string(),
        ));
    }
    guard.next_webrtc_connection_handle += 1;
    guard
        .webrtc_connections
        .insert(handle, WebRtcConnectionState::default());
    Ok(handle)
}

#[cfg(feature = "webrtc")]
pub(crate) fn webrtc_connection_exists(context: &SharedProxyVmContext, handle: i64) -> bool {
    if handle == DEFAULT_UPSTREAM_WEBRTC_CONNECTION_HANDLE {
        return true;
    }
    let guard = context.lock_webrtc();
    guard.webrtc_connections.contains_key(&handle)
}

pub(crate) fn outbound_exchange_tls_flow(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<TlsFlowState, VmError> {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        let guard = context.lock_transport();
        return Ok(guard.tls_dag.default_upstream.clone());
    }

    let guard = context.lock_exchanges();
    guard
        .exchanges
        .get(&handle)
        .map(|exchange| exchange.transport.tls_flow.clone())
        .ok_or_else(|| VmError::HostError(format!("unknown outbound exchange handle {handle}")))
}

pub(crate) fn schedule_downstream_http_handoff(
    context: &SharedProxyVmContext,
) -> Result<(), VmError> {
    let handoff_ready = {
        let transport = context.lock_transport();
        #[cfg(feature = "tls")]
        if transport.downstream_tls_server_start.is_some() {
            return Err(VmError::HostError(
                "downstream HTTP handoff requires TLS plaintext; complete the downstream TLS handshake first".to_string(),
            ));
        }
        #[cfg(feature = "tls")]
        let tls_ready = transport.downstream_tls_io.is_some();
        #[cfg(not(feature = "tls"))]
        let tls_ready = false;
        transport.downstream_tcp_io.is_some() || tls_ready
    };
    if !handoff_ready {
        return Err(VmError::HostError(
            "downstream HTTP handoff requires an attached raw downstream tcp or tls plaintext transport".to_string(),
        ));
    }
    if context.lock_downstream().downstream_carrier_ref.is_some() {
        return Err(VmError::HostError(
            "downstream HTTP handoff is only available before the connection has entered HTTP request semantics".to_string(),
        ));
    }
    Ok(())
}

pub(crate) enum PromotedDownstreamTransport {
    Tcp(ReplayPrefixedIo<tokio::net::TcpStream>),
    #[cfg(feature = "tls")]
    Tls(
        Box<
            ReplayPrefixedIo<
                tokio_rustls::server::TlsStream<
                    crate::abi_impl::transport::DownstreamReplayTcpStream,
                >,
            >,
        >,
    ),
}

pub(crate) async fn take_promoted_downstream_transport(
    context: &SharedProxyVmContext,
) -> Result<PromotedDownstreamTransport, VmError> {
    #[cfg(feature = "tls")]
    let (tcp_io, tls_io, preread, tls_pending) = {
        let mut transport = context.lock_transport();
        let tcp_io = transport.downstream_tcp_io.take();
        let tls_io = transport.downstream_tls_io.take();
        let preread = std::mem::take(&mut transport.downstream_preread_buffer);
        let tls_pending = transport.downstream_tls_server_start.is_some();
        (tcp_io, tls_io, preread, tls_pending)
    };
    #[cfg(not(feature = "tls"))]
    let (tcp_io, preread) = {
        let mut transport = context.lock_transport();
        (
            transport.downstream_tcp_io.take(),
            std::mem::take(&mut transport.downstream_preread_buffer),
        )
    };

    #[cfg(feature = "tls")]
    if tls_pending {
        return Err(VmError::HostError(
            "downstream HTTP handoff requires TLS plaintext; complete the downstream TLS handshake first".to_string(),
        ));
    }

    #[cfg(feature = "tls")]
    if let Some(io) = tls_io {
        let mut guard = io.lock().await;
        let stream = guard.take().ok_or_else(|| {
            VmError::HostError("downstream tls plaintext transport is already in use".to_string())
        })?;
        return Ok(PromotedDownstreamTransport::Tls(Box::new(
            ReplayPrefixedIo::new(preread, stream),
        )));
    }

    if let Some(io) = tcp_io {
        let mut guard = io.lock().await;
        let stream = guard.take().ok_or_else(|| {
            VmError::HostError("downstream tcp transport is already in use".to_string())
        })?;
        return Ok(PromotedDownstreamTransport::Tcp(ReplayPrefixedIo::new(
            preread, stream,
        )));
    }

    Err(VmError::HostError(
        "downstream HTTP handoff requires an attached raw downstream tcp or tls plaintext transport".to_string(),
    ))
}

pub(crate) fn append_outbound_exchange_body(
    context: &SharedProxyVmContext,
    handle: i64,
    text: &str,
) -> Result<(), VmError> {
    append_outbound_exchange_body_bytes(context, handle, text.as_bytes())
}

pub(crate) fn append_outbound_exchange_body_bytes(
    context: &SharedProxyVmContext,
    handle: i64,
    bytes: &[u8],
) -> Result<(), VmError> {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        if upstream_response_available(context) {
            return Err(VmError::HostError(
                "default upstream stream is read-only after the upstream exchange has started"
                    .to_string(),
            ));
        }
        context
            .lock_transport()
            .tcp_dag
            .default_upstream
            .note_write();
        context
            .lock_exchanges()
            .exchanges
            .get_mut(&handle)
            .expect("default upstream exchange should exist")
            .request
            .body_override
            .get_or_insert_with(Vec::new)
            .extend_from_slice(bytes);
        return Ok(());
    }

    let mut guard = context.lock_exchanges();
    let exchange = guard
        .exchanges
        .get_mut(&handle)
        .ok_or_else(|| VmError::HostError(format!("unknown outbound exchange handle {handle}")))?;
    if exchange.response_ready() {
        return Err(VmError::HostError(format!(
            "outbound exchange handle {handle} is read-only after the exchange has started",
        )));
    }
    exchange.transport.note_write();
    exchange
        .request
        .body_override
        .get_or_insert_with(Vec::new)
        .extend_from_slice(bytes);
    Ok(())
}

pub(crate) fn append_response_output_body_bytes(context: &SharedProxyVmContext, bytes: &[u8]) {
    context.lock_transport().tcp_dag.downstream.note_write();
    context
        .lock_downstream()
        .response_output
        .body
        .get_or_insert_with(Vec::new)
        .extend_from_slice(bytes);
}

#[derive(Debug)]
enum UpstreamResponseStartError {
    UnknownExchangeHandle(i64),
    MissingTarget,
    MissingClient,
    Protocol(String),
    TlsConfiguration(String),
    ResolveOutboundBody(String),
    UpstreamRequest(String),
}

impl UpstreamResponseStartError {
    fn as_vm_error(&self) -> VmError {
        match self {
            Self::UnknownExchangeHandle(handle) => {
                VmError::HostError(format!("unknown outbound exchange handle {handle}"))
            }
            Self::MissingTarget => VmError::HostError(
                "upstream target is unavailable before configuring the default upstream exchange target"
                    .to_string(),
            ),
            Self::MissingClient => VmError::HostError(
                "upstream client is unavailable outside the HTTP data plane".to_string(),
            ),
            Self::Protocol(message)
            | Self::TlsConfiguration(message)
            | Self::ResolveOutboundBody(message)
            | Self::UpstreamRequest(message) => VmError::HostError(message.clone()),
        }
    }
}

#[derive(Clone, Debug)]
struct PreparedUpstreamRequest {
    client: reqwest::Client,
    upstream_client_cache: Option<SharedUpstreamClientCache>,
    http2_sessions: Option<SharedHttpUpstreamSessions>,
    http3_sessions: Option<SharedHttp3UpstreamSessions>,
    version_preference: HttpVersionPreference,
    http2_mode: http2::Http2UpstreamMode,
    http3_mode: http3::Http3UpstreamMode,
    tls_flow: TlsFlowState,
    attached_transport: Option<AttachedHttpTransport>,
    method: Method,
    path: String,
    query: String,
    headers: HeaderMap,
    target: String,
}

struct StartedUpstreamResponse {
    status: u16,
    headers: HeaderMap,
    version: Version,
    carrier_ref: HttpCarrierRef,
    peer_addr: Option<String>,
    negotiated_alpn: Option<String>,
    peer_certificate_der: Option<Vec<u8>>,
    body: SharedUpstreamResponseBody,
}

#[derive(Debug)]
pub(crate) struct ResolvedHttpGraphResponse {
    pub response: Response<Body>,
    pub upstream_latency_ms: u64,
    pub post_response_plan: Option<DownstreamPostResponsePlan>,
}

pub async fn resolve_outbound_request_body(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let (body_override, inbound_body) = {
        let exchanges = context.lock_exchanges();
        let downstream = context.lock_downstream();
        (
            exchanges
                .exchanges
                .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
                .expect("default upstream exchange should exist")
                .request
                .body_override
                .clone(),
            downstream.inbound_request_body.clone(),
        )
    };

    if let Some(body) = body_override {
        return Ok(body);
    }

    let mut inbound = inbound_body.lock().await;
    inbound.read_all().await
}

pub(crate) fn build_upstream_url(
    upstream: &str,
    request_path: &str,
    request_query: &str,
) -> (String, Option<String>) {
    let path = if request_path.is_empty() {
        "/"
    } else {
        request_path
    };
    let path_and_query = if request_query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{request_query}")
    };

    if let Ok(url) = Url::parse(upstream) {
        let mut final_url = url;
        let needs_path = final_url.path() == "/" && final_url.query().is_none();
        if needs_path && path_and_query != "/" {
            let base = final_url[..url::Position::AfterPort].to_string();
            let merged = format!("{base}{path_and_query}");
            if let Ok(joined) = Url::parse(&merged) {
                final_url = joined;
            }
        }
        let host = final_url.host_str().map(|host| {
            if let Some(port) = final_url.port() {
                format!("{host}:{port}")
            } else {
                host.to_string()
            }
        });
        return (final_url.to_string(), host);
    }

    let upstream_url = format!("http://{}{path_and_query}", upstream);
    (upstream_url, Some(upstream.to_string()))
}

pub(crate) fn http_version_label(version: Version) -> &'static str {
    if http2::supports_response_version(version) {
        http2::response_version_label()
    } else {
        match version {
            Version::HTTP_09 => "0.9",
            Version::HTTP_10 => "1.0",
            Version::HTTP_11 => "1.1",
            Version::HTTP_3 => "3",
            _ => "1.1",
        }
    }
}

pub(crate) fn build_downstream_http_request_context(
    request_id: String,
    parts: axum::http::request::Parts,
    body: Body,
    connection_metadata: Option<&DownstreamConnectionMetadata>,
) -> HttpRequestContext {
    let request_scheme =
        resolve_downstream_request_scheme(&parts.uri, &parts.headers, connection_metadata);
    HttpRequestContext {
        request_id,
        method: parts.method,
        path: parts.uri.path().to_string(),
        query: parts.uri.query().unwrap_or("").to_string(),
        http_version: http_version_label(parts.version).to_string(),
        port: resolve_downstream_request_port(
            &parts.uri,
            &parts.headers,
            &request_scheme,
            connection_metadata,
        ),
        scheme: request_scheme,
        host: resolve_downstream_request_host(&parts.uri, &parts.headers),
        client_ip: resolve_downstream_request_client_ip(&parts.headers, connection_metadata),
        body,
        headers: parts.headers,
    }
}

fn resolve_downstream_request_scheme(
    uri: &axum::http::Uri,
    headers: &HeaderMap,
    connection_metadata: Option<&DownstreamConnectionMetadata>,
) -> String {
    if let Some(scheme) = uri.scheme_str() {
        return scheme.to_string();
    }
    if let Some(forwarded) = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return forwarded.to_string();
    }
    if let Some(connection_metadata) = connection_metadata
        && connection_metadata.secure
    {
        return "https".to_string();
    }
    "http".to_string()
}

fn resolve_downstream_request_port(
    uri: &axum::http::Uri,
    headers: &HeaderMap,
    scheme: &str,
    connection_metadata: Option<&DownstreamConnectionMetadata>,
) -> u16 {
    if let Some(port) = uri.port_u16() {
        return port;
    }
    if let Some(host_header) = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        && let Ok(authority) = host_header.parse::<axum::http::uri::Authority>()
        && let Some(port) = authority.port_u16()
    {
        return port;
    }
    if let Some(connection_metadata) = connection_metadata {
        return connection_metadata.local_addr.port();
    }
    if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    }
}

fn resolve_downstream_request_host(uri: &axum::http::Uri, headers: &HeaderMap) -> String {
    if let Some(host) = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return host.to_string();
    }
    uri.authority()
        .map(|authority| authority.as_str().to_string())
        .unwrap_or_default()
}

fn resolve_downstream_request_client_ip(
    headers: &HeaderMap,
    connection_metadata: Option<&DownstreamConnectionMetadata>,
) -> String {
    if let Some(value) = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
    {
        let first = value
            .split(',')
            .map(str::trim)
            .find(|candidate| !candidate.is_empty())
            .unwrap_or_default();
        if !first.is_empty() {
            return first.to_string();
        }
    }
    headers
        .get("x-real-ip")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| connection_metadata.map(|metadata| metadata.peer_addr.ip().to_string()))
        .unwrap_or_default()
}

pub(crate) fn is_hop_by_hop_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn filtered_upstream_headers(headers: &HeaderMap, host_header: Option<&str>) -> HeaderMap {
    let mut filtered = HeaderMap::new();
    for (name, value) in headers {
        if name != HOST && name != CONTENT_LENGTH && !is_hop_by_hop_header(name) {
            filtered.insert(name.clone(), value.clone());
        }
    }
    if let Some(host) = host_header
        && let Ok(value) = HeaderValue::from_str(host)
    {
        filtered.insert(HOST, value);
    }
    filtered
}

fn prepared_upstream_request(
    context: &SharedProxyVmContext,
) -> Result<PreparedUpstreamRequest, UpstreamResponseStartError> {
    let exchange = context
        .lock_exchanges()
        .exchanges
        .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
        .cloned()
        .expect("default upstream exchange should exist");
    if exchange.websocket_dag.is_websocket_mode() {
        return Err(UpstreamResponseStartError::Protocol(
            "default upstream exchange is already owned by the websocket DAG".to_string(),
        ));
    }
    let target = exchange
        .request
        .target
        .clone()
        .ok_or(UpstreamResponseStartError::MissingTarget)?;
    let attached_transport = exchange.transport.attached_transport;
    let tls_flow = match attached_transport {
        #[cfg(feature = "tls")]
        Some(AttachedHttpTransport::Tls(session)) => context
            .lock_transport()
            .dynamic_tls_sessions
            .get(&session)
            .cloned()
            .unwrap_or_else(TlsFlowState::for_dynamic_socket),
        _ => context.lock_transport().tls_dag.default_upstream.clone(),
    };
    let services = context.services();
    Ok(PreparedUpstreamRequest {
        client: services
            .upstream_client()
            .ok_or(UpstreamResponseStartError::MissingClient)?,
        upstream_client_cache: services.upstream_client_cache(),
        http2_sessions: services.upstream_http_sessions(),
        http3_sessions: services.upstream_http3_sessions(),
        version_preference: exchange.request.version_preference,
        http2_mode: http2::select_upstream_mode(
            &target,
            &tls_flow,
            exchange.request.version_preference,
        ),
        http3_mode: http3::select_upstream_mode(
            &target,
            &tls_flow,
            exchange.request.version_preference,
        ),
        tls_flow,
        attached_transport,
        method: exchange.request.method.clone(),
        path: exchange.request.path.clone(),
        query: exchange.request.query.clone(),
        headers: exchange.request.headers.clone(),
        target,
    })
}

async fn start_upstream_response(
    context: &SharedProxyVmContext,
) -> Result<HttpUpstreamResponseSnapshot, UpstreamResponseStartError> {
    start_outbound_exchange_response(context, DEFAULT_UPSTREAM_EXCHANGE_HANDLE).await
}

pub(crate) async fn ensure_upstream_response_started(
    context: &SharedProxyVmContext,
) -> Result<HttpUpstreamResponseSnapshot, VmError> {
    start_upstream_response(context)
        .await
        .map_err(|err| err.as_vm_error())
}

fn prepared_outbound_exchange_request(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<PreparedUpstreamRequest, UpstreamResponseStartError> {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return prepared_upstream_request(context);
    }

    let exchange = context
        .lock_exchanges()
        .exchanges
        .get(&handle)
        .cloned()
        .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
    if exchange.websocket_dag.is_websocket_mode() {
        return Err(UpstreamResponseStartError::Protocol(format!(
            "outbound exchange handle {handle} is already owned by the websocket DAG",
        )));
    }
    let target = exchange
        .request
        .target
        .clone()
        .ok_or(UpstreamResponseStartError::MissingTarget)?;
    let attached_transport = exchange.transport.attached_transport;
    let tls_flow = match attached_transport {
        #[cfg(feature = "tls")]
        Some(AttachedHttpTransport::Tls(session)) => context
            .lock_transport()
            .dynamic_tls_sessions
            .get(&session)
            .cloned()
            .unwrap_or_else(TlsFlowState::for_dynamic_socket),
        _ => exchange.transport.tls_flow.clone(),
    };
    let services = context.services();
    Ok(PreparedUpstreamRequest {
        client: services
            .upstream_client()
            .ok_or(UpstreamResponseStartError::MissingClient)?,
        upstream_client_cache: services.upstream_client_cache(),
        http2_sessions: services.upstream_http_sessions(),
        http3_sessions: services.upstream_http3_sessions(),
        version_preference: exchange.request.version_preference,
        http2_mode: http2::select_upstream_mode(
            &target,
            &tls_flow,
            exchange.request.version_preference,
        ),
        http3_mode: http3::select_upstream_mode(
            &target,
            &tls_flow,
            exchange.request.version_preference,
        ),
        tls_flow,
        attached_transport,
        method: exchange.request.method.clone(),
        path: exchange.request.path.clone(),
        query: exchange.request.query.clone(),
        headers: exchange.request.headers.clone(),
        target,
    })
}

async fn resolve_outbound_exchange_body(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<Vec<u8>, VmError> {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return resolve_outbound_request_body(context).await;
    }

    let guard = context.lock_exchanges();
    let exchange = guard
        .exchanges
        .get(&handle)
        .ok_or_else(|| VmError::HostError(format!("unknown outbound exchange handle {handle}")))?;
    Ok(exchange.request.body_override.clone().unwrap_or_default())
}

fn reqwest_tls_version(version: TlsProtocolVersion) -> reqwest::tls::Version {
    match version {
        TlsProtocolVersion::Tls1_0 => reqwest::tls::Version::TLS_1_0,
        TlsProtocolVersion::Tls1_1 => reqwest::tls::Version::TLS_1_1,
        TlsProtocolVersion::Tls1_2 => reqwest::tls::Version::TLS_1_2,
        TlsProtocolVersion::Tls1_3 => reqwest::tls::Version::TLS_1_3,
    }
}

fn upstream_client_cache_key(prepared: &PreparedUpstreamRequest) -> Option<UpstreamClientCacheKey> {
    let needs_configured_client = matches!(
        prepared.http2_mode,
        http2::Http2UpstreamMode::PriorKnowledge
    ) || prepared.tls_flow.requires_custom_client();
    if !needs_configured_client {
        return None;
    }

    let tls_key = tls_session_cache_key(&prepared.target, &prepared.tls_flow);
    if tls_key.is_none()
        && !matches!(
            prepared.http2_mode,
            http2::Http2UpstreamMode::PriorKnowledge
        )
    {
        return None;
    }

    Some(UpstreamClientCacheKey {
        tls_key,
        http2_mode: prepared.http2_mode,
    })
}

fn cached_upstream_client(
    cache: &SharedUpstreamClientCache,
    key: &UpstreamClientCacheKey,
) -> Option<reqwest::Client> {
    let mut cache = cache.lock().expect("upstream client cache lock poisoned");
    cache.get(key).cloned()
}

fn store_upstream_client(
    cache: &SharedUpstreamClientCache,
    key: UpstreamClientCacheKey,
    client: reqwest::Client,
) {
    let mut cache = cache.lock().expect("upstream client cache lock poisoned");
    let _ = cache.insert(key, client);
}

fn build_configured_upstream_client(
    prepared: &PreparedUpstreamRequest,
) -> Result<reqwest::Client, UpstreamResponseStartError> {
    if let (Some(min_version), Some(max_version)) = (
        prepared.tls_flow.min_version(),
        prepared.tls_flow.max_version(),
    ) && min_version > max_version
    {
        return Err(UpstreamResponseStartError::TlsConfiguration(format!(
            "tls min version {} cannot be greater than max version {}",
            min_version.as_str(),
            max_version.as_str(),
        )));
    }

    let mut builder = reqwest::Client::builder().tls_info(true);
    builder = http2::configure_reqwest_builder(builder, prepared.http2_mode);
    if !prepared.tls_flow.verify_peer() {
        builder = builder.danger_accept_invalid_certs(true);
    }
    if !prepared.tls_flow.verify_hostname() {
        builder = builder.danger_accept_invalid_hostnames(true);
    }
    if !prepared.tls_flow.sni_enabled() {
        builder = builder.tls_sni(false);
    }
    if let Some(min_version) = prepared.tls_flow.min_version() {
        builder = builder.min_tls_version(reqwest_tls_version(min_version));
    }
    if let Some(max_version) = prepared.tls_flow.max_version() {
        builder = builder.max_tls_version(reqwest_tls_version(max_version));
    }
    if let Some(bundle) = prepared.tls_flow.trusted_certificate_pem() {
        let certificates =
            reqwest::Certificate::from_pem_bundle(bundle.as_bytes()).map_err(|err| {
                UpstreamResponseStartError::TlsConfiguration(format!(
                    "failed to parse trusted certificate bundle: {err}",
                ))
            })?;
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
    }

    match (
        prepared.tls_flow.client_certificate_pem(),
        prepared.tls_flow.client_private_key_pem(),
    ) {
        (Some(certificate_pem), Some(private_key_pem)) => {
            let pem_bundle = format!("{certificate_pem}\n{private_key_pem}");
            let identity = reqwest::Identity::from_pem(pem_bundle.as_bytes()).map_err(|err| {
                UpstreamResponseStartError::TlsConfiguration(format!(
                    "failed to parse client certificate identity: {err}",
                ))
            })?;
            builder = builder.identity(identity);
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(UpstreamResponseStartError::TlsConfiguration(
                "client certificate and private key must both be configured".to_string(),
            ));
        }
        (None, None) => {}
    }

    builder.build().map_err(|err| {
        UpstreamResponseStartError::TlsConfiguration(format!(
            "failed to build reqwest TLS client: {err}",
        ))
    })
}

fn configured_upstream_client(
    prepared: &PreparedUpstreamRequest,
) -> Result<reqwest::Client, UpstreamResponseStartError> {
    if !matches!(
        prepared.http2_mode,
        http2::Http2UpstreamMode::PriorKnowledge
    ) && (!prepared.tls_flow.is_present() || !prepared.tls_flow.requires_custom_client())
    {
        return Ok(prepared.client.clone());
    }

    let cache_key = upstream_client_cache_key(prepared);
    if let (Some(cache), Some(key)) = (prepared.upstream_client_cache.as_ref(), cache_key.as_ref())
        && let Some(client) = cached_upstream_client(cache, key)
    {
        return Ok(client);
    }

    let client = build_configured_upstream_client(prepared)?;
    if let (Some(cache), Some(key)) = (prepared.upstream_client_cache.as_ref(), cache_key) {
        store_upstream_client(cache, key, client.clone());
    }
    Ok(client)
}

fn response_peer_certificate_der(response: &reqwest::Response) -> Option<Vec<u8>> {
    response
        .extensions()
        .get::<reqwest::tls::TlsInfo>()
        .and_then(|info| info.peer_certificate())
        .map(|bytes| bytes.to_vec())
}

async fn take_dynamic_tcp_stream_for_http(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<tokio::net::TcpStream, UpstreamResponseStartError> {
    let io = {
        let mut guard = context.lock_transport();
        let Some(state) = guard.tcp_streams.get_mut(&handle) else {
            return Err(UpstreamResponseStartError::Protocol(format!(
                "dynamic tcp stream handle {handle} is unavailable for http attachment",
            )));
        };
        state.mark_http_attached();
        guard.tcp_stream_ios.remove(&handle).ok_or_else(|| {
            UpstreamResponseStartError::Protocol(format!(
                "dynamic tcp stream handle {handle} has no active transport",
            ))
        })?
    };

    let mut guard = io.lock().await;
    guard.take().ok_or_else(|| {
        UpstreamResponseStartError::Protocol(format!(
            "dynamic tcp stream handle {handle} is already in use",
        ))
    })
}

#[cfg(feature = "tls")]
async fn take_dynamic_tls_stream_for_http(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>, UpstreamResponseStartError> {
    let io = {
        let mut guard = context.lock_transport();
        let Some(state) = guard.tcp_streams.get_mut(&handle) else {
            return Err(UpstreamResponseStartError::Protocol(format!(
                "dynamic tls session handle {handle} is unavailable for http attachment",
            )));
        };
        state.mark_http_attached();
        guard
            .dynamic_tls_session_ios
            .remove(&handle)
            .ok_or_else(|| {
                UpstreamResponseStartError::Protocol(format!(
                    "dynamic tls session handle {handle} has no active plaintext transport",
                ))
            })?
    };

    let mut guard = io.lock().await;
    guard.take().ok_or_else(|| {
        UpstreamResponseStartError::Protocol(format!(
            "dynamic tls session handle {handle} is already in use",
        ))
    })
}

fn with_outbound_tls_flow_mut<T>(
    context: &SharedProxyVmContext,
    handle: i64,
    mutate: impl FnOnce(&mut TlsFlowState) -> T,
) -> Result<T, UpstreamResponseStartError> {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return Ok(mutate(
            &mut context.lock_transport().tls_dag.default_upstream,
        ));
    }

    let mut exchanges = context.lock_exchanges();
    let exchange = exchanges
        .exchanges
        .get_mut(&handle)
        .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
    Ok(mutate(&mut exchange.transport.tls_flow))
}

fn header_content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

async fn start_upstream_response_via_reqwest(
    handle: i64,
    prepared: &PreparedUpstreamRequest,
    upstream_url: &str,
    headers: &HeaderMap,
    request_body: Vec<u8>,
) -> Result<StartedUpstreamResponse, UpstreamResponseStartError> {
    let client = configured_upstream_client(prepared)?;
    let mut outbound = client
        .request(prepared.method.clone(), upstream_url)
        .body(request_body);
    for (name, value) in headers {
        outbound = outbound.header(name, value);
    }

    let upstream_response = outbound.send().await.map_err(|err| {
        UpstreamResponseStartError::UpstreamRequest(format!(
            "outbound request to {upstream_url} failed while evaluating host call: {err}",
        ))
    })?;
    let version = upstream_response.version();
    Ok(StartedUpstreamResponse {
        status: upstream_response.status().as_u16(),
        headers: upstream_response.headers().clone(),
        version,
        carrier_ref: if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
            HttpCarrierRef::Http1DefaultUpstream
        } else {
            HttpCarrierRef::Http1DynamicExchange(handle)
        },
        peer_addr: upstream_response.remote_addr().map(|addr| addr.to_string()),
        negotiated_alpn: alpn_from_http_version(version),
        peer_certificate_der: response_peer_certificate_der(&upstream_response),
        body: Arc::new(tokio::sync::Mutex::new(
            UpstreamResponseBodyState::from_reqwest(upstream_response),
        )),
    })
}

async fn start_upstream_response_via_attached_http1<I>(
    handle: i64,
    prepared: &PreparedUpstreamRequest,
    request_path: &str,
    headers: HeaderMap,
    request_body: Vec<u8>,
    io: I,
) -> Result<StartedUpstreamResponse, UpstreamResponseStartError>
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(io))
            .await
            .map_err(|err| {
                UpstreamResponseStartError::UpstreamRequest(format!(
                    "failed to establish attached http/1.1 client connection: {err}",
                ))
            })?;
    tokio::spawn(async move {
        let _ = connection.await;
    });

    let mut request = hyper::Request::builder()
        .method(prepared.method.clone())
        .uri(request_path)
        .version(Version::HTTP_11)
        .body(Full::new(Bytes::from(request_body)))
        .map_err(|err| {
            UpstreamResponseStartError::Protocol(format!(
                "failed to build attached http request: {err}",
            ))
        })?;
    for (name, value) in &headers {
        request.headers_mut().insert(name, value.clone());
    }

    let response = sender.send_request(request).await.map_err(|err| {
        UpstreamResponseStartError::UpstreamRequest(format!(
            "attached http request failed while evaluating host call: {err}",
        ))
    })?;
    let version = response.version();
    let content_length = header_content_length(response.headers());
    Ok(StartedUpstreamResponse {
        status: response.status().as_u16(),
        headers: response.headers().clone(),
        version,
        carrier_ref: if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
            HttpCarrierRef::Http1DefaultUpstream
        } else {
            HttpCarrierRef::Http1DynamicExchange(handle)
        },
        peer_addr: None,
        negotiated_alpn: Some(HTTP11_ALPN_PROTOCOL.to_string()),
        peer_certificate_der: None,
        body: Arc::new(tokio::sync::Mutex::new(
            UpstreamResponseBodyState::from_hyper(response.into_body(), None, content_length),
        )),
    })
}

async fn start_upstream_response_via_attached_transport(
    context: &SharedProxyVmContext,
    handle: i64,
    prepared: &PreparedUpstreamRequest,
    headers: HeaderMap,
    request_body: Vec<u8>,
) -> Result<StartedUpstreamResponse, UpstreamResponseStartError> {
    if matches!(prepared.version_preference, HttpVersionPreference::Http3) {
        return Err(UpstreamResponseStartError::Protocol(
            "http3 cannot use an attached tcp or tls plaintext transport".to_string(),
        ));
    }

    let request_path = super::request_path_with_query(&prepared.path, &prepared.query);
    match prepared.attached_transport {
        Some(AttachedHttpTransport::Tcp(stream)) => {
            let stream_handle = stream;
            if Url::parse(&prepared.target)
                .ok()
                .map(|url| url.scheme().eq_ignore_ascii_case("https"))
                .unwrap_or(false)
            {
                return Err(UpstreamResponseStartError::Protocol(
                    "attached tcp transports cannot be used with https targets; attach a tls plaintext transport instead"
                        .to_string(),
                ));
            }
            let stream = take_dynamic_tcp_stream_for_http(context, stream_handle).await?;
            let mut started = start_upstream_response_via_attached_http1(
                handle,
                prepared,
                &request_path,
                headers,
                request_body,
                stream,
            )
            .await?;
            started.peer_addr = context
                .lock_transport()
                .tcp_streams
                .get(&stream_handle)
                .map(|state| state.peer_address().to_string())
                .filter(|peer_addr| !peer_addr.is_empty());
            Ok(started)
        }
        #[cfg(feature = "tls")]
        Some(AttachedHttpTransport::Tls(session)) => {
            let stream = take_dynamic_tls_stream_for_http(context, session).await?;
            let mut started = start_upstream_response_via_attached_http1(
                handle,
                prepared,
                &request_path,
                headers,
                request_body,
                stream,
            )
            .await?;
            started.negotiated_alpn = {
                let guard = context.lock_transport();
                guard
                    .dynamic_tls_sessions
                    .get(&session)
                    .and_then(|flow| (!flow.alpn().is_empty()).then(|| flow.alpn().to_string()))
            };
            started.peer_certificate_der = {
                let guard = context.lock_transport();
                guard
                    .dynamic_tls_sessions
                    .get(&session)
                    .and_then(|flow| flow.peer_certificate_der().map(|bytes| bytes.to_vec()))
            };
            started.peer_addr = context
                .lock_transport()
                .tcp_streams
                .get(&session)
                .map(|state| state.peer_address().to_string())
                .filter(|peer_addr| !peer_addr.is_empty());
            Ok(started)
        }
        None => Err(UpstreamResponseStartError::Protocol(
            "attached transport is unavailable".to_string(),
        )),
    }
}

#[cfg(feature = "http2")]
async fn start_upstream_response_via_http2(
    handle: i64,
    prepared: &PreparedUpstreamRequest,
    upstream_url: &str,
    headers: HeaderMap,
    request_body: Vec<u8>,
) -> Result<StartedUpstreamResponse, http2::Http2RequestError> {
    let sessions = prepared
        .http2_sessions
        .as_ref()
        .expect("explicit http2 transport requires shared sessions");
    let started = http2::send_request(http2::Http2SendRequest {
        sessions,
        exchange_handle: handle,
        target: &prepared.target,
        upstream_url,
        mode: prepared.http2_mode,
        tls_flow: &prepared.tls_flow,
        method: prepared.method.clone(),
        headers,
        request_body,
    })
    .await?;
    let version = started.response.version();
    let content_length = header_content_length(started.response.headers());
    Ok(StartedUpstreamResponse {
        status: started.response.status().as_u16(),
        headers: started.response.headers().clone(),
        version,
        carrier_ref: HttpCarrierRef::UpstreamHttp2Stream(started.stream_ref),
        peer_addr: started.peer_addr,
        negotiated_alpn: started.negotiated_alpn,
        peer_certificate_der: started.peer_certificate_der,
        body: Arc::new(tokio::sync::Mutex::new(
            UpstreamResponseBodyState::from_hyper(
                started.response.into_body(),
                Some(started.body_tracker),
                content_length,
            ),
        )),
    })
}

#[cfg(feature = "http3")]
async fn start_upstream_response_via_http3(
    handle: i64,
    prepared: &PreparedUpstreamRequest,
    upstream_url: &str,
    headers: HeaderMap,
    request_body: Vec<u8>,
) -> Result<StartedUpstreamResponse, http3::Http3RequestError> {
    let sessions = prepared
        .http3_sessions
        .clone()
        .expect("explicit http3 transport requires shared sessions");
    let started = http3::send_request(http3::Http3SendRequestOptions {
        exchange_handle: handle,
        target: prepared.target.clone(),
        upstream_url: upstream_url.to_string(),
        method: prepared.method.clone(),
        headers,
        request_body,
        tls_flow: prepared.tls_flow.clone(),
        mode: prepared.http3_mode,
        sessions,
    })
    .await?;
    let version = started.response.version();
    let content_length = header_content_length(started.response.headers());
    Ok(StartedUpstreamResponse {
        status: started.response.status().as_u16(),
        headers: started.response.headers().clone(),
        version,
        carrier_ref: HttpCarrierRef::UpstreamHttp3Stream(started.stream_ref),
        peer_addr: started.peer_addr,
        negotiated_alpn: started.negotiated_alpn,
        peer_certificate_der: started.peer_certificate_der,
        body: Arc::new(tokio::sync::Mutex::new(
            UpstreamResponseBodyState::from_http3(
                started.request_stream,
                Some(started.body_tracker),
                content_length,
            ),
        )),
    })
}

fn note_outbound_tls_prepared(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<(), UpstreamResponseStartError> {
    with_outbound_tls_flow_mut(context, handle, |flow| {
        flow.note_handshake_prepared();
        flow.note_client_hello_sent();
    })?;
    Ok(())
}

fn outbound_tls_handshake_complete(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<bool, UpstreamResponseStartError> {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return Ok(context
            .lock_transport()
            .tls_dag
            .default_upstream
            .handshake_complete());
    }

    let guard = context.lock_exchanges();
    let exchange = guard
        .exchanges
        .get(&handle)
        .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
    Ok(exchange.transport.tls_flow.handshake_complete())
}

fn cache_outbound_tls_session(
    context: &SharedProxyVmContext,
    handle: i64,
    negotiated_alpn: Option<String>,
    peer_certificate_der: Option<Vec<u8>>,
) -> Result<(), UpstreamResponseStartError> {
    let (cache, key, cached) = {
        let Some(cache) = context.services().tls_session_cache() else {
            return Ok(());
        };
        let (target, flow) = if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
            let target = context
                .lock_exchanges()
                .exchanges
                .get(&handle)
                .and_then(|exchange| exchange.request.target.clone());
            let flow = context.lock_transport().tls_dag.default_upstream.clone();
            (target, flow)
        } else {
            let exchanges = context.lock_exchanges();
            let exchange = exchanges
                .exchanges
                .get(&handle)
                .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
            (
                exchange.request.target.clone(),
                exchange.transport.tls_flow.clone(),
            )
        };
        let Some(target) = target else {
            return Ok(());
        };
        let Some(key) = tls_session_cache_key(&target, &flow) else {
            return Ok(());
        };
        let cached = CachedTlsSession {
            negotiated_alpn,
            peer_name: (!flow.peer_name().is_empty()).then(|| flow.peer_name().to_string()),
            server_name: (!flow.server_name().is_empty()).then(|| flow.server_name().to_string()),
            peer_certificate_der,
        };
        (cache, key, cached)
    };

    let mut guard = cache.lock().expect("tls session cache lock poisoned");
    guard.insert(key, cached);
    Ok(())
}

fn note_outbound_tls_failure(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<(), UpstreamResponseStartError> {
    with_outbound_tls_flow_mut(context, handle, TlsFlowState::mark_failed)?;
    Ok(())
}

fn finalize_outbound_tls_handshake(
    context: &SharedProxyVmContext,
    handle: i64,
    negotiated_alpn: Option<String>,
    peer_certificate_der: Option<Vec<u8>>,
) -> Result<(), UpstreamResponseStartError> {
    with_outbound_tls_flow_mut(context, handle, |flow| {
        flow.note_server_hello_received();
        flow.note_server_certificate_received(peer_certificate_der);
        if flow.verify_peer() && flow.verify_hostname() {
            flow.note_server_certificate_verified();
        } else {
            flow.note_verification_skipped();
        }
        if !flow.accepts_negotiated_alpn(negotiated_alpn.as_deref()) {
            flow.mark_failed();
            return Err(UpstreamResponseStartError::UpstreamRequest(format!(
                "tls ALPN mismatch: requested [{}], negotiated {}",
                flow.desired_alpn().join(", "),
                negotiated_alpn.as_deref().unwrap_or("none"),
            )));
        }
        flow.mark_handshake_complete(negotiated_alpn);
        Ok(())
    })?
}

fn mark_outbound_tcp_connected(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<(), UpstreamResponseStartError> {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        context
            .lock_transport()
            .tcp_dag
            .default_upstream
            .mark_connected();
        return Ok(());
    }

    let mut exchanges = context.lock_exchanges();
    let exchange = exchanges
        .exchanges
        .get_mut(&handle)
        .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
    exchange.transport.tcp_flow.mark_connected();
    Ok(())
}

async fn start_outbound_exchange_response(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<HttpUpstreamResponseSnapshot, UpstreamResponseStartError> {
    {
        let guard = context.lock_exchanges();
        let exchange = guard
            .exchanges
            .get(&handle)
            .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
        if let Ok(snapshot) = exchange.response_snapshot() {
            return Ok(snapshot);
        }
    }

    let prepared = prepared_outbound_exchange_request(context, handle)?;
    let request_body = resolve_outbound_exchange_body(context, handle)
        .await
        .map_err(|err| {
            UpstreamResponseStartError::ResolveOutboundBody(format!(
                "failed to resolve outbound exchange body: {err}",
            ))
        })?;
    let (upstream_url, host_header) =
        build_upstream_url(&prepared.target, &prepared.path, &prepared.query);
    let outbound_headers = filtered_upstream_headers(&prepared.headers, host_header.as_deref());

    let is_attached_transport = prepared.attached_transport.is_some();
    if is_attached_transport {
        let started = Instant::now();
        let upstream_response = start_upstream_response_via_attached_transport(
            context,
            handle,
            &prepared,
            outbound_headers,
            request_body,
        )
        .await?;
        let upstream_response_version = upstream_response.version;
        mark_outbound_tcp_connected(context, handle)?;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let response_http_version = http_version_label(upstream_response_version).to_string();
        let response_carrier_kind = upstream_response.carrier_ref.kind();
        let snapshot = HttpUpstreamResponseSnapshot {
            status: upstream_response.status,
            headers: upstream_response.headers.clone(),
            http_version: response_http_version.clone(),
            carrier_kind: response_carrier_kind,
            carrier_ref: upstream_response.carrier_ref.clone(),
            body: upstream_response.body.clone(),
        };

        let mut guard = context.lock_exchanges();
        let exchange = guard
            .exchanges
            .get_mut(&handle)
            .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
        exchange.transport.attached_transport = None;
        exchange.store_response(StoredUpstreamResponse::new(snapshot.clone(), latency_ms));
        exchange
            .transport
            .mark_response_ready(upstream_response_version, snapshot.carrier_ref.clone());
        exchange
            .transport
            .set_peer_addr(upstream_response.peer_addr);
        return Ok(snapshot);
    }

    let handshake_already_complete = outbound_tls_handshake_complete(context, handle)?;
    if !handshake_already_complete {
        note_outbound_tls_prepared(context, handle)?;
    }
    let started = Instant::now();
    let use_http3 = http3::should_use_explicit_upstream_transport(
        prepared.http3_mode,
        prepared.http3_sessions.as_ref(),
    );
    let use_http2 = http2::should_use_explicit_upstream_transport(
        prepared.http2_mode,
        prepared.http2_sessions.as_ref(),
    );
    let upstream_response = if use_http3 {
        #[cfg(feature = "http3")]
        {
            match start_upstream_response_via_http3(
                handle,
                &prepared,
                &upstream_url,
                outbound_headers.clone(),
                request_body.clone(),
            )
            .await
            {
                Ok(started) => started,
                Err(http3::Http3RequestError::FallbackToHttp2 { .. }) => {
                    if use_http2 {
                        #[cfg(feature = "http2")]
                        {
                            match start_upstream_response_via_http2(
                                handle,
                                &prepared,
                                &upstream_url,
                                outbound_headers.clone(),
                                request_body.clone(),
                            )
                            .await
                            {
                                Ok(started) => started,
                                Err(http2::Http2RequestError::FallbackToHttp1 { .. }) => {
                                    match start_upstream_response_via_reqwest(
                                        handle,
                                        &prepared,
                                        &upstream_url,
                                        &outbound_headers,
                                        request_body,
                                    )
                                    .await
                                    {
                                        Ok(started) => started,
                                        Err(err) => {
                                            let _ = note_outbound_tls_failure(context, handle);
                                            return Err(err);
                                        }
                                    }
                                }
                                Err(err) => {
                                    let _ = note_outbound_tls_failure(context, handle);
                                    return Err(UpstreamResponseStartError::UpstreamRequest(
                                        format!(
                                            "outbound exchange {handle} failed while evaluating host call: {}",
                                            err.into_message(),
                                        ),
                                    ));
                                }
                            }
                        }
                        #[cfg(not(feature = "http2"))]
                        {
                            unreachable!("explicit http2 transport requires the http2 feature");
                        }
                    } else {
                        match start_upstream_response_via_reqwest(
                            handle,
                            &prepared,
                            &upstream_url,
                            &outbound_headers,
                            request_body,
                        )
                        .await
                        {
                            Ok(started) => started,
                            Err(err) => {
                                let _ = note_outbound_tls_failure(context, handle);
                                return Err(err);
                            }
                        }
                    }
                }
                Err(err) => {
                    let _ = note_outbound_tls_failure(context, handle);
                    return Err(UpstreamResponseStartError::UpstreamRequest(format!(
                        "outbound exchange {handle} failed while evaluating host call: {}",
                        err.into_message(),
                    )));
                }
            }
        }
        #[cfg(not(feature = "http3"))]
        {
            unreachable!("explicit http3 transport requires the http3 feature");
        }
    } else if use_http2 {
        #[cfg(feature = "http2")]
        {
            match start_upstream_response_via_http2(
                handle,
                &prepared,
                &upstream_url,
                outbound_headers.clone(),
                request_body.clone(),
            )
            .await
            {
                Ok(started) => started,
                Err(http2::Http2RequestError::FallbackToHttp1 { .. }) => {
                    match start_upstream_response_via_reqwest(
                        handle,
                        &prepared,
                        &upstream_url,
                        &outbound_headers,
                        request_body,
                    )
                    .await
                    {
                        Ok(started) => started,
                        Err(err) => {
                            let _ = note_outbound_tls_failure(context, handle);
                            return Err(err);
                        }
                    }
                }
                Err(err) => {
                    let _ = note_outbound_tls_failure(context, handle);
                    return Err(UpstreamResponseStartError::UpstreamRequest(format!(
                        "outbound exchange {handle} failed while evaluating host call: {}",
                        err.into_message(),
                    )));
                }
            }
        }
        #[cfg(not(feature = "http2"))]
        {
            unreachable!("explicit http2 transport requires the http2 feature");
        }
    } else {
        match start_upstream_response_via_reqwest(
            handle,
            &prepared,
            &upstream_url,
            &outbound_headers,
            request_body,
        )
        .await
        {
            Ok(started) => started,
            Err(err) => {
                let _ = note_outbound_tls_failure(context, handle);
                return Err(err);
            }
        }
    };
    let upstream_response_version = upstream_response.version;
    let negotiated_alpn = upstream_response.negotiated_alpn.clone();
    if !handshake_already_complete {
        finalize_outbound_tls_handshake(
            context,
            handle,
            negotiated_alpn.clone(),
            upstream_response.peer_certificate_der.clone(),
        )?;
        cache_outbound_tls_session(
            context,
            handle,
            negotiated_alpn.clone(),
            upstream_response.peer_certificate_der.clone(),
        )?;
    }
    if !http3::supports_response_version(upstream_response_version) {
        mark_outbound_tcp_connected(context, handle)?;
    }
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let response_http_version = http_version_label(upstream_response_version).to_string();
    let response_carrier_kind = upstream_response.carrier_ref.kind();
    let snapshot = HttpUpstreamResponseSnapshot {
        status: upstream_response.status,
        headers: upstream_response.headers.clone(),
        http_version: response_http_version.clone(),
        carrier_kind: response_carrier_kind,
        carrier_ref: upstream_response.carrier_ref.clone(),
        body: upstream_response.body.clone(),
    };

    let mut guard = context.lock_exchanges();
    let exchange = guard
        .exchanges
        .get_mut(&handle)
        .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
    if let Ok(existing) = exchange.response_snapshot() {
        return Ok(existing);
    }
    exchange.store_response(StoredUpstreamResponse::new(snapshot.clone(), latency_ms));
    exchange
        .transport
        .mark_response_ready(upstream_response_version, snapshot.carrier_ref.clone());
    exchange
        .transport
        .set_peer_addr(upstream_response.peer_addr);
    Ok(snapshot)
}

pub(crate) async fn ensure_outbound_exchange_response_started(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<HttpUpstreamResponseSnapshot, VmError> {
    start_outbound_exchange_response(context, handle)
        .await
        .map_err(|err| err.as_vm_error())
}

pub(crate) fn upstream_response_available(context: &SharedProxyVmContext) -> bool {
    context
        .lock_exchanges()
        .exchanges
        .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
        .map(HttpOutboundExchangeState::response_ready)
        .unwrap_or(false)
}

#[allow(dead_code)]
pub(crate) fn outbound_exchange_response_available(
    context: &SharedProxyVmContext,
    handle: i64,
) -> bool {
    let guard = context.lock_exchanges();
    guard
        .exchanges
        .get(&handle)
        .map(HttpOutboundExchangeState::response_ready)
        .unwrap_or(false)
}

pub(crate) async fn read_upstream_response_all(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let snapshot = ensure_upstream_response_started(context).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.read_all().await
}

pub(crate) async fn read_outbound_exchange_response_all(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<Vec<u8>, VmError> {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return read_upstream_response_all(context).await;
    }
    let snapshot = ensure_outbound_exchange_response_started(context, handle).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.read_all().await
}

pub(crate) async fn read_upstream_response_next_chunk(
    context: &SharedProxyVmContext,
    max_bytes: usize,
) -> Result<Vec<u8>, VmError> {
    let snapshot = ensure_upstream_response_started(context).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.read_next_chunk(max_bytes).await
}

pub(crate) async fn read_outbound_exchange_response_next_chunk(
    context: &SharedProxyVmContext,
    handle: i64,
    max_bytes: usize,
) -> Result<Vec<u8>, VmError> {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return read_upstream_response_next_chunk(context, max_bytes).await;
    }
    let snapshot = ensure_outbound_exchange_response_started(context, handle).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.read_next_chunk(max_bytes).await
}

pub(crate) async fn read_upstream_response_next_line(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let snapshot = ensure_upstream_response_started(context).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.read_next_line().await
}

#[allow(dead_code)]
pub(crate) async fn read_outbound_exchange_response_next_line(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<Vec<u8>, VmError> {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return read_upstream_response_next_line(context).await;
    }
    let snapshot = ensure_outbound_exchange_response_started(context, handle).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.read_next_line().await
}

pub(crate) async fn upstream_response_eof(context: &SharedProxyVmContext) -> Result<bool, VmError> {
    let snapshot = ensure_upstream_response_started(context).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.eof().await
}

pub(crate) async fn outbound_exchange_response_eof(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<bool, VmError> {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return upstream_response_eof(context).await;
    }
    let snapshot = ensure_outbound_exchange_response_started(context, handle).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.eof().await
}

fn current_upstream_latency_ms(context: &SharedProxyVmContext) -> u64 {
    context
        .lock_exchanges()
        .exchanges
        .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
        .map(|exchange| exchange.upstream_latency_ms)
        .unwrap_or(0)
}

fn merge_headers(target: &mut HeaderMap, overlay: &HeaderMap) {
    for (name, value) in overlay {
        target.insert(name, value.clone());
    }
}

fn text_response(status: StatusCode, text: &str) -> Response<Body> {
    let mut response = Response::new(Body::from(text.to_string()));
    *response.status_mut() = status;
    response
}

fn response_from_output(
    body: Vec<u8>,
    headers: HeaderMap,
    status_code: Option<u16>,
) -> Response<Body> {
    let mut response = Response::new(Body::from(body));
    let status = status_code
        .and_then(|code| StatusCode::from_u16(code).ok())
        .unwrap_or(StatusCode::OK);
    *response.status_mut() = status;
    merge_headers(response.headers_mut(), &headers);
    if !response.headers().contains_key(CONTENT_TYPE) {
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    }
    response
}

fn response_from_connect_tunnel(headers: HeaderMap) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::OK;
    merge_headers(response.headers_mut(), &headers);
    response.headers_mut().remove(CONTENT_TYPE);
    response.headers_mut().remove(CONTENT_LENGTH);
    response
}

#[cfg(feature = "websocket")]
fn response_from_websocket_tunnel(
    request_headers: &HeaderMap,
    headers: HeaderMap,
    selected_subprotocol: Option<&str>,
) -> Result<Response<Body>, VmError> {
    let request_key = request_headers
        .get("sec-websocket-key")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            VmError::HostError(
                "downstream websocket tunnel requires a valid sec-websocket-key header".to_string(),
            )
        })?;
    let accept = derive_accept_key(request_key.as_bytes());
    let accept = HeaderValue::from_str(&accept).map_err(|err| {
        VmError::HostError(format!(
            "failed to encode websocket accept header for downstream tunnel: {err}",
        ))
    })?;

    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    merge_headers(response.headers_mut(), &headers);
    response
        .headers_mut()
        .insert("connection", HeaderValue::from_static("Upgrade"));
    response
        .headers_mut()
        .insert("upgrade", HeaderValue::from_static("websocket"));
    response
        .headers_mut()
        .insert("sec-websocket-accept", accept);
    if let Some(subprotocol) = selected_subprotocol {
        let value = HeaderValue::from_str(subprotocol).map_err(|err| {
            VmError::HostError(format!(
                "invalid negotiated websocket subprotocol '{subprotocol}': {err}",
            ))
        })?;
        response
            .headers_mut()
            .insert("sec-websocket-protocol", value);
    }
    response.headers_mut().remove(CONTENT_TYPE);
    response.headers_mut().remove(CONTENT_LENGTH);
    Ok(response)
}

async fn response_from_upstream_snapshot(
    snapshot: HttpUpstreamResponseSnapshot,
    response_headers: HeaderMap,
    response_status: Option<u16>,
) -> Result<Response<Body>, VmError> {
    let body = {
        let mut upstream_body = snapshot.body.lock().await;
        let passthrough = upstream_body.take_streaming_passthrough();
        Body::from_stream(try_unfold(passthrough, |mut state| async move {
            let chunk = state
                .next_chunk()
                .await
                .map_err(|err| io::Error::other(err.to_string()))?;
            Ok::<_, io::Error>(chunk.map(|chunk| (chunk, state)))
        }))
    };
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::from_u16(snapshot.status).unwrap_or(StatusCode::OK);
    for (name, value) in &snapshot.headers {
        if !is_hop_by_hop_header(name) {
            response.headers_mut().insert(name, value.clone());
        }
    }
    if let Some(status) = response_status.and_then(|code| StatusCode::from_u16(code).ok()) {
        *response.status_mut() = status;
    }
    merge_headers(response.headers_mut(), &response_headers);
    Ok(response)
}

pub(crate) async fn resolve_http_graph_response(
    context: &SharedProxyVmContext,
) -> ResolvedHttpGraphResponse {
    let (
        response_body,
        response_headers,
        response_status,
        has_post_response_plan,
        has_upstream_target,
        default_upstream_websocket_mode,
        upstream_response,
    ) = {
        let downstream = context.lock_downstream();
        let exchanges = context.lock_exchanges();
        let default_exchange = exchanges
            .exchanges
            .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
            .expect("default upstream exchange should exist");
        (
            downstream.response_output.body.clone(),
            downstream.response_output.headers.clone(),
            downstream.response_output.status,
            downstream.post_response_plan.is_some(),
            default_exchange.request.target.is_some(),
            default_exchange.websocket_dag.is_websocket_mode(),
            match &default_exchange.response {
                HttpUpstreamResponseNode::Ready(snapshot) => Some(snapshot.clone()),
                HttpUpstreamResponseNode::NotStarted => None,
            },
        )
    };

    if has_post_response_plan {
        let plan = context
            .take_downstream_post_response_plan()
            .expect("downstream post-response plan should exist");
        let response = match &plan {
            DownstreamPostResponsePlan::ConnectTunnel(_) => {
                Ok(response_from_connect_tunnel(response_headers))
            }
            #[cfg(feature = "websocket")]
            DownstreamPostResponsePlan::WebSocketTunnel(plan) => context.with_request_head(
                |request_head| {
                    response_from_websocket_tunnel(
                        request_head.headers(),
                        response_headers,
                        plan.selected_subprotocol.as_deref(),
                    )
                },
            ),
        };
        let response = match response {
            Ok(response) => response,
            Err(_) => {
                return ResolvedHttpGraphResponse {
                    response: text_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal server error",
                    ),
                    upstream_latency_ms: current_upstream_latency_ms(context),
                    post_response_plan: None,
                };
            }
        };
        return ResolvedHttpGraphResponse {
            response,
            upstream_latency_ms: current_upstream_latency_ms(context),
            post_response_plan: Some(plan),
        };
    }

    if let Some(body) = response_body {
        return ResolvedHttpGraphResponse {
            response: response_from_output(body, response_headers, response_status),
            upstream_latency_ms: current_upstream_latency_ms(context),
            post_response_plan: None,
        };
    }

    let snapshot = if let Some(snapshot) = upstream_response {
        Some(snapshot)
    } else if has_upstream_target && !default_upstream_websocket_mode {
        match start_upstream_response(context).await {
            Ok(snapshot) => Some(snapshot),
            Err(UpstreamResponseStartError::MissingTarget) => None,
            Err(UpstreamResponseStartError::UnknownExchangeHandle(_))
            | Err(UpstreamResponseStartError::MissingClient)
            | Err(UpstreamResponseStartError::Protocol(_))
            | Err(UpstreamResponseStartError::TlsConfiguration(_))
            | Err(UpstreamResponseStartError::ResolveOutboundBody(_)) => {
                return ResolvedHttpGraphResponse {
                    response: text_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal server error",
                    ),
                    upstream_latency_ms: current_upstream_latency_ms(context),
                    post_response_plan: None,
                };
            }
            Err(UpstreamResponseStartError::UpstreamRequest(_)) => {
                return ResolvedHttpGraphResponse {
                    response: text_response(StatusCode::BAD_GATEWAY, "bad gateway"),
                    upstream_latency_ms: current_upstream_latency_ms(context),
                    post_response_plan: None,
                };
            }
        }
    } else {
        None
    };

    let Some(snapshot) = snapshot else {
        return ResolvedHttpGraphResponse {
            response: text_response(StatusCode::NOT_FOUND, "not found"),
            upstream_latency_ms: 0,
            post_response_plan: None,
        };
    };

    match response_from_upstream_snapshot(snapshot, response_headers, response_status).await {
        Ok(response) => ResolvedHttpGraphResponse {
            response,
            upstream_latency_ms: current_upstream_latency_ms(context),
            post_response_plan: None,
        },
        Err(_) => ResolvedHttpGraphResponse {
            response: text_response(StatusCode::BAD_GATEWAY, "bad gateway"),
            upstream_latency_ms: current_upstream_latency_ms(context),
            post_response_plan: None,
        },
    }
}

fn mark_downstream_transport_closed(context: &SharedProxyVmContext) {
    let is_http3 = context
        .lock_downstream()
        .downstream_carrier_ref
        .as_ref()
        .is_some_and(|carrier_ref| carrier_ref.kind() == HttpCarrierKind::Http3);
    if is_http3 {
        return;
    }
    let mut transport = context.lock_transport();
    transport.tcp_dag.downstream.mark_closed();
    transport.tls_dag.downstream.mark_closed();
}

fn mark_downstream_transport_failed(context: &SharedProxyVmContext, message: &str) {
    let is_http3 = context
        .lock_downstream()
        .downstream_carrier_ref
        .as_ref()
        .is_some_and(|carrier_ref| carrier_ref.kind() == HttpCarrierKind::Http3);
    if is_http3 {
        return;
    }
    let mut transport = context.lock_transport();
    transport
        .tcp_dag
        .downstream
        .mark_failed(message.to_string());
    transport.tls_dag.downstream.mark_failed();
}

fn finalize_downstream_body_all_result(
    context: &SharedProxyVmContext,
    result: Result<Vec<u8>, VmError>,
) -> Result<Vec<u8>, VmError> {
    match result {
        Ok(bytes) => {
            mark_downstream_transport_closed(context);
            Ok(bytes)
        }
        Err(err) => {
            let message = err.to_string();
            mark_downstream_transport_failed(context, &message);
            Err(err)
        }
    }
}

fn finalize_downstream_body_read_result(
    context: &SharedProxyVmContext,
    inbound: &InboundRequestBodyState,
    result: Result<Vec<u8>, VmError>,
) -> Result<Vec<u8>, VmError> {
    match result {
        Ok(bytes) => {
            if bytes.is_empty() || inbound.is_drained() {
                mark_downstream_transport_closed(context);
            }
            Ok(bytes)
        }
        Err(err) => {
            let message = err.to_string();
            mark_downstream_transport_failed(context, &message);
            Err(err)
        }
    }
}

fn finalize_downstream_body_eof_result(
    context: &SharedProxyVmContext,
    result: Result<bool, VmError>,
) -> Result<bool, VmError> {
    match result {
        Ok(eof) => {
            if eof {
                mark_downstream_transport_closed(context);
            }
            Ok(eof)
        }
        Err(err) => {
            let message = err.to_string();
            mark_downstream_transport_failed(context, &message);
            Err(err)
        }
    }
}

pub(crate) async fn read_request_body_all(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let body: SharedInboundRequestBody = context.lock_downstream().inbound_request_body.clone();
    let mut inbound = body.lock().await;
    finalize_downstream_body_all_result(context, inbound.read_all().await)
}

pub(crate) async fn consume_request_body_all(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let body: SharedInboundRequestBody = context.lock_downstream().inbound_request_body.clone();
    let mut inbound = body.lock().await;
    finalize_downstream_body_all_result(context, inbound.read_all_and_consume().await)
}

pub(crate) async fn read_request_body_next_chunk(
    context: &SharedProxyVmContext,
    max_bytes: usize,
) -> Result<Vec<u8>, VmError> {
    let body: SharedInboundRequestBody = context.lock_downstream().inbound_request_body.clone();
    let mut inbound = body.lock().await;
    let result = inbound.read_next_chunk(max_bytes).await;
    finalize_downstream_body_read_result(context, &inbound, result)
}

pub(crate) async fn read_request_body_next_line(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let body: SharedInboundRequestBody = context.lock_downstream().inbound_request_body.clone();
    let mut inbound = body.lock().await;
    let result = inbound.read_next_line().await;
    finalize_downstream_body_read_result(context, &inbound, result)
}

pub(crate) async fn request_body_eof(context: &SharedProxyVmContext) -> Result<bool, VmError> {
    let body: SharedInboundRequestBody = context.lock_downstream().inbound_request_body.clone();
    let mut inbound = body.lock().await;
    finalize_downstream_body_eof_result(context, inbound.eof().await)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::{io, net::SocketAddr};

    use axum::{
        Router,
        body::{Body, Bytes, to_bytes},
        http::{HeaderMap, Request, Response},
        routing::any,
    };
    use futures_util::stream::try_unfold;
    use http_body_util::{BodyExt, StreamBody};
    use tokio::{
        sync::{Mutex as AsyncMutex, oneshot},
        time::{Duration, timeout},
    };

    use super::{
        HttpCarrierRef, HttpExchangeTransportState, HttpRequestContext,
        HttpUpstreamResponseSnapshot, ProxyVmContext, UpstreamResponseBodyState,
        header_content_length, response_from_upstream_snapshot,
        SharedProxyVmContext, allocate_outbound_exchange_handle, append_outbound_exchange_body,
        default_upstream_exchange_handle, ensure_outbound_exchange_response_started,
        outbound_exchange_exists, read_request_body_all, read_request_body_next_chunk,
        read_request_body_next_line, resolve_http_graph_response,
    };
    use crate::abi_impl::RateLimiterStore;
    use crate::abi_impl::http2::{
        Http2DownstreamStreamAttachment, Http2SendRequest, Http2StreamRef, Http2UpstreamMode,
        new_shared_http_upstream_sessions, send_request, total_active_streams,
    };
    use crate::abi_impl::transport::TlsFlowState;

    fn test_context() -> SharedProxyVmContext {
        Arc::new(ProxyVmContext::from_request_headers(
            HeaderMap::new(),
            Arc::new(Mutex::new(RateLimiterStore::new())),
        ))
    }

    fn test_context_with_request(body: Body, scheme: &str, host: &str) -> SharedProxyVmContext {
        Arc::new(ProxyVmContext::from_http_request(
            HttpRequestContext {
                request_id: String::new(),
                method: axum::http::Method::POST,
                path: "/".to_string(),
                query: String::new(),
                http_version: "1.1".to_string(),
                port: if scheme == "https" { 443 } else { 80 },
                scheme: scheme.to_string(),
                host: host.to_string(),
                client_ip: String::new(),
                body,
                headers: HeaderMap::new(),
            },
            Arc::new(Mutex::new(RateLimiterStore::new())),
        ))
    }

    async fn spawn_server(app: Router) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("listener should have addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server should run");
        });
        addr
    }

    #[test]
    fn dynamic_exchange_handles_are_sequential_and_isolated() {
        let context = test_context();

        let first = allocate_outbound_exchange_handle(&context).expect("first handle should exist");
        let second =
            allocate_outbound_exchange_handle(&context).expect("second handle should exist");

        assert_eq!(first, 2);
        assert_eq!(second, 3);
        assert!(outbound_exchange_exists(&context, first));
        assert!(outbound_exchange_exists(&context, second));

        append_outbound_exchange_body(&context, first, "alpha")
            .expect("first exchange write should succeed");
        append_outbound_exchange_body(&context, second, "beta")
            .expect("second exchange write should succeed");

        let guard = context.lock_exchanges();
        assert_eq!(
            guard.exchanges[&first].request.body_override.as_deref(),
            Some("alpha".as_bytes())
        );
        assert_eq!(
            guard.exchanges[&second].request.body_override.as_deref(),
            Some("beta".as_bytes())
        );
        assert_eq!(guard.exchanges[&first].request.target, None);
        assert_eq!(guard.exchanges[&second].request.target, None);
        assert!(!guard.exchanges[&first].response_ready());
        assert!(!guard.exchanges[&second].response_ready());
    }

    #[test]
    fn downstream_http2_attachment_updates_explicit_carrier_ref() {
        let mut context = ProxyVmContext::from_request_headers(
            HeaderMap::new(),
            Arc::new(Mutex::new(RateLimiterStore::new())),
        );
        assert_eq!(
            context.lock_downstream().downstream_carrier_ref.clone(),
            Some(HttpCarrierRef::DownstreamHttp1)
        );

        context.attach_downstream_http2_stream(&Http2DownstreamStreamAttachment {
            session_id: 41,
            stream_id: 9,
        });

        assert_eq!(
            context.lock_downstream().downstream_carrier_ref.clone(),
            Some(HttpCarrierRef::DownstreamHttp2Stream(Http2StreamRef {
                session_id: 41,
                stream_id: 9,
            }))
        );
    }

    #[test]
    fn exchange_transport_records_http2_stream_carrier_ref() {
        let mut transport = HttpExchangeTransportState::default();
        let carrier_ref = HttpCarrierRef::UpstreamHttp2Stream(Http2StreamRef {
            session_id: 12,
            stream_id: 7,
        });

        transport.mark_response_ready(axum::http::Version::HTTP_2, carrier_ref.clone());

        assert_eq!(transport.carrier_ref, Some(carrier_ref));
        assert_eq!(transport.http_version.as_deref(), Some("2"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn starting_one_dynamic_exchange_only_advances_that_exchange_dag() {
        let upstream_addr = spawn_server(Router::new().fallback(any(
            |request: Request<Body>| async move {
                let body = to_bytes(request.into_body(), usize::MAX)
                    .await
                    .expect("body should read");
                Body::from(format!("echo:{}", String::from_utf8_lossy(&body)))
            },
        )))
        .await;

        let mut context = test_context();
        {
            Arc::get_mut(&mut context)
                .expect("context should be uniquely owned")
                .attach_upstream_client(reqwest::Client::new());
        }

        let first = allocate_outbound_exchange_handle(&context).expect("first handle should exist");
        let second =
            allocate_outbound_exchange_handle(&context).expect("second handle should exist");
        append_outbound_exchange_body(&context, first, "one")
            .expect("first exchange write should succeed");

        {
            let mut guard = context.lock_exchanges();
            let exchange = guard
                .exchanges
                .get_mut(&first)
                .expect("first exchange should exist");
            exchange.request.target = Some(upstream_addr.to_string());
            exchange.transport.tcp_flow.configure();
            exchange
                .transport
                .tls_flow
                .observe_target(&upstream_addr.to_string());
        }

        let snapshot = ensure_outbound_exchange_response_started(&context, first)
            .await
            .expect("exchange should start");
        assert_eq!(snapshot.status, 200);

        let exchanges = context.lock_exchanges();
        assert!(exchanges.exchanges[&first].response_ready());
        assert!(
            exchanges.exchanges[&first]
                .transport
                .tcp_flow
                .is_connected()
        );
        assert!(!exchanges.exchanges[&first].transport.tls_flow.is_present());
        assert!(!exchanges.exchanges[&second].response_ready());
        assert!(
            !exchanges.exchanges[&second]
                .transport
                .tcp_flow
                .is_connected()
        );
        assert!(!exchanges.exchanges[&second].transport.tls_flow.is_present());
        assert!(
            !context
                .lock_transport()
                .tcp_dag
                .default_upstream
                .is_connected()
        );
        assert!(!exchanges.exchanges[&default_upstream_exchange_handle()].response_ready());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reading_full_downstream_body_marks_transport_closed() {
        let context =
            test_context_with_request(Body::from("payload"), "https", "origin.example.test:443");

        let body = read_request_body_all(&context)
            .await
            .expect("full body read should succeed");

        assert_eq!(body.as_slice(), b"payload");

        let transport = context.lock_transport();
        assert_eq!(transport.tcp_dag.downstream.phase_label(), "closed");
        assert_eq!(transport.tls_dag.downstream.phase_label(), "closed");
        assert_eq!(
            transport.tls_dag.downstream.server_name(),
            "origin.example.test"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reading_last_downstream_line_marks_transport_closed() {
        let context = test_context_with_request(Body::from("tail-without-newline"), "http", "");

        let line = read_request_body_next_line(&context)
            .await
            .expect("line read should succeed");

        assert_eq!(line.as_slice(), b"tail-without-newline");

        let transport = context.lock_transport();
        assert_eq!(transport.tcp_dag.downstream.phase_label(), "closed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn downstream_transport_marks_failed_when_request_body_read_errors() {
        let body = Body::new(StreamBody::new(futures_util::stream::once(async {
            Err::<hyper::body::Frame<Bytes>, io::Error>(io::Error::other("boom"))
        })));
        let context = test_context_with_request(body, "https", "origin.example.test:443");

        let err = read_request_body_next_chunk(&context, 16)
            .await
            .expect_err("body read should fail");

        assert!(
            err.to_string()
                .contains("failed to read inbound request body frame")
        );

        let transport = context.lock_transport();
        assert_eq!(transport.tcp_dag.downstream.phase_label(), "failed");
        assert!(
            transport
                .tcp_dag
                .downstream
                .failure_message()
                .contains("failed to read inbound request body frame")
        );
        assert_eq!(transport.tls_dag.downstream.phase_label(), "failed");
        assert_eq!(
            transport.tls_dag.downstream.server_name(),
            "origin.example.test"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resolve_http_graph_response_streams_upstream_body_without_waiting_for_eof() {
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let release = Arc::new(AsyncMutex::new(Some(release_rx)));
        let upstream_addr = spawn_server(Router::new().fallback(any({
            let release = release.clone();
            move |_request: Request<Body>| {
                let release = release.clone();
                async move {
                    let release_rx = release
                        .lock()
                        .await
                        .take()
                        .expect("release receiver should be available");
                    let stream = try_unfold(
                        (Some(Bytes::from_static(b"hello")), Some(release_rx)),
                        |(first, release_rx)| async move {
                            if let Some(chunk) = first {
                                return Ok::<_, io::Error>(Some((chunk, (None, release_rx))));
                            }
                            if let Some(release_rx) = release_rx {
                                let _ = release_rx.await;
                                return Ok(Some((Bytes::from_static(b"world"), (None, None))));
                            }
                            Ok(None)
                        },
                    );
                    Response::new(Body::from_stream(stream))
                }
            }
        })))
        .await;

        let mut context = test_context_with_request(Body::from("payload"), "http", "");
        Arc::get_mut(&mut context)
            .expect("context should be uniquely owned")
            .attach_upstream_client(reqwest::Client::new());
        {
            let target = format!("http://{upstream_addr}/stream");
            let mut exchanges = context.lock_exchanges();
            let exchange = exchanges
                .exchanges
                .get_mut(&default_upstream_exchange_handle())
                .expect("default upstream exchange should exist");
            exchange.request.target = Some(target.clone());
            exchange.transport.tcp_flow.configure();
            exchange.transport.tls_flow.observe_target(&target);
        }

        let resolved = timeout(Duration::from_millis(100), resolve_http_graph_response(&context))
            .await
            .expect("response resolution should not wait for the full upstream body");

        let mut body = resolved.response.into_body();
        let first = timeout(Duration::from_millis(100), body.frame())
            .await
            .expect("first upstream chunk should arrive promptly")
            .expect("body should yield a frame")
            .expect("frame should be successful")
            .into_data()
            .expect("frame should contain data");
        assert_eq!(first.as_ref(), b"hello");

        assert!(
            timeout(Duration::from_millis(50), body.frame()).await.is_err(),
            "second upstream chunk should still be pending before release"
        );

        release_tx
            .send(())
            .expect("release signal should be deliverable");

        let second = timeout(Duration::from_millis(100), body.frame())
            .await
            .expect("second upstream chunk should arrive after release")
            .expect("body should yield a second frame")
            .expect("frame should be successful")
            .into_data()
            .expect("frame should contain data");
        assert_eq!(second.as_ref(), b"world");
        assert!(
            timeout(Duration::from_millis(100), body.frame())
                .await
                .expect("body eof should be observable")
                .is_none()
        );
    }

    #[cfg(all(feature = "http2", feature = "tls"))]
    #[tokio::test(flavor = "current_thread")]
    async fn known_length_streaming_response_retires_upstream_http2_stream_without_eof_poll() {
        use std::convert::Infallible;

        use http_body_util::Full;
        use hyper::{Response as HyperResponse, body::Incoming, service::service_fn};
        use hyper_util::rt::TokioIo;
        use rcgen::generate_simple_self_signed;
        use reqwest::Method;
        use tokio_rustls::{
            TlsAcceptor,
            rustls::{
                ServerConfig,
                pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
            },
        };

        let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();

        let cert =
            generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
                .expect("test cert should build");
        let mut server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(
                    cert.serialize_der()
                        .expect("cert der should serialize"),
                )],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                    cert.serialize_private_key_der(),
                )),
            )
            .expect("server config should build");
        server_config.alpn_protocols = vec![b"h2".to_vec()];

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("listener should have addr");
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let server_handle = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept should succeed");
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let tls_stream = acceptor.accept(stream).await.expect("tls should accept");
                    let service = service_fn(|_request: hyper::Request<Incoming>| async move {
                        Ok::<_, Infallible>(HyperResponse::new(Full::new(Bytes::from_static(
                            b"hello",
                        ))))
                    });
                    let builder = hyper::server::conn::http2::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    );
                    let _ = builder
                        .serve_connection(TokioIo::new(tls_stream), service)
                        .await;
                });
            }
        });

        let sessions = new_shared_http_upstream_sessions(8);
        let origin = format!("https://127.0.0.1:{}", addr.port());
        let upstream_url = format!("{origin}/");
        let mut tls_flow = TlsFlowState::default();
        tls_flow.observe_target(&origin);
        tls_flow.set_verify_peer(false);
        tls_flow.set_verify_hostname(false);
        tls_flow.set_desired_alpn(vec!["h2".to_string(), "http/1.1".to_string()]);

        let started = send_request(Http2SendRequest {
            sessions: &sessions,
            exchange_handle: default_upstream_exchange_handle(),
            target: &origin,
            upstream_url: &upstream_url,
            mode: Http2UpstreamMode::AutomaticTls,
            tls_flow: &tls_flow,
            method: Method::GET,
            headers: HeaderMap::new(),
            request_body: Vec::new(),
        })
        .await
        .expect("http2 request should start");

        let carrier_ref = HttpCarrierRef::UpstreamHttp2Stream(started.stream_ref);
        let response_headers = started.response.headers().clone();
        let content_length = header_content_length(&response_headers);
        let response_status = started.response.status().as_u16();
        let snapshot = HttpUpstreamResponseSnapshot {
            status: response_status,
            headers: response_headers,
            http_version: "2".to_string(),
            carrier_kind: carrier_ref.kind(),
            carrier_ref,
            body: Arc::new(tokio::sync::Mutex::new(UpstreamResponseBodyState::from_hyper(
                started.response.into_body(),
                Some(started.body_tracker),
                content_length,
            ))),
        };

        let response = response_from_upstream_snapshot(snapshot, HeaderMap::new(), None)
            .await
            .expect("response should build");
        let mut body = response.into_body();
        let first = body
            .frame()
            .await
            .expect("body should yield a frame")
            .expect("frame should be successful")
            .into_data()
            .expect("frame should contain data");
        assert_eq!(first.as_ref(), b"hello");

        drop(body);
        tokio::task::yield_now().await;

        assert_eq!(
            total_active_streams(&sessions),
            0,
            "known-length streamed upstream response should retire the http2 stream without a trailing eof poll"
        );

        server_handle.abort();
    }
}
