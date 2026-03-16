#![cfg_attr(not(feature = "http3"), allow(dead_code))]

use std::{io, io::BufReader, sync::Arc};

#[cfg(feature = "http3")]
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
#[cfg(feature = "http3")]
use rustls::{
    self, ClientConfig, RootCertStore, ServerConfig, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime},
    version::TLS13,
};
#[cfg(feature = "http3")]
use socket2::SockRef;

use crate::abi_impl::transport::{TlsFlowState, TlsProtocolVersion};

pub(crate) const ALPN_PROTOCOL: &str = "h3";
#[cfg(feature = "http3")]
const QUIC_SOCKET_BUFFER_BYTES: usize = 4 * 1024 * 1024;
#[cfg(feature = "http3")]
const QUIC_STREAM_RECEIVE_WINDOW_BYTES: u32 = 8 * 1024 * 1024;
#[cfg(feature = "http3")]
const QUIC_CONNECTION_RECEIVE_WINDOW_BYTES: u32 = 32 * 1024 * 1024;
#[cfg(feature = "http3")]
const QUIC_SEND_WINDOW_BYTES: u64 = 32 * 1024 * 1024;
#[cfg(feature = "http3")]
const QUIC_KEEPALIVE_INTERVAL_MS: u64 = 5_000;
#[cfg(feature = "http3")]
const QUIC_MAX_CONCURRENT_BIDI_STREAMS: u32 = 1024;

#[cfg(feature = "http3")]
pub(crate) fn ensure_rustls_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

#[cfg(feature = "http3")]
pub(crate) fn build_quic_client_config(
    tls_flow: &TlsFlowState,
) -> Result<quinn::ClientConfig, String> {
    ensure_rustls_provider();
    let builder =
        ClientConfig::builder_with_provider(rustls::crypto::aws_lc_rs::default_provider().into())
            .with_protocol_versions(&protocol_versions_for_http3(tls_flow)?)
            .map_err(|err| format!("failed to configure QUIC TLS versions: {err}"))?;

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
                .map_err(|err| format!("failed to configure QUIC client certificate: {err}"))?,
            (None, None) => builder
                .with_root_certificates(build_root_store(tls_flow)?)
                .with_no_client_auth(),
            _ => {
                return Err(
                    "client certificate and private key must both be configured".to_string()
                );
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
                .map_err(|err| format!("failed to configure permissive QUIC client auth: {err}"))?,
            (None, None) => builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(PermissiveServerCertVerifier::new(
                    roots,
                )))
                .with_no_client_auth(),
            _ => {
                return Err(
                    "client certificate and private key must both be configured".to_string()
                );
            }
        }
    };

    config.enable_sni = tls_flow.sni_enabled();
    config.alpn_protocols = alpn_protocols_for_http3(tls_flow);
    let mut client = quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(config)
            .map_err(|err| format!("failed to wrap QUIC client config: {err}"))?,
    ));
    client.transport_config(Arc::new(default_quic_transport_config()));
    Ok(client)
}

#[cfg(feature = "http3")]
pub(crate) fn build_quic_server_config(
    certificate_chain: Vec<CertificateDer<'static>>,
    private_key: PrivateKeyDer<'static>,
    alpn_protocols: Vec<Vec<u8>>,
) -> io::Result<quinn::ServerConfig> {
    ensure_rustls_provider();
    let mut server_crypto =
        ServerConfig::builder_with_provider(rustls::crypto::aws_lc_rs::default_provider().into())
            .with_protocol_versions(&[&TLS13])
            .map_err(|err| {
                io::Error::other(format!("failed to configure QUIC TLS versions: {err}"))
            })?
            .with_no_client_auth()
            .with_single_cert(certificate_chain, private_key)
            .map_err(|err| {
                io::Error::other(format!(
                    "failed to configure QUIC server certificate: {err}"
                ))
            })?;
    server_crypto.alpn_protocols = alpn_protocols;
    let quic_crypto = QuicServerConfig::try_from(server_crypto)
        .map_err(|err| io::Error::other(format!("failed to wrap QUIC server config: {err}")))?;
    let mut server = quinn::ServerConfig::with_crypto(Arc::new(quic_crypto));
    server.transport_config(Arc::new(default_quic_transport_config()));
    Ok(server)
}

#[cfg(feature = "http3")]
pub(crate) fn tune_udp_socket_buffers(socket: &std::net::UdpSocket) -> io::Result<()> {
    let sock_ref = SockRef::from(socket);
    sock_ref.set_recv_buffer_size(QUIC_SOCKET_BUFFER_BYTES)?;
    sock_ref.set_send_buffer_size(QUIC_SOCKET_BUFFER_BYTES)?;
    Ok(())
}

#[cfg(feature = "http3")]
fn default_quic_transport_config() -> quinn::TransportConfig {
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(
        quinn::VarInt::from_u32(QUIC_MAX_CONCURRENT_BIDI_STREAMS).into(),
    );
    transport
        .stream_receive_window(quinn::VarInt::from_u32(QUIC_STREAM_RECEIVE_WINDOW_BYTES).into());
    transport.receive_window(quinn::VarInt::from_u32(QUIC_CONNECTION_RECEIVE_WINDOW_BYTES).into());
    transport.send_window(QUIC_SEND_WINDOW_BYTES);
    transport.keep_alive_interval(Some(std::time::Duration::from_millis(
        QUIC_KEEPALIVE_INTERVAL_MS,
    )));
    transport
}

#[cfg(feature = "http3")]
pub(crate) fn negotiated_alpn(connection: &quinn::Connection) -> Option<String> {
    connection
        .handshake_data()
        .and_then(|data| data.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
        .and_then(|data| data.protocol.clone())
        .map(|protocol| String::from_utf8_lossy(&protocol).to_string())
}

#[cfg(feature = "http3")]
pub(crate) fn peer_certificate_der(connection: &quinn::Connection) -> Option<Vec<u8>> {
    connection
        .peer_identity()
        .and_then(|identity| identity.downcast::<Vec<CertificateDer<'static>>>().ok())
        .and_then(|certs| certs.first().map(|cert| cert.as_ref().to_vec()))
}

#[cfg(feature = "http3")]
pub(crate) fn protocol_versions_for_http3(
    tls_flow: &TlsFlowState,
) -> Result<Vec<&'static rustls::SupportedProtocolVersion>, String> {
    if matches!(
        tls_flow.min_version(),
        Some(TlsProtocolVersion::Tls1_0)
            | Some(TlsProtocolVersion::Tls1_1)
            | Some(TlsProtocolVersion::Tls1_2)
    ) || matches!(
        tls_flow.max_version(),
        Some(TlsProtocolVersion::Tls1_0)
            | Some(TlsProtocolVersion::Tls1_1)
            | Some(TlsProtocolVersion::Tls1_2)
    ) {
        return Err("http3 over QUIC requires TLS 1.3".to_string());
    }
    if let (Some(min_version), Some(max_version)) = (tls_flow.min_version(), tls_flow.max_version())
        && min_version > max_version
    {
        return Err("http3 TLS min version cannot be greater than max version".to_string());
    }
    Ok(vec![&TLS13])
}

#[cfg(feature = "http3")]
pub(crate) fn build_root_store(tls_flow: &TlsFlowState) -> Result<RootCertStore, String> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(pem) = tls_flow.trusted_certificate_pem() {
        let certificates = rustls_pemfile::certs(&mut BufReader::new(pem.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| format!("failed to parse trusted certificates for QUIC: {err}"))?;
        let (added, _ignored) = roots.add_parsable_certificates(certificates);
        if added == 0 {
            return Err(
                "trusted certificate bundle did not contain any usable certificates".to_string(),
            );
        }
    }
    Ok(roots)
}

#[cfg(feature = "http3")]
pub(crate) fn load_client_cert_chain(
    pem: Option<&str>,
) -> Result<Vec<CertificateDer<'static>>, String> {
    let Some(pem) = pem else {
        return Err("client certificate is unavailable".to_string());
    };
    let certificates = rustls_pemfile::certs(&mut BufReader::new(pem.as_bytes()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("failed to parse client certificate: {err}"))?;
    if certificates.is_empty() {
        return Err("client certificate chain is empty".to_string());
    }
    Ok(certificates)
}

#[cfg(feature = "http3")]
pub(crate) fn load_client_private_key(pem: Option<&str>) -> Result<PrivateKeyDer<'static>, String> {
    let Some(pem) = pem else {
        return Err("client private key is unavailable".to_string());
    };
    rustls_pemfile::private_key(&mut BufReader::new(pem.as_bytes()))
        .map_err(|err| format!("failed to parse client private key: {err}"))?
        .ok_or_else(|| "client private key is unavailable".to_string())
}

#[cfg(feature = "http3")]
pub(crate) fn alpn_protocols_for_http3(tls_flow: &TlsFlowState) -> Vec<Vec<u8>> {
    if !tls_flow.desired_alpn().is_empty() {
        return tls_flow
            .desired_alpn()
            .iter()
            .map(|protocol| protocol.as_bytes().to_vec())
            .collect();
    }
    vec![ALPN_PROTOCOL.as_bytes().to_vec()]
}

#[cfg(feature = "http3")]
struct PermissiveServerCertVerifier {
    delegate: Arc<dyn ServerCertVerifier>,
}

#[cfg(feature = "http3")]
impl std::fmt::Debug for PermissiveServerCertVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PermissiveServerCertVerifier")
    }
}

#[cfg(feature = "http3")]
impl PermissiveServerCertVerifier {
    fn new(roots: RootCertStore) -> Self {
        let delegate = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
            .build()
            .expect("webpki verifier should build");
        Self { delegate }
    }
}

#[cfg(feature = "http3")]
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
