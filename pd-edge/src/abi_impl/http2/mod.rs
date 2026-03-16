mod downstream;
mod model;
mod upstream;

pub(crate) use self::downstream::{
    DownstreamHttp2ConnectionTracker, Http2DownstreamStreamAttachment,
    SharedHttpDownstreamSessions, new_shared_http_downstream_sessions,
};
#[cfg(test)]
pub(crate) use self::model::Http2SessionFrontier;
#[cfg(test)]
pub(crate) use self::upstream::total_active_streams;
pub(crate) use self::model::{
    Http2StreamRef, Http2UpstreamMode, configure_reqwest_builder, response_version_label,
    select_upstream_mode, supports_response_version,
};
#[cfg(feature = "http2")]
pub(crate) use self::upstream::{Http2RequestError, Http2SendRequest, send_request};
pub(crate) use self::upstream::{
    Http2ResponseBodyTracker, SharedHttpUpstreamSessions, classify_http2_error,
    new_shared_http_upstream_sessions, should_use_explicit_upstream_transport,
};
