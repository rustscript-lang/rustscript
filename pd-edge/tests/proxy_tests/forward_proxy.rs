use super::support::*;

#[cfg(feature = "tls")]
#[tokio::test]
async fn sample_forward_proxy_program_tunnels_https_request_through_connect_proxy() {
    let (_upstream_addr, upstream_handle) =
        spawn_https_echo_upstream_on(loopback_addr(SAMPLE_FORWARD_UPSTREAM_HTTPS_PORT)).await;
    let (_forward_proxy_addr, forward_proxy_handle) =
        spawn_connect_forward_proxy_on(loopback_addr(SAMPLE_FORWARD_PROXY_PORT)).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("proxy")
        .join("forward")
        .join("sample_forward_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/forward"))
        .header("x-insecure-upstream", "1")
        .body("via-forward-proxy")
        .send()
        .await
        .expect("forward proxy request should complete");
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
            .get("x-forward-proxy-phase")
            .and_then(|value| value.to_str().ok()),
        Some("attached-http")
    );
    assert_eq!(
        response
            .headers()
            .get("x-upstream-alpn")
            .and_then(|value| value.to_str().ok()),
        Some("http/1.1")
    );
    assert_eq!(
        response
            .headers()
            .get("x-upstream-peer")
            .and_then(|value| value.to_str().ok()),
        Some("localhost")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "via-forward-proxy"
    );

    upstream_handle.abort();
    forward_proxy_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}
