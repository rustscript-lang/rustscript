#![cfg_attr(not(feature = "http2"), allow(dead_code))]

use std::collections::HashMap;
#[cfg(feature = "http2")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::http::Version;

use crate::cache::BoundedLruStore;
#[cfg(feature = "http2")]
use crate::lock_metrics::{self, LockMetricKey};

#[cfg(feature = "http2")]
use super::model::{Http2ControlEventSource, supports_response_version};
use super::model::{Http2GoawayState, Http2ResetState, Http2SessionFrontier, Http2StreamFrontier};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Http2DownstreamStreamState {
    pub(crate) stream_id: u64,
    pub(crate) path: String,
    pub(crate) frontier: Http2StreamFrontier,
    pub(crate) reset: Option<Http2ResetState>,
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
    pub(crate) next_session_id: u64,
    pub(crate) sessions: BoundedLruStore<u64, Http2DownstreamSessionState>,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Http2DownstreamStreamAttachment {
    pub(crate) session_id: u64,
    pub(crate) stream_id: u64,
}

pub(crate) type SharedHttpDownstreamSessions = Arc<Mutex<Http2DownstreamSessionStore>>;

pub(crate) fn new_shared_http_downstream_sessions(capacity: usize) -> SharedHttpDownstreamSessions {
    Arc::new(Mutex::new(Http2DownstreamSessionStore {
        next_session_id: 0,
        sessions: BoundedLruStore::new(capacity),
    }))
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
            let mut guard = lock_metrics::lock(
                &store,
                LockMetricKey::Http2DownstreamSessionStore,
                "http downstream session store lock poisoned",
            );
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
        let mut guard = lock_metrics::lock(
            &self.store,
            LockMetricKey::Http2DownstreamSessionStore,
            "http downstream session store lock poisoned",
        );
        let session = guard.sessions.get_mut(&self.session_id)?;
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
        let mut guard = lock_metrics::lock(
            &self.store,
            LockMetricKey::Http2DownstreamSessionStore,
            "http downstream session store lock poisoned",
        );
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
        let mut guard = lock_metrics::lock(
            &self.store,
            LockMetricKey::Http2DownstreamSessionStore,
            "http downstream session store lock poisoned",
        );
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
        let mut guard = lock_metrics::lock(
            &self.store,
            LockMetricKey::Http2DownstreamSessionStore,
            "http downstream session store lock poisoned",
        );
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
