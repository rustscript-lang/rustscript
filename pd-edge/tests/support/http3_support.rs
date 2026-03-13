#![cfg(feature = "http3")]

use std::{sync::Arc, time::Duration};

use axum::body::Bytes;
use hyper::body::Buf;
use rustls::{
    self, ClientConfig, RootCertStore, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use tokio::net::lookup_host;
use url::Url;

#[derive(Debug)]
pub(crate) struct Http3TestResponse {
    pub(crate) status: axum::http::StatusCode,
    pub(crate) version: axum::http::Version,
    pub(crate) headers: axum::http::HeaderMap,
    pub(crate) body: Bytes,
}

pub(crate) async fn send_http3_request(
    url: &str,
    method: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Http3TestResponse {
    ensure_rustls_provider();

    let url = Url::parse(url).expect("http3 test URL should parse");
    let host = url.host_str().expect("http3 test URL should include host");
    let port = url
        .port_or_known_default()
        .expect("http3 test URL should include port");
    let remotes = lookup_host((host, port))
        .await
        .expect("http3 test host should resolve")
        .collect::<Vec<_>>();
    let remote = remotes
        .iter()
        .copied()
        .find(std::net::SocketAddr::is_ipv4)
        .or_else(|| remotes.first().copied())
        .expect("http3 test host should return at least one address");
    let bind_addr = if remote.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    }
    .parse()
    .expect("wildcard bind addr should parse");

    let mut endpoint =
        quinn::Endpoint::client(bind_addr).expect("http3 test endpoint should build");
    endpoint.set_default_client_config(build_quic_client_config());

    let connection = endpoint
        .connect(remote, host)
        .expect("http3 test connect should start")
        .await
        .expect("http3 test connect should succeed");
    let h3_connection = h3_quinn::Connection::new(connection.clone());
    let (driver, mut sender) = h3::client::new(h3_connection)
        .await
        .expect("http3 test connection should initialize");
    let driver_handle = tokio::spawn(async move {
        let mut driver = driver;
        let _ = futures_util::future::poll_fn(|cx| driver.poll_close(cx)).await;
    });

    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(url.as_str());
    let mut has_host = false;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("host") {
            has_host = true;
        }
        request = request.header(*name, *value);
    }
    if !has_host {
        let authority = if let Some(port) = url.port() {
            format!("{host}:{port}")
        } else {
            host.to_string()
        };
        request = request.header(axum::http::header::HOST, authority);
    }
    let request = request.body(()).expect("http3 test request should build");

    let mut stream = sender
        .send_request(request)
        .await
        .expect("http3 test request stream should open");
    if !body.is_empty() {
        stream
            .send_data(Bytes::copy_from_slice(body))
            .await
            .expect("http3 test request body should send");
    }
    stream
        .finish()
        .await
        .expect("http3 test request should finish");

    let response = stream
        .recv_response()
        .await
        .expect("http3 test response head should arrive");
    let status = response.status();
    let version = response.version();
    let response_headers = response.headers().clone();

    let mut response_body = Vec::new();
    while let Some(mut chunk) = stream
        .recv_data()
        .await
        .expect("http3 test response body should read")
    {
        response_body.extend_from_slice(chunk.copy_to_bytes(chunk.remaining()).as_ref());
    }

    connection.close(0_u32.into(), b"done");
    drop(endpoint);
    tokio::time::sleep(Duration::from_millis(10)).await;
    driver_handle.abort();

    Http3TestResponse {
        status,
        version,
        headers: response_headers,
        body: Bytes::from(response_body),
    }
}

fn ensure_rustls_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

fn build_quic_client_config() -> quinn::ClientConfig {
    let builder =
        ClientConfig::builder_with_provider(rustls::crypto::aws_lc_rs::default_provider().into())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("http3 test TLS versions should configure");
    let mut config = builder
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PermissiveServerCertVerifier::new()))
        .with_no_client_auth();
    config.enable_sni = true;
    config.alpn_protocols = vec![b"h3".to_vec()];

    let quic_config = quinn::crypto::rustls::QuicClientConfig::try_from(config)
        .expect("http3 test QUIC client config should build");
    quinn::ClientConfig::new(Arc::new(quic_config))
}

struct PermissiveServerCertVerifier {
    delegate: Arc<dyn ServerCertVerifier>,
}

impl std::fmt::Debug for PermissiveServerCertVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PermissiveServerCertVerifier")
    }
}

impl PermissiveServerCertVerifier {
    fn new() -> Self {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let delegate = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
            .build()
            .expect("http3 test verifier should build");
        Self { delegate }
    }
}

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
