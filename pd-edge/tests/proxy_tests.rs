use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderValue, Request, Response, StatusCode},
    routing::{any, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use edge::{
    ActiveControlPlaneConfig, CommandResultPayload, ControlPlaneCommand, EdgeCommandResult,
    EdgePollRequest, EdgePollResponse, FN_HTTP_RESPONSE_SET_BODY, FN_HTTP_RESPONSE_SET_HEADER,
    FN_HTTP_UPSTREAM_REQUEST_SET_TARGET, SharedState, build_admin_app, build_data_app,
    spawn_active_control_plane_client,
};
use tokio::{sync::Notify, task::JoinHandle, time::timeout};
use vm::{BytecodeBuilder, Program, Value, compile_source, encode_program};

async fn spawn_server(app: Router) -> (SocketAddr, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("server should run");
    });
    (addr, handle)
}

async fn spawn_proxy(
    max_program_bytes: usize,
) -> (SocketAddr, SocketAddr, JoinHandle<()>, JoinHandle<()>) {
    let state = SharedState::new(max_program_bytes);
    let (data_addr, data_handle) = spawn_server(build_data_app(state.clone())).await;
    let (admin_addr, admin_handle) = spawn_server(build_admin_app(state)).await;
    (data_addr, admin_addr, data_handle, admin_handle)
}

fn build_short_circuit_program(body: &str, header: Option<(&str, &str)>) -> Program {
    let mut constants = Vec::new();
    let mut bc = BytecodeBuilder::new();

    if let Some((name, value)) = header {
        let name_index = constants.len() as u32;
        constants.push(Value::String(name.to_string()));
        let value_index = constants.len() as u32;
        constants.push(Value::String(value.to_string()));
        bc.ldc(name_index);
        bc.ldc(value_index);
        bc.call(FN_HTTP_RESPONSE_SET_HEADER, 2);
    }

    let body_index = constants.len() as u32;
    constants.push(Value::String(body.to_string()));
    bc.ldc(body_index);
    bc.call(FN_HTTP_RESPONSE_SET_BODY, 1);
    bc.ret();

    Program::new(constants, bc.finish())
}

fn build_upstream_program(upstream: &str, header: Option<(&str, &str)>) -> Program {
    let mut constants = Vec::new();
    let mut bc = BytecodeBuilder::new();

    let upstream_index = constants.len() as u32;
    constants.push(Value::String(upstream.to_string()));
    bc.ldc(upstream_index);
    bc.call(FN_HTTP_UPSTREAM_REQUEST_SET_TARGET, 1);

    if let Some((name, value)) = header {
        let name_index = constants.len() as u32;
        constants.push(Value::String(name.to_string()));
        let value_index = constants.len() as u32;
        constants.push(Value::String(value.to_string()));
        bc.ldc(name_index);
        bc.ldc(value_index);
        bc.call(FN_HTTP_RESPONSE_SET_HEADER, 2);
    }

    bc.ret();
    Program::new(constants, bc.finish())
}

async fn upload_program(
    client: &reqwest::Client,
    admin_addr: SocketAddr,
    program: &Program,
) -> reqwest::Response {
    let bytes = encode_program(program).expect("encode should succeed");
    client
        .put(format!("http://{admin_addr}/program"))
        .header("content-type", "application/octet-stream")
        .body(bytes)
        .send()
        .await
        .expect("upload request should complete")
}

fn reserve_tcp_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind should succeed");
    let addr = listener.local_addr().expect("local addr should exist");
    drop(listener);
    addr
}

async fn send_pdb_continue(addr: SocketAddr) {
    tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(addr).expect("debugger tcp should accept connections");
        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));

        let mut banner = [0u8; 512];
        let _ = stream.read(&mut banner);
        stream
            .write_all(b"continue\n")
            .expect("pdb continue command should be written");
    })
    .await
    .expect("pdb helper should not panic");
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
    assert_eq!(response.text().await.expect("body should read"), "/fixed");

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
async fn debug_session_lifecycle_endpoints_work() {
    let (_data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let debug_addr = reserve_tcp_addr();

    let start_response = client
        .put(format!("http://{admin_addr}/debug/session"))
        .header("content-type", "application/json")
        .body(format!(
            r#"{{"header_name":"x-debug","header_value":"on","tcp_addr":"{debug_addr}","stop_on_entry":true}}"#
        ))
        .send()
        .await
        .expect("start debug request should complete");
    assert_eq!(start_response.status(), StatusCode::CREATED);

    let status_response = client
        .get(format!("http://{admin_addr}/debug/session"))
        .send()
        .await
        .expect("status request should complete");
    assert_eq!(status_response.status(), StatusCode::OK);
    let status_body = status_response
        .text()
        .await
        .expect("status body should read");
    assert!(status_body.contains(r#""active":true"#));
    assert!(status_body.contains(r#""header_name":"x-debug""#));

    let stop_response = client
        .delete(format!("http://{admin_addr}/debug/session"))
        .send()
        .await
        .expect("stop request should complete");
    assert_eq!(stop_response.status(), StatusCode::NO_CONTENT);

    let status_after_stop = client
        .get(format!("http://{admin_addr}/debug/session"))
        .send()
        .await
        .expect("status request should complete");
    assert_eq!(status_after_stop.status(), StatusCode::OK);
    let status_after_stop_body = status_after_stop
        .text()
        .await
        .expect("status body should read");
    assert!(status_after_stop_body.contains(r#""active":false"#));

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn tiny_language_can_enforce_simple_rate_limit() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = r#"
        use vm;

        if vm::http::rate_limit::allow(vm::http::request::get_header("x-client-id"), 2, 60) {
            vm::http::response::set_header("x-vm", "allowed");
            vm::http::response::set_body("ok");
        } else {
            vm::http::response::set_header("x-vm", "rate-limited");
            vm::http::response::set_body("blocked");
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
        use vm;

        let client_id = vm::http::request::get_header("x-client-id");
        if vm::http::rate_limit::allow(client_id, 1, 60) {{
            vm::http::upstream::request::set_path("/rewritten");
            vm::http::upstream::request::set_query("from=vm");
            vm::http::upstream::request::set_header("x-added", "yes");
            vm::http::upstream::request::set_target("{upstream_addr}");
        }} else {{
            vm::http::response::set_status(429);
            vm::http::response::set_body("blocked");
        }}
    "#
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
        use vm;

        vm::http::upstream::request::set_body("rewritten-body");
        vm::http::upstream::request::set_target("{upstream_addr}");
    "#
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
async fn debug_attached_request_does_not_block_non_debug_requests() {
    timeout(Duration::from_secs(10), async {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let program = build_short_circuit_program("ok", None);
    let upload = upload_program(&client, admin_addr, &program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let debug_addr = reserve_tcp_addr();
    let start_response = client
        .put(format!("http://{admin_addr}/debug/session"))
        .header("content-type", "application/json")
        .body(format!(
            r#"{{"header_name":"x-debug","header_value":"on","tcp_addr":"{debug_addr}","stop_on_entry":true}}"#
        ))
        .send()
        .await
        .expect("start debug request should complete");
    assert_eq!(start_response.status(), StatusCode::CREATED);

    let debug_client = client.clone();
    let debug_request = tokio::spawn(async move {
        let response = debug_client
            .get(format!("http://{data_addr}/debug-target"))
            .header("x-debug", "on")
            .send()
            .await
            .expect("debug request should complete");
        let status = response.status();
        let body = response.text().await.expect("body should read");
        (status, body)
    });

    tokio::time::sleep(Duration::from_millis(150)).await;

    let non_debug = timeout(
        Duration::from_secs(2),
        client.get(format!("http://{data_addr}/normal")).send(),
    )
    .await
    .expect("non-debug request timed out")
    .expect("non-debug request should complete");
    assert_eq!(non_debug.status(), StatusCode::OK);
    assert_eq!(non_debug.text().await.expect("body should read"), "ok");

    send_pdb_continue(debug_addr).await;

    let (status, body) = timeout(Duration::from_secs(2), debug_request)
        .await
        .expect("debug request timed out")
        .expect("debug task should join");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");

    data_handle.abort();
    admin_handle.abort();
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn uploaded_program_with_locals_executes_successfully() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = r#"
        use vm;

        let body = "from-local";
        vm::http::response::set_body(body);
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

#[derive(Clone)]
struct MockControlPlaneState {
    command: Arc<Mutex<Option<ControlPlaneCommand>>>,
    results: Arc<Mutex<Vec<EdgeCommandResult>>>,
    notified: Arc<Notify>,
}

impl MockControlPlaneState {
    fn new(command: Option<ControlPlaneCommand>) -> Self {
        Self {
            command: Arc::new(Mutex::new(command)),
            results: Arc::new(Mutex::new(Vec::new())),
            notified: Arc::new(Notify::new()),
        }
    }
}

async fn poll_rpc_handler(
    State(state): State<MockControlPlaneState>,
    Json(_request): Json<EdgePollRequest>,
) -> Json<EdgePollResponse> {
    let command = state.command.lock().expect("command lock").take();
    Json(EdgePollResponse {
        command,
        poll_interval_ms: 100,
    })
}

async fn result_rpc_handler(
    State(state): State<MockControlPlaneState>,
    Json(result): Json<EdgeCommandResult>,
) -> StatusCode {
    state.results.lock().expect("results lock").push(result);
    state.notified.notify_waiters();
    StatusCode::NO_CONTENT
}

#[tokio::test]
async fn active_control_plane_can_push_program_and_receive_result() {
    let program = build_short_circuit_program("from-active-control-plane", None);
    let program_bytes = encode_program(&program).expect("encode should succeed");
    let command = ControlPlaneCommand::ApplyProgram {
        command_id: "cmd-apply-1".to_string(),
        program_base64: STANDARD.encode(program_bytes),
    };

    let mock_state = MockControlPlaneState::new(Some(command));
    let control_app = Router::new()
        .route("/rpc/v1/edge/poll", post(poll_rpc_handler))
        .route("/rpc/v1/edge/result", post(result_rpc_handler))
        .with_state(mock_state.clone());
    let (rpc_addr, rpc_handle) = spawn_server(control_app).await;

    let state = SharedState::new(1024 * 1024);
    let (data_addr, data_handle) = spawn_server(build_data_app(state.clone())).await;
    let active_handle = spawn_active_control_plane_client(
        state.clone(),
        ActiveControlPlaneConfig {
            control_plane_url: format!("http://{rpc_addr}"),
            edge_id: "dp-test-1".to_string(),
            edge_name: "dp-test-friendly".to_string(),
            poll_interval_ms: 100,
            request_timeout_ms: 2_000,
        },
    );

    timeout(Duration::from_secs(3), mock_state.notified.notified())
        .await
        .expect("control plane should receive a result");

    let first_result = {
        let results = mock_state.results.lock().expect("results lock");
        results
            .first()
            .cloned()
            .expect("at least one result should exist")
    };
    assert_eq!(first_result.command_id, "cmd-apply-1");
    assert!(first_result.ok);
    match first_result.result {
        CommandResultPayload::ApplyProgram { report } => {
            assert!(report.applied);
            assert_eq!(report.message, None);
        }
        other => panic!("unexpected result payload: {other:?}"),
    }

    let client = reqwest::Client::new();
    let response = client
        .get(format!("http://{data_addr}/"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.text().await.expect("body should read"),
        "from-active-control-plane"
    );

    active_handle.abort();
    data_handle.abort();
    rpc_handle.abort();
}

#[tokio::test]
async fn debug_session_is_removed_after_debugger_disconnects() {
    timeout(Duration::from_secs(10), async {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let program = build_short_circuit_program("ok", None);
    let upload = upload_program(&client, admin_addr, &program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let debug_addr = reserve_tcp_addr();
    let start_response = client
        .put(format!("http://{admin_addr}/debug/session"))
        .header("content-type", "application/json")
        .body(format!(
            r#"{{"header_name":"x-debug","header_value":"on","tcp_addr":"{debug_addr}","stop_on_entry":true}}"#
        ))
        .send()
        .await
        .expect("start debug request should complete");
    assert_eq!(start_response.status(), StatusCode::CREATED);

    let debug_client = client.clone();
    let debug_request = tokio::spawn(async move {
        let response = debug_client
            .get(format!("http://{data_addr}/debug-target"))
            .header("x-debug", "on")
            .send()
            .await
            .expect("debug request should complete");
        let status = response.status();
        let body = response.text().await.expect("body should read");
        (status, body)
    });

    tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(debug_addr).expect("debugger tcp should accept");
        let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
        let mut buffer = [0u8; 256];
        let _ = stream.read(&mut buffer);
    })
    .await
    .expect("debugger helper should not panic");

    let (status, body) = timeout(Duration::from_secs(2), debug_request)
        .await
        .expect("debug request timed out")
        .expect("debug task should join");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");

    let mut inactive = false;
    for _ in 0..20 {
        let response = client
            .get(format!("http://{admin_addr}/debug/session"))
            .send()
            .await
            .expect("status request should complete");
        let body = response.text().await.expect("status body should read");
        if body.contains(r#""active":false"#) {
            inactive = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(inactive, "debug session should be cleaned up after detach");

    data_handle.abort();
    admin_handle.abort();
    })
    .await
    .expect("test timed out");
}
