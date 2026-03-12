use super::support::*;

#[tokio::test]
async fn dynamic_tcp_stream_can_attach_to_http_exchange() {
    let upstream_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let body = to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        Response::new(Body::from(format!(
            "attached:{}",
            String::from_utf8_lossy(&body)
        )))
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tcp;

        let stream = tcp::stream::new();
        tcp::stream::set_target(stream, "{upstream_addr}");
        if !tcp::stream::connect(stream) {{
            http::response::set_status(502);
            http::response::set_body("connect failed");
        }} else {{
            let exchange = http::exchange::new();
            http::exchange::set_target(exchange, "http://{upstream_addr}/attached");
            http::exchange::set_method(exchange, "POST");
            http::exchange::set_body(exchange, "payload");
            http::exchange::attach_tcp(exchange, stream);
            http::response::set_header("x-before", tcp::stream::get_phase(stream));
            http::response::set_status(http::exchange::get_status(exchange));
            http::response::set_header("x-after", tcp::stream::get_phase(stream));
            http::response::set_body(http::exchange::get_body(exchange));
        }}
    "#
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/attached-http"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-before")
            .and_then(|value| value.to_str().ok()),
        Some("connected")
    );
    assert_eq!(
        response
            .headers()
            .get("x-after")
            .and_then(|value| value.to_str().ok()),
        Some("attached-http")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "attached:payload"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn dynamic_tls_session_can_attach_to_http_exchange() {
    let (upstream_addr, upstream_handle) = spawn_https_echo_upstream().await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tcp;
        use tls;

        let stream = tcp::stream::new();
        tcp::stream::set_target(stream, "{upstream_addr}");
        if !tcp::stream::connect(stream) {{
            http::response::set_status(502);
            http::response::set_body("connect failed");
        }} else {{
            let exchange = http::exchange::new();
            http::exchange::set_target(exchange, "https://localhost:{}/echo");
            http::exchange::set_method(exchange, "POST");
            http::exchange::set_body(exchange, "secure-payload");

            let session = tls::session::from_socket(stream);
            http::exchange::attach_tls_plaintext(exchange, session);
            tls::session::set_verify(session, false);
            tls::session::set_verify_hostname(session, false);

            if !tls::session::handshake(session) {{
                http::response::set_status(502);
                http::response::set_body("handshake failed");
            }} else {{
                http::response::set_header("x-before", tcp::stream::get_phase(stream));
                http::response::set_header("x-peer", tls::session::get_peer_name(session));
                http::response::set_header("x-alpn", tls::session::get_alpn(session));
                http::response::set_status(http::exchange::get_status(exchange));
                http::response::set_header("x-after", tcp::stream::get_phase(stream));
                http::response::set_body(http::exchange::get_body(exchange));
            }}
        }}
    "#,
        upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/attached-tls"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-before")
            .and_then(|value| value.to_str().ok()),
        Some("upgraded-tls")
    );
    assert_eq!(
        response
            .headers()
            .get("x-after")
            .and_then(|value| value.to_str().ok()),
        Some("attached-http")
    );
    assert_eq!(
        response
            .headers()
            .get("x-peer")
            .and_then(|value| value.to_str().ok()),
        Some("localhost")
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
        "secure-payload"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}
