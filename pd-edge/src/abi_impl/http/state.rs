#![cfg_attr(not(feature = "http"), allow(dead_code))]

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};

use axum::{
    body::{Body, Bytes},
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode, Version,
        header::{CONTENT_LENGTH, CONTENT_TYPE, HOST},
    },
};
use http_body_util::{BodyExt, Full};
use url::Url;
use vm::VmError;

use super::super::{
    EDGE_IO_HANDLE_DYNAMIC_BASE, EdgeVirtualIoHandle, SharedHttpUpstreamSessions,
    SharedRateLimiter,
    proxy::ProxyByteStreamState,
    transport::{
        CachedTlsSession, FIRST_DYNAMIC_TCP_STREAM_HANDLE, SharedTcpStreamIo,
        SharedTlsSessionCache, SharedUdpSocketIo, TcpFlowState, TcpSocketState, TcpTransportDag,
        TlsFlowState, TlsProtocolVersion, TlsSessionCacheKey, TlsTransportDag, UdpSocketState,
        alpn_from_http_version, tls_session_cache_key,
    },
    websocket::WebSocketConnectionState,
};
#[cfg(feature = "tls")]
use crate::abi_impl::transport::SharedTlsStreamIo;
#[cfg(feature = "webrtc")]
use crate::abi_impl::webrtc::WebRtcConnectionState;
use crate::abi_impl::{http1, http2};
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
pub(crate) struct HttpRequestHead {
    pub(crate) request_id: String,
    pub(crate) method: Method,
    pub(crate) path: String,
    pub(crate) query: String,
    pub(crate) http_version: String,
    pub(crate) port: u16,
    pub(crate) scheme: String,
    pub(crate) host: String,
    pub(crate) client_ip: String,
    pub(crate) headers: HeaderMap,
}

pub(crate) struct InboundRequestBodyState {
    body: Option<Body>,
    buffered: Vec<u8>,
    read_offset: usize,
    eof: bool,
}

impl std::fmt::Debug for InboundRequestBodyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboundRequestBodyState")
            .field("buffered_len", &self.buffered.len())
            .field("read_offset", &self.read_offset)
            .field("eof", &self.eof)
            .finish()
    }
}

impl InboundRequestBodyState {
    fn new(body: Body) -> Self {
        Self {
            body: Some(body),
            buffered: Vec::new(),
            read_offset: 0,
            eof: false,
        }
    }

    async fn pull_next_frame(&mut self) -> Result<(), VmError> {
        if self.eof {
            return Ok(());
        }
        let Some(body) = self.body.as_mut() else {
            self.eof = true;
            return Ok(());
        };

        match body.frame().await {
            Some(Ok(frame)) => {
                if let Ok(chunk) = frame.into_data()
                    && !chunk.is_empty()
                {
                    self.buffered.extend_from_slice(&chunk);
                }
            }
            Some(Err(err)) => {
                return Err(VmError::HostError(format!(
                    "failed to read inbound request body frame: {err}",
                )));
            }
            None => {
                self.eof = true;
                self.body = None;
            }
        }
        Ok(())
    }

    async fn ensure_readable_byte(&mut self) -> Result<(), VmError> {
        while self.read_offset >= self.buffered.len() && !self.eof {
            self.pull_next_frame().await?;
        }
        Ok(())
    }

    async fn read_next_chunk(&mut self, max_bytes: usize) -> Result<Vec<u8>, VmError> {
        self.ensure_readable_byte().await?;
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

    async fn read_next_line(&mut self) -> Result<Vec<u8>, VmError> {
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

            self.pull_next_frame().await?;
        }
    }

    async fn read_all_and_consume(&mut self) -> Result<Vec<u8>, VmError> {
        let body = self.read_all().await?;
        self.read_offset = self.buffered.len();
        Ok(body)
    }

    async fn read_all(&mut self) -> Result<Vec<u8>, VmError> {
        while !self.eof {
            self.pull_next_frame().await?;
        }
        Ok(self.buffered.clone())
    }

    async fn eof(&mut self) -> Result<bool, VmError> {
        while self.read_offset >= self.buffered.len() && !self.eof {
            self.pull_next_frame().await?;
        }
        Ok(self.eof && self.read_offset >= self.buffered.len())
    }
}

type SharedInboundRequestBody = Arc<tokio::sync::Mutex<InboundRequestBodyState>>;
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
}

impl HttpCarrierKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Http1 => "http1",
            Self::Http2 => "http2",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HttpCarrierRef {
    DownstreamHttp1,
    DownstreamHttp2Stream(http2::Http2StreamRef),
    Http1DefaultUpstream,
    Http1DynamicExchange(i64),
    UpstreamHttp2Stream(http2::Http2StreamRef),
}

impl HttpCarrierRef {
    fn kind(&self) -> HttpCarrierKind {
        match self {
            Self::DownstreamHttp1 | Self::Http1DefaultUpstream | Self::Http1DynamicExchange(_) => {
                HttpCarrierKind::Http1
            }
            Self::DownstreamHttp2Stream(_) | Self::UpstreamHttp2Stream(_) => HttpCarrierKind::Http2,
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
    pub(crate) attached_transport: Option<AttachedHttpTransport>,
}

impl HttpExchangeTransportState {
    fn note_write(&mut self) {
        self.tcp_flow.note_write();
    }

    fn mark_response_ready(&mut self, version: Version, carrier_ref: HttpCarrierRef) {
        self.tcp_flow.mark_connected();
        self.carrier_kind = carrier_ref.kind();
        self.carrier_ref = Some(carrier_ref);
        self.http_version = Some(http_version_label(version).to_string());
    }
}

#[cfg_attr(not(feature = "http2"), allow(dead_code))]
enum UpstreamResponseSource {
    Reqwest(reqwest::Response),
    #[cfg_attr(not(feature = "http2"), allow(dead_code))]
    Hyper(hyper::body::Incoming),
    Exhausted,
}

struct UpstreamResponseBodyState {
    source: UpstreamResponseSource,
    http2_tracker: Option<http2::Http2ResponseBodyTracker>,
    body_started: bool,
    buffered: Vec<u8>,
    read_offset: usize,
    eof: bool,
}

impl std::fmt::Debug for UpstreamResponseBodyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamResponseBodyState")
            .field("buffered_len", &self.buffered.len())
            .field("read_offset", &self.read_offset)
            .field("eof", &self.eof)
            .finish()
    }
}

impl UpstreamResponseBodyState {
    fn from_reqwest(response: reqwest::Response) -> Self {
        Self {
            source: UpstreamResponseSource::Reqwest(response),
            http2_tracker: None,
            body_started: false,
            buffered: Vec::new(),
            read_offset: 0,
            eof: false,
        }
    }

    #[cfg_attr(not(feature = "http2"), allow(dead_code))]
    fn from_hyper(
        body: hyper::body::Incoming,
        http2_tracker: Option<http2::Http2ResponseBodyTracker>,
    ) -> Self {
        Self {
            source: UpstreamResponseSource::Hyper(body),
            http2_tracker,
            body_started: false,
            buffered: Vec::new(),
            read_offset: 0,
            eof: false,
        }
    }

    async fn pull_next_chunk(&mut self) -> Result<(), VmError> {
        if self.eof {
            return Ok(());
        }
        match &mut self.source {
            UpstreamResponseSource::Reqwest(response) => match response.chunk().await {
                Ok(Some(chunk)) => {
                    if !chunk.is_empty() {
                        self.buffered.extend_from_slice(&chunk);
                    }
                }
                Ok(None) => {
                    self.eof = true;
                    self.source = UpstreamResponseSource::Exhausted;
                }
                Err(err) => {
                    return Err(VmError::HostError(format!(
                        "failed to read upstream response chunk: {err}",
                    )));
                }
            },
            UpstreamResponseSource::Hyper(body) => match body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(chunk) = frame.into_data()
                        && !chunk.is_empty()
                    {
                        if !self.body_started {
                            if let Some(tracker) = &self.http2_tracker {
                                tracker.note_response_body_ready();
                            }
                            self.body_started = true;
                        }
                        self.buffered.extend_from_slice(&chunk);
                    }
                }
                Some(Err(err)) => {
                    let observed = http2::classify_http2_error(&err);
                    if let Some(tracker) = &self.http2_tracker {
                        tracker.note_body_error(&observed);
                    }
                    return Err(VmError::HostError(format!(
                        "failed to read upstream response frame: {}",
                        observed.message,
                    )));
                }
                None => {
                    if !self.body_started {
                        if let Some(tracker) = &self.http2_tracker {
                            tracker.note_response_body_ready();
                        }
                        self.body_started = true;
                    }
                    if let Some(tracker) = &self.http2_tracker {
                        tracker.note_body_eof();
                    }
                    self.eof = true;
                    self.source = UpstreamResponseSource::Exhausted;
                }
            },
            UpstreamResponseSource::Exhausted => {
                self.eof = true;
            }
        }
        Ok(())
    }

    async fn ensure_readable_byte(&mut self) -> Result<(), VmError> {
        while self.read_offset >= self.buffered.len() && !self.eof {
            self.pull_next_chunk().await?;
        }
        Ok(())
    }

    async fn read_next_chunk(&mut self, max_bytes: usize) -> Result<Vec<u8>, VmError> {
        self.ensure_readable_byte().await?;
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

    async fn read_next_line(&mut self) -> Result<Vec<u8>, VmError> {
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

            self.pull_next_chunk().await?;
        }
    }

    async fn read_all(&mut self) -> Result<Vec<u8>, VmError> {
        while !self.eof {
            self.pull_next_chunk().await?;
        }
        Ok(self.buffered.clone())
    }

    async fn eof(&mut self) -> Result<bool, VmError> {
        while self.read_offset >= self.buffered.len() && !self.eof {
            self.pull_next_chunk().await?;
        }
        Ok(self.eof && self.read_offset >= self.buffered.len())
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
            },
            response: HttpUpstreamResponseNode::NotStarted,
            transport: HttpExchangeTransportState::default(),
            websocket_dag: WebSocketConnectionState::default(),
            upstream_latency_ms: 0,
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
pub struct ProxyVmContext {
    pub(crate) request_head: HttpRequestHead,
    pub(crate) inbound_request_body: SharedInboundRequestBody,
    pub(crate) tcp_dag: TcpTransportDag,
    pub(crate) tls_dag: TlsTransportDag,
    pub(crate) next_tcp_stream_handle: i64,
    pub(crate) tcp_streams: HashMap<i64, TcpSocketState>,
    pub(crate) tcp_stream_ios: HashMap<i64, SharedTcpStreamIo>,
    #[cfg(feature = "tls")]
    pub(crate) dynamic_tls_sessions: HashMap<i64, TlsFlowState>,
    #[cfg(feature = "tls")]
    pub(crate) dynamic_tls_session_ios: HashMap<i64, SharedTlsStreamIo>,
    #[cfg_attr(not(feature = "websocket"), allow(dead_code))]
    pub(crate) downstream_websocket: WebSocketConnectionState,
    pub(crate) default_upstream_websocket: WebSocketConnectionState,
    pub(crate) default_upstream_attached_transport: Option<AttachedHttpTransport>,
    pub(crate) outbound_request: HttpOutboundRequestNode,
    pub(crate) response_output: HttpResponseOutputNode,
    pub(crate) upstream_response: HttpUpstreamResponseNode,
    pub(crate) downstream_carrier_ref: Option<HttpCarrierRef>,
    pub(crate) upstream_carrier_ref: Option<HttpCarrierRef>,
    pub(crate) upstream_client: Option<reqwest::Client>,
    pub(crate) upstream_client_cache: Option<SharedUpstreamClientCache>,
    pub(crate) tls_session_cache: Option<SharedTlsSessionCache>,
    pub(crate) upstream_http_sessions: Option<SharedHttpUpstreamSessions>,
    pub(crate) upstream_latency_ms: u64,
    pub(crate) next_outbound_exchange_handle: i64,
    pub(crate) outbound_exchanges: HashMap<i64, HttpOutboundExchangeState>,
    pub(crate) default_upstream_udp_socket: UdpSocketState,
    pub(crate) default_upstream_udp_io: Option<SharedUdpSocketIo>,
    pub(crate) next_udp_socket_handle: i64,
    pub(crate) udp_sockets: HashMap<i64, UdpSocketState>,
    pub(crate) udp_socket_ios: HashMap<i64, SharedUdpSocketIo>,
    #[cfg(feature = "webrtc")]
    pub(crate) default_upstream_webrtc: WebRtcConnectionState,
    #[cfg(feature = "webrtc")]
    pub(crate) next_webrtc_connection_handle: i64,
    #[cfg(feature = "webrtc")]
    pub(crate) webrtc_connections: HashMap<i64, WebRtcConnectionState>,
    pub(crate) next_proxy_stream_handle: i64,
    pub(crate) proxy_stream_handles: HashMap<i64, ProxyByteStreamState>,
    pub(crate) rate_limiter: SharedRateLimiter,
    pub(crate) edge_io_next_handle: i64,
    pub(crate) edge_io_handles: HashMap<i64, EdgeVirtualIoHandle>,
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
            headers: HeaderMap::new(),
        };
        let outbound_request = HttpOutboundRequestNode {
            method: request_head.method.clone(),
            path: request_head.path.clone(),
            query: request_head.query.clone(),
            headers: request_headers.clone(),
            body_override: None,
            target: None,
        };
        let tcp_dag = TcpTransportDag::for_http_request();
        let tls_dag = TlsTransportDag::for_http_request(
            request_head.scheme.as_str(),
            request_head.host.as_str(),
            request_head.http_version.as_str(),
        );
        let downstream_websocket = WebSocketConnectionState::for_http_request(&request_headers);
        Self {
            outbound_request,
            request_head: HttpRequestHead {
                headers: request_headers,
                ..request_head
            },
            inbound_request_body: Arc::new(tokio::sync::Mutex::new(InboundRequestBodyState::new(
                request.body,
            ))),
            tcp_dag,
            tls_dag,
            next_tcp_stream_handle: FIRST_DYNAMIC_TCP_STREAM_HANDLE,
            tcp_streams: HashMap::new(),
            tcp_stream_ios: HashMap::new(),
            #[cfg(feature = "tls")]
            dynamic_tls_sessions: HashMap::new(),
            #[cfg(feature = "tls")]
            dynamic_tls_session_ios: HashMap::new(),
            downstream_websocket,
            default_upstream_websocket: WebSocketConnectionState::default(),
            default_upstream_attached_transport: None,
            response_output: HttpResponseOutputNode::default(),
            upstream_response: HttpUpstreamResponseNode::NotStarted,
            downstream_carrier_ref: Some(HttpCarrierRef::DownstreamHttp1),
            upstream_carrier_ref: None,
            upstream_client: None,
            upstream_client_cache: None,
            tls_session_cache: None,
            upstream_http_sessions: None,
            upstream_latency_ms: 0,
            next_outbound_exchange_handle: FIRST_DYNAMIC_EXCHANGE_HANDLE,
            outbound_exchanges: HashMap::new(),
            default_upstream_udp_socket: UdpSocketState::default(),
            default_upstream_udp_io: None,
            next_udp_socket_handle: FIRST_DYNAMIC_UDP_SOCKET_HANDLE,
            udp_sockets: HashMap::new(),
            udp_socket_ios: HashMap::new(),
            #[cfg(feature = "webrtc")]
            default_upstream_webrtc: WebRtcConnectionState::default(),
            #[cfg(feature = "webrtc")]
            next_webrtc_connection_handle: FIRST_DYNAMIC_WEBRTC_CONNECTION_HANDLE,
            #[cfg(feature = "webrtc")]
            webrtc_connections: HashMap::new(),
            next_proxy_stream_handle: FIRST_PROXY_STREAM_HANDLE,
            proxy_stream_handles: HashMap::new(),
            rate_limiter,
            edge_io_next_handle: EDGE_IO_HANDLE_DYNAMIC_BASE,
            edge_io_handles: HashMap::new(),
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

    pub fn attach_upstream_client(&mut self, client: reqwest::Client) {
        self.upstream_client = Some(client);
    }

    pub(crate) fn attach_upstream_client_cache(&mut self, cache: SharedUpstreamClientCache) {
        self.upstream_client_cache = Some(cache);
    }

    pub(crate) fn attach_tls_session_cache(&mut self, cache: SharedTlsSessionCache) {
        self.tls_session_cache = Some(cache);
    }

    pub(crate) fn attach_upstream_http_sessions(&mut self, sessions: SharedHttpUpstreamSessions) {
        self.upstream_http_sessions = Some(sessions);
    }

    fn upstream_response(&self) -> Result<HttpUpstreamResponseSnapshot, VmError> {
        match &self.upstream_response {
            HttpUpstreamResponseNode::Ready(snapshot) => Ok(snapshot.clone()),
            HttpUpstreamResponseNode::NotStarted => Err(VmError::HostError(
                "upstream response is unavailable before upstream exchange".to_string(),
            )),
        }
    }

    fn upstream_response_ready(&self) -> bool {
        matches!(self.upstream_response, HttpUpstreamResponseNode::Ready(_))
    }

    fn store_upstream_response(&mut self, response: StoredUpstreamResponse) {
        self.upstream_carrier_ref = Some(response.snapshot.carrier_ref.clone());
        self.upstream_response = HttpUpstreamResponseNode::Ready(response.snapshot);
        self.upstream_latency_ms = response.latency_ms;
    }

    pub(crate) fn attach_downstream_http2_stream(
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
}

pub type SharedProxyVmContext = Arc<Mutex<ProxyVmContext>>;

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
    let mut guard = context.lock().expect("vm context lock poisoned");
    let handle = guard.next_outbound_exchange_handle;
    if handle == i64::MAX {
        return Err(VmError::HostError(
            "outbound exchange handle space exhausted".to_string(),
        ));
    }
    guard.next_outbound_exchange_handle += 1;
    guard
        .outbound_exchanges
        .insert(handle, HttpOutboundExchangeState::new());
    Ok(handle)
}

pub(crate) fn outbound_exchange_exists(context: &SharedProxyVmContext, handle: i64) -> bool {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return true;
    }
    let guard = context.lock().expect("vm context lock poisoned");
    guard.outbound_exchanges.contains_key(&handle)
}

pub(crate) fn allocate_tcp_stream_handle(context: &SharedProxyVmContext) -> Result<i64, VmError> {
    let mut guard = context.lock().expect("vm context lock poisoned");
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
    let guard = context.lock().expect("vm context lock poisoned");
    guard.tcp_streams.contains_key(&handle)
}

pub(crate) fn allocate_udp_socket_handle(context: &SharedProxyVmContext) -> Result<i64, VmError> {
    let mut guard = context.lock().expect("vm context lock poisoned");
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
    let guard = context.lock().expect("vm context lock poisoned");
    guard.udp_sockets.contains_key(&handle)
}

fn exchange_target_snapshot(guard: &ProxyVmContext, handle: i64) -> Result<String, VmError> {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return guard.outbound_request.target.clone().ok_or_else(|| {
            VmError::HostError(
                "http exchange target must be configured before attaching a transport".to_string(),
            )
        });
    }

    guard
        .outbound_exchanges
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
    let mut guard = context.lock().expect("vm context lock poisoned");
    if exchange != DEFAULT_UPSTREAM_EXCHANGE_HANDLE
        && !guard.outbound_exchanges.contains_key(&exchange)
    {
        return Err(VmError::HostError(format!(
            "unknown outbound exchange handle {exchange}",
        )));
    }
    let _target = exchange_target_snapshot(&guard, exchange)?;
    let Some(socket) = guard.tcp_streams.get(&stream) else {
        return Err(VmError::HostError(format!(
            "http::exchange::attach_tcp requires a dynamic tcp stream handle, got {stream}",
        )));
    };
    if socket.phase() != crate::abi_impl::transport::TcpSocketPhase::Connected {
        return Err(VmError::HostError(format!(
            "tcp stream handle {stream} must be connected before it can be attached to an http exchange",
        )));
    }

    if exchange == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        if guard.upstream_response_ready() {
            return Err(VmError::HostError(
                "default upstream exchange is read-only after the exchange has started".to_string(),
            ));
        }
        guard.default_upstream_attached_transport = Some(AttachedHttpTransport::Tcp(stream));
        return Ok(());
    }

    let exchange_state = guard
        .outbound_exchanges
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
    let mut guard = context.lock().expect("vm context lock poisoned");
    if exchange != DEFAULT_UPSTREAM_EXCHANGE_HANDLE
        && !guard.outbound_exchanges.contains_key(&exchange)
    {
        return Err(VmError::HostError(format!(
            "unknown outbound exchange handle {exchange}",
        )));
    }
    let target = exchange_target_snapshot(&guard, exchange)?;
    let tcp_state = guard.tcp_streams.get(&session).ok_or_else(|| {
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

    let tls_flow = guard
        .dynamic_tls_sessions
        .entry(session)
        .or_insert_with(TlsFlowState::for_dynamic_socket);
    if tls_flow.server_name().is_empty() && tls_flow.peer_name().is_empty() {
        tls_flow.observe_target(&target);
    }

    if exchange == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        if guard.upstream_response_ready() {
            return Err(VmError::HostError(
                "default upstream exchange is read-only after the exchange has started".to_string(),
            ));
        }
        guard.default_upstream_attached_transport = Some(AttachedHttpTransport::Tls(session));
        return Ok(());
    }

    let exchange_state = guard
        .outbound_exchanges
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
    let mut guard = context.lock().expect("vm context lock poisoned");
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
    let guard = context.lock().expect("vm context lock poisoned");
    guard.webrtc_connections.contains_key(&handle)
}

pub(crate) fn outbound_exchange_tls_flow(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<TlsFlowState, VmError> {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        let guard = context.lock().expect("vm context lock poisoned");
        return Ok(guard.tls_dag.default_upstream.clone());
    }

    let guard = context.lock().expect("vm context lock poisoned");
    guard
        .outbound_exchanges
        .get(&handle)
        .map(|exchange| exchange.transport.tls_flow.clone())
        .ok_or_else(|| VmError::HostError(format!("unknown outbound exchange handle {handle}")))
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
        let mut guard = context.lock().expect("vm context lock poisoned");
        guard.tcp_dag.default_upstream.note_write();
        guard
            .outbound_request
            .body_override
            .get_or_insert_with(Vec::new)
            .extend_from_slice(bytes);
        return Ok(());
    }

    let mut guard = context.lock().expect("vm context lock poisoned");
    let exchange = guard
        .outbound_exchanges
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
    let mut guard = context.lock().expect("vm context lock poisoned");
    guard.tcp_dag.downstream.note_write();
    guard
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
                "upstream target is unavailable before http::upstream::request::set_target"
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
    http_sessions: Option<SharedHttpUpstreamSessions>,
    http2_mode: http2::Http2UpstreamMode,
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
    negotiated_alpn: Option<String>,
    peer_certificate_der: Option<Vec<u8>>,
    body: SharedUpstreamResponseBody,
}

#[derive(Debug)]
pub(crate) struct ResolvedHttpGraphResponse {
    pub response: Response<Body>,
    pub upstream_latency_ms: u64,
}

pub async fn resolve_outbound_request_body(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let (body_override, inbound_body) = {
        let guard = context.lock().expect("vm context lock poisoned");
        (
            guard.outbound_request.body_override.clone(),
            guard.inbound_request_body.clone(),
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
        http1::response_version_label(version)
    }
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
    let guard = context.lock().expect("vm context lock poisoned");
    if guard.default_upstream_websocket.is_websocket_mode() {
        return Err(UpstreamResponseStartError::Protocol(
            "default upstream exchange is already owned by the websocket DAG".to_string(),
        ));
    }
    let target = guard
        .outbound_request
        .target
        .clone()
        .ok_or(UpstreamResponseStartError::MissingTarget)?;
    let attached_transport = guard.default_upstream_attached_transport;
    let tls_flow = match attached_transport {
        #[cfg(feature = "tls")]
        Some(AttachedHttpTransport::Tls(session)) => guard
            .dynamic_tls_sessions
            .get(&session)
            .cloned()
            .unwrap_or_else(TlsFlowState::for_dynamic_socket),
        _ => guard.tls_dag.default_upstream.clone(),
    };
    Ok(PreparedUpstreamRequest {
        client: guard
            .upstream_client
            .clone()
            .ok_or(UpstreamResponseStartError::MissingClient)?,
        upstream_client_cache: guard.upstream_client_cache.clone(),
        http_sessions: guard.upstream_http_sessions.clone(),
        http2_mode: http2::select_upstream_mode(&target, &tls_flow),
        tls_flow,
        attached_transport,
        method: guard.outbound_request.method.clone(),
        path: guard.outbound_request.path.clone(),
        query: guard.outbound_request.query.clone(),
        headers: guard.outbound_request.headers.clone(),
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

    let guard = context.lock().expect("vm context lock poisoned");
    let exchange = guard
        .outbound_exchanges
        .get(&handle)
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
        Some(AttachedHttpTransport::Tls(session)) => guard
            .dynamic_tls_sessions
            .get(&session)
            .cloned()
            .unwrap_or_else(TlsFlowState::for_dynamic_socket),
        _ => exchange.transport.tls_flow.clone(),
    };
    Ok(PreparedUpstreamRequest {
        client: guard
            .upstream_client
            .clone()
            .ok_or(UpstreamResponseStartError::MissingClient)?,
        upstream_client_cache: guard.upstream_client_cache.clone(),
        http_sessions: guard.upstream_http_sessions.clone(),
        http2_mode: http2::select_upstream_mode(&target, &tls_flow),
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

    let guard = context.lock().expect("vm context lock poisoned");
    let exchange = guard
        .outbound_exchanges
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
        let mut guard = context.lock().expect("vm context lock poisoned");
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
        let mut guard = context.lock().expect("vm context lock poisoned");
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
    let mut guard = context.lock().expect("vm context lock poisoned");
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return Ok(mutate(&mut guard.tls_dag.default_upstream));
    }

    let exchange = guard
        .outbound_exchanges
        .get_mut(&handle)
        .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
    Ok(mutate(&mut exchange.transport.tls_flow))
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
    Ok(StartedUpstreamResponse {
        status: response.status().as_u16(),
        headers: response.headers().clone(),
        version,
        carrier_ref: if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
            HttpCarrierRef::Http1DefaultUpstream
        } else {
            HttpCarrierRef::Http1DynamicExchange(handle)
        },
        negotiated_alpn: Some(http1::ALPN_PROTOCOL.to_string()),
        peer_certificate_der: None,
        body: Arc::new(tokio::sync::Mutex::new(
            UpstreamResponseBodyState::from_hyper(response.into_body(), None),
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
    let request_path = super::request_path_with_query(&prepared.path, &prepared.query);
    match prepared.attached_transport {
        Some(AttachedHttpTransport::Tcp(stream)) => {
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
            let stream = take_dynamic_tcp_stream_for_http(context, stream).await?;
            start_upstream_response_via_attached_http1(
                handle,
                prepared,
                &request_path,
                headers,
                request_body,
                stream,
            )
            .await
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
                let guard = context.lock().expect("vm context lock poisoned");
                guard
                    .dynamic_tls_sessions
                    .get(&session)
                    .and_then(|flow| (!flow.alpn().is_empty()).then(|| flow.alpn().to_string()))
            };
            started.peer_certificate_der = {
                let guard = context.lock().expect("vm context lock poisoned");
                guard
                    .dynamic_tls_sessions
                    .get(&session)
                    .and_then(|flow| flow.peer_certificate_der().map(|bytes| bytes.to_vec()))
            };
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
        .http_sessions
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
    Ok(StartedUpstreamResponse {
        status: started.response.status().as_u16(),
        headers: started.response.headers().clone(),
        version,
        carrier_ref: HttpCarrierRef::UpstreamHttp2Stream(started.stream_ref),
        negotiated_alpn: started.negotiated_alpn,
        peer_certificate_der: started.peer_certificate_der,
        body: Arc::new(tokio::sync::Mutex::new(
            UpstreamResponseBodyState::from_hyper(
                started.response.into_body(),
                Some(started.body_tracker),
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
    let guard = context.lock().expect("vm context lock poisoned");
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return Ok(guard.tls_dag.default_upstream.handshake_complete());
    }

    let exchange = guard
        .outbound_exchanges
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
        let guard = context.lock().expect("vm context lock poisoned");
        let Some(cache) = guard.tls_session_cache.clone() else {
            return Ok(());
        };
        let (target, flow) = if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
            (
                guard.outbound_request.target.as_deref(),
                &guard.tls_dag.default_upstream,
            )
        } else {
            let exchange = guard
                .outbound_exchanges
                .get(&handle)
                .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
            (
                exchange.request.target.as_deref(),
                &exchange.transport.tls_flow,
            )
        };
        let Some(target) = target else {
            return Ok(());
        };
        let Some(key) = tls_session_cache_key(target, flow) else {
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
    let mut guard = context.lock().expect("vm context lock poisoned");
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        guard.tcp_dag.default_upstream.mark_connected();
        return Ok(());
    }

    let exchange = guard
        .outbound_exchanges
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
        let guard = context.lock().expect("vm context lock poisoned");
        if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
            if let Ok(snapshot) = guard.upstream_response() {
                return Ok(snapshot);
            }
        } else {
            let exchange = guard
                .outbound_exchanges
                .get(&handle)
                .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
            if let Ok(snapshot) = exchange.response_snapshot() {
                return Ok(snapshot);
            }
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

        let mut guard = context.lock().expect("vm context lock poisoned");
        if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
            guard.default_upstream_attached_transport = None;
            guard
                .store_upstream_response(StoredUpstreamResponse::new(snapshot.clone(), latency_ms));
            return Ok(snapshot);
        }

        let exchange = guard
            .outbound_exchanges
            .get_mut(&handle)
            .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
        exchange.transport.attached_transport = None;
        exchange.store_response(StoredUpstreamResponse::new(snapshot.clone(), latency_ms));
        exchange
            .transport
            .mark_response_ready(upstream_response_version, snapshot.carrier_ref.clone());
        return Ok(snapshot);
    }

    let handshake_already_complete = outbound_tls_handshake_complete(context, handle)?;
    if !handshake_already_complete {
        note_outbound_tls_prepared(context, handle)?;
    }
    let started = Instant::now();
    let upstream_response = if http2::should_use_explicit_upstream_transport(
        prepared.http2_mode,
        prepared.http_sessions.as_ref(),
    ) {
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

    let mut guard = context.lock().expect("vm context lock poisoned");
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        if let Ok(existing) = guard.upstream_response() {
            return Ok(existing);
        }
        guard.store_upstream_response(StoredUpstreamResponse::new(snapshot.clone(), latency_ms));
        return Ok(snapshot);
    }

    let exchange = guard
        .outbound_exchanges
        .get_mut(&handle)
        .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
    if let Ok(existing) = exchange.response_snapshot() {
        return Ok(existing);
    }
    exchange.store_response(StoredUpstreamResponse::new(snapshot.clone(), latency_ms));
    exchange
        .transport
        .mark_response_ready(upstream_response_version, snapshot.carrier_ref.clone());
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
    let guard = context.lock().expect("vm context lock poisoned");
    guard.upstream_response_ready()
}

#[allow(dead_code)]
pub(crate) fn outbound_exchange_response_available(
    context: &SharedProxyVmContext,
    handle: i64,
) -> bool {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return upstream_response_available(context);
    }
    let guard = context.lock().expect("vm context lock poisoned");
    guard
        .outbound_exchanges
        .get(&handle)
        .map(HttpOutboundExchangeState::response_ready)
        .unwrap_or(false)
}

pub(crate) async fn read_upstream_response_all(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let snapshot = ensure_upstream_response_started(context).await?;
    let mut body = snapshot.body.lock().await;
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
    let mut body = snapshot.body.lock().await;
    body.read_all().await
}

pub(crate) async fn read_upstream_response_next_chunk(
    context: &SharedProxyVmContext,
    max_bytes: usize,
) -> Result<Vec<u8>, VmError> {
    let snapshot = ensure_upstream_response_started(context).await?;
    let mut body = snapshot.body.lock().await;
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
    let mut body = snapshot.body.lock().await;
    body.read_next_chunk(max_bytes).await
}

pub(crate) async fn read_upstream_response_next_line(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let snapshot = ensure_upstream_response_started(context).await?;
    let mut body = snapshot.body.lock().await;
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
    let mut body = snapshot.body.lock().await;
    body.read_next_line().await
}

pub(crate) async fn upstream_response_eof(context: &SharedProxyVmContext) -> Result<bool, VmError> {
    let snapshot = ensure_upstream_response_started(context).await?;
    let mut body = snapshot.body.lock().await;
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
    let mut body = snapshot.body.lock().await;
    body.eof().await
}

fn current_upstream_latency_ms(context: &SharedProxyVmContext) -> u64 {
    let guard = context.lock().expect("vm context lock poisoned");
    guard.upstream_latency_ms
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

async fn response_from_upstream_snapshot(
    snapshot: HttpUpstreamResponseSnapshot,
    response_headers: HeaderMap,
    response_status: Option<u16>,
) -> Result<Response<Body>, VmError> {
    let body = {
        let mut upstream_body = snapshot.body.lock().await;
        upstream_body.read_all().await?
    };
    let mut response = Response::new(Body::from(body));
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
        has_upstream_target,
        default_upstream_websocket_mode,
        upstream_response,
    ) = {
        let guard = context.lock().expect("vm context lock poisoned");
        (
            guard.response_output.body.clone(),
            guard.response_output.headers.clone(),
            guard.response_output.status,
            guard.outbound_request.target.is_some(),
            guard.default_upstream_websocket.is_websocket_mode(),
            match &guard.upstream_response {
                HttpUpstreamResponseNode::Ready(snapshot) => Some(snapshot.clone()),
                HttpUpstreamResponseNode::NotStarted => None,
            },
        )
    };

    if let Some(body) = response_body {
        return ResolvedHttpGraphResponse {
            response: response_from_output(body, response_headers, response_status),
            upstream_latency_ms: current_upstream_latency_ms(context),
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
                };
            }
            Err(UpstreamResponseStartError::UpstreamRequest(_)) => {
                return ResolvedHttpGraphResponse {
                    response: text_response(StatusCode::BAD_GATEWAY, "bad gateway"),
                    upstream_latency_ms: current_upstream_latency_ms(context),
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
        };
    };

    match response_from_upstream_snapshot(snapshot, response_headers, response_status).await {
        Ok(response) => ResolvedHttpGraphResponse {
            response,
            upstream_latency_ms: current_upstream_latency_ms(context),
        },
        Err(_) => ResolvedHttpGraphResponse {
            response: text_response(StatusCode::BAD_GATEWAY, "bad gateway"),
            upstream_latency_ms: current_upstream_latency_ms(context),
        },
    }
}

pub(crate) async fn read_request_body_all(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let body = {
        let guard = context.lock().expect("vm context lock poisoned");
        guard.inbound_request_body.clone()
    };
    let mut inbound = body.lock().await;
    inbound.read_all().await
}

pub(crate) async fn consume_request_body_all(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let body = {
        let guard = context.lock().expect("vm context lock poisoned");
        guard.inbound_request_body.clone()
    };
    let mut inbound = body.lock().await;
    inbound.read_all_and_consume().await
}

pub(crate) async fn read_request_body_next_chunk(
    context: &SharedProxyVmContext,
    max_bytes: usize,
) -> Result<Vec<u8>, VmError> {
    let body = {
        let guard = context.lock().expect("vm context lock poisoned");
        guard.inbound_request_body.clone()
    };
    let mut inbound = body.lock().await;
    inbound.read_next_chunk(max_bytes).await
}

pub(crate) async fn read_request_body_next_line(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let body = {
        let guard = context.lock().expect("vm context lock poisoned");
        guard.inbound_request_body.clone()
    };
    let mut inbound = body.lock().await;
    inbound.read_next_line().await
}

pub(crate) async fn request_body_eof(context: &SharedProxyVmContext) -> Result<bool, VmError> {
    let body = {
        let guard = context.lock().expect("vm context lock poisoned");
        guard.inbound_request_body.clone()
    };
    let mut inbound = body.lock().await;
    inbound.eof().await
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{HeaderMap, Request},
        routing::any,
    };

    use super::{
        HttpCarrierRef, HttpExchangeTransportState, ProxyVmContext, SharedProxyVmContext,
        allocate_outbound_exchange_handle, append_outbound_exchange_body,
        ensure_outbound_exchange_response_started, outbound_exchange_exists,
    };
    use crate::abi_impl::RateLimiterStore;
    use crate::abi_impl::http2::{Http2DownstreamStreamAttachment, Http2StreamRef};

    fn test_context() -> SharedProxyVmContext {
        Arc::new(Mutex::new(ProxyVmContext::from_request_headers(
            HeaderMap::new(),
            Arc::new(Mutex::new(RateLimiterStore::new())),
        )))
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

        let guard = context.lock().expect("vm context lock poisoned");
        assert_eq!(
            guard.outbound_exchanges[&first]
                .request
                .body_override
                .as_deref(),
            Some("alpha".as_bytes())
        );
        assert_eq!(
            guard.outbound_exchanges[&second]
                .request
                .body_override
                .as_deref(),
            Some("beta".as_bytes())
        );
        assert_eq!(guard.outbound_exchanges[&first].request.target, None);
        assert_eq!(guard.outbound_exchanges[&second].request.target, None);
        assert!(!guard.outbound_exchanges[&first].response_ready());
        assert!(!guard.outbound_exchanges[&second].response_ready());
    }

    #[test]
    fn downstream_http2_attachment_updates_explicit_carrier_ref() {
        let mut context = ProxyVmContext::from_request_headers(
            HeaderMap::new(),
            Arc::new(Mutex::new(RateLimiterStore::new())),
        );
        assert_eq!(
            context.downstream_carrier_ref,
            Some(HttpCarrierRef::DownstreamHttp1)
        );

        context.attach_downstream_http2_stream(&Http2DownstreamStreamAttachment {
            session_id: 41,
            stream_id: 9,
        });

        assert_eq!(
            context.downstream_carrier_ref,
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

        let context = test_context();
        {
            let mut guard = context.lock().expect("vm context lock poisoned");
            guard.attach_upstream_client(reqwest::Client::new());
        }

        let first = allocate_outbound_exchange_handle(&context).expect("first handle should exist");
        let second =
            allocate_outbound_exchange_handle(&context).expect("second handle should exist");
        append_outbound_exchange_body(&context, first, "one")
            .expect("first exchange write should succeed");

        {
            let mut guard = context.lock().expect("vm context lock poisoned");
            let exchange = guard
                .outbound_exchanges
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

        let guard = context.lock().expect("vm context lock poisoned");
        assert!(guard.outbound_exchanges[&first].response_ready());
        assert!(
            guard.outbound_exchanges[&first]
                .transport
                .tcp_flow
                .is_connected()
        );
        assert!(
            !guard.outbound_exchanges[&first]
                .transport
                .tls_flow
                .is_present()
        );
        assert!(!guard.outbound_exchanges[&second].response_ready());
        assert!(
            !guard.outbound_exchanges[&second]
                .transport
                .tcp_flow
                .is_connected()
        );
        assert!(
            !guard.outbound_exchanges[&second]
                .transport
                .tls_flow
                .is_present()
        );
        assert!(!guard.tcp_dag.default_upstream.is_connected());
        assert!(!guard.upstream_response_ready());
    }
}
