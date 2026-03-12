use super::support::*;

#[tokio::test]
async fn sample_webrtc_proxy_program_round_trips_text_messages() {
    let (webrtc_addr, webrtc_handle) =
        spawn_webrtc_echo_server("127.0.0.1:0".parse().expect("valid ephemeral addr"))
            .await
            .expect("webrtc echo server should start");
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("sample_webrtc_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/webrtc"))
        .header(
            "x-webrtc-signal-target",
            format!("http://{webrtc_addr}/offer"),
        )
        .header("x-webrtc-message", "ping")
        .header("x-webrtc-data-channel-label", "sample-chat")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-handle")
            .and_then(|value| value.to_str().ok()),
        Some("new")
    );
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-present-before-configure")
            .and_then(|value| value.to_str().ok()),
        Some("false")
    );
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-present-after-configure")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-data-channel-label")
            .and_then(|value| value.to_str().ok()),
        Some("sample-chat")
    );
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-phase-before-offer")
            .and_then(|value| value.to_str().ok()),
        Some("configured")
    );
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-phase-after-offer")
            .and_then(|value| value.to_str().ok()),
        Some("offer-created")
    );
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-phase-after-remote-description")
            .and_then(|value| value.to_str().ok()),
        Some("remote-description-set")
    );
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-connected")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-phase-after-connect")
            .and_then(|value| value.to_str().ok()),
        Some("open")
    );
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-eof-before-close")
            .and_then(|value| value.to_str().ok()),
        Some("false")
    );
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-phase")
            .and_then(|value| value.to_str().ok()),
        Some("closed")
    );
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-phase-after-close")
            .and_then(|value| value.to_str().ok()),
        Some("closed")
    );
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-eof-after-close")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-mode")
            .and_then(|value| value.to_str().ok()),
        Some("text")
    );
    assert_eq!(response.text().await.expect("body should read"), "ping");

    webrtc_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn sample_webrtc_proxy_program_round_trips_binary_messages_with_default_handle() {
    let (webrtc_addr, webrtc_handle) =
        spawn_webrtc_echo_server("127.0.0.1:0".parse().expect("valid ephemeral addr"))
            .await
            .expect("webrtc echo server should start");
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("sample_webrtc_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");
    let payload = STANDARD.encode(b"webrtc-bin");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/webrtc-binary"))
        .header(
            "x-webrtc-signal-target",
            format!("http://{webrtc_addr}/offer"),
        )
        .header("x-webrtc-binary-base64", &payload)
        .header("x-webrtc-handle", "default")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-handle")
            .and_then(|value| value.to_str().ok()),
        Some("default-upstream")
    );
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-phase-before-offer")
            .and_then(|value| value.to_str().ok()),
        Some("configured")
    );
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-phase-after-offer")
            .and_then(|value| value.to_str().ok()),
        Some("offer-created")
    );
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-phase-after-connect")
            .and_then(|value| value.to_str().ok()),
        Some("open")
    );
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-phase")
            .and_then(|value| value.to_str().ok()),
        Some("closed")
    );
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-mode")
            .and_then(|value| value.to_str().ok()),
        Some("binary")
    );
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-eof-before-close")
            .and_then(|value| value.to_str().ok()),
        Some("false")
    );
    assert_eq!(
        response
            .headers()
            .get("x-webrtc-eof-after-close")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(response.text().await.expect("body should read"), payload);

    webrtc_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}
