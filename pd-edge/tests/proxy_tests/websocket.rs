use super::support::*;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

#[tokio::test]
async fn sample_websocket_proxy_program_round_trips_text_frames() {
    let (upstream_addr, upstream_handle) = spawn_websocket_echo_upstream().await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("sample_websocket_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/ws"))
        .header("x-ws-target", format!("ws://{upstream_addr}/echo"))
        .header("x-ws-message", "hello")
        .header("x-ws-subprotocols", "superchat, chat")
        .header("x-ws-header-name", "x-client-tag")
        .header("x-ws-header-value", "sample")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-downstream-ws-present")
            .and_then(|value| value.to_str().ok()),
        Some("false")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-handle")
            .and_then(|value| value.to_str().ok()),
        Some("new")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-present-before-configure")
            .and_then(|value| value.to_str().ok()),
        Some("false")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-present-after-configure")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-phase-before-connect")
            .and_then(|value| value.to_str().ok()),
        Some("upgrade-prepared")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-phase-after-connect")
            .and_then(|value| value.to_str().ok()),
        Some("open")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-phase")
            .and_then(|value| value.to_str().ok()),
        Some("closed")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-phase-after-close")
            .and_then(|value| value.to_str().ok()),
        Some("closed")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-eof-before-close")
            .and_then(|value| value.to_str().ok()),
        Some("false")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-eof-after-close")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-protocol")
            .and_then(|value| value.to_str().ok()),
        Some("chat")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-requested-subprotocols")
            .and_then(|value| value.to_str().ok()),
        Some("superchat, chat")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-custom-header")
            .and_then(|value| value.to_str().ok()),
        Some("x-client-tag")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-mode")
            .and_then(|value| value.to_str().ok()),
        Some("text")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "echo:hello|tag:sample"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn sample_websocket_proxy_program_round_trips_binary_frames_with_default_handle() {
    let (upstream_addr, upstream_handle) = spawn_websocket_echo_upstream().await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("sample_websocket_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");
    let payload = STANDARD.encode(b"bin-payload");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/ws-binary-sample"))
        .header("x-ws-target", format!("ws://{upstream_addr}/binary"))
        .header("x-ws-binary-base64", &payload)
        .header("x-ws-handle", "default")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-downstream-ws-present")
            .and_then(|value| value.to_str().ok()),
        Some("false")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-handle")
            .and_then(|value| value.to_str().ok()),
        Some("default-upstream")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-present-before-configure")
            .and_then(|value| value.to_str().ok()),
        Some("false")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-present-after-configure")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-phase-before-connect")
            .and_then(|value| value.to_str().ok()),
        Some("upgrade-prepared")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-phase-after-connect")
            .and_then(|value| value.to_str().ok()),
        Some("open")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-phase")
            .and_then(|value| value.to_str().ok()),
        Some("closed")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-eof-before-close")
            .and_then(|value| value.to_str().ok()),
        Some("false")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-eof-after-close")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-protocol")
            .and_then(|value| value.to_str().ok()),
        Some("chat")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-mode")
            .and_then(|value| value.to_str().ok()),
        Some("binary")
    );
    assert_eq!(response.text().await.expect("body should read"), payload);

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn websocket_connection_can_round_trip_binary_frames() {
    let (upstream_addr, upstream_handle) = spawn_websocket_echo_upstream().await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let payload = STANDARD.encode(b"bin-payload");
    let source = format!(
        r#"
        use http;
        use websocket;

        let connection = websocket::connection::default_upstream();
        websocket::connection::set_target(connection, "ws://{upstream_addr}/binary");
        websocket::connection::connect(connection);
        websocket::connection::send_binary_base64(connection, "{payload}");
        let echoed = websocket::connection::read_binary_base64(connection);
        http::response::set_header("x-phase", websocket::connection::get_phase(connection));
        websocket::connection::close(connection, 1000, "binary-complete");
        http::response::set_body(echoed);
    "#
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/ws-binary"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-phase")
            .and_then(|value| value.to_str().ok()),
        Some("open")
    );
    assert_eq!(response.text().await.expect("body should read"), payload);

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn downstream_websocket_handle_exposes_upgrade_candidate_phase() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = r#"
        use http;
        use websocket;

        let downstream = websocket::connection::downstream();
        if websocket::connection::is_present(downstream) {
            http::response::set_header("x-phase", websocket::connection::get_phase(downstream));
            http::response::set_body("upgrade");
        } else {
            http::response::set_body("plain");
        }
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/ws-downstream"))
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-phase")
            .and_then(|value| value.to_str().ok()),
        Some("upgrade-observed")
    );
    assert_eq!(response.text().await.expect("body should read"), "upgrade");

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn downstream_websocket_binary_tunnel_upgrades_and_relays_frames() {
    let (upstream_addr, upstream_handle) = spawn_websocket_echo_upstream().await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = r#"
        use http;
        use proxy;
        use websocket;

        let target = http::request::get_header("x-ws-target");
        if target == "" {
            http::response::set_status(400);
            http::response::set_body("missing x-ws-target");
        } else {
            let upstream = websocket::connection::new();
            websocket::connection::set_target(upstream, target);
            let protocols = http::request::get_header("sec-websocket-protocol");
            if protocols != "" {
                websocket::connection::set_subprotocols(upstream, protocols);
            }
            let downstream = proxy::stream::downstream();
            let peer = proxy::stream::from_websocket_binary(upstream);
            let status = proxy::tunnel(downstream, peer, 1024);
            http::response::set_header("x-proxy-status", status);
            http::response::set_header("x-upstream-phase", websocket::connection::get_phase(upstream));
            http::response::set_header("x-upstream-protocol", websocket::connection::get_subprotocol(upstream));
        }
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let mut request = format!("ws://{data_addr}/ws-tunnel")
        .into_client_request()
        .expect("websocket request should build");
    request.headers_mut().insert(
        "x-ws-target",
        HeaderValue::from_str(&format!("ws://{upstream_addr}/binary"))
            .expect("websocket target header should encode"),
    );
    request.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("superchat, chat"),
    );
    let (mut websocket, response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("downstream websocket handshake should succeed");
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(
        response
            .headers()
            .get("x-proxy-status")
            .and_then(|value| value.to_str().ok()),
        Some("upgraded")
    );
    assert_eq!(
        response
            .headers()
            .get("x-upstream-phase")
            .and_then(|value| value.to_str().ok()),
        Some("open")
    );
    assert_eq!(
        response
            .headers()
            .get("x-upstream-protocol")
            .and_then(|value| value.to_str().ok()),
        Some("chat")
    );
    assert_eq!(
        response
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|value| value.to_str().ok()),
        Some("chat")
    );

    websocket
        .send(Message::Binary(b"hello".to_vec().into()))
        .await
        .expect("binary websocket frame should send");
    let echoed = websocket
        .next()
        .await
        .expect("websocket should yield a response")
        .expect("websocket response should decode");
    assert_eq!(echoed, Message::Binary(b"hello".to_vec().into()));

    websocket
        .close(None)
        .await
        .expect("websocket close should succeed");

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn downstream_transport_proxy_accepts_and_executes_websocket_frames_directly() {
    let (data_addr, admin_addr, data_handle, admin_handle) =
        spawn_transport_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let payload = STANDARD.encode(b"transport-binary");
    let source = format!(
        r#"
        use websocket;

        let downstream = websocket::connection::downstream();
        websocket::connection::set_subprotocols(downstream, "chat");
        websocket::connection::connect(downstream);
        let payload = websocket::connection::read_binary_base64(downstream);
        websocket::connection::send_binary_base64(downstream, payload);
        websocket::connection::close(downstream, 1000, "done");
    "#
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let mut request = format!("ws://{data_addr}/direct")
        .into_client_request()
        .expect("websocket request should build");
    request.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("superchat, chat"),
    );
    let (mut websocket, response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("downstream websocket accept should succeed");
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(
        response
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|value| value.to_str().ok()),
        Some("chat")
    );

    websocket
        .send(Message::Binary(
            STANDARD
                .decode(&payload)
                .expect("payload should decode")
                .into(),
        ))
        .await
        .expect("downstream websocket payload should send");
    let echoed = websocket
        .next()
        .await
        .expect("websocket should yield a response")
        .expect("websocket response should decode");
    assert_eq!(echoed, Message::Binary(b"transport-binary".to_vec().into()));

    let closed = websocket
        .next()
        .await
        .expect("websocket should yield a close frame")
        .expect("close frame should decode");
    assert!(matches!(closed, Message::Close(_)));

    data_handle.abort();
    admin_handle.abort();
}
