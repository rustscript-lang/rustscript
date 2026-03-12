use super::support::*;

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
