mod downstream;
mod model;
mod upstream;

pub(crate) use self::downstream::{
    DownstreamHttp3ConnectionTracker, Http3DownstreamStreamAttachment,
    SharedHttp3DownstreamSessions, new_shared_http3_downstream_sessions,
};
pub(crate) use self::model::{
    Http3StreamRef, Http3UpstreamMode, select_upstream_mode, supports_response_version,
};
#[cfg(feature = "http3")]
pub(crate) use self::upstream::{
    Http3RequestBody, Http3RequestError, Http3SendRequestOptions, classify_http3_error,
    send_request,
};
pub(crate) use self::upstream::{
    Http3ResponseBodyTracker, SharedHttp3UpstreamSessions, new_shared_http3_upstream_sessions,
    should_use_explicit_upstream_transport,
};
