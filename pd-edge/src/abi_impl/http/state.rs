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
use futures_util::{Stream, stream::try_unfold};
use http_body_util::{BodyExt, StreamBody};
#[cfg(feature = "http3")]
use hyper::body::Buf;
use hyper::body::{Frame, SizeHint};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use parking_lot::Mutex as ParkingMutex;
use tokio::io::copy_bidirectional;
use tokio::sync::oneshot;
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
use super::outbound_http1::{
    OutboundHttp1ForwardBody, OutboundHttp1ForwardResponse, OutboundHttp1Request,
    OutboundHttp1RequestBody, OutboundHttp1RequestHeaders, OutboundHttp1Scheme,
    OutboundHttp1Target, PlainHttp1ResponseBody, PlainHttp1SenderLease, SharedPlainHttp1SenderPool,
    forward_via_sender_pool, new_shared_plain_http1_sender_pool,
};
use super::outbound_http1_fast_path_eligible;
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

#[cfg(feature = "webrtc")]
use std::sync::MutexGuard;

static NEXT_HTTP_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Default)]
pub(crate) struct LazyRequestId {
    value: Arc<OnceLock<String>>,
}

impl LazyRequestId {
    pub(crate) fn deferred() -> Self {
        Self::default()
    }

    pub(crate) fn from_string(value: String) -> Self {
        let stored = OnceLock::new();
        let _ = stored.set(value);
        Self {
            value: Arc::new(stored),
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        self.value.get_or_init(|| {
            format!(
                "req-{:016x}",
                NEXT_HTTP_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
            )
        })
    }
}

#[derive(Debug)]
pub(crate) struct DownstreamDerivedRequestParts {
    uri: Uri,
    version: Version,
    headers: LazyHttpHeaders,
    connection_metadata: Option<DownstreamConnectionMetadata>,
    query: OnceLock<String>,
    http_version: OnceLock<String>,
    scheme: OnceLock<String>,
    host: OnceLock<String>,
    client_ip: OnceLock<String>,
    port: OnceLock<u16>,
}

impl DownstreamDerivedRequestParts {
    fn new(
        uri: Uri,
        version: Version,
        headers: LazyHttpHeaders,
        connection_metadata: Option<DownstreamConnectionMetadata>,
    ) -> Self {
        Self {
            uri,
            version,
            headers,
            connection_metadata,
            query: OnceLock::new(),
            http_version: OnceLock::new(),
            scheme: OnceLock::new(),
            host: OnceLock::new(),
            client_ip: OnceLock::new(),
            port: OnceLock::new(),
        }
    }

    fn query(&self) -> &str {
        self.query
            .get_or_init(|| self.uri.query().unwrap_or("").to_string())
    }

    fn http_version(&self) -> &str {
        self.http_version
            .get_or_init(|| http_version_label(self.version).to_string())
    }

    fn scheme(&self) -> &str {
        self.scheme.get_or_init(|| {
            resolve_downstream_request_scheme(
                &self.uri,
                &self.headers,
                self.connection_metadata.as_ref(),
            )
        })
    }

    fn host(&self) -> &str {
        self.host
            .get_or_init(|| resolve_downstream_request_host(&self.uri, &self.headers))
    }

    fn client_ip(&self) -> &str {
        self.client_ip.get_or_init(|| {
            resolve_downstream_request_client_ip(&self.headers, self.connection_metadata.as_ref())
        })
    }

    fn port(&self) -> u16 {
        *self.port.get_or_init(|| {
            resolve_downstream_request_port(
                &self.uri,
                &self.headers,
                self.scheme(),
                self.connection_metadata.as_ref(),
            )
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) enum RequestStringField {
    Static(String),
    Query(Arc<DownstreamDerivedRequestParts>),
    HttpVersion(Arc<DownstreamDerivedRequestParts>),
    Scheme(Arc<DownstreamDerivedRequestParts>),
    Host(Arc<DownstreamDerivedRequestParts>),
    ClientIp(Arc<DownstreamDerivedRequestParts>),
}

impl RequestStringField {
    fn as_str(&self) -> &str {
        match self {
            Self::Static(value) => value.as_str(),
            Self::Query(parts) => parts.query(),
            Self::HttpVersion(parts) => parts.http_version(),
            Self::Scheme(parts) => parts.scheme(),
            Self::Host(parts) => parts.host(),
            Self::ClientIp(parts) => parts.client_ip(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum RequestPortField {
    Static(u16),
    Derived(Arc<DownstreamDerivedRequestParts>),
}

impl RequestPortField {
    fn get(&self) -> u16 {
        match self {
            Self::Static(value) => *value,
            Self::Derived(parts) => parts.port(),
        }
    }
}

#[derive(Debug)]
pub struct HttpRequestContext {
    pub(crate) request_id: LazyRequestId,
    pub(crate) method: Method,
    pub(crate) path: String,
    pub(crate) query: RequestStringField,
    pub(crate) http_version: RequestStringField,
    pub(crate) port: RequestPortField,
    pub(crate) scheme: RequestStringField,
    pub(crate) host: RequestStringField,
    pub(crate) client_ip: RequestStringField,
    pub(crate) body: Body,
    pub(crate) headers: LazyHttpHeaders,
}

#[derive(Clone, Debug)]
struct RawHttpHeader {
    name: Bytes,
    value: Bytes,
}

#[derive(Debug)]
struct LazyHttpHeadersInner {
    raw: Option<Arc<[RawHttpHeader]>>,
    parsed: OnceLock<HeaderMap>,
}

#[derive(Clone, Debug)]
pub struct LazyHttpHeaders {
    inner: Arc<LazyHttpHeadersInner>,
}

impl Default for LazyHttpHeaders {
    fn default() -> Self {
        HeaderMap::new().into()
    }
}

impl From<HeaderMap> for LazyHttpHeaders {
    fn from(headers: HeaderMap) -> Self {
        let parsed = OnceLock::new();
        let _ = parsed.set(headers);
        Self {
            inner: Arc::new(LazyHttpHeadersInner { raw: None, parsed }),
        }
    }
}

impl LazyHttpHeaders {
    pub(crate) fn from_raw_header_bytes(raw: Vec<(Bytes, Bytes)>) -> Self {
        let raw = raw
            .into_iter()
            .map(|(name, value)| RawHttpHeader { name, value })
            .collect::<Vec<_>>();
        Self {
            inner: Arc::new(LazyHttpHeadersInner {
                raw: Some(raw.into()),
                parsed: OnceLock::new(),
            }),
        }
    }

    pub(crate) fn headers(&self) -> &HeaderMap {
        self.inner.parsed.get_or_init(|| {
            let mut parsed = HeaderMap::new();
            if let Some(raw) = self.inner.raw.as_ref() {
                for header in raw.iter() {
                    let Ok(name) = HeaderName::from_bytes(&header.name) else {
                        continue;
                    };
                    let Ok(value) = HeaderValue::from_bytes(&header.value) else {
                        continue;
                    };
                    parsed.append(name, value);
                }
            }
            parsed
        })
    }

    pub(crate) fn get_str(&self, name: &str) -> Option<String> {
        if let Some(parsed) = self.inner.parsed.get() {
            return parsed
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
        }
        self.inner.raw.as_ref().and_then(|raw| {
            raw.iter().find_map(|header| {
                header
                    .name
                    .as_ref()
                    .eq_ignore_ascii_case(name.as_bytes())
                    .then(|| std::str::from_utf8(&header.value).ok().map(str::to_string))
                    .flatten()
            })
        })
    }

    pub(crate) fn contains_name(&self, name: &str) -> bool {
        if let Some(parsed) = self.inner.parsed.get() {
            return parsed.contains_key(name);
        }
        self.inner.raw.as_ref().is_some_and(|raw| {
            raw.iter()
                .any(|header| header.name.as_ref().eq_ignore_ascii_case(name.as_bytes()))
        })
    }

    pub(crate) fn header_contains_token(&self, name: &str, token: &str) -> bool {
        if let Some(parsed) = self.inner.parsed.get() {
            return parsed
                .get_all(name)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .flat_map(|value| value.split(','))
                .map(str::trim)
                .any(|value| value.eq_ignore_ascii_case(token));
        }
        self.inner.raw.as_ref().is_some_and(|raw| {
            raw.iter()
                .filter(|header| header.name.as_ref().eq_ignore_ascii_case(name.as_bytes()))
                .filter_map(|header| std::str::from_utf8(&header.value).ok())
                .flat_map(|value| value.split(','))
                .map(str::trim)
                .any(|value| value.eq_ignore_ascii_case(token))
        })
    }

    pub(crate) fn content_length(&self) -> Option<u64> {
        self.get_str(CONTENT_LENGTH.as_str())
            .and_then(|value| value.parse::<u64>().ok())
    }

    pub(crate) fn for_each_header<F>(&self, mut f: F)
    where
        F: FnMut(&str, &[u8]),
    {
        if let Some(parsed) = self.inner.parsed.get() {
            for (name, value) in parsed {
                f(name.as_str(), value.as_bytes());
            }
            return;
        }
        if let Some(raw) = self.inner.raw.as_ref() {
            for header in raw.iter() {
                if let Ok(name) = std::str::from_utf8(&header.name) {
                    f(name, &header.value);
                }
            }
        }
    }
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
    request_id: LazyRequestId,
    method: Method,
    path: String,
    query: RequestStringField,
    http_version: RequestStringField,
    port: RequestPortField,
    scheme: RequestStringField,
    host: RequestStringField,
    client_ip: RequestStringField,
    headers: LazyHttpHeaders,
}

impl HttpRequestHead {
    pub(crate) fn request_id(&self) -> &str {
        self.request_id.as_str()
    }

    pub(crate) fn method(&self) -> &Method {
        &self.method
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn query(&self) -> &str {
        self.query.as_str()
    }

    pub(crate) fn http_version(&self) -> &str {
        self.http_version.as_str()
    }

    pub(crate) fn port(&self) -> u16 {
        self.port.get()
    }

    pub(crate) fn scheme(&self) -> &str {
        self.scheme.as_str()
    }

    pub(crate) fn host(&self) -> &str {
        self.host.as_str()
    }

    pub(crate) fn client_ip(&self) -> &str {
        self.client_ip.as_str()
    }

    pub(crate) fn headers(&self) -> &HeaderMap {
        self.headers.headers()
    }

    pub(crate) fn lazy_headers(&self) -> &LazyHttpHeaders {
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

    fn is_pristine_unread(&self) -> bool {
        self.stream.buffered.is_empty() && self.stream.read_offset == 0 && !self.stream.eof
    }
}

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
pub(crate) struct HttpResponseOutputNode {
    pub(crate) headers: HeaderMap,
    pub(crate) body: Option<Vec<u8>>,
    pub(crate) status: Option<u16>,
    pub(crate) body_source_exchange: Option<i64>,
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
enum UpstreamResponseSource {
    #[cfg_attr(not(feature = "http2"), allow(dead_code))]
    Hyper(hyper::body::Incoming),
    PlainHttp1(PlainHttp1ResponseBody),
    #[cfg(feature = "http3")]
    Http3(Box<h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>>),
    Exhausted,
}

struct UpstreamResponseBodySource {
    source: UpstreamResponseSource,
    http2_tracker: Option<http2::Http2ResponseBodyTracker>,
    http3_tracker: Option<http3::Http3ResponseBodyTracker>,
    plain_http1_sender_lease: Option<PlainHttp1SenderLease>,
    remaining_body_bytes: Option<u64>,
    body_started: bool,
    body_finished: bool,
    trailers: Option<HeaderMap>,
}

impl Default for UpstreamResponseBodySource {
    fn default() -> Self {
        Self {
            source: UpstreamResponseSource::Exhausted,
            http2_tracker: None,
            http3_tracker: None,
            plain_http1_sender_lease: None,
            remaining_body_bytes: None,
            body_started: false,
            body_finished: false,
            trailers: None,
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
            if let Some(lease) = self.plain_http1_sender_lease.as_mut() {
                lease.release();
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
                UpstreamResponseSource::Hyper(body) => match body.frame().await {
                    Some(Ok(frame)) => match frame.into_data() {
                        Ok(chunk) => {
                            self.note_chunk_delivered(chunk.len());
                            Ok(BufferedByteStreamPull::Chunk(chunk))
                        }
                        Err(frame) => match frame.into_trailers() {
                            Ok(trailers) => {
                                self.trailers = Some(trailers);
                                self.note_body_complete();
                                self.source = UpstreamResponseSource::Exhausted;
                                Ok(BufferedByteStreamPull::Eof)
                            }
                            Err(_) => Ok(BufferedByteStreamPull::Skip),
                        },
                    },
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
                UpstreamResponseSource::PlainHttp1(body) => match body.pull_next().await? {
                    Some(chunk) => {
                        self.note_chunk_delivered(chunk.len());
                        Ok(BufferedByteStreamPull::Chunk(chunk))
                    }
                    None => {
                        self.trailers = body.take_trailers();
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
    trailers_sent: bool,
}

impl StreamingUpstreamResponseBodyState {
    async fn next_frame(&mut self) -> Result<Option<Frame<Bytes>>, VmError> {
        if let Some(prefix) = self.prefix.take()
            && !prefix.is_empty()
        {
            return Ok(Some(Frame::data(prefix)));
        }

        loop {
            match self.source.pull_next().await? {
                BufferedByteStreamPull::Chunk(chunk) => {
                    if !chunk.is_empty() {
                        return Ok(Some(Frame::data(chunk)));
                    }
                }
                BufferedByteStreamPull::Skip => {}
                BufferedByteStreamPull::Eof => {
                    if !self.trailers_sent
                        && let Some(trailers) = self.source.trailers.take()
                    {
                        self.trailers_sent = true;
                        return Ok(Some(Frame::trailers(trailers)));
                    }
                    return Ok(None);
                }
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
    plain_http1_sender_lease: Option<PlainHttp1SenderLease>,
    content_length: Option<u64>,
) -> UpstreamResponseBodySource {
    let mut source = UpstreamResponseBodySource {
        source,
        http2_tracker,
        http3_tracker,
        plain_http1_sender_lease,
        remaining_body_bytes: content_length,
        body_started: false,
        body_finished: false,
        trailers: None,
    };
    if matches!(content_length, Some(0)) {
        source.note_body_complete();
    }
    source
}

impl UpstreamResponseBodyState {
    fn empty() -> Self {
        Self {
            source: UpstreamResponseBodySource::default(),
            stream: BufferedByteStream {
                eof: true,
                ..BufferedByteStream::default()
            },
        }
    }

    #[cfg_attr(not(feature = "http2"), allow(dead_code))]
    fn from_hyper(
        body: hyper::body::Incoming,
        http2_tracker: Option<http2::Http2ResponseBodyTracker>,
        plain_http1_sender_lease: Option<PlainHttp1SenderLease>,
        content_length: Option<u64>,
    ) -> Self {
        if matches!(content_length, Some(0)) {
            if let Some(mut lease) = plain_http1_sender_lease {
                lease.release();
            }
            return Self::empty();
        }
        Self {
            source: upstream_response_body_source(
                UpstreamResponseSource::Hyper(body),
                http2_tracker,
                None,
                plain_http1_sender_lease,
                content_length,
            ),
            stream: BufferedByteStream::default(),
        }
    }

    fn from_plain_http1(body: PlainHttp1ResponseBody, content_length: Option<u64>) -> Self {
        if matches!(content_length, Some(0)) {
            return Self::empty();
        }
        Self {
            source: upstream_response_body_source(
                UpstreamResponseSource::PlainHttp1(body),
                None,
                None,
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
        if matches!(content_length, Some(0)) {
            return Self::empty();
        }
        Self {
            source: upstream_response_body_source(
                UpstreamResponseSource::Http3(Box::new(request_stream)),
                None,
                http3_tracker,
                None,
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

    async fn read_trailers(&mut self) -> Result<HeaderMap, VmError> {
        let _ = self.stream.read_all(&mut self.source).await?;
        Ok(self.source.trailers.clone().unwrap_or_default())
    }

    fn is_known_empty(&self) -> bool {
        self.stream.eof && self.stream.buffered.is_empty()
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
            trailers_sent: false,
        }
    }
}

type SharedUpstreamResponseBody = Arc<tokio::sync::Mutex<UpstreamResponseBodyState>>;
type SharedHttpHeaders = Arc<HeaderMap>;

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
    native_default_upstream_forward_response: Option<NativeDefaultUpstreamForwardResponse>,
    native_default_upstream_forward_task: Option<NativeDefaultUpstreamForwardTask>,
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
            native_default_upstream_forward_response: None,
            native_default_upstream_forward_task: None,
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
            native_default_upstream_forward_response: None,
            native_default_upstream_forward_task: None,
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
                scheme: request_head.scheme.clone(),
                host: request_head.host.clone(),
                http_version: request_head.http_version.clone(),
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
                path: "/".to_string(),
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
            path: "/".to_string(),
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

    pub(crate) fn insert_downstream_response_header(&self, name: HeaderName, value: HeaderValue) {
        let mut downstream = self.lock_downstream();
        downstream.vm_touches.response_headers_mutated = true;
        if downstream.post_response_plan.is_none()
            && downstream.response_output.body.is_none()
            && downstream.response_output.body_source_exchange.is_none()
            && downstream.response_output.status.is_none()
            && downstream.response_output.headers.is_empty()
            && let Some(native_response) =
                downstream.native_default_upstream_forward_response.as_mut()
        {
            native_response.headers.insert(name, value);
            return;
        }
        downstream.response_output.headers.insert(name, value);
    }

    pub(crate) fn insert_downstream_response_headers(&self, headers: HeaderMap) {
        let mut downstream = self.lock_downstream();
        downstream.vm_touches.response_headers_mutated = true;
        if downstream.post_response_plan.is_none()
            && downstream.response_output.body.is_none()
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
            return;
        }
        for (name, value) in headers {
            if let Some(name) = name {
                downstream.response_output.headers.insert(name, value);
            }
        }
    }

    pub(crate) fn set_downstream_response_status(&self, status: u16) {
        let mut downstream = self.lock_downstream();
        downstream.vm_touches.response_status_mutated = true;
        if downstream.post_response_plan.is_none()
            && downstream.response_output.body.is_none()
            && downstream.response_output.body_source_exchange.is_none()
            && downstream.response_output.headers.is_empty()
            && downstream.response_output.status.is_none()
            && let Some(native_response) =
                downstream.native_default_upstream_forward_response.as_mut()
        {
            native_response.status = status;
            return;
        }
        downstream.response_output.status = Some(status);
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
        downstream.native_default_upstream_forward_response = None;
        if let Some(task) = downstream.native_default_upstream_forward_task.take() {
            task.abort();
        }
    }

    fn begin_native_default_upstream_http_forward(&self) {
        let mut downstream = self.lock_downstream();
        downstream.native_default_upstream_http_forward = true;
        downstream.native_default_upstream_forward_response = None;
        downstream.native_default_upstream_forward_task = None;
    }

    fn store_native_default_upstream_forward_task(&self, task: NativeDefaultUpstreamForwardTask) {
        let mut downstream = self.lock_downstream();
        downstream.native_default_upstream_http_forward = true;
        downstream.native_default_upstream_forward_response = None;
        downstream.native_default_upstream_forward_task = Some(task);
    }

    fn take_native_default_upstream_forward_response(
        &self,
    ) -> Option<NativeDefaultUpstreamForwardResponse> {
        self.lock_downstream()
            .native_default_upstream_forward_response
            .take()
    }

    fn native_default_upstream_forward_response_ready(&self) -> bool {
        self.lock_downstream()
            .native_default_upstream_forward_response
            .is_some()
    }

    fn take_native_default_upstream_forward_task(
        &self,
    ) -> Option<NativeDefaultUpstreamForwardTask> {
        self.lock_downstream()
            .native_default_upstream_forward_task
            .take()
    }

    fn native_default_upstream_forward_task_pending(&self) -> bool {
        self.lock_downstream()
            .native_default_upstream_forward_task
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

pub(crate) fn append_response_output_body_bytes(context: &SharedProxyVmContext, bytes: &[u8]) {
    context.lock_transport().tcp_dag.downstream.note_write();
    let mut downstream = context.lock_downstream();
    downstream.vm_touches.response_body_mutated = true;
    downstream.response_output.body_source_exchange = None;
    downstream
        .response_output
        .body
        .get_or_insert_with(Vec::new)
        .extend_from_slice(bytes);
    sync_response_output_body_headers(&mut downstream.response_output);
}

pub(crate) fn sync_response_output_body_headers(response: &mut HttpResponseOutputNode) {
    if let Some(body) = response.body.as_ref() {
        response.headers.remove(TRANSFER_ENCODING);
        if let Ok(value) = HeaderValue::from_str(&body.len().to_string()) {
            response.headers.insert(CONTENT_LENGTH, value);
        }
    }
}

#[derive(Debug)]
enum UpstreamResponseStartError {
    UnknownExchangeHandle(i64),
    MissingTarget,
    MissingClient,
    Protocol(String),
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
            | Self::ResolveOutboundBody(message)
            | Self::UpstreamRequest(message) => VmError::HostError(message.clone()),
        }
    }
}

#[derive(Clone, Debug)]
struct PreparedUpstreamRequest {
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
    target_host: Option<String>,
    target_host_arc: Option<Arc<str>>,
    target_port: Option<u16>,
    target_host_header: Option<String>,
    target_authority: Option<Arc<str>>,
    target_plain_http1_pool_key: Option<Arc<str>>,
    target_inherits_request_path: bool,
    target_scheme: HttpUpstreamScheme,
}

#[derive(Clone, Debug)]
enum DefaultUpstreamRequestHead {
    Inherit {
        header_overrides: HeaderMap,
    },
    Explicit {
        method: Method,
        path: String,
        query: String,
        headers: HeaderMap,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct DefaultUpstreamRequestSnapshot {
    version_preference: HttpVersionPreference,
    target: Option<String>,
    target_host: Option<String>,
    target_host_arc: Option<Arc<str>>,
    target_port: Option<u16>,
    target_host_header: Option<String>,
    target_authority: Option<Arc<str>>,
    target_plain_http1_pool_key: Option<Arc<str>>,
    target_inherits_request_path: bool,
    target_scheme: HttpUpstreamScheme,
    head: DefaultUpstreamRequestHead,
}

impl DefaultUpstreamRequestSnapshot {
    pub(crate) fn from_request(request: &HttpOutboundRequestNode) -> Self {
        let head = if request.inherits_request_head {
            DefaultUpstreamRequestHead::Inherit {
                header_overrides: request.inherited_header_overrides.clone(),
            }
        } else {
            DefaultUpstreamRequestHead::Explicit {
                method: request.method.clone(),
                path: request.path.clone(),
                query: request.query.clone(),
                headers: request.headers.clone(),
            }
        };
        Self {
            version_preference: request.version_preference,
            target: request.target.clone(),
            target_host: request.target_host.clone(),
            target_host_arc: request.target_host_arc.clone(),
            target_port: request.target_port,
            target_host_header: request.target_host_header.clone(),
            target_authority: request.target_authority.clone(),
            target_plain_http1_pool_key: request.target_plain_http1_pool_key.clone(),
            target_inherits_request_path: request.target_inherits_request_path,
            target_scheme: request.target_scheme,
            head,
        }
    }

    fn method_or_request_head<'a>(&'a self, request_head: &'a HttpRequestHead) -> &'a Method {
        match &self.head {
            DefaultUpstreamRequestHead::Inherit { .. } => request_head.method(),
            DefaultUpstreamRequestHead::Explicit { method, .. } => method,
        }
    }

    fn path_or_request_head<'a>(&'a self, request_head: &'a HttpRequestHead) -> &'a str {
        match &self.head {
            DefaultUpstreamRequestHead::Inherit { .. } => request_head.path(),
            DefaultUpstreamRequestHead::Explicit { path, .. } => path,
        }
    }

    fn query_or_request_head<'a>(&'a self, request_head: &'a HttpRequestHead) -> &'a str {
        match &self.head {
            DefaultUpstreamRequestHead::Inherit { .. } => request_head.query(),
            DefaultUpstreamRequestHead::Explicit { query, .. } => query,
        }
    }

    fn cloned_headers_or_request_head(&self, request_head: &HttpRequestHead) -> HeaderMap {
        match &self.head {
            DefaultUpstreamRequestHead::Inherit { header_overrides } => {
                let mut headers = request_head.headers().clone();
                merge_headers(&mut headers, header_overrides);
                headers
            }
            DefaultUpstreamRequestHead::Explicit { headers, .. } => headers.clone(),
        }
    }

    fn filtered_headers_or_request_head(
        &self,
        request_head: &HttpRequestHead,
        host_header: Option<&str>,
    ) -> HeaderMap {
        match &self.head {
            DefaultUpstreamRequestHead::Inherit { header_overrides } => {
                let mut headers = filtered_upstream_headers(request_head.headers(), host_header);
                merge_headers(&mut headers, header_overrides);
                headers
            }
            DefaultUpstreamRequestHead::Explicit { headers, .. } => {
                filtered_upstream_headers(headers, host_header)
            }
        }
    }

    fn outbound_http1_headers_or_request_head(
        &self,
        request_head: &HttpRequestHead,
        host_header: Option<&str>,
    ) -> OutboundHttp1RequestHeaders {
        match &self.head {
            DefaultUpstreamRequestHead::Inherit { header_overrides }
                if header_overrides.is_empty() =>
            {
                OutboundHttp1RequestHeaders::InheritedFiltered {
                    headers: request_head.lazy_headers().clone(),
                    host_header: host_header.map(str::to_string),
                }
            }
            _ => self
                .filtered_headers_or_request_head(request_head, host_header)
                .into(),
        }
    }
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

impl std::fmt::Debug for StartedUpstreamResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartedUpstreamResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("version", &self.version)
            .field("carrier_ref", &self.carrier_ref)
            .field("peer_addr", &self.peer_addr)
            .field("negotiated_alpn", &self.negotiated_alpn)
            .field(
                "peer_certificate_der_len",
                &self.peer_certificate_der.as_ref().map(Vec::len),
            )
            .finish()
    }
}

type NativeDefaultUpstreamForwardBody = OutboundHttp1ForwardBody;

type NativeDefaultUpstreamForwardTask = tokio::task::JoinHandle<
    Result<NativeDefaultUpstreamForwardResponse, UpstreamResponseStartError>,
>;

type NativeDefaultUpstreamForwardResponse = OutboundHttp1ForwardResponse;

#[derive(Debug)]
pub(crate) struct ResolvedNativeHttp1DownstreamResponse {
    pub(crate) response: NativeDefaultUpstreamForwardResponse,
    pub(crate) response_headers: HeaderMap,
    pub(crate) response_status: Option<u16>,
    pub(crate) upstream_latency_ms: u64,
}

#[derive(Debug)]
pub(crate) struct ResolvedNativeLocalHttp1DownstreamResponse {
    pub(crate) status: u16,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Vec<u8>,
    pub(crate) default_content_type: bool,
}

pub(crate) struct DownstreamHttpBodyPassthrough {
    inner: StreamingUpstreamResponseBodyState,
}

impl DownstreamHttpBodyPassthrough {
    pub(crate) async fn next_frame(&mut self) -> Result<Option<Frame<Bytes>>, VmError> {
        self.inner.next_frame().await
    }
}

pub(crate) enum SnapshotHttp1DownstreamHeaders {
    Snapshot {
        base: Arc<HeaderMap>,
        overlay: HeaderMap,
    },
    Explicit(HeaderMap),
}

impl SnapshotHttp1DownstreamHeaders {
    pub(crate) fn contains_name(&self, name: HeaderName) -> bool {
        match self {
            Self::Snapshot { base, overlay } => {
                overlay.contains_key(&name)
                    || (!is_hop_by_hop_header(&name) && base.contains_key(&name))
            }
            Self::Explicit(headers) => headers.contains_key(&name),
        }
    }

    pub(crate) fn header_contains_token(&self, name: HeaderName, token: &str) -> bool {
        let contains = |headers: &HeaderMap| {
            headers
                .get_all(&name)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .flat_map(|value| value.split(','))
                .map(str::trim)
                .any(|value| value.eq_ignore_ascii_case(token))
        };
        match self {
            Self::Snapshot { base, overlay } => {
                if overlay.contains_key(&name) {
                    contains(overlay)
                } else if is_hop_by_hop_header(&name) {
                    false
                } else {
                    contains(base)
                }
            }
            Self::Explicit(headers) => contains(headers),
        }
    }

    pub(crate) fn connection_keep_alive(&self, version: Version) -> bool {
        let connection_close = self.header_contains_token(CONNECTION, "close");
        let connection_keep_alive = self.header_contains_token(CONNECTION, "keep-alive");
        match version {
            Version::HTTP_10 => connection_keep_alive && !connection_close,
            _ => !connection_close,
        }
    }

    pub(crate) fn insert_override(&mut self, name: HeaderName, value: HeaderValue) {
        match self {
            Self::Snapshot { overlay, .. } => {
                overlay.insert(name, value);
            }
            Self::Explicit(headers) => {
                headers.insert(name, value);
            }
        }
    }

    pub(crate) fn write_http1_lines(&self, head: &mut bytes::BytesMut) {
        match self {
            Self::Snapshot { base, overlay } => {
                let overridden: HashSet<HeaderName> = overlay.keys().cloned().collect();
                for (name, value) in base.iter() {
                    if overridden.contains(name) || is_hop_by_hop_header(name) {
                        continue;
                    }
                    head.extend_from_slice(name.as_str().as_bytes());
                    head.extend_from_slice(b": ");
                    head.extend_from_slice(value.as_bytes());
                    head.extend_from_slice(b"\r\n");
                }
                for (name, value) in overlay.iter() {
                    head.extend_from_slice(name.as_str().as_bytes());
                    head.extend_from_slice(b": ");
                    head.extend_from_slice(value.as_bytes());
                    head.extend_from_slice(b"\r\n");
                }
            }
            Self::Explicit(headers) => {
                for (name, value) in headers.iter() {
                    head.extend_from_slice(name.as_str().as_bytes());
                    head.extend_from_slice(b": ");
                    head.extend_from_slice(value.as_bytes());
                    head.extend_from_slice(b"\r\n");
                }
            }
        }
    }
}

pub(crate) struct ResolvedSnapshotHttp1DownstreamResponse {
    pub(crate) status: u16,
    pub(crate) headers: SnapshotHttp1DownstreamHeaders,
    pub(crate) version: Version,
    pub(crate) upstream_latency_ms: u64,
    body: SharedUpstreamResponseBody,
}

impl ResolvedSnapshotHttp1DownstreamResponse {
    pub(crate) async fn take_body_passthrough(&self) -> Option<DownstreamHttpBodyPassthrough> {
        let mut body = self.body.lock().await;
        if body.is_known_empty() {
            None
        } else {
            Some(DownstreamHttpBodyPassthrough {
                inner: body.take_streaming_passthrough(),
            })
        }
    }

    pub(crate) fn into_head(self) -> (u16, SnapshotHttp1DownstreamHeaders, Version, u64) {
        (
            self.status,
            self.headers,
            self.version,
            self.upstream_latency_ms,
        )
    }
}

#[derive(Debug)]
pub(crate) struct ResolvedHttpGraphResponse {
    pub response: Response<Body>,
    pub upstream_latency_ms: u64,
    pub post_response_plan: Option<DownstreamPostResponsePlan>,
}

pub(crate) enum Http1DownstreamResolution {
    NativeLocal(ResolvedNativeLocalHttp1DownstreamResponse),
    Native(Result<ResolvedNativeHttp1DownstreamResponse, Response<Body>>),
    Snapshot(Result<ResolvedSnapshotHttp1DownstreamResponse, Response<Body>>),
    Graph(ResolvedHttpGraphResponse),
}

#[derive(Clone, Debug)]
struct DownstreamHttp1ResolutionState {
    response_headers: HeaderMap,
    response_status: Option<u16>,
    has_post_response_plan: bool,
    has_response_body: bool,
    body_source_exchange: Option<i64>,
    has_upstream_target: bool,
    default_upstream_websocket_mode: bool,
    native_forward_active: bool,
    default_upstream_response_body_read: bool,
    body_source_exchange_response_read: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeDefaultUpstreamRequestBodyMode {
    Empty,
    BufferedImmediate,
    StreamInbound,
    BufferRemaining,
}

#[derive(Clone, Debug)]
enum NativeDefaultUpstreamRequestBodyTemplate {
    Empty,
    Bytes(Bytes),
    Streaming { content_length: Option<u64> },
}

type StreamingInboundBodyStream =
    Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send + 'static>>;

struct StreamingInboundHttpBody {
    stream: ParkingMutex<StreamingInboundBodyStream>,
    content_length: Option<u64>,
}

impl StreamingInboundHttpBody {
    fn new(stream: StreamingInboundBodyStream, content_length: Option<u64>) -> Self {
        Self {
            stream: ParkingMutex::new(stream),
            content_length,
        }
    }
}

impl std::fmt::Debug for StreamingInboundHttpBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamingInboundHttpBody")
            .field("content_length", &self.content_length)
            .finish()
    }
}

impl hyper::body::Body for StreamingInboundHttpBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut stream = self.stream.lock();
        match Stream::poll_next(stream.as_mut(), cx) {
            Poll::Ready(Some(Ok(chunk))) => Poll::Ready(Some(Ok(Frame::data(chunk)))),
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(err))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        false
    }

    fn size_hint(&self) -> SizeHint {
        let mut hint = SizeHint::new();
        if let Some(content_length) = self.content_length {
            hint.set_exact(content_length);
        }
        hint
    }
}

const OUTBOUND_HTTP1_REQUEST_BODY_STREAM_CHUNK_BYTES: usize = 16 * 1024;

pub async fn resolve_outbound_request_body(
    context: &SharedProxyVmContext,
) -> Result<Bytes, VmError> {
    let (body_override, request_body_known_empty) = {
        let exchanges = context.lock_exchanges();
        let exchange = exchanges
            .exchanges
            .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
            .expect("default upstream exchange should exist");
        let request_body_known_empty = request_body_known_empty_for_exchange(context, exchange);
        (
            exchange.request.body_override.clone(),
            request_body_known_empty,
        )
    };

    if let Some(body) = body_override {
        return Ok(Bytes::from(body));
    }

    if request_body_known_empty {
        return Ok(Bytes::new());
    }

    let mut inbound = context.inbound_request_body.lock().await;
    inbound.read_all().await.map(Bytes::from)
}

async fn native_default_upstream_request_body_mode(
    context: &SharedProxyVmContext,
) -> NativeDefaultUpstreamRequestBodyMode {
    let (body_override, request_body_known_empty) = {
        let exchanges = context.lock_exchanges();
        let exchange = exchanges
            .exchanges
            .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
            .expect("default upstream exchange should exist");
        (
            exchange.request.body_override.clone(),
            request_body_known_empty_for_exchange(context, exchange),
        )
    };

    if let Some(body_override) = body_override {
        if body_override.is_empty() {
            return NativeDefaultUpstreamRequestBodyMode::Empty;
        }
        return NativeDefaultUpstreamRequestBodyMode::BufferedImmediate;
    }

    if request_body_known_empty {
        return NativeDefaultUpstreamRequestBodyMode::Empty;
    }

    let inbound = context.inbound_request_body.lock().await;
    if inbound.is_pristine_unread() {
        NativeDefaultUpstreamRequestBodyMode::StreamInbound
    } else {
        NativeDefaultUpstreamRequestBodyMode::BufferRemaining
    }
}

fn stream_remaining_inbound_request_body(
    context: SharedProxyVmContext,
) -> impl futures_util::Stream<Item = Result<Bytes, VmError>> + Send + 'static {
    try_unfold(context, |context| async move {
        let (chunk, drained) = {
            let mut inbound = context.inbound_request_body.lock().await;
            let chunk = inbound
                .read_next_chunk(OUTBOUND_HTTP1_REQUEST_BODY_STREAM_CHUNK_BYTES)
                .await;
            let drained = chunk.as_ref().is_ok_and(|_| inbound.is_drained());
            (chunk, drained)
        };

        match chunk {
            Ok(chunk) => {
                if chunk.is_empty() {
                    mark_downstream_transport_closed(&context);
                    Ok(None)
                } else {
                    if drained {
                        mark_downstream_transport_closed(&context);
                    }
                    Ok(Some((Bytes::from(chunk), context)))
                }
            }
            Err(err) => {
                mark_downstream_transport_failed(&context, &err.to_string());
                Err(err)
            }
        }
    })
}

async fn default_upstream_outbound_http1_request_body_template(
    context: &SharedProxyVmContext,
) -> Result<NativeDefaultUpstreamRequestBodyTemplate, UpstreamResponseStartError> {
    let (body_override, request_body_known_empty, content_length) = {
        let exchanges = context.lock_exchanges();
        let exchange = exchanges
            .exchanges
            .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
            .expect("default upstream exchange should exist");
        (
            exchange.request.body_override.clone(),
            request_body_known_empty_for_exchange(context, exchange),
            context.with_request_head(|request_head| request_head.lazy_headers().content_length()),
        )
    };

    if let Some(body_override) = body_override {
        return Ok(if body_override.is_empty() {
            NativeDefaultUpstreamRequestBodyTemplate::Empty
        } else {
            NativeDefaultUpstreamRequestBodyTemplate::Bytes(Bytes::from(body_override))
        });
    }

    if request_body_known_empty {
        return Ok(NativeDefaultUpstreamRequestBodyTemplate::Empty);
    }

    match native_default_upstream_request_body_mode(context).await {
        NativeDefaultUpstreamRequestBodyMode::Empty => {
            Ok(NativeDefaultUpstreamRequestBodyTemplate::Empty)
        }
        NativeDefaultUpstreamRequestBodyMode::BufferedImmediate
        | NativeDefaultUpstreamRequestBodyMode::BufferRemaining => {
            let request_body = resolve_outbound_request_body(context)
                .await
                .map_err(|err| {
                    UpstreamResponseStartError::ResolveOutboundBody(format!(
                        "failed to resolve outbound exchange body: {err}",
                    ))
                })?;
            Ok(if request_body.is_empty() {
                NativeDefaultUpstreamRequestBodyTemplate::Empty
            } else {
                NativeDefaultUpstreamRequestBodyTemplate::Bytes(request_body)
            })
        }
        NativeDefaultUpstreamRequestBodyMode::StreamInbound => {
            Ok(NativeDefaultUpstreamRequestBodyTemplate::Streaming { content_length })
        }
    }
}

fn stream_remaining_inbound_request_body_io(
    context: SharedProxyVmContext,
) -> impl futures_util::Stream<Item = Result<Bytes, io::Error>> + Send + 'static {
    stream_remaining_inbound_request_body(context)
        .map(|result| result.map_err(|err| io::Error::other(err.to_string())))
}

fn streaming_inbound_http_body(
    context: &SharedProxyVmContext,
    content_length: Option<u64>,
) -> StreamingInboundHttpBody {
    StreamingInboundHttpBody::new(
        Box::pin(stream_remaining_inbound_request_body_io(context.clone())),
        content_length,
    )
}

fn into_http_body_from_default_upstream_template(
    context: &SharedProxyVmContext,
    request_body: NativeDefaultUpstreamRequestBodyTemplate,
) -> (Body, Option<u64>, bool) {
    match request_body {
        NativeDefaultUpstreamRequestBodyTemplate::Empty => (Body::empty(), Some(0), false),
        NativeDefaultUpstreamRequestBodyTemplate::Bytes(body) => {
            let content_length = u64::try_from(body.len()).unwrap_or(u64::MAX);
            (Body::from(body), Some(content_length), content_length > 0)
        }
        NativeDefaultUpstreamRequestBodyTemplate::Streaming { content_length } => {
            let body_present = !matches!(content_length, Some(0));
            (
                Body::new(streaming_inbound_http_body(context, content_length)),
                content_length,
                body_present,
            )
        }
    }
}

#[cfg(feature = "http3")]
fn into_http3_request_body_from_default_upstream_template(
    context: &SharedProxyVmContext,
    request_body: NativeDefaultUpstreamRequestBodyTemplate,
) -> http3::Http3RequestBody {
    match request_body {
        NativeDefaultUpstreamRequestBodyTemplate::Empty => http3::Http3RequestBody::Empty,
        NativeDefaultUpstreamRequestBodyTemplate::Bytes(body) => {
            http3::Http3RequestBody::Bytes(body)
        }
        NativeDefaultUpstreamRequestBodyTemplate::Streaming { .. } => {
            http3::Http3RequestBody::Streaming(Box::pin(stream_remaining_inbound_request_body_io(
                context.clone(),
            )))
        }
    }
}

fn request_body_known_empty_for_exchange(
    context: &SharedProxyVmContext,
    exchange: &HttpOutboundExchangeState,
) -> bool {
    if let Some(body_override) = exchange.request.body_override.as_ref() {
        return body_override.is_empty();
    }

    context.with_request_head(|request_head| {
        request_headers_indicate_empty_body_lazy(request_head.lazy_headers())
    })
}

pub(crate) fn build_configured_upstream_url(
    upstream: &str,
    inherits_request_path: bool,
    host_header: Option<&str>,
    request_path: &str,
    request_query: &str,
) -> (String, Option<String>) {
    if !inherits_request_path {
        return (upstream.to_string(), host_header.map(str::to_string));
    }

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
    (
        format!("{upstream}{path_and_query}"),
        host_header.map(str::to_string),
    )
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
    request_id: LazyRequestId,
    parts: axum::http::request::Parts,
    body: Body,
    connection_metadata: Option<&DownstreamConnectionMetadata>,
) -> HttpRequestContext {
    build_downstream_http_request_context_from_components(
        request_id,
        parts.method,
        parts.uri,
        parts.version,
        body,
        parts.headers.into(),
        connection_metadata,
    )
}

pub(crate) fn build_downstream_http_request_context_from_components(
    request_id: LazyRequestId,
    method: Method,
    uri: axum::http::Uri,
    version: Version,
    body: Body,
    headers: LazyHttpHeaders,
    connection_metadata: Option<&DownstreamConnectionMetadata>,
) -> HttpRequestContext {
    let derived = Arc::new(DownstreamDerivedRequestParts::new(
        uri.clone(),
        version,
        headers.clone(),
        connection_metadata.cloned(),
    ));
    HttpRequestContext {
        request_id,
        method,
        path: uri.path().to_string(),
        query: RequestStringField::Query(derived.clone()),
        http_version: RequestStringField::HttpVersion(derived.clone()),
        port: RequestPortField::Derived(derived.clone()),
        scheme: RequestStringField::Scheme(derived.clone()),
        host: RequestStringField::Host(derived.clone()),
        client_ip: RequestStringField::ClientIp(derived),
        body,
        headers,
    }
}

fn resolve_downstream_request_scheme(
    uri: &axum::http::Uri,
    headers: &LazyHttpHeaders,
    connection_metadata: Option<&DownstreamConnectionMetadata>,
) -> String {
    if let Some(scheme) = uri.scheme_str() {
        return scheme.to_string();
    }
    if let Some(forwarded) = headers
        .get_str("x-forwarded-proto")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return forwarded;
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
    headers: &LazyHttpHeaders,
    scheme: &str,
    connection_metadata: Option<&DownstreamConnectionMetadata>,
) -> u16 {
    if let Some(port) = uri.port_u16() {
        return port;
    }
    if let Some(host_header) = headers
        .get_str(HOST.as_str())
        .map(|value| value.trim().to_string())
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

fn resolve_downstream_request_host(uri: &axum::http::Uri, headers: &LazyHttpHeaders) -> String {
    if let Some(host) = headers
        .get_str(HOST.as_str())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return host;
    }
    uri.authority()
        .map(|authority| authority.as_str().to_string())
        .unwrap_or_default()
}

fn resolve_downstream_request_client_ip(
    headers: &LazyHttpHeaders,
    connection_metadata: Option<&DownstreamConnectionMetadata>,
) -> String {
    if let Some(value) = headers.get_str("x-forwarded-for") {
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
        .get_str("x-real-ip")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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

fn snapshot_default_upstream_request(
    context: &SharedProxyVmContext,
) -> Result<
    (
        DefaultUpstreamRequestSnapshot,
        Option<AttachedHttpTransport>,
    ),
    UpstreamResponseStartError,
> {
    let exchanges = context.lock_exchanges();
    let exchange = exchanges
        .exchanges
        .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
        .expect("default upstream exchange should exist");
    if exchange.websocket_dag.is_websocket_mode() {
        return Err(UpstreamResponseStartError::Protocol(
            "default upstream exchange is already owned by the websocket DAG".to_string(),
        ));
    }
    Ok((
        DefaultUpstreamRequestSnapshot::from_request(&exchange.request),
        exchange.transport.attached_transport,
    ))
}

fn prepared_upstream_request(
    context: &SharedProxyVmContext,
) -> Result<PreparedUpstreamRequest, UpstreamResponseStartError> {
    let (request, attached_transport) = snapshot_default_upstream_request(context)?;
    if request.target.is_none() {
        return Err(UpstreamResponseStartError::MissingTarget);
    }
    let target = request
        .target
        .clone()
        .ok_or(UpstreamResponseStartError::MissingTarget)?;
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
    let (method, path, query, headers) = context.with_request_head(|request_head| {
        (
            request.method_or_request_head(request_head).clone(),
            request.path_or_request_head(request_head).to_string(),
            request.query_or_request_head(request_head).to_string(),
            request.cloned_headers_or_request_head(request_head),
        )
    });
    Ok(PreparedUpstreamRequest {
        http2_sessions: context.services().upstream_http_sessions(),
        http3_sessions: context.services().upstream_http3_sessions(),
        version_preference: request.version_preference,
        http2_mode: http2::select_upstream_mode(
            request.target_scheme,
            &tls_flow,
            request.version_preference,
        ),
        http3_mode: http3::select_upstream_mode(
            request.target_scheme,
            &tls_flow,
            request.version_preference,
        ),
        tls_flow,
        attached_transport,
        method,
        path,
        query,
        headers,
        target,
        target_host: request.target_host.clone(),
        target_host_arc: request.target_host_arc.clone(),
        target_port: request.target_port,
        target_host_header: request.target_host_header.clone(),
        target_authority: request.target_authority.clone(),
        target_plain_http1_pool_key: request.target_plain_http1_pool_key.clone(),
        target_inherits_request_path: request.target_inherits_request_path,
        target_scheme: request.target_scheme,
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

    let (request, attached_transport, tls_flow) = {
        let exchanges = context.lock_exchanges();
        let exchange = exchanges
            .exchanges
            .get(&handle)
            .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
        if exchange.websocket_dag.is_websocket_mode() {
            return Err(UpstreamResponseStartError::Protocol(format!(
                "outbound exchange handle {handle} is already owned by the websocket DAG",
            )));
        }
        let tls_flow = match exchange.transport.attached_transport {
            #[cfg(feature = "tls")]
            Some(AttachedHttpTransport::Tls(session)) => context
                .lock_transport()
                .dynamic_tls_sessions
                .get(&session)
                .cloned()
                .unwrap_or_else(TlsFlowState::for_dynamic_socket),
            _ => exchange.transport.tls_flow.clone(),
        };
        (
            DefaultUpstreamRequestSnapshot::from_request(&exchange.request),
            exchange.transport.attached_transport,
            tls_flow,
        )
    };
    if request.target.is_none() {
        return Err(UpstreamResponseStartError::MissingTarget);
    }
    let target = request
        .target
        .clone()
        .ok_or(UpstreamResponseStartError::MissingTarget)?;
    let (method, path, query, headers) = context.with_request_head(|request_head| {
        (
            request.method_or_request_head(request_head).clone(),
            request.path_or_request_head(request_head).to_string(),
            request.query_or_request_head(request_head).to_string(),
            request.cloned_headers_or_request_head(request_head),
        )
    });
    Ok(PreparedUpstreamRequest {
        http2_sessions: context.services().upstream_http_sessions(),
        http3_sessions: context.services().upstream_http3_sessions(),
        version_preference: request.version_preference,
        http2_mode: http2::select_upstream_mode(
            request.target_scheme,
            &tls_flow,
            request.version_preference,
        ),
        http3_mode: http3::select_upstream_mode(
            request.target_scheme,
            &tls_flow,
            request.version_preference,
        ),
        tls_flow,
        attached_transport,
        method,
        path,
        query,
        headers,
        target,
        target_host: request.target_host.clone(),
        target_host_arc: request.target_host_arc.clone(),
        target_port: request.target_port,
        target_host_header: request.target_host_header.clone(),
        target_authority: request.target_authority.clone(),
        target_plain_http1_pool_key: request.target_plain_http1_pool_key.clone(),
        target_inherits_request_path: request.target_inherits_request_path,
        target_scheme: request.target_scheme,
    })
}

async fn resolve_outbound_exchange_body(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<Bytes, VmError> {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return resolve_outbound_request_body(context).await;
    }

    let guard = context.lock_exchanges();
    let exchange = guard
        .exchanges
        .get(&handle)
        .ok_or_else(|| VmError::HostError(format!("unknown outbound exchange handle {handle}")))?;
    Ok(exchange
        .request
        .body_override
        .clone()
        .map(Bytes::from)
        .unwrap_or_default())
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

fn request_headers_indicate_empty_body_lazy(headers: &LazyHttpHeaders) -> bool {
    matches!(headers.content_length(), Some(0))
        || (!headers.contains_name(CONTENT_LENGTH.as_str())
            && !headers.contains_name(TRANSFER_ENCODING.as_str()))
}

async fn start_upstream_response_via_plain_http1_sender_pool(
    context: &SharedProxyVmContext,
    handle: i64,
    prepared: &PreparedUpstreamRequest,
    request_body: Bytes,
) -> Result<StartedUpstreamResponse, UpstreamResponseStartError> {
    let services = context.services();
    let pool = services
        .plain_http1_sender_pool()
        .ok_or(UpstreamResponseStartError::MissingClient)?;
    let sender_pool_capacity = services.upstream_http_reuse_entries();
    let host = prepared.target_host.clone().ok_or_else(|| {
        UpstreamResponseStartError::Protocol(
            "outbound exchange host should be configured for plain http/1.1 forwarding".to_string(),
        )
    })?;
    let host_arc = prepared
        .target_host_arc
        .clone()
        .unwrap_or_else(|| Arc::from(host.as_str()));
    let port = prepared.target_port.ok_or_else(|| {
        UpstreamResponseStartError::Protocol(
            "outbound exchange port should be configured for plain http/1.1 forwarding".to_string(),
        )
    })?;
    let authority = prepared
        .target_authority
        .clone()
        .unwrap_or_else(|| Arc::from(format_upstream_authority(&host, port)));
    let target = OutboundHttp1Target {
        scheme: match prepared.target_scheme {
            HttpUpstreamScheme::Http => OutboundHttp1Scheme::Http,
            #[cfg(feature = "tls")]
            HttpUpstreamScheme::Https => OutboundHttp1Scheme::Https,
            #[cfg(not(feature = "tls"))]
            HttpUpstreamScheme::Https => {
                return Err(UpstreamResponseStartError::Protocol(
                    "https http/1.1 forwarding requires the tls feature".to_string(),
                ));
            }
        },
        authority: authority.clone(),
        host: host_arc,
        port,
        plain_pool_key: prepared.target_plain_http1_pool_key.clone(),
        #[cfg(feature = "tls")]
        tls_flow: (prepared.target_scheme == HttpUpstreamScheme::Https)
            .then_some(prepared.tls_flow.clone()),
    };
    let request_path = super::request_path_with_query(&prepared.path, &prepared.query);
    let request_body = (!request_body.is_empty()).then_some(request_body);
    let started = Instant::now();
    let response = forward_via_sender_pool(&pool, sender_pool_capacity, &target, started, || {
        Ok(OutboundHttp1Request {
            method: prepared.method.clone(),
            path_and_query: request_path.clone(),
            headers: filtered_upstream_headers(&prepared.headers, Some(authority.as_ref())).into(),
            body: request_body
                .as_ref()
                .map_or(OutboundHttp1RequestBody::Empty, |body| {
                    OutboundHttp1RequestBody::Bytes(body.clone())
                }),
        })
    })
    .await
    .map_err(|err| UpstreamResponseStartError::UpstreamRequest(err.to_string()))?;
    Ok(started_upstream_response_from_plain_http1_forward(
        handle, response,
    ))
}

async fn start_default_upstream_response_via_plain_http1_sender_pool(
    context: &SharedProxyVmContext,
    request: &DefaultUpstreamRequestSnapshot,
) -> Result<StartedUpstreamResponse, UpstreamResponseStartError> {
    let services = context.services();
    let pool = services
        .plain_http1_sender_pool()
        .ok_or(UpstreamResponseStartError::MissingClient)?;
    let sender_pool_capacity = services.upstream_http_reuse_entries();
    let host = request.target_host.clone().ok_or_else(|| {
        UpstreamResponseStartError::Protocol(
            "default upstream host should be configured for plain http/1.1 forwarding".to_string(),
        )
    })?;
    let host_arc = request
        .target_host_arc
        .clone()
        .unwrap_or_else(|| Arc::from(host.as_str()));
    let port = request.target_port.ok_or_else(|| {
        UpstreamResponseStartError::Protocol(
            "default upstream port should be configured for plain http/1.1 forwarding".to_string(),
        )
    })?;
    let authority = request
        .target_authority
        .clone()
        .unwrap_or_else(|| Arc::from(format_upstream_authority(&host, port)));
    let tls_flow = context.lock_transport().tls_dag.default_upstream.clone();
    let target = OutboundHttp1Target {
        scheme: match request.target_scheme {
            HttpUpstreamScheme::Http => OutboundHttp1Scheme::Http,
            #[cfg(feature = "tls")]
            HttpUpstreamScheme::Https => OutboundHttp1Scheme::Https,
            #[cfg(not(feature = "tls"))]
            HttpUpstreamScheme::Https => {
                return Err(UpstreamResponseStartError::Protocol(
                    "https http/1.1 forwarding requires the tls feature".to_string(),
                ));
            }
        },
        authority: authority.clone(),
        host: host_arc,
        port,
        plain_pool_key: request.target_plain_http1_pool_key.clone(),
        #[cfg(feature = "tls")]
        tls_flow: (request.target_scheme == HttpUpstreamScheme::Https).then_some(tls_flow),
    };
    let request_body = default_upstream_outbound_http1_request_body_template(context).await?;
    let request_path = context.with_request_head(|request_head| {
        super::request_path_with_query(
            request.path_or_request_head(request_head),
            request.query_or_request_head(request_head),
        )
    });
    let started = Instant::now();
    let response = forward_via_sender_pool(&pool, sender_pool_capacity, &target, started, || {
        context.with_request_head(|request_head| {
            Ok::<_, VmError>(OutboundHttp1Request {
                method: request.method_or_request_head(request_head).clone(),
                path_and_query: request_path.clone(),
                headers: request
                    .outbound_http1_headers_or_request_head(request_head, Some(authority.as_ref())),
                body: match &request_body {
                    NativeDefaultUpstreamRequestBodyTemplate::Empty => {
                        OutboundHttp1RequestBody::Empty
                    }
                    NativeDefaultUpstreamRequestBodyTemplate::Bytes(body) => {
                        OutboundHttp1RequestBody::Bytes(body.clone())
                    }
                    NativeDefaultUpstreamRequestBodyTemplate::Streaming { content_length } => {
                        OutboundHttp1RequestBody::Streaming {
                            content_length: *content_length,
                            stream: Box::pin(stream_remaining_inbound_request_body(
                                context.clone(),
                            )),
                        }
                    }
                },
            })
        })
    })
    .await
    .map_err(|err| UpstreamResponseStartError::UpstreamRequest(err.to_string()))?;
    Ok(started_upstream_response_from_plain_http1_forward(
        DEFAULT_UPSTREAM_EXCHANGE_HANDLE,
        response,
    ))
}

fn started_upstream_response_from_plain_http1_forward(
    handle: i64,
    upstream_response: OutboundHttp1ForwardResponse,
) -> StartedUpstreamResponse {
    StartedUpstreamResponse {
        status: upstream_response.status,
        headers: upstream_response.headers,
        version: upstream_response.version,
        carrier_ref: if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
            HttpCarrierRef::Http1DefaultUpstream
        } else {
            HttpCarrierRef::Http1DynamicExchange(handle)
        },
        peer_addr: None,
        negotiated_alpn: upstream_response.negotiated_alpn,
        peer_certificate_der: upstream_response.peer_certificate_der,
        body: Arc::new(tokio::sync::Mutex::new(match upstream_response.body {
            OutboundHttp1ForwardBody::Empty => UpstreamResponseBodyState::empty(),
            OutboundHttp1ForwardBody::Raw {
                body,
                content_length,
            } => UpstreamResponseBodyState::from_plain_http1(body, content_length),
        })),
    }
}

fn started_upstream_response_into_snapshot(
    started: StartedUpstreamResponse,
) -> (HttpUpstreamResponseSnapshot, Version, Option<String>) {
    let StartedUpstreamResponse {
        status,
        headers,
        version,
        carrier_ref,
        peer_addr,
        negotiated_alpn: _,
        peer_certificate_der: _,
        body,
    } = started;
    let snapshot = HttpUpstreamResponseSnapshot {
        status,
        headers: Arc::new(headers),
        http_version: http_version_label(version),
        carrier_kind: carrier_ref.kind(),
        carrier_ref: carrier_ref.clone(),
        body,
    };
    (snapshot, version, peer_addr)
}

async fn response_from_started_upstream_response(
    native_response: NativeDefaultUpstreamForwardResponse,
    response_headers: HeaderMap,
    response_status: Option<u16>,
) -> Response<Body> {
    let mut response = Response::new(match native_response.body {
        NativeDefaultUpstreamForwardBody::Empty => Body::empty(),
        NativeDefaultUpstreamForwardBody::Raw {
            body,
            content_length,
        } => {
            let passthrough = UpstreamResponseBodyState::from_plain_http1(body, content_length)
                .take_streaming_passthrough();
            Body::new(StreamBody::new(try_unfold(
                passthrough,
                |mut state| async move {
                    let frame: Option<Frame<Bytes>> = state
                        .next_frame()
                        .await
                        .map_err(|err: VmError| io::Error::other(err.to_string()))?;
                    Ok::<_, io::Error>(frame.map(|frame| (frame, state)))
                },
            )))
        }
    });
    *response.status_mut() = StatusCode::from_u16(native_response.status).unwrap_or(StatusCode::OK);
    *response.version_mut() = native_response.version;
    *response.headers_mut() = native_response.headers;
    let hop_by_hop_headers = response
        .headers()
        .keys()
        .filter(|name| is_hop_by_hop_header(name))
        .cloned()
        .collect::<Vec<_>>();
    for header in hop_by_hop_headers {
        response.headers_mut().remove(header);
    }
    if let Some(status) = response_status.and_then(|code| StatusCode::from_u16(code).ok()) {
        *response.status_mut() = status;
    }
    merge_headers(response.headers_mut(), &response_headers);
    response
}

async fn start_default_upstream_plain_http1_fast_path(
    context: &SharedProxyVmContext,
) -> Result<Option<HttpUpstreamResponseSnapshot>, UpstreamResponseStartError> {
    let (request, attached_transport) = snapshot_default_upstream_request(context)?;
    if attached_transport.is_some() {
        return Ok(None);
    }

    if request.target.is_none() {
        return Err(UpstreamResponseStartError::MissingTarget);
    }
    let tls_flow = context.lock_transport().tls_dag.default_upstream.clone();
    let services = context.services();
    let http2_sessions = services.upstream_http_sessions();
    let http3_sessions = services.upstream_http3_sessions();
    let http2_mode =
        http2::select_upstream_mode(request.target_scheme, &tls_flow, request.version_preference);
    let http3_mode =
        http3::select_upstream_mode(request.target_scheme, &tls_flow, request.version_preference);
    if tls_flow.requires_custom_client() {
        return Ok(None);
    }
    let use_http2 =
        http2::should_use_explicit_upstream_transport(http2_mode, http2_sessions.as_ref());
    let use_http3 =
        http3::should_use_explicit_upstream_transport(http3_mode, http3_sessions.as_ref());
    if !outbound_http1_fast_path_eligible(
        request.version_preference,
        request.target.is_some(),
        false,
        services.plain_http1_sender_pool().is_some(),
        use_http2,
        use_http3,
    ) {
        return Ok(None);
    }

    let response = forward_native_default_upstream_http_via_sender_pool(context, &request).await?;
    materialize_native_default_upstream_forward_response(context, response)
}

fn materialize_native_default_upstream_forward_response(
    context: &SharedProxyVmContext,
    response: NativeDefaultUpstreamForwardResponse,
) -> Result<Option<HttpUpstreamResponseSnapshot>, UpstreamResponseStartError> {
    let NativeDefaultUpstreamForwardResponse {
        status,
        headers,
        version,
        body,
        upstream_latency_ms,
        negotiated_alpn,
        peer_certificate_der,
    } = response;
    let started = StartedUpstreamResponse {
        status,
        headers,
        version,
        carrier_ref: HttpCarrierRef::Http1DefaultUpstream,
        peer_addr: None,
        negotiated_alpn,
        peer_certificate_der,
        body: Arc::new(tokio::sync::Mutex::new(match body {
            NativeDefaultUpstreamForwardBody::Empty => UpstreamResponseBodyState::empty(),
            NativeDefaultUpstreamForwardBody::Raw {
                body,
                content_length,
            } => UpstreamResponseBodyState::from_plain_http1(body, content_length),
        })),
    };
    let (snapshot, upstream_response_version, peer_addr) =
        started_upstream_response_into_snapshot(started);
    let mut exchanges = context.lock_exchanges();
    let exchange = exchanges
        .exchanges
        .get_mut(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
        .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(
            DEFAULT_UPSTREAM_EXCHANGE_HANDLE,
        ))?;
    if let Ok(existing) = exchange.response_snapshot() {
        return Ok(Some(existing));
    }
    exchange.store_response(StoredUpstreamResponse::new(
        snapshot.clone(),
        upstream_latency_ms,
    ));
    exchange
        .transport
        .mark_response_ready(upstream_response_version, snapshot.carrier_ref.clone());
    exchange.transport.set_peer_addr(peer_addr);
    context.clear_native_default_upstream_http_forward();
    Ok(Some(snapshot))
}

async fn take_or_await_native_default_upstream_forward_response(
    context: &SharedProxyVmContext,
) -> Result<Option<NativeDefaultUpstreamForwardResponse>, UpstreamResponseStartError> {
    if let Some(response) = context.take_native_default_upstream_forward_response() {
        return Ok(Some(response));
    }

    let Some(task) = context.take_native_default_upstream_forward_task() else {
        return Ok(None);
    };
    match task.await {
        Ok(Ok(response)) => Ok(Some(response)),
        Ok(Err(err)) => Err(err),
        Err(err) => Err(UpstreamResponseStartError::Protocol(format!(
            "native default upstream forward task failed: {err}"
        ))),
    }
}

async fn try_materialize_ready_or_pending_native_default_upstream_forward_response(
    context: &SharedProxyVmContext,
) -> Result<Option<HttpUpstreamResponseSnapshot>, UpstreamResponseStartError> {
    let Some(response) = take_or_await_native_default_upstream_forward_response(context).await?
    else {
        return Ok(None);
    };
    materialize_native_default_upstream_forward_response(context, response)
}

async fn try_resolve_ready_or_pending_native_default_upstream_forward_response(
    context: &SharedProxyVmContext,
    response_headers: HeaderMap,
    response_status: Option<u16>,
) -> Result<Option<ResolvedHttpGraphResponse>, UpstreamResponseStartError> {
    let Some(response) = take_or_await_native_default_upstream_forward_response(context).await?
    else {
        return Ok(None);
    };
    let upstream_latency_ms = response.upstream_latency_ms;
    Ok(Some(ResolvedHttpGraphResponse {
        response: response_from_started_upstream_response(
            response,
            response_headers,
            response_status,
        )
        .await,
        upstream_latency_ms,
        post_response_plan: None,
    }))
}

fn capture_downstream_http1_resolution_state(
    context: &SharedProxyVmContext,
) -> DownstreamHttp1ResolutionState {
    let downstream = context.lock_downstream();
    let exchanges = context.lock_exchanges();
    let default_exchange = exchanges
        .exchanges
        .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
        .expect("default upstream exchange should exist");
    let body_source_exchange = downstream.response_output.body_source_exchange;
    DownstreamHttp1ResolutionState {
        response_headers: downstream.response_output.headers.clone(),
        response_status: downstream.response_output.status,
        has_post_response_plan: downstream.post_response_plan.is_some(),
        has_response_body: downstream.response_output.body.is_some(),
        body_source_exchange,
        has_upstream_target: default_exchange.request.target.is_some(),
        default_upstream_websocket_mode: default_exchange.websocket_dag.is_websocket_mode(),
        native_forward_active: downstream.native_default_upstream_http_forward,
        default_upstream_response_body_read: downstream
            .vm_touches
            .exchange_response_body_reads
            .contains(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE),
        body_source_exchange_response_read: body_source_exchange.is_some_and(|exchange| {
            downstream
                .vm_touches
                .exchange_response_body_reads
                .contains(&exchange)
        }),
    }
}

pub(crate) async fn try_resolve_native_http1_downstream_response(
    context: &SharedProxyVmContext,
) -> Option<Result<ResolvedNativeHttp1DownstreamResponse, Response<Body>>> {
    let state = capture_downstream_http1_resolution_state(context);
    if state.has_post_response_plan
        || state.has_response_body
        || !state.native_forward_active
        || state.default_upstream_response_body_read
    {
        return None;
    }

    match take_or_await_native_default_upstream_forward_response(context).await {
        Ok(Some(response)) => {
            let upstream_latency_ms = response.upstream_latency_ms;
            context.clear_native_default_upstream_http_forward();
            Some(Ok(ResolvedNativeHttp1DownstreamResponse {
                response,
                response_headers: state.response_headers,
                response_status: state.response_status,
                upstream_latency_ms,
            }))
        }
        Ok(None) | Err(UpstreamResponseStartError::MissingTarget) => None,
        Err(UpstreamResponseStartError::UpstreamRequest(_)) => {
            context.clear_native_default_upstream_http_forward();
            Some(Err(text_response(StatusCode::BAD_GATEWAY, "bad gateway")))
        }
        Err(
            err @ (UpstreamResponseStartError::UnknownExchangeHandle(_)
            | UpstreamResponseStartError::MissingClient
            | UpstreamResponseStartError::Protocol(_)
            | UpstreamResponseStartError::ResolveOutboundBody(_)),
        ) => {
            context.clear_native_default_upstream_http_forward();
            Some(Err(text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &err.as_vm_error().to_string(),
            )))
        }
    }
}

pub(crate) async fn try_resolve_snapshot_http1_downstream_response(
    context: &SharedProxyVmContext,
) -> Option<Result<ResolvedSnapshotHttp1DownstreamResponse, Response<Body>>> {
    let state = capture_downstream_http1_resolution_state(context);
    if state.has_post_response_plan || state.has_response_body {
        return None;
    }

    if state.body_source_exchange_response_read {
        return None;
    }

    let (snapshot, upstream_latency_ms) = if let Some(exchange) = state.body_source_exchange {
        let snapshot = if exchange == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
            match start_upstream_response(context).await {
                Ok(snapshot) => snapshot,
                Err(UpstreamResponseStartError::MissingTarget) => return None,
                Err(
                    err @ (UpstreamResponseStartError::UnknownExchangeHandle(_)
                    | UpstreamResponseStartError::MissingClient
                    | UpstreamResponseStartError::Protocol(_)
                    | UpstreamResponseStartError::ResolveOutboundBody(_)),
                ) => {
                    return Some(Err(text_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &err.as_vm_error().to_string(),
                    )));
                }
                Err(UpstreamResponseStartError::UpstreamRequest(_)) => {
                    return Some(Err(text_response(StatusCode::BAD_GATEWAY, "bad gateway")));
                }
            }
        } else {
            match start_outbound_exchange_response(context, exchange).await {
                Ok(snapshot) => snapshot,
                Err(UpstreamResponseStartError::MissingTarget) => return None,
                Err(
                    err @ (UpstreamResponseStartError::UnknownExchangeHandle(_)
                    | UpstreamResponseStartError::MissingClient
                    | UpstreamResponseStartError::Protocol(_)
                    | UpstreamResponseStartError::ResolveOutboundBody(_)),
                ) => {
                    return Some(Err(text_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &err.as_vm_error().to_string(),
                    )));
                }
                Err(UpstreamResponseStartError::UpstreamRequest(_)) => {
                    return Some(Err(text_response(StatusCode::BAD_GATEWAY, "bad gateway")));
                }
            }
        };
        (snapshot, outbound_exchange_latency_ms(context, exchange))
    } else {
        let snapshot = {
            let exchanges = context.lock_exchanges();
            exchanges
                .exchanges
                .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
                .and_then(|exchange| match &exchange.response {
                    HttpUpstreamResponseNode::Ready(snapshot) => Some(snapshot.clone()),
                    HttpUpstreamResponseNode::NotStarted => None,
                })
        };
        let snapshot = if let Some(snapshot) = snapshot {
            snapshot
        } else if state.has_upstream_target && !state.default_upstream_websocket_mode {
            match start_upstream_response(context).await {
                Ok(snapshot) => snapshot,
                Err(UpstreamResponseStartError::MissingTarget) => return None,
                Err(
                    err @ (UpstreamResponseStartError::UnknownExchangeHandle(_)
                    | UpstreamResponseStartError::MissingClient
                    | UpstreamResponseStartError::Protocol(_)
                    | UpstreamResponseStartError::ResolveOutboundBody(_)),
                ) => {
                    return Some(Err(text_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &err.as_vm_error().to_string(),
                    )));
                }
                Err(UpstreamResponseStartError::UpstreamRequest(_)) => {
                    return Some(Err(text_response(StatusCode::BAD_GATEWAY, "bad gateway")));
                }
            }
        } else {
            return None;
        };
        (snapshot, current_upstream_latency_ms(context))
    };

    let (status, headers) = if state.body_source_exchange.is_some() {
        explicit_snapshot_downstream_response_head(
            &snapshot,
            state.response_headers,
            state.response_status,
        )
    } else {
        downstream_snapshot_response_head(&snapshot, state.response_headers, state.response_status)
    };
    Some(Ok(ResolvedSnapshotHttp1DownstreamResponse {
        status,
        headers,
        version: Version::HTTP_11,
        upstream_latency_ms,
        body: snapshot.body.clone(),
    }))
}

pub(crate) async fn resolve_http1_downstream_response(
    context: &SharedProxyVmContext,
) -> Http1DownstreamResolution {
    if let Some(native_local) = try_take_native_local_http1_downstream_response(context) {
        return Http1DownstreamResolution::NativeLocal(native_local);
    }
    if let Some(native_result) = try_resolve_native_http1_downstream_response(context).await {
        return Http1DownstreamResolution::Native(native_result);
    }
    if let Some(snapshot_result) = try_resolve_snapshot_http1_downstream_response(context).await {
        return Http1DownstreamResolution::Snapshot(snapshot_result);
    }
    Http1DownstreamResolution::Graph(resolve_http_graph_response(context).await)
}

pub(crate) fn try_take_native_local_http1_downstream_response(
    context: &SharedProxyVmContext,
) -> Option<ResolvedNativeLocalHttp1DownstreamResponse> {
    let mut downstream = context.lock_downstream();
    if downstream.post_response_plan.is_some() {
        return None;
    }
    if downstream.response_output.body_source_exchange.is_some() {
        return None;
    }
    let body = downstream.response_output.body.take()?;
    downstream.native_default_upstream_http_forward = false;
    downstream.native_default_upstream_forward_response = None;
    if let Some(task) = downstream.native_default_upstream_forward_task.take() {
        task.abort();
    }
    Some(ResolvedNativeLocalHttp1DownstreamResponse {
        status: downstream
            .response_output
            .status
            .take()
            .unwrap_or(StatusCode::OK.as_u16()),
        headers: std::mem::take(&mut downstream.response_output.headers),
        body,
        default_content_type: true,
    })
}

async fn forward_native_default_upstream_http_via_sender_pool(
    context: &SharedProxyVmContext,
    request: &DefaultUpstreamRequestSnapshot,
) -> Result<NativeDefaultUpstreamForwardResponse, UpstreamResponseStartError> {
    let services = context.services();
    let pool = services
        .plain_http1_sender_pool()
        .ok_or(UpstreamResponseStartError::MissingClient)?;
    let sender_pool_capacity = services.upstream_http_reuse_entries();
    let request_body = default_upstream_outbound_http1_request_body_template(context).await?;
    let started_at = Instant::now();
    let host = request.target_host.clone().ok_or_else(|| {
        UpstreamResponseStartError::Protocol(
            "default upstream host should be configured".to_string(),
        )
    })?;
    let host_arc = request
        .target_host_arc
        .clone()
        .unwrap_or_else(|| Arc::from(host.as_str()));
    let port = request.target_port.ok_or_else(|| {
        UpstreamResponseStartError::Protocol(
            "default upstream port should be configured".to_string(),
        )
    })?;
    let authority = request
        .target_authority
        .clone()
        .unwrap_or_else(|| Arc::from(format_upstream_authority(&host, port)));
    let tls_flow = context.lock_transport().tls_dag.default_upstream.clone();
    let target = OutboundHttp1Target {
        scheme: match request.target_scheme {
            HttpUpstreamScheme::Http => OutboundHttp1Scheme::Http,
            #[cfg(feature = "tls")]
            HttpUpstreamScheme::Https => OutboundHttp1Scheme::Https,
            #[cfg(not(feature = "tls"))]
            HttpUpstreamScheme::Https => {
                return Err(UpstreamResponseStartError::Protocol(
                    "https http/1.1 forwarding requires the tls feature".to_string(),
                ));
            }
        },
        authority: authority.clone(),
        host: host_arc,
        port,
        plain_pool_key: request.target_plain_http1_pool_key.clone(),
        #[cfg(feature = "tls")]
        tls_flow: (request.target_scheme == HttpUpstreamScheme::Https).then_some(tls_flow),
    };
    let request_path = context.with_request_head(|request_head| {
        super::request_path_with_query(
            request.path_or_request_head(request_head),
            request.query_or_request_head(request_head),
        )
    });
    let mut make_request = || {
        context.with_request_head(|request_head| {
            Ok::<_, VmError>(OutboundHttp1Request {
                method: request.method_or_request_head(request_head).clone(),
                path_and_query: request_path.clone(),
                headers: request
                    .outbound_http1_headers_or_request_head(request_head, Some(authority.as_ref())),
                body: match &request_body {
                    NativeDefaultUpstreamRequestBodyTemplate::Empty => {
                        OutboundHttp1RequestBody::Empty
                    }
                    NativeDefaultUpstreamRequestBodyTemplate::Bytes(body) => {
                        OutboundHttp1RequestBody::Bytes(body.clone())
                    }
                    NativeDefaultUpstreamRequestBodyTemplate::Streaming { content_length } => {
                        OutboundHttp1RequestBody::Streaming {
                            content_length: *content_length,
                            stream: Box::pin(stream_remaining_inbound_request_body(
                                context.clone(),
                            )),
                        }
                    }
                },
            })
        })
    };
    let response = forward_via_sender_pool(
        &pool,
        sender_pool_capacity,
        &target,
        started_at,
        &mut make_request,
    )
    .await
    .map_err(|err| UpstreamResponseStartError::UpstreamRequest(err.to_string()))?;
    mark_outbound_tcp_connected(context, DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
        .map_err(|err| UpstreamResponseStartError::Protocol(err.as_vm_error().to_string()))?;
    if request.target_scheme == HttpUpstreamScheme::Https {
        finalize_outbound_tls_handshake(
            context,
            DEFAULT_UPSTREAM_EXCHANGE_HANDLE,
            response.negotiated_alpn.clone(),
            response.peer_certificate_der.clone(),
        )?;
        cache_outbound_tls_session(
            context,
            DEFAULT_UPSTREAM_EXCHANGE_HANDLE,
            response.negotiated_alpn.clone(),
            response.peer_certificate_der.clone(),
        )?;
    }
    Ok(response)
}

fn schedule_native_default_upstream_http_forward_response(
    context: &SharedProxyVmContext,
    request: DefaultUpstreamRequestSnapshot,
) {
    let task_context = context.clone();
    let task = tokio::spawn(async move {
        forward_native_default_upstream_http_via_sender_pool(&task_context, &request).await
    });
    context.store_native_default_upstream_forward_task(task);
}

pub(crate) async fn start_native_default_upstream_http_forward_response(
    context: &SharedProxyVmContext,
) -> Result<bool, VmError> {
    if context.native_default_upstream_forward_response_ready()
        || context.native_default_upstream_forward_task_pending()
        || context.native_default_upstream_http_forward_active()
    {
        return Ok(true);
    }

    let Ok((request, attached_transport)) = snapshot_default_upstream_request(context) else {
        return Ok(false);
    };
    if attached_transport.is_some() {
        return Ok(false);
    }

    let services = context.services();
    let tls_flow = context.lock_transport().tls_dag.default_upstream.clone();
    let http2_mode =
        http2::select_upstream_mode(request.target_scheme, &tls_flow, request.version_preference);
    let http3_mode =
        http3::select_upstream_mode(request.target_scheme, &tls_flow, request.version_preference);
    let use_http2 = http2::should_use_explicit_upstream_transport(
        http2_mode,
        services.upstream_http_sessions().as_ref(),
    );
    let use_http3 = http3::should_use_explicit_upstream_transport(
        http3_mode,
        services.upstream_http3_sessions().as_ref(),
    );
    if !outbound_http1_fast_path_eligible(
        request.version_preference,
        request.target.is_some(),
        false,
        services.plain_http1_sender_pool().is_some(),
        use_http2,
        use_http3,
    ) {
        return Ok(false);
    }

    match native_default_upstream_request_body_mode(context).await {
        NativeDefaultUpstreamRequestBodyMode::Empty
        | NativeDefaultUpstreamRequestBodyMode::BufferedImmediate => {
            schedule_native_default_upstream_http_forward_response(context, request);
            Ok(true)
        }
        NativeDefaultUpstreamRequestBodyMode::StreamInbound => {
            context.begin_native_default_upstream_http_forward();
            Ok(true)
        }
        NativeDefaultUpstreamRequestBodyMode::BufferRemaining => Ok(false),
    }
}

async fn try_resolve_native_default_upstream_http_forward_response(
    context: &SharedProxyVmContext,
    response_headers: HeaderMap,
    response_status: Option<u16>,
) -> Result<Option<ResolvedHttpGraphResponse>, UpstreamResponseStartError> {
    let (request, attached_transport) = snapshot_default_upstream_request(context)?;
    if attached_transport.is_some() {
        return Ok(None);
    }

    if request.target.is_none() {
        return Err(UpstreamResponseStartError::MissingTarget);
    }
    let tls_flow = context.lock_transport().tls_dag.default_upstream.clone();
    let services = context.services();
    let http2_sessions = services.upstream_http_sessions();
    let http3_sessions = services.upstream_http3_sessions();
    let http2_mode =
        http2::select_upstream_mode(request.target_scheme, &tls_flow, request.version_preference);
    let http3_mode =
        http3::select_upstream_mode(request.target_scheme, &tls_flow, request.version_preference);
    let use_http2 =
        http2::should_use_explicit_upstream_transport(http2_mode, http2_sessions.as_ref());
    let use_http3 =
        http3::should_use_explicit_upstream_transport(http3_mode, http3_sessions.as_ref());
    if use_http2 || use_http3 {
        return Ok(None);
    }

    let started = Instant::now();
    if outbound_http1_fast_path_eligible(
        request.version_preference,
        request.target.is_some(),
        false,
        services.plain_http1_sender_pool().is_some(),
        use_http2,
        use_http3,
    ) && let Ok(response) =
        forward_native_default_upstream_http_via_sender_pool(context, &request).await
    {
        let upstream_latency_ms = response.upstream_latency_ms;
        return Ok(Some(ResolvedHttpGraphResponse {
            response: response_from_started_upstream_response(
                response,
                response_headers,
                response_status,
            )
            .await,
            upstream_latency_ms,
            post_response_plan: None,
        }));
    }
    let upstream_response =
        start_default_upstream_response_via_plain_http1_sender_pool(context, &request).await?;
    let (snapshot, _, _) = started_upstream_response_into_snapshot(upstream_response);
    let response =
        match response_from_upstream_snapshot(snapshot, response_headers, response_status).await {
            Ok(response) => response,
            Err(_) => text_response(StatusCode::BAD_GATEWAY, "bad gateway"),
        };
    mark_outbound_tcp_connected(context, DEFAULT_UPSTREAM_EXCHANGE_HANDLE)?;
    let upstream_latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(Some(ResolvedHttpGraphResponse {
        response,
        upstream_latency_ms,
        post_response_plan: None,
    }))
}

async fn start_upstream_response_via_attached_http1<I>(
    handle: i64,
    prepared: &PreparedUpstreamRequest,
    request_path: &str,
    headers: HeaderMap,
    request_body: Body,
    content_length: Option<u64>,
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
        .body(request_body)
        .map_err(|err| {
            UpstreamResponseStartError::Protocol(format!(
                "failed to build attached http request: {err}",
            ))
        })?;
    for (name, value) in &headers {
        request.headers_mut().insert(name, value.clone());
    }
    if let Some(content_length) = content_length {
        request.headers_mut().insert(
            CONTENT_LENGTH,
            HeaderValue::from_str(&content_length.to_string()).map_err(|err| {
                UpstreamResponseStartError::Protocol(format!(
                    "failed to encode attached http content-length: {err}",
                ))
            })?,
        );
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
            UpstreamResponseBodyState::from_hyper(response.into_body(), None, None, content_length),
        )),
    })
}

async fn start_upstream_response_via_attached_transport(
    context: &SharedProxyVmContext,
    handle: i64,
    prepared: &PreparedUpstreamRequest,
    headers: HeaderMap,
    request_body: Body,
    content_length: Option<u64>,
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
            if matches!(prepared.target_scheme, HttpUpstreamScheme::Https) {
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
                content_length,
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
                content_length,
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
    headers: HeaderMap,
    request_body: Body,
    request_body_present: bool,
) -> Result<StartedUpstreamResponse, http2::Http2RequestError> {
    let sessions = prepared
        .http2_sessions
        .as_ref()
        .expect("explicit http2 transport requires shared sessions");
    let started = http2::send_request(http2::Http2SendRequest {
        sessions,
        exchange_handle: handle,
        target_scheme: prepared.target_scheme,
        target_host: prepared
            .target_host
            .as_deref()
            .expect("http2 upstream target host should exist"),
        target_port: prepared
            .target_port
            .expect("http2 upstream target port should exist"),
        target_host_header: prepared.target_host_header.as_deref(),
        request_path: &prepared.path,
        request_query: &prepared.query,
        mode: prepared.http2_mode,
        tls_flow: &prepared.tls_flow,
        method: prepared.method.clone(),
        headers,
        request_body,
        request_body_present,
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
                None,
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
    request_body: http3::Http3RequestBody,
) -> Result<StartedUpstreamResponse, http3::Http3RequestError> {
    let sessions = prepared
        .http3_sessions
        .clone()
        .expect("explicit http3 transport requires shared sessions");
    let started = http3::send_request(http3::Http3SendRequestOptions {
        exchange_handle: handle,
        target_scheme: prepared.target_scheme,
        target_host: prepared
            .target_host
            .clone()
            .expect("http3 upstream target host should exist"),
        target_port: prepared
            .target_port
            .expect("http3 upstream target port should exist"),
        target_host_header: prepared.target_host_header.clone(),
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
        let (scheme, target_host, target_port, flow) = if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE
        {
            let (scheme, target_host, target_port) = context
                .lock_exchanges()
                .exchanges
                .get(&handle)
                .map(|exchange| {
                    (
                        exchange.request.target_scheme,
                        exchange.request.target_host.clone(),
                        exchange.request.target_port,
                    )
                })
                .unwrap_or((HttpUpstreamScheme::Http, None, None));
            let flow = context.lock_transport().tls_dag.default_upstream.clone();
            (scheme, target_host, target_port, flow)
        } else {
            let exchanges = context.lock_exchanges();
            let exchange = exchanges
                .exchanges
                .get(&handle)
                .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
            (
                exchange.request.target_scheme,
                exchange.request.target_host.clone(),
                exchange.request.target_port,
                exchange.transport.tls_flow.clone(),
            )
        };
        let Some(target_host) = target_host else {
            return Ok(());
        };
        let Some(target_port) = target_port else {
            return Ok(());
        };
        let Some(key) = tls_session_cache_key(scheme.as_str(), &target_host, target_port, &flow)
        else {
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

    let _ = cache.insert(
        key,
        cached,
        LockMetricKey::TlsSessionCache,
        "tls session cache lock poisoned",
    );
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

    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE
        && let Ok(Some(snapshot)) =
            try_materialize_ready_or_pending_native_default_upstream_forward_response(context).await
    {
        return Ok(snapshot);
    }

    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE
        && let Some(snapshot) = start_default_upstream_plain_http1_fast_path(context).await?
    {
        return Ok(snapshot);
    }

    let prepared = prepared_outbound_exchange_request(context, handle)?;
    let (upstream_url, host_header) = build_configured_upstream_url(
        &prepared.target,
        prepared.target_inherits_request_path,
        prepared.target_host_header.as_deref(),
        &prepared.path,
        &prepared.query,
    );
    let outbound_headers = filtered_upstream_headers(&prepared.headers, host_header.as_deref());

    let is_attached_transport = prepared.attached_transport.is_some();
    if is_attached_transport {
        let (request_body, content_length, _) = if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
            into_http_body_from_default_upstream_template(
                context,
                default_upstream_outbound_http1_request_body_template(context).await?,
            )
        } else {
            let request_body = resolve_outbound_exchange_body(context, handle)
                .await
                .map_err(|err| {
                    UpstreamResponseStartError::ResolveOutboundBody(format!(
                        "failed to resolve outbound exchange body: {err}",
                    ))
                })?;
            if request_body.is_empty() {
                (Body::empty(), Some(0), false)
            } else {
                let content_length = u64::try_from(request_body.len()).unwrap_or(u64::MAX);
                (Body::from(request_body), Some(content_length), true)
            }
        };
        let started = Instant::now();
        let upstream_response = start_upstream_response_via_attached_transport(
            context,
            handle,
            &prepared,
            outbound_headers,
            request_body,
            content_length,
        )
        .await?;
        let StartedUpstreamResponse {
            status,
            headers,
            version: upstream_response_version,
            carrier_ref,
            peer_addr,
            negotiated_alpn: _,
            peer_certificate_der: _,
            body,
        } = upstream_response;
        mark_outbound_tcp_connected(context, handle)?;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let snapshot = HttpUpstreamResponseSnapshot {
            status,
            headers: Arc::new(headers),
            http_version: http_version_label(upstream_response_version),
            carrier_kind: carrier_ref.kind(),
            carrier_ref: carrier_ref.clone(),
            body,
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
        exchange.transport.set_peer_addr(peer_addr);
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
    let default_request_body_template =
        if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE && (use_http3 || use_http2) {
            Some(default_upstream_outbound_http1_request_body_template(context).await?)
        } else {
            None
        };
    let request_body = if default_request_body_template.is_none() {
        Some(
            resolve_outbound_exchange_body(context, handle)
                .await
                .map_err(|err| {
                    UpstreamResponseStartError::ResolveOutboundBody(format!(
                        "failed to resolve outbound exchange body: {err}",
                    ))
                })?,
        )
    } else {
        None
    };
    let upstream_response = if use_http3 {
        #[cfg(feature = "http3")]
        {
            match start_upstream_response_via_http3(
                handle,
                &prepared,
                &upstream_url,
                outbound_headers.clone(),
                if let Some(template) = default_request_body_template.clone() {
                    into_http3_request_body_from_default_upstream_template(context, template)
                } else {
                    http3::Http3RequestBody::Bytes(
                        request_body
                            .clone()
                            .expect("non-default outbound body should be resolved"),
                    )
                },
            )
            .await
            {
                Ok(started) => started,
                Err(http3::Http3RequestError::FallbackToHttp2 { .. }) => {
                    if use_http2 {
                        #[cfg(feature = "http2")]
                        {
                            let (http2_request_body, _, request_body_present) =
                                if let Some(template) = default_request_body_template.clone() {
                                    into_http_body_from_default_upstream_template(context, template)
                                } else {
                                    let request_body = request_body
                                        .clone()
                                        .expect("non-default outbound body should be resolved");
                                    if request_body.is_empty() {
                                        (Body::empty(), Some(0), false)
                                    } else {
                                        let content_length =
                                            u64::try_from(request_body.len()).unwrap_or(u64::MAX);
                                        (Body::from(request_body), Some(content_length), true)
                                    }
                                };
                            match start_upstream_response_via_http2(
                                handle,
                                &prepared,
                                outbound_headers.clone(),
                                http2_request_body,
                                request_body_present,
                            )
                            .await
                            {
                                Ok(started) => started,
                                Err(http2::Http2RequestError::FallbackToHttp1 { .. }) => {
                                    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
                                        let (request, _) =
                                            snapshot_default_upstream_request(context)?;
                                        match start_default_upstream_response_via_plain_http1_sender_pool(
                                            context,
                                            &request,
                                        )
                                        .await
                                        {
                                            Ok(started) => started,
                                            Err(err) => {
                                                let _ = note_outbound_tls_failure(context, handle);
                                                return Err(err);
                                            }
                                        }
                                    } else {
                                        match start_upstream_response_via_plain_http1_sender_pool(
                                            context,
                                            handle,
                                            &prepared,
                                            request_body.expect(
                                                "non-default outbound body should be resolved",
                                            ),
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
                    } else if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
                        let (request, _) = snapshot_default_upstream_request(context)?;
                        match start_default_upstream_response_via_plain_http1_sender_pool(
                            context, &request,
                        )
                        .await
                        {
                            Ok(started) => started,
                            Err(err) => {
                                let _ = note_outbound_tls_failure(context, handle);
                                return Err(err);
                            }
                        }
                    } else {
                        match start_upstream_response_via_plain_http1_sender_pool(
                            context,
                            handle,
                            &prepared,
                            request_body.expect("non-default outbound body should be resolved"),
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
            let (http2_request_body, _, request_body_present) =
                if let Some(template) = default_request_body_template.clone() {
                    into_http_body_from_default_upstream_template(context, template)
                } else {
                    let request_body = request_body
                        .clone()
                        .expect("non-default outbound body should be resolved");
                    if request_body.is_empty() {
                        (Body::empty(), Some(0), false)
                    } else {
                        let content_length = u64::try_from(request_body.len()).unwrap_or(u64::MAX);
                        (Body::from(request_body), Some(content_length), true)
                    }
                };
            match start_upstream_response_via_http2(
                handle,
                &prepared,
                outbound_headers.clone(),
                http2_request_body,
                request_body_present,
            )
            .await
            {
                Ok(started) => started,
                Err(http2::Http2RequestError::FallbackToHttp1 { .. }) => {
                    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
                        let (request, _) = snapshot_default_upstream_request(context)?;
                        match start_default_upstream_response_via_plain_http1_sender_pool(
                            context, &request,
                        )
                        .await
                        {
                            Ok(started) => started,
                            Err(err) => {
                                let _ = note_outbound_tls_failure(context, handle);
                                return Err(err);
                            }
                        }
                    } else {
                        match start_upstream_response_via_plain_http1_sender_pool(
                            context,
                            handle,
                            &prepared,
                            request_body.expect("non-default outbound body should be resolved"),
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
        #[cfg(not(feature = "http2"))]
        {
            unreachable!("explicit http2 transport requires the http2 feature");
        }
    } else if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        let (request, _) = snapshot_default_upstream_request(context)?;
        match start_default_upstream_response_via_plain_http1_sender_pool(context, &request).await {
            Ok(started) => started,
            Err(err) => {
                let _ = note_outbound_tls_failure(context, handle);
                return Err(err);
            }
        }
    } else {
        match start_upstream_response_via_plain_http1_sender_pool(
            context,
            handle,
            &prepared,
            request_body.expect("outbound body should be resolved for http/1.1 sender pool"),
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
    let StartedUpstreamResponse {
        status,
        headers,
        version: _,
        carrier_ref,
        peer_addr,
        negotiated_alpn: _,
        peer_certificate_der: _,
        body,
    } = upstream_response;
    let snapshot = HttpUpstreamResponseSnapshot {
        status,
        headers: Arc::new(headers),
        http_version: http_version_label(upstream_response_version),
        carrier_kind: carrier_ref.kind(),
        carrier_ref: carrier_ref.clone(),
        body,
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
    exchange.transport.set_peer_addr(peer_addr);
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
    if context.native_default_upstream_forward_response_ready() {
        return true;
    }
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
    context.note_exchange_response_body_read(DEFAULT_UPSTREAM_EXCHANGE_HANDLE);
    let snapshot = ensure_upstream_response_started(context).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.read_all().await
}

pub(crate) async fn read_upstream_response_trailers(
    context: &SharedProxyVmContext,
) -> Result<HeaderMap, VmError> {
    context.note_exchange_response_body_read(DEFAULT_UPSTREAM_EXCHANGE_HANDLE);
    let snapshot = ensure_upstream_response_started(context).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.read_trailers().await
}

pub(crate) async fn read_outbound_exchange_response_all(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<Vec<u8>, VmError> {
    context.note_exchange_response_body_read(handle);
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return read_upstream_response_all(context).await;
    }
    let snapshot = ensure_outbound_exchange_response_started(context, handle).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.read_all().await
}

pub(crate) async fn read_outbound_exchange_response_trailers(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<HeaderMap, VmError> {
    context.note_exchange_response_body_read(handle);
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return read_upstream_response_trailers(context).await;
    }
    let snapshot = ensure_outbound_exchange_response_started(context, handle).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.read_trailers().await
}

pub(crate) async fn read_upstream_response_next_chunk(
    context: &SharedProxyVmContext,
    max_bytes: usize,
) -> Result<Vec<u8>, VmError> {
    context.note_exchange_response_body_read(DEFAULT_UPSTREAM_EXCHANGE_HANDLE);
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
    context.note_exchange_response_body_read(handle);
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
    context.note_exchange_response_body_read(DEFAULT_UPSTREAM_EXCHANGE_HANDLE);
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
    context.note_exchange_response_body_read(handle);
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

pub(crate) async fn read_downstream_response_trailers(
    context: &SharedProxyVmContext,
) -> Result<HeaderMap, VmError> {
    let (
        has_local_body,
        body_source_exchange,
        has_post_response_plan,
        has_upstream_target,
        default_upstream_websocket_mode,
    ) = {
        let downstream = context.lock_downstream();
        let exchanges = context.lock_exchanges();
        let default_exchange = exchanges
            .exchanges
            .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
            .expect("default upstream exchange should exist");
        (
            downstream.response_output.body.is_some(),
            downstream.response_output.body_source_exchange,
            downstream.post_response_plan.is_some(),
            default_exchange.request.target.is_some(),
            default_exchange.websocket_dag.is_websocket_mode(),
        )
    };

    if has_local_body || has_post_response_plan {
        return Ok(HeaderMap::new());
    }
    if let Some(exchange) = body_source_exchange {
        return read_outbound_exchange_response_trailers(context, exchange).await;
    }
    if has_upstream_target && !default_upstream_websocket_mode {
        return read_upstream_response_trailers(context).await;
    }
    Ok(HeaderMap::new())
}

fn current_upstream_latency_ms(context: &SharedProxyVmContext) -> u64 {
    if let Some(latency_ms) = context.native_default_upstream_forward_latency_ms() {
        return latency_ms;
    }
    context
        .lock_exchanges()
        .exchanges
        .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
        .map(|exchange| exchange.upstream_latency_ms)
        .unwrap_or(0)
}

fn outbound_exchange_latency_ms(context: &SharedProxyVmContext, handle: i64) -> u64 {
    context
        .lock_exchanges()
        .exchanges
        .get(&handle)
        .map(|exchange| exchange.upstream_latency_ms)
        .unwrap_or(0)
}

fn merge_headers(target: &mut HeaderMap, overlay: &HeaderMap) {
    for (name, value) in overlay {
        target.insert(name, value.clone());
    }
}

fn downstream_snapshot_response_head(
    snapshot: &HttpUpstreamResponseSnapshot,
    response_headers: HeaderMap,
    response_status: Option<u16>,
) -> (u16, SnapshotHttp1DownstreamHeaders) {
    (
        response_status.unwrap_or(snapshot.status),
        SnapshotHttp1DownstreamHeaders::Snapshot {
            base: snapshot.headers.clone(),
            overlay: response_headers,
        },
    )
}

fn explicit_snapshot_downstream_response_head(
    snapshot: &HttpUpstreamResponseSnapshot,
    response_headers: HeaderMap,
    response_status: Option<u16>,
) -> (u16, SnapshotHttp1DownstreamHeaders) {
    (
        response_status.unwrap_or(snapshot.status),
        SnapshotHttp1DownstreamHeaders::Explicit(response_headers),
    )
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
    let body_is_empty = body.is_empty();
    let mut response = if body_is_empty {
        Response::new(Body::empty())
    } else {
        Response::new(Body::from(body))
    };
    let status = status_code
        .and_then(|code| StatusCode::from_u16(code).ok())
        .unwrap_or(StatusCode::OK);
    *response.status_mut() = status;
    merge_headers(response.headers_mut(), &headers);
    if body_is_empty {
        response
            .headers_mut()
            .entry(CONTENT_LENGTH)
            .or_insert_with(|| HeaderValue::from_static("0"));
    } else if !response.headers().contains_key(CONTENT_TYPE) {
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    }
    response
}

pub(crate) async fn materialize_downstream_response_body_source(
    context: &SharedProxyVmContext,
) -> Result<(), VmError> {
    let exchange = {
        let downstream = context.lock_downstream();
        if downstream.response_output.body.is_some() {
            return Ok(());
        }
        downstream.response_output.body_source_exchange
    };
    let Some(exchange) = exchange else {
        return Ok(());
    };
    let body = read_outbound_exchange_response_all(context, exchange).await?;
    let mut downstream = context.lock_downstream();
    if downstream.response_output.body.is_none()
        && downstream.response_output.body_source_exchange == Some(exchange)
    {
        downstream.response_output.body_source_exchange = None;
        downstream.response_output.body = Some(body);
        sync_response_output_body_headers(&mut downstream.response_output);
    }
    Ok(())
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

async fn streaming_body_from_upstream_snapshot(
    snapshot: &HttpUpstreamResponseSnapshot,
) -> Result<Body, VmError> {
    let mut upstream_body = snapshot.body.lock().await;
    if upstream_body.is_known_empty() {
        return Ok(Body::empty());
    }
    let passthrough = upstream_body.take_streaming_passthrough();
    Ok(Body::new(StreamBody::new(try_unfold(
        passthrough,
        |mut state| async move {
            let frame: Option<Frame<Bytes>> = state
                .next_frame()
                .await
                .map_err(|err: VmError| io::Error::other(err.to_string()))?;
            Ok::<_, io::Error>(frame.map(|frame| (frame, state)))
        },
    ))))
}

fn filtered_snapshot_headers(snapshot: &HttpUpstreamResponseSnapshot) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in snapshot.headers.iter() {
        if !is_hop_by_hop_header(name) {
            headers.insert(name, value.clone());
        }
    }
    headers
}

async fn response_from_upstream_snapshot_head(
    snapshot: HttpUpstreamResponseSnapshot,
    status: u16,
    headers: HeaderMap,
) -> Result<Response<Body>, VmError> {
    let body = streaming_body_from_upstream_snapshot(&snapshot).await?;
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    *response.headers_mut() = headers;
    Ok(response)
}

async fn response_from_upstream_snapshot(
    snapshot: HttpUpstreamResponseSnapshot,
    response_headers: HeaderMap,
    response_status: Option<u16>,
) -> Result<Response<Body>, VmError> {
    let status = response_status.unwrap_or(snapshot.status);
    let mut headers = filtered_snapshot_headers(&snapshot);
    merge_headers(&mut headers, &response_headers);
    response_from_upstream_snapshot_head(snapshot, status, headers).await
}

pub(crate) async fn resolve_http_graph_response(
    context: &SharedProxyVmContext,
) -> ResolvedHttpGraphResponse {
    let native_fast_path = {
        let mut downstream = context.lock_downstream();
        if downstream.post_response_plan.is_none() && downstream.response_output.body.is_none() {
            downstream
                .native_default_upstream_forward_response
                .take()
                .map(|response| {
                    downstream.native_default_upstream_http_forward = false;
                    (
                        response,
                        std::mem::take(&mut downstream.response_output.headers),
                        downstream.response_output.status.take(),
                    )
                })
        } else {
            None
        }
    };
    if let Some((response, response_headers, response_status)) = native_fast_path {
        let upstream_latency_ms = response.upstream_latency_ms;
        return ResolvedHttpGraphResponse {
            response: response_from_started_upstream_response(
                response,
                response_headers,
                response_status,
            )
            .await,
            upstream_latency_ms,
            post_response_plan: None,
        };
    }

    let (
        response_body,
        body_source_exchange,
        response_headers,
        response_status,
        has_post_response_plan,
        has_upstream_target,
        default_upstream_websocket_mode,
        upstream_response,
        native_default_upstream_http_forward,
    ) = {
        let downstream = context.lock_downstream();
        let exchanges = context.lock_exchanges();
        let default_exchange = exchanges
            .exchanges
            .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
            .expect("default upstream exchange should exist");
        (
            downstream.response_output.body.clone(),
            downstream.response_output.body_source_exchange,
            downstream.response_output.headers.clone(),
            downstream.response_output.status,
            downstream.post_response_plan.is_some(),
            default_exchange.request.target.is_some(),
            default_exchange.websocket_dag.is_websocket_mode(),
            match &default_exchange.response {
                HttpUpstreamResponseNode::Ready(snapshot) => Some(snapshot.clone()),
                HttpUpstreamResponseNode::NotStarted => None,
            },
            downstream.native_default_upstream_http_forward,
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
            DownstreamPostResponsePlan::WebSocketTunnel(plan) => {
                context.with_request_head(|request_head| {
                    response_from_websocket_tunnel(
                        request_head.headers(),
                        response_headers,
                        plan.selected_subprotocol.as_deref(),
                    )
                })
            }
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

    if let Some(exchange) = body_source_exchange {
        let snapshot = if exchange == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
            match start_upstream_response(context).await {
                Ok(snapshot) => snapshot,
                Err(UpstreamResponseStartError::MissingTarget) => {
                    return ResolvedHttpGraphResponse {
                        response: text_response(StatusCode::NOT_FOUND, "not found"),
                        upstream_latency_ms: 0,
                        post_response_plan: None,
                    };
                }
                Err(
                    err @ (UpstreamResponseStartError::UnknownExchangeHandle(_)
                    | UpstreamResponseStartError::MissingClient
                    | UpstreamResponseStartError::Protocol(_)
                    | UpstreamResponseStartError::ResolveOutboundBody(_)),
                ) => {
                    return ResolvedHttpGraphResponse {
                        response: text_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &err.as_vm_error().to_string(),
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
            match start_outbound_exchange_response(context, exchange).await {
                Ok(snapshot) => snapshot,
                Err(UpstreamResponseStartError::MissingTarget) => {
                    return ResolvedHttpGraphResponse {
                        response: text_response(StatusCode::NOT_FOUND, "not found"),
                        upstream_latency_ms: 0,
                        post_response_plan: None,
                    };
                }
                Err(
                    err @ (UpstreamResponseStartError::UnknownExchangeHandle(_)
                    | UpstreamResponseStartError::MissingClient
                    | UpstreamResponseStartError::Protocol(_)
                    | UpstreamResponseStartError::ResolveOutboundBody(_)),
                ) => {
                    return ResolvedHttpGraphResponse {
                        response: text_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &err.as_vm_error().to_string(),
                        ),
                        upstream_latency_ms: outbound_exchange_latency_ms(context, exchange),
                        post_response_plan: None,
                    };
                }
                Err(UpstreamResponseStartError::UpstreamRequest(_)) => {
                    return ResolvedHttpGraphResponse {
                        response: text_response(StatusCode::BAD_GATEWAY, "bad gateway"),
                        upstream_latency_ms: outbound_exchange_latency_ms(context, exchange),
                        post_response_plan: None,
                    };
                }
            }
        };

        let explicit_status = response_status.unwrap_or(snapshot.status);
        let response =
            response_from_upstream_snapshot_head(snapshot, explicit_status, response_headers).await;

        return match response {
            Ok(response) => ResolvedHttpGraphResponse {
                response,
                upstream_latency_ms: outbound_exchange_latency_ms(context, exchange),
                post_response_plan: None,
            },
            Err(_) => ResolvedHttpGraphResponse {
                response: text_response(StatusCode::BAD_GATEWAY, "bad gateway"),
                upstream_latency_ms: outbound_exchange_latency_ms(context, exchange),
                post_response_plan: None,
            },
        };
    }

    if native_default_upstream_http_forward && upstream_response.is_none() {
        if let Ok(Some(resolved)) =
            try_resolve_ready_or_pending_native_default_upstream_forward_response(
                context,
                response_headers.clone(),
                response_status,
            )
            .await
        {
            context.clear_native_default_upstream_http_forward();
            return resolved;
        }
        match try_resolve_native_default_upstream_http_forward_response(
            context,
            response_headers.clone(),
            response_status,
        )
        .await
        {
            Ok(Some(resolved)) => {
                context.clear_native_default_upstream_http_forward();
                return resolved;
            }
            Ok(None) => {}
            Err(UpstreamResponseStartError::MissingTarget) => {}
            Err(
                err @ (UpstreamResponseStartError::UnknownExchangeHandle(_)
                | UpstreamResponseStartError::MissingClient
                | UpstreamResponseStartError::Protocol(_)
                | UpstreamResponseStartError::ResolveOutboundBody(_)),
            ) => {
                context.clear_native_default_upstream_http_forward();
                return ResolvedHttpGraphResponse {
                    response: text_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &err.as_vm_error().to_string(),
                    ),
                    upstream_latency_ms: current_upstream_latency_ms(context),
                    post_response_plan: None,
                };
            }
            Err(UpstreamResponseStartError::UpstreamRequest(_)) => {
                context.clear_native_default_upstream_http_forward();
                return ResolvedHttpGraphResponse {
                    response: text_response(StatusCode::BAD_GATEWAY, "bad gateway"),
                    upstream_latency_ms: current_upstream_latency_ms(context),
                    post_response_plan: None,
                };
            }
        }
    }

    let snapshot = if let Some(snapshot) = upstream_response {
        Some(snapshot)
    } else if has_upstream_target && !default_upstream_websocket_mode {
        match start_upstream_response(context).await {
            Ok(snapshot) => Some(snapshot),
            Err(UpstreamResponseStartError::MissingTarget) => None,
            Err(
                err @ (UpstreamResponseStartError::UnknownExchangeHandle(_)
                | UpstreamResponseStartError::MissingClient
                | UpstreamResponseStartError::Protocol(_)
                | UpstreamResponseStartError::ResolveOutboundBody(_)),
            ) => {
                return ResolvedHttpGraphResponse {
                    response: text_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &err.as_vm_error().to_string(),
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
    context.note_downstream_request_body_read();
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
    context.note_downstream_request_body_read();
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
    context.note_downstream_request_body_read();
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
    let mut inbound = context.inbound_request_body.lock().await;
    finalize_downstream_body_all_result(context, inbound.read_all().await)
}

pub(crate) async fn consume_request_body_all(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let mut inbound = context.inbound_request_body.lock().await;
    finalize_downstream_body_all_result(context, inbound.read_all_and_consume().await)
}

pub(crate) async fn read_request_body_next_chunk(
    context: &SharedProxyVmContext,
    max_bytes: usize,
) -> Result<Vec<u8>, VmError> {
    let mut inbound = context.inbound_request_body.lock().await;
    let result = inbound.read_next_chunk(max_bytes).await;
    finalize_downstream_body_read_result(context, &inbound, result)
}

pub(crate) async fn read_request_body_next_line(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let mut inbound = context.inbound_request_body.lock().await;
    let result = inbound.read_next_line().await;
    finalize_downstream_body_read_result(context, &inbound, result)
}

pub(crate) async fn request_body_eof(context: &SharedProxyVmContext) -> Result<bool, VmError> {
    let mut inbound = context.inbound_request_body.lock().await;
    finalize_downstream_body_eof_result(context, inbound.eof().await)
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
                path: "/".to_string(),
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
