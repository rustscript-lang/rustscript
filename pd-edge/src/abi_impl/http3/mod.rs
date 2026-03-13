mod downstream;
mod model;
mod upstream;

pub(crate) use self::downstream::{
    DownstreamHttp3ConnectionTracker, Http3DownstreamStreamAttachment,
    SharedHttp3DownstreamSessions, new_shared_http3_downstream_sessions,
};
pub(crate) use self::model::{
    Http3StreamRef, Http3UpstreamMode, response_version_label, select_upstream_mode,
    session_origin, supports_response_version,
};
pub(crate) use self::upstream::{
    Http3ObservedError, Http3ResponseBodyTracker, SharedHttp3UpstreamSessions,
    new_shared_http3_upstream_sessions, should_use_explicit_upstream_transport,
};
#[cfg(feature = "http3")]
pub(crate) use self::upstream::{
    Http3RequestError, Http3SendRequestOptions, classify_http3_error, send_request,
};
