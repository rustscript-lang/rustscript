use super::support::*;
use tower::ServiceExt;

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
async fn no_active_program_returns_404() {
    let (data_addr, _admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let response = client
        .get(format!("http://{data_addr}/anything"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn upload_valid_program_controls_subsequent_requests() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program = build_short_circuit_program("hello vm", None);

    let upload = upload_program(&client, admin_addr, &program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.expect("body should read"), "hello vm");

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn short_circuit_path_returns_200_body_and_headers() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program = build_short_circuit_program("payload", Some(("x-vm", "short")));

    let upload = upload_program(&client, admin_addr, &program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-vm")
            .and_then(|value| value.to_str().ok()),
        Some("short")
    );
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/plain")
    );
    assert_eq!(response.text().await.expect("body should read"), "payload");

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn upstream_path_proxies_method_path_query_and_body() {
    let upstream_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let (parts, body) = request.into_parts();
        let body = to_bytes(body, usize::MAX)
            .await
            .expect("body should be readable");
        let path_and_query = parts
            .uri
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/");
        let content = format!(
            "{}|{}|{}",
            parts.method,
            path_and_query,
            String::from_utf8_lossy(&body)
        );
        let mut response = Response::new(Body::from(content));
        response
            .headers_mut()
            .insert("x-upstream", HeaderValue::from_static("yes"));
        response
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program = build_upstream_program(&upstream_addr.to_string(), None);

    let upload = upload_program(&client, admin_addr, &program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/api/v1/items?x=1"))
        .body("ping")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-upstream")
            .and_then(|value| value.to_str().ok()),
        Some("yes")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "POST|/api/v1/items?x=1|ping"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn upstream_accepts_full_url_with_path() {
    let upstream_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let path = request
            .uri()
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/");
        Response::new(Body::from(path.to_string()))
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program = build_upstream_program(&format!("http://{upstream_addr}/fixed"), None);

    let upload = upload_program(&client, admin_addr, &program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/other?x=1"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.text().await.expect("body should read"),
        "/fixed?x=1"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn vm_response_headers_are_applied_on_short_circuit_and_proxied_paths() {
    let upstream_app = Router::new().fallback(any(|_request: Request<Body>| async move {
        let mut response = Response::new(Body::from("upstream"));
        response
            .headers_mut()
            .insert("x-vm", HeaderValue::from_static("from-upstream"));
        response
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let short_program = build_short_circuit_program("short", Some(("x-vm", "from-vm-short")));
    let upload_short = upload_program(&client, admin_addr, &short_program).await;
    assert_eq!(upload_short.status(), StatusCode::NO_CONTENT);
    let short_response = client
        .get(format!("http://{data_addr}/"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(
        short_response
            .headers()
            .get("x-vm")
            .and_then(|value| value.to_str().ok()),
        Some("from-vm-short")
    );

    let proxied_program =
        build_upstream_program(&upstream_addr.to_string(), Some(("x-vm", "from-vm-proxy")));
    let upload_proxy = upload_program(&client, admin_addr, &proxied_program).await;
    assert_eq!(upload_proxy.status(), StatusCode::NO_CONTENT);
    let proxied_response = client
        .get(format!("http://{data_addr}/"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(proxied_response.status(), StatusCode::OK);
    assert_eq!(
        proxied_response
            .headers()
            .get("x-vm")
            .and_then(|value| value.to_str().ok()),
        Some("from-vm-proxy")
    );
    assert_eq!(
        proxied_response.text().await.expect("body should read"),
        "upstream"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn invalid_upload_returns_400_and_keeps_previous_program() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let original = build_short_circuit_program("old", None);
    let upload_ok = upload_program(&client, admin_addr, &original).await;
    assert_eq!(upload_ok.status(), StatusCode::NO_CONTENT);

    let upload_bad = client
        .put(format!("http://{admin_addr}/program"))
        .header("content-type", "application/octet-stream")
        .body(vec![0u8, 1, 2, 3, 4])
        .send()
        .await
        .expect("upload should complete");
    assert_eq!(upload_bad.status(), StatusCode::BAD_REQUEST);

    let response = client
        .get(format!("http://{data_addr}/"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.expect("body should read"), "old");

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn in_flight_request_uses_old_program_after_swap() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    let started_for_handler = started.clone();
    let release_for_handler = release.clone();
    let upstream_app = Router::new().fallback(any(move |_request: Request<Body>| {
        let started = started_for_handler.clone();
        let release = release_for_handler.clone();
        async move {
            started.notify_one();
            release.notified().await;
            Response::new(Body::from("old"))
        }
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let old_program = build_upstream_program(&upstream_addr.to_string(), None);
    let upload_old = upload_program(&client, admin_addr, &old_program).await;
    assert_eq!(upload_old.status(), StatusCode::NO_CONTENT);

    let in_flight_client = client.clone();
    let in_flight_url = format!("http://{data_addr}/slow");
    let in_flight = tokio::spawn(async move {
        let response = in_flight_client
            .get(in_flight_url)
            .send()
            .await
            .expect("in-flight request should complete");
        let status = response.status();
        let body = response.text().await.expect("in-flight body should read");
        (status, body)
    });

    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("upstream should receive in-flight request");

    let new_program = build_short_circuit_program("new", None);
    let upload_new = upload_program(&client, admin_addr, &new_program).await;
    assert_eq!(upload_new.status(), StatusCode::NO_CONTENT);

    release.notify_waiters();

    let (status, body) = in_flight.await.expect("join should succeed");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "old");

    let next_response = client
        .get(format!("http://{data_addr}/next"))
        .send()
        .await
        .expect("next request should complete");
    assert_eq!(next_response.status(), StatusCode::OK);
    assert_eq!(next_response.text().await.expect("body should read"), "new");

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn upstream_unreachable_returns_502() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let closed_addr = listener.local_addr().expect("listener should have addr");
    drop(listener);

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let program = build_upstream_program(&closed_addr.to_string(), None);
    let upload = upload_program(&client, admin_addr, &program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn tiny_language_can_enforce_simple_rate_limit() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = r#"
        use http;
        use rate_limit;

        if rate_limit::allow(http::request::get_header("x-client-id"), 2, 60) {
            http::response::set_header("x-vm", "allowed");
            http::response::set_body("ok");
        } else {
            http::response::set_header("x-vm", "rate-limited");
            http::response::set_body("blocked");
        }
    "#;
    let compiled = compile_source(source).expect("source should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    for _ in 0..2 {
        let response = client
            .get(format!("http://{data_addr}/"))
            .header("x-client-id", "abc")
            .send()
            .await
            .expect("request should complete");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("x-vm")
                .and_then(|value| value.to_str().ok()),
            Some("allowed")
        );
        assert_eq!(response.text().await.expect("body should read"), "ok");
    }

    let blocked = client
        .get(format!("http://{data_addr}/"))
        .header("x-client-id", "abc")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(blocked.status(), StatusCode::OK);
    assert_eq!(
        blocked
            .headers()
            .get("x-vm")
            .and_then(|value| value.to_str().ok()),
        Some("rate-limited")
    );
    assert_eq!(blocked.text().await.expect("body should read"), "blocked");

    let other_key = client
        .get(format!("http://{data_addr}/"))
        .header("x-client-id", "xyz")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(other_key.status(), StatusCode::OK);
    assert_eq!(
        other_key
            .headers()
            .get("x-vm")
            .and_then(|value| value.to_str().ok()),
        Some("allowed")
    );

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn http_prefixed_host_abi_can_rewrite_request_and_short_circuit() {
    let upstream_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let method = request.method().clone();
        let path = request
            .uri()
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/");
        let added = request
            .headers()
            .get("x-added")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        Response::new(Body::from(format!("{method}|{path}|{added}")))
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = format!(
        r#"
        use http;
        use rate_limit;

        let client_id = http::request::get_header("x-client-id");
        if rate_limit::allow(client_id, 1, 60) {{
            let upstream = http::exchange::default_upstream();
            http::exchange::set_path(upstream, "/rewritten");
            http::exchange::set_query(upstream, "from=vm");
            http::exchange::set_header(upstream, "x-added", "yes");
            http::exchange::set_target(upstream, "127.0.0.1", {upstream_port});
        }} else {{
            http::response::set_status(429);
            http::response::set_body("blocked");
        }}
    "#,
        upstream_port = upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let first = client
        .get(format!("http://{data_addr}/anything?x=1"))
        .header("x-client-id", "abc")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(
        first.text().await.expect("body should read"),
        "GET|/rewritten?from=vm|yes"
    );

    let second = client
        .get(format!("http://{data_addr}/anything?x=1"))
        .header("x-client-id", "abc")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(second.text().await.expect("body should read"), "blocked");

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn http_request_body_can_be_rewritten_before_proxying() {
    let upstream_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let (parts, body) = request.into_parts();
        let body = to_bytes(body, usize::MAX)
            .await
            .expect("body should be readable");
        let path = parts
            .uri
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/");
        Response::new(Body::from(format!(
            "{}|{}",
            path,
            String::from_utf8_lossy(&body)
        )))
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = format!(
        r#"
        use http;

        let upstream = http::exchange::default_upstream();
        http::exchange::set_body(upstream, "rewritten-body");
        http::exchange::set_target(upstream, "127.0.0.1", {upstream_port});
    "#,
        upstream_port = upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/payload"))
        .body("original-body")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.text().await.expect("body should read"),
        "/payload|rewritten-body"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn http_proxy_reuses_plain_http1_upstream_connection_for_non_empty_forwarded_bodies() {
    use std::convert::Infallible;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::body::Body;
    use hyper::{Request, Response, service::service_fn};
    use hyper_util::rt::TokioIo;

    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("upstream listener should bind");
    let upstream_addr = upstream_listener
        .local_addr()
        .expect("upstream listener should have addr");
    let upstream_connection_count = Arc::new(AtomicUsize::new(0));
    let upstream_handle = tokio::spawn({
        let upstream_connection_count = Arc::clone(&upstream_connection_count);
        async move {
            loop {
                let (stream, _) = upstream_listener
                    .accept()
                    .await
                    .expect("upstream accept should succeed");
                upstream_connection_count.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    let service =
                        service_fn(|request: Request<hyper::body::Incoming>| async move {
                            let path = request.uri().path().to_string();
                            let mut response =
                                Response::new(Body::from(format!("Hello, World! {path}")));
                            response.headers_mut().insert(
                                "x-upstream-http1-keepalive",
                                "ok".parse().expect("header should parse"),
                            );
                            Ok::<_, Infallible>(response)
                        });
                    let builder = hyper::server::conn::http1::Builder::new();
                    if let Err(err) = builder
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        panic!("http1 upstream connection should serve: {err}");
                    }
                });
            }
        }
    });

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use proxy;

        let upstream = http::exchange::default_upstream();
        http::exchange::set_target(upstream, "127.0.0.1", {});
        proxy::forward_default_upstream([]);
    "#,
        upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    for path in ["/first", "/second"] {
        let response = client
            .get(format!("http://{data_addr}{path}"))
            .send()
            .await
            .expect("request should complete");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("x-upstream-http1-keepalive")
                .and_then(|value| value.to_str().ok()),
            Some("ok")
        );
        assert_eq!(
            response.text().await.expect("body should read"),
            format!("Hello, World! {path}")
        );
    }

    assert_eq!(
        upstream_connection_count.load(Ordering::Relaxed),
        1,
        "two proxied http/1.1 requests with non-empty upstream bodies should reuse one upstream connection",
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn http_request_body_chunk_api_reads_in_chunks() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = r#"
        use http;

        let first = http::request::body::next_chunk(4);
        let second = http::request::body::next_chunk(4);
        let rest = http::request::body::next_chunk(64);
        let done = http::request::body::eof();
        if done {
            http::response::set_body(first + second + rest);
        } else {
            http::response::set_body("body-not-finished");
        }
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/chunked"))
        .body("abcdefghij")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.text().await.expect("body should read"),
        "abcdefghij"
    );

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn sample_proxy_program_streams_or_buffers_upstream_body() {
    let (_upstream_addr, upstream_handle) = spawn_chunked_upstream_on(
        vec!["ab", "cd", "ef"],
        loopback_addr(SAMPLE_PROXY_UPSTREAM_PORT),
    )
    .await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("http")
        .join("proxy")
        .join("sample_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let streaming = client
        .get(format!("http://{data_addr}/proxy"))
        .header("Streaming", "1")
        .send()
        .await
        .expect("streaming request should complete");
    assert_eq!(streaming.status(), StatusCode::OK);
    assert_eq!(
        streaming
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/plain")
    );
    assert_eq!(
        streaming
            .headers()
            .get("x-stream")
            .and_then(|value| value.to_str().ok()),
        None
    );
    assert_eq!(
        streaming.text().await.expect("streaming body should read"),
        "abAcdAefA"
    );

    let buffered = client
        .get(format!("http://{data_addr}/proxy"))
        .send()
        .await
        .expect("buffered request should complete");
    assert_eq!(buffered.status(), StatusCode::OK);
    assert_eq!(
        buffered
            .headers()
            .get("x-proxy-status")
            .and_then(|value| value.to_str().ok()),
        Some("forwarded")
    );
    assert_eq!(
        buffered.text().await.expect("buffered body should read"),
        "abcdef"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn sample_request_transform_program_streams_or_buffers_downstream_request_body() {
    let upstream_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let body = to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        let mut response = Response::new(Body::from(body));
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain"),
        );
        response
    }));
    let (_upstream_addr, upstream_handle) = spawn_server_on(
        upstream_app,
        loopback_addr(SAMPLE_REQUEST_TRANSFORM_UPSTREAM_PORT),
    )
    .await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("http")
        .join("proxy")
        .join("sample_request_transform_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let transformed = client
        .post(format!("http://{data_addr}/transform"))
        .header("Chunk-Transform", "1")
        .body("abcdefghi")
        .send()
        .await
        .expect("transformed request should complete");
    assert_eq!(transformed.status(), StatusCode::OK);
    assert_eq!(
        transformed
            .headers()
            .get("x-request-stream")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        transformed
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/plain")
    );
    assert_eq!(
        transformed
            .headers()
            .get("x-proxy-status")
            .and_then(|value| value.to_str().ok()),
        Some("forwarded")
    );
    assert_eq!(
        transformed
            .text()
            .await
            .expect("transformed body should read"),
        "abc#def#ghi#"
    );

    let buffered = client
        .post(format!("http://{data_addr}/transform"))
        .body("abcdefghi")
        .send()
        .await
        .expect("buffered request should complete");
    assert_eq!(buffered.status(), StatusCode::OK);
    assert_eq!(
        buffered
            .headers()
            .get("x-request-stream")
            .and_then(|value| value.to_str().ok()),
        Some("false")
    );
    assert_eq!(
        buffered
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/plain")
    );
    assert_eq!(
        buffered
            .headers()
            .get("x-proxy-status")
            .and_then(|value| value.to_str().ok()),
        Some("forwarded")
    );
    assert_eq!(
        buffered.text().await.expect("buffered body should read"),
        "abcdefghi"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn sample_sse_proxy_program_mutates_each_upstream_event_before_returning() {
    let (_upstream_addr, upstream_handle) = spawn_sse_upstream_on(
        vec![
            "id: 1\n",
            "data: alpha\n",
            "\n",
            "id: 2\n",
            "data: beta\n",
            "\n",
        ],
        loopback_addr(SAMPLE_SSE_UPSTREAM_PORT),
    )
    .await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("http")
        .join("proxy")
        .join("sample_sse_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/sse"))
        .send()
        .await
        .expect("sse request should complete");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.text().await.expect("sse body should read");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(
        headers
            .get("x-sse-mutated")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        body,
        "id: 1 [mutated]\ndata: alpha [mutated]\n\nid: 2 [mutated]\ndata: beta [mutated]\n\n"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn direct_vm_can_read_upstream_response_line_by_line_via_io_handle_api() {
    let (upstream_addr, upstream_handle) =
        spawn_sse_upstream(vec!["id: 1\n", "data: alpha\n", "\n"]).await;
    let source = format!(
        r#"
        use http;
        use io;
        use tcp;

        let upstream = http::exchange::default_upstream();
        http::exchange::set_target(upstream, "127.0.0.1", {upstream_port});
        http::exchange::set_path(upstream, "/events");
        http::response::set_status(http::exchange::get_status(upstream));
        let downstream = tcp::stream::downstream();

        while !http::exchange::body::eof(upstream) {{
            let line = io::read_line(upstream);
            if line == "" {{
                tcp::stream::write(downstream, "\n");
            }} else {{
                tcp::stream::write(downstream, line + "\n");
            }}
        }}
    "#,
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
        .expect("direct vm run should succeed");

    upstream_handle.abort();
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn sample_subrequest_proxy_program_fans_out_across_default_and_dynamic_exchanges() {
    let plain_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let body = to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        Response::new(Body::from(format!(
            "plain:{}",
            String::from_utf8_lossy(&body)
        )))
    }));
    let (_plain_addr, plain_handle) =
        spawn_server_on(plain_app, loopback_addr(SAMPLE_SUBREQUEST_PRIMARY_PORT)).await;
    let (_secure_addr, secure_handle) =
        spawn_https_echo_upstream_on(loopback_addr(SAMPLE_SUBREQUEST_SECONDARY_PORT)).await;

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
        .join("http")
        .join("proxy")
        .join("sample_subrequest_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/fanout"))
        .send()
        .await
        .expect("fanout request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-secondary-peer")
            .and_then(|value| value.to_str().ok()),
        Some("localhost")
    );
    assert_eq!(
        response
            .headers()
            .get("x-secondary-alpn")
            .and_then(|value| value.to_str().ok()),
        Some("http/1.1")
    );
    assert_eq!(
        response.text().await.expect("response body should read"),
        "plain:alpha|beta"
    );

    plain_handle.abort();
    secure_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn http_exchange_supports_multiple_dynamic_subrequests_in_one_vm_run() {
    let first_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let body = to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        Response::new(Body::from(format!(
            "first:{}",
            String::from_utf8_lossy(&body)
        )))
    }));
    let second_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let body = to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        Response::new(Body::from(format!(
            "second:{}",
            String::from_utf8_lossy(&body)
        )))
    }));
    let (first_addr, first_handle) = spawn_server(first_app).await;
    let (second_addr, second_handle) = spawn_server(second_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tcp;

        let first = http::exchange::new();
        let second = http::exchange::new();
        if first == second {{
            http::response::set_status(500);
            http::response::set_body("same-handle");
        }} else {{
            http::exchange::set_target(first, "127.0.0.1", {first_port});
            http::exchange::set_path(first, "/one");
            http::exchange::set_target(second, "127.0.0.1", {second_port});
            http::exchange::set_path(second, "/two");
            tcp::stream::write(first, "one");
            tcp::stream::write(second, "two");
            http::response::set_body(
                http::exchange::get_body(first) + "|" + http::exchange::get_body(second)
            );
        }}
    "#,
        first_port = first_addr.port(),
        second_port = second_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/subrequests"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.text().await.expect("body should read"),
        "first:one|second:two"
    );

    first_handle.abort();
    second_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn dynamic_exchange_rejects_write_after_response_has_started() {
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

        let exchange = http::exchange::new();
        http::exchange::set_target(exchange, "127.0.0.1", {upstream_port});
        http::exchange::get_status(exchange);
        tcp::stream::write(exchange, "late");
    "#,
        upstream_port = upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/late-dynamic-write"))
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
async fn upstream_http2_response_version_is_exposed_to_vm_programs() {
    let (upstream_addr, _connection_count, upstream_handle) =
        spawn_https_http2_multiplex_upstream().await;

    let (data_addr, admin_addr, data_handle, admin_handle) =
        spawn_proxy_with_state(SharedState::new(1024 * 1024)).await;
    let client = reqwest::Client::new();

    let source = format!(
        r#"
        use http;
        use tls;

        let upstream = http::exchange::default_upstream();
        http::exchange::set_target(upstream, "127.0.0.1", {});
        http::exchange::set_scheme(upstream, "https");
        http::exchange::set_path(upstream, "/fast");
        let session = tls::session::from_socket(upstream);
        tls::session::set_verify(session, false);
        http::response::set_header(
            "x-upstream-version",
            http::exchange::get_http_version(upstream)
        );
        http::response::set_body(http::exchange::get_body(upstream));
    "#,
        upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/http2-default"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-upstream-version")
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

#[cfg(all(feature = "tls", feature = "http3"))]
#[tokio::test]
async fn upstream_http3_response_version_is_exposed_to_vm_programs() {
    let (upstream_addr, _connection_count, upstream_handle) =
        spawn_https_http3_multiplex_upstream().await;

    let (data_addr, admin_addr, data_handle, admin_handle) =
        spawn_proxy_with_state(SharedState::new(1024 * 1024)).await;
    let client = reqwest::Client::new();

    let source = format!(
        r#"
        use http;
        use tls;

        let upstream = http::exchange::default_upstream();
        http::exchange::set_target(upstream, "127.0.0.1", {});
        http::exchange::set_scheme(upstream, "https");
        http::exchange::set_path(upstream, "/fast");
        http::exchange::set_version(upstream, "3");
        let session = tls::session::from_socket(upstream);
        tls::session::set_verify(session, false);

        http::exchange::send(upstream);
        let body = http::exchange::get_body(upstream);
        http::response::set_header(
            "x-upstream-version",
            http::exchange::get_http_version(upstream)
        );
        http::response::set_header("x-upstream-alpn", tls::session::get_alpn(session));
        http::response::set_body(body);
    "#,
        upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/http3-default"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-upstream-version")
            .and_then(|value| value.to_str().ok()),
        Some("3")
    );
    assert_eq!(
        response
            .headers()
            .get("x-upstream-alpn")
            .and_then(|value| value.to_str().ok()),
        Some("h3")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "fast-body"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[cfg(feature = "http2")]
#[tokio::test]
async fn sample_downstream_http2_program_handles_cleartext_h2_requests() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("http")
        .join("downstream")
        .join("sample_downstream_http2_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let stream = tokio::net::TcpStream::connect(data_addr)
        .await
        .expect("http2 client should connect");
    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, connection) =
        hyper::client::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
            .handshake(io)
            .await
            .expect("http2 client handshake should succeed");
    let connection_handle = tokio::spawn(async move {
        connection
            .await
            .expect("http2 client connection should run");
    });

    let host = data_addr.to_string();
    let request = Request::builder()
        .method("POST")
        .uri(format!("http://{data_addr}/downstream-sample?mode=http2"))
        .version(axum::http::Version::HTTP_2)
        .header("host", &host)
        .body(http_body_util::Full::new(axum::body::Bytes::from_static(
            b"payload-2",
        )))
        .expect("http2 request should build");
    let response = sender
        .send_request(request)
        .await
        .expect("http2 request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response
            .headers()
            .get("x-request-version")
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
    assert_eq!(
        response
            .headers()
            .get("x-request-method")
            .and_then(|value| value.to_str().ok()),
        Some("POST")
    );
    assert_eq!(
        response
            .headers()
            .get("x-request-host")
            .and_then(|value| value.to_str().ok()),
        Some(host.as_str())
    );
    assert_eq!(
        response
            .headers()
            .get("x-request-carrier")
            .and_then(|value| value.to_str().ok()),
        Some("http2")
    );
    let body = http_body_util::BodyExt::collect(response.into_body())
        .await
        .expect("http2 response body should collect")
        .to_bytes();
    assert_eq!(body.as_ref(), b"h2:/downstream-sample?mode=http2|payload-2");

    connection_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[cfg(feature = "http3")]
#[tokio::test]
async fn sample_downstream_http3_program_handles_http3_requests() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_http3_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("http")
        .join("downstream")
        .join("sample_downstream_http3_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = send_http3_request(
        &format!(
            "https://127.0.0.1:{}/downstream-sample?mode=http3",
            data_addr.port()
        ),
        "POST",
        &[],
        b"payload-3",
    )
    .await;
    assert_eq!(response.status, axum::http::StatusCode::CREATED);
    assert_eq!(response.version, axum::http::Version::HTTP_3);
    assert_eq!(
        response
            .headers
            .get("x-request-version")
            .and_then(|value| value.to_str().ok()),
        Some("3")
    );
    assert_eq!(
        response
            .headers
            .get("x-request-method")
            .and_then(|value| value.to_str().ok()),
        Some("POST")
    );
    assert_eq!(
        response
            .headers
            .get("x-request-host")
            .and_then(|value| value.to_str().ok()),
        Some(format!("127.0.0.1:{}", data_addr.port()).as_str())
    );
    assert_eq!(
        response
            .headers
            .get("x-request-carrier")
            .and_then(|value| value.to_str().ok()),
        Some("http3")
    );
    assert_eq!(
        response.body.as_ref(),
        b"h3:/downstream-sample?mode=http3|payload-3"
    );

    data_handle.abort();
    admin_handle.abort();
}

#[cfg(all(feature = "tls", feature = "http2"))]
#[tokio::test]
async fn sample_upstream_http2_program_demonstrates_multiplex_and_reuse() {
    let (_upstream_addr, connection_count, upstream_handle) =
        spawn_https_http2_sample_upstream_on(loopback_addr(SAMPLE_UPSTREAM_HTTP2_PORT)).await;

    let (data_addr, admin_addr, data_handle, admin_handle) =
        spawn_proxy_with_state(SharedState::new(1024 * 1024)).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("http")
        .join("upstream")
        .join("sample_upstream_http2_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/upstream-http2-sample"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-multiplex-fast-version")
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
    assert_eq!(
        response
            .headers()
            .get("x-multiplex-slow-version")
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
    assert_eq!(
        response
            .headers()
            .get("x-multiplex-fast-alpn")
            .and_then(|value| value.to_str().ok()),
        Some("h2")
    );
    assert_eq!(
        response
            .headers()
            .get("x-multiplex-slow-alpn")
            .and_then(|value| value.to_str().ok()),
        Some("h2")
    );
    assert_eq!(
        response
            .headers()
            .get("x-multiplex-fast-eof")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        response
            .headers()
            .get("x-multiplex-slow-eof")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        response
            .headers()
            .get("x-reuse-version")
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
    assert_eq!(
        response
            .headers()
            .get("x-reuse-alpn")
            .and_then(|value| value.to_str().ok()),
        Some("h2")
    );
    assert_eq!(
        response
            .headers()
            .get("x-reuse-eof")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        response
            .headers()
            .get("x-sample-pattern")
            .and_then(|value| value.to_str().ok()),
        Some("two-requests-multiplex-then-reuse")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "multiplex:PUT|/fast|fast-request|beta|POST|/slow|slow-request|alpha;reuse:PATCH|/reuse|reuse-request|gamma"
    );
    assert_eq!(
        connection_count.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "sample should multiplex and reuse one upstream http2 connection",
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[cfg(all(feature = "tls", feature = "http3"))]
#[tokio::test]
async fn sample_upstream_http3_program_demonstrates_multiplex_and_reuse() {
    let (_upstream_addr, connection_count, upstream_handle) =
        spawn_https_http3_sample_upstream_on(loopback_addr(SAMPLE_UPSTREAM_HTTP3_PORT)).await;

    let (data_addr, admin_addr, data_handle, admin_handle) =
        spawn_proxy_with_state(SharedState::new(1024 * 1024)).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("http")
        .join("upstream")
        .join("sample_upstream_http3_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/upstream-http3-sample"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-multiplex-fast-version")
            .and_then(|value| value.to_str().ok()),
        Some("3")
    );
    assert_eq!(
        response
            .headers()
            .get("x-multiplex-slow-version")
            .and_then(|value| value.to_str().ok()),
        Some("3")
    );
    assert_eq!(
        response
            .headers()
            .get("x-multiplex-fast-alpn")
            .and_then(|value| value.to_str().ok()),
        Some("h3")
    );
    assert_eq!(
        response
            .headers()
            .get("x-multiplex-slow-alpn")
            .and_then(|value| value.to_str().ok()),
        Some("h3")
    );
    assert_eq!(
        response
            .headers()
            .get("x-multiplex-fast-eof")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        response
            .headers()
            .get("x-multiplex-slow-eof")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        response
            .headers()
            .get("x-reuse-version")
            .and_then(|value| value.to_str().ok()),
        Some("3")
    );
    assert_eq!(
        response
            .headers()
            .get("x-reuse-alpn")
            .and_then(|value| value.to_str().ok()),
        Some("h3")
    );
    assert_eq!(
        response
            .headers()
            .get("x-reuse-eof")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        response
            .headers()
            .get("x-sample-pattern")
            .and_then(|value| value.to_str().ok()),
        Some("two-requests-multiplex-then-reuse")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "multiplex:PUT|/fast|fast-request|beta|POST|/slow|slow-request|alpha;reuse:PATCH|/reuse|reuse-request|gamma"
    );
    assert_eq!(
        connection_count.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "sample should multiplex and reuse one upstream http3 connection",
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[cfg(all(feature = "tls", feature = "http2"))]
#[tokio::test]
async fn dynamic_exchanges_can_multiplex_over_single_http2_connection() {
    let (upstream_addr, connection_count, upstream_handle) =
        spawn_https_http2_multiplex_upstream().await;

    let (data_addr, admin_addr, data_handle, admin_handle) =
        spawn_proxy_with_state(SharedState::new(1024 * 1024)).await;
    let client = reqwest::Client::new();

    let source = format!(
        r#"
        use http;
        use tls;

        let first = http::exchange::new();
        let second = http::exchange::new();

        http::exchange::set_target(first, "localhost", {});
        http::exchange::set_scheme(first, "https");
        http::exchange::set_path(first, "/slow");
        http::exchange::set_target(second, "localhost", {});
        http::exchange::set_scheme(second, "https");
        http::exchange::set_path(second, "/fast");
        tls::session::set_verify(tls::session::from_socket(first), false);
        tls::session::set_verify(tls::session::from_socket(second), false);

        http::exchange::send(first);
        http::response::set_header("x-first-version", http::exchange::get_http_version(first));
        http::exchange::send(second);
        http::response::set_header("x-second-version", http::exchange::get_http_version(second));
        http::response::set_body(http::exchange::get_body(second) + "|" + http::exchange::get_body(first));
    "#,
        upstream_addr.port(),
        upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/http2-multiplex"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-first-version")
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
    assert_eq!(
        response
            .headers()
            .get("x-second-version")
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "fast-body|slow-body"
    );
    assert_eq!(
        connection_count.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "http2 exchanges should share one upstream connection",
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[cfg(all(feature = "tls", feature = "http3"))]
#[tokio::test]
async fn dynamic_exchanges_can_multiplex_over_single_http3_connection() {
    let (upstream_addr, connection_count, upstream_handle) =
        spawn_https_http3_multiplex_upstream().await;

    let (data_addr, admin_addr, data_handle, admin_handle) =
        spawn_proxy_with_state(SharedState::new(1024 * 1024)).await;
    let client = reqwest::Client::new();

    let source = format!(
        r#"
        use http;
        use tls;

        let first = http::exchange::new();
        let second = http::exchange::new();

        http::exchange::set_target(first, "127.0.0.1", {});
        http::exchange::set_scheme(first, "https");
        http::exchange::set_path(first, "/slow");
        http::exchange::set_version(first, "3");
        http::exchange::set_target(second, "127.0.0.1", {});
        http::exchange::set_scheme(second, "https");
        http::exchange::set_path(second, "/fast");
        http::exchange::set_version(second, "3");
        tls::session::set_verify(tls::session::from_socket(first), false);
        tls::session::set_verify(tls::session::from_socket(second), false);

        http::exchange::send(first);
        http::response::set_header("x-first-version", http::exchange::get_http_version(first));
        http::exchange::send(second);
        http::response::set_header("x-second-version", http::exchange::get_http_version(second));
        http::response::set_body(http::exchange::get_body(second) + "|" + http::exchange::get_body(first));
    "#,
        upstream_addr.port(),
        upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/http3-multiplex"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-first-version")
            .and_then(|value| value.to_str().ok()),
        Some("3")
    );
    assert_eq!(
        response
            .headers()
            .get("x-second-version")
            .and_then(|value| value.to_str().ok()),
        Some("3")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "fast-body|slow-body"
    );
    assert_eq!(
        connection_count.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "http3 exchanges should share one upstream connection",
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[cfg(all(feature = "tls", feature = "http2"))]
#[tokio::test]
async fn dynamic_exchange_body_chunks_can_be_read_independently_over_http2() {
    let (upstream_addr, connection_count, upstream_handle) =
        spawn_https_http2_multiplex_upstream().await;

    let (data_addr, admin_addr, data_handle, admin_handle) =
        spawn_proxy_with_state(SharedState::new(1024 * 1024)).await;
    let client = reqwest::Client::new();

    let source = format!(
        r#"
        use http;
        use tls;

        let slow = http::exchange::new();
        let fast = http::exchange::new();

        http::exchange::set_target(slow, "localhost", {});
        http::exchange::set_scheme(slow, "https");
        http::exchange::set_path(slow, "/slow");
        http::exchange::set_target(fast, "localhost", {});
        http::exchange::set_scheme(fast, "https");
        http::exchange::set_path(fast, "/fast");
        tls::session::set_verify(tls::session::from_socket(slow), false);
        tls::session::set_verify(tls::session::from_socket(fast), false);

        http::exchange::send(slow);
        http::exchange::send(fast);

        let fast_head = http::exchange::body::next_chunk(fast, 4);
        let slow_head = http::exchange::body::next_chunk(slow, 4);
        let fast_tail = http::exchange::body::next_chunk(fast, 32);
        let slow_tail = http::exchange::body::next_chunk(slow, 32);

        http::response::set_header("x-fast-version", http::exchange::get_http_version(fast));
        http::response::set_header("x-slow-version", http::exchange::get_http_version(slow));
        http::response::set_header(
            "x-fast-eof",
            if http::exchange::body::eof(fast) => {{ "true" }} else => {{ "false" }}
        );
        http::response::set_header(
            "x-slow-eof",
            if http::exchange::body::eof(slow) => {{ "true" }} else => {{ "false" }}
        );
        http::response::set_body(fast_head + "|" + slow_head + "|" + fast_tail + "|" + slow_tail);
    "#,
        upstream_addr.port(),
        upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/http2-chunks"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-fast-version")
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
    assert_eq!(
        response
            .headers()
            .get("x-slow-version")
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
    assert_eq!(
        response
            .headers()
            .get("x-fast-eof")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        response
            .headers()
            .get("x-slow-eof")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "fast|slow|-body|-body"
    );
    assert_eq!(
        connection_count.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "http2 chunk reads should stay on one upstream connection",
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[cfg(all(feature = "tls", feature = "http3"))]
#[tokio::test]
async fn dynamic_exchange_body_chunks_can_be_read_independently_over_http3() {
    let (upstream_addr, connection_count, upstream_handle) =
        spawn_https_http3_multiplex_upstream().await;

    let (data_addr, admin_addr, data_handle, admin_handle) =
        spawn_proxy_with_state(SharedState::new(1024 * 1024)).await;
    let client = reqwest::Client::new();

    let source = format!(
        r#"
        use http;
        use tls;

        let slow = http::exchange::new();
        let fast = http::exchange::new();

        http::exchange::set_target(slow, "127.0.0.1", {});
        http::exchange::set_scheme(slow, "https");
        http::exchange::set_path(slow, "/slow");
        http::exchange::set_version(slow, "3");
        http::exchange::set_target(fast, "127.0.0.1", {});
        http::exchange::set_scheme(fast, "https");
        http::exchange::set_path(fast, "/fast");
        http::exchange::set_version(fast, "3");
        tls::session::set_verify(tls::session::from_socket(slow), false);
        tls::session::set_verify(tls::session::from_socket(fast), false);

        http::exchange::send(slow);
        http::exchange::send(fast);

        let fast_head = http::exchange::body::next_chunk(fast, 4);
        let slow_head = http::exchange::body::next_chunk(slow, 4);
        let fast_tail = http::exchange::body::next_chunk(fast, 32);
        let slow_tail = http::exchange::body::next_chunk(slow, 32);

        http::response::set_header("x-fast-version", http::exchange::get_http_version(fast));
        http::response::set_header("x-slow-version", http::exchange::get_http_version(slow));
        http::response::set_header(
            "x-fast-eof",
            if http::exchange::body::eof(fast) => {{ "true" }} else => {{ "false" }}
        );
        http::response::set_header(
            "x-slow-eof",
            if http::exchange::body::eof(slow) => {{ "true" }} else => {{ "false" }}
        );
        http::response::set_body(fast_head + "|" + slow_head + "|" + fast_tail + "|" + slow_tail);
    "#,
        upstream_addr.port(),
        upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/http3-chunks"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-fast-version")
            .and_then(|value| value.to_str().ok()),
        Some("3")
    );
    assert_eq!(
        response
            .headers()
            .get("x-slow-version")
            .and_then(|value| value.to_str().ok()),
        Some("3")
    );
    assert_eq!(
        response
            .headers()
            .get("x-fast-eof")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        response
            .headers()
            .get("x-slow-eof")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "fast|slow|-body|-body"
    );
    assert_eq!(
        connection_count.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "http3 chunk reads should stay on one upstream connection",
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn downstream_http2_requests_expose_version_metadata_to_vm_programs() {
    let state = SharedState::new(1024 * 1024);
    let source = r#"
        use http;

        http::response::set_header("x-request-version", http::request::get_http_version());
        http::response::set_body("ok");
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let program_bytes = encode_program(&compiled.program).expect("program should encode");
    let report = edge::apply_program_from_bytes(&state, &program_bytes).await;
    assert!(report.applied, "program should apply");

    let app = build_http_proxy_app(state);
    let request = Request::builder()
        .method("GET")
        .uri("/http2-downstream")
        .version(axum::http::Version::HTTP_2)
        .header("host", "app.example.test")
        .body(Body::empty())
        .expect("request should build");
    let response = app
        .oneshot(request)
        .await
        .expect("in-process request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-request-version")
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read")
            .as_ref(),
        b"ok"
    );
}

#[cfg(feature = "http3")]
#[tokio::test]
async fn downstream_http3_requests_expose_version_metadata_to_vm_programs() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_http3_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = r#"
        use http;

        http::response::set_header("x-request-version", http::request::get_http_version());
        http::response::set_body("ok");
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = send_http3_request(
        &format!("https://127.0.0.1:{}/http3-downstream", data_addr.port()),
        "GET",
        &[],
        b"",
    )
    .await;
    assert_eq!(response.status, axum::http::StatusCode::OK);
    assert_eq!(response.version, axum::http::Version::HTTP_3);
    assert_eq!(
        response
            .headers
            .get("x-request-version")
            .and_then(|value| value.to_str().ok()),
        Some("3")
    );
    assert_eq!(response.body.as_ref(), b"ok");

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn same_vm_program_handles_downstream_http11_http2_and_http3_requests() {
    let state = SharedState::new(1024 * 1024);
    let source = r#"
        use http;

        http::response::set_status(201);
        http::response::set_header("x-request-version", http::request::get_http_version());
        http::response::set_header("x-method", http::request::get_method());
        http::response::set_header("x-host", http::request::get_host());
        http::response::set_body(
            http::request::get_path_with_query() + "|" + http::request::get_body()
        );
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let program_bytes = encode_program(&compiled.program).expect("program should encode");
    let report = edge::apply_program_from_bytes(&state, &program_bytes).await;
    assert!(report.applied, "program should apply");

    let http11_response = build_http_proxy_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/same-program?mode=http11")
                .version(axum::http::Version::HTTP_11)
                .header("host", "app.example.test")
                .body(Body::from("payload-11"))
                .expect("http1 request should build"),
        )
        .await
        .expect("http1 request should complete");
    assert_eq!(http11_response.status(), StatusCode::CREATED);
    assert_eq!(
        http11_response
            .headers()
            .get("x-request-version")
            .and_then(|value| value.to_str().ok()),
        Some("1.1")
    );
    assert_eq!(
        http11_response
            .headers()
            .get("x-method")
            .and_then(|value| value.to_str().ok()),
        Some("POST")
    );
    assert_eq!(
        http11_response
            .headers()
            .get("x-host")
            .and_then(|value| value.to_str().ok()),
        Some("app.example.test")
    );
    assert_eq!(
        to_bytes(http11_response.into_body(), usize::MAX)
            .await
            .expect("http1 body should read")
            .as_ref(),
        b"/same-program?mode=http11|payload-11"
    );

    let http2_response = build_http_proxy_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/same-program?mode=http2")
                .version(axum::http::Version::HTTP_2)
                .header("host", "app.example.test")
                .body(Body::from("payload-2"))
                .expect("http2 request should build"),
        )
        .await
        .expect("http2 request should complete");
    assert_eq!(http2_response.status(), StatusCode::CREATED);
    assert_eq!(
        http2_response
            .headers()
            .get("x-request-version")
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
    assert_eq!(
        http2_response
            .headers()
            .get("x-method")
            .and_then(|value| value.to_str().ok()),
        Some("POST")
    );
    assert_eq!(
        http2_response
            .headers()
            .get("x-host")
            .and_then(|value| value.to_str().ok()),
        Some("app.example.test")
    );
    assert_eq!(
        to_bytes(http2_response.into_body(), usize::MAX)
            .await
            .expect("http2 body should read")
            .as_ref(),
        b"/same-program?mode=http2|payload-2"
    );

    #[cfg(feature = "http3")]
    {
        let http3_response = build_http_proxy_app(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/same-program?mode=http3")
                    .version(axum::http::Version::HTTP_3)
                    .header("host", "app.example.test")
                    .body(Body::from("payload-3"))
                    .expect("http3 request should build"),
            )
            .await
            .expect("http3 request should complete");
        assert_eq!(http3_response.status(), StatusCode::CREATED);
        assert_eq!(
            http3_response
                .headers()
                .get("x-request-version")
                .and_then(|value| value.to_str().ok()),
            Some("3")
        );
        assert_eq!(
            http3_response
                .headers()
                .get("x-method")
                .and_then(|value| value.to_str().ok()),
            Some("POST")
        );
        assert_eq!(
            http3_response
                .headers()
                .get("x-host")
                .and_then(|value| value.to_str().ok()),
            Some("app.example.test")
        );
        assert_eq!(
            to_bytes(http3_response.into_body(), usize::MAX)
                .await
                .expect("http3 body should read")
                .as_ref(),
            b"/same-program?mode=http3|payload-3"
        );
    }
}

#[cfg(feature = "http2")]
#[tokio::test]
async fn data_plane_accepts_cleartext_http2_prior_knowledge_requests() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = r#"
        use http;

        http::response::set_header("x-request-version", http::request::get_http_version());
        http::response::set_body(http::request::get_body());
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let stream = tokio::net::TcpStream::connect(data_addr)
        .await
        .expect("http2 client should connect");
    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, connection) =
        hyper::client::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
            .handshake(io)
            .await
            .expect("http2 client handshake should succeed");
    let connection_handle = tokio::spawn(async move {
        connection
            .await
            .expect("http2 client connection should run");
    });

    let request = Request::builder()
        .method("POST")
        .uri(format!("http://{data_addr}/h2c"))
        .version(axum::http::Version::HTTP_2)
        .header("host", format!("{data_addr}"))
        .body(http_body_util::Full::new(axum::body::Bytes::from_static(
            b"h2c-body",
        )))
        .expect("http2 request should build");
    let response = sender
        .send_request(request)
        .await
        .expect("http2 request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-request-version")
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
    let body = http_body_util::BodyExt::collect(response.into_body())
        .await
        .expect("http2 response body should collect")
        .to_bytes();
    assert_eq!(body.as_ref(), b"h2c-body");

    connection_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn uploaded_program_with_locals_executes_successfully() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = r#"
        use http;

        let body = "from-local";
        http::response::set_body(body);
    "#;
    let compiled = compile_source(source).expect("source should compile");
    assert!(
        compiled.program.debug.is_some(),
        "compiled source should carry debug info"
    );

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.text().await.expect("body should read"),
        "from-local"
    );

    data_handle.abort();
    admin_handle.abort();
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn http_proxy_https_listener_defaults_to_http_semantics_for_http_only_programs() {
    let (_http_addr, https_addr, admin_addr, http_handle, https_handle, admin_handle) =
        spawn_http_https_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = r#"
        use http;

        http::response::set_body("hello over https");
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let https_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("https test client should build");
    let response = https_client
        .get(format!(
            "https://localhost:{}/direct-http",
            https_addr.port()
        ))
        .send()
        .await
        .expect("https request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.text().await.expect("response body should read"),
        "hello over https"
    );

    http_handle.abort();
    https_handle.abort();
    admin_handle.abort();
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn http_proxy_https_listener_returns_404_for_noop_program() {
    let (_http_addr, https_addr, admin_addr, http_handle, https_handle, admin_handle) =
        spawn_http_https_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = r#"
        let unused = 1;
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let https_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("https test client should build");
    let response = https_client
        .get(format!("https://localhost:{}/noop", https_addr.port()))
        .send()
        .await
        .expect("https request should complete");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.text().await.expect("response body should read"),
        "not found"
    );

    http_handle.abort();
    https_handle.abort();
    admin_handle.abort();
}

#[cfg(all(feature = "tls", feature = "http2"))]
#[tokio::test]
async fn http_proxy_https_listener_reuses_http2_connections_for_tls_upstream_programs() {
    use http_body_util::{BodyExt, Full};
    use hyper::Request;
    use hyper_util::rt::{TokioExecutor, TokioIo};

    ensure_rustls_provider();

    let (upstream_addr, connection_count, upstream_handle) =
        spawn_https_http2_sample_upstream().await;
    let (_http_addr, https_addr, admin_addr, http_handle, https_handle, admin_handle) =
        spawn_http_https_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tls;

        let upstream = http::exchange::default_upstream();
        http::exchange::set_target(upstream, "127.0.0.1", {});
        http::exchange::set_scheme(upstream, "https");
        http::exchange::set_method(upstream, http::request::get_method());
        http::exchange::set_path(upstream, http::request::get_path());
        http::exchange::set_body(upstream, http::request::get_body());
        http::exchange::set_header(upstream, "x-demo-request", http::request::get_path());

        let session = tls::session::from_socket(upstream);
        tls::session::set_verify(session, false);
        tls::session::set_alpn(session, "h2,http/1.1");
        http::exchange::set_version(upstream, "2");

        http::response::set_header("x-downstream-version", http::request::get_http_version());
        http::response::set_header("x-upstream-version", http::exchange::get_http_version(upstream));
        http::response::set_body(http::exchange::get_body(upstream));
    "#,
        upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PermissiveTestServerCertVerifier::new()))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));

    let stream = tokio::net::TcpStream::connect(https_addr)
        .await
        .expect("https listener should accept http2 tls clients");
    let server_name = rustls::pki_types::ServerName::try_from("localhost")
        .expect("server name should parse")
        .to_owned();
    let tls_stream = connector
        .connect(server_name, stream)
        .await
        .expect("https h2 tls handshake should succeed");
    assert_eq!(
        tls_stream
            .get_ref()
            .1
            .alpn_protocol()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned()),
        Some("h2".to_string())
    );

    let (mut sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake(TokioIo::new(tls_stream))
        .await
        .expect("http2 client handshake should succeed");
    let connection_handle = tokio::spawn(async move {
        connection
            .await
            .expect("http2 client connection should run");
    });

    for (path, body) in [("/perf-one", "one"), ("/perf-two", "two")] {
        let response = sender
            .send_request(
                Request::builder()
                    .method("POST")
                    .uri(format!("https://localhost:{}{path}", https_addr.port()))
                    .version(axum::http::Version::HTTP_2)
                    .header("host", format!("localhost:{}", https_addr.port()))
                    .body(Full::new(axum::body::Bytes::from(body.to_string())))
                    .expect("http2 request should build"),
            )
            .await
            .expect("http2 request should complete");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.version(), axum::http::Version::HTTP_2);
        assert_eq!(
            response
                .headers()
                .get("x-downstream-version")
                .and_then(|value| value.to_str().ok()),
            Some("2")
        );
        assert_eq!(
            response
                .headers()
                .get("x-upstream-version")
                .and_then(|value| value.to_str().ok()),
            Some("2")
        );
        let body_bytes = response
            .into_body()
            .collect()
            .await
            .expect("http2 response body should collect")
            .to_bytes();
        assert_eq!(
            body_bytes.as_ref(),
            format!("POST|{path}|{path}|{body}").as_bytes()
        );
    }

    assert_eq!(
        connection_count.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "two proxied https+h2 requests should reuse one upstream http2 connection",
    );

    connection_handle.abort();
    upstream_handle.abort();
    http_handle.abort();
    https_handle.abort();
    admin_handle.abort();
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn http_proxy_https_listener_ignores_handoff_import_when_program_enters_http_directly() {
    let (_http_addr, https_addr, admin_addr, http_handle, https_handle, admin_handle) =
        spawn_http_https_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = r#"
        use http;

        let mode = 0;
        if mode == 1 {
            http::downstream::attach_transport();
        }
        http::response::set_body("handoff import stays on HTTP path");
    "#;
    let compiled = compile_source(source).expect("source should compile");
    assert!(
        compiled
            .program
            .imports
            .iter()
            .any(|import| import.name == "http::downstream::attach_transport"),
        "program should import attach_transport for regression coverage"
    );
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let https_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("https test client should build");
    let response = https_client
        .get(format!(
            "https://localhost:{}/handoff-import-direct-http",
            https_addr.port()
        ))
        .send()
        .await
        .expect("https request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.text().await.expect("response body should read"),
        "handoff import stays on HTTP path"
    );

    http_handle.abort();
    https_handle.abort();
    admin_handle.abort();
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn http_proxy_https_listener_explicit_handoff_uses_listener_goal_before_raw_transport() {
    let (_http_addr, https_addr, admin_addr, http_handle, https_handle, admin_handle) =
        spawn_http_https_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = r#"
        use http;

        if http::request::get_scheme() == "tcp" {
            http::downstream::attach_transport();
        }

        http::response::set_body(http::request::get_scheme());
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let https_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("https test client should build");
    let response = https_client
        .get(format!(
            "https://localhost:{}/listener-goal-handoff",
            https_addr.port()
        ))
        .send()
        .await
        .expect("https request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.expect("body should read"), "https");

    http_handle.abort();
    https_handle.abort();
    admin_handle.abort();
}
