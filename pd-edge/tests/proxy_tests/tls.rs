use super::support::*;

async fn spawn_ca_signed_https_client_metadata_upstream(
    materials: &TlsTestMaterials,
) -> (SocketAddr, JoinHandle<()>) {
    ensure_rustls_provider();

    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from(materials.ca_der.clone()))
        .expect("ca cert should be trusted");
    let client_verifier = rustls::server::WebPkiClientVerifier::builder(roots.into())
        .build()
        .expect("client verifier should build");
    let mut server_config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(
            vec![CertificateDer::from(materials.server_cert_der.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(materials.server_key_der.clone())),
        )
        .expect("server config should build");
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];

    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    let handle = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("accept should succeed");
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};

                let mut stream = acceptor
                    .accept(stream)
                    .await
                    .expect("tls accept should succeed");
                let server_name = stream
                    .get_ref()
                    .1
                    .server_name()
                    .unwrap_or_default()
                    .to_string();
                let client_cert_count = stream
                    .get_ref()
                    .1
                    .peer_certificates()
                    .map(|certificates| certificates.len())
                    .unwrap_or(0);
                let mut request = Vec::new();
                let mut buffer = [0u8; 2048];
                let mut expected_body_len = None;

                loop {
                    let read = match stream.read(&mut buffer).await {
                        Ok(read) => read,
                        Err(err)
                            if matches!(
                                err.kind(),
                                std::io::ErrorKind::BrokenPipe
                                    | std::io::ErrorKind::ConnectionAborted
                                    | std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::UnexpectedEof
                            ) =>
                        {
                            break;
                        }
                        Err(err) => panic!("request read should succeed: {err}"),
                    };
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if expected_body_len.is_none()
                        && let Some(header_end) =
                            request.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        let header_end = header_end + 4;
                        let headers = String::from_utf8_lossy(&request[..header_end]);
                        let content_length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                if !name.eq_ignore_ascii_case("content-length") {
                                    return None;
                                }
                                value.trim().parse::<usize>().ok()
                            })
                            .unwrap_or(0);
                        expected_body_len = Some(header_end + content_length);
                    }
                    if let Some(total_len) = expected_body_len
                        && request.len() >= total_len
                    {
                        break;
                    }
                }

                let body = if let Some(header_end) =
                    request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    request[header_end + 4..].to_vec()
                } else {
                    Vec::new()
                };
                let body = String::from_utf8_lossy(&body).into_owned();
                let response_body =
                    format!("sni:{server_name}|mtls:{client_cert_count}|body:{body}");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .unwrap_or_else(|err| {
                        if matches!(
                            err.kind(),
                            std::io::ErrorKind::BrokenPipe
                                | std::io::ErrorKind::ConnectionAborted
                                | std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::UnexpectedEof
                        ) {
                            return;
                        }
                        panic!("response should write: {err}");
                    });
                if let Err(err) = stream.flush().await
                    && !matches!(
                        err.kind(),
                        std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::UnexpectedEof
                    )
                {
                    panic!("response should flush: {err}");
                }
                let _ = stream.shutdown().await;
            });
        }
    });
    (addr, handle)
}

#[tokio::test]
async fn tls_session_can_disable_verification_and_expose_handshake_phase() {
    let (upstream_addr, upstream_handle) = spawn_https_echo_upstream().await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tcp;
        use tls;

        let exchange = http::exchange::default_upstream();
        http::exchange::set_target(exchange, "localhost", {});
        http::exchange::set_scheme(exchange, "https");
        http::exchange::set_path(exchange, "/echo");
        http::exchange::set_body(exchange, "phase-body");

        let session = tls::session::from_socket(exchange);
        tls::session::set_verify(session, false);
        tls::session::handshake(session);

        http::response::set_header("x-phase", tls::session::get_phase(session));
        http::response::set_header("x-peer-cert", tls::session::get_peer_certificate(session));
        http::response::set_body(http::exchange::get_body(exchange));
    "#,
        upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/tls-phase"))
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
    assert!(
        response
            .headers()
            .get("x-peer-cert")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| !value.is_empty())
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "phase-body"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn tls_session_handshake_advances_from_cached_session_without_presence_guard() {
    let (upstream_addr, upstream_handle) = spawn_https_echo_upstream().await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tls;

        let exchange = http::exchange::default_upstream();
        http::exchange::set_target(exchange, "localhost", {});
        http::exchange::set_scheme(exchange, "https");
        http::exchange::set_path(exchange, "/echo");
        http::exchange::set_body(exchange, "reuse-body");

        let session = tls::session::from_socket(exchange);
        tls::session::set_verify(session, false);
        tls::session::handshake(session);

        http::response::set_header("x-phase", tls::session::get_phase(session));
        if tls::session::is_session_reused(session) {{
            http::response::set_header("x-reused", "true");
        }} else {{
            http::response::set_header("x-reused", "false");
        }}
        http::response::set_body(http::exchange::get_body(exchange));
    "#,
        upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let first = client
        .get(format!("http://{data_addr}/tls-reuse-first"))
        .send()
        .await
        .expect("first request should complete");
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(
        first
            .headers()
            .get("x-phase")
            .and_then(|value| value.to_str().ok()),
        Some("plaintext-ready")
    );
    assert_eq!(
        first
            .headers()
            .get("x-reused")
            .and_then(|value| value.to_str().ok()),
        Some("false")
    );
    assert_eq!(first.text().await.expect("body should read"), "reuse-body");

    let second = client
        .get(format!("http://{data_addr}/tls-reuse-second"))
        .send()
        .await
        .expect("second request should complete");
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(
        second
            .headers()
            .get("x-phase")
            .and_then(|value| value.to_str().ok()),
        Some("plaintext-ready")
    );
    assert_eq!(
        second
            .headers()
            .get("x-reused")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(second.text().await.expect("body should read"), "reuse-body");

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn tls_session_accepts_custom_trusted_certificate_bundle() {
    let materials = build_ca_signed_tls_materials();
    let (upstream_addr, upstream_handle) =
        spawn_ca_signed_https_echo_upstream(&materials, false).await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tls;

        let exchange = http::exchange::default_upstream();
        http::exchange::set_target(exchange, "localhost", {});
        http::exchange::set_scheme(exchange, "https");
        http::exchange::set_path(exchange, "/echo");
        http::exchange::set_body(exchange, "trusted-body");

        let session = tls::session::from_socket(exchange);
        tls::session::set_trusted_certificate(session, {});
        tls::session::handshake(session);

        http::response::set_header("x-phase", tls::session::get_phase(session));
        http::response::set_header("x-alpn", tls::session::get_alpn(session));
        http::response::set_body(http::exchange::get_body(exchange));
    "#,
        upstream_addr.port(),
        source_string_literal(&materials.ca_pem),
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/tls-ca"))
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
            .get("x-alpn")
            .and_then(|value| value.to_str().ok()),
        Some("http/1.1")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "trusted-body"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn tls_session_supports_client_certificate_authentication() {
    let materials = build_ca_signed_tls_materials();
    let (upstream_addr, upstream_handle) =
        spawn_ca_signed_https_echo_upstream(&materials, true).await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tls;

        let exchange = http::exchange::default_upstream();
        http::exchange::set_target(exchange, "localhost", {});
        http::exchange::set_scheme(exchange, "https");
        http::exchange::set_path(exchange, "/echo");
        http::exchange::set_body(exchange, "mtls-body");

        let session = tls::session::from_socket(exchange);
        tls::session::set_trusted_certificate(session, {});
        tls::session::set_client_certificate(session, {});
        tls::session::set_client_private_key(session, {});
        tls::session::handshake(session);

        http::response::set_header("x-phase", tls::session::get_phase(session));
        http::response::set_body(http::exchange::get_body(exchange));
    "#,
        upstream_addr.port(),
        source_string_literal(&materials.ca_pem),
        source_string_literal(&materials.client_cert_pem),
        source_string_literal(&materials.client_key_pem),
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/tls-mtls"))
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
        response.text().await.expect("body should read"),
        "mtls:1:mtls-body"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn tls_session_reuse_preserves_mtls_and_sni_behavior() {
    let materials = build_ca_signed_tls_materials();
    let (upstream_addr, upstream_handle) =
        spawn_ca_signed_https_client_metadata_upstream(&materials).await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tls;

        let exchange = http::exchange::default_upstream();
        http::exchange::set_target(exchange, "localhost", {});
        http::exchange::set_scheme(exchange, "https");
        http::exchange::set_path(exchange, "/echo");
        http::exchange::set_body(exchange, "mtls-reuse-body");

        let session = tls::session::from_socket(exchange);
        tls::session::set_trusted_certificate(session, {});
        tls::session::set_client_certificate(session, {});
        tls::session::set_client_private_key(session, {});
        tls::session::handshake(session);

        http::response::set_header("x-phase", tls::session::get_phase(session));
        http::response::set_header("x-peer-name", tls::session::get_peer_name(session));
        if tls::session::is_session_reused(session) {{
            http::response::set_header("x-reused", "true");
        }} else {{
            http::response::set_header("x-reused", "false");
        }}
        http::response::set_body(http::exchange::get_body(exchange));
    "#,
        upstream_addr.port(),
        source_string_literal(&materials.ca_pem),
        source_string_literal(&materials.client_cert_pem),
        source_string_literal(&materials.client_key_pem),
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let first = client
        .get(format!("http://{data_addr}/tls-reuse-mtls-first"))
        .send()
        .await
        .expect("first request should complete");
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(
        first
            .headers()
            .get("x-phase")
            .and_then(|value| value.to_str().ok()),
        Some("plaintext-ready")
    );
    assert_eq!(
        first
            .headers()
            .get("x-peer-name")
            .and_then(|value| value.to_str().ok()),
        Some("localhost")
    );
    assert_eq!(
        first
            .headers()
            .get("x-reused")
            .and_then(|value| value.to_str().ok()),
        Some("false")
    );
    let first_body = first.text().await.expect("body should read");
    assert_eq!(first_body, "sni:localhost|mtls:1|body:mtls-reuse-body");

    let second = client
        .get(format!("http://{data_addr}/tls-reuse-mtls-second"))
        .send()
        .await
        .expect("second request should complete");
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(
        second
            .headers()
            .get("x-phase")
            .and_then(|value| value.to_str().ok()),
        Some("plaintext-ready")
    );
    assert_eq!(
        second
            .headers()
            .get("x-peer-name")
            .and_then(|value| value.to_str().ok()),
        Some("localhost")
    );
    assert_eq!(
        second
            .headers()
            .get("x-reused")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    let second_body = second.text().await.expect("body should read");
    assert_eq!(second_body, first_body);

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn tls_session_rejects_alpn_policy_mismatch() {
    let (upstream_addr, upstream_handle) = spawn_https_echo_upstream().await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tls;

        let exchange = http::exchange::default_upstream();
        http::exchange::set_target(exchange, "localhost", {});
        http::exchange::set_scheme(exchange, "https");
        http::exchange::set_path(exchange, "/echo");
        let session = tls::session::from_socket(exchange);
        tls::session::set_verify(session, false);
        tls::session::set_alpn(session, "h2");
        tls::session::handshake(session);
    "#,
        upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/tls-alpn-mismatch"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[cfg(all(feature = "tls", feature = "http2"))]
#[tokio::test]
async fn tls_session_accepts_h2_alpn_policy_when_http2_is_negotiated() {
    let (upstream_addr, _connection_count, upstream_handle) =
        spawn_https_http2_multiplex_upstream().await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tls;

        let exchange = http::exchange::default_upstream();
        http::exchange::set_target(exchange, "localhost", {});
        http::exchange::set_scheme(exchange, "https");
        http::exchange::set_path(exchange, "/fast");

        let session = tls::session::from_socket(exchange);
        tls::session::set_verify(session, false);
        tls::session::set_alpn(session, "h2");
        tls::session::handshake(session);

        http::response::set_header("x-phase", tls::session::get_phase(session));
        http::response::set_header("x-alpn", tls::session::get_alpn(session));
        http::response::set_header("x-version", http::exchange::get_http_version(exchange));
        http::response::set_body(http::exchange::get_body(exchange));
    "#,
        upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/tls-alpn-h2"))
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
            .get("x-alpn")
            .and_then(|value| value.to_str().ok()),
        Some("h2")
    );
    assert_eq!(
        response
            .headers()
            .get("x-version")
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "fast-body"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[cfg(all(feature = "tls", feature = "http2"))]
#[tokio::test]
async fn http2_capable_client_falls_back_to_http11_when_h2_is_unavailable() {
    let (upstream_addr, upstream_handle) = spawn_https_echo_upstream().await;

    let (data_addr, admin_addr, data_handle, admin_handle) =
        spawn_proxy_with_state(SharedState::new(1024 * 1024)).await;
    let client = reqwest::Client::new();

    let source = format!(
        r#"
        use http;
        use tls;

        let upstream = http::exchange::default_upstream();
        http::exchange::set_target(upstream, "localhost", {});
        http::exchange::set_scheme(upstream, "https");
        http::exchange::set_path(upstream, "/echo");
        http::exchange::set_body(upstream, "fallback-body");

        let session = tls::session::from_socket(http::exchange::default_upstream());
        tls::session::set_verify(session, false);
        http::response::set_header(
            "x-version",
            http::exchange::get_http_version(upstream)
        );
        http::response::set_header("x-alpn", tls::session::get_alpn(session));
        http::response::set_body(http::exchange::get_body(upstream));
    "#,
        upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/http2-fallback"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-version")
            .and_then(|value| value.to_str().ok()),
        Some("1.1")
    );
    assert_eq!(
        response
            .headers()
            .get("x-alpn")
            .and_then(|value| value.to_str().ok()),
        Some("http/1.1")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "fallback-body"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}
