use super::support::*;

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
        http::exchange::set_target(exchange, "https://localhost:{}/echo");
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
        http::exchange::set_target(exchange, "https://localhost:{}/echo");
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
        http::exchange::set_target(exchange, "https://localhost:{}/echo");
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
        http::exchange::set_target(exchange, "https://localhost:{}/echo");
        http::exchange::set_body(exchange, "mtls-body");

        let session = tls::session::from_socket(exchange);
        tls::session::set_trusted_certificate(session, {});
        tls::session::set_certificate(session, {});
        tls::session::set_private_key(session, {});
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
async fn tls_session_rejects_alpn_policy_mismatch() {
    let (upstream_addr, upstream_handle) = spawn_https_echo_upstream().await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tls;

        let exchange = http::exchange::default_upstream();
        http::exchange::set_target(exchange, "https://localhost:{}/echo");
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
        http::exchange::set_target(exchange, "https://localhost:{}/fast");

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

        http::upstream::request::set_target("https://localhost:{}/echo");
        http::upstream::request::set_body("fallback-body");

        let session = tls::session::from_socket(http::exchange::default_upstream());
        tls::session::set_verify(session, false);
        http::response::set_header(
            "x-version",
            http::upstream::response::get_http_version()
        );
        http::response::set_header("x-alpn", tls::session::get_alpn(session));
        http::response::set_body(http::upstream::response::get_body());
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
