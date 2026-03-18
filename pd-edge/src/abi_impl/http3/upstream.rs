#![cfg_attr(not(feature = "http3"), allow(dead_code))]

#[cfg(feature = "http3")]
use std::collections::HashMap;
#[cfg(feature = "http3")]
use std::env;
#[cfg(feature = "http3")]
use std::io;
#[cfg(feature = "http3")]
use std::pin::Pin;
#[cfg(feature = "http3")]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};

#[cfg(feature = "http3")]
use axum::{
    body::Bytes,
    http::{HeaderMap, HeaderValue, Method, Request, Response, header::HOST},
};
#[cfg(feature = "http3")]
use futures_util::future;
#[cfg(feature = "http3")]
use futures_util::{Stream, StreamExt};
#[cfg(feature = "http3")]
use quinn::ConnectionError;
#[cfg(feature = "http3")]
use tokio::net::lookup_host;

#[cfg(feature = "http3")]
use crate::abi_impl::{
    http::state::HttpUpstreamScheme,
    quic::{
        build_quic_client_config, negotiated_alpn, peer_certificate_der, tune_udp_socket_buffers,
    },
    transport::{TlsFlowState, TlsSessionCacheKey, tls_session_cache_key},
};
#[cfg(feature = "http3")]
use crate::cache::BoundedLruStore;
#[cfg(feature = "http3")]
use crate::lock_metrics::{self, LockMetricKey};

use super::model::Http3UpstreamMode;
#[cfg(feature = "http3")]
use super::model::{
    Http3ControlEventSource, Http3GoawayState, Http3ResetState, Http3SessionFrontier,
    Http3SessionGoal, Http3StreamFrontier, Http3StreamRef, session_origin,
};

#[cfg(feature = "http3")]
const DEFAULT_HTTP3_UPSTREAM_MAX_REUSABLE_SESSIONS_PER_ORIGIN: usize = 8;
#[cfg(feature = "http3")]
const DEFAULT_HTTP3_UPSTREAM_TARGET_ACTIVE_STREAMS_PER_SESSION: usize = 4;
#[cfg(feature = "http3")]
static HTTP3_UPSTREAM_MAX_REUSABLE_SESSIONS_PER_ORIGIN: OnceLock<usize> = OnceLock::new();
#[cfg(feature = "http3")]
static HTTP3_UPSTREAM_TARGET_ACTIVE_STREAMS_PER_SESSION: OnceLock<usize> = OnceLock::new();

#[cfg(feature = "http3")]
fn http3_upstream_max_reusable_sessions_per_origin() -> usize {
    *HTTP3_UPSTREAM_MAX_REUSABLE_SESSIONS_PER_ORIGIN.get_or_init(|| {
        env::var("PD_EDGE_HTTP3_UPSTREAM_MAX_REUSABLE_SESSIONS_PER_ORIGIN")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_HTTP3_UPSTREAM_MAX_REUSABLE_SESSIONS_PER_ORIGIN)
    })
}

#[cfg(feature = "http3")]
fn http3_upstream_target_active_streams_per_session() -> usize {
    *HTTP3_UPSTREAM_TARGET_ACTIVE_STREAMS_PER_SESSION.get_or_init(|| {
        env::var("PD_EDGE_HTTP3_UPSTREAM_TARGET_ACTIVE_STREAMS_PER_SESSION")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_HTTP3_UPSTREAM_TARGET_ACTIVE_STREAMS_PER_SESSION)
    })
}

#[derive(Clone, Debug)]
pub(crate) struct Http3SessionStore {
    #[cfg(feature = "http3")]
    sessions: BoundedLruStore<Http3SessionKey, Http3SessionPoolEntry>,
    #[cfg(feature = "http3")]
    next_session_id: u64,
}

#[cfg(test)]
impl Http3SessionStore {
    pub(crate) fn capacity(&self) -> usize {
        #[cfg(feature = "http3")]
        {
            self.sessions.capacity()
        }
        #[cfg(not(feature = "http3"))]
        {
            0
        }
    }
}

pub(crate) type SharedHttp3UpstreamSessions = Arc<Mutex<Http3SessionStore>>;

pub(crate) fn new_shared_http3_upstream_sessions(capacity: usize) -> SharedHttp3UpstreamSessions {
    #[cfg(not(feature = "http3"))]
    let _ = capacity;

    Arc::new(Mutex::new(Http3SessionStore {
        #[cfg(feature = "http3")]
        sessions: BoundedLruStore::new(capacity),
        #[cfg(feature = "http3")]
        next_session_id: 0,
    }))
}

#[cfg(feature = "http3")]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Http3SessionKey {
    origin: String,
    tls_key: Option<TlsSessionCacheKey>,
}

#[cfg(feature = "http3")]
#[derive(Clone, Debug, Default)]
struct Http3SessionPoolEntry {
    sessions: Vec<Arc<Http3UpstreamSession>>,
}

#[cfg(feature = "http3")]
#[derive(Clone, Debug, PartialEq, Eq)]
struct Http3UpstreamStreamState {
    stream_id: u64,
    exchange_handle: i64,
    frontier: Http3StreamFrontier,
    reset: Option<Http3ResetState>,
}

#[cfg(feature = "http3")]
#[derive(Clone, Debug, PartialEq, Eq)]
struct Http3UpstreamSessionDagState {
    frontier: Http3SessionFrontier,
    goaway: Option<Http3GoawayState>,
    streams: HashMap<u64, Http3UpstreamStreamState>,
}

#[cfg(feature = "http3")]
type Http3SendRequest = h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>;
#[cfg(feature = "http3")]
type Http3Driver = h3::client::Connection<h3_quinn::Connection, Bytes>;
#[cfg(feature = "http3")]
type Http3RequestStream = h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>;

#[cfg(feature = "http3")]
struct Http3UpstreamSession {
    session_id: u64,
    _endpoint: quinn::Endpoint,
    _connection: quinn::Connection,
    sender: tokio::sync::Mutex<Http3SendRequest>,
    peer_addr: String,
    negotiated_alpn: Option<String>,
    peer_certificate_der: Option<Vec<u8>>,
    dag: Mutex<Http3UpstreamSessionDagState>,
}

#[cfg(feature = "http3")]
impl std::fmt::Debug for Http3UpstreamSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Http3UpstreamSession")
            .field("session_id", &self.session_id)
            .field("peer_addr", &self.peer_addr)
            .field("negotiated_alpn", &self.negotiated_alpn)
            .finish()
    }
}

#[cfg(feature = "http3")]
#[derive(Clone, Debug)]
pub(crate) struct Http3ResponseBodyTracker {
    session: Arc<Http3UpstreamSession>,
    stream_ref: Http3StreamRef,
}

#[cfg(not(feature = "http3"))]
#[derive(Clone, Debug, Default)]
pub(crate) struct Http3ResponseBodyTracker;

#[cfg(not(feature = "http3"))]
impl Http3ResponseBodyTracker {
    pub(crate) fn note_response_body_ready(&self) {}

    pub(crate) fn note_body_eof(&self) {}

    pub(crate) fn note_body_error(&self, _observed: &Http3ObservedError) {}
}

#[cfg(feature = "http3")]
pub(crate) struct Http3StartedResponse {
    pub(crate) response: Response<()>,
    pub(crate) peer_addr: Option<String>,
    pub(crate) negotiated_alpn: Option<String>,
    pub(crate) peer_certificate_der: Option<Vec<u8>>,
    pub(crate) stream_ref: Http3StreamRef,
    pub(crate) request_stream: Http3RequestStream,
    pub(crate) body_tracker: Http3ResponseBodyTracker,
}

#[cfg(feature = "http3")]
#[derive(Debug)]
pub(crate) enum Http3RequestError {
    FallbackToHttp2 { negotiated_alpn: Option<String> },
    Transport(String),
}

#[cfg(feature = "http3")]
impl Http3RequestError {
    pub(crate) fn transport(message: impl Into<String>) -> Self {
        Self::Transport(message.into())
    }

    pub(crate) fn into_message(self) -> String {
        match self {
            Self::FallbackToHttp2 { negotiated_alpn } => format!(
                "http3 request fell back to lower HTTP versions after negotiating {}",
                negotiated_alpn.as_deref().unwrap_or("no ALPN"),
            ),
            Self::Transport(message) => message,
        }
    }
}

#[cfg(feature = "http3")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Http3ObservedError {
    pub(crate) message: String,
    pub(crate) reset: Option<Http3ResetState>,
    pub(crate) goaway: Option<Http3GoawayState>,
}

#[cfg(not(feature = "http3"))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Http3ObservedError {
    pub(crate) message: String,
}

#[cfg(feature = "http3")]
pub(crate) enum Http3RequestBody {
    Empty,
    Bytes(Bytes),
    Streaming(Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send + 'static>>),
}

#[cfg(feature = "http3")]
pub(crate) struct Http3SendRequestOptions {
    pub(crate) exchange_handle: i64,
    pub(crate) target_scheme: HttpUpstreamScheme,
    pub(crate) target_host: String,
    pub(crate) target_port: u16,
    pub(crate) target_host_header: Option<String>,
    pub(crate) upstream_url: String,
    pub(crate) method: Method,
    pub(crate) headers: HeaderMap,
    pub(crate) request_body: Http3RequestBody,
    pub(crate) tls_flow: TlsFlowState,
    pub(crate) mode: Http3UpstreamMode,
    pub(crate) sessions: SharedHttp3UpstreamSessions,
}

#[cfg(feature = "http3")]
pub(crate) fn should_use_explicit_upstream_transport(
    mode: Http3UpstreamMode,
    sessions: Option<&SharedHttp3UpstreamSessions>,
) -> bool {
    !matches!(mode, Http3UpstreamMode::Disabled) && sessions.is_some()
}

#[cfg(not(feature = "http3"))]
pub(crate) fn should_use_explicit_upstream_transport(
    _mode: Http3UpstreamMode,
    _sessions: Option<&SharedHttp3UpstreamSessions>,
) -> bool {
    false
}

#[cfg(feature = "http3")]
impl Http3UpstreamSession {
    fn new(
        session_id: u64,
        endpoint: quinn::Endpoint,
        connection: quinn::Connection,
        sender: Http3SendRequest,
        peer_addr: String,
        negotiated_alpn: Option<String>,
        peer_certificate_der: Option<Vec<u8>>,
    ) -> Self {
        let mut dag = Http3UpstreamSessionDagState {
            frontier: Http3SessionFrontier::Candidate,
            goaway: None,
            streams: HashMap::new(),
        };
        dag.advance_session_goal(Http3SessionGoal::Attached);
        dag.frontier = Http3SessionFrontier::ControlStreamsOpen;
        dag.frontier = Http3SessionFrontier::SettingsExchanged;
        dag.advance_session_goal(Http3SessionGoal::Open);
        Self {
            session_id,
            _endpoint: endpoint,
            _connection: connection,
            sender: tokio::sync::Mutex::new(sender),
            peer_addr,
            negotiated_alpn,
            peer_certificate_der,
            dag: Mutex::new(dag),
        }
    }

    fn is_reusable(&self) -> bool {
        let dag = self.lock_dag();
        dag.can_accept_new_streams()
    }

    fn should_retain(&self) -> bool {
        let dag = self.lock_dag();
        !dag.frontier.is_terminal() || dag.has_active_streams()
    }

    fn active_stream_count(&self) -> usize {
        let dag = self.lock_dag();
        dag.streams.len()
    }

    async fn sender_clone(&self) -> Http3SendRequest {
        self.sender.lock().await.clone()
    }

    fn attach_stream(&self, exchange_handle: i64, stream_id: u64) -> Http3StreamRef {
        let mut dag = self.lock_dag();
        dag.attach_stream(self.session_id, exchange_handle, stream_id)
    }

    fn mark_stream_request_committed(&self, stream_id: u64, body_present: bool) {
        let mut dag = self.lock_dag();
        dag.mark_stream_request_committed(stream_id, body_present);
    }

    fn mark_stream_response_head_ready(&self, stream_id: u64) {
        let mut dag = self.lock_dag();
        dag.mark_stream_response_head_ready(stream_id);
    }

    fn mark_stream_response_body_ready(&self, stream_id: u64) {
        let mut dag = self.lock_dag();
        dag.mark_stream_response_body_ready(stream_id);
    }

    fn mark_stream_closed(&self, stream_id: u64) {
        let mut dag = self.lock_dag();
        dag.mark_stream_closed(stream_id);
    }

    fn mark_stream_reset(
        &self,
        stream_id: u64,
        reason: Option<String>,
        source: Http3ControlEventSource,
    ) {
        let mut dag = self.lock_dag();
        dag.mark_stream_reset(stream_id, reason, source);
    }

    fn mark_goaway(&self, reason: Option<String>, source: Http3ControlEventSource) {
        let mut dag = self.lock_dag();
        dag.goaway = Some(Http3GoawayState { reason, source });
        dag.advance_session_goal(Http3SessionGoal::Draining);
    }

    fn mark_connection_closed(&self) {
        let mut dag = self.lock_dag();
        if dag.frontier == Http3SessionFrontier::Draining && dag.streams.is_empty() {
            dag.frontier = Http3SessionFrontier::Closed;
        } else if dag.frontier == Http3SessionFrontier::Open {
            dag.frontier = Http3SessionFrontier::Draining;
        }
    }

    fn mark_connection_failed(&self, reason: Option<String>) {
        let mut dag = self.lock_dag();
        dag.frontier = Http3SessionFrontier::Failed;
        for stream in dag.streams.values_mut() {
            if !stream.frontier.is_terminal() {
                stream.reset = Some(Http3ResetState {
                    reason: reason.clone(),
                    source: Http3ControlEventSource::Transport,
                });
                stream.frontier = Http3StreamFrontier::Reset;
            }
        }
    }

    fn lock_dag(&self) -> lock_metrics::ProfiledMutexGuard<'_, Http3UpstreamSessionDagState> {
        lock_metrics::lock(
            &self.dag,
            LockMetricKey::Http3UpstreamSessionDag,
            "http3 upstream session lock poisoned",
        )
    }
}

#[cfg(feature = "http3")]
impl Http3UpstreamSessionDagState {
    fn advance_session_goal(&mut self, goal: Http3SessionGoal) {
        match goal {
            Http3SessionGoal::Attached => {
                if self.frontier == Http3SessionFrontier::Candidate {
                    self.frontier = Http3SessionFrontier::Attached;
                }
            }
            Http3SessionGoal::Open => {
                if !self.frontier.is_terminal() {
                    self.frontier = Http3SessionFrontier::Open;
                }
            }
            Http3SessionGoal::Draining => {
                if !self.frontier.is_terminal() {
                    self.frontier = Http3SessionFrontier::Draining;
                }
            }
        }
    }

    fn can_accept_new_streams(&self) -> bool {
        self.frontier == Http3SessionFrontier::Open && self.goaway.is_none()
    }

    fn has_active_streams(&self) -> bool {
        self.streams
            .values()
            .any(|stream| !stream.frontier.is_terminal())
    }

    fn attach_stream(
        &mut self,
        session_id: u64,
        exchange_handle: i64,
        stream_id: u64,
    ) -> Http3StreamRef {
        self.streams.insert(
            stream_id,
            Http3UpstreamStreamState {
                stream_id,
                exchange_handle,
                frontier: Http3StreamFrontier::AttachedToExchange,
                reset: None,
            },
        );
        Http3StreamRef {
            session_id,
            stream_id,
        }
    }

    fn mark_stream_request_committed(&mut self, stream_id: u64, body_present: bool) {
        let Some(stream) = self.streams.get_mut(&stream_id) else {
            return;
        };
        stream.frontier = if body_present {
            Http3StreamFrontier::RequestBodyOpen
        } else {
            Http3StreamFrontier::RequestCommitted
        };
    }

    fn mark_stream_response_head_ready(&mut self, stream_id: u64) {
        if let Some(stream) = self.streams.get_mut(&stream_id) {
            stream.frontier = Http3StreamFrontier::ResponseHeadReady;
        }
    }

    fn mark_stream_response_body_ready(&mut self, stream_id: u64) {
        if let Some(stream) = self.streams.get_mut(&stream_id) {
            stream.frontier = Http3StreamFrontier::ResponseBodyReady;
        }
    }

    fn mark_stream_closed(&mut self, stream_id: u64) {
        if let Some(stream) = self.streams.get_mut(&stream_id) {
            stream.frontier = Http3StreamFrontier::Closed;
        }
        self.streams
            .retain(|_, stream| !stream.frontier.is_terminal());
        if self.frontier == Http3SessionFrontier::Draining && self.streams.is_empty() {
            self.frontier = Http3SessionFrontier::Closed;
        }
    }

    fn mark_stream_reset(
        &mut self,
        stream_id: u64,
        reason: Option<String>,
        source: Http3ControlEventSource,
    ) {
        if let Some(stream) = self.streams.get_mut(&stream_id) {
            stream.reset = Some(Http3ResetState { reason, source });
            stream.frontier = Http3StreamFrontier::Reset;
        }
        self.streams
            .retain(|_, stream| !stream.frontier.is_terminal());
    }
}

#[cfg(feature = "http3")]
impl Http3ResponseBodyTracker {
    pub(crate) fn note_response_body_ready(&self) {
        self.session
            .mark_stream_response_body_ready(self.stream_ref.stream_id);
    }

    pub(crate) fn note_body_eof(&self) {
        self.session.mark_stream_closed(self.stream_ref.stream_id);
    }

    pub(crate) fn note_body_error(&self, observed: &Http3ObservedError) {
        apply_stream_error(&self.session, self.stream_ref.stream_id, observed);
    }
}

#[cfg(feature = "http3")]
impl Http3SessionPoolEntry {
    fn retain_sessions(&mut self) {
        self.sessions.retain(|session| session.should_retain());
    }

    fn reusable_session_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|session| session.is_reusable())
            .count()
    }

    fn select_reusable_session(&self) -> Option<(Arc<Http3UpstreamSession>, usize)> {
        self.sessions
            .iter()
            .filter(|session| session.is_reusable())
            .map(|session| (session.clone(), session.active_stream_count()))
            .min_by_key(|(_, active_streams)| *active_streams)
    }
}

#[cfg(feature = "http3")]
fn session_key(
    target_scheme: HttpUpstreamScheme,
    target_host: &str,
    target_port: u16,
    tls_flow: &TlsFlowState,
) -> Option<Http3SessionKey> {
    Some(Http3SessionKey {
        origin: session_origin(target_scheme, target_host, target_port)?,
        tls_key: tls_session_cache_key(target_scheme.as_str(), target_host, target_port, tls_flow),
    })
}

#[cfg(feature = "http3")]
fn build_http3_request(
    upstream_url: &str,
    target_host: &str,
    target_port: u16,
    target_host_header: Option<&str>,
    method: Method,
    mut headers: HeaderMap,
) -> Result<Request<()>, Http3RequestError> {
    if !headers.contains_key(HOST) {
        let host_value = target_host_header
            .map(str::to_string)
            .unwrap_or_else(|| format_upstream_authority(target_host, target_port));
        let value = HeaderValue::from_str(&host_value).map_err(|err| {
            Http3RequestError::transport(format!("invalid host header for '{upstream_url}': {err}"))
        })?;
        headers.insert(HOST, value);
    }

    let mut request = Request::builder().method(method).uri(upstream_url);
    for (name, value) in &headers {
        request = request.header(name, value);
    }
    request
        .body(())
        .map_err(|err| Http3RequestError::transport(format!("invalid http3 request: {err}")))
}

#[cfg(feature = "http3")]
fn format_upstream_authority(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(feature = "http3")]
async fn acquire_or_open_session(
    sessions: &SharedHttp3UpstreamSessions,
    target_scheme: HttpUpstreamScheme,
    target_host: &str,
    target_port: u16,
    tls_flow: &TlsFlowState,
    mode: Http3UpstreamMode,
) -> Result<Arc<Http3UpstreamSession>, Http3RequestError> {
    let key = session_key(target_scheme, target_host, target_port, tls_flow).ok_or_else(|| {
        Http3RequestError::transport(format!(
            "http3 session key could not be built for target {target_host}:{target_port}"
        ))
    })?;

    {
        let mut guard = lock_metrics::lock(
            sessions,
            LockMetricKey::Http3UpstreamSessionStore,
            "http3 upstream session store lock poisoned",
        );
        guard
            .sessions
            .retain(|_, entry| entry.sessions.iter().any(|session| session.should_retain()));
        if let Some(entry) = guard.sessions.get_mut(&key) {
            entry.retain_sessions();
            if let Some((session, active_streams)) = entry.select_reusable_session()
                && (active_streams < http3_upstream_target_active_streams_per_session()
                    || entry.reusable_session_count()
                        >= http3_upstream_max_reusable_sessions_per_origin())
            {
                return Ok(session);
            }
        }
    }

    match connect_session(target_scheme, target_host, target_port, tls_flow, mode).await {
        Ok(session) => {
            let mut guard = lock_metrics::lock(
                sessions,
                LockMetricKey::Http3UpstreamSessionStore,
                "http3 upstream session store lock poisoned",
            );
            guard
                .sessions
                .retain(|_, entry| entry.sessions.iter().any(|cached| cached.should_retain()));
            if guard.sessions.peek(&key).is_some() {
                {
                    let entry = guard
                        .sessions
                        .get_mut(&key)
                        .expect("existing http3 session entry should still exist");
                    entry.retain_sessions();
                    if let Some((cached, active_streams)) = entry.select_reusable_session()
                        && (active_streams < http3_upstream_target_active_streams_per_session()
                            || entry.reusable_session_count()
                                >= http3_upstream_max_reusable_sessions_per_origin())
                    {
                        return Ok(cached);
                    }
                }

                let session_id = guard.next_session_id.saturating_add(1);
                guard.next_session_id = session_id;
                let session = session.into_session(session_id);
                guard
                    .sessions
                    .get_mut(&key)
                    .expect("existing http3 session entry should still exist")
                    .sessions
                    .push(session.clone());
                return Ok(session);
            }
            let session_id = guard.next_session_id.saturating_add(1);
            guard.next_session_id = session_id;
            let session = session.into_session(session_id);
            guard.sessions.insert(
                key,
                Http3SessionPoolEntry {
                    sessions: vec![session.clone()],
                },
            );
            Ok(session)
        }
        Err(_) if matches!(mode, Http3UpstreamMode::Preferred) => {
            Err(Http3RequestError::FallbackToHttp2 {
                negotiated_alpn: None,
            })
        }
        Err(err) => Err(err),
    }
}

#[cfg(feature = "http3")]
struct ConnectedHttp3Session {
    endpoint: quinn::Endpoint,
    connection: quinn::Connection,
    sender: Http3SendRequest,
    driver: Http3Driver,
    peer_addr: String,
    negotiated_alpn: Option<String>,
    peer_certificate_der: Option<Vec<u8>>,
}

#[cfg(feature = "http3")]
impl ConnectedHttp3Session {
    fn into_session(self, session_id: u64) -> Arc<Http3UpstreamSession> {
        let session = Arc::new(Http3UpstreamSession::new(
            session_id,
            self.endpoint,
            self.connection,
            self.sender,
            self.peer_addr,
            self.negotiated_alpn,
            self.peer_certificate_der,
        ));
        spawn_http3_connection_driver(self.driver, session.clone());
        session
    }
}

#[cfg(feature = "http3")]
async fn connect_session(
    target_scheme: HttpUpstreamScheme,
    target_host: &str,
    target_port: u16,
    tls_flow: &TlsFlowState,
    mode: Http3UpstreamMode,
) -> Result<ConnectedHttp3Session, Http3RequestError> {
    if target_scheme != HttpUpstreamScheme::Https {
        return Err(Http3RequestError::transport(
            "http3 requires an https target".to_string(),
        ));
    }
    let remote = lookup_host((target_host, target_port))
        .await
        .map_err(|err| {
            Http3RequestError::transport(format!(
                "http3 failed to resolve {target_host}:{target_port}: {err}"
            ))
        })?
        .next()
        .ok_or_else(|| {
            Http3RequestError::transport(format!(
                "http3 could not resolve {target_host}:{target_port}"
            ))
        })?;
    let bind_addr: std::net::SocketAddr = if remote.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    }
    .parse()
    .expect("valid wildcard addr");
    let socket = std::net::UdpSocket::bind(bind_addr).map_err(|err| {
        Http3RequestError::transport(format!("http3 failed to bind local udp socket: {err}"))
    })?;
    socket.set_nonblocking(true).map_err(|err| {
        Http3RequestError::transport(format!("http3 failed to set udp socket nonblocking: {err}"))
    })?;
    tune_udp_socket_buffers(&socket).map_err(|err| {
        Http3RequestError::transport(format!("http3 failed to tune udp socket buffers: {err}"))
    })?;
    let mut endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        None,
        socket,
        Arc::new(quinn::TokioRuntime),
    )
    .map_err(|err| {
        Http3RequestError::transport(format!("http3 failed to create endpoint: {err}"))
    })?;
    endpoint.set_default_client_config(
        build_quic_client_config(tls_flow).map_err(Http3RequestError::transport)?,
    );

    let server_name = if !tls_flow.server_name().is_empty() {
        tls_flow.server_name().to_string()
    } else if !tls_flow.peer_name().is_empty() {
        tls_flow.peer_name().to_string()
    } else {
        target_host.to_string()
    };
    let connection = endpoint
        .connect(remote, &server_name)
        .map_err(|err| {
            Http3RequestError::transport(format!(
                "http3 failed to start QUIC connection to {remote}: {err}"
            ))
        })?
        .await
        .map_err(|err| {
            if matches!(mode, Http3UpstreamMode::Preferred) {
                return Http3RequestError::FallbackToHttp2 {
                    negotiated_alpn: None,
                };
            }
            Http3RequestError::transport(format!("http3 QUIC handshake failed for {remote}: {err}"))
        })?;

    let negotiated = negotiated_alpn(&connection);
    if negotiated.as_deref() != Some(crate::abi_impl::quic::ALPN_PROTOCOL) {
        if matches!(mode, Http3UpstreamMode::Preferred) {
            return Err(Http3RequestError::FallbackToHttp2 {
                negotiated_alpn: negotiated,
            });
        }
        return Err(Http3RequestError::transport(format!(
            "http3 negotiated {} instead of {}",
            negotiated.as_deref().unwrap_or("no ALPN"),
            crate::abi_impl::quic::ALPN_PROTOCOL,
        )));
    }

    let peer_addr = connection.remote_address().to_string();
    let peer_certificate_der = peer_certificate_der(&connection);
    let h3_connection = h3_quinn::Connection::new(connection.clone());
    let (driver, sender) = h3::client::new(h3_connection).await.map_err(|err| {
        if matches!(mode, Http3UpstreamMode::Preferred) {
            return Http3RequestError::FallbackToHttp2 {
                negotiated_alpn: negotiated.clone(),
            };
        }
        Http3RequestError::transport(format!(
            "http3 connection setup failed after QUIC handshake: {err}"
        ))
    })?;
    Ok(ConnectedHttp3Session {
        endpoint,
        connection,
        sender,
        driver,
        peer_addr,
        negotiated_alpn: negotiated,
        peer_certificate_der,
    })
}

#[cfg(feature = "http3")]
fn spawn_http3_connection_driver(driver: Http3Driver, session: Arc<Http3UpstreamSession>) {
    tokio::spawn(async move {
        let mut driver = driver;
        let result = future::poll_fn(|cx| driver.poll_close(cx)).await;
        if result.is_h3_no_error() {
            session.mark_connection_closed();
            return;
        }
        let observed = classify_http3_error(&result);
        apply_session_error(&session, &observed);
    });
}

#[cfg(feature = "http3")]
fn apply_session_error(session: &Arc<Http3UpstreamSession>, observed: &Http3ObservedError) {
    if let Some(goaway) = &observed.goaway {
        session.mark_goaway(goaway.reason.clone(), goaway.source);
    }
    if observed.reset.is_none() && observed.goaway.is_none() {
        session.mark_connection_failed(Some(observed.message.clone()));
    }
}

#[cfg(feature = "http3")]
fn apply_stream_error(
    session: &Arc<Http3UpstreamSession>,
    stream_id: u64,
    observed: &Http3ObservedError,
) {
    if let Some(reset) = &observed.reset {
        session.mark_stream_reset(stream_id, reset.reason.clone(), reset.source);
    }
    if let Some(goaway) = &observed.goaway {
        session.mark_goaway(goaway.reason.clone(), goaway.source);
    }
    if observed.reset.is_none() && observed.goaway.is_none() {
        session.mark_connection_failed(Some(observed.message.clone()));
    }
}

#[cfg(feature = "http3")]
pub(crate) fn classify_http3_error<E>(err: &E) -> Http3ObservedError
where
    E: std::error::Error + 'static,
{
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(candidate) = current {
        if let Some(connection_err) = candidate.downcast_ref::<ConnectionError>() {
            return Http3ObservedError {
                message: connection_err.to_string(),
                reset: None,
                goaway: Some(Http3GoawayState {
                    reason: Some(connection_err.to_string()),
                    source: Http3ControlEventSource::Transport,
                }),
            };
        }
        current = candidate.source();
    }
    Http3ObservedError {
        message: err.to_string(),
        reset: None,
        goaway: None,
    }
}

#[cfg(feature = "http3")]
pub(crate) async fn send_request(
    options: Http3SendRequestOptions,
) -> Result<Http3StartedResponse, Http3RequestError> {
    let session = acquire_or_open_session(
        &options.sessions,
        options.target_scheme,
        &options.target_host,
        options.target_port,
        &options.tls_flow,
        options.mode,
    )
    .await?;
    let request = build_http3_request(
        &options.upstream_url,
        &options.target_host,
        options.target_port,
        options.target_host_header.as_deref(),
        options.method.clone(),
        options.headers.clone(),
    )?;
    let mut sender = session.sender_clone().await;
    let mut request_stream = sender.send_request(request).await.map_err(|err| {
        let observed = classify_http3_error(&err);
        apply_session_error(&session, &observed);
        match options.mode {
            Http3UpstreamMode::Preferred => Http3RequestError::FallbackToHttp2 {
                negotiated_alpn: session.negotiated_alpn.clone(),
            },
            _ => Http3RequestError::transport(format!(
                "http3 session {} failed to open a request stream: {}",
                session.session_id, observed.message
            )),
        }
    })?;
    let stream_id = request_stream.id().into_inner();
    let stream_ref = session.attach_stream(options.exchange_handle, stream_id);

    let request_body_present = match options.request_body {
        Http3RequestBody::Empty => false,
        Http3RequestBody::Bytes(body) => {
            if !body.is_empty() {
                request_stream.send_data(body).await.map_err(|err| {
                    let observed = classify_http3_error(&err);
                    apply_stream_error(&session, stream_id, &observed);
                    Http3RequestError::transport(format!(
                        "http3 stream {} on session {} failed to send request body: {}",
                        stream_id, session.session_id, observed.message
                    ))
                })?;
                true
            } else {
                false
            }
        }
        Http3RequestBody::Streaming(mut body_stream) => {
            let mut sent_any = false;
            while let Some(chunk) = body_stream.next().await {
                let chunk = chunk.map_err(|err| {
                    let message = err.to_string();
                    session.mark_stream_reset(
                        stream_id,
                        Some(message.clone()),
                        Http3ControlEventSource::Transport,
                    );
                    Http3RequestError::transport(format!(
                        "http3 stream {} on session {} failed to read request body: {}",
                        stream_id, session.session_id, message
                    ))
                })?;
                if chunk.is_empty() {
                    continue;
                }
                request_stream.send_data(chunk).await.map_err(|err| {
                    let observed = classify_http3_error(&err);
                    apply_stream_error(&session, stream_id, &observed);
                    Http3RequestError::transport(format!(
                        "http3 stream {} on session {} failed to send request body: {}",
                        stream_id, session.session_id, observed.message
                    ))
                })?;
                sent_any = true;
            }
            sent_any
        }
    };
    request_stream.finish().await.map_err(|err| {
        let observed = classify_http3_error(&err);
        apply_stream_error(&session, stream_id, &observed);
        Http3RequestError::transport(format!(
            "http3 stream {} on session {} failed to finish request body: {}",
            stream_id, session.session_id, observed.message
        ))
    })?;
    session.mark_stream_request_committed(stream_id, request_body_present);

    let response = request_stream.recv_response().await.map_err(|err| {
        let observed = classify_http3_error(&err);
        apply_stream_error(&session, stream_id, &observed);
        Http3RequestError::transport(format!(
            "http3 stream {} on session {} failed to receive response headers: {}",
            stream_id, session.session_id, observed.message
        ))
    })?;
    session.mark_stream_response_head_ready(stream_id);

    Ok(Http3StartedResponse {
        response,
        peer_addr: Some(session.peer_addr.clone()),
        negotiated_alpn: session.negotiated_alpn.clone(),
        peer_certificate_der: session.peer_certificate_der.clone(),
        stream_ref,
        request_stream,
        body_tracker: Http3ResponseBodyTracker {
            session,
            stream_ref,
        },
    })
}
