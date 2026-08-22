use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll};
use std::thread;

use vm::{
    CallOutcome, CallReturn, HostAsyncBridge, HostFunctionRegistry, HostFuture, HostFutureOutput,
    HostOpId, HostStackFunction, HttpConfig, HttpHostExt, Value, Vm, VmError, VmMap, VmResetState,
    VmResult, VmStatus, compile_source, register_http_builtin_module,
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

/// Drives an in-progress reset to completion by polling with a real waker.
/// Returns once the VM is reusable (Ready state) or panics on timeout.
async fn drive_reset(vm: &mut Vm) {
    use std::sync::Arc;
    use std::task::Wake;
    use std::time::Duration;
    // Use a notify-based waker so the worker thread can wake us when it exits.
    let notify = Arc::new(tokio::sync::Notify::new());
    struct ResetWaker {
        notify: Arc<tokio::sync::Notify>,
    }
    impl Wake for ResetWaker {
        fn wake(self: Arc<Self>) {
            self.notify.notify_one();
        }
    }
    let waker = Arc::new(ResetWaker {
        notify: notify.clone(),
    })
    .into();
    for _ in 0..100 {
        if vm.reset_state() == VmResetState::Ready {
            return;
        }
        let mut cx = Context::from_waker(&waker);
        match vm.poll_reset_for_reuse(&mut cx, std::time::Instant::now()) {
            Poll::Ready(Ok(())) => return,
            Poll::Ready(Err(error)) => panic!("reset failed: {error}"),
            Poll::Pending => {
                // Wait for the worker thread to wake us, or timeout.
                tokio::select! {
                    _ = notify.notified() => {},
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                }
            }
        }
    }
    panic!("reset did not complete within 100 polls");
}

/// Creates a registry with the standard HTTP extension registered against the
/// authoritative combined snapshot, so exact V13 `http::*` imports from the
/// standard compile entry bind and execute without legacy name-only fallback.
fn standard_http_registry() -> HostFunctionRegistry {
    let mut registry = HostFunctionRegistry::new();
    register_http_builtin_module(&mut registry)
        .expect("standard HTTP registration against the combined snapshot should succeed");
    registry
}

async fn run_sse_source(source: &str, config: HttpConfig) -> Result<Vm, vm::VmError> {
    let compiled = compile_source(source).expect("SSE source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.configure_http(config).unwrap();
    vm.set_async_bridge(Box::<TokioHostDriver>::default());
    standard_http_registry().bind_vm_cached(&mut vm).unwrap();
    drive(&mut vm).await.map(|()| vm)
}

fn server(response_parts: Vec<&'static [u8]>) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4096];
        let read = stream.read(&mut request).unwrap();
        let request = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
        assert!(request.starts_with("get /events http/1.1"));
        assert!(request.contains("accept: text/event-stream"));
        for part in response_parts {
            stream.write_all(part).unwrap();
            stream.flush().unwrap();
        }
    });
    (port, handle)
}

fn recording_server(
    responses: Vec<Vec<&'static [u8]>>,
) -> (u16, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        for response_parts in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
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
            stream.read_exact(&mut body).unwrap();
            sender
                .send(format!("{head}{}", String::from_utf8_lossy(&body)))
                .unwrap();
            for part in response_parts {
                stream.write_all(part).unwrap();
                stream.flush().unwrap();
            }
        }
    });
    (port, receiver, handle)
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

fn assert_no_connection(listener: TcpListener, context: &'static str) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            match listener.accept() {
                Ok(_) => panic!("{context} must be rejected before a second connection"),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return;
                    }
                    thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => panic!("unexpected accept error: {error}"),
            }
        }
    })
}

fn rejecting_redirect_server(
    location: impl FnOnce(u16) -> String + Send + 'static,
) -> (u16, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let location = location(port);
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
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
        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            match listener.accept() {
                Ok(_) => panic!("invalid redirect must be rejected before a second connection"),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return;
                    }
                    thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => panic!("unexpected accept error: {error}"),
            }
        }
    });
    (port, receiver, handle)
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
    vm.set_async_bridge(Box::<TokioHostDriver>::default());
    standard_http_registry().bind_vm_cached(&mut vm).unwrap();

    drive(&mut vm).await.unwrap();
    server.join().unwrap();

    let result = &vm.stack()[0];
    assert_eq!(field(result, "outcome"), &Value::string("eof"));
    assert_eq!(field(result, "status"), &Value::Int(200));
    assert_eq!(field(result, "items"), &Value::Int(4));
    assert!(
        matches!(field(result, "bytes_received"), Value::Int(n) if *n > 0),
        "bytes_received must be positive when data was delivered, got: {:?}",
        field(result, "bytes_received"),
    );
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
        standard_http_registry().bind_vm_cached(&mut vm).unwrap();
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
    standard_http_registry().bind_vm_cached(&mut vm).unwrap();
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
    standard_http_registry().bind_vm_cached(&mut vm).unwrap();
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
    vm.set_async_bridge(Box::<TokioHostDriver>::default());
    standard_http_registry().bind_vm_cached(&mut vm).unwrap();
    assert!(matches!(vm.run().unwrap(), VmStatus::Waiting(_)));
    vm.reset_for_reuse();
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
    let vm = run_sse_source(&source, config(port)).await.unwrap();
    assert_eq!(field(&vm.stack()[0], "outcome"), &Value::string("eof"));
    let request = requests.recv().unwrap().to_ascii_lowercase();
    assert!(request.starts_with("post /events http/1.1"));
    assert!(request.ends_with("payload"));
    server.join().unwrap();
}

fn redirect_server(status: u16) -> (u16, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        for index in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
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
    (port, receiver, handle)
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
        let first = requests.recv().unwrap().to_ascii_lowercase();
        let second = requests.recv().unwrap().to_ascii_lowercase();
        assert!(first.starts_with("post /start http/1.1"));
        if preserves_post {
            assert!(
                second.starts_with("post /final http/1.1"),
                "status {status}: {second}"
            );
            assert!(second.ends_with("payload"), "status {status}: {second}");
        } else {
            assert!(
                second.starts_with("get /final http/1.1"),
                "status {status}: {second}"
            );
            assert!(!second.ends_with("payload"), "status {status}: {second}");
        }
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
    let request = requests.recv().unwrap().to_ascii_lowercase();
    assert!(request.contains("authorization:"));
    assert!(request.contains("cookie: a=b"));
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
        let target_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
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
        let request = requests.recv().unwrap().to_ascii_lowercase();
        assert!(request.contains("authorization:"));
        assert!(request.contains("cookie: a=b"));
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
async fn sse_reset_releases_the_connection_permit_before_reuse() {
    let (port, server) = server(vec![
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\n\r\n",
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
    vm.set_async_bridge(Box::<TokioHostDriver>::default());
    standard_http_registry().bind_vm_cached(&mut vm).unwrap();

    assert!(matches!(vm.run().unwrap(), VmStatus::Waiting(_)));
    // Begin the reset. The scope close sets stopping on the shared state,
    // which the worker thread observes between items and stops promptly.
    vm.reset_for_reuse();
    // Drive the reset to completion with a real waker. The worker thread
    // exits after seeing the stopping flag; poll until quiescent.
    drive_reset(&mut vm).await;
    assert!(vm.is_reusable(), "VM should be reusable after reset");
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

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0; 1024];
        let read = socket.read(&mut request).unwrap();
        assert!(read > 0, "SSE request should be received");
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
        socket.flush().unwrap();
        thread::sleep(std::time::Duration::from_millis(80));
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

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0; 1024];
        let read = socket.read(&mut request).unwrap();
        assert!(read > 0, "SSE request should be received");
        thread::sleep(std::time::Duration::from_millis(80));
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
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0; 1024];
        assert!(socket.read(&mut request).unwrap() > 0);
        thread::sleep(std::time::Duration::from_millis(80));
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
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0; 1024];
        assert!(socket.read(&mut request).unwrap() > 0);
        thread::sleep(std::time::Duration::from_millis(80));
    });
    let mut deadline_config = config(port);
    deadline_config.max_stream_duration = std::time::Duration::from_millis(20);
    deadline_config.stream_idle_timeout = std::time::Duration::from_millis(200);
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
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0; 1024];
        assert!(socket.read(&mut request).unwrap() > 0);
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
        socket.flush().unwrap();
        for _ in 0..40 {
            thread::sleep(std::time::Duration::from_millis(25));
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
    vm.set_async_bridge(Box::<TokioHostDriver>::default());
    let mut registry = standard_http_registry();
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
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        let mut request = [0; 1024];
        assert!(first.read(&mut request).unwrap() > 0);
        let first = thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(80));
            drop(first);
        });

        let (mut second, _) = listener.accept().unwrap();
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
    vm.set_async_bridge(Box::<TokioHostDriver>::default());
    standard_http_registry().bind_vm_cached(&mut vm).unwrap();

    let error = drive(&mut vm)
        .await
        .expect_err("the first stream should reach its total deadline");
    assert!(error.to_string().contains("total deadline"), "{error}");
    vm.reset_for_reuse();
    drive(&mut vm)
        .await
        .expect("the second stream should acquire the released permit");
    assert_eq!(field(&vm.stack()[0], "outcome"), &Value::string("eof"));
    server.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn sse_callback_stop_after_deadline_fails_and_releases_permit_without_another_poll() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        let mut request = [0; 1024];
        assert!(first.read(&mut request).unwrap() > 0);
        first
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .unwrap();
        first.flush().unwrap();
        let first = thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(500));
            drop(first);
        });

        let (mut second, _) = listener.accept().unwrap();
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
    vm.set_async_bridge(Box::<TokioHostDriver>::default());
    let wait_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = standard_http_registry();
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

    vm.reset_for_reuse();
    drive(&mut vm)
        .await
        .expect("the next stream should acquire the released permit");
    assert_eq!(wait_calls.load(Ordering::SeqCst), 2);
    assert_eq!(field(&vm.stack()[0], "outcome"), &Value::string("stopped"));
    server.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn sse_callback_continue_after_deadline_fails_before_another_network_poll() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0; 1024];
        assert!(socket.read(&mut request).unwrap() > 0);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .unwrap();
        socket.flush().unwrap();
        thread::sleep(std::time::Duration::from_millis(500));
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
    vm.set_async_bridge(Box::<TokioHostDriver>::default());
    let wait_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = standard_http_registry();
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
async fn sse_revalidates_redirects_and_strips_cross_origin_credentials() {
    let (target_port, target_requests, target) = recording_server(vec![vec![
        b"HTTP/1.1 200 OK\r\nContent-Type: Text/Event-Stream; Charset=UTF-8\r\nX-Obs: \x80\r\nContent-Length: 0\r\n\r\n",
    ]]);
    let redirect = format!(
        "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:{target_port}/final\r\nContent-Length: 0\r\n\r\n"
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
            {{method: "POST", url: "http://127.0.0.1:{source_port}/start", body: "payload", headers: {{Authorization: "Bearer secret", Cookie: "a=b"}}}},
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
    let first = source_requests.recv().unwrap().to_ascii_lowercase();
    assert!(first.starts_with("post /start http/1.1"));
    assert!(first.ends_with("payload"));
    assert!(first.contains("authorization: bearer secret"));
    assert!(first.contains("cookie: a=b"));
    let second = target_requests.recv().unwrap().to_ascii_lowercase();
    assert!(second.starts_with("post /final http/1.1"));
    assert!(second.ends_with("payload"));
    assert!(!second.contains("authorization:"));
    assert!(!second.contains("cookie:"));
    source_server.join().unwrap();
    target.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn sse_silent_server_reset_cancels_worker_and_releases_permit() {
    // This test verifies that a silent server (sends headers, then stays
    // silent on the TCP connection) does not prevent scope close from
    // cancelling the worker. The cancellable network read via Notify +
    // select! ensures the worker wakes promptly when the scope is closed,
    // without waiting for the server to send another frame.
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 4096];
        let read = stream.read(&mut request).unwrap();
        assert!(read > 0, "SSE request should be received");
        // Send SSE headers, then stay silent (no body data).
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .unwrap();
        stream.flush().unwrap();
        // Block on reading from the socket. When the worker is cancelled,
        // the OwnedResponse is dropped, closing the TCP connection, and
        // this read returns 0 (EOF) or ConnectionReset.
        let mut buf = [0; 1024];
        match stream.read(&mut buf) {
            Ok(0) => {} // Connection closed by peer — expected.
            Ok(n) => panic!("unexpected data after SSE headers: {n} bytes"),
            Err(error) => {
                assert_eq!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset,
                    "unexpected read error: {error}"
                );
            }
        }
    });

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
    vm.set_async_bridge(Box::<TokioHostDriver>::default());
    standard_http_registry().bind_vm_cached(&mut vm).unwrap();

    // Start the SSE stream. It should be pending (waiting for the callback).
    assert!(matches!(vm.run().unwrap(), VmStatus::Waiting(_)));

    // Begin the reset. The scope close sets stopping and notifies the
    // cancel Notify, which the worker observes inside the select! and
    // stops promptly without waiting for the silent server.
    vm.reset_for_reuse();

    // Drive the reset to completion with a real waker.
    drive_reset(&mut vm).await;
    assert!(vm.is_reusable(), "VM should be reusable after reset");

    // The server should have detected the connection close (read returned
    // 0 or ConnectionReset), proving the worker was cancelled without
    // waiting for another frame from the silent server.
    server.join().expect("silent-server thread should finish");
}
