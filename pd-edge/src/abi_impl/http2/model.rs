#![cfg_attr(not(feature = "http2"), allow(dead_code))]

use axum::http::Version;

use crate::abi_impl::{
    http::{HttpUpstreamScheme, HttpVersionPreference},
    transport::{HTTP11_ALPN_PROTOCOL, TlsFlowState},
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
    pub(crate) fn as_str(self) -> &'static str {
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
    pub(crate) fn is_terminal(self) -> bool {
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
    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Closed | Self::Reset)
    }
}

pub(crate) fn supports_response_version(version: Version) -> bool {
    matches!(version, Version::HTTP_2)
}

pub(crate) fn response_version_label() -> &'static str {
    "2"
}

pub(crate) fn select_upstream_mode(
    scheme: HttpUpstreamScheme,
    tls_flow: &TlsFlowState,
    version_preference: HttpVersionPreference,
) -> Http2UpstreamMode {
    if !cfg!(feature = "http2") {
        return Http2UpstreamMode::Disabled;
    }

    match version_preference {
        HttpVersionPreference::Http1 | HttpVersionPreference::Http3 => {
            return Http2UpstreamMode::Disabled;
        }
        HttpVersionPreference::Http2 => {
            return if scheme == HttpUpstreamScheme::Https {
                Http2UpstreamMode::AutomaticTls
            } else {
                Http2UpstreamMode::PriorKnowledge
            };
        }
        HttpVersionPreference::Auto => {}
    }

    let desired_alpn = tls_flow.desired_alpn();
    let explicitly_offers_http2 = desired_alpn
        .iter()
        .any(|protocol| protocol.eq_ignore_ascii_case(ALPN_PROTOCOL));
    let explicitly_prefers_http11 = desired_alpn
        .iter()
        .any(|protocol| protocol.eq_ignore_ascii_case(HTTP11_ALPN_PROTOCOL));
    let explicitly_rejects_http2 =
        !desired_alpn.is_empty() && explicitly_prefers_http11 && !explicitly_offers_http2;
    if explicitly_rejects_http2 {
        return Http2UpstreamMode::Disabled;
    }

    match scheme {
        HttpUpstreamScheme::Https => Http2UpstreamMode::AutomaticTls,
        HttpUpstreamScheme::Http if explicitly_offers_http2 => Http2UpstreamMode::PriorKnowledge,
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
        builder
    }

    #[cfg(feature = "http2")]
    {
        match mode {
            Http2UpstreamMode::Disabled | Http2UpstreamMode::AutomaticTls => builder,
            Http2UpstreamMode::PriorKnowledge => builder.http2_prior_knowledge(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::abi_impl::{
        http::{HttpUpstreamScheme, HttpVersionPreference},
        transport::TlsFlowState,
    };

    use super::{ALPN_PROTOCOL, Http2UpstreamMode, select_upstream_mode};

    #[test]
    fn https_targets_select_automatic_http2_when_enabled() {
        let mode = select_upstream_mode(
            HttpUpstreamScheme::Https,
            &TlsFlowState::default(),
            HttpVersionPreference::Auto,
        );
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
        let mode =
            select_upstream_mode(HttpUpstreamScheme::Http, &flow, HttpVersionPreference::Auto);
        if cfg!(feature = "http2") {
            assert_eq!(mode, Http2UpstreamMode::PriorKnowledge);
        } else {
            assert_eq!(mode, Http2UpstreamMode::Disabled);
        }
    }

    #[test]
    fn explicit_http3_preference_disables_http2() {
        let mode = select_upstream_mode(
            HttpUpstreamScheme::Https,
            &TlsFlowState::default(),
            HttpVersionPreference::Http3,
        );
        assert_eq!(mode, Http2UpstreamMode::Disabled);
    }

    #[test]
    fn explicit_http2_preference_requires_http2_even_without_alpn_hint() {
        let mode = select_upstream_mode(
            HttpUpstreamScheme::Http,
            &TlsFlowState::default(),
            HttpVersionPreference::Http2,
        );
        if cfg!(feature = "http2") {
            assert_eq!(mode, Http2UpstreamMode::PriorKnowledge);
        } else {
            assert_eq!(mode, Http2UpstreamMode::Disabled);
        }
    }
}
