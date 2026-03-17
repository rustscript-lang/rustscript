use super::support::*;

#[cfg(feature = "tls")]
struct PermissiveTestServerCertVerifier {
    delegate: Arc<dyn rustls::client::danger::ServerCertVerifier>,
}

#[cfg(feature = "tls")]
impl std::fmt::Debug for PermissiveTestServerCertVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PermissiveTestServerCertVerifier")
    }
}

#[cfg(feature = "tls")]
impl PermissiveTestServerCertVerifier {
    fn new() -> Self {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let delegate = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
            .build()
            .expect("webpki verifier should build");
        Self { delegate }
    }
}

#[cfg(feature = "tls")]
impl rustls::client::danger::ServerCertVerifier for PermissiveTestServerCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.delegate.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.delegate.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.delegate.supported_verify_schemes()
    }
}

#[tokio::test]
async fn sample_upstream_transport_proxy_program_streams_plain_http_body() {
    let (_upstream_addr, upstream_handle) = spawn_chunked_upstream_on(
        vec!["ab", "cd", "ef"],
        loopback_addr(SAMPLE_TRANSPORT_UPSTREAM_HTTP_PORT),
    )
    .await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("transport")
        .join("upstream")
        .join("sample_upstream_transport_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/proxy"))
        .header("Streaming", "1")
        .send()
        .await
        .expect("streaming request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/plain")
    );
    assert_eq!(
        response
            .headers()
            .get("x-peer-name")
            .and_then(|value| value.to_str().ok()),
        None
    );
    assert_eq!(
        response
            .headers()
            .get("x-upload-pipe")
            .and_then(|value| value.to_str().ok()),
        Some("eof")
    );
    assert_eq!(
        response.text().await.expect("response body should read"),
        "abAcdAefA"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn sample_upstream_transport_proxy_program_handles_https_tls_session() {
    let (_upstream_addr, upstream_handle) =
        spawn_https_echo_upstream_on(loopback_addr(SAMPLE_TRANSPORT_UPSTREAM_HTTPS_PORT)).await;
    let mut state = SharedState::new(1024 * 1024);
    state.client = reqwest::Client::builder()
        .tls_info(true)
        .danger_accept_invalid_certs(true)
        .build()
        .expect("tls test client should build");
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy_with_state(state).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("transport")
        .join("upstream")
        .join("sample_upstream_transport_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/proxy"))
        .header("x-upstream-scheme", "https")
        .body("secure-payload")
        .send()
        .await
        .expect("https request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-stream")
            .and_then(|value| value.to_str().ok()),
        Some("false")
    );
    assert_eq!(
        response
            .headers()
            .get("x-peer-name")
            .and_then(|value| value.to_str().ok()),
        Some("localhost")
    );
    assert_eq!(
        response
            .headers()
            .get("x-upload-pipe")
            .and_then(|value| value.to_str().ok()),
        Some("eof")
    );
    assert_eq!(
        response
            .headers()
            .get("x-upstream-alpn")
            .and_then(|value| value.to_str().ok()),
        Some("http/1.1")
    );
    assert_eq!(
        response.text().await.expect("response body should read"),
        "secure-payload"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn sample_transport_tls_handshake_program_echoes_plaintext_after_tls_handshake() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    ensure_rustls_provider();

    let (data_addr, admin_addr, data_handle, admin_handle) =
        spawn_transport_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("transport")
        .join("tls")
        .join("sample_transport_tls_handshake_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PermissiveTestServerCertVerifier::new()))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"echo/1".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));

    let stream = tokio::net::TcpStream::connect(data_addr)
        .await
        .expect("transport proxy should accept tls");
    let server_name = rustls::pki_types::ServerName::try_from("edge.example.test")
        .expect("server name should parse")
        .to_owned();
    let mut tls_stream = connector
        .connect(server_name, stream)
        .await
        .expect("tls handshake should complete");
    assert_eq!(
        tls_stream
            .get_ref()
            .1
            .alpn_protocol()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned()),
        Some("echo/1".to_string())
    );

    let mut banner = vec![0u8; "accepted alpn=echo/1\n".len()];
    tls_stream
        .read_exact(&mut banner)
        .await
        .expect("tls banner should read");
    assert_eq!(
        String::from_utf8(banner).expect("banner should be utf8"),
        "accepted alpn=echo/1\n"
    );

    tls_stream
        .write_all(b"hello")
        .await
        .expect("tls payload should write");
    tls_stream
        .shutdown()
        .await
        .expect("tls write half-close should succeed");
    let mut echoed = Vec::new();
    tls_stream
        .read_to_end(&mut echoed)
        .await
        .expect("tls echo should read");
    assert_eq!(echoed, b"hello");

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn transport_downstream_preread_replays_into_raw_tcp_reads() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (data_addr, admin_addr, data_handle, admin_handle) =
        spawn_transport_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = r#"
        use tcp;

        let downstream = tcp::stream::downstream();
        let preview = tcp::stream::peek(downstream, 2);
        let payload = tcp::stream::read(downstream, 5);
        tcp::stream::write(downstream, preview + "|" + payload);
        tcp::stream::close(downstream);
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let mut stream = tokio::net::TcpStream::connect(data_addr)
        .await
        .expect("transport proxy should accept raw preread connection");
    stream
        .write_all(b"hello")
        .await
        .expect("raw payload should write");
    stream
        .shutdown()
        .await
        .expect("client write half-close should succeed");

    let mut echoed = Vec::new();
    stream
        .read_to_end(&mut echoed)
        .await
        .expect("raw response should read");
    assert_eq!(echoed, b"he|hello");

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn transport_downstream_http_handoff_continues_same_vm_invocation() {
    let (data_addr, admin_addr, data_handle, admin_handle) =
        spawn_transport_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = r#"
        use http;
        use tcp;

        http::response::set_header("x-pre-handoff", "still-here");

        let downstream = tcp::stream::downstream();
        if http::request::get_scheme() == "tcp" {
            http::downstream::attach_transport();
        }

        if http::request::get_scheme() != "tcp" {
            http::response::set_status(201);
            http::response::set_body(
                http::request::get_method() + "|" +
                http::request::get_path_with_query() + "|" +
                http::request::get_body()
            );
        }
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/inline"))
        .body("payload")
        .send()
        .await
        .expect("inline handoff request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response
            .headers()
            .get("x-pre-handoff")
            .and_then(|value| value.to_str().ok()),
        Some("still-here")
    );
    assert_eq!(
        response.text().await.expect("response body should read"),
        "POST|/inline|payload"
    );

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn transport_downstream_http_handoff_promotes_plain_connection_into_http11_runtime() {
    let (data_addr, admin_addr, data_handle, admin_handle) =
        spawn_transport_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("transport")
        .join("handoff")
        .join("sample_transport_http_handoff_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/promoted?mode=http1"))
        .body("payload")
        .send()
        .await
        .expect("promoted http1 request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response
            .headers()
            .get("x-request-scheme")
            .and_then(|value| value.to_str().ok()),
        Some("http")
    );
    assert_eq!(
        response
            .headers()
            .get("x-request-version")
            .and_then(|value| value.to_str().ok()),
        Some("1.1")
    );
    assert_eq!(
        response.text().await.expect("response body should read"),
        "POST|/promoted?mode=http1|payload"
    );

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn transport_downstream_http_handoff_promotes_plain_connection_into_http2_runtime() {
    use http_body_util::{BodyExt, Full};
    use hyper::Request;
    use hyper_util::rt::{TokioExecutor, TokioIo};

    let (data_addr, admin_addr, data_handle, admin_handle) =
        spawn_transport_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("transport")
        .join("handoff")
        .join("sample_transport_http_handoff_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let stream = tokio::net::TcpStream::connect(data_addr)
        .await
        .expect("transport proxy should accept promoted http2");
    let (mut sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake(TokioIo::new(stream))
        .await
        .expect("http2 handshake should succeed");
    let connection_task = tokio::spawn(async move {
        connection.await.expect("http2 connection should run");
    });

    let response = sender
        .send_request(
            Request::builder()
                .method("POST")
                .uri(format!("http://{data_addr}/promoted?mode=http2"))
                .version(axum::http::Version::HTTP_2)
                .header("host", format!("{data_addr}"))
                .body(Full::new(axum::body::Bytes::from_static(b"h2-payload")))
                .expect("http2 request should build"),
        )
        .await
        .expect("http2 request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response
            .headers()
            .get("x-request-scheme")
            .and_then(|value| value.to_str().ok()),
        Some("http")
    );
    assert_eq!(
        response
            .headers()
            .get("x-request-version")
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .expect("http2 body should collect")
            .to_bytes()
            .as_ref(),
        b"POST|/promoted?mode=http2|h2-payload"
    );

    drop(sender);
    connection_task.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn transport_downstream_http_handoff_promotes_tls_plaintext_into_https_runtime() {
    use http_body_util::{BodyExt, Full};
    use hyper::Request;
    use hyper_util::rt::TokioIo;

    ensure_rustls_provider();

    let (data_addr, admin_addr, data_handle, admin_handle) =
        spawn_transport_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("transport")
        .join("handoff")
        .join("sample_transport_http_handoff_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PermissiveTestServerCertVerifier::new()))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));

    let stream = tokio::net::TcpStream::connect(data_addr)
        .await
        .expect("transport proxy should accept promoted https");
    let server_name = rustls::pki_types::ServerName::try_from("localhost")
        .expect("server name should parse")
        .to_owned();
    let tls_stream = connector
        .connect(server_name, stream)
        .await
        .expect("downstream tls handshake should succeed");
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls_stream))
        .await
        .expect("http1 over tls handshake should succeed");
    let connection_task = tokio::spawn(async move {
        connection
            .await
            .expect("http1 over tls connection should run");
    });

    let response = sender
        .send_request(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "https://localhost:{}/promoted?mode=https",
                    data_addr.port()
                ))
                .header("host", format!("localhost:{}", data_addr.port()))
                .body(Full::new(axum::body::Bytes::from_static(b"secure-body")))
                .expect("https request should build"),
        )
        .await
        .expect("https request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response
            .headers()
            .get("x-request-scheme")
            .and_then(|value| value.to_str().ok()),
        Some("https")
    );
    assert_eq!(
        response
            .headers()
            .get("x-request-version")
            .and_then(|value| value.to_str().ok()),
        Some("1.1")
    );
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .expect("https body should collect")
            .to_bytes()
            .as_ref(),
        b"POST|/promoted?mode=https|secure-body"
    );

    connection_task.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn sample_http_proxy_tls_prelude_program_serves_http_and_https_listeners() {
    ensure_rustls_provider();

    let (http_addr, https_addr, admin_addr, http_handle, https_handle, admin_handle) =
        spawn_http_https_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("http")
        .join("proxy")
        .join("sample_http_proxy_tls_prelude_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let http_response = client
        .post(format!("http://{http_addr}/plain?mode=http"))
        .body("plain-body")
        .send()
        .await
        .expect("plain http request should complete");
    assert_eq!(http_response.status(), StatusCode::CREATED);
    assert_eq!(
        http_response
            .headers()
            .get("x-request-scheme")
            .and_then(|value| value.to_str().ok()),
        Some("http")
    );
    assert_eq!(
        http_response
            .text()
            .await
            .expect("plain response body should read"),
        "POST|/plain?mode=http|plain-body"
    );

    let plaintext_https_listener_response = client
        .post(format!("http://{https_addr}/plaintext-on-https-port"))
        .body("plaintext-body")
        .send()
        .await
        .expect("plaintext http on https listener should complete");
    assert_eq!(
        plaintext_https_listener_response.status(),
        StatusCode::CREATED
    );
    assert_eq!(
        plaintext_https_listener_response
            .headers()
            .get("x-request-scheme")
            .and_then(|value| value.to_str().ok()),
        Some("http")
    );
    assert_eq!(
        plaintext_https_listener_response
            .text()
            .await
            .expect("plaintext https-listener body should read"),
        "POST|/plaintext-on-https-port|plaintext-body"
    );

    let https_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("https test client should build");
    let https_response = https_client
        .post(format!(
            "https://localhost:{}/secure?mode=https",
            https_addr.port()
        ))
        .body("secure-body")
        .send()
        .await
        .expect("https request should complete");
    assert_eq!(https_response.status(), StatusCode::CREATED);
    assert_eq!(
        https_response
            .headers()
            .get("x-request-scheme")
            .and_then(|value| value.to_str().ok()),
        Some("https")
    );
    assert_eq!(
        https_response
            .text()
            .await
            .expect("https response body should read"),
        "POST|/secure?mode=https|secure-body"
    );

    let mut bad_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PermissiveTestServerCertVerifier::new()))
        .with_no_client_auth();
    bad_config.alpn_protocols = vec![b"imap".to_vec()];
    let bad_connector = tokio_rustls::TlsConnector::from(Arc::new(bad_config));
    let bad_stream = tokio::net::TcpStream::connect(https_addr)
        .await
        .expect("https listener should accept mismatched alpn tls");
    let bad_name = rustls::pki_types::ServerName::try_from("localhost")
        .expect("server name should parse")
        .to_owned();
    let bad_read = timeout(
        Duration::from_secs(1),
        bad_connector.connect(bad_name, bad_stream),
    )
    .await
    .expect("mismatched alpn connection should terminate promptly");
    assert!(
        bad_read.is_err(),
        "expected alpn-mismatched tls handshake to fail, got {bad_read:?}"
    );

    http_handle.abort();
    https_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn sample_tunnel_proxy_program_tunnels_plain_http_body() {
    let upstream_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let body = to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        Response::new(Body::from(format!(
            "tunnel:{}",
            String::from_utf8_lossy(&body)
        )))
    }));
    let (_upstream_addr, upstream_handle) = spawn_server_on(
        upstream_app,
        loopback_addr(SAMPLE_TUNNEL_UPSTREAM_HTTP_PORT),
    )
    .await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("proxy")
        .join("tunnel")
        .join("sample_tunnel_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/tunnel"))
        .body("abcdefghij")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-proxy-status")
            .and_then(|value| value.to_str().ok()),
        Some("forwarded")
    );
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        None
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "tunnel:abcdefghij"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn transport_downstream_socket_phase_closes_after_http_body_is_buffered() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = r#"
        use http;
        use tcp;

        let body = http::request::get_body();
        let downstream = tcp::stream::downstream();
        http::response::set_header("x-phase", tcp::stream::get_phase(downstream));
        http::response::set_body(body);
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/body-phase"))
        .body("payload")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-phase")
            .and_then(|value| value.to_str().ok()),
        Some("closed")
    );
    assert_eq!(response.text().await.expect("body should read"), "payload");

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn transport_connect_tunnel_upgrades_downstream_into_dynamic_tcp() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (upstream_addr, upstream_handle) =
        edge::sample_echo::spawn_tcp_echo_server("127.0.0.1:0".parse().expect("valid addr"))
            .await
            .expect("tcp echo should start");
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = format!(
        r#"
        use http;
        use proxy;
        use tcp;

        let upstream = tcp::stream::new();
        tcp::stream::set_target(upstream, "{upstream_host}", {upstream_port});
        if !tcp::stream::connect(upstream) {{
            http::response::set_status(502);
            http::response::set_body("tcp connect failed");
        }} else {{
            let downstream = proxy::stream::downstream();
            let peer = proxy::stream::from_tcp(upstream);
            let status = proxy::bridge(downstream, peer, 64);
            http::response::set_header("x-proxy-status", status);
        }}
    "#,
        upstream_host = upstream_addr.ip(),
        upstream_port = upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let mut stream = tokio::net::TcpStream::connect(data_addr)
        .await
        .expect("proxy should accept connect requests");
    let request = format!("CONNECT tunnel HTTP/1.1\r\nHost: {data_addr}\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("connect request should write");

    let mut response = Vec::new();
    let mut buffer = [0u8; 512];
    loop {
        let read = stream
            .read(&mut buffer)
            .await
            .expect("connect response should read");
        assert!(read > 0, "connect response should not close early");
        response.extend_from_slice(&buffer[..read]);
        if response.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let response_text = String::from_utf8_lossy(&response);
    assert!(
        response_text.starts_with("HTTP/1.1 200 OK"),
        "unexpected connect response: {response_text}"
    );
    assert!(
        response_text.contains("x-proxy-status: upgraded"),
        "missing upgraded header: {response_text}"
    );

    stream
        .write_all(b"hello-through-edge")
        .await
        .expect("payload should write through tunnel");
    let mut echoed = [0u8; 128];
    let read = stream.read(&mut echoed).await.expect("echo should read");
    assert_eq!(&echoed[..read], b"hello-through-edge");

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn sample_tunnel_proxy_program_tunnels_https_body_via_tls_plaintext_stream() {
    let (_upstream_addr, upstream_handle) =
        spawn_https_echo_upstream_on(loopback_addr(SAMPLE_TUNNEL_UPSTREAM_HTTPS_PORT)).await;
    let mut state = SharedState::new(1024 * 1024);
    state.client = reqwest::Client::builder()
        .tls_info(true)
        .danger_accept_invalid_certs(true)
        .build()
        .expect("tls test client should build");
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy_with_state(state).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("proxy")
        .join("tunnel")
        .join("sample_tunnel_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/tls-tunnel"))
        .header("x-upstream-scheme", "https")
        .body("secure-payload")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-proxy-status")
            .and_then(|value| value.to_str().ok()),
        Some("forwarded")
    );
    assert_eq!(
        response
            .headers()
            .get("x-peer-name")
            .and_then(|value| value.to_str().ok()),
        Some("localhost")
    );
    assert_eq!(
        response
            .headers()
            .get("x-upstream-alpn")
            .and_then(|value| value.to_str().ok()),
        Some("http/1.1")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "secure-payload"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn proxy_pipe_forwards_dynamic_exchange_response_via_proxy_stream_handle() {
    let upstream_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let body = to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        Response::new(Body::from(format!(
            "dynamic:{}",
            String::from_utf8_lossy(&body)
        )))
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use proxy;

        let exchange = http::exchange::new();
        http::exchange::set_target(exchange, "{upstream_host}", {upstream_port});
        http::exchange::set_path(exchange, "/dynamic");
        http::exchange::set_body(exchange, "payload");

        let response = proxy::stream::exchange(exchange);
        let downstream = proxy::stream::downstream();
        let status = proxy::pipe(response, downstream, 5);
        http::response::set_header("x-proxy-status", status);
        http::response::set_status(http::exchange::get_status(exchange));

        let content_type = http::exchange::get_header(exchange, "content-type");
        if content_type != "" {{
            http::response::set_header("content-type", content_type);
        }}
    "#,
        upstream_host = upstream_addr.ip(),
        upstream_port = upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/dynamic-proxy"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-proxy-status")
            .and_then(|value| value.to_str().ok()),
        Some("eof")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "dynamic:payload"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn transport_connect_tunnel_upgrades_downstream_into_dynamic_tls_plaintext() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (upstream_addr, upstream_handle) = spawn_https_echo_upstream().await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = format!(
        r#"
        use http;
        use proxy;
        use tcp;
        use tls;

        let upstream = tcp::stream::new();
        tcp::stream::set_target(upstream, "localhost", {upstream_port});
        if !tcp::stream::connect(upstream) {{
            http::response::set_status(502);
            http::response::set_body("tcp connect failed");
        }} else {{
            let session = tls::session::from_socket(upstream);
            tls::session::set_verify(session, false);
            tls::session::set_verify_hostname(session, false);
            if !tls::session::handshake(session) {{
                http::response::set_status(502);
                http::response::set_body("tls handshake failed");
            }} else {{
                let downstream = proxy::stream::downstream();
                let peer = proxy::stream::from_tls_plaintext(session);
                let status = proxy::bridge(downstream, peer, 256);
                http::response::set_header("x-proxy-status", status);
                http::response::set_header("x-tls-phase", tls::session::get_phase(session));
            }}
        }}
    "#,
        upstream_port = upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let mut stream = tokio::net::TcpStream::connect(data_addr)
        .await
        .expect("proxy should accept connect requests");
    let request = format!("CONNECT secure HTTP/1.1\r\nHost: {data_addr}\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("connect request should write");

    let mut response = Vec::new();
    let mut buffer = [0u8; 512];
    loop {
        let read = stream
            .read(&mut buffer)
            .await
            .expect("connect response should read");
        assert!(read > 0, "connect response should not close early");
        response.extend_from_slice(&buffer[..read]);
        if response.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let response_text = String::from_utf8_lossy(&response);
    assert!(
        response_text.starts_with("HTTP/1.1 200 OK"),
        "unexpected connect response: {response_text}"
    );
    assert!(
        response_text.contains("x-proxy-status: upgraded"),
        "missing upgraded header: {response_text}"
    );
    assert!(
        response_text.contains("x-tls-phase: plaintext-ready"),
        "missing tls phase header: {response_text}"
    );

    let tunneled_request = format!(
        "POST /echo HTTP/1.1\r\nHost: localhost:{}\r\nContent-Length: 5\r\n\r\nhello",
        upstream_addr.port()
    );
    stream
        .write_all(tunneled_request.as_bytes())
        .await
        .expect("tunneled plaintext request should write");

    let mut tunneled_response = Vec::new();
    loop {
        let read = stream
            .read(&mut buffer)
            .await
            .expect("tunneled response should read");
        if read == 0 {
            break;
        }
        tunneled_response.extend_from_slice(&buffer[..read]);
        if tunneled_response
            .windows(5)
            .any(|window| window == b"hello")
        {
            break;
        }
    }
    let tunneled_text = String::from_utf8_lossy(&tunneled_response);
    assert!(
        tunneled_text.contains("HTTP/1.1 200 OK"),
        "unexpected tunneled response: {tunneled_text}"
    );
    assert!(
        tunneled_text.contains("hello"),
        "missing echoed payload in tunneled response: {tunneled_text}"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn transport_downstream_tls_session_reflects_forwarded_https_metadata() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = r#"
        use http;
        use tcp;
        use tls;

        let sock = tcp::stream::downstream();
        let session = tls::session::from_socket(sock);
        if tls::session::is_present(session) {
            http::response::set_header("x-tls", "true");
            http::response::set_header("x-alpn", tls::session::get_alpn(session));
        } else {
            http::response::set_header("x-tls", "false");
        }
        http::response::set_body("ok");
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/tls"))
        .header("x-forwarded-proto", "https")
        .header("host", "app.example.test:443")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-tls")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        response
            .headers()
            .get("x-alpn")
            .and_then(|value| value.to_str().ok()),
        Some("http/1.1")
    );
    assert_eq!(response.text().await.expect("body should read"), "ok");

    data_handle.abort();
    admin_handle.abort();
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn transport_dynamic_tls_session_can_handshake_against_socket_target() {
    let (upstream_addr, upstream_handle) = spawn_https_echo_upstream().await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = format!(
        r#"
        use http;
        use tcp;
        use tls;

        let stream = tcp::stream::new();
        tcp::stream::set_target(stream, "localhost", {});
        if !tcp::stream::connect(stream) {{
            http::response::set_status(502);
            http::response::set_body("connect failed");
        }} else {{
            let session = tls::session::from_socket(stream);
            tls::session::set_verify(session, false);
            tls::session::set_verify_hostname(session, false);
            if !tls::session::handshake(session) {{
                http::response::set_status(502);
                http::response::set_body("handshake failed");
            }} else {{
                http::response::set_header("x-phase", tls::session::get_phase(session));
                http::response::set_header("x-peer-name", tls::session::get_peer_name(session));
                http::response::set_header("x-stream-phase", tcp::stream::get_phase(stream));
                http::response::set_body("ok");
            }}
        }}
    "#,
        upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/dynamic-tls-socket-target"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-phase")
            .and_then(|value| value.to_str().ok()),
        Some("plaintext-ready")
    );
    assert_eq!(
        response
            .headers()
            .get("x-peer-name")
            .and_then(|value| value.to_str().ok()),
        Some("localhost")
    );
    assert_eq!(
        response
            .headers()
            .get("x-stream-phase")
            .and_then(|value| value.to_str().ok()),
        Some("upgraded-tls")
    );
    assert_eq!(response.text().await.expect("body should read"), "ok");

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn transport_dynamic_tls_session_socket_target_direct_vm_run() {
    let (upstream_addr, upstream_handle) = spawn_https_echo_upstream().await;
    let source = format!(
        r#"
        use http;
        use tcp;
        use tls;

        let stream = tcp::stream::new();
        tcp::stream::set_target(stream, "localhost", {});
        if !tcp::stream::connect(stream) {{
            http::response::set_status(502);
            http::response::set_body("connect failed");
        }} else {{
            let session = tls::session::from_socket(stream);
            tls::session::set_verify(session, false);
            tls::session::set_verify_hostname(session, false);
            if !tls::session::handshake(session) {{
                http::response::set_status(502);
                http::response::set_body("handshake failed");
            }} else {{
                http::response::set_body("ok");
            }}
        }}
    "#,
        upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let context = Arc::new(ProxyVmContext::from_request_headers(
        axum::http::HeaderMap::new(),
        Arc::new(RateLimiterStore::new()),
    ));

    run_edge_program_direct(compiled.program, context)
        .await
        .expect("direct vm dynamic tls run should succeed");

    upstream_handle.abort();
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn transport_downstream_tls_phase_closes_after_http_body_is_buffered() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = r#"
        use http;
        use tcp;
        use tls;

        let body = http::request::get_body();
        let downstream = tcp::stream::downstream();
        let session = tls::session::from_socket(downstream);
        http::response::set_header("x-tcp-phase", tcp::stream::get_phase(downstream));
        http::response::set_header("x-tls-phase", tls::session::get_phase(session));
        http::response::set_body(body);
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/tls-body-phase"))
        .header("x-forwarded-proto", "https")
        .header("host", "app.example.test:443")
        .body("payload")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-tcp-phase")
            .and_then(|value| value.to_str().ok()),
        Some("closed")
    );
    assert_eq!(
        response
            .headers()
            .get("x-tls-phase")
            .and_then(|value| value.to_str().ok()),
        Some("closed")
    );
    assert_eq!(response.text().await.expect("body should read"), "payload");

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn transport_default_upstream_socket_accepts_multiple_writes_before_exchange() {
    let upstream_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let body = to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        Response::new(Body::from(String::from_utf8_lossy(&body).into_owned()))
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tcp;

        let upstream_exchange = http::exchange::default_upstream();
        http::exchange::set_target(upstream_exchange, "{upstream_host}", {upstream_port});
        let downstream = tcp::stream::downstream();
        let upstream = tcp::stream::default_upstream();
        while !tcp::stream::eof(downstream) {{
            let chunk = tcp::stream::read(downstream, 3);
            if chunk != "" {{
                tcp::stream::write(upstream, chunk);
            }}
        }}
        http::response::set_body(http::exchange::get_body(upstream_exchange));
    "#,
        upstream_host = upstream_addr.ip(),
        upstream_port = upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/echo"))
        .body("abcdefghij")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.text().await.expect("body should read"),
        "abcdefghij"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn transport_default_upstream_socket_rejects_write_after_response_has_started() {
    let upstream_app = Router::new().fallback(any(|_request: Request<Body>| async move {
        Response::new(Body::from("upstream"))
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tcp;

        let upstream_exchange = http::exchange::default_upstream();
        http::exchange::set_target(upstream_exchange, "{upstream_host}", {upstream_port});
        let upstream = tcp::stream::default_upstream();
        http::exchange::get_status(upstream_exchange);
        tcp::stream::write(upstream, "late");
    "#,
        upstream_host = upstream_addr.ip(),
        upstream_port = upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/late-write"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn downstream_transport_proxy_exposes_raw_tcp_read_and_write() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (data_addr, admin_addr, data_handle, admin_handle) =
        spawn_transport_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = r#"
        use tcp;

        let downstream = tcp::stream::downstream();
        let body = tcp::stream::read(downstream, 5);
        tcp::stream::write(downstream, body);
        tcp::stream::close(downstream);
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let mut stream = tokio::net::TcpStream::connect(data_addr)
        .await
        .expect("transport proxy should accept tcp");
    stream
        .write_all(b"hello")
        .await
        .expect("payload should write");
    stream
        .shutdown()
        .await
        .expect("write half-close should succeed");
    let mut echoed = Vec::new();
    stream
        .read_to_end(&mut echoed)
        .await
        .expect("echo should read");
    assert_eq!(echoed, b"hello");

    data_handle.abort();
    admin_handle.abort();
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn downstream_transport_proxy_completes_tls_handshake_and_echoes_payload() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    ensure_rustls_provider();

    let (data_addr, admin_addr, data_handle, admin_handle) =
        spawn_transport_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = r#"
        use tcp;
        use tls;

        let downstream = tcp::stream::downstream();
        let session = tls::session::from_socket(downstream);
        tls::session::set_alpn(session, "echo/1");
        if tls::session::handshake(session) {
            let body = tcp::stream::read(downstream, 5);
            tcp::stream::write(downstream, body);
            tcp::stream::close(downstream);
        }
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PermissiveTestServerCertVerifier::new()))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"echo/1".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));

    let stream = tokio::net::TcpStream::connect(data_addr)
        .await
        .expect("transport proxy should accept tls");
    let server_name = rustls::pki_types::ServerName::try_from("allowed.example.test")
        .expect("server name should parse")
        .to_owned();
    let mut tls_stream = connector
        .connect(server_name, stream)
        .await
        .expect("tls handshake should complete");
    assert_eq!(
        tls_stream
            .get_ref()
            .1
            .alpn_protocol()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned()),
        Some("echo/1".to_string())
    );
    tls_stream
        .write_all(b"hello")
        .await
        .expect("tls payload should write");
    tls_stream
        .shutdown()
        .await
        .expect("tls write half-close should succeed");
    let mut echoed = Vec::new();
    tls_stream
        .read_to_end(&mut echoed)
        .await
        .expect("tls echo should read");
    assert_eq!(echoed, b"hello");

    data_handle.abort();
    admin_handle.abort();
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn downstream_transport_proxy_reuses_default_self_signed_certificate() {
    ensure_rustls_provider();

    let (data_addr, admin_addr, data_handle, admin_handle) =
        spawn_transport_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = r#"
        use tcp;
        use tls;

        let downstream = tcp::stream::downstream();
        let session = tls::session::from_socket(downstream);
        tls::session::set_alpn(session, "echo/1");
        if tls::session::handshake(session) {
            tcp::stream::close(downstream);
        }
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PermissiveTestServerCertVerifier::new()))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"echo/1".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));

    let server_name = rustls::pki_types::ServerName::try_from("localhost")
        .expect("server name should parse")
        .to_owned();

    let first_stream = tokio::net::TcpStream::connect(data_addr)
        .await
        .expect("first tls client should connect");
    let first = connector
        .connect(server_name.clone(), first_stream)
        .await
        .expect("first tls handshake should complete");
    let first_cert = first
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certificates| certificates.first().cloned())
        .expect("first handshake should expose peer certificate")
        .to_vec();
    drop(first);

    let second_stream = tokio::net::TcpStream::connect(data_addr)
        .await
        .expect("second tls client should connect");
    let second = connector
        .connect(server_name, second_stream)
        .await
        .expect("second tls handshake should complete");
    let second_cert = second
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certificates| certificates.first().cloned())
        .expect("second handshake should expose peer certificate")
        .to_vec();

    assert_eq!(
        first_cert, second_cert,
        "default downstream self-signed certificate should be reused across requests"
    );

    data_handle.abort();
    admin_handle.abort();
}
