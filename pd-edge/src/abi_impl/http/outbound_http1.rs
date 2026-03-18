use std::{
    collections::HashMap,
    io,
    pin::Pin,
    str,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Instant,
};

use axum::{
    body::Bytes,
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, Version,
        header::{CONNECTION, CONTENT_LENGTH, HOST, TRANSFER_ENCODING},
    },
};
use bytes::{Buf, BytesMut};
use futures_util::{StreamExt, stream::BoxStream};
use parking_lot::Mutex as ParkingMutex;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use vm::VmError;

use crate::abi_impl::http::LazyHttpHeaders;
use crate::abi_impl::transport::HTTP11_ALPN_PROTOCOL;
#[cfg(feature = "tls")]
use crate::abi_impl::transport::{
    TlsFlowState, TlsSessionCacheKey, build_dynamic_client_config, tls_session_cache_key,
};
#[cfg(feature = "tls")]
use tokio_rustls::{TlsConnector, rustls::pki_types::ServerName};

type PlainHttp1PooledConnection = RawHttp1Connection;
pub(crate) type SharedPlainHttp1SenderPool = Arc<ParkingMutex<PlainHttp1SenderPool>>;

static NATIVE_FORWARD_METRICS_ENABLED: OnceLock<bool> = OnceLock::new();
static NATIVE_FORWARD_METRICS_COUNT: AtomicU64 = AtomicU64::new(0);
static NATIVE_FORWARD_METRICS_POOL_HITS: AtomicU64 = AtomicU64::new(0);
static NATIVE_FORWARD_METRICS_POOL_MISSES: AtomicU64 = AtomicU64::new(0);
static NATIVE_FORWARD_METRICS_RETRIES: AtomicU64 = AtomicU64::new(0);
static NATIVE_FORWARD_METRICS_BUILD_US: AtomicU64 = AtomicU64::new(0);
static NATIVE_FORWARD_METRICS_CONNECT_US: AtomicU64 = AtomicU64::new(0);
static NATIVE_FORWARD_METRICS_READY_US: AtomicU64 = AtomicU64::new(0);
static NATIVE_FORWARD_METRICS_SEND_US: AtomicU64 = AtomicU64::new(0);
static NATIVE_FORWARD_METRICS_TOTAL_US: AtomicU64 = AtomicU64::new(0);
static HTTP1_POOL_METRICS_ENABLED: OnceLock<bool> = OnceLock::new();
static HTTP1_POOL_METRICS_REQUESTS: AtomicU64 = AtomicU64::new(0);
static HTTP1_POOL_METRICS_OPEN: AtomicU64 = AtomicU64::new(0);
static HTTP1_POOL_METRICS_LEASED: AtomicU64 = AtomicU64::new(0);
static HTTP1_POOL_METRICS_REUSED: AtomicU64 = AtomicU64::new(0);
static HTTP1_POOL_METRICS_DROPPED_DIRTY: AtomicU64 = AtomicU64::new(0);
static HTTP1_POOL_METRICS_PARSE_FAILURES: AtomicU64 = AtomicU64::new(0);
static HTTP1_POOL_METRICS_CLOSE_DELIMITED: AtomicU64 = AtomicU64::new(0);
const HTTP1_CHUNK_WRITE_OVERHEAD_BYTES: usize = 32;

#[derive(Debug)]
enum RawHttp1Stream {
    Tcp(TcpStream),
    #[cfg(feature = "tls")]
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

#[derive(Debug)]
struct RawHttp1Connection {
    stream: RawHttp1Stream,
    buffered: BytesMut,
    write_scratch: BytesMut,
    negotiated_alpn: Option<String>,
    peer_certificate_der: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Http1PoolKey {
    Plain(Arc<str>),
    #[cfg(feature = "tls")]
    Tls(TlsSessionCacheKey),
}

#[derive(Debug, Default)]
pub(crate) struct PlainHttp1SenderPool {
    hot_key: Option<Http1PoolKey>,
    hot_connections: Vec<PlainHttp1PooledConnection>,
    other_connections: HashMap<Http1PoolKey, Vec<PlainHttp1PooledConnection>>,
}

#[derive(Debug)]
struct PlainHttp1ConnectionAcquire {
    connection: PlainHttp1PooledConnection,
    pool_hit: bool,
    connect_us: u64,
    ready_us: u64,
}

#[derive(Debug)]
enum PlainHttp1BodyKind {
    Done,
    Fixed { remaining: u64 },
    Chunked { state: ChunkedState },
    CloseDelimited,
}

#[derive(Debug)]
enum ChunkedState {
    NeedSize,
    ReadData { remaining: usize },
    ExpectChunkTerminator,
    ReadTrailers,
}

#[derive(Debug)]
struct ParsedResponseHead {
    status: u16,
    headers: HeaderMap,
    version: Version,
    content_length: Option<u64>,
    keep_alive: bool,
    body_kind: Option<PlainHttp1BodyKind>,
}

#[derive(Debug)]
pub(crate) struct PlainHttp1SenderLease {
    pool: SharedPlainHttp1SenderPool,
    pool_key: Http1PoolKey,
    capacity: usize,
    may_reuse: bool,
    dirty: bool,
    connection: Option<PlainHttp1PooledConnection>,
}

#[derive(Debug)]
pub(crate) struct PlainHttp1ResponseBody {
    lease: PlainHttp1SenderLease,
    kind: PlainHttp1BodyKind,
    trailers: Option<HeaderMap>,
}

#[derive(Debug)]
pub(crate) enum OutboundHttp1ForwardBody {
    Empty,
    Raw {
        body: PlainHttp1ResponseBody,
        content_length: Option<u64>,
    },
}

#[derive(Debug)]
pub(crate) struct OutboundHttp1ForwardResponse {
    pub(crate) status: u16,
    pub(crate) headers: HeaderMap,
    pub(crate) version: Version,
    pub(crate) body: OutboundHttp1ForwardBody,
    pub(crate) upstream_latency_ms: u64,
    pub(crate) negotiated_alpn: Option<String>,
    pub(crate) peer_certificate_der: Option<Vec<u8>>,
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct OutboundHttp1Request {
    pub(crate) method: Method,
    pub(crate) path_and_query: String,
    pub(crate) headers: OutboundHttp1RequestHeaders,
    pub(crate) body: OutboundHttp1RequestBody,
}

#[derive(Clone, Debug)]
pub(crate) enum OutboundHttp1RequestHeaders {
    Parsed(HeaderMap),
    InheritedFiltered {
        headers: LazyHttpHeaders,
        host_header: Option<Arc<str>>,
    },
}

impl From<HeaderMap> for OutboundHttp1RequestHeaders {
    fn from(headers: HeaderMap) -> Self {
        Self::Parsed(headers)
    }
}

pub(crate) enum OutboundHttp1RequestBody {
    Empty,
    Bytes(Bytes),
    Streaming {
        content_length: Option<u64>,
        stream: BoxStream<'static, Result<Bytes, VmError>>,
    },
}

impl std::fmt::Debug for OutboundHttp1RequestBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("OutboundHttp1RequestBody::Empty"),
            Self::Bytes(body) => f
                .debug_tuple("OutboundHttp1RequestBody::Bytes")
                .field(&body.len())
                .finish(),
            Self::Streaming { content_length, .. } => f
                .debug_struct("OutboundHttp1RequestBody::Streaming")
                .field("content_length", content_length)
                .finish(),
        }
    }
}

pub(crate) struct SerializedOutboundHttp1Request {
    pub(crate) method: Method,
    pub(crate) body: OutboundHttp1RequestBody,
    pub(crate) use_chunked_body: bool,
}

impl OutboundHttp1RequestBody {
    fn is_retryable(&self) -> bool {
        matches!(self, Self::Empty | Self::Bytes(_))
    }

    pub(crate) fn content_length(&self) -> Option<u64> {
        match self {
            Self::Empty => Some(0),
            Self::Bytes(body) => Some(u64::try_from(body.len()).unwrap_or(u64::MAX)),
            Self::Streaming { content_length, .. } => *content_length,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum OutboundHttp1Scheme {
    Http,
    #[cfg(feature = "tls")]
    Https,
}

#[derive(Clone, Debug)]
pub(crate) struct OutboundHttp1Target {
    pub(crate) scheme: OutboundHttp1Scheme,
    pub(crate) authority: Arc<str>,
    pub(crate) host: Arc<str>,
    pub(crate) port: u16,
    pub(crate) plain_pool_key: Option<Arc<str>>,
    #[cfg(feature = "tls")]
    pub(crate) tls_flow: Option<TlsFlowState>,
}

fn native_forward_metrics_enabled() -> bool {
    *NATIVE_FORWARD_METRICS_ENABLED
        .get_or_init(|| std::env::var_os("PD_EDGE_NATIVE_FORWARD_METRICS").is_some())
}

fn http1_pool_metrics_enabled() -> bool {
    *HTTP1_POOL_METRICS_ENABLED
        .get_or_init(|| std::env::var_os("PD_EDGE_HTTP1_POOL_METRICS").is_some())
}

fn note_http1_pool_connection_opened() {
    if http1_pool_metrics_enabled() {
        HTTP1_POOL_METRICS_OPEN.fetch_add(1, Ordering::Relaxed);
    }
}

fn note_http1_pool_connection_closed_clean() {
    if http1_pool_metrics_enabled() {
        HTTP1_POOL_METRICS_OPEN.fetch_sub(1, Ordering::Relaxed);
    }
}

fn note_http1_pool_connection_leased() {
    if http1_pool_metrics_enabled() {
        HTTP1_POOL_METRICS_LEASED.fetch_add(1, Ordering::Relaxed);
    }
}

fn note_http1_pool_connection_released() {
    if http1_pool_metrics_enabled() {
        HTTP1_POOL_METRICS_LEASED.fetch_sub(1, Ordering::Relaxed);
    }
}

fn note_http1_pool_connection_reused() {
    if http1_pool_metrics_enabled() {
        HTTP1_POOL_METRICS_REUSED.fetch_add(1, Ordering::Relaxed);
    }
}

fn note_http1_pool_connection_dropped_dirty() {
    if http1_pool_metrics_enabled() {
        HTTP1_POOL_METRICS_DROPPED_DIRTY.fetch_add(1, Ordering::Relaxed);
        HTTP1_POOL_METRICS_OPEN.fetch_sub(1, Ordering::Relaxed);
    }
}

fn note_http1_pool_parse_failure() {
    if http1_pool_metrics_enabled() {
        HTTP1_POOL_METRICS_PARSE_FAILURES.fetch_add(1, Ordering::Relaxed);
    }
}

fn note_http1_pool_close_delimited_response() {
    if http1_pool_metrics_enabled() {
        HTTP1_POOL_METRICS_CLOSE_DELIMITED.fetch_add(1, Ordering::Relaxed);
    }
}

fn record_http1_pool_metrics_sample() {
    if !http1_pool_metrics_enabled() {
        return;
    }
    let count = HTTP1_POOL_METRICS_REQUESTS.fetch_add(1, Ordering::Relaxed) + 1;
    if count.is_multiple_of(1000) {
        eprintln!(
            "http1_pool_metrics requests={count} open={} leased={} reused={} dropped_dirty={} parse_failures={} close_delimited={}",
            HTTP1_POOL_METRICS_OPEN.load(Ordering::Relaxed),
            HTTP1_POOL_METRICS_LEASED.load(Ordering::Relaxed),
            HTTP1_POOL_METRICS_REUSED.load(Ordering::Relaxed),
            HTTP1_POOL_METRICS_DROPPED_DIRTY.load(Ordering::Relaxed),
            HTTP1_POOL_METRICS_PARSE_FAILURES.load(Ordering::Relaxed),
            HTTP1_POOL_METRICS_CLOSE_DELIMITED.load(Ordering::Relaxed),
        );
    }
}

fn record_native_forward_metrics(
    pool_hit: bool,
    retries: u64,
    build_us: u64,
    connect_us: u64,
    ready_us: u64,
    send_us: u64,
    total_us: u64,
) {
    if !native_forward_metrics_enabled() {
        return;
    }
    let count = NATIVE_FORWARD_METRICS_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if pool_hit {
        NATIVE_FORWARD_METRICS_POOL_HITS.fetch_add(1, Ordering::Relaxed);
    } else {
        NATIVE_FORWARD_METRICS_POOL_MISSES.fetch_add(1, Ordering::Relaxed);
    }
    NATIVE_FORWARD_METRICS_RETRIES.fetch_add(retries, Ordering::Relaxed);
    NATIVE_FORWARD_METRICS_BUILD_US.fetch_add(build_us, Ordering::Relaxed);
    NATIVE_FORWARD_METRICS_CONNECT_US.fetch_add(connect_us, Ordering::Relaxed);
    NATIVE_FORWARD_METRICS_READY_US.fetch_add(ready_us, Ordering::Relaxed);
    NATIVE_FORWARD_METRICS_SEND_US.fetch_add(send_us, Ordering::Relaxed);
    NATIVE_FORWARD_METRICS_TOTAL_US.fetch_add(total_us, Ordering::Relaxed);
    if count.is_multiple_of(1000) {
        let hits = NATIVE_FORWARD_METRICS_POOL_HITS.load(Ordering::Relaxed);
        let misses = NATIVE_FORWARD_METRICS_POOL_MISSES.load(Ordering::Relaxed);
        let retries = NATIVE_FORWARD_METRICS_RETRIES.load(Ordering::Relaxed);
        let build_avg =
            NATIVE_FORWARD_METRICS_BUILD_US.load(Ordering::Relaxed) as f64 / count as f64;
        let connect_avg =
            NATIVE_FORWARD_METRICS_CONNECT_US.load(Ordering::Relaxed) as f64 / count as f64;
        let ready_avg =
            NATIVE_FORWARD_METRICS_READY_US.load(Ordering::Relaxed) as f64 / count as f64;
        let send_avg = NATIVE_FORWARD_METRICS_SEND_US.load(Ordering::Relaxed) as f64 / count as f64;
        let total_avg =
            NATIVE_FORWARD_METRICS_TOTAL_US.load(Ordering::Relaxed) as f64 / count as f64;
        eprintln!(
            "native_forward_metrics requests={count} pool_hit_rate={:.2}% retries={} avg_build_us={build_avg:.1} avg_connect_us={connect_avg:.1} avg_ready_us={ready_avg:.1} avg_send_us={send_avg:.1} avg_total_us={total_avg:.1}",
            if hits + misses == 0 {
                0.0
            } else {
                (hits as f64 * 100.0) / (hits + misses) as f64
            },
            retries,
        );
    }
}

impl PlainHttp1SenderPool {
    fn take(&mut self, key: &Http1PoolKey) -> Option<PlainHttp1PooledConnection> {
        if self.hot_key.as_ref() == Some(key) {
            return self.hot_connections.pop();
        }
        self.other_connections.get_mut(key).and_then(Vec::pop)
    }

    fn put(
        &mut self,
        key: Http1PoolKey,
        capacity: usize,
        connection: PlainHttp1PooledConnection,
    ) -> bool {
        let capacity = capacity.max(1);
        if self.hot_key.as_ref() == Some(&key) {
            if self.hot_connections.len() < capacity {
                self.hot_connections.push(connection);
                return true;
            }
            return false;
        }
        if self.hot_key.is_none() {
            self.hot_key = Some(key);
            self.hot_connections.push(connection);
            return true;
        }
        let connections = self.other_connections.entry(key).or_default();
        if connections.len() < capacity {
            connections.push(connection);
            return true;
        }
        false
    }
}

pub(crate) fn new_shared_plain_http1_sender_pool() -> SharedPlainHttp1SenderPool {
    Arc::new(ParkingMutex::new(PlainHttp1SenderPool::default()))
}

impl tokio::io::AsyncRead for RawHttp1Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            RawHttp1Stream::Tcp(stream) => Pin::new(stream).poll_read(cx, buf),
            #[cfg(feature = "tls")]
            RawHttp1Stream::Tls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for RawHttp1Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        match self.get_mut() {
            RawHttp1Stream::Tcp(stream) => Pin::new(stream).poll_write(cx, buf),
            #[cfg(feature = "tls")]
            RawHttp1Stream::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match self.get_mut() {
            RawHttp1Stream::Tcp(stream) => Pin::new(stream).poll_flush(cx),
            #[cfg(feature = "tls")]
            RawHttp1Stream::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match self.get_mut() {
            RawHttp1Stream::Tcp(stream) => Pin::new(stream).poll_shutdown(cx),
            #[cfg(feature = "tls")]
            RawHttp1Stream::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        match self.get_mut() {
            RawHttp1Stream::Tcp(stream) => Pin::new(stream).poll_write_vectored(cx, bufs),
            #[cfg(feature = "tls")]
            RawHttp1Stream::Tls(stream) => Pin::new(stream.as_mut()).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            RawHttp1Stream::Tcp(stream) => stream.is_write_vectored(),
            #[cfg(feature = "tls")]
            RawHttp1Stream::Tls(stream) => stream.is_write_vectored(),
        }
    }
}

fn plain_pool_key(authority: &str) -> Http1PoolKey {
    Http1PoolKey::Plain(Arc::from(format!("http://{authority}")))
}

#[cfg(feature = "tls")]
fn tls_pool_key(
    authority: &str,
    host: &str,
    port: u16,
    tls_flow: &TlsFlowState,
) -> Result<Http1PoolKey, VmError> {
    let key = tls_session_cache_key("https", host, port, tls_flow).ok_or_else(|| {
        VmError::HostError(format!(
            "https upstream {authority} is missing tls flow state for outbound http/1.1 pooling",
        ))
    })?;
    Ok(Http1PoolKey::Tls(key))
}

fn target_pool_key(target: &OutboundHttp1Target) -> Result<Http1PoolKey, VmError> {
    match &target.scheme {
        OutboundHttp1Scheme::Http => Ok(target
            .plain_pool_key
            .as_ref()
            .map(|key| Http1PoolKey::Plain(key.clone()))
            .unwrap_or_else(|| plain_pool_key(target.authority.as_ref()))),
        #[cfg(feature = "tls")]
        OutboundHttp1Scheme::Https => tls_pool_key(
            target.authority.as_ref(),
            target.host.as_ref(),
            target.port,
            target.tls_flow.as_ref().ok_or_else(|| {
                VmError::HostError(format!(
                    "https upstream {} is missing tls configuration for outbound http/1.1 forwarding",
                    target.authority,
                ))
            })?,
        ),
    }
}

async fn read_more(connection: &mut PlainHttp1PooledConnection) -> Result<bool, VmError> {
    let read = connection
        .stream
        .read_buf(&mut connection.buffered)
        .await
        .map_err(|err| {
            VmError::HostError(format!(
                "failed to read outbound http/1.1 upstream response: {err}"
            ))
        })?;
    Ok(read > 0)
}

async fn acquire_plain_http1_connection(
    pool: &SharedPlainHttp1SenderPool,
    target: &OutboundHttp1Target,
) -> Result<PlainHttp1ConnectionAcquire, VmError> {
    let pool_key = target_pool_key(target)?;
    if let Some(connection) = pool.lock().take(&pool_key) {
        note_http1_pool_connection_reused();
        return Ok(PlainHttp1ConnectionAcquire {
            connection,
            pool_hit: true,
            connect_us: 0,
            ready_us: 0,
        });
    }

    let connect_started = Instant::now();
    let stream = TcpStream::connect((target.host.as_ref(), target.port))
        .await
        .map_err(|err| {
            VmError::HostError(format!("failed to connect to {}: {err}", target.authority,))
        })?;
    stream
        .set_nodelay(true)
        .map_err(|err| VmError::HostError(format!("failed to tune upstream socket: {err}")))?;
    let connect_us = u64::try_from(connect_started.elapsed().as_micros()).unwrap_or(u64::MAX);

    match &target.scheme {
        OutboundHttp1Scheme::Http => {
            note_http1_pool_connection_opened();
            Ok(PlainHttp1ConnectionAcquire {
                connection: PlainHttp1PooledConnection {
                    stream: RawHttp1Stream::Tcp(stream),
                    buffered: BytesMut::with_capacity(8 * 1024),
                    write_scratch: BytesMut::with_capacity(8 * 1024),
                    negotiated_alpn: Some(HTTP11_ALPN_PROTOCOL.to_string()),
                    peer_certificate_der: None,
                },
                pool_hit: false,
                connect_us,
                ready_us: 0,
            })
        }
        #[cfg(feature = "tls")]
        OutboundHttp1Scheme::Https => {
            let tls_flow = target.tls_flow.as_ref().ok_or_else(|| {
                VmError::HostError(format!(
                    "https upstream {} is missing tls configuration for outbound http/1.1 forwarding",
                    target.authority,
                ))
            })?;
            let config = build_dynamic_client_config(tls_flow)?;
            let connector = TlsConnector::from(Arc::new(config));
            let server_name_value = if !tls_flow.server_name().is_empty() {
                tls_flow.server_name().to_string()
            } else {
                target.host.as_ref().to_string()
            };
            let server_name = ServerName::try_from(server_name_value.clone()).map_err(|err| {
                VmError::HostError(format!(
                    "invalid tls server name `{server_name_value}` for https upstream {}: {err}",
                    target.authority,
                ))
            })?;
            let ready_started = Instant::now();
            let tls_stream = connector
                .connect(server_name, stream)
                .await
                .map_err(|err| {
                    VmError::HostError(format!(
                        "failed to establish tls connection to https upstream {}: {err}",
                        target.authority,
                    ))
                })?;
            let negotiated_alpn = tls_stream
                .get_ref()
                .1
                .alpn_protocol()
                .map(|protocol| String::from_utf8_lossy(protocol).into_owned());
            let peer_certificate_der = tls_stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|certificates| certificates.first().cloned())
                .map(|certificate| certificate.to_vec());
            note_http1_pool_connection_opened();
            Ok(PlainHttp1ConnectionAcquire {
                connection: PlainHttp1PooledConnection {
                    stream: RawHttp1Stream::Tls(Box::new(tls_stream)),
                    buffered: BytesMut::with_capacity(8 * 1024),
                    write_scratch: BytesMut::with_capacity(8 * 1024),
                    negotiated_alpn,
                    peer_certificate_der,
                },
                pool_hit: false,
                connect_us,
                ready_us: u64::try_from(ready_started.elapsed().as_micros()).unwrap_or(u64::MAX),
            })
        }
    }
}

fn release_plain_http1_connection(
    pool: &SharedPlainHttp1SenderPool,
    pool_key: Http1PoolKey,
    capacity: usize,
    connection: PlainHttp1PooledConnection,
) {
    if !pool.lock().put(pool_key, capacity, connection) {
        note_http1_pool_connection_closed_clean();
    }
}

impl PlainHttp1SenderLease {
    fn new(
        pool: SharedPlainHttp1SenderPool,
        pool_key: Http1PoolKey,
        capacity: usize,
        may_reuse: bool,
        connection: PlainHttp1PooledConnection,
    ) -> Self {
        note_http1_pool_connection_leased();
        Self {
            pool,
            pool_key,
            capacity,
            may_reuse,
            dirty: false,
            connection: Some(connection),
        }
    }

    fn connection_mut(&mut self) -> Result<&mut PlainHttp1PooledConnection, VmError> {
        self.connection.as_mut().ok_or_else(|| {
            VmError::HostError(
                "plain http/1.1 upstream connection lease is unavailable".to_string(),
            )
        })
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.may_reuse = false;
    }

    pub(crate) fn release(&mut self) {
        if let Some(connection) = self.connection.take()
            && self.may_reuse
            && !self.dirty
        {
            release_plain_http1_connection(
                &self.pool,
                self.pool_key.clone(),
                self.capacity,
                connection,
            );
        }
    }
}

impl Drop for PlainHttp1SenderLease {
    fn drop(&mut self) {
        note_http1_pool_connection_released();
        if self.connection.take().is_some() {
            if self.dirty || self.may_reuse {
                note_http1_pool_connection_dropped_dirty();
            } else {
                note_http1_pool_connection_closed_clean();
            }
        }
    }
}

impl PlainHttp1ResponseBody {
    fn new(lease: PlainHttp1SenderLease, kind: PlainHttp1BodyKind) -> Self {
        Self {
            lease,
            kind,
            trailers: None,
        }
    }

    fn take_buffer_prefix(&mut self, count: usize) -> Result<Bytes, VmError> {
        let connection = self.lease.connection_mut()?;
        Ok(connection.buffered.split_to(count).freeze())
    }

    fn take_buffer_all(&mut self) -> Result<Bytes, VmError> {
        let connection = self.lease.connection_mut()?;
        Ok(connection.buffered.split().freeze())
    }

    async fn ensure_buffered_bytes(&mut self, count: usize) -> Result<bool, VmError> {
        loop {
            let available = self.lease.connection_mut()?.buffered.len();
            if available >= count {
                return Ok(true);
            }
            if !read_more(self.lease.connection_mut()?).await? {
                return Ok(false);
            }
        }
    }

    async fn read_line(&mut self) -> Result<Option<Bytes>, VmError> {
        loop {
            let line_end = {
                let connection = self.lease.connection_mut()?;
                find_crlf(&connection.buffered)
            };
            if let Some(line_end) = line_end {
                let line = self.take_buffer_prefix(line_end)?;
                self.lease.connection_mut()?.buffered.advance(2);
                return Ok(Some(line));
            }
            if !read_more(self.lease.connection_mut()?).await? {
                return Ok(None);
            }
        }
    }

    fn record_trailer_line(&mut self, line: &[u8]) -> Result<(), VmError> {
        let Some(separator) = line.iter().position(|byte| *byte == b':') else {
            note_http1_pool_parse_failure();
            self.lease.mark_dirty();
            return Err(VmError::HostError(
                "invalid plain http/1.1 trailer line".to_string(),
            ));
        };
        let name = HeaderName::from_bytes(&line[..separator]).map_err(|err| {
            note_http1_pool_parse_failure();
            self.lease.mark_dirty();
            VmError::HostError(format!("invalid plain http/1.1 trailer name: {err}",))
        })?;
        let value =
            HeaderValue::from_bytes(line[separator + 1..].trim_ascii_start()).map_err(|err| {
                note_http1_pool_parse_failure();
                self.lease.mark_dirty();
                VmError::HostError(format!(
                    "invalid plain http/1.1 trailer value for `{name}`: {err}",
                ))
            })?;
        self.trailers
            .get_or_insert_with(HeaderMap::new)
            .append(name, value);
        Ok(())
    }

    pub(crate) fn take_trailers(&mut self) -> Option<HeaderMap> {
        self.trailers.take()
    }

    pub(crate) async fn pull_next(&mut self) -> Result<Option<Bytes>, VmError> {
        loop {
            let kind = std::mem::replace(&mut self.kind, PlainHttp1BodyKind::Done);
            match kind {
                PlainHttp1BodyKind::Done => return Ok(None),
                PlainHttp1BodyKind::Fixed { mut remaining } => {
                    if remaining == 0 {
                        self.lease.release();
                        return Ok(None);
                    }
                    let available = self.lease.connection_mut()?.buffered.len();
                    if available == 0 {
                        if !read_more(self.lease.connection_mut()?).await? {
                            self.lease.mark_dirty();
                            return Err(VmError::HostError(
                                "plain http/1.1 upstream connection closed before fixed-length body completed".to_string(),
                            ));
                        }
                        self.kind = PlainHttp1BodyKind::Fixed { remaining };
                        continue;
                    }
                    let take = available.min(usize::try_from(remaining).unwrap_or(usize::MAX));
                    remaining = remaining.saturating_sub(u64::try_from(take).unwrap_or(u64::MAX));
                    let chunk = self.take_buffer_prefix(take)?;
                    if remaining == 0 {
                        self.lease.release();
                        self.kind = PlainHttp1BodyKind::Done;
                    } else {
                        self.kind = PlainHttp1BodyKind::Fixed { remaining };
                    }
                    return Ok(Some(chunk));
                }
                PlainHttp1BodyKind::CloseDelimited => {
                    if !self.lease.connection_mut()?.buffered.is_empty() {
                        self.kind = PlainHttp1BodyKind::CloseDelimited;
                        return Ok(Some(self.take_buffer_all()?));
                    }
                    if !read_more(self.lease.connection_mut()?).await? {
                        self.lease.release();
                        self.kind = PlainHttp1BodyKind::Done;
                        return Ok(None);
                    }
                    self.kind = PlainHttp1BodyKind::CloseDelimited;
                }
                PlainHttp1BodyKind::Chunked { mut state } => {
                    match state {
                        ChunkedState::NeedSize => {
                            let Some(line) = self.read_line().await? else {
                                self.lease.mark_dirty();
                                return Err(VmError::HostError(
                                "plain http/1.1 upstream connection closed while reading chunk size".to_string(),
                            ));
                            };
                            let line = str::from_utf8(&line).map_err(|err| {
                                note_http1_pool_parse_failure();
                                VmError::HostError(format!(
                                    "invalid utf-8 in plain http/1.1 chunk size line: {err}",
                                ))
                            })?;
                            let size = line
                                .split(';')
                                .next()
                                .map(str::trim)
                                .filter(|value| !value.is_empty())
                                .ok_or_else(|| {
                                    note_http1_pool_parse_failure();
                                    VmError::HostError(
                                        "missing plain http/1.1 chunk size line".to_string(),
                                    )
                                })
                                .and_then(|value| {
                                    usize::from_str_radix(value, 16).map_err(|err| {
                                        note_http1_pool_parse_failure();
                                        VmError::HostError(format!(
                                            "invalid plain http/1.1 chunk size `{value}`: {err}",
                                        ))
                                    })
                                })?;
                            if size == 0 {
                                state = ChunkedState::ReadTrailers;
                            } else {
                                state = ChunkedState::ReadData { remaining: size };
                            }
                            self.kind = PlainHttp1BodyKind::Chunked { state };
                        }
                        ChunkedState::ReadData { mut remaining } => {
                            if remaining == 0 {
                                self.kind = PlainHttp1BodyKind::Chunked {
                                    state: ChunkedState::ExpectChunkTerminator,
                                };
                                continue;
                            }
                            let available = self.lease.connection_mut()?.buffered.len();
                            if available == 0 {
                                if !read_more(self.lease.connection_mut()?).await? {
                                    self.lease.mark_dirty();
                                    return Err(VmError::HostError(
                                    "plain http/1.1 upstream connection closed before chunked body completed".to_string(),
                                ));
                                }
                                self.kind = PlainHttp1BodyKind::Chunked {
                                    state: ChunkedState::ReadData { remaining },
                                };
                                continue;
                            }
                            let take = available.min(remaining);
                            remaining -= take;
                            let chunk = self.take_buffer_prefix(take)?;
                            if remaining == 0 {
                                self.kind = PlainHttp1BodyKind::Chunked {
                                    state: ChunkedState::ExpectChunkTerminator,
                                };
                            } else {
                                self.kind = PlainHttp1BodyKind::Chunked {
                                    state: ChunkedState::ReadData { remaining },
                                };
                            }
                            return Ok(Some(chunk));
                        }
                        ChunkedState::ExpectChunkTerminator => {
                            if !self.ensure_buffered_bytes(2).await? {
                                self.lease.mark_dirty();
                                return Err(VmError::HostError(
                                "plain http/1.1 upstream connection closed before chunk terminator".to_string(),
                            ));
                            }
                            let terminator = self.take_buffer_prefix(2)?;
                            if terminator.as_ref() != b"\r\n" {
                                note_http1_pool_parse_failure();
                                self.lease.mark_dirty();
                                return Err(VmError::HostError(
                                    "invalid plain http/1.1 chunk terminator".to_string(),
                                ));
                            }
                            self.kind = PlainHttp1BodyKind::Chunked {
                                state: ChunkedState::NeedSize,
                            };
                        }
                        ChunkedState::ReadTrailers => {
                            let Some(line) = self.read_line().await? else {
                                self.lease.mark_dirty();
                                return Err(VmError::HostError(
                                "plain http/1.1 upstream connection closed while reading trailers".to_string(),
                            ));
                            };
                            if line.is_empty() {
                                self.lease.release();
                                self.kind = PlainHttp1BodyKind::Done;
                                return Ok(None);
                            }
                            self.record_trailer_line(&line)?;
                            self.kind = PlainHttp1BodyKind::Chunked {
                                state: ChunkedState::ReadTrailers,
                            };
                        }
                    }
                }
            }
        }
    }
}

fn find_crlf(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|pair| pair == b"\r\n")
}

fn header_content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

fn header_contains_token(headers: &HeaderMap, name: HeaderName, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|value| value.eq_ignore_ascii_case(token))
}

fn response_keeps_alive(version: Version, headers: &HeaderMap) -> bool {
    let connection_close = header_contains_token(headers, CONNECTION, "close");
    let connection_keep_alive = header_contains_token(headers, CONNECTION, "keep-alive");
    match version {
        Version::HTTP_10 => connection_keep_alive && !connection_close,
        _ => !connection_close,
    }
}

fn response_has_no_body(status: u16, method: &Method) -> bool {
    method == Method::HEAD || (100..200).contains(&status) || status == 204 || status == 304
}

fn transfer_is_chunked(headers: &HeaderMap) -> bool {
    header_contains_token(headers, TRANSFER_ENCODING, "chunked")
}

fn version_from_http11_minor(minor: Option<u8>) -> Version {
    match minor {
        Some(0) => Version::HTTP_10,
        Some(1) => Version::HTTP_11,
        _ => Version::HTTP_11,
    }
}

fn append_chunk_prefix(encoded: &mut BytesMut, len: usize) {
    let mut value = len;
    let mut digits = [0u8; usize::BITS as usize / 4];
    let mut index = digits.len();
    loop {
        index -= 1;
        let digit = (value & 0x0f) as u8;
        digits[index] = match digit {
            0..=9 => b'0' + digit,
            _ => b'A' + (digit - 10),
        };
        value >>= 4;
        if value == 0 {
            break;
        }
    }
    encoded.extend_from_slice(&digits[index..]);
    encoded.extend_from_slice(b"\r\n");
}

async fn parse_response_head(
    connection: &mut PlainHttp1PooledConnection,
    request_method: &Method,
) -> Result<ParsedResponseHead, VmError> {
    loop {
        let mut header_storage = [httparse::EMPTY_HEADER; 64];
        let mut response = httparse::Response::new(&mut header_storage);
        match response.parse(&connection.buffered) {
            Ok(httparse::Status::Complete(consumed)) => {
                let version = version_from_http11_minor(response.version);
                let status = response.code.ok_or_else(|| {
                    note_http1_pool_parse_failure();
                    VmError::HostError(
                        "plain http/1.1 upstream response missing status code".to_string(),
                    )
                })?;
                let mut headers = HeaderMap::new();
                for header in response.headers {
                    let name = HeaderName::from_bytes(header.name.as_bytes()).map_err(|err| {
                        note_http1_pool_parse_failure();
                        VmError::HostError(format!(
                            "invalid plain http/1.1 upstream header name `{}`: {err}",
                            header.name,
                        ))
                    })?;
                    let value = HeaderValue::from_bytes(header.value).map_err(|err| {
                        note_http1_pool_parse_failure();
                        VmError::HostError(format!(
                            "invalid plain http/1.1 upstream header value for `{}`: {err}",
                            header.name,
                        ))
                    })?;
                    headers.append(name, value);
                }
                connection.buffered.advance(consumed);
                let keep_alive = response_keeps_alive(version, &headers);
                let content_length = header_content_length(&headers);
                let body_kind = if response_has_no_body(status, request_method)
                    || matches!(content_length, Some(0))
                {
                    None
                } else if transfer_is_chunked(&headers) {
                    Some(PlainHttp1BodyKind::Chunked {
                        state: ChunkedState::NeedSize,
                    })
                } else if let Some(content_length) = content_length {
                    Some(PlainHttp1BodyKind::Fixed {
                        remaining: content_length,
                    })
                } else {
                    note_http1_pool_close_delimited_response();
                    Some(PlainHttp1BodyKind::CloseDelimited)
                };
                return Ok(ParsedResponseHead {
                    status,
                    headers,
                    version,
                    content_length,
                    keep_alive,
                    body_kind,
                });
            }
            Ok(httparse::Status::Partial) => {
                if !read_more(connection).await? {
                    note_http1_pool_parse_failure();
                    return Err(VmError::HostError(
                        "plain http/1.1 upstream connection closed before response head completed"
                            .to_string(),
                    ));
                }
            }
            Err(err) => {
                note_http1_pool_parse_failure();
                return Err(VmError::HostError(format!(
                    "failed to parse plain http/1.1 upstream response head: {err}",
                )));
            }
        }
    }
}

fn is_hop_by_hop_header_name(name: &str) -> bool {
    let name = name.as_bytes();
    name.eq_ignore_ascii_case(b"connection")
        || name.eq_ignore_ascii_case(b"keep-alive")
        || name.eq_ignore_ascii_case(b"proxy-authenticate")
        || name.eq_ignore_ascii_case(b"proxy-authorization")
        || name.eq_ignore_ascii_case(b"te")
        || name.eq_ignore_ascii_case(b"trailer")
        || name.eq_ignore_ascii_case(b"transfer-encoding")
        || name.eq_ignore_ascii_case(b"upgrade")
}

fn encode_request_headers(
    encoded: &mut BytesMut,
    headers: &OutboundHttp1RequestHeaders,
    authority: &str,
) -> (bool, bool, bool, bool) {
    let mut has_host = false;
    let mut has_content_length = false;
    let mut has_connection = false;
    let mut has_transfer_encoding = false;
    match headers {
        OutboundHttp1RequestHeaders::Parsed(headers) => {
            for (name, value) in headers {
                if name == HOST {
                    has_host = true;
                } else if name == CONTENT_LENGTH {
                    has_content_length = true;
                } else if name == CONNECTION {
                    has_connection = true;
                } else if name == TRANSFER_ENCODING {
                    has_transfer_encoding = true;
                }
                encoded.extend_from_slice(name.as_str().as_bytes());
                encoded.extend_from_slice(b": ");
                encoded.extend_from_slice(value.as_bytes());
                encoded.extend_from_slice(b"\r\n");
            }
        }
        OutboundHttp1RequestHeaders::InheritedFiltered {
            headers,
            host_header,
        } => {
            headers.for_each_header(|name, value| {
                if name.eq_ignore_ascii_case(HOST.as_str()) {
                    has_host = true;
                    return;
                }
                if name.eq_ignore_ascii_case(CONTENT_LENGTH.as_str()) {
                    has_content_length = true;
                    return;
                }
                if name.eq_ignore_ascii_case(CONNECTION.as_str()) {
                    has_connection = true;
                    return;
                }
                if name.eq_ignore_ascii_case(TRANSFER_ENCODING.as_str()) {
                    has_transfer_encoding = true;
                    return;
                }
                if is_hop_by_hop_header_name(name) {
                    return;
                }
                encoded.extend_from_slice(name.as_bytes());
                encoded.extend_from_slice(b": ");
                encoded.extend_from_slice(value);
                encoded.extend_from_slice(b"\r\n");
            });
            if let Some(host) = host_header {
                has_host = true;
                encoded.extend_from_slice(HOST.as_str().as_bytes());
                encoded.extend_from_slice(b": ");
                encoded.extend_from_slice(host.as_bytes());
                encoded.extend_from_slice(b"\r\n");
            }
            has_connection = false;
            has_transfer_encoding = false;
            has_content_length = false;
        }
    }
    if !has_host {
        encoded.extend_from_slice(HOST.as_str().as_bytes());
        encoded.extend_from_slice(b": ");
        encoded.extend_from_slice(authority.as_bytes());
        encoded.extend_from_slice(b"\r\n");
    }
    (
        has_host,
        has_content_length,
        has_connection,
        has_transfer_encoding,
    )
}

#[cfg(test)]
fn serialize_request_head_into(
    request: &OutboundHttp1Request,
    authority: &str,
    encoded: &mut BytesMut,
) -> bool {
    let method = &request.method;
    let path_and_query = &request.path_and_query;
    let path = if path_and_query.is_empty() {
        "/"
    } else {
        path_and_query.as_str()
    };
    let body_content_length = request.body.content_length();
    encoded.clear();
    encoded.reserve(method.as_str().len() + path.len() + 128);
    encoded.extend_from_slice(method.as_str().as_bytes());
    encoded.extend_from_slice(b" ");
    encoded.extend_from_slice(path.as_bytes());
    encoded.extend_from_slice(b" HTTP/1.1\r\n");

    let (_has_host, has_content_length, has_connection, has_transfer_encoding) =
        encode_request_headers(encoded, &request.headers, authority);
    let use_chunked_body =
        !has_transfer_encoding && !has_content_length && body_content_length.is_none();
    if !has_content_length
        && !has_transfer_encoding
        && let Some(content_length) = body_content_length
    {
        encoded.extend_from_slice(CONTENT_LENGTH.as_str().as_bytes());
        encoded.extend_from_slice(b": ");
        encoded.extend_from_slice(content_length.to_string().as_bytes());
        encoded.extend_from_slice(b"\r\n");
    } else if use_chunked_body {
        encoded.extend_from_slice(TRANSFER_ENCODING.as_str().as_bytes());
        encoded.extend_from_slice(b": chunked\r\n");
    }
    if !has_connection {
        encoded.extend_from_slice(CONNECTION.as_str().as_bytes());
        encoded.extend_from_slice(b": keep-alive\r\n");
    }
    encoded.extend_from_slice(b"\r\n");
    use_chunked_body
}

pub(crate) fn serialize_request_head_parts_into(
    method: &Method,
    request_path: &str,
    request_query: &str,
    headers: &OutboundHttp1RequestHeaders,
    authority: &str,
    body_content_length: Option<u64>,
    encoded: &mut BytesMut,
) -> bool {
    let path = if request_path.is_empty() {
        "/"
    } else {
        request_path
    };
    encoded.clear();
    encoded.reserve(method.as_str().len() + path.len() + request_query.len().saturating_add(129));
    encoded.extend_from_slice(method.as_str().as_bytes());
    encoded.extend_from_slice(b" ");
    encoded.extend_from_slice(path.as_bytes());
    if !request_query.is_empty() {
        encoded.extend_from_slice(b"?");
        encoded.extend_from_slice(request_query.as_bytes());
    }
    encoded.extend_from_slice(b" HTTP/1.1\r\n");

    let (_has_host, has_content_length, has_connection, has_transfer_encoding) =
        encode_request_headers(encoded, headers, authority);
    let use_chunked_body =
        !has_transfer_encoding && !has_content_length && body_content_length.is_none();
    if !has_content_length
        && !has_transfer_encoding
        && let Some(content_length) = body_content_length
    {
        encoded.extend_from_slice(CONTENT_LENGTH.as_str().as_bytes());
        encoded.extend_from_slice(b": ");
        encoded.extend_from_slice(content_length.to_string().as_bytes());
        encoded.extend_from_slice(b"\r\n");
    } else if use_chunked_body {
        encoded.extend_from_slice(TRANSFER_ENCODING.as_str().as_bytes());
        encoded.extend_from_slice(b": chunked\r\n");
    }
    if !has_connection {
        encoded.extend_from_slice(CONNECTION.as_str().as_bytes());
        encoded.extend_from_slice(b": keep-alive\r\n");
    }
    encoded.extend_from_slice(b"\r\n");
    use_chunked_body
}

#[cfg(test)]
pub(crate) async fn forward_via_sender_pool<F>(
    pool: &SharedPlainHttp1SenderPool,
    sender_pool_capacity: usize,
    target: &OutboundHttp1Target,
    started_at: Instant,
    mut make_request: F,
) -> Result<OutboundHttp1ForwardResponse, VmError>
where
    F: FnMut() -> Result<OutboundHttp1Request, VmError>,
{
    let total_started = Instant::now();
    let pool_key = target_pool_key(target)?;
    let acquired = acquire_plain_http1_connection(pool, target).await?;
    let mut connection = acquired.connection;
    let mut pool_hit = acquired.pool_hit;
    let mut connect_us = acquired.connect_us;
    let mut ready_us = acquired.ready_us;
    let mut retries = 0u64;

    let build_started = Instant::now();
    let request = make_request()?;
    let request_method = request.method.clone();
    let request_retryable = request.body.is_retryable();
    let mut request = request;
    let mut build_us = u64::try_from(build_started.elapsed().as_micros()).unwrap_or(u64::MAX);
    let mut send_us = 0u64;

    let send_started = Instant::now();
    let mut parsed_response = match send_and_parse_response(
        &mut connection,
        request,
        target.authority.as_ref(),
        &request_method,
    )
    .await
    {
        Ok(parsed) => parsed,
        Err(err) if request_retryable => {
            note_http1_pool_connection_dropped_dirty();
            retries = 1;
            send_us = u64::try_from(send_started.elapsed().as_micros()).unwrap_or(u64::MAX);
            let reacquired = acquire_plain_http1_connection(pool, target).await?;
            connection = reacquired.connection;
            pool_hit &= reacquired.pool_hit;
            connect_us = connect_us.saturating_add(reacquired.connect_us);
            ready_us = ready_us.saturating_add(reacquired.ready_us);

            let retry_build_started = Instant::now();
            let retry_request = make_request()?;
            let retry_method = retry_request.method.clone();
            request = retry_request;
            build_us = build_us.saturating_add(
                u64::try_from(retry_build_started.elapsed().as_micros()).unwrap_or(u64::MAX),
            );
            let retry_send_started = Instant::now();
            let parsed = send_and_parse_response(
                &mut connection,
                request,
                target.authority.as_ref(),
                &retry_method,
            )
            .await
            .map_err(|err| {
                note_http1_pool_connection_dropped_dirty();
                VmError::HostError(format!(
                    "outbound request to {}://{} failed while evaluating host call: {err}",
                    match target.scheme {
                        OutboundHttp1Scheme::Http => "http",
                        #[cfg(feature = "tls")]
                        OutboundHttp1Scheme::Https => "https",
                    },
                    target.authority,
                ))
            })?;
            send_us = send_us.saturating_add(
                u64::try_from(retry_send_started.elapsed().as_micros()).unwrap_or(u64::MAX),
            );
            parsed
        }
        Err(err) => {
            note_http1_pool_connection_dropped_dirty();
            return Err(VmError::HostError(format!(
                "outbound request to {}://{} failed while evaluating host call: {err}",
                match target.scheme {
                    OutboundHttp1Scheme::Http => "http",
                    #[cfg(feature = "tls")]
                    OutboundHttp1Scheme::Https => "https",
                },
                target.authority,
            )));
        }
    };
    if send_us == 0 {
        send_us = u64::try_from(send_started.elapsed().as_micros()).unwrap_or(u64::MAX);
    }
    let negotiated_alpn = connection.negotiated_alpn.clone();
    let peer_certificate_der = connection.peer_certificate_der.clone();

    let response_body = if let Some(body_kind) = parsed_response.body_kind.take() {
        let may_reuse =
            parsed_response.keep_alive && !matches!(body_kind, PlainHttp1BodyKind::CloseDelimited);
        let lease = PlainHttp1SenderLease::new(
            pool.clone(),
            pool_key,
            sender_pool_capacity,
            may_reuse,
            connection,
        );
        OutboundHttp1ForwardBody::Raw {
            body: PlainHttp1ResponseBody::new(lease, body_kind),
            content_length: parsed_response.content_length,
        }
    } else {
        if parsed_response.keep_alive {
            release_plain_http1_connection(pool, pool_key, sender_pool_capacity, connection);
        } else {
            note_http1_pool_connection_closed_clean();
        }
        OutboundHttp1ForwardBody::Empty
    };

    record_native_forward_metrics(
        pool_hit,
        retries,
        build_us,
        connect_us,
        ready_us,
        send_us,
        u64::try_from(total_started.elapsed().as_micros()).unwrap_or(u64::MAX),
    );
    record_http1_pool_metrics_sample();
    Ok(OutboundHttp1ForwardResponse {
        status: parsed_response.status,
        headers: parsed_response.headers,
        version: parsed_response.version,
        body: response_body,
        upstream_latency_ms: u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        negotiated_alpn,
        peer_certificate_der,
    })
}

pub(crate) async fn forward_serialized_via_sender_pool<F>(
    pool: &SharedPlainHttp1SenderPool,
    sender_pool_capacity: usize,
    target: &OutboundHttp1Target,
    started_at: Instant,
    mut make_request: F,
) -> Result<OutboundHttp1ForwardResponse, VmError>
where
    F: FnMut(&mut BytesMut, &str) -> Result<SerializedOutboundHttp1Request, VmError>,
{
    let total_started = Instant::now();
    let pool_key = target_pool_key(target)?;
    let acquired = acquire_plain_http1_connection(pool, target).await?;
    let mut connection = acquired.connection;
    let mut pool_hit = acquired.pool_hit;
    let mut connect_us = acquired.connect_us;
    let mut ready_us = acquired.ready_us;
    let mut retries = 0u64;

    let build_started = Instant::now();
    let request = make_request(&mut connection.write_scratch, target.authority.as_ref())?;
    let request_method = request.method.clone();
    let request_retryable = request.body.is_retryable();
    let mut request = request;
    let mut build_us = u64::try_from(build_started.elapsed().as_micros()).unwrap_or(u64::MAX);
    let mut send_us = 0u64;

    let send_started = Instant::now();
    let mut parsed_response =
        match send_serialized_request_and_parse_response(&mut connection, request, &request_method)
            .await
        {
            Ok(parsed) => parsed,
            Err(err) if request_retryable => {
                note_http1_pool_connection_dropped_dirty();
                retries = 1;
                send_us = u64::try_from(send_started.elapsed().as_micros()).unwrap_or(u64::MAX);
                let reacquired = acquire_plain_http1_connection(pool, target).await?;
                connection = reacquired.connection;
                pool_hit &= reacquired.pool_hit;
                connect_us = connect_us.saturating_add(reacquired.connect_us);
                ready_us = ready_us.saturating_add(reacquired.ready_us);

                let retry_build_started = Instant::now();
                let retry_request =
                    make_request(&mut connection.write_scratch, target.authority.as_ref())?;
                let retry_method = retry_request.method.clone();
                request = retry_request;
                build_us = build_us.saturating_add(
                    u64::try_from(retry_build_started.elapsed().as_micros()).unwrap_or(u64::MAX),
                );
                let retry_send_started = Instant::now();
                let parsed = send_serialized_request_and_parse_response(
                    &mut connection,
                    request,
                    &retry_method,
                )
                .await
                .map_err(|err| {
                    note_http1_pool_connection_dropped_dirty();
                    VmError::HostError(format!(
                        "outbound request to {}://{} failed while evaluating host call: {err}",
                        match target.scheme {
                            OutboundHttp1Scheme::Http => "http",
                            #[cfg(feature = "tls")]
                            OutboundHttp1Scheme::Https => "https",
                        },
                        target.authority,
                    ))
                })?;
                send_us = send_us.saturating_add(
                    u64::try_from(retry_send_started.elapsed().as_micros()).unwrap_or(u64::MAX),
                );
                parsed
            }
            Err(err) => {
                note_http1_pool_connection_dropped_dirty();
                return Err(VmError::HostError(format!(
                    "outbound request to {}://{} failed while evaluating host call: {err}",
                    match target.scheme {
                        OutboundHttp1Scheme::Http => "http",
                        #[cfg(feature = "tls")]
                        OutboundHttp1Scheme::Https => "https",
                    },
                    target.authority,
                )));
            }
        };
    if send_us == 0 {
        send_us = u64::try_from(send_started.elapsed().as_micros()).unwrap_or(u64::MAX);
    }
    let negotiated_alpn = connection.negotiated_alpn.clone();
    let peer_certificate_der = connection.peer_certificate_der.clone();

    let response_body = if let Some(body_kind) = parsed_response.body_kind.take() {
        let may_reuse =
            parsed_response.keep_alive && !matches!(body_kind, PlainHttp1BodyKind::CloseDelimited);
        let lease = PlainHttp1SenderLease::new(
            pool.clone(),
            pool_key,
            sender_pool_capacity,
            may_reuse,
            connection,
        );
        OutboundHttp1ForwardBody::Raw {
            body: PlainHttp1ResponseBody::new(lease, body_kind),
            content_length: parsed_response.content_length,
        }
    } else {
        if parsed_response.keep_alive {
            release_plain_http1_connection(pool, pool_key, sender_pool_capacity, connection);
        } else {
            note_http1_pool_connection_closed_clean();
        }
        OutboundHttp1ForwardBody::Empty
    };

    record_native_forward_metrics(
        pool_hit,
        retries,
        build_us,
        connect_us,
        ready_us,
        send_us,
        u64::try_from(total_started.elapsed().as_micros()).unwrap_or(u64::MAX),
    );
    record_http1_pool_metrics_sample();
    Ok(OutboundHttp1ForwardResponse {
        status: parsed_response.status,
        headers: parsed_response.headers,
        version: parsed_response.version,
        body: response_body,
        upstream_latency_ms: u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        negotiated_alpn,
        peer_certificate_der,
    })
}

#[cfg(test)]
async fn send_and_parse_response(
    connection: &mut PlainHttp1PooledConnection,
    request: OutboundHttp1Request,
    authority: &str,
    request_method: &Method,
) -> Result<ParsedResponseHead, VmError> {
    send_request(connection, request, authority).await?;
    parse_response_head(connection, request_method).await
}

async fn send_serialized_request_and_parse_response(
    connection: &mut PlainHttp1PooledConnection,
    request: SerializedOutboundHttp1Request,
    request_method: &Method,
) -> Result<ParsedResponseHead, VmError> {
    connection
        .stream
        .write_all(&connection.write_scratch)
        .await
        .map_err(|err| {
            VmError::HostError(format!(
                "failed to write plain http/1.1 upstream request: {err}"
            ))
        })?;
    send_request_body(connection, request.body, request.use_chunked_body).await?;
    parse_response_head(connection, request_method).await
}

#[cfg(test)]
async fn send_request(
    connection: &mut PlainHttp1PooledConnection,
    request: OutboundHttp1Request,
    authority: &str,
) -> Result<(), VmError> {
    let use_chunked_body =
        serialize_request_head_into(&request, authority, &mut connection.write_scratch);
    connection
        .stream
        .write_all(&connection.write_scratch)
        .await
        .map_err(|err| {
            VmError::HostError(format!(
                "failed to write plain http/1.1 upstream request: {err}"
            ))
        })?;
    send_request_body(connection, request.body, use_chunked_body).await
}

async fn send_request_body(
    connection: &mut PlainHttp1PooledConnection,
    body: OutboundHttp1RequestBody,
    use_chunked_body: bool,
) -> Result<(), VmError> {
    match body {
        OutboundHttp1RequestBody::Empty => Ok(()),
        OutboundHttp1RequestBody::Bytes(body) => {
            connection.stream.write_all(&body).await.map_err(|err| {
                VmError::HostError(format!(
                    "failed to write plain http/1.1 upstream request body: {err}"
                ))
            })
        }
        OutboundHttp1RequestBody::Streaming {
            content_length,
            mut stream,
        } => {
            let mut remaining = content_length;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                if chunk.is_empty() {
                    continue;
                }
                if let Some(remaining_bytes) = remaining.as_mut() {
                    let chunk_len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
                    if chunk_len > *remaining_bytes {
                        return Err(VmError::HostError(
                            "streamed outbound http/1.1 request body exceeded declared content-length"
                                .to_string(),
                        ));
                    }
                    *remaining_bytes -= chunk_len;
                }
                if use_chunked_body {
                    connection.write_scratch.clear();
                    connection
                        .write_scratch
                        .reserve(HTTP1_CHUNK_WRITE_OVERHEAD_BYTES + chunk.len());
                    append_chunk_prefix(&mut connection.write_scratch, chunk.len());
                    connection
                        .stream
                        .write_all(&connection.write_scratch)
                        .await
                        .map_err(|err| {
                            VmError::HostError(format!(
                                "failed to write chunked plain http/1.1 request chunk header: {err}"
                            ))
                        })?;
                }
                connection.stream.write_all(&chunk).await.map_err(|err| {
                    VmError::HostError(format!(
                        "failed to stream plain http/1.1 upstream request body: {err}"
                    ))
                })?;
                if use_chunked_body {
                    connection.stream.write_all(b"\r\n").await.map_err(|err| {
                        VmError::HostError(format!(
                            "failed to finalize chunked plain http/1.1 request chunk: {err}"
                        ))
                    })?;
                }
            }
            if let Some(remaining_bytes) = remaining
                && remaining_bytes != 0
            {
                return Err(VmError::HostError(
                    "streamed outbound http/1.1 request body ended before declared content-length"
                        .to_string(),
                ));
            }
            if use_chunked_body {
                connection.write_scratch.clear();
                connection.write_scratch.extend_from_slice(b"0\r\n\r\n");
                connection
                    .stream
                    .write_all(&connection.write_scratch)
                    .await
                    .map_err(|err| {
                        VmError::HostError(format!(
                            "failed to finalize chunked plain http/1.1 request body: {err}"
                        ))
                    })?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        OutboundHttp1Request, OutboundHttp1RequestBody, OutboundHttp1Scheme, OutboundHttp1Target,
        forward_via_sender_pool, new_shared_plain_http1_sender_pool,
    };
    use axum::{
        body::Bytes,
        http::{HeaderMap, Method, Version},
    };
    use futures_util::stream::try_unfold;
    use std::sync::Arc;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::oneshot,
        time::{Duration, timeout},
    };
    use vm::VmError;

    fn split_http1_head_and_body(buffer: &[u8]) -> Option<(&[u8], &[u8])> {
        buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|offset| buffer.split_at(offset + 4))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn streams_request_body_before_full_body_is_available() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("listener addr should exist");
        let (first_chunk_tx, first_chunk_rx) = oneshot::channel::<()>();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept should succeed");
            let mut buffer = Vec::new();
            let mut tmp = [0u8; 1024];
            let mut first_chunk_tx = Some(first_chunk_tx);
            loop {
                let read = stream.read(&mut tmp).await.expect("read should succeed");
                assert!(read > 0, "upstream request should not close early");
                buffer.extend_from_slice(&tmp[..read]);
                let Some((head, body)) = split_http1_head_and_body(&buffer) else {
                    continue;
                };
                let head_text = std::str::from_utf8(head).expect("head should be utf8");
                assert!(
                    head_text.contains("content-length: 10"),
                    "request head should advertise the streamed content length",
                );
                if body.len() >= 5 {
                    assert_eq!(&body[..5], b"hello");
                    if let Some(first_chunk_tx) = first_chunk_tx.take() {
                        first_chunk_tx
                            .send(())
                            .expect("first chunk signal should be deliverable");
                    }
                }
                if body.len() >= 10 {
                    assert_eq!(&body[..10], b"helloworld");
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .expect("response should write");
        });

        let pool = new_shared_plain_http1_sender_pool();
        let target = OutboundHttp1Target {
            scheme: OutboundHttp1Scheme::Http,
            authority: Arc::from(format!("127.0.0.1:{}", addr.port())),
            host: Arc::from("127.0.0.1"),
            port: addr.port(),
            plain_pool_key: None,
            #[cfg(feature = "tls")]
            tls_flow: None,
        };
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let body_stream = try_unfold(
            (Some(Bytes::from_static(b"hello")), Some(release_rx)),
            |(first, release_rx)| async move {
                if let Some(first) = first {
                    return Ok::<_, VmError>(Some((first, (None, release_rx))));
                }
                let Some(release_rx) = release_rx else {
                    return Ok(None);
                };
                release_rx
                    .await
                    .map_err(|_| VmError::HostError("release signal dropped".to_string()))?;
                Ok(Some((Bytes::from_static(b"world"), (None, None))))
            },
        );
        let mut body_stream = Some(Box::pin(body_stream));

        let forward = tokio::spawn(async move {
            forward_via_sender_pool(&pool, 8, &target, std::time::Instant::now(), move || {
                let stream = body_stream
                    .take()
                    .expect("streaming request should only be built once");
                Ok(OutboundHttp1Request {
                    method: Method::POST,
                    path_and_query: "/".to_string(),
                    headers: HeaderMap::new().into(),
                    body: OutboundHttp1RequestBody::Streaming {
                        content_length: Some(10),
                        stream,
                    },
                })
            })
            .await
        });

        timeout(Duration::from_millis(250), first_chunk_rx)
            .await
            .expect("upstream should observe the first request chunk before the second is released")
            .expect("first chunk signal should arrive");
        release_tx
            .send(())
            .expect("second chunk release should be deliverable");

        let response = forward
            .await
            .expect("forward task should complete")
            .expect("forward should succeed");
        assert_eq!(response.status, 200);
        assert_eq!(response.version, Version::HTTP_11);
        server.await.expect("server task should complete");
    }
}
