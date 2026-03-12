#![cfg_attr(not(feature = "tls"), allow(dead_code))]

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
};

use axum::http::Version;
use url::Url;

pub(crate) const TCP_STREAM_DOWNSTREAM: i64 = 0;
pub(crate) const TCP_STREAM_DEFAULT_UPSTREAM: i64 = 1;
pub(crate) const UDP_SOCKET_DOWNSTREAM: i64 = 0;
pub(crate) const UDP_SOCKET_DEFAULT_UPSTREAM: i64 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TcpStreamRef {
    Downstream,
    DefaultUpstream,
}

impl TcpStreamRef {
    pub(crate) fn handle(self) -> i64 {
        match self {
            Self::Downstream => TCP_STREAM_DOWNSTREAM,
            Self::DefaultUpstream => TCP_STREAM_DEFAULT_UPSTREAM,
        }
    }
}

pub(crate) fn decode_tcp_stream_handle(handle: i64) -> Option<TcpStreamRef> {
    match handle {
        TCP_STREAM_DOWNSTREAM => Some(TcpStreamRef::Downstream),
        TCP_STREAM_DEFAULT_UPSTREAM => Some(TcpStreamRef::DefaultUpstream),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UdpSocketRef {
    Downstream,
    DefaultUpstream,
}

impl UdpSocketRef {
    pub(crate) fn handle(self) -> i64 {
        match self {
            Self::Downstream => UDP_SOCKET_DOWNSTREAM,
            Self::DefaultUpstream => UDP_SOCKET_DEFAULT_UPSTREAM,
        }
    }
}

pub(crate) fn decode_udp_socket_handle(handle: i64) -> Option<UdpSocketRef> {
    match handle {
        UDP_SOCKET_DOWNSTREAM => Some(UdpSocketRef::Downstream),
        UDP_SOCKET_DEFAULT_UPSTREAM => Some(UdpSocketRef::DefaultUpstream),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TlsSessionRef {
    Downstream,
    DefaultUpstream,
}

impl TlsSessionRef {
    pub(crate) fn handle(self) -> i64 {
        match self {
            Self::Downstream => TCP_STREAM_DOWNSTREAM,
            Self::DefaultUpstream => TCP_STREAM_DEFAULT_UPSTREAM,
        }
    }
}

pub(crate) fn decode_tls_session_handle(handle: i64) -> Option<TlsSessionRef> {
    match handle {
        TCP_STREAM_DOWNSTREAM => Some(TlsSessionRef::Downstream),
        TCP_STREAM_DEFAULT_UPSTREAM => Some(TlsSessionRef::DefaultUpstream),
        _ => None,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TcpFlowState {
    configured: bool,
    connected: bool,
    rx_observed: bool,
    tx_observed: bool,
    closed: bool,
}

impl TcpFlowState {
    pub(crate) fn downstream_ready() -> Self {
        Self {
            configured: true,
            connected: true,
            rx_observed: false,
            tx_observed: false,
            closed: false,
        }
    }

    pub(crate) fn configure(&mut self) {
        self.configured = true;
        self.closed = false;
    }

    pub(crate) fn note_read(&mut self) {
        self.rx_observed = true;
        self.closed = false;
    }

    pub(crate) fn note_write(&mut self) {
        self.tx_observed = true;
        self.closed = false;
    }

    pub(crate) fn mark_connected(&mut self) {
        self.configured = true;
        self.connected = true;
        self.closed = false;
    }

    #[cfg(test)]
    pub(crate) fn is_configured(&self) -> bool {
        self.configured
    }

    #[cfg(test)]
    pub(crate) fn is_connected(&self) -> bool {
        self.connected
    }

    #[cfg(test)]
    pub(crate) fn saw_read(&self) -> bool {
        self.rx_observed
    }

    #[cfg(test)]
    pub(crate) fn saw_write(&self) -> bool {
        self.tx_observed
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum UdpSocketPhase {
    #[default]
    Inactive,
    Bound,
    Configured,
    Connected,
    Closed,
    Failed,
}

impl UdpSocketPhase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Inactive => "inactive",
            Self::Bound => "bound",
            Self::Configured => "configured",
            Self::Connected => "connected",
            Self::Closed => "closed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct UdpSocketState {
    present: bool,
    phase: UdpSocketPhase,
    bind_address: Option<String>,
    target: Option<String>,
    local_address: Option<String>,
    peer_address: Option<String>,
    failure_message: Option<String>,
}

impl UdpSocketState {
    pub(crate) fn set_bind_address(&mut self, address: String) {
        self.present = true;
        self.bind_address = if address.is_empty() {
            None
        } else {
            Some(address)
        };
        self.local_address = None;
        self.failure_message = None;
        if self.phase == UdpSocketPhase::Inactive {
            self.phase = UdpSocketPhase::Bound;
        }
    }

    pub(crate) fn set_target(&mut self, target: String) {
        self.present = true;
        self.target = if target.is_empty() {
            None
        } else {
            Some(target)
        };
        self.peer_address = None;
        self.failure_message = None;
        self.phase = UdpSocketPhase::Configured;
    }

    pub(crate) fn mark_connected(&mut self, local_address: String, peer_address: String) {
        self.present = true;
        self.phase = UdpSocketPhase::Connected;
        self.local_address = Some(local_address);
        self.peer_address = Some(peer_address);
        self.failure_message = None;
    }

    pub(crate) fn mark_closed(&mut self) {
        self.phase = UdpSocketPhase::Closed;
    }

    pub(crate) fn mark_failed(&mut self, message: impl Into<String>) {
        self.present = true;
        self.phase = UdpSocketPhase::Failed;
        self.failure_message = Some(message.into());
    }

    pub(crate) fn is_present(&self) -> bool {
        self.present
    }

    pub(crate) fn phase(&self) -> UdpSocketPhase {
        self.phase
    }

    pub(crate) fn bind_address(&self) -> Option<&str> {
        self.bind_address.as_deref()
    }

    pub(crate) fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }

    pub(crate) fn local_address(&self) -> &str {
        self.local_address.as_deref().unwrap_or_default()
    }

    pub(crate) fn peer_address(&self) -> &str {
        self.peer_address
            .as_deref()
            .or(self.target.as_deref())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn failure_message(&self) -> &str {
        self.failure_message.as_deref().unwrap_or_default()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum TlsSessionPath {
    #[default]
    None,
    Opaque,
    FullHandshake,
    #[allow(dead_code)]
    SessionReuse,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TlsHandshakePhase {
    #[default]
    None,
    Configured,
    ClientHelloPrepared,
    ClientHelloSent,
    ServerHelloReceived,
    ServerCertificateReceived,
    ServerCertificateVerified,
    VerificationSkipped,
    PlaintextReady,
    Failed,
    OpaqueEstablished,
}

impl TlsHandshakePhase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Configured => "configured",
            Self::ClientHelloPrepared => "client-hello-prepared",
            Self::ClientHelloSent => "client-hello-sent",
            Self::ServerHelloReceived => "server-hello-received",
            Self::ServerCertificateReceived => "server-certificate-received",
            Self::ServerCertificateVerified => "server-certificate-verified",
            Self::VerificationSkipped => "verification-skipped",
            Self::PlaintextReady => "plaintext-ready",
            Self::Failed => "failed",
            Self::OpaqueEstablished => "opaque-established",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum TlsProtocolVersion {
    Tls1_0,
    Tls1_1,
    Tls1_2,
    Tls1_3,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TlsSessionCacheKey {
    pub(crate) origin: String,
    pub(crate) desired_alpn: Vec<String>,
    pub(crate) verify_peer: bool,
    pub(crate) verify_hostname: bool,
    pub(crate) sni_enabled: bool,
    pub(crate) trusted_certificate_fingerprint: Option<u64>,
    pub(crate) client_certificate_fingerprint: Option<u64>,
    pub(crate) client_private_key_fingerprint: Option<u64>,
    pub(crate) min_version: Option<TlsProtocolVersion>,
    pub(crate) max_version: Option<TlsProtocolVersion>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CachedTlsSession {
    pub(crate) negotiated_alpn: Option<String>,
    pub(crate) peer_name: Option<String>,
    pub(crate) server_name: Option<String>,
    pub(crate) peer_certificate_der: Option<Vec<u8>>,
}

pub(crate) type SharedTlsSessionCache = Arc<Mutex<HashMap<TlsSessionCacheKey, CachedTlsSession>>>;
pub(crate) type SharedUdpSocketIo = Arc<tokio::sync::Mutex<tokio::net::UdpSocket>>;

pub(crate) fn new_shared_tls_session_cache() -> SharedTlsSessionCache {
    Arc::new(Mutex::new(HashMap::new()))
}

impl TlsProtocolVersion {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Tls1_0 => "1.0",
            Self::Tls1_1 => "1.1",
            Self::Tls1_2 => "1.2",
            Self::Tls1_3 => "1.3",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TlsFlowState {
    present: bool,
    handshake_complete: bool,
    plaintext_ready: bool,
    session_path: TlsSessionPath,
    phase: TlsHandshakePhase,
    peer_name: Option<String>,
    server_name: Option<String>,
    alpn: Option<String>,
    desired_alpn: Vec<String>,
    verify_peer: bool,
    verify_hostname: bool,
    sni_enabled: bool,
    trusted_certificate_pem: Option<String>,
    client_certificate_pem: Option<String>,
    client_private_key_pem: Option<String>,
    min_version: Option<TlsProtocolVersion>,
    max_version: Option<TlsProtocolVersion>,
    peer_certificate_der: Option<Vec<u8>>,
}

impl TlsFlowState {
    pub(crate) fn for_downstream_request(scheme: &str, host: &str, http_version: &str) -> Self {
        if !scheme.eq_ignore_ascii_case("https") {
            return Self::default();
        }

        Self {
            present: true,
            handshake_complete: true,
            plaintext_ready: true,
            session_path: TlsSessionPath::Opaque,
            phase: TlsHandshakePhase::OpaqueEstablished,
            peer_name: None,
            server_name: normalize_authority_host(host),
            alpn: alpn_from_http_version_label(http_version),
            desired_alpn: Vec::new(),
            verify_peer: true,
            verify_hostname: true,
            sni_enabled: true,
            trusted_certificate_pem: None,
            client_certificate_pem: None,
            client_private_key_pem: None,
            min_version: None,
            max_version: None,
            peer_certificate_der: None,
        }
    }

    pub(crate) fn observe_target(&mut self, target: &str) {
        self.present = upstream_target_uses_tls(target);
        self.handshake_complete = false;
        self.plaintext_ready = false;
        self.session_path = TlsSessionPath::None;
        self.peer_name = upstream_target_host(target);
        self.server_name = if self.present && self.sni_enabled {
            self.peer_name.clone()
        } else {
            None
        };
        self.alpn = None;
        self.peer_certificate_der = None;
        self.phase = if self.present {
            TlsHandshakePhase::Configured
        } else {
            TlsHandshakePhase::None
        };
    }

    pub(crate) fn set_desired_alpn(&mut self, protocols: Vec<String>) {
        self.desired_alpn = protocols;
    }

    pub(crate) fn set_verify_peer(&mut self, verify: bool) {
        self.verify_peer = verify;
    }

    pub(crate) fn set_verify_hostname(&mut self, verify: bool) {
        self.verify_hostname = verify;
    }

    pub(crate) fn set_sni_enabled(&mut self, enabled: bool) {
        self.sni_enabled = enabled;
        if enabled && self.present {
            self.server_name = self.peer_name.clone();
        } else if !enabled {
            self.server_name = None;
        }
    }

    pub(crate) fn set_trusted_certificate_pem(&mut self, certificate_pem: String) {
        self.trusted_certificate_pem = if certificate_pem.is_empty() {
            None
        } else {
            Some(certificate_pem)
        };
    }

    pub(crate) fn set_client_certificate_pem(&mut self, certificate_pem: String) {
        self.client_certificate_pem = if certificate_pem.is_empty() {
            None
        } else {
            Some(certificate_pem)
        };
    }

    pub(crate) fn set_client_private_key_pem(&mut self, private_key_pem: String) {
        self.client_private_key_pem = if private_key_pem.is_empty() {
            None
        } else {
            Some(private_key_pem)
        };
    }

    pub(crate) fn set_min_version(&mut self, version: Option<TlsProtocolVersion>) {
        self.min_version = version;
    }

    pub(crate) fn set_max_version(&mut self, version: Option<TlsProtocolVersion>) {
        self.max_version = version;
    }

    pub(crate) fn note_handshake_prepared(&mut self) {
        if !self.present {
            return;
        }
        self.phase = TlsHandshakePhase::ClientHelloPrepared;
        self.handshake_complete = false;
        self.plaintext_ready = false;
        self.peer_certificate_der = None;
    }

    pub(crate) fn note_client_hello_sent(&mut self) {
        if !self.present {
            return;
        }
        self.phase = TlsHandshakePhase::ClientHelloSent;
    }

    pub(crate) fn note_server_hello_received(&mut self) {
        if !self.present {
            return;
        }
        self.phase = TlsHandshakePhase::ServerHelloReceived;
    }

    pub(crate) fn note_server_certificate_received(
        &mut self,
        peer_certificate_der: Option<Vec<u8>>,
    ) {
        if !self.present {
            return;
        }
        self.phase = TlsHandshakePhase::ServerCertificateReceived;
        self.peer_certificate_der = peer_certificate_der;
    }

    pub(crate) fn note_server_certificate_verified(&mut self) {
        if !self.present {
            return;
        }
        self.phase = TlsHandshakePhase::ServerCertificateVerified;
    }

    pub(crate) fn note_verification_skipped(&mut self) {
        if !self.present {
            return;
        }
        self.phase = TlsHandshakePhase::VerificationSkipped;
    }

    pub(crate) fn mark_handshake_complete(&mut self, negotiated_alpn: Option<String>) {
        if !self.present {
            return;
        }

        self.handshake_complete = true;
        self.plaintext_ready = true;
        self.phase = TlsHandshakePhase::PlaintextReady;
        if matches!(self.session_path, TlsSessionPath::None) {
            self.session_path = TlsSessionPath::FullHandshake;
        }
        self.alpn = negotiated_alpn;
    }

    pub(crate) fn mark_session_reused(&mut self, cached: &CachedTlsSession) {
        if !self.present {
            return;
        }

        self.handshake_complete = true;
        self.plaintext_ready = true;
        self.session_path = TlsSessionPath::SessionReuse;
        self.phase = TlsHandshakePhase::PlaintextReady;
        self.alpn = cached.negotiated_alpn.clone();
        self.peer_certificate_der = cached.peer_certificate_der.clone();
        if let Some(peer_name) = &cached.peer_name {
            self.peer_name = Some(peer_name.clone());
        }
        if self.sni_enabled {
            if let Some(server_name) = &cached.server_name {
                self.server_name = Some(server_name.clone());
            } else {
                self.server_name = self.peer_name.clone();
            }
        } else {
            self.server_name = None;
        }
    }

    pub(crate) fn mark_failed(&mut self) {
        if !self.present {
            return;
        }
        self.handshake_complete = false;
        self.plaintext_ready = false;
        self.phase = TlsHandshakePhase::Failed;
    }

    pub(crate) fn is_present(&self) -> bool {
        self.present
    }

    pub(crate) fn handshake_complete(&self) -> bool {
        self.handshake_complete
    }

    pub(crate) fn peer_name(&self) -> &str {
        self.peer_name.as_deref().unwrap_or_default()
    }

    pub(crate) fn server_name(&self) -> &str {
        self.server_name.as_deref().unwrap_or_default()
    }

    pub(crate) fn alpn(&self) -> &str {
        self.alpn.as_deref().unwrap_or_default()
    }

    pub(crate) fn phase_label(&self) -> &'static str {
        self.phase.as_str()
    }

    pub(crate) fn desired_alpn(&self) -> &[String] {
        &self.desired_alpn
    }

    pub(crate) fn accepts_negotiated_alpn(&self, negotiated_alpn: Option<&str>) -> bool {
        if self.desired_alpn.is_empty() {
            return true;
        }
        let Some(negotiated_alpn) = negotiated_alpn else {
            return false;
        };
        self.desired_alpn
            .iter()
            .any(|configured| configured == negotiated_alpn)
    }

    pub(crate) fn verify_peer(&self) -> bool {
        self.verify_peer
    }

    pub(crate) fn verify_hostname(&self) -> bool {
        self.verify_hostname
    }

    pub(crate) fn sni_enabled(&self) -> bool {
        self.sni_enabled
    }

    pub(crate) fn trusted_certificate_pem(&self) -> Option<&str> {
        self.trusted_certificate_pem.as_deref()
    }

    pub(crate) fn client_certificate_pem(&self) -> Option<&str> {
        self.client_certificate_pem.as_deref()
    }

    pub(crate) fn client_private_key_pem(&self) -> Option<&str> {
        self.client_private_key_pem.as_deref()
    }

    pub(crate) fn min_version(&self) -> Option<TlsProtocolVersion> {
        self.min_version
    }

    pub(crate) fn max_version(&self) -> Option<TlsProtocolVersion> {
        self.max_version
    }

    pub(crate) fn peer_certificate_der(&self) -> Option<&[u8]> {
        self.peer_certificate_der.as_deref()
    }

    pub(crate) fn requires_custom_client(&self) -> bool {
        !self.desired_alpn.is_empty()
            || !self.verify_peer
            || !self.verify_hostname
            || !self.sni_enabled
            || self.trusted_certificate_pem.is_some()
            || self.client_certificate_pem.is_some()
            || self.client_private_key_pem.is_some()
            || self.min_version.is_some()
            || self.max_version.is_some()
    }

    pub(crate) fn is_session_reused(&self) -> bool {
        matches!(self.session_path, TlsSessionPath::SessionReuse)
    }

    #[cfg(test)]
    pub(crate) fn plaintext_ready(&self) -> bool {
        self.plaintext_ready
    }
}

impl Default for TlsFlowState {
    fn default() -> Self {
        Self {
            present: false,
            handshake_complete: false,
            plaintext_ready: false,
            session_path: TlsSessionPath::None,
            phase: TlsHandshakePhase::None,
            peer_name: None,
            server_name: None,
            alpn: None,
            desired_alpn: Vec::new(),
            verify_peer: true,
            verify_hostname: true,
            sni_enabled: true,
            trusted_certificate_pem: None,
            client_certificate_pem: None,
            client_private_key_pem: None,
            min_version: None,
            max_version: None,
            peer_certificate_der: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TcpTransportDag {
    pub(crate) downstream: TcpFlowState,
    pub(crate) default_upstream: TcpFlowState,
}

impl TcpTransportDag {
    pub(crate) fn for_http_request() -> Self {
        Self {
            downstream: TcpFlowState::downstream_ready(),
            default_upstream: TcpFlowState::default(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TlsTransportDag {
    pub(crate) downstream: TlsFlowState,
    pub(crate) default_upstream: TlsFlowState,
}

impl TlsTransportDag {
    pub(crate) fn for_http_request(scheme: &str, host: &str, http_version: &str) -> Self {
        Self {
            downstream: TlsFlowState::for_downstream_request(scheme, host, http_version),
            default_upstream: TlsFlowState::default(),
        }
    }
}

pub(crate) fn alpn_from_http_version(version: Version) -> Option<String> {
    match version {
        Version::HTTP_09 => Some("http/0.9".to_string()),
        Version::HTTP_10 => Some("http/1.0".to_string()),
        Version::HTTP_11 => Some("http/1.1".to_string()),
        Version::HTTP_2 => Some("h2".to_string()),
        Version::HTTP_3 => Some("h3".to_string()),
        _ => None,
    }
}

fn alpn_from_http_version_label(version: &str) -> Option<String> {
    match version {
        "0.9" => Some("http/0.9".to_string()),
        "1.0" => Some("http/1.0".to_string()),
        "1.1" => Some("http/1.1".to_string()),
        "2" => Some("h2".to_string()),
        "3" => Some("h3".to_string()),
        _ => None,
    }
}

fn upstream_target_uses_tls(target: &str) -> bool {
    Url::parse(target)
        .map(|url| matches!(url.scheme().to_ascii_lowercase().as_str(), "https" | "wss"))
        .unwrap_or(false)
}

fn upstream_target_host(target: &str) -> Option<String> {
    if let Ok(url) = Url::parse(target) {
        return url.host_str().map(str::to_string);
    }

    Url::parse(&format!("http://{target}"))
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
}

fn tls_session_origin(target: &str) -> Option<String> {
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

fn fingerprint_optional_pem(value: Option<&str>) -> Option<u64> {
    let value = value?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    Some(hasher.finish())
}

pub(crate) fn tls_session_cache_key(
    target: &str,
    flow: &TlsFlowState,
) -> Option<TlsSessionCacheKey> {
    if !flow.is_present() {
        return None;
    }
    Some(TlsSessionCacheKey {
        origin: tls_session_origin(target)?,
        desired_alpn: flow.desired_alpn().to_vec(),
        verify_peer: flow.verify_peer(),
        verify_hostname: flow.verify_hostname(),
        sni_enabled: flow.sni_enabled(),
        trusted_certificate_fingerprint: fingerprint_optional_pem(flow.trusted_certificate_pem()),
        client_certificate_fingerprint: fingerprint_optional_pem(flow.client_certificate_pem()),
        client_private_key_fingerprint: fingerprint_optional_pem(flow.client_private_key_pem()),
        min_version: flow.min_version(),
        max_version: flow.max_version(),
    })
}

fn normalize_authority_host(value: &str) -> Option<String> {
    Url::parse(&format!("http://{value}"))
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
}

#[cfg(test)]
mod tests {
    use axum::http::Version;

    use super::{
        CachedTlsSession, TCP_STREAM_DEFAULT_UPSTREAM, TCP_STREAM_DOWNSTREAM, TlsFlowState,
        TlsHandshakePhase, TlsProtocolVersion, TlsSessionPath, TlsTransportDag,
        UDP_SOCKET_DEFAULT_UPSTREAM, UDP_SOCKET_DOWNSTREAM, UdpSocketPhase, UdpSocketState,
        alpn_from_http_version, decode_tcp_stream_handle, decode_tls_session_handle,
        decode_udp_socket_handle, tls_session_cache_key,
    };

    #[test]
    fn reserved_socket_handles_decode_to_default_streams_and_sessions() {
        assert_eq!(
            decode_tcp_stream_handle(TCP_STREAM_DOWNSTREAM),
            Some(super::TcpStreamRef::Downstream)
        );
        assert_eq!(
            decode_tcp_stream_handle(TCP_STREAM_DEFAULT_UPSTREAM),
            Some(super::TcpStreamRef::DefaultUpstream)
        );
        assert_eq!(
            decode_tls_session_handle(TCP_STREAM_DOWNSTREAM),
            Some(super::TlsSessionRef::Downstream)
        );
        assert_eq!(
            decode_tls_session_handle(TCP_STREAM_DEFAULT_UPSTREAM),
            Some(super::TlsSessionRef::DefaultUpstream)
        );
        assert_eq!(decode_tcp_stream_handle(2), None);
        assert_eq!(decode_tls_session_handle(2), None);
    }

    #[test]
    fn reserved_udp_socket_handles_decode_to_default_sockets() {
        assert_eq!(
            decode_udp_socket_handle(UDP_SOCKET_DOWNSTREAM),
            Some(super::UdpSocketRef::Downstream)
        );
        assert_eq!(
            decode_udp_socket_handle(UDP_SOCKET_DEFAULT_UPSTREAM),
            Some(super::UdpSocketRef::DefaultUpstream)
        );
        assert_eq!(decode_udp_socket_handle(2), None);
    }

    #[test]
    fn udp_socket_state_tracks_target_binding_and_failures() {
        let mut socket = UdpSocketState::default();
        assert_eq!(socket.phase(), UdpSocketPhase::Inactive);
        assert!(!socket.is_present());

        socket.set_bind_address("127.0.0.1:0".to_string());
        assert!(socket.is_present());
        assert_eq!(socket.phase(), UdpSocketPhase::Bound);
        assert_eq!(socket.bind_address(), Some("127.0.0.1:0"));

        socket.set_target("udp://127.0.0.1:9000".to_string());
        assert_eq!(socket.phase(), UdpSocketPhase::Configured);
        assert_eq!(socket.peer_address(), "udp://127.0.0.1:9000");

        socket.mark_connected("127.0.0.1:45000".to_string(), "127.0.0.1:9000".to_string());
        assert_eq!(socket.phase(), UdpSocketPhase::Connected);
        assert_eq!(socket.local_address(), "127.0.0.1:45000");
        assert_eq!(socket.peer_address(), "127.0.0.1:9000");

        socket.mark_failed("boom");
        assert_eq!(socket.phase(), UdpSocketPhase::Failed);
        assert_eq!(socket.failure_message(), "boom");

        socket.mark_closed();
        assert_eq!(socket.phase(), UdpSocketPhase::Closed);
    }

    #[test]
    fn downstream_https_tls_flow_carries_server_name_and_alpn() {
        let dag = TlsTransportDag::for_http_request("https", "api.example.com:443", "2");
        assert!(dag.downstream.is_present());
        assert_eq!(dag.downstream.server_name(), "api.example.com");
        assert_eq!(dag.downstream.alpn(), "h2");
        assert_eq!(dag.downstream.peer_name(), "");
        assert_eq!(dag.downstream.phase_label(), "opaque-established");
        assert!(!dag.downstream.is_session_reused());
    }

    #[test]
    fn observing_upstream_https_target_resets_tls_flow_until_handshake_completes() {
        let mut flow = TlsFlowState::default();
        flow.observe_target("https://origin.example.net:8443/path");
        assert!(flow.is_present());
        assert_eq!(flow.peer_name(), "origin.example.net");
        assert_eq!(flow.server_name(), "origin.example.net");
        assert_eq!(flow.alpn(), "");
        assert_eq!(flow.phase_label(), "configured");

        flow.note_handshake_prepared();
        flow.note_client_hello_sent();
        flow.note_server_hello_received();
        flow.note_server_certificate_received(Some(vec![1, 2, 3]));
        flow.note_server_certificate_verified();
        flow.mark_handshake_complete(Some("h2".to_string()));
        assert!(flow.is_present());
        assert_eq!(flow.peer_name(), "origin.example.net");
        assert_eq!(flow.alpn(), "h2");
        assert_eq!(flow.phase_label(), "plaintext-ready");
        assert_eq!(flow.peer_certificate_der(), Some(&[1, 2, 3][..]));
        assert_eq!(flow.session_path, TlsSessionPath::FullHandshake);
    }

    #[test]
    fn observing_plain_http_target_clears_tls_presence() {
        let mut flow = TlsFlowState::for_downstream_request("https", "downstream.example", "1.1");
        flow.observe_target("http://origin.example.net/plain");
        assert!(!flow.is_present());
        assert_eq!(flow.peer_name(), "origin.example.net");
        assert_eq!(flow.alpn(), "");
        assert_eq!(flow.phase_label(), "none");
        flow.mark_handshake_complete(Some("http/1.1".to_string()));
        assert_eq!(flow.alpn(), "");
    }

    #[test]
    fn tls_configuration_flags_and_alpn_filters_are_tracked() {
        let mut flow = TlsFlowState::default();
        flow.observe_target("https://origin.example.net/api");
        flow.set_desired_alpn(vec!["h2".to_string(), "http/1.1".to_string()]);
        flow.set_verify_peer(false);
        flow.set_verify_hostname(false);
        flow.set_sni_enabled(false);
        flow.set_trusted_certificate_pem("ca-pem".to_string());
        flow.set_client_certificate_pem("client-cert".to_string());
        flow.set_client_private_key_pem("client-key".to_string());
        flow.set_min_version(Some(TlsProtocolVersion::Tls1_2));
        flow.set_max_version(Some(TlsProtocolVersion::Tls1_3));

        assert!(flow.requires_custom_client());
        assert!(!flow.verify_peer());
        assert!(!flow.verify_hostname());
        assert!(!flow.sni_enabled());
        assert_eq!(flow.server_name(), "");
        assert_eq!(
            flow.desired_alpn(),
            ["h2".to_string(), "http/1.1".to_string()]
        );
        assert!(flow.accepts_negotiated_alpn(Some("h2")));
        assert!(!flow.accepts_negotiated_alpn(Some("imap")));
        assert_eq!(flow.min_version(), Some(TlsProtocolVersion::Tls1_2));
        assert_eq!(flow.max_version(), Some(TlsProtocolVersion::Tls1_3));
    }

    #[test]
    fn tls_failure_phase_is_recorded() {
        let mut flow = TlsFlowState::default();
        flow.observe_target("https://origin.example.net/api");
        flow.note_handshake_prepared();
        flow.note_client_hello_sent();
        flow.mark_failed();

        assert_eq!(flow.phase_label(), TlsHandshakePhase::Failed.as_str());
        assert!(!flow.handshake_complete());
        assert!(!flow.plaintext_ready());
    }

    #[test]
    fn resumed_tls_session_marks_handshake_complete_without_full_handshake_path() {
        let mut flow = TlsFlowState::default();
        flow.observe_target("https://origin.example.net/api");
        flow.mark_session_reused(&CachedTlsSession {
            negotiated_alpn: Some("h2".to_string()),
            peer_name: Some("origin.example.net".to_string()),
            server_name: Some("origin.example.net".to_string()),
            peer_certificate_der: Some(vec![7, 8, 9]),
        });

        assert!(flow.handshake_complete());
        assert!(flow.plaintext_ready());
        assert!(flow.is_session_reused());
        assert_eq!(flow.phase_label(), "plaintext-ready");
        assert_eq!(flow.alpn(), "h2");
        assert_eq!(flow.peer_certificate_der(), Some(&[7, 8, 9][..]));
    }

    #[test]
    fn tls_session_cache_key_is_origin_scoped_and_configuration_sensitive() {
        let mut first = TlsFlowState::default();
        first.observe_target("https://origin.example.net:8443/a");
        first.set_verify_peer(false);
        first.set_desired_alpn(vec!["h2".to_string()]);

        let mut second = first.clone();
        second.observe_target("https://origin.example.net:8443/b");

        let first_key = tls_session_cache_key("https://origin.example.net:8443/a", &first)
            .expect("key should build");
        let second_key = tls_session_cache_key("https://origin.example.net:8443/b", &second)
            .expect("key should build");
        assert_eq!(first_key, second_key);

        second.set_verify_hostname(false);
        let changed_key = tls_session_cache_key("https://origin.example.net:8443/b", &second)
            .expect("key should build");
        assert_ne!(first_key, changed_key);
    }

    #[test]
    fn http_versions_map_to_expected_alpn_labels() {
        assert_eq!(
            alpn_from_http_version(Version::HTTP_09).as_deref(),
            Some("http/0.9")
        );
        assert_eq!(
            alpn_from_http_version(Version::HTTP_10).as_deref(),
            Some("http/1.0")
        );
        assert_eq!(
            alpn_from_http_version(Version::HTTP_11).as_deref(),
            Some("http/1.1")
        );
        assert_eq!(
            alpn_from_http_version(Version::HTTP_2).as_deref(),
            Some("h2")
        );
        assert_eq!(
            alpn_from_http_version(Version::HTTP_3).as_deref(),
            Some("h3")
        );
    }
}
