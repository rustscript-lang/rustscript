#![cfg_attr(not(feature = "http2"), allow(dead_code))]

use std::collections::HashMap;
#[cfg(feature = "http2")]
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use axum::http::Version;
use url::Url;

use crate::cache::BoundedLruStore;

#[cfg(feature = "http2")]
use crate::abi_impl::transport::{TlsSessionCacheKey, tls_session_cache_key};
use crate::abi_impl::{http1, transport::TlsFlowState};

#[cfg(feature = "http2")]
use axum::{
    body::Bytes,
    http::{HeaderMap, HeaderValue, Method, Request, header::HOST},
};
#[cfg(feature = "http2")]
use http_body_util::Full;
#[cfg(feature = "http2")]
use hyper::{Response, body::Incoming};
#[cfg(feature = "http2")]
use hyper_util::rt::{TokioExecutor, TokioIo};
#[cfg(feature = "http2")]
use std::{
    error::Error,
    io::{self, BufReader},
    sync::atomic::AtomicBool,
};
#[cfg(feature = "http2")]
use tokio::net::TcpStream;
#[cfg(feature = "http2")]
use tokio_rustls::{
    TlsConnector,
    rustls::{
        self, ClientConfig, RootCertStore, SignatureScheme,
        client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        pki_types::{CertificateDer, ServerName, UnixTime},
        version::{TLS12, TLS13},
    },
};

pub(crate) const ALPN_PROTOCOL: &str = "h2";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Http2StreamRef {
    pub(crate) session_id: u64,
    pub(crate) stream_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Http2ControlEventSource {
    RemotePeer,
    LocalRuntime,
    Transport,
}

impl Http2ControlEventSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::RemotePeer => "remote",
            Self::LocalRuntime => "local",
            Self::Transport => "transport",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Http2GoawayState {
    pub(crate) reason: Option<String>,
    pub(crate) source: Http2ControlEventSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Http2ResetState {
    pub(crate) reason: Option<String>,
    pub(crate) source: Http2ControlEventSource,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum Http2UpstreamMode {
    #[default]
    Disabled,
    AutomaticTls,
    PriorKnowledge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Http2SessionGoal {
    Attached,
    Open,
    Draining,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Http2StreamGoal {
    Attached,
    RequestCommitted,
    ResponseHeadReady,
    ResponseBodyReady,
    Closed,
    Reset,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Http2SessionFrontier {
    #[default]
    Candidate,
    Attachable,
    PrefaceExchanged,
    PeerSettingsReceived,
    Open,
    Draining,
    Closed,
    Failed,
}

impl Http2SessionFrontier {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Closed | Self::Failed)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Http2StreamFrontier {
    #[default]
    Reserved,
    AttachedToExchange,
    RequestHeadersSent,
    RequestBodyOpen,
    RequestCommitted,
    ResponseHeadReady,
    ResponseBodyReady,
    HalfClosedLocal,
    HalfClosedRemote,
    Closed,
    Reset,
}

impl Http2StreamFrontier {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Closed | Self::Reset)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Http2UpstreamStreamState {
    stream_id: u64,
    exchange_handle: i64,
    frontier: Http2StreamFrontier,
    reset: Option<Http2ResetState>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Http2DownstreamStreamState {
    pub(crate) stream_id: u64,
    pub(crate) path: String,
    pub(crate) frontier: Http2StreamFrontier,
    pub(crate) reset: Option<Http2ResetState>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Http2UpstreamSessionDagState {
    frontier: Http2SessionFrontier,
    goaway: Option<Http2GoawayState>,
    streams: HashMap<u64, Http2UpstreamStreamState>,
    next_local_stream_id: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct Http2SessionStore {
    #[cfg(feature = "http2")]
    sessions: BoundedLruStore<Http2SessionKey, Arc<Http2UpstreamSession>>,
    #[cfg(feature = "http2")]
    next_session_id: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Http2DownstreamSessionState {
    pub(crate) session_id: u64,
    pub(crate) frontier: Http2SessionFrontier,
    pub(crate) peer_address: String,
    pub(crate) total_streams: u64,
    pub(crate) active_streams: u64,
    pub(crate) last_path: Option<String>,
    pub(crate) last_error: Option<String>,
    pub(crate) goaway: Option<Http2GoawayState>,
    pub(crate) streams: HashMap<u64, Http2DownstreamStreamState>,
    pub(crate) next_stream_id: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct Http2DownstreamSessionStore {
    next_session_id: u64,
    sessions: BoundedLruStore<u64, Http2DownstreamSessionState>,
}

#[cfg(test)]
impl Http2DownstreamSessionStore {
    pub(crate) fn capacity(&self) -> usize {
        self.sessions.capacity()
    }

    pub(crate) fn len(&self) -> usize {
        self.sessions.len()
    }

    pub(crate) fn snapshot_values(&self) -> Vec<Http2DownstreamSessionState> {
        self.sessions.values().cloned().collect()
    }
}

#[cfg(test)]
impl Http2SessionStore {
    pub(crate) fn capacity(&self) -> usize {
        self.sessions.capacity()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Http2DownstreamStreamAttachment {
    pub(crate) session_id: u64,
    pub(crate) stream_id: u64,
}

pub(crate) type SharedHttpUpstreamSessions = Arc<Mutex<Http2SessionStore>>;
pub(crate) type SharedHttpDownstreamSessions = Arc<Mutex<Http2DownstreamSessionStore>>;

pub(crate) fn new_shared_http_upstream_sessions(capacity: usize) -> SharedHttpUpstreamSessions {
    #[cfg(not(feature = "http2"))]
    let _ = capacity;

    Arc::new(Mutex::new(Http2SessionStore {
        #[cfg(feature = "http2")]
        sessions: BoundedLruStore::new(capacity),
        #[cfg(feature = "http2")]
        next_session_id: 0,
    }))
}

pub(crate) fn new_shared_http_downstream_sessions(capacity: usize) -> SharedHttpDownstreamSessions {
    Arc::new(Mutex::new(Http2DownstreamSessionStore {
        next_session_id: 0,
        sessions: BoundedLruStore::new(capacity),
    }))
}

pub(crate) fn supports_response_version(version: Version) -> bool {
    matches!(version, Version::HTTP_2)
}

pub(crate) fn response_version_label() -> &'static str {
    "2"
}

pub(crate) fn select_upstream_mode(target: &str, tls_flow: &TlsFlowState) -> Http2UpstreamMode {
    if !cfg!(feature = "http2") {
        return Http2UpstreamMode::Disabled;
    }

    let desired_alpn = tls_flow.desired_alpn();
    let explicitly_offers_http2 = desired_alpn
        .iter()
        .any(|protocol| protocol.eq_ignore_ascii_case(ALPN_PROTOCOL));
    let explicitly_prefers_http11 = desired_alpn
        .iter()
        .any(|protocol| protocol.eq_ignore_ascii_case(http1::ALPN_PROTOCOL));
    let explicitly_rejects_http2 =
        !desired_alpn.is_empty() && explicitly_prefers_http11 && !explicitly_offers_http2;
    if explicitly_rejects_http2 {
        return Http2UpstreamMode::Disabled;
    }

    let scheme = Url::parse(target)
        .ok()
        .map(|url| url.scheme().to_ascii_lowercase())
        .unwrap_or_else(|| {
            if tls_flow.is_present() {
                "https".to_string()
            } else {
                "http".to_string()
            }
        });
    match scheme.as_str() {
        "https" => Http2UpstreamMode::AutomaticTls,
        "http" if explicitly_offers_http2 => Http2UpstreamMode::PriorKnowledge,
        _ => Http2UpstreamMode::Disabled,
    }
}

pub(crate) fn configure_reqwest_builder(
    builder: reqwest::ClientBuilder,
    mode: Http2UpstreamMode,
) -> reqwest::ClientBuilder {
    #[cfg(not(feature = "http2"))]
    {
        let _ = mode;
        return builder;
    }

    #[cfg(feature = "http2")]
    match mode {
        Http2UpstreamMode::Disabled | Http2UpstreamMode::AutomaticTls => builder,
        Http2UpstreamMode::PriorKnowledge => builder.http2_prior_knowledge(),
    }
}

#[cfg(feature = "http2")]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Http2SessionKey {
    origin: String,
    mode: Http2UpstreamMode,
    tls_key: Option<TlsSessionCacheKey>,
}

#[cfg(feature = "http2")]
#[derive(Debug)]
struct Http2UpstreamSession {
    session_id: u64,
    sender: hyper::client::conn::http2::SendRequest<Full<Bytes>>,
    negotiated_alpn: Option<String>,
    peer_certificate_der: Option<Vec<u8>>,
    dag: Mutex<Http2UpstreamSessionDagState>,
}

#[cfg(feature = "http2")]
#[derive(Clone, Debug)]
pub(crate) struct Http2ResponseBodyTracker {
    session: Arc<Http2UpstreamSession>,
    stream_ref: Http2StreamRef,
}

#[cfg(not(feature = "http2"))]
#[derive(Clone, Debug, Default)]
pub(crate) struct Http2ResponseBodyTracker;

#[cfg(feature = "http2")]
#[derive(Debug)]
pub(crate) struct Http2StartedResponse {
    pub(crate) response: Response<Incoming>,
    pub(crate) negotiated_alpn: Option<String>,
    pub(crate) peer_certificate_der: Option<Vec<u8>>,
    pub(crate) stream_ref: Http2StreamRef,
    pub(crate) body_tracker: Http2ResponseBodyTracker,
}

#[cfg(feature = "http2")]
#[derive(Debug)]
pub(crate) enum Http2RequestError {
    FallbackToHttp1 { negotiated_alpn: Option<String> },
    Transport(String),
}

#[cfg(feature = "http2")]
impl Http2RequestError {
    pub(crate) fn transport(message: impl Into<String>) -> Self {
        Self::Transport(message.into())
    }

    pub(crate) fn into_message(self) -> String {
        match self {
            Self::FallbackToHttp1 {
                negotiated_alpn, ..
            } => format!(
                "http2 request fell back to http/1.1 after negotiating {}",
                negotiated_alpn.as_deref().unwrap_or("no ALPN"),
            ),
            Self::Transport(message) => message,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Http2ObservedError {
    pub(crate) message: String,
    pub(crate) reset: Option<Http2ResetState>,
    pub(crate) goaway: Option<Http2GoawayState>,
}

#[cfg(feature = "http2")]
pub(crate) fn should_use_explicit_upstream_transport(
    mode: Http2UpstreamMode,
    sessions: Option<&SharedHttpUpstreamSessions>,
) -> bool {
    !matches!(mode, Http2UpstreamMode::Disabled) && sessions.is_some()
}

#[cfg(not(feature = "http2"))]
pub(crate) fn should_use_explicit_upstream_transport(
    _mode: Http2UpstreamMode,
    _sessions: Option<&SharedHttpUpstreamSessions>,
) -> bool {
    false
}

#[cfg(feature = "http2")]
impl Http2UpstreamSession {
    fn new(
        session_id: u64,
        sender: hyper::client::conn::http2::SendRequest<Full<Bytes>>,
        negotiated_alpn: Option<String>,
        peer_certificate_der: Option<Vec<u8>>,
    ) -> Self {
        let mut dag = Http2UpstreamSessionDagState {
            frontier: Http2SessionFrontier::Candidate,
            goaway: None,
            streams: HashMap::new(),
            next_local_stream_id: 1,
        };
        dag.advance_session_goal(Http2SessionGoal::Attached);
        dag.frontier = Http2SessionFrontier::PrefaceExchanged;
        dag.frontier = Http2SessionFrontier::PeerSettingsReceived;
        dag.advance_session_goal(Http2SessionGoal::Open);
        Self {
            session_id,
            sender,
            negotiated_alpn,
            peer_certificate_der,
            dag: Mutex::new(dag),
        }
    }

    fn is_reusable(&self) -> bool {
        let dag = self
            .dag
            .lock()
            .expect("http2 upstream session lock poisoned");
        dag.can_accept_new_streams()
    }

    fn should_retain(&self) -> bool {
        let dag = self
            .dag
            .lock()
            .expect("http2 upstream session lock poisoned");
        !dag.frontier.is_terminal() || dag.has_active_streams()
    }

    fn reserve_stream(&self, exchange_handle: i64) -> Result<Http2StreamRef, Http2RequestError> {
        let mut dag = self
            .dag
            .lock()
            .expect("http2 upstream session lock poisoned");
        dag.reserve_stream(self.session_id, exchange_handle)
    }

    fn mark_stream_request_committed(&self, stream_id: u64, body_present: bool) {
        let mut dag = self
            .dag
            .lock()
            .expect("http2 upstream session lock poisoned");
        dag.mark_stream_request_committed(stream_id, body_present);
    }

    fn mark_stream_response_head_ready(&self, stream_id: u64) {
        let mut dag = self
            .dag
            .lock()
            .expect("http2 upstream session lock poisoned");
        dag.advance_stream_goal(stream_id, Http2StreamGoal::ResponseHeadReady);
    }

    fn mark_stream_response_body_ready(&self, stream_id: u64) {
        let mut dag = self
            .dag
            .lock()
            .expect("http2 upstream session lock poisoned");
        dag.advance_stream_goal(stream_id, Http2StreamGoal::ResponseBodyReady);
    }

    fn mark_stream_closed(&self, stream_id: u64) {
        let mut dag = self
            .dag
            .lock()
            .expect("http2 upstream session lock poisoned");
        dag.advance_stream_goal(stream_id, Http2StreamGoal::Closed);
        dag.prune_terminal_streams();
    }

    fn mark_stream_reset(
        &self,
        stream_id: u64,
        reason: Option<String>,
        source: Http2ControlEventSource,
    ) {
        let mut dag = self
            .dag
            .lock()
            .expect("http2 upstream session lock poisoned");
        dag.mark_stream_reset(stream_id, reason, source);
        dag.prune_terminal_streams();
    }

    fn mark_goaway(&self, reason: Option<String>, source: Http2ControlEventSource) {
        let mut dag = self
            .dag
            .lock()
            .expect("http2 upstream session lock poisoned");
        dag.mark_goaway(reason, source);
    }

    fn mark_connection_closed(&self) {
        let mut dag = self
            .dag
            .lock()
            .expect("http2 upstream session lock poisoned");
        dag.mark_connection_closed();
    }

    fn mark_connection_failed(&self, reason: Option<String>) {
        let mut dag = self
            .dag
            .lock()
            .expect("http2 upstream session lock poisoned");
        dag.mark_connection_failed(reason);
    }
}

#[cfg(feature = "http2")]
impl Http2ResponseBodyTracker {
    pub(crate) fn note_response_body_ready(&self) {
        self.session
            .mark_stream_response_body_ready(self.stream_ref.stream_id);
    }

    pub(crate) fn note_body_eof(&self) {
        self.session.mark_stream_closed(self.stream_ref.stream_id);
    }

    pub(crate) fn note_body_error(&self, observed: &Http2ObservedError) {
        if let Some(reset) = &observed.reset {
            self.session.mark_stream_reset(
                self.stream_ref.stream_id,
                reset.reason.clone(),
                reset.source,
            );
        }
        if let Some(goaway) = &observed.goaway {
            self.session
                .mark_goaway(goaway.reason.clone(), goaway.source);
        }
        if observed.reset.is_none() && observed.goaway.is_none() {
            self.session
                .mark_connection_failed(Some(observed.message.clone()));
        }
    }
}

#[cfg(not(feature = "http2"))]
impl Http2ResponseBodyTracker {
    pub(crate) fn note_response_body_ready(&self) {}

    pub(crate) fn note_body_eof(&self) {}

    pub(crate) fn note_body_error(&self, _observed: &Http2ObservedError) {}
}

#[cfg(feature = "http2")]
impl Http2UpstreamSessionDagState {
    fn advance_session_goal(&mut self, goal: Http2SessionGoal) {
        match goal {
            Http2SessionGoal::Attached => {
                if self.frontier == Http2SessionFrontier::Candidate {
                    self.frontier = Http2SessionFrontier::Attachable;
                }
            }
            Http2SessionGoal::Open => {
                if self.frontier == Http2SessionFrontier::Candidate {
                    self.frontier = Http2SessionFrontier::Attachable;
                }
                if self.frontier == Http2SessionFrontier::Attachable {
                    self.frontier = Http2SessionFrontier::PrefaceExchanged;
                }
                if self.frontier == Http2SessionFrontier::PrefaceExchanged {
                    self.frontier = Http2SessionFrontier::PeerSettingsReceived;
                }
                if self.frontier == Http2SessionFrontier::PeerSettingsReceived {
                    self.frontier = Http2SessionFrontier::Open;
                }
            }
            Http2SessionGoal::Draining => {
                if !self.frontier.is_terminal() {
                    self.frontier = Http2SessionFrontier::Draining;
                }
            }
        }
    }

    fn can_accept_new_streams(&self) -> bool {
        self.frontier == Http2SessionFrontier::Open && self.goaway.is_none()
    }

    fn has_active_streams(&self) -> bool {
        self.streams
            .values()
            .any(|stream| !stream.frontier.is_terminal())
    }

    fn prune_terminal_streams(&mut self) {
        self.streams
            .retain(|_, stream| !stream.frontier.is_terminal());
        if self.frontier == Http2SessionFrontier::Draining && !self.has_active_streams() {
            self.frontier = Http2SessionFrontier::Closed;
        }
    }

    fn reserve_stream(
        &mut self,
        session_id: u64,
        exchange_handle: i64,
    ) -> Result<Http2StreamRef, Http2RequestError> {
        if !self.can_accept_new_streams() {
            let message = if let Some(goaway) = &self.goaway {
                format!(
                    "http2 session is draining after GOAWAY from {} ({})",
                    goaway.source.as_str(),
                    goaway.reason.as_deref().unwrap_or("no reason"),
                )
            } else {
                format!(
                    "http2 session is not open for new streams (frontier={:?})",
                    self.frontier
                )
            };
            return Err(Http2RequestError::transport(message));
        }
        self.prune_terminal_streams();
        let stream_id = self.next_local_stream_id;
        self.next_local_stream_id = self.next_local_stream_id.saturating_add(2);
        self.streams.insert(
            stream_id,
            Http2UpstreamStreamState {
                stream_id,
                exchange_handle,
                frontier: Http2StreamFrontier::Reserved,
                reset: None,
            },
        );
        self.advance_stream_goal(stream_id, Http2StreamGoal::Attached);
        Ok(Http2StreamRef {
            session_id,
            stream_id,
        })
    }

    fn advance_stream_goal(&mut self, stream_id: u64, goal: Http2StreamGoal) {
        let Some(stream) = self.streams.get_mut(&stream_id) else {
            return;
        };
        match goal {
            Http2StreamGoal::Attached => {
                if stream.frontier == Http2StreamFrontier::Reserved {
                    stream.frontier = Http2StreamFrontier::AttachedToExchange;
                }
            }
            Http2StreamGoal::RequestCommitted => {
                if stream.frontier == Http2StreamFrontier::Reserved {
                    stream.frontier = Http2StreamFrontier::AttachedToExchange;
                }
                if stream.frontier == Http2StreamFrontier::AttachedToExchange {
                    stream.frontier = Http2StreamFrontier::RequestHeadersSent;
                }
                if stream.frontier == Http2StreamFrontier::RequestHeadersSent {
                    stream.frontier = Http2StreamFrontier::RequestCommitted;
                }
            }
            Http2StreamGoal::ResponseHeadReady => {
                if stream.frontier == Http2StreamFrontier::Reserved {
                    stream.frontier = Http2StreamFrontier::AttachedToExchange;
                }
                if stream.frontier == Http2StreamFrontier::AttachedToExchange {
                    stream.frontier = Http2StreamFrontier::RequestHeadersSent;
                }
                if matches!(
                    stream.frontier,
                    Http2StreamFrontier::RequestHeadersSent
                        | Http2StreamFrontier::RequestCommitted
                        | Http2StreamFrontier::HalfClosedLocal
                ) {
                    stream.frontier = Http2StreamFrontier::ResponseHeadReady;
                }
            }
            Http2StreamGoal::ResponseBodyReady => {
                if stream.frontier == Http2StreamFrontier::Reserved {
                    stream.frontier = Http2StreamFrontier::AttachedToExchange;
                }
                if stream.frontier == Http2StreamFrontier::AttachedToExchange {
                    stream.frontier = Http2StreamFrontier::RequestHeadersSent;
                }
                if matches!(
                    stream.frontier,
                    Http2StreamFrontier::RequestHeadersSent
                        | Http2StreamFrontier::RequestCommitted
                        | Http2StreamFrontier::HalfClosedLocal
                ) {
                    stream.frontier = Http2StreamFrontier::ResponseHeadReady;
                }
                if stream.frontier == Http2StreamFrontier::ResponseHeadReady {
                    stream.frontier = Http2StreamFrontier::ResponseBodyReady;
                }
            }
            Http2StreamGoal::Closed => {
                if matches!(
                    stream.frontier,
                    Http2StreamFrontier::ResponseHeadReady | Http2StreamFrontier::ResponseBodyReady
                ) {
                    stream.frontier = Http2StreamFrontier::HalfClosedRemote;
                }
                stream.frontier = Http2StreamFrontier::Closed;
            }
            Http2StreamGoal::Reset => {
                stream.frontier = Http2StreamFrontier::Reset;
            }
        }
    }

    fn mark_stream_request_committed(&mut self, stream_id: u64, body_present: bool) {
        self.advance_stream_goal(stream_id, Http2StreamGoal::RequestCommitted);
        if let Some(stream) = self.streams.get_mut(&stream_id) {
            if body_present {
                stream.frontier = Http2StreamFrontier::RequestBodyOpen;
            }
            stream.frontier = Http2StreamFrontier::HalfClosedLocal;
        }
    }

    fn mark_stream_reset(
        &mut self,
        stream_id: u64,
        reason: Option<String>,
        source: Http2ControlEventSource,
    ) {
        if let Some(stream) = self.streams.get_mut(&stream_id) {
            stream.reset = Some(Http2ResetState { reason, source });
        }
        self.advance_stream_goal(stream_id, Http2StreamGoal::Reset);
    }

    fn mark_goaway(&mut self, reason: Option<String>, source: Http2ControlEventSource) {
        self.goaway = Some(Http2GoawayState { reason, source });
        self.advance_session_goal(Http2SessionGoal::Draining);
        if !self.has_active_streams() {
            self.frontier = Http2SessionFrontier::Closed;
        }
    }

    fn mark_connection_closed(&mut self) {
        if !self.has_active_streams() {
            self.frontier = Http2SessionFrontier::Closed;
        } else {
            self.mark_connection_failed(Some(
                "http2 connection closed while streams remained active".to_string(),
            ));
        }
    }

    fn mark_connection_failed(&mut self, reason: Option<String>) {
        self.frontier = Http2SessionFrontier::Failed;
        for stream in self.streams.values_mut() {
            if !stream.frontier.is_terminal() {
                stream.reset = Some(Http2ResetState {
                    reason: reason.clone(),
                    source: Http2ControlEventSource::Transport,
                });
                stream.frontier = Http2StreamFrontier::Reset;
            }
        }
    }
}

#[cfg(feature = "http2")]
#[derive(Clone, Debug)]
pub(crate) struct DownstreamHttp2ConnectionTracker {
    store: SharedHttpDownstreamSessions,
    session_id: u64,
    saw_http2: Arc<AtomicBool>,
}

#[cfg(feature = "http2")]
impl DownstreamHttp2ConnectionTracker {
    pub(crate) fn new(store: SharedHttpDownstreamSessions, peer_address: String) -> Self {
        let session_id = {
            let mut guard = store
                .lock()
                .expect("http downstream session store lock poisoned");
            let next = guard.next_session_id.saturating_add(1);
            guard.next_session_id = next;
            guard.sessions.insert(
                next,
                Http2DownstreamSessionState {
                    session_id: next,
                    frontier: Http2SessionFrontier::Candidate,
                    peer_address,
                    total_streams: 0,
                    active_streams: 0,
                    last_path: None,
                    last_error: None,
                    goaway: None,
                    streams: HashMap::new(),
                    next_stream_id: 1,
                },
            );
            next
        };
        Self {
            store,
            session_id,
            saw_http2: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn observe_request(
        &self,
        version: Version,
        path: &str,
    ) -> Option<Http2DownstreamStreamAttachment> {
        if !supports_response_version(version) {
            return None;
        }
        self.saw_http2.store(true, Ordering::Relaxed);
        let mut guard = self
            .store
            .lock()
            .expect("http downstream session store lock poisoned");
        let Some(session) = guard.sessions.get_mut(&self.session_id) else {
            return None;
        };
        if session.frontier == Http2SessionFrontier::Candidate {
            session.frontier = Http2SessionFrontier::Attachable;
        }
        session.frontier = Http2SessionFrontier::Open;
        let stream_id = session.next_stream_id;
        session.next_stream_id = session.next_stream_id.saturating_add(2);
        session.total_streams += 1;
        session.active_streams += 1;
        session.last_path = Some(path.to_string());
        session.streams.insert(
            stream_id,
            Http2DownstreamStreamState {
                stream_id,
                path: path.to_string(),
                frontier: Http2StreamFrontier::RequestCommitted,
                reset: None,
            },
        );
        Some(Http2DownstreamStreamAttachment {
            session_id: self.session_id,
            stream_id,
        })
    }

    pub(crate) fn note_response_head(&self, attachment: Option<&Http2DownstreamStreamAttachment>) {
        let Some(attachment) = attachment else {
            return;
        };
        let mut guard = self
            .store
            .lock()
            .expect("http downstream session store lock poisoned");
        let Some(session) = guard.sessions.get_mut(&attachment.session_id) else {
            return;
        };
        let Some(stream) = session.streams.get_mut(&attachment.stream_id) else {
            return;
        };
        stream.frontier = Http2StreamFrontier::ResponseHeadReady;
    }

    pub(crate) fn finish_request(
        &self,
        attachment: Option<&Http2DownstreamStreamAttachment>,
        error: Option<String>,
    ) {
        let Some(attachment) = attachment else {
            return;
        };
        let mut guard = self
            .store
            .lock()
            .expect("http downstream session store lock poisoned");
        let Some(session) = guard.sessions.get_mut(&attachment.session_id) else {
            return;
        };
        session.active_streams = session.active_streams.saturating_sub(1);
        if let Some(stream) = session.streams.get_mut(&attachment.stream_id) {
            if let Some(message) = error.clone() {
                stream.reset = Some(Http2ResetState {
                    reason: Some(message),
                    source: Http2ControlEventSource::Transport,
                });
                stream.frontier = Http2StreamFrontier::Reset;
            } else {
                if stream.frontier == Http2StreamFrontier::ResponseHeadReady {
                    stream.frontier = Http2StreamFrontier::ResponseBodyReady;
                }
                stream.frontier = Http2StreamFrontier::Closed;
            }
        }
        session
            .streams
            .retain(|_, stream| !stream.frontier.is_terminal());
        if session.frontier == Http2SessionFrontier::Draining && session.active_streams == 0 {
            session.frontier = Http2SessionFrontier::Closed;
        }
    }

    pub(crate) fn finish_connection(&self, error: Option<String>) {
        let mut guard = self
            .store
            .lock()
            .expect("http downstream session store lock poisoned");
        if !self.saw_http2.load(Ordering::Relaxed) {
            let _ = guard.sessions.remove(&self.session_id);
            return;
        }
        let Some(session) = guard.sessions.get_mut(&self.session_id) else {
            return;
        };
        session.active_streams = 0;
        session.last_error = error.clone();
        if let Some(message) = error {
            session.frontier = Http2SessionFrontier::Failed;
            for stream in session.streams.values_mut() {
                if !stream.frontier.is_terminal() {
                    stream.reset = Some(Http2ResetState {
                        reason: Some(message.clone()),
                        source: Http2ControlEventSource::Transport,
                    });
                    stream.frontier = Http2StreamFrontier::Reset;
                }
            }
        } else {
            session.goaway = Some(Http2GoawayState {
                reason: Some("connection closed gracefully".to_string()),
                source: Http2ControlEventSource::LocalRuntime,
            });
            session.frontier = Http2SessionFrontier::Draining;
            session
                .streams
                .retain(|_, stream| !stream.frontier.is_terminal());
            if session.streams.is_empty() {
                session.frontier = Http2SessionFrontier::Closed;
            }
        }
    }
}

#[cfg(not(feature = "http2"))]
#[derive(Clone, Debug)]
pub(crate) struct DownstreamHttp2ConnectionTracker;

#[cfg(not(feature = "http2"))]
impl DownstreamHttp2ConnectionTracker {
    pub(crate) fn new(_store: SharedHttpDownstreamSessions, _peer_address: String) -> Self {
        Self
    }

    pub(crate) fn observe_request(
        &self,
        _version: Version,
        _path: &str,
    ) -> Option<Http2DownstreamStreamAttachment> {
        None
    }

    pub(crate) fn note_response_head(&self, _attachment: Option<&Http2DownstreamStreamAttachment>) {
    }

    pub(crate) fn finish_request(
        &self,
        _attachment: Option<&Http2DownstreamStreamAttachment>,
        _error: Option<String>,
    ) {
    }

    pub(crate) fn finish_connection(&self, _error: Option<String>) {}
}

#[cfg(feature = "http2")]
pub(crate) async fn send_request(
    sessions: &SharedHttpUpstreamSessions,
    exchange_handle: i64,
    target: &str,
    upstream_url: &str,
    mode: Http2UpstreamMode,
    tls_flow: &TlsFlowState,
    method: Method,
    headers: HeaderMap,
    request_body: Vec<u8>,
) -> Result<Http2StartedResponse, Http2RequestError> {
    let session = acquire_or_open_session(sessions, target, mode, tls_flow).await?;
    let mut sender = session.sender.clone();
    sender.ready().await.map_err(|err| {
        let observed = classify_http2_error(&err);
        apply_session_error(&session, &observed);
        Http2RequestError::transport(format!(
            "http2 session {} is not ready for a new stream: {}",
            session.session_id, observed.message,
        ))
    })?;

    let stream_ref = session.reserve_stream(exchange_handle)?;
    let request_body_present = !request_body.is_empty();
    let request = build_http2_request(upstream_url, method, headers, request_body)?;
    let response = sender.send_request(request).await.map_err(|err| {
        let observed = classify_http2_error(&err);
        apply_stream_error(&session, stream_ref.stream_id, &observed);
        Http2RequestError::transport(format!(
            "http2 stream {} on session {} failed: {}",
            stream_ref.stream_id, session.session_id, observed.message,
        ))
    })?;
    session.mark_stream_request_committed(stream_ref.stream_id, request_body_present);
    session.mark_stream_response_head_ready(stream_ref.stream_id);

    Ok(Http2StartedResponse {
        response,
        negotiated_alpn: session.negotiated_alpn.clone(),
        peer_certificate_der: session.peer_certificate_der.clone(),
        stream_ref,
        body_tracker: Http2ResponseBodyTracker {
            session,
            stream_ref,
        },
    })
}

#[cfg(feature = "http2")]
fn apply_session_error(session: &Arc<Http2UpstreamSession>, observed: &Http2ObservedError) {
    if let Some(goaway) = &observed.goaway {
        session.mark_goaway(goaway.reason.clone(), goaway.source);
    }
    if observed.reset.is_none() && observed.goaway.is_none() {
        session.mark_connection_failed(Some(observed.message.clone()));
    }
}

#[cfg(feature = "http2")]
fn apply_stream_error(
    session: &Arc<Http2UpstreamSession>,
    stream_id: u64,
    observed: &Http2ObservedError,
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

#[cfg(feature = "http2")]
pub(crate) fn classify_http2_error<E>(err: &E) -> Http2ObservedError
where
    E: Error + 'static,
{
    let mut current: Option<&(dyn Error + 'static)> = Some(err);
    while let Some(candidate) = current {
        if let Some(h2_err) = candidate.downcast_ref::<h2::Error>() {
            let source = if h2_err.is_remote() {
                Http2ControlEventSource::RemotePeer
            } else {
                Http2ControlEventSource::Transport
            };
            let reason = h2_err.reason().map(|reason| reason.to_string());
            return Http2ObservedError {
                message: h2_err.to_string(),
                reset: h2_err.is_reset().then(|| Http2ResetState {
                    reason: reason.clone(),
                    source,
                }),
                goaway: h2_err
                    .is_go_away()
                    .then(|| Http2GoawayState { reason, source }),
            };
        }
        current = candidate.source();
    }

    Http2ObservedError {
        message: err.to_string(),
        reset: None,
        goaway: None,
    }
}

#[cfg(not(feature = "http2"))]
pub(crate) fn classify_http2_error<E>(err: &E) -> Http2ObservedError
where
    E: std::fmt::Display,
{
    Http2ObservedError {
        message: err.to_string(),
        reset: None,
        goaway: None,
    }
}

#[cfg(feature = "http2")]
fn build_http2_request(
    upstream_url: &str,
    method: Method,
    mut headers: HeaderMap,
    request_body: Vec<u8>,
) -> Result<Request<Full<Bytes>>, Http2RequestError> {
    let url = Url::parse(upstream_url).map_err(|err| {
        Http2RequestError::transport(format!("invalid upstream URL '{upstream_url}': {err}"))
    })?;
    let path_and_query = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };

    if !headers.contains_key(HOST) {
        let host_value = if let Some(port) = url.port() {
            format!("{}:{port}", url.host_str().unwrap_or_default())
        } else {
            url.host_str().unwrap_or_default().to_string()
        };
        let value = HeaderValue::from_str(&host_value).map_err(|err| {
            Http2RequestError::transport(format!("invalid host header for '{upstream_url}': {err}"))
        })?;
        headers.insert(HOST, value);
    }

    let mut request = Request::builder().method(method).uri(path_and_query);
    for (name, value) in &headers {
        request = request.header(name, value);
    }
    request
        .body(Full::new(Bytes::from(request_body)))
        .map_err(|err| Http2RequestError::transport(format!("invalid http2 request: {err}")))
}

#[cfg(feature = "http2")]
async fn acquire_or_open_session(
    sessions: &SharedHttpUpstreamSessions,
    target: &str,
    mode: Http2UpstreamMode,
    tls_flow: &TlsFlowState,
) -> Result<Arc<Http2UpstreamSession>, Http2RequestError> {
    let key = session_key(target, mode, tls_flow).ok_or_else(|| {
        Http2RequestError::transport(format!(
            "http2 session key could not be built for target '{target}'",
        ))
    })?;

    {
        let mut guard = sessions
            .lock()
            .expect("http upstream session store lock poisoned");
        cleanup_closed_sessions(&mut guard);
        if let Some(existing) = guard.sessions.get(&key).cloned() {
            if existing.is_reusable() {
                return Ok(existing);
            }
            let _ = guard.sessions.remove(&key);
        }
    }

    let session_id = {
        let mut guard = sessions
            .lock()
            .expect("http upstream session store lock poisoned");
        let next = guard.next_session_id.saturating_add(1);
        guard.next_session_id = next;
        next
    };
    let opened = open_session(target, mode, tls_flow, session_id).await?;

    let mut guard = sessions
        .lock()
        .expect("http upstream session store lock poisoned");
    cleanup_closed_sessions(&mut guard);
    if let Some(existing) = guard.sessions.get(&key).cloned() {
        if existing.is_reusable() {
            return Ok(existing);
        }
        let _ = guard.sessions.remove(&key);
    }
    guard.sessions.insert(key, opened.clone());
    Ok(opened)
}

#[cfg(feature = "http2")]
fn cleanup_closed_sessions(store: &mut Http2SessionStore) {
    store
        .sessions
        .retain(|_key, session| session.should_retain());
}

#[cfg(feature = "http2")]
fn session_key(
    target: &str,
    mode: Http2UpstreamMode,
    tls_flow: &TlsFlowState,
) -> Option<Http2SessionKey> {
    Some(Http2SessionKey {
        origin: http1::session_origin(target)?,
        mode,
        tls_key: if tls_flow.is_present() {
            tls_session_cache_key(target, tls_flow)
        } else {
            None
        },
    })
}

#[cfg(feature = "http2")]
async fn open_session(
    target: &str,
    mode: Http2UpstreamMode,
    tls_flow: &TlsFlowState,
    session_id: u64,
) -> Result<Arc<Http2UpstreamSession>, Http2RequestError> {
    match mode {
        Http2UpstreamMode::AutomaticTls => {
            let (sender, negotiated_alpn, peer_certificate_der, connection) =
                connect_tls_http2(target, tls_flow, session_id).await?;
            let session = Arc::new(Http2UpstreamSession::new(
                session_id,
                sender,
                negotiated_alpn,
                peer_certificate_der,
            ));
            spawn_http2_connection(connection, session.clone());
            Ok(session)
        }
        Http2UpstreamMode::PriorKnowledge => {
            let (sender, connection) = connect_cleartext_http2(target, session_id).await?;
            let session = Arc::new(Http2UpstreamSession::new(session_id, sender, None, None));
            spawn_http2_connection(connection, session.clone());
            Ok(session)
        }
        Http2UpstreamMode::Disabled => Err(Http2RequestError::transport(
            "http2 session creation was attempted while HTTP/2 was disabled",
        )),
    }
}

#[cfg(feature = "http2")]
async fn connect_cleartext_http2(
    target: &str,
    session_id: u64,
) -> Result<
    (
        hyper::client::conn::http2::SendRequest<Full<Bytes>>,
        hyper::client::conn::http2::Connection<TokioIo<TcpStream>, Full<Bytes>, TokioExecutor>,
    ),
    Http2RequestError,
> {
    let url = Url::parse(target).map_err(|err| {
        Http2RequestError::transport(format!("invalid cleartext http2 target '{target}': {err}"))
    })?;
    let host = url.host_str().ok_or_else(|| {
        Http2RequestError::transport(format!("http2 target '{target}' is missing a host"))
    })?;
    let port = url.port_or_known_default().ok_or_else(|| {
        Http2RequestError::transport(format!("http2 target '{target}' is missing a port"))
    })?;

    let stream = TcpStream::connect((host, port)).await.map_err(|err| {
        Http2RequestError::transport(format!(
            "http2 session {session_id} failed to connect to {host}:{port}: {err}",
        ))
    })?;
    let io = TokioIo::new(stream);
    let (sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake(io)
        .await
        .map_err(|err| {
            Http2RequestError::transport(format!(
                "http2 session {session_id} handshake failed: {err}",
            ))
        })?;
    Ok((sender, connection))
}

#[cfg(feature = "http2")]
async fn connect_tls_http2(
    target: &str,
    tls_flow: &TlsFlowState,
    session_id: u64,
) -> Result<
    (
        hyper::client::conn::http2::SendRequest<Full<Bytes>>,
        Option<String>,
        Option<Vec<u8>>,
        hyper::client::conn::http2::Connection<
            TokioIo<tokio_rustls::client::TlsStream<TcpStream>>,
            Full<Bytes>,
            TokioExecutor,
        >,
    ),
    Http2RequestError,
> {
    let url = Url::parse(target).map_err(|err| {
        Http2RequestError::transport(format!("invalid tls http2 target '{target}': {err}"))
    })?;
    let host = url.host_str().ok_or_else(|| {
        Http2RequestError::transport(format!("http2 target '{target}' is missing a host"))
    })?;
    let port = url.port_or_known_default().ok_or_else(|| {
        Http2RequestError::transport(format!("http2 target '{target}' is missing a port"))
    })?;
    let stream = TcpStream::connect((host, port)).await.map_err(|err| {
        Http2RequestError::transport(format!(
            "http2 session {session_id} failed to connect to {host}:{port}: {err}",
        ))
    })?;

    let server_name = if !tls_flow.server_name().is_empty() {
        tls_flow.server_name().to_string()
    } else {
        host.to_string()
    };
    let tls_config = build_tls_client_config(tls_flow)?;
    let connector = TlsConnector::from(Arc::new(tls_config));
    let server_name = ServerName::try_from(server_name.clone()).map_err(|err| {
        Http2RequestError::transport(format!(
            "http2 session {session_id} has invalid TLS server name '{server_name}': {err}",
        ))
    })?;
    let tls_stream = connector
        .connect(server_name, stream)
        .await
        .map_err(|err| {
            Http2RequestError::transport(format!(
                "http2 session {session_id} TLS handshake failed: {err}",
            ))
        })?;
    let negotiated_alpn = tls_stream
        .get_ref()
        .1
        .alpn_protocol()
        .map(|protocol| String::from_utf8_lossy(protocol).to_string());
    let peer_certificate_der = tls_stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certificates| certificates.first().map(|certificate| certificate.to_vec()));

    if negotiated_alpn.as_deref() != Some(ALPN_PROTOCOL) {
        if can_fallback_to_http11(tls_flow) {
            return Err(Http2RequestError::FallbackToHttp1 { negotiated_alpn });
        }
        return Err(Http2RequestError::transport(format!(
            "http2 session {session_id} negotiated {} instead of {ALPN_PROTOCOL}",
            negotiated_alpn.as_deref().unwrap_or("no ALPN"),
        )));
    }

    let io = TokioIo::new(tls_stream);
    let (sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake(io)
        .await
        .map_err(|err| {
            Http2RequestError::transport(format!(
                "http2 session {session_id} handshake failed after TLS setup: {err}",
            ))
        })?;
    Ok((sender, negotiated_alpn, peer_certificate_der, connection))
}

#[cfg(feature = "http2")]
fn spawn_http2_connection<T>(
    connection: hyper::client::conn::http2::Connection<T, Full<Bytes>, TokioExecutor>,
    session: Arc<Http2UpstreamSession>,
) where
    T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        match connection.await {
            Ok(()) => session.mark_connection_closed(),
            Err(err) => {
                let observed = classify_http2_error(&err);
                if let Some(goaway) = observed.goaway {
                    session.mark_goaway(goaway.reason, goaway.source);
                    session.mark_connection_closed();
                    return;
                }
                session.mark_connection_failed(Some(observed.message));
            }
        }
    });
}

#[cfg(feature = "http2")]
fn can_fallback_to_http11(tls_flow: &TlsFlowState) -> bool {
    let desired = tls_flow.desired_alpn();
    desired.is_empty()
        || desired
            .iter()
            .any(|protocol| protocol.eq_ignore_ascii_case(http1::ALPN_PROTOCOL))
}

#[cfg(feature = "http2")]
fn build_tls_client_config(tls_flow: &TlsFlowState) -> Result<ClientConfig, Http2RequestError> {
    ensure_rustls_provider();

    let versions = protocol_versions_for_http2(tls_flow)?;
    let builder =
        ClientConfig::builder_with_provider(rustls::crypto::aws_lc_rs::default_provider().into())
            .with_protocol_versions(&versions)
            .map_err(|err| {
                Http2RequestError::transport(format!(
                    "failed to configure TLS versions for http2 session: {err}",
                ))
            })?;

    let mut config = if tls_flow.verify_peer() && tls_flow.verify_hostname() {
        match (
            tls_flow.client_certificate_pem(),
            tls_flow.client_private_key_pem(),
        ) {
            (Some(_), Some(_)) => builder
                .with_root_certificates(build_root_store(tls_flow)?)
                .with_client_auth_cert(
                    load_client_cert_chain(tls_flow.client_certificate_pem())?,
                    load_client_private_key(tls_flow.client_private_key_pem())?,
                )
                .map_err(|err| {
                    Http2RequestError::transport(format!(
                        "failed to configure client certificate for http2 session: {err}",
                    ))
                })?,
            (Some(_), None) | (None, Some(_)) => {
                return Err(Http2RequestError::transport(
                    "client certificate and private key must both be configured",
                ));
            }
            (None, None) => builder
                .with_root_certificates(build_root_store(tls_flow)?)
                .with_no_client_auth(),
        }
    } else {
        let verifier = Arc::new(PermissiveServerCertVerifier::new(build_root_store(
            tls_flow,
        )?));
        let dangerous = builder
            .dangerous()
            .with_custom_certificate_verifier(verifier);
        match (
            tls_flow.client_certificate_pem(),
            tls_flow.client_private_key_pem(),
        ) {
            (Some(_), Some(_)) => dangerous
                .with_client_auth_cert(
                    load_client_cert_chain(tls_flow.client_certificate_pem())?,
                    load_client_private_key(tls_flow.client_private_key_pem())?,
                )
                .map_err(|err| {
                    Http2RequestError::transport(format!(
                        "failed to configure client certificate for http2 session: {err}",
                    ))
                })?,
            (Some(_), None) | (None, Some(_)) => {
                return Err(Http2RequestError::transport(
                    "client certificate and private key must both be configured",
                ));
            }
            (None, None) => dangerous.with_no_client_auth(),
        }
    };

    config.enable_sni = tls_flow.sni_enabled();
    config.alpn_protocols = alpn_protocols_for_mode(tls_flow);
    Ok(config)
}

#[cfg(feature = "http2")]
fn build_root_store(tls_flow: &TlsFlowState) -> Result<RootCertStore, Http2RequestError> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(bundle) = tls_flow.trusted_certificate_pem() {
        let mut reader = BufReader::new(bundle.as_bytes());
        for certificate in rustls_pemfile::certs(&mut reader) {
            let certificate = certificate.map_err(|err| {
                Http2RequestError::transport(format!(
                    "failed to parse trusted certificate bundle: {err}",
                ))
            })?;
            roots.add(certificate).map_err(|err| {
                Http2RequestError::transport(format!(
                    "failed to add trusted certificate to http2 root store: {err}",
                ))
            })?;
        }
    }
    Ok(roots)
}

#[cfg(feature = "http2")]
fn load_client_cert_chain(
    certificate_pem: Option<&str>,
) -> Result<Vec<CertificateDer<'static>>, Http2RequestError> {
    let Some(certificate_pem) = certificate_pem else {
        return Ok(Vec::new());
    };
    let mut reader = BufReader::new(certificate_pem.as_bytes());
    let chain = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, io::Error>>()
        .map_err(|err| {
            Http2RequestError::transport(
                format!("failed to parse client certificate chain: {err}",),
            )
        })?;
    if chain.is_empty() {
        return Err(Http2RequestError::transport(
            "client certificate chain is empty",
        ));
    }
    Ok(chain)
}

#[cfg(feature = "http2")]
fn load_client_private_key(
    private_key_pem: Option<&str>,
) -> Result<rustls::pki_types::PrivateKeyDer<'static>, Http2RequestError> {
    let Some(private_key_pem) = private_key_pem else {
        return Err(Http2RequestError::transport(
            "client private key is unavailable",
        ));
    };
    let mut reader = BufReader::new(private_key_pem.as_bytes());
    rustls_pemfile::private_key(&mut reader)
        .map_err(|err| {
            Http2RequestError::transport(format!("failed to parse client private key: {err}"))
        })?
        .ok_or_else(|| Http2RequestError::transport("client private key is unavailable"))
}

#[cfg(feature = "http2")]
fn protocol_versions_for_http2(
    tls_flow: &TlsFlowState,
) -> Result<Vec<&'static rustls::SupportedProtocolVersion>, Http2RequestError> {
    use crate::abi_impl::transport::TlsProtocolVersion;

    if matches!(
        tls_flow.min_version(),
        Some(TlsProtocolVersion::Tls1_0) | Some(TlsProtocolVersion::Tls1_1)
    ) || matches!(
        tls_flow.max_version(),
        Some(TlsProtocolVersion::Tls1_0) | Some(TlsProtocolVersion::Tls1_1)
    ) {
        return Err(Http2RequestError::transport(
            "http2 over TLS requires TLS 1.2 or newer",
        ));
    }

    let min = tls_flow.min_version().unwrap_or(TlsProtocolVersion::Tls1_2);
    let max = tls_flow.max_version().unwrap_or(TlsProtocolVersion::Tls1_3);
    if min > max {
        return Err(Http2RequestError::transport(
            "http2 TLS min version cannot be greater than max version",
        ));
    }

    let mut versions = Vec::new();
    if min <= TlsProtocolVersion::Tls1_2 && max >= TlsProtocolVersion::Tls1_2 {
        versions.push(&TLS12);
    }
    if min <= TlsProtocolVersion::Tls1_3 && max >= TlsProtocolVersion::Tls1_3 {
        versions.push(&TLS13);
    }
    if versions.is_empty() {
        return Err(Http2RequestError::transport(
            "http2 TLS version constraints left no supported protocol versions",
        ));
    }
    Ok(versions)
}

#[cfg(feature = "http2")]
fn alpn_protocols_for_mode(tls_flow: &TlsFlowState) -> Vec<Vec<u8>> {
    if !tls_flow.desired_alpn().is_empty() {
        return tls_flow
            .desired_alpn()
            .iter()
            .map(|protocol| protocol.as_bytes().to_vec())
            .collect();
    }
    vec![
        ALPN_PROTOCOL.as_bytes().to_vec(),
        http1::ALPN_PROTOCOL.as_bytes().to_vec(),
    ]
}

#[cfg(feature = "http2")]
fn ensure_rustls_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

#[cfg(feature = "http2")]
struct PermissiveServerCertVerifier {
    delegate: Arc<dyn ServerCertVerifier>,
}

#[cfg(feature = "http2")]
impl std::fmt::Debug for PermissiveServerCertVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PermissiveServerCertVerifier")
    }
}

#[cfg(feature = "http2")]
impl PermissiveServerCertVerifier {
    fn new(roots: RootCertStore) -> Self {
        let delegate = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
            .build()
            .expect("webpki verifier should build");
        Self { delegate }
    }
}

#[cfg(feature = "http2")]
impl ServerCertVerifier for PermissiveServerCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.delegate.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.delegate.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.delegate.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::abi_impl::transport::TlsFlowState;

    use super::{
        ALPN_PROTOCOL, Http2ControlEventSource, Http2GoawayState, Http2SessionFrontier,
        Http2SessionGoal, Http2StreamFrontier, Http2StreamGoal, Http2UpstreamMode,
        Http2UpstreamSessionDagState, select_upstream_mode,
    };

    fn open_session_dag() -> Http2UpstreamSessionDagState {
        let mut dag = Http2UpstreamSessionDagState {
            frontier: Http2SessionFrontier::Candidate,
            goaway: None,
            streams: HashMap::new(),
            next_local_stream_id: 1,
        };
        dag.advance_session_goal(Http2SessionGoal::Open);
        dag
    }

    #[test]
    fn https_targets_select_automatic_http2_when_enabled() {
        let mode = select_upstream_mode("https://example.com/data", &TlsFlowState::default());
        if cfg!(feature = "http2") {
            assert_eq!(mode, Http2UpstreamMode::AutomaticTls);
        } else {
            assert_eq!(mode, Http2UpstreamMode::Disabled);
        }
    }

    #[test]
    fn cleartext_prior_knowledge_requires_explicit_h2_preference() {
        let mut flow = TlsFlowState::default();
        flow.set_desired_alpn(vec![ALPN_PROTOCOL.to_string()]);
        let mode = select_upstream_mode("http://example.com/data", &flow);
        if cfg!(feature = "http2") {
            assert_eq!(mode, Http2UpstreamMode::PriorKnowledge);
        } else {
            assert_eq!(mode, Http2UpstreamMode::Disabled);
        }
    }

    #[test]
    fn goaway_moves_session_to_draining_and_blocks_new_streams() {
        let mut dag = open_session_dag();
        let first = dag
            .reserve_stream(7, 1)
            .expect("first stream should reserve on open session");
        let second = dag
            .reserve_stream(7, 2)
            .expect("second stream should reserve on open session");

        dag.mark_goaway(
            Some("NO_ERROR".to_string()),
            Http2ControlEventSource::RemotePeer,
        );

        assert_eq!(dag.frontier, Http2SessionFrontier::Draining);
        assert_eq!(
            dag.goaway,
            Some(Http2GoawayState {
                reason: Some("NO_ERROR".to_string()),
                source: Http2ControlEventSource::RemotePeer,
            })
        );
        assert!(
            dag.reserve_stream(7, 3).is_err(),
            "GOAWAY must block new stream attachment"
        );

        dag.advance_stream_goal(first.stream_id, Http2StreamGoal::Closed);
        dag.advance_stream_goal(second.stream_id, Http2StreamGoal::Closed);
        dag.prune_terminal_streams();
        assert_eq!(dag.frontier, Http2SessionFrontier::Closed);
    }

    #[test]
    fn stream_reset_isolated_to_one_stream_frontier() {
        let mut dag = open_session_dag();
        let first = dag
            .reserve_stream(9, 11)
            .expect("first stream should reserve");
        let second = dag
            .reserve_stream(9, 12)
            .expect("second stream should reserve");

        dag.mark_stream_request_committed(first.stream_id, true);
        dag.mark_stream_request_committed(second.stream_id, true);
        dag.mark_stream_reset(
            first.stream_id,
            Some("CANCEL".to_string()),
            Http2ControlEventSource::RemotePeer,
        );

        assert_eq!(dag.frontier, Http2SessionFrontier::Open);
        assert_eq!(
            dag.streams
                .get(&first.stream_id)
                .map(|stream| stream.frontier),
            Some(Http2StreamFrontier::Reset)
        );
        assert_eq!(
            dag.streams
                .get(&second.stream_id)
                .map(|stream| stream.frontier),
            Some(Http2StreamFrontier::HalfClosedLocal)
        );
    }

    #[test]
    fn response_body_goal_advances_from_attached_stream() {
        let mut dag = open_session_dag();
        let stream = dag
            .reserve_stream(3, 99)
            .expect("stream should reserve on open session");

        dag.advance_stream_goal(stream.stream_id, Http2StreamGoal::ResponseBodyReady);

        assert_eq!(
            dag.streams
                .get(&stream.stream_id)
                .map(|state| state.frontier),
            Some(Http2StreamFrontier::ResponseBodyReady)
        );
    }
}
