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
async fn sample_transport_proxy_program_streams_plain_http_body() {
    let (upstream_addr, upstream_handle) = spawn_chunked_upstream(vec!["ab", "cd", "ef"]).await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("sample_transport_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let upstream_target = format!("http://{upstream_addr}/sample");
    let response = client
        .get(format!("http://{data_addr}/proxy"))
        .header("x-upstream-target", &upstream_target)
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
async fn sample_transport_proxy_program_handles_https_tls_session() {
    let (upstream_addr, upstream_handle) = spawn_https_echo_upstream().await;
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
        .join("sample_transport_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let upstream_target = format!("https://localhost:{}/echo", upstream_addr.port());
    let response = client
        .post(format!("http://{data_addr}/proxy"))
        .header("x-upstream-target", &upstream_target)
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
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("sample_tunnel_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/tunnel"))
        .header("x-upstream-target", format!("http://{upstream_addr}/echo"))
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
        Some("closed")
    );
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/plain")
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

    let source = r#"
        use http;
        use proxy;
        use tcp;

        let target = http::request::get_header("x-connect-target");
        if target == "" {
            http::response::set_status(400);
            http::response::set_body("missing x-connect-target");
        } else {
            let upstream = tcp::stream::new();
            tcp::stream::set_target(upstream, target);
            if !tcp::stream::connect(upstream) {
                http::response::set_status(502);
                http::response::set_body("tcp connect failed");
            } else {
                let downstream = proxy::stream::downstream();
                let peer = proxy::stream::from_tcp(upstream);
                let status = proxy::tunnel(downstream, peer, 64);
                http::response::set_header("x-proxy-status", status);
            }
        }
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let mut stream = tokio::net::TcpStream::connect(data_addr)
        .await
        .expect("proxy should accept connect requests");
    let request = format!(
        "CONNECT tunnel HTTP/1.1\r\nHost: {data_addr}\r\nx-connect-target: {upstream_addr}\r\n\r\n"
    );
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
    let (upstream_addr, upstream_handle) = spawn_https_echo_upstream().await;
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
        .join("sample_tunnel_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/tls-tunnel"))
        .header(
            "x-upstream-target",
            format!("https://localhost:{}/echo", upstream_addr.port()),
        )
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
        Some("closed")
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
        http::exchange::set_target(exchange, "http://{upstream_addr}/dynamic");
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
    "#
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

    let source = r#"
        use http;
        use proxy;
        use tcp;
        use tls;

        let target = http::request::get_header("x-connect-target");
        if target == "" {
            http::response::set_status(400);
            http::response::set_body("missing x-connect-target");
        } else {
            let upstream = tcp::stream::new();
            tcp::stream::set_target(upstream, target);
            if !tcp::stream::connect(upstream) {
                http::response::set_status(502);
                http::response::set_body("tcp connect failed");
            } else {
                let session = tls::session::from_socket(upstream);
                tls::session::set_verify(session, false);
                tls::session::set_verify_hostname(session, false);
                if !tls::session::handshake(session) {
                    http::response::set_status(502);
                    http::response::set_body("tls handshake failed");
                } else {
                    let downstream = proxy::stream::downstream();
                    let peer = proxy::stream::from_tls_plaintext(session);
                    let status = proxy::tunnel(downstream, peer, 256);
                    http::response::set_header("x-proxy-status", status);
                    http::response::set_header("x-tls-phase", tls::session::get_phase(session));
                }
            }
        }
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let mut stream = tokio::net::TcpStream::connect(data_addr)
        .await
        .expect("proxy should accept connect requests");
    let connect_target = format!("localhost:{}", upstream_addr.port());
    let request = format!(
        "CONNECT secure HTTP/1.1\r\nHost: {data_addr}\r\nx-connect-target: {connect_target}\r\n\r\n"
    );
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
            http::response::set_header("x-server-name", tls::session::get_server_name(session));
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
            .get("x-server-name")
            .and_then(|value| value.to_str().ok()),
        Some("app.example.test")
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
        tcp::stream::set_target(stream, "localhost:{}");
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
                http::response::set_header("x-server-name", tls::session::get_server_name(session));
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
            .get("x-server-name")
            .and_then(|value| value.to_str().ok()),
        Some("localhost")
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
        tcp::stream::set_target(stream, "localhost:{}");
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
        Arc::new(Mutex::new(RateLimiterStore::new())),
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
        http::response::set_header("x-server-name", tls::session::get_server_name(session));
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
    assert_eq!(
        response
            .headers()
            .get("x-server-name")
            .and_then(|value| value.to_str().ok()),
        Some("app.example.test")
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

        http::upstream::request::set_target("{upstream_addr}");
        let downstream = tcp::stream::downstream();
        let upstream = tcp::stream::default_upstream();
        while !tcp::stream::eof(downstream) {{
            let chunk = tcp::stream::read(downstream, 3);
            if chunk != "" {{
                tcp::stream::write(upstream, chunk);
            }}
        }}
        http::response::set_body(http::upstream::response::get_body());
    "#
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

        http::upstream::request::set_target("{upstream_addr}");
        let upstream = tcp::stream::default_upstream();
        http::upstream::response::get_status();
        tcp::stream::write(upstream, "late");
    "#
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
    stream.shutdown().await.expect("write half-close should succeed");
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
async fn downstream_transport_proxy_controls_tls_sni_and_handshake() {
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
        if tls::session::get_server_name(session) != "allowed.example.test" {
            tcp::stream::close(downstream);
        } else {
            tls::session::set_alpn(session, "echo/1");
            if tls::session::handshake(session) {
                let body = tcp::stream::read(downstream, 5);
                tcp::stream::write(downstream, body);
                tcp::stream::close(downstream);
            }
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

    let blocked_stream = tokio::net::TcpStream::connect(data_addr)
        .await
        .expect("transport proxy should accept blocked tls");
    let blocked_name = rustls::pki_types::ServerName::try_from("blocked.example.test")
        .expect("blocked name should parse")
        .to_owned();
    let blocked = connector.connect(blocked_name, blocked_stream).await;
    assert!(blocked.is_err(), "blocked SNI should not complete tls");

    let allowed_stream = tokio::net::TcpStream::connect(data_addr)
        .await
        .expect("transport proxy should accept allowed tls");
    let allowed_name = rustls::pki_types::ServerName::try_from("allowed.example.test")
        .expect("allowed name should parse")
        .to_owned();
    let mut allowed = connector
        .connect(allowed_name, allowed_stream)
        .await
        .expect("allowed SNI should complete tls");
    assert_eq!(
        allowed
            .get_ref()
            .1
            .alpn_protocol()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned()),
        Some("echo/1".to_string())
    );
    allowed
        .write_all(b"hello")
        .await
        .expect("tls payload should write");
    allowed
        .shutdown()
        .await
        .expect("tls write half-close should succeed");
    let mut echoed = Vec::new();
    allowed
        .read_to_end(&mut echoed)
        .await
        .expect("tls echo should read");
    assert_eq!(echoed, b"hello");

    data_handle.abort();
    admin_handle.abort();
}
