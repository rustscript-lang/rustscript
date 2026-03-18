#![cfg_attr(not(feature = "http2"), allow(dead_code))]

#[cfg(feature = "http2")]
use std::collections::HashMap;
#[cfg(not(feature = "http2"))]
use std::sync::{Arc, Mutex};
#[cfg(feature = "http2")]
use std::sync::{
    Arc, Mutex, RwLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use crate::abi_impl::http::state::HttpUpstreamScheme;
#[cfg(feature = "http2")]
use crate::abi_impl::transport::{
    HTTP11_ALPN_PROTOCOL, TlsFlowState, TlsProtocolVersion, TlsSessionCacheKey,
    tls_session_cache_key,
};
#[cfg(feature = "http2")]
use crate::cache::ShardedRwLruStore;
#[cfg(feature = "http2")]
use crate::lock_metrics::{self, LockMetricKey, ProfiledMutexGuard};
#[cfg(feature = "http2")]
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, Method, Request, header::HOST},
};
#[cfg(feature = "http2")]
use hyper::{Response, body::Incoming};
#[cfg(feature = "http2")]
use hyper_util::rt::{TokioExecutor, TokioIo};
#[cfg(feature = "http2")]
use std::{error::Error, io::BufReader};
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

use super::Http2UpstreamMode;
#[cfg(feature = "http2")]
use super::model::{
    ALPN_PROTOCOL, Http2ControlEventSource, Http2SessionFrontier, Http2SessionGoal,
    Http2StreamFrontier, Http2StreamGoal, Http2StreamRef,
};
use super::model::{Http2GoawayState, Http2ResetState};

#[derive(Debug)]
pub(crate) struct Http2SessionStore {
    #[cfg(feature = "http2")]
    sessions: ShardedRwLruStore<Http2SessionKey, Arc<Http2SessionEntry>>,
    #[cfg(feature = "http2")]
    next_session_id: AtomicU64,
}

#[cfg(test)]
impl Http2SessionStore {
    pub(crate) fn capacity(&self) -> usize {
        #[cfg(feature = "http2")]
        {
            self.sessions.capacity()
        }

        #[cfg(not(feature = "http2"))]
        {
            0
        }
    }
}

pub(crate) type SharedHttpUpstreamSessions = Arc<Http2SessionStore>;

#[cfg(feature = "http2")]
struct Http2RequestParts<'a> {
    request_path: &'a str,
    request_query: &'a str,
    target_host: &'a str,
    target_port: u16,
    target_host_header: Option<&'a str>,
    method: Method,
    headers: HeaderMap,
    request_body: Body,
}

pub(crate) fn new_shared_http_upstream_sessions(capacity: usize) -> SharedHttpUpstreamSessions {
    #[cfg(not(feature = "http2"))]
    let _ = capacity;

    Arc::new(Http2SessionStore {
        #[cfg(feature = "http2")]
        sessions: ShardedRwLruStore::new(capacity),
        #[cfg(feature = "http2")]
        next_session_id: AtomicU64::new(0),
    })
}

#[cfg(all(test, feature = "http2"))]
pub(crate) fn total_active_streams(sessions: &SharedHttpUpstreamSessions) -> usize {
    sessions
        .sessions
        .values_cloned()
        .into_iter()
        .map(|entry| entry.active_stream_count())
        .sum()
}

#[cfg(feature = "http2")]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Http2SessionKey {
    origin: String,
    mode: Http2UpstreamMode,
    tls_key: Option<TlsSessionCacheKey>,
}

#[cfg(feature = "http2")]
#[derive(Clone, Debug, PartialEq, Eq)]
struct Http2UpstreamStreamState {
    stream_id: u64,
    exchange_handle: i64,
    frontier: Http2StreamFrontier,
    reset: Option<Http2ResetState>,
}

#[cfg(feature = "http2")]
#[derive(Debug)]
struct Http2SessionEntry {
    current: RwLock<Option<Arc<Http2UpstreamSession>>>,
    opening: AtomicBool,
    opened: tokio::sync::Notify,
}

#[cfg(feature = "http2")]
impl Http2SessionEntry {
    fn new() -> Self {
        Self {
            current: RwLock::new(None),
            opening: AtomicBool::new(false),
            opened: tokio::sync::Notify::new(),
        }
    }

    fn reusable_session(&self) -> Option<Arc<Http2UpstreamSession>> {
        let session = self
            .current
            .read()
            .expect("http2 session entry lock poisoned")
            .clone()?;
        if session.is_reusable() {
            return Some(session);
        }

        let mut current = self
            .current
            .write()
            .expect("http2 session entry lock poisoned");
        if current
            .as_ref()
            .is_some_and(|existing| Arc::ptr_eq(existing, &session) && !existing.is_reusable())
        {
            *current = None;
        }
        None
    }

    fn try_begin_open(&self) -> bool {
        self.opening
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn finish_open(&self, session: Option<Arc<Http2UpstreamSession>>) {
        *self
            .current
            .write()
            .expect("http2 session entry lock poisoned") = session;
        self.opening.store(false, Ordering::Release);
        self.opened.notify_waiters();
    }

    async fn wait_for_open(&self) {
        self.opened.notified().await;
    }

    #[cfg(test)]
    fn active_stream_count(&self) -> usize {
        self.current
            .read()
            .expect("http2 session entry lock poisoned")
            .as_ref()
            .map_or(0, |session| session.active_stream_count())
    }
}

#[cfg(feature = "http2")]
#[derive(Clone, Debug, PartialEq, Eq)]
struct Http2UpstreamSessionDagState {
    frontier: Http2SessionFrontier,
    goaway: Option<Http2GoawayState>,
    streams: HashMap<u64, Http2UpstreamStreamState>,
    next_local_stream_id: u64,
}

#[cfg(feature = "http2")]
#[derive(Debug)]
struct Http2UpstreamSession {
    session_id: u64,
    sender: hyper::client::conn::http2::SendRequest<Body>,
    peer_addr: String,
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
    pub(crate) peer_addr: Option<String>,
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
    fn lock_dag(&self) -> ProfiledMutexGuard<'_, Http2UpstreamSessionDagState> {
        lock_metrics::lock(
            &self.dag,
            LockMetricKey::Http2UpstreamSessionDag,
            "http2 upstream session lock poisoned",
        )
    }

    fn new(
        session_id: u64,
        sender: hyper::client::conn::http2::SendRequest<Body>,
        peer_addr: String,
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

    #[cfg(test)]
    fn active_stream_count(&self) -> usize {
        let dag = self.lock_dag();
        dag.streams
            .values()
            .filter(|stream| !stream.frontier.is_terminal())
            .count()
    }

    fn reserve_stream(&self, exchange_handle: i64) -> Result<Http2StreamRef, Http2RequestError> {
        let mut dag = self.lock_dag();
        dag.reserve_stream(self.session_id, exchange_handle)
    }

    fn mark_stream_request_committed(&self, stream_id: u64, body_present: bool) {
        let mut dag = self.lock_dag();
        dag.mark_stream_request_committed(stream_id, body_present);
    }

    fn mark_stream_response_head_ready(&self, stream_id: u64) {
        let mut dag = self.lock_dag();
        dag.advance_stream_goal(stream_id, Http2StreamGoal::ResponseHeadReady);
    }

    fn mark_stream_response_body_ready(&self, stream_id: u64) {
        let mut dag = self.lock_dag();
        dag.advance_stream_goal(stream_id, Http2StreamGoal::ResponseBodyReady);
    }

    fn mark_stream_closed(&self, stream_id: u64) {
        let mut dag = self.lock_dag();
        dag.advance_stream_goal(stream_id, Http2StreamGoal::Closed);
        dag.prune_terminal_streams();
    }

    fn mark_stream_reset(
        &self,
        stream_id: u64,
        reason: Option<String>,
        source: Http2ControlEventSource,
    ) {
        let mut dag = self.lock_dag();
        dag.mark_stream_reset(stream_id, reason, source);
        dag.prune_terminal_streams();
    }

    fn mark_goaway(&self, reason: Option<String>, source: Http2ControlEventSource) {
        let mut dag = self.lock_dag();
        dag.mark_goaway(reason, source);
    }

    fn mark_connection_closed(&self) {
        let mut dag = self.lock_dag();
        dag.mark_connection_closed();
    }

    fn mark_connection_failed(&self, reason: Option<String>) {
        let mut dag = self.lock_dag();
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
pub(crate) struct Http2SendRequest<'a> {
    pub(crate) sessions: &'a SharedHttpUpstreamSessions,
    pub(crate) exchange_handle: i64,
    pub(crate) target_scheme: HttpUpstreamScheme,
    pub(crate) target_host: &'a str,
    pub(crate) target_port: u16,
    pub(crate) target_host_header: Option<&'a str>,
    pub(crate) request_path: &'a str,
    pub(crate) request_query: &'a str,
    pub(crate) mode: Http2UpstreamMode,
    pub(crate) tls_flow: &'a TlsFlowState,
    pub(crate) method: Method,
    pub(crate) headers: HeaderMap,
    pub(crate) request_body: Body,
    pub(crate) request_body_present: bool,
}

#[cfg(feature = "http2")]
pub(crate) async fn send_request(
    request: Http2SendRequest<'_>,
) -> Result<Http2StartedResponse, Http2RequestError> {
    let Http2SendRequest {
        sessions,
        exchange_handle,
        target_scheme,
        target_host,
        target_port,
        target_host_header,
        request_path,
        request_query,
        mode,
        tls_flow,
        method,
        headers,
        request_body,
        request_body_present,
    } = request;
    let session = acquire_or_open_session(
        sessions,
        target_scheme,
        target_host,
        target_port,
        mode,
        tls_flow,
    )
    .await?;
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
    let request = build_http2_request(Http2RequestParts {
        request_path,
        request_query,
        target_host,
        target_port,
        target_host_header,
        method,
        headers,
        request_body,
    })?;
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
        peer_addr: Some(session.peer_addr.clone()),
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
                    .then_some(Http2GoawayState { reason, source }),
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
fn build_http2_request(parts: Http2RequestParts<'_>) -> Result<Request<Body>, Http2RequestError> {
    let path = if parts.request_path.is_empty() {
        "/"
    } else {
        parts.request_path
    };
    let path_and_query = if parts.request_query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", parts.request_query)
    };
    let mut headers = parts.headers;

    if !headers.contains_key(HOST) {
        let host_value = parts
            .target_host_header
            .map(str::to_string)
            .unwrap_or_else(|| format_upstream_authority(parts.target_host, parts.target_port));
        let value = HeaderValue::from_str(&host_value).map_err(|err| {
            Http2RequestError::transport(format!(
                "invalid host header for '{}://{}': {err}",
                "http", host_value
            ))
        })?;
        headers.insert(HOST, value);
    }

    let mut request = Request::builder().method(parts.method).uri(path_and_query);
    for (name, value) in &headers {
        request = request.header(name, value);
    }
    request
        .body(parts.request_body)
        .map_err(|err| Http2RequestError::transport(format!("invalid http2 request: {err}")))
}

#[cfg(feature = "http2")]
async fn acquire_or_open_session(
    sessions: &SharedHttpUpstreamSessions,
    target_scheme: HttpUpstreamScheme,
    target_host: &str,
    target_port: u16,
    mode: Http2UpstreamMode,
    tls_flow: &TlsFlowState,
) -> Result<Arc<Http2UpstreamSession>, Http2RequestError> {
    let key =
        session_key(target_scheme, target_host, target_port, mode, tls_flow).ok_or_else(|| {
            Http2RequestError::transport(format!(
                "http2 session key could not be built for target {target_host}:{target_port}",
            ))
        })?;

    let entry = sessions.sessions.get_or_insert_with_cloned(
        key,
        LockMetricKey::Http2UpstreamSessionStore,
        "http upstream session store lock poisoned",
        || Arc::new(Http2SessionEntry::new()),
    );

    loop {
        if let Some(existing) = entry.reusable_session() {
            return Ok(existing);
        }

        if entry.try_begin_open() {
            let session_id = sessions.next_session_id.fetch_add(1, Ordering::Relaxed) + 1;
            let opened = open_session(
                target_scheme,
                target_host,
                target_port,
                mode,
                tls_flow,
                session_id,
            )
            .await;
            match opened {
                Ok(session) => {
                    entry.finish_open(Some(session.clone()));
                    return Ok(session);
                }
                Err(err) => {
                    entry.finish_open(None);
                    return Err(err);
                }
            }
        }

        entry.wait_for_open().await;
    }
}

#[cfg(feature = "http2")]
fn session_key(
    target_scheme: HttpUpstreamScheme,
    target_host: &str,
    target_port: u16,
    mode: Http2UpstreamMode,
    tls_flow: &TlsFlowState,
) -> Option<Http2SessionKey> {
    Some(Http2SessionKey {
        origin: session_origin(target_scheme, target_host, target_port)?,
        mode,
        tls_key: if tls_flow.is_present() {
            tls_session_cache_key(target_scheme.as_str(), target_host, target_port, tls_flow)
        } else {
            None
        },
    })
}

#[cfg(feature = "http2")]
async fn open_session(
    _target_scheme: HttpUpstreamScheme,
    target_host: &str,
    target_port: u16,
    mode: Http2UpstreamMode,
    tls_flow: &TlsFlowState,
    session_id: u64,
) -> Result<Arc<Http2UpstreamSession>, Http2RequestError> {
    match mode {
        Http2UpstreamMode::AutomaticTls => {
            let (sender, negotiated_alpn, peer_certificate_der, connection, peer_addr) =
                connect_tls_http2(target_host, target_port, tls_flow, session_id).await?;
            let session = Arc::new(Http2UpstreamSession::new(
                session_id,
                sender,
                peer_addr,
                negotiated_alpn,
                peer_certificate_der,
            ));
            spawn_http2_connection(connection, session.clone());
            Ok(session)
        }
        Http2UpstreamMode::PriorKnowledge => {
            let (sender, peer_addr, connection) =
                connect_cleartext_http2(target_host, target_port, session_id).await?;
            let session = Arc::new(Http2UpstreamSession::new(
                session_id, sender, peer_addr, None, None,
            ));
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
    host: &str,
    port: u16,
    session_id: u64,
) -> Result<
    (
        hyper::client::conn::http2::SendRequest<Body>,
        String,
        hyper::client::conn::http2::Connection<TokioIo<TcpStream>, Body, TokioExecutor>,
    ),
    Http2RequestError,
> {
    let stream = TcpStream::connect((host, port)).await.map_err(|err| {
        Http2RequestError::transport(format!(
            "http2 session {session_id} failed to connect to {host}:{port}: {err}",
        ))
    })?;
    let peer_addr = stream
        .peer_addr()
        .map(|addr| addr.to_string())
        .map_err(|err| {
            Http2RequestError::transport(format!(
                "http2 session {session_id} failed to read peer addr: {err}",
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
    Ok((sender, peer_addr, connection))
}

#[cfg(feature = "http2")]
async fn connect_tls_http2(
    host: &str,
    port: u16,
    tls_flow: &TlsFlowState,
    session_id: u64,
) -> Result<
    (
        hyper::client::conn::http2::SendRequest<Body>,
        Option<String>,
        Option<Vec<u8>>,
        hyper::client::conn::http2::Connection<
            TokioIo<tokio_rustls::client::TlsStream<TcpStream>>,
            Body,
            TokioExecutor,
        >,
        String,
    ),
    Http2RequestError,
> {
    let stream = TcpStream::connect((host, port)).await.map_err(|err| {
        Http2RequestError::transport(format!(
            "http2 session {session_id} failed to connect to {host}:{port}: {err}",
        ))
    })?;
    let peer_addr = stream
        .peer_addr()
        .map(|addr| addr.to_string())
        .map_err(|err| {
            Http2RequestError::transport(format!(
                "http2 session {session_id} failed to read peer addr: {err}",
            ))
        })?;

    let peer_name = if !tls_flow.peer_name().is_empty() {
        tls_flow.peer_name().to_string()
    } else {
        host.to_string()
    };
    let tls_config = build_tls_client_config(tls_flow)?;
    let connector = TlsConnector::from(Arc::new(tls_config));
    let server_name = ServerName::try_from(peer_name.clone()).map_err(|err| {
        Http2RequestError::transport(format!(
            "http2 session {session_id} has invalid TLS peer name '{peer_name}': {err}",
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
    Ok((
        sender,
        negotiated_alpn,
        peer_certificate_der,
        connection,
        peer_addr,
    ))
}

#[cfg(feature = "http2")]
fn spawn_http2_connection<T>(
    connection: hyper::client::conn::http2::Connection<T, Body, TokioExecutor>,
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
            .any(|protocol| protocol.eq_ignore_ascii_case(HTTP11_ALPN_PROTOCOL))
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
                        "failed to configure mTLS client auth for http2 session: {err}",
                    ))
                })?,
            (None, None) => builder
                .with_root_certificates(build_root_store(tls_flow)?)
                .with_no_client_auth(),
            _ => {
                return Err(Http2RequestError::transport(
                    "http2 client certificate and private key must both be set",
                ));
            }
        }
    } else {
        let roots = build_root_store(tls_flow)?;
        match (
            tls_flow.client_certificate_pem(),
            tls_flow.client_private_key_pem(),
        ) {
            (Some(_), Some(_)) => builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(PermissiveServerCertVerifier::new(
                    roots,
                )))
                .with_client_auth_cert(
                    load_client_cert_chain(tls_flow.client_certificate_pem())?,
                    load_client_private_key(tls_flow.client_private_key_pem())?,
                )
                .map_err(|err| {
                    Http2RequestError::transport(format!(
                        "failed to configure permissive mTLS client auth for http2 session: {err}",
                    ))
                })?,
            (None, None) => builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(PermissiveServerCertVerifier::new(
                    roots,
                )))
                .with_no_client_auth(),
            _ => {
                return Err(Http2RequestError::transport(
                    "http2 client certificate and private key must both be set",
                ));
            }
        }
    };

    config.alpn_protocols = alpn_protocols_for_mode(tls_flow);
    Ok(config)
}

#[cfg(feature = "http2")]
fn build_root_store(tls_flow: &TlsFlowState) -> Result<RootCertStore, Http2RequestError> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    if let Some(pem) = tls_flow.trusted_certificate_pem() {
        let certificates = rustls_pemfile::certs(&mut BufReader::new(pem.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| {
                Http2RequestError::transport(format!(
                    "failed to parse trusted certificates for http2 session: {err}",
                ))
            })?;
        let (added, _ignored) = roots.add_parsable_certificates(certificates);
        if added == 0 {
            return Err(Http2RequestError::transport(
                "trusted certificate bundle did not contain any usable certificates",
            ));
        }
    }

    Ok(roots)
}

#[cfg(feature = "http2")]
fn load_client_cert_chain(
    pem: Option<&str>,
) -> Result<Vec<CertificateDer<'static>>, Http2RequestError> {
    let Some(pem) = pem else {
        return Err(Http2RequestError::transport(
            "client certificate is unavailable",
        ));
    };
    let certificates = rustls_pemfile::certs(&mut BufReader::new(pem.as_bytes()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| {
            Http2RequestError::transport(format!("failed to parse client certificate: {err}"))
        })?;
    if certificates.is_empty() {
        return Err(Http2RequestError::transport(
            "client certificate chain is empty",
        ));
    }
    Ok(certificates)
}

#[cfg(feature = "http2")]
fn load_client_private_key(
    pem: Option<&str>,
) -> Result<rustls::pki_types::PrivateKeyDer<'static>, Http2RequestError> {
    let Some(pem) = pem else {
        return Err(Http2RequestError::transport(
            "client private key is unavailable",
        ));
    };
    rustls_pemfile::private_key(&mut BufReader::new(pem.as_bytes()))
        .map_err(|err| {
            Http2RequestError::transport(format!("failed to parse client private key: {err}"))
        })?
        .ok_or_else(|| Http2RequestError::transport("client private key is unavailable"))
}

#[cfg(feature = "http2")]
fn protocol_versions_for_http2(
    tls_flow: &TlsFlowState,
) -> Result<Vec<&'static rustls::SupportedProtocolVersion>, Http2RequestError> {
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
        HTTP11_ALPN_PROTOCOL.as_bytes().to_vec(),
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

fn session_origin(
    target_scheme: HttpUpstreamScheme,
    target_host: &str,
    target_port: u16,
) -> Option<String> {
    if target_host.is_empty() || target_port == 0 {
        return None;
    }
    Some(format!(
        "{}://{}:{target_port}",
        target_scheme.as_str(),
        target_host.to_ascii_lowercase()
    ))
}

#[cfg(feature = "http2")]
fn format_upstream_authority(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use crate::abi_impl::http::state::HttpUpstreamScheme;

    use super::session_origin;

    #[test]
    fn session_origin_normalizes_scheme_host_and_port() {
        assert_eq!(
            session_origin(HttpUpstreamScheme::Https, "Example.COM", 443).as_deref(),
            Some("https://example.com:443")
        );
        assert_eq!(
            session_origin(HttpUpstreamScheme::Http, "Example.COM", 8080).as_deref(),
            Some("http://example.com:8080")
        );
    }

    #[cfg(feature = "http2")]
    use std::collections::HashMap;

    #[cfg(feature = "http2")]
    use super::{
        Http2ControlEventSource, Http2GoawayState, Http2SessionFrontier, Http2SessionGoal,
        Http2StreamFrontier, Http2StreamGoal, Http2UpstreamSessionDagState,
    };

    #[cfg(feature = "http2")]
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

    #[cfg(feature = "http2")]
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

    #[cfg(feature = "http2")]
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

    #[cfg(feature = "http2")]
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
