#![cfg_attr(not(feature = "http3"), allow(dead_code))]

use axum::http::Version;
use url::Url;

use crate::abi_impl::{http::HttpVersionPreference, quic::ALPN_PROTOCOL, transport::TlsFlowState};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Http3StreamRef {
    pub(crate) session_id: u64,
    pub(crate) stream_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Http3ControlEventSource {
    RemotePeer,
    LocalRuntime,
    Transport,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Http3GoawayState {
    pub(crate) reason: Option<String>,
    pub(crate) source: Http3ControlEventSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Http3ResetState {
    pub(crate) reason: Option<String>,
    pub(crate) source: Http3ControlEventSource,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum Http3UpstreamMode {
    #[default]
    Disabled,
    Preferred,
    Required,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Http3SessionGoal {
    Attached,
    Open,
    Draining,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Http3StreamGoal {
    Attached,
    RequestCommitted,
    ResponseHeadReady,
    ResponseBodyReady,
    Closed,
    Reset,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Http3SessionFrontier {
    #[default]
    Candidate,
    Attached,
    ControlStreamsOpen,
    SettingsExchanged,
    Open,
    Draining,
    Closed,
    Failed,
}

impl Http3SessionFrontier {
    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Closed | Self::Failed)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Http3StreamFrontier {
    #[default]
    Reserved,
    AttachedToExchange,
    RequestCommitted,
    RequestBodyOpen,
    ResponseHeadReady,
    ResponseBodyReady,
    Closed,
    Reset,
}

impl Http3StreamFrontier {
    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Closed | Self::Reset)
    }
}

pub(crate) fn supports_response_version(version: Version) -> bool {
    matches!(version, Version::HTTP_3)
}

pub(crate) fn response_version_label() -> &'static str {
    "3"
}

pub(crate) fn select_upstream_mode(
    target: &str,
    tls_flow: &TlsFlowState,
    version_preference: HttpVersionPreference,
) -> Http3UpstreamMode {
    if !cfg!(feature = "http3") {
        return Http3UpstreamMode::Disabled;
    }

    let scheme = Url::parse(target)
        .ok()
        .map(|url| url.scheme().to_ascii_lowercase())
        .unwrap_or_default();
    let desired_alpn = tls_flow.desired_alpn();
    let explicitly_offers_http3 = desired_alpn
        .iter()
        .any(|protocol| protocol.eq_ignore_ascii_case(ALPN_PROTOCOL));
    let https_target = scheme == "https";

    match version_preference {
        HttpVersionPreference::Http3 if https_target => Http3UpstreamMode::Required,
        HttpVersionPreference::Http3 => Http3UpstreamMode::Disabled,
        HttpVersionPreference::Auto if https_target && explicitly_offers_http3 => {
            Http3UpstreamMode::Preferred
        }
        _ => Http3UpstreamMode::Disabled,
    }
}

pub(crate) fn session_origin(target: &str) -> Option<String> {
    let url = Url::parse(target).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let port = url.port_or_known_default()?;
    Some(format!(
        "{}://{}:{}",
        url.scheme().to_ascii_lowercase(),
        host,
        port
    ))
}
