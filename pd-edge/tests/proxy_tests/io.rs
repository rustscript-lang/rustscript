use super::support::*;

#[tokio::test]
async fn sample_io_upstream_handle_program_uses_tcp_and_http_handles_with_io() {
    let upstream_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let body = to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        let response_body = format!("echo:{}\nsecond:ok\n", String::from_utf8_lossy(&body));
        let mut response = Response::new(Body::from(response_body));
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain"),
        );
        response
    }));
    let (_upstream_addr, upstream_handle) =
        spawn_server_on(upstream_app, loopback_addr(SAMPLE_IO_UPSTREAM_PORT)).await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("transport")
        .join("io")
        .join("sample_io_upstream_handle_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/io-upstream-handles"))
        .body("payload")
        .send()
        .await
        .expect("request should complete");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.text().await.expect("body should read");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        headers
            .get("x-io-upstream-handles")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        headers
            .get("x-first-line")
            .and_then(|value| value.to_str().ok()),
        Some("echo:payload|via-io-handle")
    );
    assert_eq!(body, "echo:payload|via-io-handle\nsecond:ok\n");

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn io_protocol_handles_accept_direct_integer_arguments() {
    let upstream_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let body = to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        let response_body = format!("literal:{}\ntrailer:done\n", String::from_utf8_lossy(&body));
        let mut response = Response::new(Body::from(response_body));
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain"),
        );
        response
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use io;

        let upstream = http::exchange::default_upstream();
        http::exchange::set_target(upstream, "{upstream_host}", {upstream_port});
        http::exchange::set_path(upstream, "/literal");
        http::exchange::set_method(upstream, "POST");
        io::write(1, "direct-int-body");
        http::response::set_status(http::exchange::get_status(upstream));
        http::response::set_header("x-first-line", io::read_line(1));
        http::response::set_body(io::read_all(1));
    "#,
        upstream_host = upstream_addr.ip(),
        upstream_port = upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/io-upstream-direct-int"))
        .send()
        .await
        .expect("request should complete");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.text().await.expect("body should read");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        headers
            .get("x-first-line")
            .and_then(|value| value.to_str().ok()),
        Some("literal:direct-int-body")
    );
    assert_eq!(body, "literal:direct-int-body\ntrailer:done\n");

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn io_protocol_handles_accept_tls_session_handles_for_https_exchange() {
    let (upstream_addr, upstream_handle) = spawn_https_echo_upstream().await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use io;
        use tls;

        let upstream = http::exchange::default_upstream();
        http::exchange::set_scheme(upstream, "https");
        http::exchange::set_target(upstream, "localhost", {});
        http::exchange::set_path(upstream, "/echo");
        http::exchange::set_method(upstream, "POST");

        let session = tls::session::from_socket(upstream);
        tls::session::set_verify(session, false);

        io::write(session, "tls-direct-body");
        http::response::set_status(http::exchange::get_status(upstream));
        http::response::set_header("x-first-line", io::read_line(session));
        http::response::set_body(io::read_all(session));
    "#,
        upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/io-upstream-tls-handle"))
        .send()
        .await
        .expect("request should complete");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.text().await.expect("body should read");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        headers
            .get("x-first-line")
            .and_then(|value| value.to_str().ok()),
        Some("tls-direct-body")
    );
    assert_eq!(body, "tls-direct-body");

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn direct_vm_io_protocol_handles_accept_direct_integer_arguments() {
    let upstream_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let body = to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        Response::new(Body::from(format!(
            "literal:{}\ntrailer:done\n",
            String::from_utf8_lossy(&body)
        )))
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;
    let source = format!(
        r#"
        use http;
        use io;

        let upstream = http::exchange::default_upstream();
        http::exchange::set_target(upstream, "{upstream_host}", {upstream_port});
        http::exchange::set_path(upstream, "/literal");
        http::exchange::set_method(upstream, "POST");
        io::write(1, "direct-int-body");
        http::response::set_header("x-first-line", io::read_line(1));
        http::response::set_body(io::read_all(1));
    "#,
        upstream_host = upstream_addr.ip(),
        upstream_port = upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let mut context = Arc::new(ProxyVmContext::from_request_headers(
        axum::http::HeaderMap::new(),
        Arc::new(RateLimiterStore::new()),
    ));
    {
        Arc::get_mut(&mut context)
            .expect("vm context should be uniquely owned")
            .attach_upstream_client(reqwest::Client::new());
    }

    run_edge_program_direct(compiled.program, context.clone())
        .await
        .expect("direct vm io run should succeed");

    upstream_handle.abort();
}

#[tokio::test]
async fn io_open_no_longer_treats_virtual_protocol_paths_as_builtin_handles() {
    let source = r#"
        use io;

        io::open("request.body", "r");
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let context = Arc::new(ProxyVmContext::from_request_headers(
        axum::http::HeaderMap::new(),
        Arc::new(RateLimiterStore::new()),
    ));

    let error = run_edge_program_direct(compiled.program, context.clone())
        .await
        .expect_err("virtual protocol paths should no longer resolve through io::open");
    match error {
        VmError::HostError(message) => {
            assert!(
                message.contains("edge io::open read failed"),
                "unexpected host error: {message}"
            );
        }
        other => panic!("unexpected error type: {other:?}"),
    }
}
