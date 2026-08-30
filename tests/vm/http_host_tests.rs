#![cfg(all(feature = "http-client", not(target_family = "wasm")))]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::task::{Context, Poll};
use std::thread;
use std::time::{Duration, Instant};

use vm::{
    CallOutcome, CallReturn, HostAsyncBridge, HostFunctionRegistry, HostFuture, HostFutureOutput,
    HostOpId, HttpConfig, HttpHostExt, Program, Value, Vm, VmError, VmResult, VmStatus,
    compile_source,
};

#[derive(Default)]
struct TokioHostDriver {
    submitted: HashMap<HostOpId, HostFuture>,
}

impl HostAsyncBridge for TokioHostDriver {
    fn submit_op(&mut self, op_id: HostOpId, future: HostFuture) -> VmResult<()> {
        self.submitted.insert(op_id, future);
        Ok(())
    }

    fn poll_op(&mut self, op_id: HostOpId, _cx: &mut Context<'_>) -> Poll<VmResult<CallReturn>> {
        Poll::Ready(Err(VmError::HostError(format!(
            "unknown external host operation {op_id}"
        ))))
    }

    fn poll_submitted_op(
        &mut self,
        op_id: HostOpId,
        cx: &mut Context<'_>,
    ) -> Poll<VmResult<HostFutureOutput>> {
        let poll = self.submitted.get_mut(&op_id).map_or_else(
            || {
                Poll::Ready(Err(VmError::HostError(format!(
                    "unknown submitted host operation {op_id}"
                ))))
            },
            |future| future.as_mut().poll(cx),
        );
        if poll.is_ready() {
            self.submitted.remove(&op_id);
        }
        poll
    }

    fn cancel_op(&mut self, op_id: HostOpId) {
        self.submitted.remove(&op_id);
    }
}

fn install_host_driver(vm: &mut Vm) {
    vm.set_async_bridge(Box::<TokioHostDriver>::default())
        .expect("test async bridge should install");
}

fn build_request_program(url: String) -> Program {
    compile_source(&format!(
        r#"
        use http;
        http::client::request({{"method": "GET", "url": "{url}"}});
        "#
    ))
    .expect("HTTP request source should compile")
    .program
}

fn build_request_program_with_method(url: &str, method: &str) -> Program {
    compile_source(&format!(
        r#"
        use http;
        http::client::request({{"method": "{method}", "url": "{url}", "body": "payload"}});
        "#
    ))
    .expect("HTTP request source should compile")
    .program
}

fn build_request_program_with_headers(url: &str, method: &str) -> Program {
    compile_source(&format!(
        r#"
        use http;
        http::client::request({{"method": "{method}", "url": "{url}", "body": "payload", "headers": {{
            Authorization: "Bearer secret",
            "Proxy-Authorization": "Basic proxy-secret",
            Cookie: "a=b",
            "X-Api-Key": "api-secret",
            "X-Arbitrary": "custom-secret",
            "Content-Type": "application/body",
            Accept: "application/json",
            "Accept-Language": "en-US",
            "Accept-Encoding": "identity"
        }}}});
        "#
    ))
    .expect("HTTP request source with headers should compile")
    .program
}

const TEST_IO_TIMEOUT: Duration = Duration::from_secs(5);

fn bind_test_listener() -> TcpListener {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("test listener should bind");
    listener
        .set_nonblocking(true)
        .expect("test listener should be nonblocking");
    listener
}

fn accept_test_connection(
    listener: &TcpListener,
) -> std::io::Result<(TcpStream, std::net::SocketAddr)> {
    let deadline = Instant::now() + TEST_IO_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((stream, address)) => {
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(TEST_IO_TIMEOUT))?;
                stream.set_write_timeout(Some(TEST_IO_TIMEOUT))?;
                return Ok((stream, address));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "test server accept timed out",
                    ));
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error),
        }
    }
}

fn local_http_config(port: u16) -> HttpConfig {
    HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        ..HttpConfig::default()
    }
}

fn spawn_test_server() -> (u16, thread::JoinHandle<()>) {
    let listener = bind_test_listener();
    let port = listener
        .local_addr()
        .expect("test listener should have an address")
        .port();
    let handle = thread::spawn(move || {
        let (mut stream, _) =
            accept_test_connection(&listener).expect("test request should arrive");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream
                .read(&mut buffer)
                .expect("request should be readable");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        assert!(request.starts_with(b"GET / HTTP/1.1"));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nX-Test: yes\r\n\r\nok")
            .expect("response should be writable");
    });
    (port, handle)
}

fn spawn_response_server(response: Vec<u8>) -> (u16, thread::JoinHandle<()>) {
    let listener = bind_test_listener();
    let port = listener
        .local_addr()
        .expect("response listener should have an address")
        .port();
    let handle = thread::spawn(move || {
        let (mut stream, _) =
            accept_test_connection(&listener).expect("response request should arrive");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream
                .read(&mut buffer)
                .expect("response request should be readable");
            assert!(read > 0, "request ended before its headers");
            request.extend_from_slice(&buffer[..read]);
        }
        let _ = stream.write_all(&response);
    });
    (port, handle)
}

fn response_head_with_size(size: usize) -> Vec<u8> {
    let prefix = b"HTTP/1.1 204 No Content\r\nX-Pad: ";
    let suffix = b"\r\n\r\n";
    let value_len = size
        .checked_sub(prefix.len() + suffix.len())
        .expect("response-head test size should fit its framing");
    let mut response = Vec::with_capacity(size);
    response.extend_from_slice(prefix);
    response.extend(std::iter::repeat_n(b'a', value_len));
    response.extend_from_slice(suffix);
    assert_eq!(response.len(), size);
    response
}

fn spawn_redirect_server(
    status: u16,
    redirects: usize,
) -> (u16, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let listener = bind_test_listener();
    let port = listener
        .local_addr()
        .expect("redirect listener should have an address")
        .port();
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        for index in 0..=redirects {
            let (mut stream, _) =
                accept_test_connection(&listener).expect("redirect request should arrive");
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                stream
                    .read_exact(&mut byte)
                    .expect("redirect request headers should be readable");
                request.push(byte[0]);
            }
            let head = String::from_utf8(request).expect("request should be valid UTF-8");
            let content_length = head
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().expect("valid content length"))
                    })
                })
                .unwrap_or(0);
            let mut body = vec![0; content_length];
            stream
                .read_exact(&mut body)
                .expect("redirect request body should be readable");
            sender
                .send(format!("{head}{}", String::from_utf8_lossy(&body)))
                .expect("request should be recorded");
            if index < redirects {
                let location = if index + 1 == redirects {
                    format!("http://127.0.0.1:{port}/final")
                } else {
                    format!("http://127.0.0.1:{port}/hop/{index}")
                };
                write!(
                    stream,
                    "HTTP/1.1 {status} Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n"
                )
                .expect("redirect response should be writable");
            } else {
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .expect("final response should be writable");
            }
        }
    });
    (port, receiver, handle)
}

fn read_recorded_request(stream: &mut std::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        stream
            .read_exact(&mut byte)
            .expect("request headers should be readable");
        request.push(byte[0]);
    }
    let head = String::from_utf8(request).expect("request should be valid UTF-8");
    let content_length = head
        .lines()
        .find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("valid content length"))
            })
        })
        .unwrap_or(0);
    let mut body = vec![0; content_length];
    stream
        .read_exact(&mut body)
        .expect("request body should be readable");
    format!("{head}{}", String::from_utf8_lossy(&body))
}

fn header_value<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request
        .split("\r\n")
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .find_map(|(header_name, value)| {
            header_name
                .eq_ignore_ascii_case(name)
                .then_some(value.trim())
        })
}

fn has_header(request: &str, name: &str) -> bool {
    header_value(request, name).is_some()
}

fn request_line(request: &str) -> &str {
    request.split_once("\r\n").map_or(request, |(line, _)| line)
}

fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn spawn_cross_origin_redirect_servers(
    status: u16,
) -> (
    u16,
    u16,
    mpsc::Receiver<String>,
    mpsc::Receiver<String>,
    thread::JoinHandle<()>,
    thread::JoinHandle<()>,
) {
    let target_listener = bind_test_listener();
    let target_port = target_listener
        .local_addr()
        .expect("target should have an address")
        .port();
    let source_listener = bind_test_listener();
    let source_port = source_listener
        .local_addr()
        .expect("source should have an address")
        .port();
    let (source_sender, source_requests) = mpsc::channel();
    let (target_sender, target_requests) = mpsc::channel();
    let source_handle = thread::spawn(move || {
        let (mut stream, _) =
            accept_test_connection(&source_listener).expect("source request should arrive");
        let request = read_recorded_request(&mut stream);
        source_sender.send(request).expect("source request record");
        write!(
            stream,
            "HTTP/1.1 {status} Redirect\r\nLocation: http://127.0.0.1:{target_port}/final\r\nContent-Length: 0\r\n\r\n"
        )
        .expect("redirect response should be writable");
    });
    let target_handle = thread::spawn(move || {
        let (mut stream, _) =
            accept_test_connection(&target_listener).expect("target request should arrive");
        let request = read_recorded_request(&mut stream);
        target_sender.send(request).expect("target request record");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .expect("final response should be writable");
    });
    (
        source_port,
        target_port,
        source_requests,
        target_requests,
        source_handle,
        target_handle,
    )
}

fn response_field<'a>(value: &'a Value, key: &str) -> &'a Value {
    let Value::Map(map) = value else {
        panic!("expected response map, got {value:?}");
    };
    map.get(&Value::string(key))
        .unwrap_or_else(|| panic!("response missing field {key}"))
}

async fn drive_vm_to_halt(vm: &mut Vm) -> Result<(), vm::VmError> {
    let mut status = vm.run()?;
    loop {
        match status {
            VmStatus::Halted => return Ok(()),
            VmStatus::Yielded => status = vm.resume()?,
            VmStatus::Waiting(_) => {
                vm.await_waiting_host_op().await?;
                status = vm.resume()?;
            }
        }
    }
}

async fn run_raw_response(response: Vec<u8>, mut config: HttpConfig) -> Result<Value, VmError> {
    let (port, server) = spawn_response_server(response);
    config.allowed_schemes = vec!["http".to_string()];
    config.allowed_hosts = vec!["127.0.0.1".to_string()];
    config.allowed_ports = vec![port];
    config.allow_private_ips = true;
    let mut vm = Vm::new(build_request_program(format!("http://127.0.0.1:{port}/")));
    vm.configure_http(config)
        .expect("raw-response HTTP configuration should be valid");
    install_host_driver(&mut vm);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default host registry should bind HTTP");
    let outcome = drive_vm_to_halt(&mut vm).await;
    server.join().expect("raw response server should finish");
    outcome.map(|()| vm.stack()[0].clone())
}

#[tokio::test(flavor = "current_thread")]
async fn http_host_executes_a_bounded_request_and_returns_a_response_map() {
    let (port, server) = spawn_test_server();
    let mut vm = Vm::new(build_request_program(format!("http://127.0.0.1:{port}/")));
    vm.configure_http(local_http_config(port))
        .expect("HTTP configuration should be valid");
    install_host_driver(&mut vm);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default host registry should bind HTTP");

    drive_vm_to_halt(&mut vm)
        .await
        .expect("http request should complete");
    server.join().expect("test server should finish");

    assert_eq!(response_field(&vm.stack()[0], "status"), &Value::Int(200));
    assert_eq!(
        response_field(&vm.stack()[0], "body"),
        &Value::bytes(b"ok".to_vec())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn buffered_redirect_rewrites_only_post_for_301_and_302() {
    for status in [301, 302] {
        for method in ["POST", "PUT", "PATCH", "DELETE", "OPTIONS"] {
            let (port, requests, server) = spawn_redirect_server(status, 1);
            let mut vm = Vm::new(build_request_program_with_method(
                &format!("http://127.0.0.1:{port}/start"),
                method,
            ));
            vm.configure_http(local_http_config(port))
                .expect("HTTP configuration should be valid");
            install_host_driver(&mut vm);
            HostFunctionRegistry::new()
                .bind_vm_cached(&mut vm)
                .expect("default host registry should bind HTTP");

            drive_vm_to_halt(&mut vm)
                .await
                .expect("redirected request should complete");
            assert_eq!(response_field(&vm.stack()[0], "status"), &Value::Int(200));

            let first = requests.recv().expect("initial request should be recorded");
            let second = requests
                .recv()
                .expect("redirected request should be recorded");
            assert!(
                request_line(&first).starts_with(&format!("{method} /start HTTP/1.1")),
                "status {status}, method {method}: <redacted>"
            );
            let expected_method = if method == "POST" { "GET" } else { method };
            assert!(
                request_line(&second).starts_with(&format!("{expected_method} /final HTTP/1.1")),
                "status {status}, method {method}: <redacted>"
            );
            if method == "POST" {
                assert!(!second.ends_with("payload"), "status {status}: <redacted>");
            } else {
                assert!(second.ends_with("payload"), "status {status}: <redacted>");
            }
            server.join().expect("redirect server should finish");
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn buffered_cross_origin_redirect_strips_credentials_and_custom_headers() {
    for status in [301, 302, 303, 307, 308] {
        let (
            source_port,
            target_port,
            source_requests,
            target_requests,
            source_server,
            target_server,
        ) = spawn_cross_origin_redirect_servers(status);
        let mut http_config = local_http_config(source_port);
        http_config.allowed_ports.push(target_port);
        let mut vm = Vm::new(build_request_program_with_headers(
            &format!("http://127.0.0.1:{source_port}/start"),
            "POST",
        ));
        vm.configure_http(http_config)
            .expect("HTTP configuration should be valid");
        install_host_driver(&mut vm);
        HostFunctionRegistry::new()
            .bind_vm_cached(&mut vm)
            .expect("default host registry should bind HTTP");

        drive_vm_to_halt(&mut vm)
            .await
            .expect("cross-origin redirect should complete");
        assert_eq!(response_field(&vm.stack()[0], "status"), &Value::Int(200));

        let first = source_requests
            .recv()
            .expect("initial request should be recorded");
        assert_eq!(header_value(&first, "authorization"), Some("Bearer secret"));
        assert_eq!(
            header_value(&first, "proxy-authorization"),
            Some("Basic proxy-secret")
        );
        assert_eq!(header_value(&first, "cookie"), Some("a=b"));
        assert_eq!(header_value(&first, "x-api-key"), Some("api-secret"));
        assert_eq!(header_value(&first, "x-arbitrary"), Some("custom-secret"));

        let second = target_requests
            .recv()
            .expect("redirected request should be recorded");
        let expected_method = if status == 307 || status == 308 {
            "POST"
        } else {
            "GET"
        };
        assert!(
            request_line(&second).starts_with(&format!("{expected_method} /final HTTP/1.1")),
            "status {status}: <redacted>"
        );
        let expected_host = format!("127.0.0.1:{target_port}");
        assert_eq!(
            header_value(&second, "host"),
            Some(expected_host.as_str()),
            "status {status}: authority was not rebuilt"
        );
        assert_eq!(header_value(&second, "transfer-encoding"), None);
        if expected_method == "POST" {
            assert!(second.ends_with("payload"), "status {status}: <redacted>");
        } else {
            assert!(!second.ends_with("payload"), "status {status}: <redacted>");
            assert_eq!(header_value(&second, "content-type"), None);
            assert_eq!(header_value(&second, "transfer-encoding"), None);
            assert!(
                header_value(&second, "content-length").is_none_or(|value| value == "0"),
                "status {status}: stale content length in <redacted>"
            );
        }
        for forbidden in [
            "authorization",
            "proxy-authorization",
            "cookie",
            "x-api-key",
            "x-arbitrary",
        ] {
            assert!(
                !has_header(&second, forbidden),
                "status {status}, forbidden {forbidden}: <redacted>"
            );
        }
        for safe in ["accept", "accept-language", "accept-encoding"] {
            assert!(
                has_header(&second, safe),
                "status {status}, safe {safe}: <redacted>"
            );
        }
        source_server
            .join()
            .expect("source redirect server should finish");
        target_server.join().expect("target server should finish");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn buffered_same_origin_redirect_preserves_caller_header_values() {
    for status in [301, 302, 303, 307, 308] {
        let (port, requests, server) = spawn_redirect_server(status, 1);
        let mut vm = Vm::new(build_request_program_with_headers(
            &format!("http://127.0.0.1:{port}/start"),
            "POST",
        ));
        vm.configure_http(local_http_config(port))
            .expect("HTTP configuration should be valid");
        install_host_driver(&mut vm);
        HostFunctionRegistry::new()
            .bind_vm_cached(&mut vm)
            .expect("default host registry should bind HTTP");
        drive_vm_to_halt(&mut vm)
            .await
            .expect("same-origin redirect should complete");

        let first = requests.recv().expect("initial request should be recorded");
        let second = requests
            .recv()
            .expect("redirected request should be recorded");
        assert_eq!(header_value(&first, "authorization"), Some("Bearer secret"));
        assert_eq!(
            header_value(&first, "proxy-authorization"),
            Some("Basic proxy-secret")
        );
        assert_eq!(header_value(&first, "cookie"), Some("a=b"));
        assert_eq!(header_value(&first, "x-api-key"), Some("api-secret"));
        assert_eq!(header_value(&first, "x-arbitrary"), Some("custom-secret"));

        for name in [
            "authorization",
            "proxy-authorization",
            "cookie",
            "x-api-key",
            "x-arbitrary",
        ] {
            assert_eq!(
                header_value(&second, name),
                header_value(&first, name),
                "status {status}, header {name}"
            );
        }
        let rewrites = status == 301 || status == 302 || status == 303;
        assert_eq!(
            header_value(&second, "content-type").is_some(),
            !rewrites,
            "status {status}: stale body header in <redacted>"
        );
        server
            .join()
            .expect("same-origin redirect server should finish");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn buffered_response_limits_reject_adversarial_framing_and_exact_head_overflow() {
    let mut oversized_trailers =
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n".to_vec();
    for index in 0..70 {
        oversized_trailers
            .extend_from_slice(format!("X-Trailer-{index}: {}\r\n", "a".repeat(1024)).as_bytes());
    }
    oversized_trailers.extend_from_slice(b"\r\n");

    let cases = [
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n"
                .to_vec(),
            "response body",
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
                .to_vec(),
            "http",
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nZ\r\nnope\r\n0\r\n\r\n".to_vec(),
            "http",
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: nope\r\n\r\n".to_vec(),
            "http",
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 3\r\n\r\nabc".to_vec(),
            "http",
        ),
        (
            b"HTTP/1.1 200 OK\r\nBroken-Header\r\nContent-Length: 0\r\n\r\n".to_vec(),
            "http",
        ),
        (oversized_trailers, "response"),
    ];
    for (response, expected) in cases {
        let config = HttpConfig {
            max_response_body_bytes: 4,
            ..HttpConfig::default()
        };
        let error = run_raw_response(response, config)
            .await
            .expect_err("adversarial response must be rejected");
        assert!(
            contains_ascii_case_insensitive(&error.to_string(), expected),
            "expected {expected} error, got {error}"
        );
    }

    let exact = run_raw_response(response_head_with_size(64 * 1024), HttpConfig::default())
        .await
        .expect("a response head at the exact limit should be accepted");
    assert_eq!(response_field(&exact, "status"), &Value::Int(204));

    let error = run_raw_response(
        response_head_with_size(64 * 1024 + 1),
        HttpConfig::default(),
    )
    .await
    .expect_err("a response head over the limit must be rejected");
    assert!(
        error.to_string().contains("response head") || error.to_string().contains("connection"),
        "unexpected oversized-head error: {error}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn buffered_redirect_chain_reaches_final_body() {
    let (port, requests, server) = spawn_redirect_server(307, 2);
    let mut vm = Vm::new(build_request_program(format!(
        "http://127.0.0.1:{port}/start"
    )));
    vm.configure_http(local_http_config(port))
        .expect("HTTP configuration should be valid");
    install_host_driver(&mut vm);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default host registry should bind HTTP");

    drive_vm_to_halt(&mut vm)
        .await
        .expect("redirect chain should complete");
    assert_eq!(response_field(&vm.stack()[0], "status"), &Value::Int(200));
    assert_eq!(
        response_field(&vm.stack()[0], "body"),
        &Value::bytes(b"ok".to_vec())
    );
    for _ in 0..3 {
        requests
            .recv()
            .expect("each redirect request should be recorded");
    }
    server.join().expect("redirect server should finish");
}

#[test]
fn http_host_rejects_targets_until_an_explicit_policy_allows_them() {
    let mut vm = Vm::new(build_request_program("http://127.0.0.1:1/".to_string()));
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default host registry should bind HTTP");
    let error = vm
        .run()
        .expect_err("unconfigured HTTP targets must be rejected");
    assert!(
        error.to_string().contains("HTTP host is not configured")
            || error
                .to_string()
                .contains("HTTP target host is not allowed"),
        "unexpected error: {error}"
    );
}

#[test]
fn empty_registry_keeps_language_builtins_but_rejects_http_capability() {
    let mut language_vm = Vm::new(
        vm::compile_source("assert(true);")
            .expect("language builtin program should compile")
            .program,
    );
    HostFunctionRegistry::empty()
        .bind_vm_cached(&mut language_vm)
        .expect("empty registry should bind a program without host imports");
    assert_eq!(
        language_vm.run().expect("language builtin should run"),
        VmStatus::Halted
    );

    let mut http_vm = Vm::new(build_request_program("http://127.0.0.1:1/".to_string()));
    let error = HostFunctionRegistry::restricted()
        .bind_vm_cached(&mut http_vm)
        .expect_err("unapproved HTTP capability must fail during preflight");
    assert!(error.to_string().contains("http::client::request"));
}

#[test]
fn restricted_registry_requires_explicit_namespaced_builtin_capability() {
    let compiled = compile_source(
        r#"use io;
io::open("/tmp/rustscript-capability-test", "r");"#,
    )
    .expect("namespaced host builtin should compile");
    let mut vm = Vm::new(compiled.program);
    let error = HostFunctionRegistry::restricted()
        .bind_vm_cached(&mut vm)
        .expect_err("ungranted namespaced builtin must fail during preflight");
    assert!(error.to_string().contains("io_open"));
}

#[test]
fn capability_binding_plan_cannot_cross_registry_profiles() {
    let program = build_request_program("http://127.0.0.1:1/".to_string());
    let unrestricted = HostFunctionRegistry::new();
    let plan = unrestricted
        .prepare_plan(&program.imports)
        .expect("unrestricted registry should prepare HTTP plan");
    let mut vm = Vm::new(program);
    let error = HostFunctionRegistry::restricted()
        .bind_vm_with_plan(&mut vm, &plan)
        .expect_err("capability plan must not cross registry profiles");
    assert!(error.to_string().contains("different capability profile"));
}

#[test]
fn capability_binding_plan_cannot_outlive_registry_mutation() {
    let program = build_request_program("http://127.0.0.1:1/".to_string());
    let mut registry = HostFunctionRegistry::new();
    let plan = registry
        .prepare_plan(&program.imports)
        .expect("registry should prepare HTTP plan");
    registry
        .allow_builtin("http::client::request")
        .expect("HTTP capability should be a known host callable");
    let mut vm = Vm::new(program);
    let error = registry
        .bind_vm_with_plan(&mut vm, &plan)
        .expect_err("stale capability plan must not bind");
    assert!(error.to_string().contains("different capability profile"));
}

#[test]
fn capability_binding_plan_detects_divergent_registry_clone_mutations() {
    let unchanged_program = build_request_program("http://127.0.0.1:1/".to_string());
    let mut unchanged_registry = HostFunctionRegistry::restricted();
    unchanged_registry
        .allow_builtin("http::client::request")
        .expect("HTTP capability should be known");
    let unchanged_plan = unchanged_registry
        .prepare_plan(&unchanged_program.imports)
        .expect("restricted registry should prepare HTTP plan");
    let unchanged_clone = unchanged_registry.clone();
    let mut unchanged_vm = Vm::new(unchanged_program);
    unchanged_clone
        .bind_vm_with_plan(&mut unchanged_vm, &unchanged_plan)
        .expect("an unchanged registry clone should reuse the plan");

    let branch_program = build_request_program("http://127.0.0.1:1/".to_string());
    let branch_registry = HostFunctionRegistry::restricted();
    let mut first_mutation = branch_registry.clone();
    let mut second_mutation = branch_registry;
    first_mutation
        .allow_builtin("http::client::request")
        .expect("HTTP capability should be known");
    second_mutation
        .allow_builtin("io::open")
        .expect("io capability should be known");
    let plan = first_mutation
        .prepare_plan(&branch_program.imports)
        .expect("first capability branch should prepare HTTP plan");
    let mut mutated_vm = Vm::new(branch_program);
    let error = second_mutation
        .bind_vm_with_plan(&mut mutated_vm, &plan)
        .expect_err("divergent capability branches must reject each other's plan");
    assert!(error.to_string().contains("different capability profile"));
}

#[test]
fn registry_state_rejects_structural_sibling_mutations() {
    let program = build_request_program("http://127.0.0.1:1/".to_string());
    let registry = HostFunctionRegistry::new();
    let mut source = registry.clone();
    let destination = registry;
    source.register_static_args("test::structural", 0, |_args| {
        Ok(CallOutcome::Return(CallReturn::One(Value::Null)))
    });
    let plan = source
        .prepare_plan(&program.imports)
        .expect("mutated source registry should prepare HTTP plan");
    let mut vm = Vm::new(program);
    let error = destination
        .bind_vm_with_plan(&mut vm, &plan)
        .expect_err("structural sibling mutation must reject the plan");
    assert!(error.to_string().contains("different registry state"));
}

#[test]
fn cached_plan_refreshes_after_a_sibling_registry_mutation() {
    let program = build_request_program("http://127.0.0.1:1/".to_string());
    let registry = HostFunctionRegistry::new();
    let mut mutating_sibling = registry.clone();
    let destination = registry;

    let mut priming_vm = Vm::new(build_request_program("http://127.0.0.1:1/".to_string()));
    destination
        .bind_vm_cached(&mut priming_vm)
        .expect("destination should prime its plan cache");
    mutating_sibling.register_static_args("test::cache_refresh", 0, |_args| {
        Ok(CallOutcome::Return(CallReturn::One(Value::Null)))
    });

    let mut refreshed_vm = Vm::new(program);
    destination
        .bind_vm_cached(&mut refreshed_vm)
        .expect("destination should rebuild a plan after sibling mutation");
}

#[tokio::test(flavor = "current_thread")]
async fn max_stream_duration_does_not_shorten_buffered_requests() {
    let listener = bind_test_listener();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = accept_test_connection(&listener).unwrap();
        let mut request = [0; 1024];
        assert!(socket.read(&mut request).unwrap() > 0);
        thread::sleep(std::time::Duration::from_millis(30));
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .unwrap();
    });
    let mut vm = Vm::new(build_request_program(format!("http://127.0.0.1:{port}/")));
    let mut buffered_config = local_http_config(port);
    buffered_config.max_stream_duration = std::time::Duration::from_millis(1);
    buffered_config.request_timeout = std::time::Duration::from_millis(200);
    vm.configure_http(buffered_config).unwrap();
    install_host_driver(&mut vm);
    HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();
    drive_vm_to_halt(&mut vm).await.unwrap();
    assert_eq!(response_field(&vm.stack()[0], "status"), &Value::Int(200));
    server.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn explicitly_allowed_http_capability_reaches_http_policy() {
    let mut vm = Vm::new(build_request_program("http://127.0.0.1:1/".to_string()));
    vm.configure_http(HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![1],
        allow_private_ips: true,
        ..HttpConfig::default()
    })
    .expect("HTTP configuration should be valid");
    install_host_driver(&mut vm);
    let mut registry = HostFunctionRegistry::restricted();
    registry
        .allow_builtin("http::client::request")
        .expect("HTTP builtin should be explicitly allowlisted");
    registry
        .bind_vm_cached(&mut vm)
        .expect("explicit capability plan should bind");
    let error = drive_vm_to_halt(&mut vm)
        .await
        .expect_err("connection failure should reach HTTP runtime");
    assert!(!matches!(error, vm::VmError::UnboundImport(_)));
}

#[test]
fn http_in_flight_limit_rejects_before_starting_a_request() {
    let mut vm = Vm::new(build_request_program("http://127.0.0.1:1/".to_string()));
    vm.set_http_max_in_flight(0);
    vm.configure_http(HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![1],
        allow_private_ips: true,

        ..HttpConfig::default()
    })
    .expect("HTTP configuration should be valid");
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default host registry should bind HTTP");
    let error = vm
        .run()
        .expect_err("zero in-flight capacity must reject the request");
    assert!(error.to_string().contains("in-flight request limit"));
}

#[test]
fn http_config_accepts_bounded_stream_defaults_and_rejects_zero_bounds() {
    let defaults = HttpConfig::default();
    defaults
        .validate()
        .expect("default HTTP stream bounds should be valid");
    assert!(defaults.max_stream_item_bytes > 0);
    assert!(defaults.max_stream_total_bytes > 0);
    assert!(defaults.max_sse_line_bytes > 0);
    assert_eq!(
        defaults.max_stream_duration,
        std::time::Duration::from_secs(5 * 60)
    );
    assert!(!defaults.stream_idle_timeout.is_zero());

    HttpConfig {
        max_stream_duration: std::time::Duration::from_millis(1),
        ..defaults.clone()
    }
    .validate()
    .expect("an explicit positive stream duration should be valid");

    let invalid = [
        HttpConfig {
            max_stream_item_bytes: 0,
            ..defaults.clone()
        },
        HttpConfig {
            max_stream_total_bytes: 0,
            ..defaults.clone()
        },
        HttpConfig {
            max_sse_line_bytes: 0,
            ..defaults.clone()
        },
        HttpConfig {
            max_stream_duration: std::time::Duration::ZERO,
            ..defaults.clone()
        },
        HttpConfig {
            stream_idle_timeout: std::time::Duration::ZERO,
            ..defaults.clone()
        },
    ];
    for config in invalid {
        assert!(config.validate().is_err(), "zero stream bound must fail");
    }

    let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
    let error = vm
        .configure_http(HttpConfig {
            max_stream_item_bytes: 0,
            ..HttpConfig::default()
        })
        .expect_err("configuration must reject a zero stream bound");
    assert!(error.to_string().contains("max_stream_item_bytes"));
    assert!(!vm.http_is_configured());
}

#[test]
fn http_config_rejects_request_timeout_that_cannot_form_a_deadline() {
    let invalid = HttpConfig {
        request_timeout: std::time::Duration::MAX,
        ..HttpConfig::default()
    };
    let validation_error = invalid
        .validate()
        .expect_err("overflowing request timeout must be rejected");
    assert!(validation_error.to_string().contains("request_timeout"));

    let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
    let configure_error = vm
        .configure_http(invalid)
        .expect_err("configuration must reject an overflowing request timeout");
    assert!(configure_error.to_string().contains("request_timeout"));
    assert!(!vm.http_is_configured());

    let invalid = HttpConfig {
        max_stream_duration: std::time::Duration::MAX,
        ..HttpConfig::default()
    };
    let validation_error = invalid
        .validate()
        .expect_err("overflowing stream duration must be rejected");
    assert!(validation_error.to_string().contains("max_stream_duration"));

    let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
    let configure_error = vm
        .configure_http(invalid)
        .expect_err("configuration must reject an overflowing stream duration");
    assert!(configure_error.to_string().contains("max_stream_duration"));
    assert!(!vm.http_is_configured());
}

fn spawn_pending_server() -> (u16, mpsc::Receiver<()>, thread::JoinHandle<()>) {
    let listener = bind_test_listener();
    let port = listener
        .local_addr()
        .expect("pending listener should have an address")
        .port();
    let (ready_sender, ready_receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        let (mut stream, _) =
            accept_test_connection(&listener).expect("pending request should arrive");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream
                .read(&mut buffer)
                .expect("pending request should be readable");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        ready_sender
            .send(())
            .expect("pending request readiness should be observed");
        while stream.read(&mut buffer).unwrap_or(0) != 0 {}
    });
    (port, ready_receiver, handle)
}

fn spawn_pending_then_response_server() -> (u16, mpsc::Receiver<()>, thread::JoinHandle<()>) {
    let listener = bind_test_listener();
    let port = listener
        .local_addr()
        .expect("pending listener should have an address")
        .port();
    let (ready_sender, ready_receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        let (mut pending, _) =
            accept_test_connection(&listener).expect("pending request should arrive");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = pending
                .read(&mut buffer)
                .expect("pending request should be readable");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        ready_sender
            .send(())
            .expect("pending request readiness should be observed");
        while pending
            .read(&mut buffer)
            .expect("pending connection should remain readable")
            != 0
        {}

        let (mut response, _) =
            accept_test_connection(&listener).expect("replacement request should arrive");
        let mut request = Vec::new();
        loop {
            let read = response
                .read(&mut buffer)
                .expect("replacement request should be readable");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        response
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .expect("replacement response should be writable");
    });
    (port, ready_receiver, handle)
}

async fn reset_and_wait(vm: &mut Vm) -> Result<(), vm::VmError> {
    vm.reset_for_reuse()?;
    std::future::poll_fn(|cx| vm.poll_reset_for_reuse(cx)).await
}

#[tokio::test(flavor = "current_thread")]
async fn reset_retires_buffered_http_future_and_releases_its_permit() {
    let (port, ready, server) = spawn_pending_then_response_server();
    let mut vm = Vm::new(build_request_program(format!("http://127.0.0.1:{port}/")));
    vm.set_http_max_in_flight(1);
    vm.configure_http(local_http_config(port))
        .expect("HTTP configuration should be valid");
    install_host_driver(&mut vm);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default host registry should bind HTTP");
    assert!(matches!(vm.run(), Ok(VmStatus::Waiting(_))));
    ready
        .recv()
        .expect("first request should reach the transport");

    reset_and_wait(&mut vm)
        .await
        .expect("reset should retire the pending HTTP operation");
    assert!(
        vm.is_reusable(),
        "reset must wait for HTTP worker quiescence"
    );

    assert!(matches!(vm.run(), Ok(VmStatus::Waiting(_))));
    drive_vm_to_halt(&mut vm)
        .await
        .expect("replacement request should acquire the released permit");
    assert_eq!(response_field(&vm.stack()[0], "status"), &Value::Int(200));
    server.join().expect("pending server should finish");
}

#[test]
fn shutdown_and_drop_retire_buffered_http_futures() {
    for shutdown in [true, false] {
        let (port, ready, server) = spawn_pending_server();
        let mut vm = Vm::new(build_request_program(format!("http://127.0.0.1:{port}/")));
        vm.set_http_max_in_flight(1);
        vm.configure_http(local_http_config(port))
            .expect("HTTP configuration should be valid");
        install_host_driver(&mut vm);
        HostFunctionRegistry::new()
            .bind_vm_cached(&mut vm)
            .expect("default host registry should bind HTTP");
        assert!(matches!(vm.run(), Ok(VmStatus::Waiting(_))));
        ready
            .recv()
            .expect("request should reach the transport before teardown");
        if shutdown {
            vm.shutdown();
        }
        drop(vm);
        server.join().expect("teardown should close the transport");
    }
}

/// HttpConfig and the max-in-flight limit are persistent module state: both
/// survive `reset_for_reuse`, and `clear_http_configuration` removes only the
/// configuration while the max-in-flight policy (and any live scope
/// admission) remains in force.
#[test]
fn http_config_and_max_policy_are_persistent_while_clear_removes_only_config() {
    let mut vm = Vm::new(build_request_program("http://127.0.0.1:1/".to_string()));
    vm.set_http_max_in_flight(2);
    vm.configure_http(local_http_config(1))
        .expect("HTTP config should be valid");
    assert_eq!(vm.http_max_in_flight(), 2);
    assert!(vm.http_is_configured());

    // Reset retires only scope runtime state; persistent policy survives.
    vm.reset_for_reuse()
        .expect("reset should complete for an idle VM");
    assert_eq!(
        vm.http_max_in_flight(),
        2,
        "max-in-flight policy must survive reset"
    );
    assert!(vm.http_is_configured(), "HTTP config must survive reset");

    // clear removes only the config; the max-in-flight policy remains.
    vm.clear_http_configuration();
    assert!(!vm.http_is_configured());
    assert_eq!(
        vm.http_max_in_flight(),
        2,
        "clear_http_configuration must not reset the max-in-flight policy"
    );
}

/// `set_http_max_in_flight` updates the persistent policy but must not eagerly
/// create scope runtime state: it only touches the live scope admission when a
/// request has already declared it. A lazily created admission must observe
/// the *current* persistent max at capture time.
#[test]
fn set_max_in_flight_updates_policy_without_eagerly_creating_runtime_state() {
    #[derive(Debug)]
    struct Probe;

    let mut vm = Vm::new(build_request_program("http://127.0.0.1:1/".to_string()));
    // No scope runtime state exists yet: set_http_max_in_flight must not
    // eagerly create any arena state or ordinary resource.
    vm.set_http_max_in_flight(5);
    assert_eq!(vm.execution_scope().resources().len(), 0);
    assert!(
        vm.host_context().scope_state::<Probe>().is_none(),
        "the scope-state arena must stay empty until a request declares state"
    );

    // A lazily created admission reads the current persistent max: with max 0
    // the first request is rejected before any connection is attempted.
    vm.set_http_max_in_flight(0);
    vm.configure_http(local_http_config(1))
        .expect("HTTP config should be valid");
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default host registry should bind HTTP");
    let error = vm
        .run()
        .expect_err("zero in-flight capacity must reject the request");
    assert!(error.to_string().contains("in-flight request limit"));
}
