#![cfg(all(feature = "http-client", not(target_family = "wasm")))]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll};
use std::thread;
use std::time::{Duration, Instant};

use vm::operation::OperationCancelReason;
use vm::{
    CallOutcome, CallReturn, HostAsyncBridge, HostFunctionRegistry, HostFuture, HostFutureOutput,
    HostOpId, HostStackFunction, HttpConfig, HttpHostExt, Value, Vm, VmError, VmMap, VmResult,
    VmStatus, compile_source,
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
        self.submitted.get_mut(&op_id).map_or_else(
            || {
                Poll::Ready(Err(VmError::HostError(format!(
                    "unknown submitted host operation {op_id}"
                ))))
            },
            |future| future.as_mut().poll(cx),
        )
    }

    fn cancel_op(&mut self, op_id: HostOpId) {
        self.submitted.remove(&op_id);
    }

    fn request_cancel_op(
        &mut self,
        op_id: HostOpId,
        _reason: OperationCancelReason,
    ) -> VmResult<()> {
        self.cancel_op(op_id);
        Ok(())
    }

    fn poll_cancel_op(&mut self, _op_id: HostOpId, _cx: &mut Context<'_>) -> Poll<VmResult<()>> {
        Poll::Ready(Ok(()))
    }
}

struct AsyncWaitOnce {
    calls: Arc<AtomicUsize>,
}

struct CountCalls {
    calls: Arc<AtomicUsize>,
}

impl HostStackFunction for CountCalls {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CallOutcome::Return(CallReturn::one(Value::Bool(true))))
    }
}

impl HostStackFunction for AsyncWaitOnce {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            vm.submit_host_future(Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                Ok(HostFutureOutput::returning(CallReturn::one(Value::Bool(
                    true,
                ))))
            }))
        } else {
            Ok(CallOutcome::Return(CallReturn::one(Value::Bool(true))))
        }
    }
}

fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    let Value::Map(map) = value else {
        panic!("expected map, got {value:?}");
    };
    map.get(&Value::string(key))
        .unwrap_or_else(|| panic!("missing field {key}"))
}

fn map(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Map(Arc::new(VmMap::from_entries(
        entries
            .into_iter()
            .map(|(key, value)| (Value::string(key), value))
            .collect(),
    )))
}

async fn drive(vm: &mut Vm) -> VmResult<()> {
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

async fn reset_and_wait(vm: &mut Vm) -> VmResult<()> {
    vm.reset_for_reuse()?;
    std::future::poll_fn(|cx| vm.poll_reset_for_reuse(cx)).await
}

async fn run_sse_source(source: &str, config: HttpConfig) -> Result<Vm, vm::VmError> {
    let compiled = compile_source(source).expect("SSE source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.configure_http(config).unwrap();
    vm.set_async_bridge(Box::<TokioHostDriver>::default())
        .expect("test async bridge should install");
    HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();
    drive(&mut vm).await.map(|()| vm)
}

struct ServerHandle {
    shutdown: Option<mpsc::Sender<()>>,
    handle: Option<thread::JoinHandle<()>>,
}

const TEST_IO_TIMEOUT: Duration = Duration::from_secs(5);

fn bind_test_listener() -> TcpListener {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}

fn configure_test_stream(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(TEST_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(TEST_IO_TIMEOUT))?;
    Ok(())
}

fn accept_test_connection(listener: &TcpListener) -> std::io::Result<(TcpStream, SocketAddr)> {
    let deadline = Instant::now() + TEST_IO_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((stream, address)) => {
                configure_test_stream(&stream)?;
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

impl ServerHandle {
    fn join(mut self) -> thread::Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.handle
            .take()
            .expect("test server thread handle")
            .join()
    }
}

fn wait_for_test_timeout(duration: std::time::Duration) {
    let (_wake, wake_rx) = mpsc::channel::<()>();
    let _ = wake_rx.recv_timeout(duration);
}

fn accept_or_shutdown(
    listener: &TcpListener,
    shutdown: &mpsc::Receiver<()>,
) -> std::io::Result<Option<(TcpStream, SocketAddr)>> {
    let deadline = Instant::now() + TEST_IO_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((stream, address)) => {
                configure_test_stream(&stream)?;
                return Ok(Some((stream, address)));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "test server accept timed out",
                    ));
                }
                match shutdown.recv_timeout(std::time::Duration::from_millis(10)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(None),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn server(response_parts: Vec<&'static [u8]>) -> (u16, ServerHandle) {
    let listener = bind_test_listener();
    let addr = listener.local_addr().unwrap();
    let (shutdown, shutdown_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let Some((mut stream, _)) = accept_or_shutdown(&listener, &shutdown_rx).unwrap() else {
            return;
        };
        stream.set_nonblocking(false).unwrap();
        let mut request = [0_u8; 4096];
        let read = stream.read(&mut request).unwrap();
        let request = String::from_utf8_lossy(&request[..read]);
        assert_eq!(request_line(&request), "GET /events HTTP/1.1");
        assert_eq!(header_value(&request, "accept"), Some("text/event-stream"));
        for part in response_parts {
            if stream.write_all(part).is_err() || stream.flush().is_err() {
                break;
            }
        }
    });
    (
        addr.port(),
        ServerHandle {
            shutdown: Some(shutdown),
            handle: Some(handle),
        },
    )
}

fn recording_server(
    responses: Vec<Vec<&'static [u8]>>,
) -> (u16, mpsc::Receiver<String>, ServerHandle) {
    let listener = bind_test_listener();
    let addr = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    let (shutdown, shutdown_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        for response_parts in responses {
            'connection: loop {
                let Some((mut stream, _)) = accept_or_shutdown(&listener, &shutdown_rx).unwrap()
                else {
                    return;
                };
                stream.set_nonblocking(false).unwrap();

                let mut request = Vec::new();
                let mut byte = [0_u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    match stream.read_exact(&mut byte) {
                        Ok(()) => request.push(byte[0]),
                        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                            continue 'connection;
                        }
                        Err(error) => panic!("failed to read test request: {error}"),
                    }
                }
                let head = String::from_utf8(request).unwrap();

                let content_length = head
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                    })
                    .unwrap_or(0);
                let mut body = vec![0; content_length];
                match stream.read_exact(&mut body) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                        continue 'connection;
                    }
                    Err(error) => panic!("failed to read test request body: {error}"),
                }
                sender
                    .send(format!("{head}{}", String::from_utf8_lossy(&body)))
                    .unwrap();
                for part in response_parts {
                    if stream.write_all(part).is_err() || stream.flush().is_err() {
                        break;
                    }
                }
                break 'connection;
            }
        }
    });
    (
        addr.port(),
        receiver,
        ServerHandle {
            shutdown: Some(shutdown),
            handle: Some(handle),
        },
    )
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

fn request_line(request: &str) -> &str {
    request.split_once("\r\n").map_or(request, |(line, _)| line)
}

fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn has_header(request: &str, name: &str) -> bool {
    request
        .split("\r\n")
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .any(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
}

fn config(port: u16) -> HttpConfig {
    HttpConfig {
        allowed_schemes: vec!["http".into()],
        allowed_hosts: vec!["127.0.0.1".into()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        ..HttpConfig::default()
    }
}

fn assert_no_connection(listener: TcpListener, context: &'static str) -> ServerHandle {
    listener.set_nonblocking(true).unwrap();
    let (shutdown, shutdown_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        if accept_or_shutdown(&listener, &shutdown_rx)
            .unwrap()
            .is_some()
        {
            panic!("{context} must be rejected before a second connection");
        }
    });
    ServerHandle {
        shutdown: Some(shutdown),
        handle: Some(handle),
    }
}

fn rejecting_redirect_server(
    location: impl FnOnce(u16) -> String + Send + 'static,
) -> (u16, mpsc::Receiver<String>, ServerHandle) {
    let listener = bind_test_listener();
    let addr = listener.local_addr().unwrap();
    let location = location(addr.port());
    let (sender, receiver) = mpsc::channel();
    let (shutdown, shutdown_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let Some((mut stream, _)) = accept_or_shutdown(&listener, &shutdown_rx).unwrap() else {
            return;
        };
        stream.set_nonblocking(false).unwrap();
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        sender.send(String::from_utf8(request).unwrap()).unwrap();
        write!(
            stream,
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n"
        )
        .unwrap();
        drop(stream);
        if accept_or_shutdown(&listener, &shutdown_rx)
            .unwrap()
            .is_some()
        {
            panic!("invalid redirect must be rejected before a second connection");
        }
    });
    (
        addr.port(),
        receiver,
        ServerHandle {
            shutdown: Some(shutdown),
            handle: Some(handle),
        },
    )
}

#[tokio::test(flavor = "current_thread")]
async fn sse_delivers_open_events_end_and_terminal_summary() {
    let (port, server) = server(vec![
        b"HTTP/1.1 200 OK\r\nContent-Type: Text/Event-Stream; charset=utf-8\r\nTransfer-Encoding: chunked\r\n\r\n",
        b"b\r\ndata: one\n\n\r\n",
        b"18\r\nevent: named\ndata: two\n\n\r\n",
        b"0\r\n\r\n",
    ]);
    let source = format!(
        r#"
        use http;
        fn record(item: map) -> map {{
            if item["kind"] == "open" && item["status"] != 200 {{ let _ = 1 / 0; }}
            if item["kind"] == "event" && item["data"] == "one" && item["event"] != null {{ let _ = 1 / 0; }}
            if item["kind"] == "event" && item["data"] == "two" && item["event"] != "named" {{ let _ = 1 / 0; }}
            if item["kind"] == "end" && item != {{kind: "end"}} {{ let _ = 1 / 0; }}
            {{action: "continue"}}
        }}
        let result = http::client::sse(
            {{"method": "GET", "url": "http://127.0.0.1:{port}/events"}},
            record
        );
        result;
        "#
    );
    let compiled = compile_source(&source).expect("SSE source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.configure_http(config(port)).unwrap();
    vm.set_async_bridge(Box::<TokioHostDriver>::default())
        .expect("test async bridge should install");
    HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();

    drive(&mut vm).await.unwrap();
    server.join().unwrap();

    let result = &vm.stack()[0];
    assert_eq!(field(result, "outcome"), &Value::string("eof"));
    assert_eq!(field(result, "status"), &Value::Int(200));
    assert_eq!(field(result, "items"), &Value::Int(4));
    assert_eq!(field(result, "bytes_sent"), &Value::Int(0));
}

#[test]
fn sse_rejects_wrong_callback_schema_and_invalid_timeout_before_permit_admission() {
    assert!(compile_source(
        r#"use http; http::client::sse({"method":"GET","url":"http://127.0.0.1:1/"}, |item| 1);"#
    )
    .is_err());

    for (timeout, expected) in [
        ("0", "positive"),
        ("-1", "positive"),
        ("\"1\"", "type mismatch"),
    ] {
        let source = format!(
            r#"
            use http;
            fn callback(item: map) -> map {{ {{action: "continue"}} }}
            http::client::sse(
                {{method: "GET", url: "http://127.0.0.1:1/events", timeout_ms: {timeout}}},
                callback
            );
            "#
        );
        let compiled = compile_source(&source).unwrap();
        let mut vm = Vm::new(compiled.program);
        vm.set_http_max_in_flight(0);
        vm.configure_http(config(1)).unwrap();
        HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();
        let error = vm.run().unwrap_err();
        assert!(error.to_string().contains(expected), "{timeout}: {error}");
        assert!(
            !error.to_string().contains("in-flight request limit"),
            "timeout validation must precede permit admission: {error}"
        );
    }

    let source = r#"
        use http;
        fn callback(item: map) -> map { {action: "continue"} }
        http::client::sse(
            {method: "GET", url: "http://127.0.0.1:1/events", timeout_ms: 1},
            callback
        );
    "#;
    let compiled = compile_source(source).unwrap();
    let mut vm = Vm::new(compiled.program);
    vm.set_http_max_in_flight(0);
    vm.configure_http(config(1)).unwrap();
    HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();
    let error = vm.run().unwrap_err();
    assert!(
        error.to_string().contains("in-flight request limit"),
        "a positive timeout should pass timeout admission: {error}"
    );

    let source = r#"
        use http;
        fn callback(item: map) -> map { {action: "continue"} }
        http::client::sse(
            {method: "PUT", url: "http://127.0.0.1:1/events"},
            callback
        );
    "#;
    let compiled = compile_source(source).unwrap();
    let mut vm = Vm::new(compiled.program);
    vm.configure_http(config(1)).unwrap();
    HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();
    let error = vm.run().unwrap_err();
    assert!(error.to_string().contains("GET or POST"), "{error}");
}

#[test]
fn sse_admission_does_not_require_a_tokio_reactor() {
    let source = r#"
        use http;
        fn callback(item: map) -> map { {action: "continue"} }
        http::client::sse(
            {method: "GET", url: "http://127.0.0.1:1/events"},
            callback
        );
    "#;
    let compiled = compile_source(source).unwrap();
    let mut vm = Vm::new(compiled.program);
    vm.configure_http(config(1)).unwrap();
    vm.set_async_bridge(Box::<TokioHostDriver>::default())
        .expect("test async bridge should install");
    HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();
    assert!(matches!(vm.run().unwrap(), VmStatus::Waiting(_)));
    vm.reset_for_reuse().expect("SSE reset should complete");
}

#[tokio::test(flavor = "current_thread")]
async fn sse_accepts_post_with_body() {
    let (port, requests, server) = recording_server(vec![vec![
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\n\r\n",
    ]]);
    let source = format!(
        r#"
        use http;
        fn callback(item: map) -> map {{ {{action: "continue"}} }}
        http::client::sse(
            {{method: "POST", url: "http://127.0.0.1:{port}/events", body: "payload"}},
            callback
        );
        "#
    );
    let mut vm = run_sse_source(&source, config(port)).await.unwrap();
    assert_eq!(vm.host_context().resource_count(), 0);
    assert_eq!(vm.host_context().operation_count(), 0);
    assert_eq!(field(&vm.stack()[0], "outcome"), &Value::string("eof"));
    let request = requests.recv().unwrap();
    assert_eq!(request_line(&request), "POST /events HTTP/1.1");
    assert!(request.ends_with("payload"));
    server.join().unwrap();
}

fn redirect_server(status: u16) -> (u16, mpsc::Receiver<String>, ServerHandle) {
    let listener = bind_test_listener();
    let addr = listener.local_addr().unwrap();
    let port = addr.port();
    listener.set_nonblocking(true).unwrap();
    let (sender, receiver) = mpsc::channel();
    let (shutdown, shutdown_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        for index in 0..2 {
            let Some((mut stream, _)) = accept_or_shutdown(&listener, &shutdown_rx).unwrap() else {
                return;
            };
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let head = String::from_utf8(request).unwrap();
            let length = head
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                })
                .unwrap_or(0);
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            sender
                .send(format!("{head}{}", String::from_utf8_lossy(&body)))
                .unwrap();
            if index == 0 {
                write!(
                    stream,
                    "HTTP/1.1 {status} Redirect\r\nLocation: http://127.0.0.1:{port}/final\r\nContent-Length: 0\r\n\r\n"
                )
                .unwrap();
            } else {
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\n\r\n")
                    .unwrap();
            }
        }
    });
    (
        addr.port(),
        receiver,
        ServerHandle {
            shutdown: Some(shutdown),
            handle: Some(handle),
        },
    )
}

#[tokio::test(flavor = "current_thread")]
async fn sse_post_redirect_method_and_body_follow_http_rules() {
    for (status, preserves_post) in [
        (301, false),
        (302, false),
        (303, false),
        (307, true),
        (308, true),
    ] {
        let (port, requests, server) = redirect_server(status);
        let source = format!(
            r#"use http;
            fn callback(item: map) -> map {{ {{action: "continue"}} }}
            http::client::sse({{method:"POST", url:"http://127.0.0.1:{port}/start", body:"payload"}}, callback);"#
        );
        run_sse_source(&source, config(port)).await.unwrap();
        let first = requests.recv().unwrap();
        let second = requests.recv().unwrap();
        assert_eq!(request_line(&first), "POST /start HTTP/1.1");
        if preserves_post {
            assert!(
                request_line(&second) == "POST /final HTTP/1.1",
                "status {status}: <redacted>"
            );
            assert!(second.ends_with("payload"), "status {status}: <redacted>");
        } else {
            assert!(
                request_line(&second) == "GET /final HTTP/1.1",
                "status {status}: <redacted>"
            );
            assert!(!second.ends_with("payload"), "status {status}: <redacted>");
        }
        server.join().unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn sse_get_redirect_preserves_get_for_301_and_302() {
    for status in [301, 302] {
        let (port, requests, server) = redirect_server(status);
        let source = format!(
            r#"use http;
            fn callback(item: map) -> map {{ {{action: "continue"}} }}
            http::client::sse({{method:"GET", url:"http://127.0.0.1:{port}/start"}}, callback);"#
        );
        run_sse_source(&source, config(port)).await.unwrap();
        let first = requests.recv().unwrap();
        let second = requests.recv().unwrap();
        assert_eq!(request_line(&first), "GET /start HTTP/1.1");
        assert!(
            request_line(&second) == "GET /final HTTP/1.1",
            "status {status}: <redacted>"
        );
        assert!(!second.ends_with("payload"), "status {status}: <redacted>");
        server.join().unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn sse_rejects_redirect_userinfo_before_reconnecting() {
    let (port, requests, server) = rejecting_redirect_server(|port| {
        format!("http://redirect-user:redirect-password@127.0.0.1:{port}/final")
    });
    let source = format!(
        r#"use http;
        fn callback(item: map) -> map {{ {{action: "continue"}} }}
        http::client::sse(
            {{method:"GET", url:"http://127.0.0.1:{port}/start", headers:{{Authorization:"Bearer secret", Cookie:"a=b"}}}},
            callback
        );"#
    );
    let error = match run_sse_source(&source, config(port)).await {
        Ok(_) => panic!("redirect userinfo must be rejected"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("URL userinfo is not allowed"),
        "{error}"
    );
    let request = requests.recv().unwrap();
    assert_eq!(
        header_value(&request, "authorization"),
        Some("Bearer secret")
    );
    assert_eq!(header_value(&request, "cookie"), Some("a=b"));
    assert!(!request.contains("redirect-user"));
    assert!(!request.contains("redirect-password"));
    server.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn sse_rejects_disallowed_redirect_targets_before_connecting() {
    for (host, allow_target_port, expected) in [
        ("127.0.0.1", false, "target port"),
        ("localhost", true, "target host"),
    ] {
        let target_listener = bind_test_listener();
        let target_port = target_listener.local_addr().unwrap().port();
        let no_target_connection = assert_no_connection(target_listener, expected);
        let location = format!("http://{host}:{target_port}/final");
        let redirect = format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n"
        );
        let redirect = Box::leak(redirect.into_bytes().into_boxed_slice());
        let (source_port, requests, source_server) = recording_server(vec![vec![redirect]]);
        let source = format!(
            r#"use http;
            fn callback(item: map) -> map {{ {{action: "continue"}} }}
            http::client::sse(
                {{method:"GET", url:"http://127.0.0.1:{source_port}/start", headers:{{Authorization:"Bearer secret", Cookie:"a=b"}}}},
                callback
            );"#
        );
        let mut allowed = config(source_port);
        if allow_target_port {
            allowed.allowed_ports.push(target_port);
        }
        let error = match run_sse_source(&source, allowed).await {
            Ok(_) => panic!("disallowed redirect target must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains(expected), "{error}");
        let request = requests.recv().unwrap();
        assert_eq!(
            header_value(&request, "authorization"),
            Some("Bearer secret")
        );
        assert_eq!(header_value(&request, "cookie"), Some("a=b"));
        source_server.join().unwrap();
        no_target_connection.join().unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn sse_stop_retires_without_end_and_returns_stopped_summary() {
    let (port, server) = server(vec![
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
        b"9\r\ndata: x\n\n\r\n",
        b"0\r\n\r\n",
    ]);
    let source = format!(
        r#"use http;
        fn stop(item: map) -> map {{ {{action: "stop"}} }}
        http::client::sse({{"method":"GET","url":"http://127.0.0.1:{port}/events"}}, stop);"#
    );
    let vm = run_sse_source(&source, config(port)).await.unwrap();
    server.join().unwrap();
    assert_eq!(field(&vm.stack()[0], "outcome"), &Value::string("stopped"));
    assert_eq!(field(&vm.stack()[0], "items"), &Value::Int(1));
}

#[tokio::test(flavor = "current_thread")]
async fn sse_rejected_nested_admission_rolls_back_before_reset_reuse() {
    let (port, _requests, server) = recording_server(vec![
        vec![
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"9\r\ndata: x\n\n\r\n",
            b"0\r\n\r\n",
        ],
        vec![b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\n\r\n"],
        vec![
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"9\r\ndata: x\n\n\r\n",
            b"0\r\n\r\n",
        ],
        vec![b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\n\r\n"],
        vec![
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"9\r\ndata: x\n\n\r\n",
            b"0\r\n\r\n",
        ],
        vec![b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\n\r\n"],
    ]);
    let source = format!(
        r#"
        use http;
        fn inner(item: map) -> map {{ {{action: "continue"}} }}
        fn outer(item: map) -> map {{
            http::client::sse(
                {{method: "GET", url: "http://127.0.0.1:{port}/inner"}},
                inner
            );
            {{action: "continue"}}
        }}
        http::client::sse(
            {{method: "GET", url: "http://127.0.0.1:{port}/outer"}},
            outer
        );
        "#
    );
    let compiled = compile_source(&source).unwrap();
    let mut vm = Vm::new(compiled.program);
    vm.set_http_max_in_flight(2);
    vm.configure_http(config(port)).unwrap();
    vm.set_async_bridge(Box::<TokioHostDriver>::default())
        .expect("test async bridge should install");
    HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();

    for _ in 0..3 {
        let error = drive(&mut vm)
            .await
            .expect_err("nested SSE must be rejected");
        assert!(
            error
                .to_string()
                .contains("vm already owns an active callable stream"),
            "{error}"
        );
        let cleanup_result = std::future::poll_fn(|cx| vm.poll_waiting_host_op(cx)).await;
        if let Err(cleanup_error) = cleanup_result {
            assert!(
                cleanup_error
                    .to_string()
                    .contains("vm already owns an active callable stream"),
                "{cleanup_error}"
            );
        }
        assert_eq!(vm.host_context().resource_count(), 0);
        assert_eq!(vm.host_context().operation_count(), 0);
        reset_and_wait(&mut vm)
            .await
            .expect("rejected nested SSE must drain before reset reuse");
        assert_eq!(vm.host_context().resource_count(), 0);
        assert_eq!(vm.host_context().operation_count(), 0);
        assert!(vm.is_reusable());
    }
    server.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn sse_reset_releases_the_connection_permit_before_reuse() {
    let (port, requests, server) = recording_server(vec![
        vec![b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\n\r\n"],
        vec![b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\n\r\n"],
    ]);
    let source = format!(
        r#"use http;
        http::client::sse(
            {{"method":"GET","url":"http://127.0.0.1:{port}/events"}},
            |item| {{action: "continue"}}
        );"#
    );
    let compiled = compile_source(&source).unwrap();
    let mut vm = Vm::new(compiled.program);
    vm.set_http_max_in_flight(1);
    vm.configure_http(config(port)).unwrap();
    vm.set_async_bridge(Box::<TokioHostDriver>::default())
        .expect("test async bridge should install");
    HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();

    assert!(matches!(vm.run().unwrap(), VmStatus::Waiting(_)));
    assert!(
        requests
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("initial stream request should be recorded")
            .starts_with("GET /events HTTP/1.1")
    );
    reset_and_wait(&mut vm)
        .await
        .expect("SSE reset should complete");
    drive(&mut vm).await.unwrap();
    assert_eq!(field(&vm.stack()[0], "outcome"), &Value::string("eof"));
    assert!(
        requests
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("reused stream request should be recorded")
            .starts_with("GET /events HTTP/1.1")
    );
    server.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn sse_reset_while_callback_waits_retires_stream_to_quiescence() {
    let (port, _requests, server) = recording_server(vec![
        vec![
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"5\r\ndata: x\n\n\r\n",
            b"0\r\n\r\n",
        ],
        vec![b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\n\r\n"],
    ]);
    let source = format!(
        r#"use http;
        fn async_wait() -> bool;
        http::client::sse(
            {{"method":"GET","url":"http://127.0.0.1:{port}/events"}},
            |item| {{
                action: if async_wait() => {{ "continue" }} else => {{ "continue" }}
            }}
        );"#
    );
    let compiled = compile_source(&source).unwrap();
    let mut vm = Vm::new(compiled.program);
    vm.set_http_max_in_flight(1);
    vm.configure_http(config(port)).unwrap();
    vm.set_async_bridge(Box::<TokioHostDriver>::default())
        .expect("test async bridge should install");
    let wait_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = HostFunctionRegistry::new();
    registry.register_stack("async_wait", 0, {
        let wait_calls = Arc::clone(&wait_calls);
        move || {
            Box::new(AsyncWaitOnce {
                calls: Arc::clone(&wait_calls),
            })
        }
    });
    registry.bind_vm_cached(&mut vm).unwrap();

    assert!(matches!(vm.run().unwrap(), VmStatus::Waiting(_)));
    vm.await_waiting_host_op().await.unwrap();
    assert!(matches!(vm.resume().unwrap(), VmStatus::Waiting(_)));
    assert_eq!(wait_calls.load(Ordering::SeqCst), 1);

    reset_and_wait(&mut vm)
        .await
        .expect("reset must cancel callback and retire the stream");
    drive(&mut vm)
        .await
        .expect("the reused VM must reacquire the permit");
    assert_eq!(wait_calls.load(Ordering::SeqCst), 3);
    assert_eq!(field(&vm.stack()[0], "outcome"), &Value::string("eof"));
    server.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn sse_rejects_status_content_type_and_idle_peer() {
    for (head, expected) in [
        (b"HTTP/1.1 404 Not Found\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\n\r\n".as_slice(), "status 404"),
        (b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n".as_slice(), "Content-Type"),
    ] {
        let (port, server) = server(vec![head]);
        let source = format!(
            r#"use http; fn go(item: map) -> map {{ {{action:"continue"}} }} http::client::sse({{"method":"GET","url":"http://127.0.0.1:{port}/events"}}, go);"#
        );
        let error = match run_sse_source(&source, config(port)).await {
            Ok(_) => panic!("invalid SSE response must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains(expected), "{error}");
        server.join().unwrap();
    }

    let listener = bind_test_listener();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = accept_test_connection(&listener).unwrap();
        let mut request = [0; 1024];
        let read = socket.read(&mut request).unwrap();
        assert!(read > 0, "SSE request should be received");
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
        socket.flush().unwrap();
        wait_for_test_timeout(std::time::Duration::from_millis(80));
    });
    let mut idle_config = config(port);
    idle_config.stream_idle_timeout = std::time::Duration::from_millis(20);
    let source = format!(
        r#"use http; fn go(item: map) -> map {{ {{action:"continue"}} }} http::client::sse({{"method":"GET","url":"http://127.0.0.1:{port}/events"}}, go);"#
    );
    let error = match run_sse_source(&source, idle_config).await {
        Ok(_) => panic!("idle SSE peer must time out"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("idle timeout"), "{error}");
    server.join().unwrap();

    let listener = bind_test_listener();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = accept_test_connection(&listener).unwrap();
        let mut request = [0; 1024];
        let read = socket.read(&mut request).unwrap();
        assert!(read > 0, "SSE request should be received");
        wait_for_test_timeout(std::time::Duration::from_millis(80));
    });
    let mut opening_config = config(port);
    opening_config.stream_idle_timeout = std::time::Duration::from_millis(20);
    let source = format!(
        r#"use http; fn go(item: map) -> map {{ {{action:"continue"}} }} http::client::sse({{"method":"GET","url":"http://127.0.0.1:{port}/events"}}, go);"#
    );
    let error = match run_sse_source(&source, opening_config).await {
        Ok(_) => panic!("SSE response opening must obey idle timeout"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("idle timeout while opening"),
        "{error}"
    );
    server.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn sse_script_timeout_shortens_the_host_stream_duration() {
    let listener = bind_test_listener();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = accept_test_connection(&listener).unwrap();
        let mut request = [0; 1024];
        assert!(socket.read(&mut request).unwrap() > 0);
        wait_for_test_timeout(std::time::Duration::from_millis(80));
    });
    let mut deadline_config = config(port);
    deadline_config.max_stream_duration = std::time::Duration::from_millis(200);
    deadline_config.stream_idle_timeout = std::time::Duration::from_millis(200);
    let source = format!(
        r#"use http; fn go(item: map) -> map {{ {{action:"continue"}} }} http::client::sse({{"method":"GET","url":"http://127.0.0.1:{port}/events","timeout_ms":20}}, go);"#
    );
    let error = match run_sse_source(&source, deadline_config).await {
        Ok(_) => panic!("script deadline should shorten the host maximum"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("total deadline"), "{error}");
    server.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn sse_host_stream_duration_caps_script_timeout_while_opening() {
    let listener = bind_test_listener();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = accept_test_connection(&listener).unwrap();
        let mut request = [0; 1024];
        let _ = socket.read(&mut request);
        wait_for_test_timeout(std::time::Duration::from_millis(600));
    });
    let mut deadline_config = config(port);
    deadline_config.max_stream_duration = std::time::Duration::from_millis(250);
    deadline_config.stream_idle_timeout = std::time::Duration::from_millis(800);
    let source = format!(
        r#"use http; fn go(item: map) -> map {{ {{action:"continue"}} }} http::client::sse({{"method":"GET","url":"http://127.0.0.1:{port}/events","timeout_ms":1000}}, go);"#
    );
    let error = match run_sse_source(&source, deadline_config).await {
        Ok(_) => panic!("host duration should cap the script timeout during opening"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("total deadline"), "{error}");
    server.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn sse_total_deadline_expires_despite_periodic_progress_below_idle_timeout() {
    let listener = bind_test_listener();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = accept_test_connection(&listener).unwrap();
        let mut request = [0; 1024];
        assert!(socket.read(&mut request).unwrap() > 0);
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
        socket.flush().unwrap();
        for _ in 0..40 {
            wait_for_test_timeout(std::time::Duration::from_millis(25));
            if socket.write_all(b"c\r\ndata: tick\n\n\r\n").is_err() {
                break;
            }
            if socket.flush().is_err() {
                break;
            }
        }
    });
    let mut deadline_config = config(port);
    deadline_config.max_stream_duration = std::time::Duration::from_millis(600);
    deadline_config.stream_idle_timeout = std::time::Duration::from_millis(250);
    let callbacks = Arc::new(AtomicUsize::new(0));
    let source = format!(
        r#"use http;
        fn count_call() -> bool;
        fn go(item: map) -> map {{
            {{action: if count_call() => {{"continue"}} else => {{"continue"}}}}
        }}
        http::client::sse({{"method":"GET","url":"http://127.0.0.1:{port}/events"}}, go);"#
    );
    let compiled = compile_source(&source).unwrap();
    let mut vm = Vm::new(compiled.program);
    vm.configure_http(deadline_config).unwrap();
    vm.set_async_bridge(Box::<TokioHostDriver>::default())
        .expect("test async bridge should install");
    let mut registry = HostFunctionRegistry::new();
    registry.register_stack("count_call", 0, {
        let callbacks = Arc::clone(&callbacks);
        move || {
            Box::new(CountCalls {
                calls: Arc::clone(&callbacks),
            })
        }
    });
    registry.bind_vm_cached(&mut vm).unwrap();
    let error = drive(&mut vm)
        .await
        .expect_err("periodic progress must not extend the total deadline");
    assert!(error.to_string().contains("total deadline"), "{error}");
    server.join().unwrap();
    assert!(
        callbacks.load(Ordering::SeqCst) >= 4,
        "multiple progress events must reach callbacks inside the idle bound"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn sse_total_deadline_releases_the_connection_permit_for_reuse() {
    let listener = bind_test_listener();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut first, _) = accept_test_connection(&listener).unwrap();
        let mut request = [0; 1024];
        assert!(first.read(&mut request).unwrap() > 0);
        let first = thread::spawn(move || {
            wait_for_test_timeout(std::time::Duration::from_millis(80));
            drop(first);
        });

        let (mut second, _) = accept_test_connection(&listener).unwrap();
        let mut request = [0; 1024];
        assert!(second.read(&mut request).unwrap() > 0);
        second
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\n\r\n",
            )
            .unwrap();
        first.join().unwrap();
    });
    let source = format!(
        r#"use http; http::client::sse({{"method":"GET","url":"http://127.0.0.1:{port}/events"}}, |item| {{action:"continue"}});"#
    );
    let compiled = compile_source(&source).unwrap();
    let mut vm = Vm::new(compiled.program);
    vm.set_http_max_in_flight(1);
    let mut deadline_config = config(port);
    deadline_config.max_stream_duration = std::time::Duration::from_millis(20);
    deadline_config.stream_idle_timeout = std::time::Duration::from_millis(200);
    vm.configure_http(deadline_config).unwrap();
    vm.set_async_bridge(Box::<TokioHostDriver>::default())
        .expect("test async bridge should install");
    HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();

    let error = drive(&mut vm)
        .await
        .expect_err("the first stream should reach its total deadline");
    assert!(error.to_string().contains("total deadline"), "{error}");
    vm.reset_for_reuse().expect("SSE reset should complete");
    drive(&mut vm)
        .await
        .expect("the second stream should acquire the released permit");
    assert_eq!(field(&vm.stack()[0], "outcome"), &Value::string("eof"));
    server.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn sse_callback_stop_after_deadline_fails_and_releases_permit_without_another_poll() {
    let listener = bind_test_listener();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut first, _) = accept_test_connection(&listener).unwrap();
        let mut request = [0; 1024];
        assert!(first.read(&mut request).unwrap() > 0);
        first
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .unwrap();
        first.flush().unwrap();
        let first = thread::spawn(move || {
            wait_for_test_timeout(std::time::Duration::from_millis(500));
            drop(first);
        });

        let (mut second, _) = accept_test_connection(&listener).unwrap();
        let mut request = [0; 1024];
        assert!(second.read(&mut request).unwrap() > 0);
        second
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\n\r\n",
            )
            .unwrap();
        first.join().unwrap();
    });
    let source = format!(
        r#"
        use http;
        fn async_wait() -> bool;
        http::client::sse(
            {{"method":"GET","url":"http://127.0.0.1:{port}/events"}},
            |item| {{
                action: if async_wait() => {{ "stop" }} else => {{ "stop" }}
            }}
        );
        "#
    );
    let compiled = compile_source(&source).unwrap();
    let mut vm = Vm::new(compiled.program);
    vm.set_http_max_in_flight(1);
    let mut deadline_config = config(port);
    deadline_config.max_stream_duration = std::time::Duration::from_millis(100);
    deadline_config.stream_idle_timeout = std::time::Duration::from_secs(1);
    vm.configure_http(deadline_config).unwrap();
    vm.set_async_bridge(Box::<TokioHostDriver>::default())
        .expect("test async bridge should install");
    let wait_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = HostFunctionRegistry::new();
    registry.register_stack("async_wait", 0, {
        let wait_calls = Arc::clone(&wait_calls);
        move || {
            Box::new(AsyncWaitOnce {
                calls: Arc::clone(&wait_calls),
            })
        }
    });
    registry.bind_vm_cached(&mut vm).unwrap();

    let error = drive(&mut vm)
        .await
        .expect_err("a callback action after the total deadline must fail");
    assert!(
        matches!(error, VmError::HostError(ref message) if message == "SSE total deadline exceeded"),
        "{error}"
    );
    assert_eq!(wait_calls.load(Ordering::SeqCst), 1);
    assert!(vm.stack().iter().all(|value| {
        let Value::Map(map) = value else {
            return true;
        };
        map.get(&Value::string("outcome")) != Some(&Value::string("stopped"))
    }));

    vm.reset_for_reuse().expect("SSE reset should complete");
    drive(&mut vm)
        .await
        .expect("the next stream should acquire the released permit");
    assert_eq!(wait_calls.load(Ordering::SeqCst), 2);
    assert_eq!(field(&vm.stack()[0], "outcome"), &Value::string("stopped"));
    server.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn sse_callback_continue_after_deadline_fails_before_another_network_poll() {
    let listener = bind_test_listener();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = accept_test_connection(&listener).unwrap();
        let mut request = [0; 1024];
        assert!(socket.read(&mut request).unwrap() > 0);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .unwrap();
        socket.flush().unwrap();
        wait_for_test_timeout(std::time::Duration::from_millis(500));
    });
    let source = format!(
        r#"
        use http;
        fn async_wait() -> bool;
        http::client::sse(
            {{"method":"GET","url":"http://127.0.0.1:{port}/events"}},
            |item| {{
                action: if async_wait() => {{ "continue" }} else => {{ "continue" }}
            }}
        );
        "#
    );
    let compiled = compile_source(&source).unwrap();
    let mut vm = Vm::new(compiled.program);
    let mut deadline_config = config(port);
    deadline_config.max_stream_duration = std::time::Duration::from_millis(100);
    deadline_config.stream_idle_timeout = std::time::Duration::from_secs(1);
    vm.configure_http(deadline_config).unwrap();
    vm.set_async_bridge(Box::<TokioHostDriver>::default())
        .expect("test async bridge should install");
    let wait_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = HostFunctionRegistry::new();
    registry.register_stack("async_wait", 0, {
        let wait_calls = Arc::clone(&wait_calls);
        move || {
            Box::new(AsyncWaitOnce {
                calls: Arc::clone(&wait_calls),
            })
        }
    });
    registry.bind_vm_cached(&mut vm).unwrap();

    let error = drive(&mut vm)
        .await
        .expect_err("a continue action after the total deadline must fail");
    assert!(
        matches!(error, VmError::HostError(ref message) if message == "SSE total deadline exceeded"),
        "{error}"
    );
    assert_eq!(wait_calls.load(Ordering::SeqCst), 1);
    server.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn sse_chunked_trailers_cannot_bypass_total_body_limits() {
    let trailer = format!("0\r\nX-Trailer: {}\r\n\r\n", "a".repeat(64 * 1024));
    let trailer = Box::leak(trailer.into_bytes().into_boxed_slice());
    let (port, server) = server(vec![
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
        b"9\r\nda",
        b"ta: x\n\n\r\n",
        trailer,
    ]);
    let mut stream_config = config(port);
    stream_config.max_stream_total_bytes = 9;
    let source = format!(
        r#"use http; fn on_event(item: map) -> map {{ {{action: "continue"}} }} http::client::sse({{"method":"GET","url":"http://127.0.0.1:{port}/events"}}, on_event);"#
    );
    let error = match run_sse_source(&source, stream_config).await {
        Ok(_) => panic!("oversized SSE trailers must be rejected"),
        Err(error) => error,
    };
    assert!(
        contains_ascii_case_insensitive(&error.to_string(), "response")
            || contains_ascii_case_insensitive(&error.to_string(), "connection"),
        "unexpected trailer-limit error: {error}"
    );
    server.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn sse_revalidates_redirects_and_strips_cross_origin_credentials() {
    for status in [301, 302, 303, 307, 308] {
        let (target_port, target_requests, target) = recording_server(vec![vec![
        b"HTTP/1.1 200 OK\r\nContent-Type: Text/Event-Stream; Charset=UTF-8\r\nX-Obs: \x80\r\nContent-Length: 0\r\n\r\n",
    ]]);
        let redirect = format!(
            "HTTP/1.1 {status} Redirect\r\nLocation: http://127.0.0.1:{target_port}/final\r\nContent-Length: 0\r\n\r\n"
        );
        let redirect = Box::leak(redirect.into_bytes().into_boxed_slice());
        let (source_port, source_requests, source_server) = recording_server(vec![vec![redirect]]);
        let source_code = format!(
            r#"
        use http;
        fn record(item: map) -> map {{
            if item["kind"] == "open" && item != {{
                kind: "open",
                status: 200,
                headers: {{"content-type": "Text/Event-Stream; Charset=UTF-8", "x-obs": b"\x80", "content-length": "0"}},
                url: "http://127.0.0.1:{target_port}/final"
            }} {{ let _ = 1 / 0; }}
            if item["kind"] == "end" && item != {{kind: "end"}} {{ let _ = 1 / 0; }}
            {{action: "continue"}}
        }}
        http::client::sse(
            {{method: "POST", url: "http://127.0.0.1:{source_port}/start", body: "payload", headers: {{
                Authorization: "Bearer secret",
                "Proxy-Authorization": "Basic proxy-secret",
                Cookie: "a=b",
                "X-Api-Key": "api-secret",
                "X-Arbitrary": "custom-secret",
                "Content-Type": "application/body",
                Accept: "text/event-stream",
                "Accept-Language": "en-US",
                "Accept-Encoding": "identity"
            }}}},
            record
        );
        "#
        );
        let mut allowed = config(source_port);
        allowed.allowed_ports.push(target_port);
        let vm = run_sse_source(&source_code, allowed).await.unwrap();
        let final_url = format!("http://127.0.0.1:{target_port}/final");
        assert_eq!(
            &vm.stack()[0],
            &map([
                ("outcome", Value::string("eof")),
                ("status", Value::Int(200)),
                (
                    "headers",
                    map([
                        (
                            "content-type",
                            Value::string("Text/Event-Stream; Charset=UTF-8"),
                        ),
                        ("x-obs", Value::bytes(vec![0x80])),
                        ("content-length", Value::string("0")),
                    ]),
                ),
                ("url", Value::string(final_url)),
                ("items", Value::Int(2)),
                ("bytes_received", Value::Int(0)),
                ("bytes_sent", Value::Int(0)),
            ])
        );
        let first = source_requests.recv().unwrap();
        assert_eq!(request_line(&first), "POST /start HTTP/1.1");
        assert!(first.ends_with("payload"));
        assert_eq!(header_value(&first, "authorization"), Some("Bearer secret"));
        assert_eq!(
            header_value(&first, "proxy-authorization"),
            Some("Basic proxy-secret")
        );
        assert_eq!(header_value(&first, "cookie"), Some("a=b"));
        assert_eq!(header_value(&first, "x-api-key"), Some("api-secret"));
        assert_eq!(header_value(&first, "x-arbitrary"), Some("custom-secret"));
        let second = target_requests.recv().unwrap();
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
            assert!(
                header_value(&second, "content-length").is_none_or(|value| value == "7"),
                "status {status}: stale content length in <redacted>"
            );
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
                "status {status}: <redacted>"
            );
        }
        for safe in ["accept", "accept-language", "accept-encoding"] {
            assert!(has_header(&second, safe), "status {status}: <redacted>");
        }
        source_server.join().unwrap();
        target.join().unwrap();
    }
}
