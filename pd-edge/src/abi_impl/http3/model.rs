#![cfg_attr(not(feature = "http3"), allow(dead_code))]

use axum::http::Version;

use crate::abi_impl::{
    http::{HttpUpstreamScheme, HttpVersionPreference},
    quic::ALPN_PROTOCOL,
    transport::TlsFlowState,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Http3StreamRef {
    pub(crate) session_id: u64,
    pub(crate) stream_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Http3ControlEventSource {
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

pub(crate) fn select_upstream_mode(
    scheme: HttpUpstreamScheme,
    tls_flow: &TlsFlowState,
    version_preference: HttpVersionPreference,
) -> Http3UpstreamMode {
    if !cfg!(feature = "http3") {
        return Http3UpstreamMode::Disabled;
    }

    match version_preference {
        HttpVersionPreference::Http1 | HttpVersionPreference::Http2 => {
            return Http3UpstreamMode::Disabled;
        }
        HttpVersionPreference::Http3 | HttpVersionPreference::Auto => {}
    }

    let desired_alpn = tls_flow.desired_alpn();
    let explicitly_offers_http3 = desired_alpn
        .iter()
        .any(|protocol| protocol.eq_ignore_ascii_case(ALPN_PROTOCOL));
    let https_target = scheme == HttpUpstreamScheme::Https;

    match version_preference {
        HttpVersionPreference::Http3 if https_target => Http3UpstreamMode::Required,
        HttpVersionPreference::Http3 => Http3UpstreamMode::Disabled,
        HttpVersionPreference::Auto if https_target && explicitly_offers_http3 => {
            Http3UpstreamMode::Preferred
        }
        HttpVersionPreference::Http1
        | HttpVersionPreference::Http2
        | HttpVersionPreference::Auto => Http3UpstreamMode::Disabled,
    }
}

pub(crate) fn session_origin(scheme: HttpUpstreamScheme, host: &str, port: u16) -> Option<String> {
    if host.is_empty() || port == 0 {
        return None;
    }
    Some(format!(
        "{}://{}:{port}",
        scheme.as_str(),
        host.to_ascii_lowercase()
    ))
}
