use super::support::*;

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
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
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
            .get("x-ws-protocol")
            .and_then(|value| value.to_str().ok()),
        Some("chat")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "echo:hello"
    );

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
