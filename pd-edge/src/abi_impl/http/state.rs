#![cfg_attr(not(feature = "http"), allow(dead_code))]

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    hash::Hash,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context as TaskContext, Poll},
    time::Instant,
};

use axum::{
    body::{Body, Bytes},
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode, Uri, Version,
        header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST, TRANSFER_ENCODING},
        uri::Authority,
    },
};
use bytes::BytesMut;
use futures_util::{Stream, stream::try_unfold};
use http_body_util::{BodyExt, StreamBody};
#[cfg(feature = "http3")]
use hyper::body::Buf;
use hyper::body::{Frame, SizeHint};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use parking_lot::Mutex as ParkingMutex;
use tokio::io::copy_bidirectional;
use tokio::sync::{Notify, mpsc, oneshot};
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
        TcpTransportDag, TlsFlowState, TlsTransportDag, UdpSocketState, tls_session_cache_key,
    },
    websocket::WebSocketConnectionState,
};
use super::fast_path::outbound_http1_fast_path_eligible;
use super::outbound_http1::{
    OutboundHttp1ForwardBody, OutboundHttp1ForwardResponse, OutboundHttp1RequestBody,
    OutboundHttp1RequestHeaders, OutboundHttp1Scheme, OutboundHttp1Target, PlainHttp1ResponseBody,
    PlainHttp1SenderLease, SerializedOutboundHttp1Request, SharedPlainHttp1SenderPool,
    forward_serialized_via_sender_pool, new_shared_plain_http1_sender_pool,
    serialize_request_head_parts_into,
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
use crate::lock_metrics::{self, LockMetricKey, ProfiledMutexGuard};

mod body_io;
mod downstream_response;
mod request_context;
mod upstream;
mod upstream_body;

use body_io::{
    BufferedByteSource, BufferedByteSourceFuture, BufferedByteStream, BufferedByteStreamPull,
};
pub(crate) use body_io::{
    InboundRequestBodyState, consume_request_body_all, read_request_body_all,
    read_request_body_next_chunk, read_request_body_next_line, request_body_eof,
};
pub(crate) use downstream_response::{
    DownstreamResponseStreamWriteMode, HttpResponseOutputNode, append_response_output_body_bytes,
    current_upstream_latency_ms, downstream_snapshot_response_head,
    explicit_snapshot_downstream_response_head, finish_downstream_response_stream,
    materialize_downstream_response_body_source, merge_headers, outbound_exchange_latency_ms,
    resolve_committed_http_graph_response, resolve_http_graph_response,
    response_from_upstream_snapshot, start_downstream_response_stream,
    sync_response_output_body_headers, text_response, write_downstream_response_stream_bytes,
};
pub(crate) use request_context::{
    DownstreamConnectionMetadata, DownstreamHttpListenerGoal, HttpRequestHead, LazyRequestId,
    RequestPortField, RequestStringField, build_downstream_http_request_context,
    build_downstream_http_request_context_from_components, http_version_label,
};
pub use request_context::{HttpRequestContext, LazyHttpHeaders};
#[allow(unused_imports)]
pub(crate) use upstream::{
    DefaultUpstreamRequestSnapshot, DownstreamHttpBodyPassthrough, Http1DownstreamResolution,
    ResolvedHttpGraphResponse, ResolvedNativeHttp1DownstreamResponse,
    ResolvedNativeLocalHttp1DownstreamResponse, ResolvedSnapshotHttp1DownstreamResponse,
    SnapshotHttp1DownstreamHeaders, build_configured_upstream_url,
    ensure_outbound_exchange_response_started, ensure_upstream_response_started,
    header_content_length, is_hop_by_hop_header, outbound_exchange_response_available,
    outbound_exchange_response_eof, read_downstream_response_trailers,
    read_outbound_exchange_response_all, read_outbound_exchange_response_next_chunk,
    read_outbound_exchange_response_next_line, read_outbound_exchange_response_trailers,
    read_upstream_response_all, read_upstream_response_next_chunk,
    read_upstream_response_next_line, read_upstream_response_trailers,
    resolve_http1_downstream_response, start_native_default_upstream_http_forward_response,
    try_resolve_native_http1_downstream_response, try_resolve_snapshot_http1_downstream_response,
    try_take_native_local_http1_downstream_response, upstream_response_available,
    upstream_response_eof,
};
use upstream::{
    NativeDefaultUpstreamForwardResponse, UpstreamResponseStartError,
    response_from_started_upstream_response, start_outbound_exchange_response,
    start_upstream_response, try_resolve_native_default_upstream_http_forward_response,
    try_resolve_ready_or_pending_native_default_upstream_forward_response,
};
use upstream_body::{SharedHttpHeaders, SharedUpstreamResponseBody};
use upstream_body::{StreamingUpstreamResponseBodyState, UpstreamResponseBodyState};

#[cfg(feature = "webrtc")]
use std::sync::MutexGuard;

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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum HttpUpstreamScheme {
    #[default]
    Http,
    Https,
}

impl HttpUpstreamScheme {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, VmError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "http" => Ok(Self::Http),
            "https" => Ok(Self::Https),
            _ => Err(VmError::HostError(format!(
                "invalid upstream scheme '{value}'; expected http or https",
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParsedHttpUpstreamTarget {
    pub(crate) scheme: HttpUpstreamScheme,
    pub(crate) host: String,
    pub(crate) host_arc: Arc<str>,
    pub(crate) port: u16,
    pub(crate) host_header: String,
    pub(crate) authority: Arc<str>,
    pub(crate) plain_http1_pool_key: Arc<str>,
    pub(crate) target: String,
    pub(crate) inherits_request_path: bool,
}

fn format_upstream_authority(host: &str, port: u16) -> String {
    let host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn build_upstream_origin(
    scheme: HttpUpstreamScheme,
    host: &str,
    port: u16,
) -> Result<ParsedHttpUpstreamTarget, VmError> {
    if port == 0 {
        return Err(VmError::HostError(
            "upstream port must be between 1 and 65535".to_string(),
        ));
    }

    let authority = format_upstream_authority(host, port);
    if authority.contains('@') {
        return Err(VmError::HostError(format!(
            "invalid upstream target host='{host}' port={port}",
        )));
    }
    let parsed_authority = authority.parse::<Authority>().map_err(|_| {
        VmError::HostError(format!("invalid upstream target host='{host}' port={port}",))
    })?;
    let target = format!("{}://{authority}", scheme.as_str());
    let parsed_host = parsed_authority.host().trim_matches(['[', ']']);
    if parsed_host.is_empty() {
        return Err(VmError::HostError(format!(
            "invalid upstream target host='{host}' port={port}",
        )));
    }

    Ok(ParsedHttpUpstreamTarget {
        scheme,
        host: parsed_host.to_string(),
        host_arc: Arc::from(parsed_host),
        port: parsed_authority
            .port_u16()
            .expect("http upstream origin should have an explicit port"),
        host_header: authority,
        authority: Arc::from(parsed_authority.as_str()),
        plain_http1_pool_key: Arc::from(format!("http://{}", parsed_authority.as_str())),
        target,
        inherits_request_path: true,
    })
}

#[derive(Clone, Debug)]
pub(crate) struct HttpOutboundRequestNode {
    inherits_request_head: bool,
    pub(crate) method: Method,
    pub(crate) path: String,
    pub(crate) query: String,
    pub(crate) headers: HeaderMap,
    inherited_header_overrides: HeaderMap,
    pub(crate) body_override: Option<Vec<u8>>,
    pub(crate) target: Option<String>,
    pub(crate) target_host: Option<String>,
    target_host_arc: Option<Arc<str>>,
    pub(crate) target_port: Option<u16>,
    pub(crate) target_host_header: Option<String>,
    target_authority: Option<Arc<str>>,
    target_plain_http1_pool_key: Option<Arc<str>>,
    pub(crate) target_inherits_request_path: bool,
    pub(crate) target_scheme: HttpUpstreamScheme,
    pub(crate) version_preference: HttpVersionPreference,
}

impl HttpOutboundRequestNode {
    fn new() -> Self {
        Self {
            inherits_request_head: false,
            method: Method::GET,
            path: "/".to_string(),
            query: String::new(),
            headers: HeaderMap::new(),
            inherited_header_overrides: HeaderMap::new(),
            body_override: None,
            target: None,
            target_host: None,
            target_host_arc: None,
            target_port: None,
            target_host_header: None,
            target_authority: None,
            target_plain_http1_pool_key: None,
            target_inherits_request_path: false,
            target_scheme: HttpUpstreamScheme::Http,
            version_preference: HttpVersionPreference::Auto,
        }
    }

    pub(crate) fn default_upstream() -> Self {
        Self {
            inherits_request_head: true,
            method: Method::GET,
            path: String::new(),
            query: String::new(),
            headers: HeaderMap::new(),
            inherited_header_overrides: HeaderMap::new(),
            body_override: None,
            target: None,
            target_host: None,
            target_host_arc: None,
            target_port: None,
            target_host_header: None,
            target_authority: None,
            target_plain_http1_pool_key: None,
            target_inherits_request_path: false,
            target_scheme: HttpUpstreamScheme::Http,
            version_preference: HttpVersionPreference::Auto,
        }
    }

    fn reset_inherited_request_head(&mut self) {
        self.inherits_request_head = true;
        self.method = Method::GET;
        self.path.clear();
        self.query.clear();
        self.headers.clear();
        self.inherited_header_overrides.clear();
    }

    pub(crate) fn set_target_host_port(&mut self, host: &str, port: u16) -> Result<(), VmError> {
        let parsed = build_upstream_origin(self.target_scheme, host, port)?;
        self.target = Some(parsed.target);
        self.target_host = Some(parsed.host);
        self.target_host_arc = Some(parsed.host_arc);
        self.target_port = Some(parsed.port);
        self.target_host_header = Some(parsed.host_header);
        self.target_authority = Some(parsed.authority);
        self.target_plain_http1_pool_key = Some(parsed.plain_http1_pool_key);
        self.target_inherits_request_path = true;
        Ok(())
    }

    pub(crate) fn set_target_scheme(&mut self, scheme: HttpUpstreamScheme) -> Result<(), VmError> {
        self.target_scheme = scheme;
        if let (Some(host), Some(port)) = (self.target_host.as_deref(), self.target_port) {
            let parsed = build_upstream_origin(scheme, host, port)?;
            self.target = Some(parsed.target);
            self.target_host_arc = Some(parsed.host_arc);
            self.target_host_header = Some(parsed.host_header);
            self.target_authority = Some(parsed.authority);
            self.target_plain_http1_pool_key = Some(parsed.plain_http1_pool_key);
        }
        Ok(())
    }

    pub(crate) fn materialize_inherited_request_head(&mut self, request_head: &HttpRequestHead) {
        if !self.inherits_request_head {
            return;
        }
        self.method = request_head.method().clone();
        self.path = request_head.path().to_string();
        self.query = request_head.query().to_string();
        self.headers = request_head.headers().clone();
        merge_headers(&mut self.headers, &self.inherited_header_overrides);
        self.inherited_header_overrides.clear();
        self.inherits_request_head = false;
    }

    pub(crate) fn insert_header(&mut self, name: HeaderName, value: HeaderValue) {
        if self.inherits_request_head {
            self.inherited_header_overrides.insert(name, value);
        } else {
            self.headers.insert(name, value);
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct HttpVmTouchState {
    pub(crate) request_body_read: bool,
    pub(crate) response_body_read: bool,
    pub(crate) response_body_mutated: bool,
    pub(crate) response_headers_mutated: bool,
    pub(crate) response_status_mutated: bool,
    pub(crate) exchange_response_body_reads: HashSet<i64>,
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
    pub(crate) headers: SharedHttpHeaders,
    pub(crate) http_version: &'static str,
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
            request: HttpOutboundRequestNode::new(),
            response: HttpUpstreamResponseNode::NotStarted,
            transport: HttpExchangeTransportState::default(),
            websocket_dag: WebSocketConnectionState::default(),
            upstream_latency_ms: 0,
        }
    }

    fn default_upstream() -> Self {
        Self {
            request: HttpOutboundRequestNode::default_upstream(),
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
    plain_http1_sender_pool: Option<SharedPlainHttp1SenderPool>,
    upstream_http_reuse_entries: usize,
    tls_session_cache: Option<SharedTlsSessionCache>,
    upstream_http_sessions: Option<SharedHttpUpstreamSessions>,
    upstream_http3_sessions: Option<SharedHttp3UpstreamSessions>,
    downstream_http_sessions: Option<http2::SharedHttpDownstreamSessions>,
    #[cfg(feature = "tls")]
    downstream_tls_termination: Option<Arc<tokio_rustls::rustls::ServerConfig>>,
    rate_limiter: SharedRateLimiter,
}

pub(crate) type SharedRuntimeServices = Arc<RuntimeServices>;

impl RuntimeServices {
    fn new(rate_limiter: SharedRateLimiter) -> Self {
        Self {
            plain_http1_sender_pool: None,
            upstream_http_reuse_entries: 0,
            tls_session_cache: None,
            upstream_http_sessions: None,
            upstream_http3_sessions: None,
            downstream_http_sessions: None,
            #[cfg(feature = "tls")]
            downstream_tls_termination: None,
            rate_limiter,
        }
    }

    pub(crate) fn plain_http1_sender_pool(&self) -> Option<SharedPlainHttp1SenderPool> {
        self.plain_http1_sender_pool.clone()
    }

    pub(crate) fn upstream_http_reuse_entries(&self) -> usize {
        self.upstream_http_reuse_entries
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

pub(crate) fn new_shared_runtime_services(
    rate_limiter: SharedRateLimiter,
) -> SharedRuntimeServices {
    Arc::new(RuntimeServices::new(rate_limiter))
}

pub(crate) struct HttpPlaneRuntimeServicesConfig {
    pub(crate) rate_limiter: SharedRateLimiter,
    pub(crate) plain_http1_sender_pool: SharedPlainHttp1SenderPool,
    pub(crate) upstream_http_reuse_entries: usize,
    pub(crate) tls_session_cache: SharedTlsSessionCache,
    pub(crate) upstream_http_sessions: SharedHttpUpstreamSessions,
    pub(crate) upstream_http3_sessions: SharedHttp3UpstreamSessions,
    pub(crate) downstream_http_sessions: http2::SharedHttpDownstreamSessions,
}

pub(crate) fn new_shared_http_plane_runtime_services(
    config: HttpPlaneRuntimeServicesConfig,
) -> SharedRuntimeServices {
    Arc::new(RuntimeServices {
        plain_http1_sender_pool: Some(config.plain_http1_sender_pool),
        upstream_http_reuse_entries: config.upstream_http_reuse_entries,
        tls_session_cache: Some(config.tls_session_cache),
        upstream_http_sessions: Some(config.upstream_http_sessions),
        upstream_http3_sessions: Some(config.upstream_http3_sessions),
        downstream_http_sessions: Some(config.downstream_http_sessions),
        #[cfg(feature = "tls")]
        downstream_tls_termination: None,
        rate_limiter: config.rate_limiter,
    })
}

#[derive(Debug)]
pub(crate) struct DownstreamState {
    #[cfg_attr(not(feature = "websocket"), allow(dead_code))]
    pub(crate) downstream_websocket: WebSocketConnectionState,
    downstream_websocket_initialized: bool,
    pub(crate) response_output: HttpResponseOutputNode,
    pub(crate) downstream_carrier_ref: Option<HttpCarrierRef>,
    pub(crate) downstream_http1_upgrade: Option<DownstreamHttp1Upgrade>,
    pub(crate) post_response_plan: Option<DownstreamPostResponsePlan>,
    pub(crate) native_default_upstream_http_forward: bool,
    native_default_upstream_forward_request: Option<DefaultUpstreamRequestSnapshot>,
    native_default_upstream_forward_response: Option<NativeDefaultUpstreamForwardResponse>,
    inline_http_response_sender: Option<InlineDownstreamHttpResponseSender>,
    pub(crate) vm_touches: HttpVmTouchState,
}

impl DownstreamState {
    fn from_http_request(_request_head: &HttpRequestHead) -> Self {
        Self {
            downstream_websocket: WebSocketConnectionState::default(),
            downstream_websocket_initialized: false,
            response_output: HttpResponseOutputNode::default(),
            downstream_carrier_ref: Some(HttpCarrierRef::DownstreamHttp1),
            downstream_http1_upgrade: None,
            post_response_plan: None,
            native_default_upstream_http_forward: false,
            native_default_upstream_forward_request: None,
            native_default_upstream_forward_response: None,
            inline_http_response_sender: None,
            vm_touches: HttpVmTouchState::default(),
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

    #[cfg_attr(not(feature = "http3"), allow(dead_code))]
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
            downstream_websocket: WebSocketConnectionState::default(),
            downstream_websocket_initialized: true,
            response_output: HttpResponseOutputNode::default(),
            downstream_carrier_ref: None,
            downstream_http1_upgrade: None,
            post_response_plan: None,
            native_default_upstream_http_forward: false,
            native_default_upstream_forward_request: None,
            native_default_upstream_forward_response: None,
            inline_http_response_sender: None,
            vm_touches: HttpVmTouchState::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ExchangeRegistry {
    pub(crate) next_outbound_exchange_handle: i64,
    pub(crate) exchanges: HashMap<i64, HttpOutboundExchangeState>,
    initialized_from_request: bool,
}

impl ExchangeRegistry {
    fn from_http_request(_request_head: &HttpRequestHead) -> Self {
        Self {
            next_outbound_exchange_handle: FIRST_DYNAMIC_EXCHANGE_HANDLE,
            exchanges: HashMap::new(),
            initialized_from_request: false,
        }
    }

    fn seed_from_http_request(&mut self) {
        if self.initialized_from_request {
            return;
        }
        self.exchanges.insert(
            DEFAULT_UPSTREAM_EXCHANGE_HANDLE,
            HttpOutboundExchangeState::default_upstream(),
        );
        self.initialized_from_request = true;
    }
}

#[derive(Debug)]
pub(crate) struct LazyHandleMap<K, V>(Option<HashMap<K, V>>);

impl<K, V> LazyHandleMap<K, V>
where
    K: Eq + Hash,
{
    pub(crate) fn get(&self, key: &K) -> Option<&V> {
        self.0.as_ref().and_then(|map| map.get(key))
    }

    pub(crate) fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.0.as_mut().and_then(|map| map.get_mut(key))
    }

    pub(crate) fn contains_key(&self, key: &K) -> bool {
        self.0.as_ref().is_some_and(|map| map.contains_key(key))
    }

    pub(crate) fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.0.get_or_insert_with(HashMap::new).insert(key, value)
    }

    pub(crate) fn get_or_insert_with(&mut self, key: K, default: impl FnOnce() -> V) -> &mut V {
        self.0
            .get_or_insert_with(HashMap::new)
            .entry(key)
            .or_insert_with(default)
    }

    pub(crate) fn remove(&mut self, key: &K) -> Option<V> {
        self.0.as_mut().and_then(|map| map.remove(key))
    }
}

impl<K, V> Default for LazyHandleMap<K, V> {
    fn default() -> Self {
        Self(None)
    }
}

#[derive(Debug)]
pub(crate) struct TransportState {
    pub(crate) tcp_dag: TcpTransportDag,
    pub(crate) tls_dag: TlsTransportDag,
    downstream_request_seed: Option<DownstreamTransportRequestSeed>,
    initialized_from_request: bool,
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
    pub(crate) tcp_streams: LazyHandleMap<i64, TcpSocketState>,
    pub(crate) tcp_stream_ios: LazyHandleMap<i64, SharedTcpStreamIo>,
    #[cfg(feature = "tls")]
    pub(crate) dynamic_tls_sessions: LazyHandleMap<i64, TlsFlowState>,
    #[cfg(feature = "tls")]
    pub(crate) dynamic_tls_session_ios: LazyHandleMap<i64, SharedTlsStreamIo>,
    pub(crate) default_upstream_udp_socket: UdpSocketState,
    pub(crate) default_upstream_udp_io: Option<SharedUdpSocketIo>,
    pub(crate) next_udp_socket_handle: i64,
    pub(crate) udp_sockets: LazyHandleMap<i64, UdpSocketState>,
    pub(crate) udp_socket_ios: LazyHandleMap<i64, SharedUdpSocketIo>,
}

#[derive(Clone, Debug)]
struct DownstreamTransportRequestSeed {
    scheme: RequestStringField,
    host: RequestStringField,
    http_version: RequestStringField,
}

impl TransportState {
    fn from_http_request(request_head: &HttpRequestHead) -> Self {
        Self {
            tcp_dag: TcpTransportDag::default(),
            tls_dag: TlsTransportDag::default(),
            downstream_request_seed: Some(DownstreamTransportRequestSeed {
                scheme: request_head.scheme_field().clone(),
                host: request_head.host_field().clone(),
                http_version: request_head.http_version_field().clone(),
            }),
            initialized_from_request: false,
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
            tcp_streams: LazyHandleMap::default(),
            tcp_stream_ios: LazyHandleMap::default(),
            #[cfg(feature = "tls")]
            dynamic_tls_sessions: LazyHandleMap::default(),
            #[cfg(feature = "tls")]
            dynamic_tls_session_ios: LazyHandleMap::default(),
            default_upstream_udp_socket: UdpSocketState::default(),
            default_upstream_udp_io: None,
            next_udp_socket_handle: FIRST_DYNAMIC_UDP_SOCKET_HANDLE,
            udp_sockets: LazyHandleMap::default(),
            udp_socket_ios: LazyHandleMap::default(),
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
            downstream_request_seed: None,
            initialized_from_request: true,
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
            tcp_streams: LazyHandleMap::default(),
            tcp_stream_ios: LazyHandleMap::default(),
            #[cfg(feature = "tls")]
            dynamic_tls_sessions: LazyHandleMap::default(),
            #[cfg(feature = "tls")]
            dynamic_tls_session_ios: LazyHandleMap::default(),
            default_upstream_udp_socket: UdpSocketState::default(),
            default_upstream_udp_io: None,
            next_udp_socket_handle: FIRST_DYNAMIC_UDP_SOCKET_HANDLE,
            udp_sockets: LazyHandleMap::default(),
            udp_socket_ios: LazyHandleMap::default(),
        }
    }

    fn seed_from_http_request(&mut self) {
        if self.initialized_from_request {
            return;
        }
        self.tcp_dag = TcpTransportDag::for_http_request();
        if let Some(seed) = &self.downstream_request_seed {
            self.tls_dag = TlsTransportDag::for_http_request(
                seed.scheme.as_str(),
                seed.host.as_str(),
                seed.http_version.as_str(),
            );
        } else {
            self.tls_dag = TlsTransportDag::default();
        }
        self.initialized_from_request = true;
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
    pub(crate) proxy_stream_handles: LazyHandleMap<i64, ProxyByteStreamState>,
}

impl Default for ProxyStreamRegistry {
    fn default() -> Self {
        Self {
            next_proxy_stream_handle: FIRST_PROXY_STREAM_HANDLE,
            proxy_stream_handles: LazyHandleMap::default(),
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
    inbound_request_body: tokio::sync::Mutex<InboundRequestBodyState>,
    services: SharedRuntimeServices,
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
        Self::from_http_request_with_services(request, new_shared_runtime_services(rate_limiter))
    }

    pub(crate) fn from_http_request_with_services(
        request: HttpRequestContext,
        services: SharedRuntimeServices,
    ) -> Self {
        let HttpRequestContext {
            request_id,
            method,
            path,
            query,
            http_version,
            port,
            scheme,
            host,
            client_ip,
            body,
            headers,
        } = request;
        let request_head = HttpRequestHead {
            request_id,
            method,
            path,
            query,
            http_version,
            port,
            scheme,
            host,
            client_ip,
            headers,
        };
        Self {
            inbound_request_body: tokio::sync::Mutex::new(InboundRequestBodyState::new(body)),
            downstream: Mutex::new(DownstreamState::from_http_request(&request_head)),
            exchanges: Mutex::new(ExchangeRegistry::from_http_request(&request_head)),
            transport: Mutex::new(TransportState::from_http_request(&request_head)),
            request_head: Mutex::new(request_head),
            services,
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
        Self::from_request_headers_with_services(
            request_headers,
            new_shared_runtime_services(rate_limiter),
        )
    }

    pub(crate) fn from_request_headers_with_services(
        request_headers: HeaderMap,
        services: SharedRuntimeServices,
    ) -> Self {
        Self::from_http_request_with_services(
            HttpRequestContext {
                request_id: LazyRequestId::from_string(String::new()),
                method: Method::GET,
                path: RequestStringField::Static("/".to_string()),
                query: RequestStringField::Static(String::new()),
                http_version: RequestStringField::Static("1.1".to_string()),
                port: RequestPortField::Static(80),
                scheme: RequestStringField::Static("http".to_string()),
                host: RequestStringField::Static(String::new()),
                client_ip: RequestStringField::Static(String::new()),
                body: Body::empty(),
                headers: request_headers.into(),
            },
            services,
        )
    }

    pub fn from_downstream_tcp_stream(
        stream: tokio::net::TcpStream,
        request_id: String,
        rate_limiter: SharedRateLimiter,
    ) -> Result<Self, VmError> {
        Self::from_downstream_tcp_stream_with_services(
            stream,
            request_id,
            new_shared_runtime_services(rate_limiter),
        )
    }

    pub(crate) fn from_downstream_tcp_stream_with_services(
        stream: tokio::net::TcpStream,
        request_id: String,
        services: SharedRuntimeServices,
    ) -> Result<Self, VmError> {
        let local_addr = stream.local_addr().map_err(|err| {
            VmError::HostError(format!("failed to read downstream local address: {err}"))
        })?;
        let peer_addr = stream.peer_addr().map_err(|err| {
            VmError::HostError(format!("failed to read downstream peer address: {err}"))
        })?;
        let io = Arc::new(tokio::sync::Mutex::new(Some(stream)));
        let request_head = HttpRequestHead {
            request_id: LazyRequestId::from_string(request_id),
            method: Method::GET,
            path: RequestStringField::Static("/".to_string()),
            query: RequestStringField::Static(String::new()),
            http_version: RequestStringField::Static(String::new()),
            port: RequestPortField::Static(peer_addr.port()),
            scheme: RequestStringField::Static("tcp".to_string()),
            host: RequestStringField::Static(peer_addr.to_string()),
            client_ip: RequestStringField::Static(peer_addr.ip().to_string()),
            headers: HeaderMap::new().into(),
        };
        Ok(Self {
            inbound_request_body: tokio::sync::Mutex::new(InboundRequestBodyState::new(
                Body::empty(),
            )),
            downstream: Mutex::new(DownstreamState::for_transport_connection()),
            exchanges: Mutex::new(ExchangeRegistry::from_http_request(&request_head)),
            transport: Mutex::new(TransportState::from_downstream_tcp_stream(
                io, local_addr, peer_addr,
            )),
            request_head: Mutex::new(request_head),
            services,
            #[cfg(feature = "webrtc")]
            webrtc: Mutex::new(WebRtcRegistry::default()),
            proxy: Mutex::new(ProxyStreamRegistry::default()),
            edge_io: Mutex::new(EdgeIoRegistry::default()),
        })
    }

    fn services_mut(&mut self) -> &mut RuntimeServices {
        Arc::make_mut(&mut self.services)
    }

    pub(crate) fn attach_runtime_services(&mut self, services: SharedRuntimeServices) {
        self.services = services;
    }

    pub fn attach_upstream_http1_support(&mut self, reuse_entries: usize) {
        let services = self.services_mut();
        if services.plain_http1_sender_pool.is_none() {
            services.plain_http1_sender_pool = Some(new_shared_plain_http1_sender_pool());
        }
        if services.upstream_http_reuse_entries == 0 {
            services.upstream_http_reuse_entries = reuse_entries.max(1);
        }
    }

    #[cfg(feature = "tls")]
    pub(crate) fn attach_downstream_tls_termination(
        &mut self,
        server_config: Arc<tokio_rustls::rustls::ServerConfig>,
    ) {
        self.services_mut().downstream_tls_termination = Some(server_config);
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

    #[cfg_attr(not(feature = "http3"), allow(dead_code))]
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

    pub(crate) fn attach_downstream_connection_metadata(
        &mut self,
        metadata: &DownstreamConnectionMetadata,
    ) {
        let transport = self
            .transport
            .get_mut()
            .expect("transport state lock poisoned");
        transport.seed_from_http_request();
        transport.downstream_local_addr = Some(metadata.local_addr);
        transport.downstream_peer_addr = Some(metadata.peer_addr);
    }

    pub(crate) fn set_downstream_listener_goal(&mut self, goal: DownstreamHttpListenerGoal) {
        let transport = self
            .transport
            .get_mut()
            .expect("transport state lock poisoned");
        transport.seed_from_http_request();
        transport.downstream_listener_goal = goal;
    }

    pub(crate) fn with_request_head<T>(&self, read: impl FnOnce(&HttpRequestHead) -> T) -> T {
        let request_head = self.lock_request_head();
        read(&request_head)
    }

    pub(crate) fn services(&self) -> &RuntimeServices {
        self.services.as_ref()
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

    pub(crate) fn note_downstream_request_body_read(&self) {
        self.lock_downstream().vm_touches.request_body_read = true;
    }

    pub(crate) fn note_downstream_response_body_read(&self) {
        self.lock_downstream().vm_touches.response_body_read = true;
    }

    pub(crate) fn note_downstream_response_body_mutated(&self) {
        self.lock_downstream().vm_touches.response_body_mutated = true;
    }

    pub(crate) fn note_downstream_response_headers_mutated(&self) {
        self.lock_downstream().vm_touches.response_headers_mutated = true;
    }

    pub(crate) fn note_exchange_response_body_read(&self, exchange: i64) {
        self.lock_downstream()
            .vm_touches
            .exchange_response_body_reads
            .insert(exchange);
    }

    fn ensure_downstream_response_head_mutable(
        downstream: &DownstreamState,
    ) -> Result<(), VmError> {
        if downstream.response_output.stream_committed() {
            return Err(VmError::HostError(
                "downstream response headers and status are immutable after response streaming begins"
                    .to_string(),
            ));
        }
        Ok(())
    }

    pub(crate) fn insert_downstream_response_header(
        &self,
        name: HeaderName,
        value: HeaderValue,
    ) -> Result<(), VmError> {
        let mut downstream = self.lock_downstream();
        downstream.vm_touches.response_headers_mutated = true;
        Self::ensure_downstream_response_head_mutable(&downstream)?;
        if downstream.post_response_plan.is_none()
            && !downstream.response_output.has_local_body()
            && downstream.response_output.body_source_exchange.is_none()
            && downstream.response_output.status.is_none()
            && downstream.response_output.headers.is_empty()
            && let Some(native_response) =
                downstream.native_default_upstream_forward_response.as_mut()
        {
            native_response.headers.insert(name, value);
            return Ok(());
        }
        downstream.response_output.headers.insert(name, value);
        Ok(())
    }

    pub(crate) fn insert_downstream_response_headers(
        &self,
        headers: HeaderMap,
    ) -> Result<(), VmError> {
        let mut downstream = self.lock_downstream();
        downstream.vm_touches.response_headers_mutated = true;
        Self::ensure_downstream_response_head_mutable(&downstream)?;
        if downstream.post_response_plan.is_none()
            && !downstream.response_output.has_local_body()
            && downstream.response_output.body_source_exchange.is_none()
            && downstream.response_output.status.is_none()
            && downstream.response_output.headers.is_empty()
            && let Some(native_response) =
                downstream.native_default_upstream_forward_response.as_mut()
        {
            for (name, value) in headers {
                if let Some(name) = name {
                    native_response.headers.insert(name, value);
                }
            }
            return Ok(());
        }
        for (name, value) in headers {
            if let Some(name) = name {
                downstream.response_output.headers.insert(name, value);
            }
        }
        Ok(())
    }

    pub(crate) fn set_downstream_response_status(&self, status: u16) -> Result<(), VmError> {
        let mut downstream = self.lock_downstream();
        downstream.vm_touches.response_status_mutated = true;
        Self::ensure_downstream_response_head_mutable(&downstream)?;
        if downstream.post_response_plan.is_none()
            && !downstream.response_output.has_local_body()
            && downstream.response_output.body_source_exchange.is_none()
            && downstream.response_output.headers.is_empty()
            && downstream.response_output.status.is_none()
            && let Some(native_response) =
                downstream.native_default_upstream_forward_response.as_mut()
        {
            native_response.status = status;
            return Ok(());
        }
        downstream.response_output.status = Some(status);
        Ok(())
    }

    pub(crate) fn downstream_websocket(&self) -> WebSocketConnectionState {
        self.ensure_downstream_websocket_initialized();
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

    pub(crate) fn clear_native_default_upstream_http_forward(&self) {
        let mut downstream = self.lock_downstream();
        downstream.native_default_upstream_http_forward = false;
        downstream.native_default_upstream_forward_request = None;
        downstream.native_default_upstream_forward_response = None;
    }

    fn store_native_default_upstream_forward_request(
        &self,
        request: DefaultUpstreamRequestSnapshot,
    ) {
        let mut downstream = self.lock_downstream();
        downstream.native_default_upstream_http_forward = true;
        downstream.native_default_upstream_forward_request = Some(request);
        downstream.native_default_upstream_forward_response = None;
    }

    fn take_native_default_upstream_forward_response(
        &self,
    ) -> Option<NativeDefaultUpstreamForwardResponse> {
        self.lock_downstream()
            .native_default_upstream_forward_response
            .take()
    }

    fn take_native_default_upstream_forward_request(
        &self,
    ) -> Option<DefaultUpstreamRequestSnapshot> {
        self.lock_downstream()
            .native_default_upstream_forward_request
            .take()
    }

    fn native_default_upstream_forward_response_ready(&self) -> bool {
        self.lock_downstream()
            .native_default_upstream_forward_response
            .is_some()
    }

    fn native_default_upstream_forward_request_pending(&self) -> bool {
        self.lock_downstream()
            .native_default_upstream_forward_request
            .is_some()
    }

    fn native_default_upstream_forward_latency_ms(&self) -> Option<u64> {
        self.lock_downstream()
            .native_default_upstream_forward_response
            .as_ref()
            .map(|response| response.upstream_latency_ms)
    }

    pub(crate) fn native_default_upstream_http_forward_active(&self) -> bool {
        self.lock_downstream().native_default_upstream_http_forward
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

    pub(crate) fn downstream_response_stream_ready_notify(&self) -> Arc<Notify> {
        self.lock_downstream().response_output.stream_ready_notify()
    }

    pub(crate) fn downstream_response_stream_committed(&self) -> bool {
        self.lock_downstream().response_output.stream_committed()
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

    pub(crate) async fn promote_downstream_http_request(
        &self,
        request: HttpRequestContext,
        http2_attachment: Option<http2::Http2DownstreamStreamAttachment>,
        downstream_http1_upgrade: Option<OnUpgrade>,
    ) {
        let HttpRequestContext {
            request_id,
            method,
            path,
            query,
            http_version,
            port,
            scheme,
            host,
            client_ip,
            body,
            headers,
        } = request;
        let request_head = HttpRequestHead {
            request_id,
            method,
            path,
            query,
            http_version,
            port,
            scheme,
            host,
            client_ip,
            headers,
        };
        *self.lock_request_head() = request_head;

        {
            let mut exchanges = self.lock_exchanges();
            let default_exchange = exchanges
                .exchanges
                .get_mut(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
                .expect("default upstream exchange should exist");
            default_exchange.request.reset_inherited_request_head();
        }

        *self.inbound_request_body.lock().await = InboundRequestBodyState::new(body);

        let mut downstream = self.lock_downstream();
        downstream.downstream_websocket = WebSocketConnectionState::default();
        downstream.downstream_websocket_initialized = false;
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
        self.ensure_downstream_websocket_initialized();
        let mut downstream = self.lock_downstream();
        mutate(&mut downstream.downstream_websocket)
    }

    fn ensure_downstream_websocket_initialized(&self) {
        let needs_init = {
            let downstream = self.lock_downstream();
            !downstream.downstream_websocket_initialized
        };
        if !needs_init {
            return;
        }
        let websocket = self.with_request_head(|request_head| {
            WebSocketConnectionState::for_http_request(request_head.headers())
        });
        let mut downstream = self.lock_downstream();
        if !downstream.downstream_websocket_initialized {
            downstream.downstream_websocket = websocket;
            downstream.downstream_websocket_initialized = true;
        }
    }

    fn lock_request_head(&self) -> ProfiledMutexGuard<'_, HttpRequestHead> {
        lock_metrics::lock(
            &self.request_head,
            LockMetricKey::VmRequestHead,
            "vm request head lock poisoned",
        )
    }

    pub(crate) fn lock_downstream(&self) -> ProfiledMutexGuard<'_, DownstreamState> {
        lock_metrics::lock(
            &self.downstream,
            LockMetricKey::VmDownstream,
            "vm downstream state lock poisoned",
        )
    }

    pub(crate) fn lock_exchanges(&self) -> ProfiledMutexGuard<'_, ExchangeRegistry> {
        let mut guard = lock_metrics::lock(
            &self.exchanges,
            LockMetricKey::VmExchanges,
            "vm exchange registry lock poisoned",
        );
        guard.seed_from_http_request();
        guard
    }

    pub(crate) fn lock_transport(&self) -> ProfiledMutexGuard<'_, TransportState> {
        let mut guard = lock_metrics::lock(
            &self.transport,
            LockMetricKey::VmTransport,
            "vm transport state lock poisoned",
        );
        guard.seed_from_http_request();
        guard
    }

    #[cfg(feature = "webrtc")]
    pub(crate) fn lock_webrtc(&self) -> MutexGuard<'_, WebRtcRegistry> {
        self.webrtc
            .lock()
            .expect("vm webrtc registry lock poisoned")
    }

    pub(crate) fn lock_proxy(&self) -> ProfiledMutexGuard<'_, ProxyStreamRegistry> {
        lock_metrics::lock(
            &self.proxy,
            LockMetricKey::VmProxy,
            "vm proxy registry lock poisoned",
        )
    }

    pub(crate) fn lock_edge_io(&self) -> ProfiledMutexGuard<'_, EdgeIoRegistry> {
        lock_metrics::lock(
            &self.edge_io,
            LockMetricKey::VmEdgeIo,
            "vm edge io registry lock poisoned",
        )
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
    let (target_scheme, target_host) = exchanges
        .exchanges
        .get(&exchange)
        .ok_or_else(|| VmError::HostError(format!("unknown outbound exchange handle {exchange}")))?
        .request
        .target
        .as_ref()
        .map(|_| {
            let request = &exchanges.exchanges[&exchange].request;
            (request.target_scheme, request.target_host.clone())
        })
        .ok_or_else(|| {
            VmError::HostError(
                "http exchange target must be configured before attaching a transport".to_string(),
            )
        })?;
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
        .get_or_insert_with(session, TlsFlowState::for_dynamic_socket);
    if !tls_flow.handshake_complete()
        && let Some(target_host) = target_host
    {
        tls_flow.observe_target(target_scheme.as_str(), &target_host);
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::{io, net::SocketAddr};

    use axum::{
        Router,
        body::{Body, Bytes, to_bytes},
        http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode},
        routing::any,
    };
    use futures_util::stream::try_unfold;
    use http_body_util::{BodyExt, StreamBody};
    use tokio::{
        sync::{Mutex as AsyncMutex, oneshot},
        time::{Duration, timeout},
    };

    use super::{
        Http1DownstreamResolution, HttpCarrierRef, HttpExchangeTransportState, HttpRequestContext,
        HttpUpstreamResponseSnapshot, HttpUpstreamScheme, LazyRequestId, ProxyVmContext,
        RequestPortField, RequestStringField, ResolvedSnapshotHttp1DownstreamResponse,
        SharedProxyVmContext, SnapshotHttp1DownstreamHeaders, UpstreamResponseBodyState,
        allocate_outbound_exchange_handle, append_outbound_exchange_body,
        default_upstream_exchange_handle, ensure_outbound_exchange_response_started,
        header_content_length, outbound_exchange_exists, read_request_body_all,
        read_request_body_next_chunk, read_request_body_next_line, resolve_http_graph_response,
        resolve_http1_downstream_response, response_from_upstream_snapshot,
        sync_response_output_body_headers,
    };
    use crate::abi_impl::RateLimiterStore;
    use crate::abi_impl::http2::{Http2DownstreamStreamAttachment, Http2StreamRef};
    #[cfg(feature = "http2")]
    use crate::abi_impl::http2::{
        Http2SendRequest, Http2UpstreamMode, new_shared_http_upstream_sessions, send_request,
        total_active_streams,
    };
    #[cfg(feature = "http2")]
    use crate::abi_impl::transport::TlsFlowState;

    fn test_context() -> SharedProxyVmContext {
        Arc::new(ProxyVmContext::from_request_headers(
            HeaderMap::new(),
            Arc::new(RateLimiterStore::new()),
        ))
    }

    fn test_context_with_request(body: Body, scheme: &str, host: &str) -> SharedProxyVmContext {
        Arc::new(ProxyVmContext::from_http_request(
            HttpRequestContext {
                request_id: LazyRequestId::from_string(String::new()),
                method: axum::http::Method::POST,
                path: RequestStringField::Static("/".to_string()),
                query: RequestStringField::Static(String::new()),
                http_version: RequestStringField::Static("1.1".to_string()),
                port: RequestPortField::Static(if scheme == "https" { 443 } else { 80 }),
                scheme: RequestStringField::Static(scheme.to_string()),
                host: RequestStringField::Static(host.to_string()),
                client_ip: RequestStringField::Static(String::new()),
                body,
                headers: HeaderMap::new().into(),
            },
            Arc::new(RateLimiterStore::new()),
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

    async fn response_parts(response: Response<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let (parts, body) = response.into_parts();
        let body = to_bytes(body, usize::MAX)
            .await
            .expect("response body should collect");
        (parts.status, parts.headers, body.to_vec())
    }

    fn snapshot_headers_to_map(headers: SnapshotHttp1DownstreamHeaders) -> HeaderMap {
        match headers {
            SnapshotHttp1DownstreamHeaders::Snapshot { base, overlay } => {
                let mut headers = HeaderMap::new();
                for (name, value) in base.iter() {
                    if !super::is_hop_by_hop_header(name) && !overlay.contains_key(name) {
                        headers.insert(name.clone(), value.clone());
                    }
                }
                super::merge_headers(&mut headers, &overlay);
                headers
            }
            SnapshotHttp1DownstreamHeaders::Explicit(headers) => headers,
        }
    }

    async fn snapshot_response_parts(
        snapshot: ResolvedSnapshotHttp1DownstreamResponse,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let ResolvedSnapshotHttp1DownstreamResponse {
            status,
            headers,
            version: _,
            upstream_latency_ms: _,
            body,
        } = snapshot;
        let body = body
            .lock()
            .await
            .read_all()
            .await
            .expect("snapshot body should collect");
        (
            StatusCode::from_u16(status).expect("snapshot status should be valid"),
            snapshot_headers_to_map(headers),
            body,
        )
    }

    async fn configure_snapshot_shortcut_context(
        context: &SharedProxyVmContext,
        upstream_port: u16,
    ) {
        {
            let mut exchanges = context.lock_exchanges();
            let exchange = exchanges
                .exchanges
                .get_mut(&default_upstream_exchange_handle())
                .expect("default upstream exchange should exist");
            exchange
                .request
                .set_target_host_port("127.0.0.1", upstream_port)
                .expect("target should be valid");
            exchange.request.inherits_request_head = false;
            exchange.request.path = "/snapshot".to_string();
            exchange.request.query.clear();
            exchange.transport.tcp_flow.configure();
            exchange
                .transport
                .tls_flow
                .observe_target("http", "127.0.0.1");
        }
        let snapshot =
            ensure_outbound_exchange_response_started(context, default_upstream_exchange_handle())
                .await
                .expect("default upstream response should start");
        context.with_downstream_response_mut(|response| {
            response.status = Some(StatusCode::ACCEPTED.as_u16());
            response.body = None;
            response.body_source_exchange = Some(default_upstream_exchange_handle());
            response.headers.clear();
            for (name, value) in snapshot.headers.iter() {
                if !super::is_hop_by_hop_header(name) {
                    response.headers.insert(name.clone(), value.clone());
                }
            }
            response.headers.insert(
                HeaderName::from_static("x-dag"),
                HeaderValue::from_static("snapshot-shortcut"),
            );
        });
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
            Arc::new(RateLimiterStore::new()),
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
                .attach_upstream_http1_support(8);
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
            exchange
                .request
                .set_target_host_port("127.0.0.1", upstream_addr.port())
                .expect("target should be valid");
            exchange.transport.tcp_flow.configure();
            exchange
                .transport
                .tls_flow
                .observe_target("http", "127.0.0.1");
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
            .attach_upstream_http1_support(8);
        {
            let mut exchanges = context.lock_exchanges();
            let exchange = exchanges
                .exchanges
                .get_mut(&default_upstream_exchange_handle())
                .expect("default upstream exchange should exist");
            exchange
                .request
                .set_target_host_port("127.0.0.1", upstream_addr.port())
                .expect("target should be valid");
            exchange.request.inherits_request_head = false;
            exchange.request.path = "/stream".to_string();
            exchange.request.query.clear();
            exchange.transport.tcp_flow.configure();
            exchange
                .transport
                .tls_flow
                .observe_target("http", "127.0.0.1");
        }

        let resolved = timeout(
            Duration::from_millis(100),
            resolve_http_graph_response(&context),
        )
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
            timeout(Duration::from_millis(50), body.frame())
                .await
                .is_err(),
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

    #[tokio::test(flavor = "current_thread")]
    async fn native_local_http1_shortcut_matches_graph_response_semantics() {
        let configure = |context: &SharedProxyVmContext| {
            context.with_downstream_response_mut(|response| {
                response.status = Some(StatusCode::CREATED.as_u16());
                response.headers.insert(
                    HeaderName::from_static("x-dag"),
                    HeaderValue::from_static("local-shortcut"),
                );
                response.body = Some(b"payload".to_vec());
                sync_response_output_body_headers(response);
            });
        };

        let fast_context = test_context();
        configure(&fast_context);
        let graph_context = test_context();
        configure(&graph_context);

        let (fast_status, fast_headers, fast_body, fast_default_content_type) =
            match resolve_http1_downstream_response(&fast_context).await {
                Http1DownstreamResolution::NativeLocal(native_local) => (
                    StatusCode::from_u16(native_local.status)
                        .expect("native local status should be valid"),
                    native_local.headers,
                    native_local.body,
                    native_local.default_content_type,
                ),
                resolution => panic!(
                    "expected native local shortcut resolution, got {:?}",
                    std::mem::discriminant(&resolution)
                ),
            };
        let graph = resolve_http_graph_response(&graph_context).await;
        let (graph_status, graph_headers, graph_body) = response_parts(graph.response).await;

        assert_eq!(fast_status, graph_status);
        assert_eq!(fast_body, graph_body);
        assert_eq!(
            fast_headers
                .get("x-dag")
                .and_then(|value| value.to_str().ok()),
            graph_headers
                .get("x-dag")
                .and_then(|value| value.to_str().ok())
        );
        assert!(fast_default_content_type);
        assert_eq!(
            graph_headers
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/plain")
        );
        assert_eq!(
            fast_headers
                .get(axum::http::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok()),
            graph_headers
                .get(axum::http::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn snapshot_http1_shortcut_matches_graph_response_semantics_without_mutating_dag() {
        let upstream_addr =
            spawn_server(Router::new().fallback(any(|_request: Request<Body>| async {
                let mut response = Response::new(Body::from("upstream-body"));
                response
                    .headers_mut()
                    .insert("x-upstream", HeaderValue::from_static("from-origin"));
                response
            })))
            .await;

        let mut fast_context = test_context_with_request(Body::from("payload"), "http", "");
        Arc::get_mut(&mut fast_context)
            .expect("fast context should be uniquely owned")
            .attach_upstream_http1_support(8);
        configure_snapshot_shortcut_context(&fast_context, upstream_addr.port()).await;

        let mut graph_context = test_context_with_request(Body::from("payload"), "http", "");
        Arc::get_mut(&mut graph_context)
            .expect("graph context should be uniquely owned")
            .attach_upstream_http1_support(8);
        configure_snapshot_shortcut_context(&graph_context, upstream_addr.port()).await;

        let (fast_status, fast_headers, fast_body) =
            match resolve_http1_downstream_response(&fast_context).await {
                Http1DownstreamResolution::Snapshot(Ok(snapshot)) => {
                    snapshot_response_parts(snapshot).await
                }
                resolution => panic!(
                    "expected snapshot shortcut resolution, got {:?}",
                    std::mem::discriminant(&resolution)
                ),
            };
        let graph = resolve_http_graph_response(&graph_context).await;
        let (graph_status, graph_headers, graph_body) = response_parts(graph.response).await;

        assert_eq!(fast_status, graph_status);
        assert_eq!(fast_body, graph_body);
        assert_eq!(
            fast_headers
                .get("x-upstream")
                .and_then(|value| value.to_str().ok()),
            graph_headers
                .get("x-upstream")
                .and_then(|value| value.to_str().ok())
        );
        assert_eq!(
            fast_headers
                .get("x-dag")
                .and_then(|value| value.to_str().ok()),
            graph_headers
                .get("x-dag")
                .and_then(|value| value.to_str().ok())
        );
        assert_eq!(
            fast_context.with_downstream_response(|response| response.body_source_exchange),
            Some(default_upstream_exchange_handle())
        );
        assert_eq!(
            graph_context.with_downstream_response(|response| response.body_source_exchange),
            Some(default_upstream_exchange_handle())
        );
        assert!(
            fast_context.lock_exchanges().exchanges[&default_upstream_exchange_handle()]
                .response_ready()
        );
        assert!(
            graph_context.lock_exchanges().exchanges[&default_upstream_exchange_handle()]
                .response_ready()
        );
    }

    #[cfg(all(feature = "http2", feature = "tls"))]
    #[tokio::test(flavor = "current_thread")]
    async fn known_length_streaming_response_retires_upstream_http2_stream_without_eof_poll() {
        use std::convert::Infallible;

        use http_body_util::Full;
        use hyper::Method;
        use hyper::{Response as HyperResponse, body::Incoming, service::service_fn};
        use hyper_util::rt::TokioIo;
        use rcgen::generate_simple_self_signed;
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
                    cert.serialize_der().expect("cert der should serialize"),
                )],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.serialize_private_key_der())),
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
        let mut tls_flow = TlsFlowState::default();
        tls_flow.observe_target("https", "127.0.0.1");
        tls_flow.set_verify_peer(false);
        tls_flow.set_verify_hostname(false);
        tls_flow.set_desired_alpn(vec!["h2".to_string(), "http/1.1".to_string()]);
        let authority = format!("127.0.0.1:{}", addr.port());

        let started = send_request(Http2SendRequest {
            sessions: &sessions,
            exchange_handle: default_upstream_exchange_handle(),
            target_scheme: HttpUpstreamScheme::Https,
            target_host: "127.0.0.1",
            target_port: addr.port(),
            target_host_header: Some(&authority),
            request_path: "/",
            request_query: "",
            mode: Http2UpstreamMode::AutomaticTls,
            tls_flow: &tls_flow,
            method: Method::GET,
            headers: HeaderMap::new(),
            request_body: Body::empty(),
            request_body_present: false,
        })
        .await
        .expect("http2 request should start");

        let carrier_ref = HttpCarrierRef::UpstreamHttp2Stream(started.stream_ref);
        let response_headers = started.response.headers().clone();
        let content_length = header_content_length(&response_headers);
        let response_status = started.response.status().as_u16();
        let snapshot = HttpUpstreamResponseSnapshot {
            status: response_status,
            headers: Arc::new(response_headers),
            http_version: "2",
            carrier_kind: carrier_ref.kind(),
            carrier_ref,
            body: Arc::new(tokio::sync::Mutex::new(
                UpstreamResponseBodyState::from_hyper(
                    started.response.into_body(),
                    Some(started.body_tracker),
                    None,
                    content_length,
                ),
            )),
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
