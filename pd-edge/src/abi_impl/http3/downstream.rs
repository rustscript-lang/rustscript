#![cfg_attr(not(feature = "http3"), allow(dead_code))]

use std::collections::HashMap;
#[cfg(feature = "http3")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::cache::BoundedLruStore;

use super::model::{
    Http3ControlEventSource, Http3GoawayState, Http3ResetState, Http3SessionFrontier,
    Http3StreamFrontier,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Http3DownstreamStreamState {
    pub(crate) stream_id: u64,
    pub(crate) path: String,
    pub(crate) frontier: Http3StreamFrontier,
    pub(crate) reset: Option<Http3ResetState>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Http3DownstreamSessionState {
    pub(crate) session_id: u64,
    pub(crate) frontier: Http3SessionFrontier,
    pub(crate) peer_address: String,
    pub(crate) total_streams: u64,
    pub(crate) active_streams: u64,
    pub(crate) last_path: Option<String>,
    pub(crate) last_error: Option<String>,
    pub(crate) goaway: Option<Http3GoawayState>,
    pub(crate) streams: HashMap<u64, Http3DownstreamStreamState>,
}

#[derive(Clone, Debug)]
pub(crate) struct Http3DownstreamSessionStore {
    pub(crate) next_session_id: u64,
    pub(crate) sessions: BoundedLruStore<u64, Http3DownstreamSessionState>,
}

#[cfg(test)]
impl Http3DownstreamSessionStore {
    pub(crate) fn capacity(&self) -> usize {
        self.sessions.capacity()
    }

    pub(crate) fn len(&self) -> usize {
        self.sessions.len()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Http3DownstreamStreamAttachment {
    pub(crate) session_id: u64,
    pub(crate) stream_id: u64,
}

pub(crate) type SharedHttp3DownstreamSessions = Arc<Mutex<Http3DownstreamSessionStore>>;

pub(crate) fn new_shared_http3_downstream_sessions(
    capacity: usize,
) -> SharedHttp3DownstreamSessions {
    Arc::new(Mutex::new(Http3DownstreamSessionStore {
        next_session_id: 0,
        sessions: BoundedLruStore::new(capacity),
    }))
}

#[cfg(feature = "http3")]
#[derive(Clone, Debug)]
pub(crate) struct DownstreamHttp3ConnectionTracker {
    store: SharedHttp3DownstreamSessions,
    session_id: u64,
    saw_http3: Arc<AtomicBool>,
}

#[cfg(feature = "http3")]
impl DownstreamHttp3ConnectionTracker {
    pub(crate) fn new(store: SharedHttp3DownstreamSessions, peer_address: String) -> Self {
        let session_id = {
            let mut guard = store
                .lock()
                .expect("http3 downstream session store lock poisoned");
            let next = guard.next_session_id.saturating_add(1);
            guard.next_session_id = next;
            guard.sessions.insert(
                next,
                Http3DownstreamSessionState {
                    session_id: next,
                    frontier: Http3SessionFrontier::Candidate,
                    peer_address,
                    total_streams: 0,
                    active_streams: 0,
                    last_path: None,
                    last_error: None,
                    goaway: None,
                    streams: HashMap::new(),
                },
            );
            next
        };
        Self {
            store,
            session_id,
            saw_http3: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn observe_request(
        &self,
        path: &str,
        stream_id: u64,
    ) -> Option<Http3DownstreamStreamAttachment> {
        self.saw_http3.store(true, Ordering::Relaxed);
        let mut guard = self
            .store
            .lock()
            .expect("http3 downstream session store lock poisoned");
        let session = guard.sessions.get_mut(&self.session_id)?;
        if session.frontier == Http3SessionFrontier::Candidate {
            session.frontier = Http3SessionFrontier::Attached;
        }
        session.frontier = Http3SessionFrontier::Open;
        session.total_streams += 1;
        session.active_streams += 1;
        session.last_path = Some(path.to_string());
        session.streams.insert(
            stream_id,
            Http3DownstreamStreamState {
                stream_id,
                path: path.to_string(),
                frontier: Http3StreamFrontier::RequestCommitted,
                reset: None,
            },
        );
        Some(Http3DownstreamStreamAttachment {
            session_id: self.session_id,
            stream_id,
        })
    }

    pub(crate) fn note_response_head(&self, attachment: Option<&Http3DownstreamStreamAttachment>) {
        let Some(attachment) = attachment else {
            return;
        };
        let mut guard = self
            .store
            .lock()
            .expect("http3 downstream session store lock poisoned");
        let Some(session) = guard.sessions.get_mut(&attachment.session_id) else {
            return;
        };
        let Some(stream) = session.streams.get_mut(&attachment.stream_id) else {
            return;
        };
        stream.frontier = Http3StreamFrontier::ResponseHeadReady;
    }

    pub(crate) fn finish_request(
        &self,
        attachment: Option<&Http3DownstreamStreamAttachment>,
        error: Option<String>,
    ) {
        let Some(attachment) = attachment else {
            return;
        };
        let mut guard = self
            .store
            .lock()
            .expect("http3 downstream session store lock poisoned");
        let Some(session) = guard.sessions.get_mut(&attachment.session_id) else {
            return;
        };
        session.active_streams = session.active_streams.saturating_sub(1);
        if let Some(stream) = session.streams.get_mut(&attachment.stream_id) {
            if let Some(message) = error.clone() {
                stream.reset = Some(Http3ResetState {
                    reason: Some(message),
                    source: Http3ControlEventSource::Transport,
                });
                stream.frontier = Http3StreamFrontier::Reset;
            } else {
                if stream.frontier == Http3StreamFrontier::ResponseHeadReady {
                    stream.frontier = Http3StreamFrontier::ResponseBodyReady;
                }
                stream.frontier = Http3StreamFrontier::Closed;
            }
        }
        session
            .streams
            .retain(|_, stream| !stream.frontier.is_terminal());
        if session.frontier == Http3SessionFrontier::Draining && session.active_streams == 0 {
            session.frontier = Http3SessionFrontier::Closed;
        }
    }

    pub(crate) fn finish_connection(&self, error: Option<String>) {
        let mut guard = self
            .store
            .lock()
            .expect("http3 downstream session store lock poisoned");
        if !self.saw_http3.load(Ordering::Relaxed) {
            let _ = guard.sessions.remove(&self.session_id);
            return;
        }
        let Some(session) = guard.sessions.get_mut(&self.session_id) else {
            return;
        };
        session.active_streams = 0;
        session.last_error = error.clone();
        if let Some(message) = error {
            session.frontier = Http3SessionFrontier::Failed;
            for stream in session.streams.values_mut() {
                if !stream.frontier.is_terminal() {
                    stream.reset = Some(Http3ResetState {
                        reason: Some(message.clone()),
                        source: Http3ControlEventSource::Transport,
                    });
                    stream.frontier = Http3StreamFrontier::Reset;
                }
            }
        } else {
            session.goaway = Some(Http3GoawayState {
                reason: Some("connection closed gracefully".to_string()),
                source: Http3ControlEventSource::LocalRuntime,
            });
            session.frontier = Http3SessionFrontier::Draining;
            session
                .streams
                .retain(|_, stream| !stream.frontier.is_terminal());
            if session.streams.is_empty() {
                session.frontier = Http3SessionFrontier::Closed;
            }
        }
    }
}

#[cfg(not(feature = "http3"))]
#[derive(Clone, Debug)]
pub(crate) struct DownstreamHttp3ConnectionTracker;

#[cfg(not(feature = "http3"))]
impl DownstreamHttp3ConnectionTracker {
    pub(crate) fn new(_store: SharedHttp3DownstreamSessions, _peer_address: String) -> Self {
        Self
    }

    pub(crate) fn observe_request(
        &self,
        _path: &str,
        _stream_id: u64,
    ) -> Option<Http3DownstreamStreamAttachment> {
        None
    }

    pub(crate) fn note_response_head(&self, _attachment: Option<&Http3DownstreamStreamAttachment>) {
    }

    pub(crate) fn finish_request(
        &self,
        _attachment: Option<&Http3DownstreamStreamAttachment>,
        _error: Option<String>,
    ) {
    }

    pub(crate) fn finish_connection(&self, _error: Option<String>) {}
}
