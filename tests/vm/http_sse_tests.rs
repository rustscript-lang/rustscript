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
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    vm.configure_http(config).unwrap();
    vm.set_async_bridge(Box::<TokioHostDriver>::default());
    standard_http_registry().bind_vm_cached(&mut vm).unwrap();
    // Bound the client-side VM drive so a rare lost producer/completion signal
    // can never strand the test thread indefinitely; it surfaces as a bounded,
    // diagnosable error instead. This pairs with the bounded server I/O and
    // bounded recv/join helpers so every test-side await is bounded.
    tokio::time::timeout(SERVER_IO_WATCHDOG, drive(&mut vm))
        .await
        .map_err(|_| {
            VmError::HostError(format!(
                "client-side SSE drive exceeded the {SERVER_IO_WATCHDOG:?} watchdog"
            ))
        })?
        .map(|()| vm)
}

// ----------------------------------------------------------------------
// Shared bounded I/O for HTTP/SSE test servers.
//
// Every server connection and every server cross-thread completion wait is
// bounded by a single watchdog so that a dropped/stalled/partial peer cannot
// hang the whole test binary indefinitely. The watchdog is sized for a loaded
// CI host and acts only as a liveness guard that converts an unbounded hang
// into a deterministic, diagnosable panic — never as a semantic timing
// tolerance (protocol assertions are unchanged).
// ----------------------------------------------------------------------

/// Shared liveness watchdog for all HTTP/SSE test-server socket I/O, request
/// receives, and thread joins.
const SERVER_IO_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(10);

/// Absolute cap on an HTTP request head read in tests. Larger heads are
/// reported as malformed/oversized rather than buffered without bound.
const MAX_REQUEST_HEAD_BYTES: usize = 64 * 1024;

/// Accept a connection and arm the shared read/write watchdog on the socket
/// BEFORE any blocking read/write, so a stalled/dropped peer can never strand
/// a server thread beyond `SERVER_IO_WATCHDOG` on a single system call.
fn accept_with_timeout(
    listener: &TcpListener,
) -> std::io::Result<(std::net::TcpStream, std::net::SocketAddr)> {
    let (stream, addr) = listener.accept()?;
    stream.set_read_timeout(Some(SERVER_IO_WATCHDOG))?;
    stream.set_write_timeout(Some(SERVER_IO_WATCHDOG))?;
    Ok((stream, addr))
}

/// Absolute deadline for reading a single server-side request (head + body).
/// Composed as now + watchdog; a slow-loris peer that dribbles bytes under the
/// per-call read timeout still terminates by this overall bound.
fn request_deadline() -> std::time::Instant {
    std::time::Instant::now()
        .checked_add(SERVER_IO_WATCHDOG)
        .expect("server request watchdog deadline must be representable")
}

/// Bounded read of an HTTP request head until the terminating blank line.
/// Terminates (with a diagnostic panic) on EOF, on the header-size cap, or on
/// the absolute `deadline`. Returns the raw head bytes, never lowercased, for
/// exact recording. The per-socket read timeout already bounds each `read`; the
/// absolute deadline additionally stops a slow-loris peer that dribbles bytes
/// under the per-call timeout. The caller must already have armed a socket read
/// timeout (see `accept_with_timeout`).
fn read_request_head_impl(
    stream: &mut std::net::TcpStream,
    deadline: std::time::Instant,
    context: &str,
) -> Vec<u8> {
    let mut head = Vec::with_capacity(128);
    let mut scratch = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_REQUEST_HEAD_BYTES {
            panic!(
                "{context}: request head exceeded {MAX_REQUEST_HEAD_BYTES} bytes \
                 (got {}); malformed/oversized peer",
                head.len()
            );
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "{context}: request head read exceeded watchdog at {} bytes; \
                 peer stalled on a partial head:\n{:?}",
                head.len(),
                String::from_utf8_lossy(&head)
            );
        }
        match stream.read(&mut scratch) {
            Ok(0) => panic!(
                "{context}: EOF while reading request head at {} bytes:\n{:?}",
                head.len(),
                String::from_utf8_lossy(&head)
            ),
            Ok(n) => head.extend_from_slice(&scratch[..n]),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Nothing available yet; the deadline check above terminates us.
            }
            Err(error) => panic!("{context}: request head read failed: {error}"),
        }
    }
    head
}

/// Read a request head under the shared `SERVER_IO_WATCHDOG` deadline.
fn read_request_head(stream: &mut std::net::TcpStream, context: &str) -> Vec<u8> {
    read_request_head_impl(stream, request_deadline(), context)
}

/// Parse the `Content-Length` declared in a raw request head, defaulting to 0.
fn declared_content_length(head: &str) -> usize {
    head.lines()
        .find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
        })
        .unwrap_or(0)
}

/// Bounded exact read of exactly `expected` bytes. Panics — never silently
/// truncates — if the peer closes before `expected` bytes arrive or the
/// absolute `deadline` elapses, reporting received/expected progress. The
/// per-socket read timeout already bounds each individual `read`; the absolute
/// deadline stops a peer that dribbles bytes under the per-call timeout.
fn read_exact_body_impl(
    stream: &mut std::net::TcpStream,
    expected: usize,
    deadline: std::time::Instant,
    context: &str,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(expected);
    let mut chunk = [0_u8; 4096];
    while body.len() < expected {
        if std::time::Instant::now() >= deadline {
            panic!(
                "{context}: body read exceeded watchdog; \
                 received {} of {expected} bytes; peer stalled mid-body",
                body.len()
            );
        }
        let want = (expected - body.len()).min(chunk.len());
        match stream.read(&mut chunk[..want]) {
            Ok(0) => panic!(
                "{context}: UnexpectedEof mid-body; received {} of {expected} bytes",
                body.len()
            ),
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Nothing available yet; the deadline check above terminates us.
            }
            Err(error) => panic!("{context}: body read failed: {error}"),
        }
    }
    body
}

/// Read a request body under the shared `SERVER_IO_WATCHDOG` deadline.
fn read_exact_body(stream: &mut std::net::TcpStream, expected: usize, context: &str) -> Vec<u8> {
    read_exact_body_impl(stream, expected, request_deadline(), context)
}

/// Receive the next recorded server request, waiting at most the shared
/// watchdog. On timeout or disconnect, panic with server/test context instead
/// of hanging the test binary forever.
fn recv_with_timeout<T>(receiver: &mpsc::Receiver<T>, context: &str) -> T {
    match receiver.recv_timeout(SERVER_IO_WATCHDOG) {
        Ok(value) => value,
        Err(mpsc::RecvTimeoutError::Timeout) => panic!(
            "{context}: timed out waiting for a server request after {SERVER_IO_WATCHDOG:?} \
             watchdog (server thread did not send)"
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!(
            "{context}: server request channel disconnected; the server thread closed \
             or panicked before recording the request"
        ),
    }
}

/// Join a server thread, polling `is_finished` against an absolute `deadline`.
/// The socket read/write timeout guarantees that a server blocked on I/O exits
/// shortly after the peek deadline, so a timeout panic never leaves a permanent
/// leak. On success the thread is always actually joined (no detach).
fn join_with_timeout_impl(
    handle: thread::JoinHandle<()>,
    deadline: std::time::Instant,
    context: &str,
) {
    while std::time::Instant::now() < deadline {
        if handle.is_finished() {
            handle.join().unwrap_or_else(|error| {
                let detail = error
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| error.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "no panic message".to_string());
                panic!("{context}: server thread panicked: {detail}");
            });
            return;
        }
        thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("{context}: server thread did not finish within the join watchdog");
}

/// Join a server thread under the shared `SERVER_IO_WATCHDOG` (+1s grace).
fn join_with_timeout(handle: thread::JoinHandle<()>, context: &str) {
    let deadline =
        std::time::Instant::now() + SERVER_IO_WATCHDOG + std::time::Duration::from_secs(1);
    join_with_timeout_impl(handle, deadline, context);
}

/// Probe that the peer closed a connection: read until the socket reports EOF
/// (Ok(0)) or ConnectionReset. Bounded by the socket read timeout armed in
/// `accept_with_timeout`; on any other outcome (data, stall past watchdog,
/// unrelated error) panic with server/test context. This is the focused
/// regression probe for production teardown: after a redirect response is
/// dropped unread by the client, the client must close the socket, and this
/// helper observes exactly that.
fn expect_peer_close(stream: &mut std::net::TcpStream, context: &str) {
    let mut buf = [0_u8; 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return, // EOF — peer closed.
            Ok(n) => panic!(
                "{context}: expected peer close but received {n} bytes: {:?}",
                String::from_utf8_lossy(&buf[..n])
            ),
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => return,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                panic!(
                    "{context}: expected peer close but the socket stayed open past the \
                     {SERVER_IO_WATCHDOG:?} watchdog"
                );
            }
            Err(error) => panic!("{context}: unexpected read error while probing close: {error}"),
        }
    }
}

fn server(response_parts: Vec<&'static [u8]>) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let (mut stream, _) = accept_with_timeout(&listener).unwrap();
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
        for (index, response_parts) in responses.into_iter().enumerate() {
            let context = format!("recording_server connection {index}");
            let (mut stream, _) = accept_with_timeout(&listener).unwrap();
            let head = read_request_head(&mut stream, &context);
            let content_length = declared_content_length(&String::from_utf8_lossy(&head));
            let body = read_exact_body(&mut stream, content_length, &context);
            sender
                .send(format!(
                    "{}{}",
                    String::from_utf8_lossy(&head),
                    String::from_utf8_lossy(&body)
                ))
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
        let (mut stream, _) = accept_with_timeout(&listener).unwrap();
        let head = read_request_head(&mut stream, "rejecting_redirect_server");
        sender.send(String::from_utf8(head).unwrap()).unwrap();
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
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    vm.configure_http(config(port)).unwrap();
    vm.set_async_bridge(Box::<TokioHostDriver>::default());
    standard_http_registry().bind_vm_cached(&mut vm).unwrap();

    drive(&mut vm).await.unwrap();
    join_with_timeout(
        server,
        "sse_delivers_open_events_end_and_terminal_summary server",
    );

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
        let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
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
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
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
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
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
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
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
    let request = recv_with_timeout(&requests, "sse_accepts_post_with_body").to_ascii_lowercase();
    assert!(request.starts_with("post /events http/1.1"));
    assert!(request.ends_with("payload"));
    join_with_timeout(server, "sse_accepts_post_with_body server");
}

fn redirect_server(status: u16) -> (u16, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        for index in 0..2 {
            let context = format!("redirect_server({status}) hop {index}");
            let (mut stream, _) = accept_with_timeout(&listener).unwrap();
            let head = read_request_head(&mut stream, &context);
            let content_length = declared_content_length(&String::from_utf8_lossy(&head));
            let body = read_exact_body(&mut stream, content_length, &context);
            sender
                .send(format!(
                    "{}{}",
                    String::from_utf8_lossy(&head),
                    String::from_utf8_lossy(&body)
                ))
                .unwrap();
            if index == 0 {
                write!(
                    stream,
                    "HTTP/1.1 {status} Redirect\r\nLocation: http://127.0.0.1:{port}/final\r\nContent-Length: 0\r\n\r\n"
                )
                .unwrap();
                // Focused teardown regression: after the client reads this
                // redirect response and drops it unread (production
                // `open_stream_response` `continue`), the client must close the
                // socket before connecting the next hop. Observe that close
                // directly; a hang/stall here would fail bounded instead of
                // stranding the server thread.
                expect_peer_close(&mut stream, &context);
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
        let first = recv_with_timeout(&requests, &format!("redirect status {status} first hop"))
            .to_ascii_lowercase();
        let second = recv_with_timeout(&requests, &format!("redirect status {status} second hop"))
            .to_ascii_lowercase();
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
        join_with_timeout(server, &format!("redirect status {status} server"));
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
    let request = recv_with_timeout(
        &requests,
        "sse_rejects_redirect_userinfo_before_reconnecting",
    )
    .to_ascii_lowercase();
    assert!(request.contains("authorization:"));
    assert!(request.contains("cookie: a=b"));
    assert!(!request.contains("redirect-user"));
    assert!(!request.contains("redirect-password"));
    join_with_timeout(
        server,
        "sse_rejects_redirect_userinfo_before_reconnecting server",
    );
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
        let request = recv_with_timeout(&requests, "sse_rejects_disallowed_redirect_targets")
            .to_ascii_lowercase();
        assert!(request.contains("authorization:"));
        assert!(request.contains("cookie: a=b"));
        join_with_timeout(
            source_server,
            "sse_rejects_disallowed_redirect_targets source server",
        );
        join_with_timeout(
            no_target_connection,
            "sse_rejects_disallowed_redirect_targets no-target probe",
        );
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
    join_with_timeout(
        server,
        "sse_stop_retires_without_end_and_returns_stopped_summary server",
    );
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
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
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
    join_with_timeout(
        server,
        "sse_reset_releases_the_connection_permit_before_reuse server",
    );
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
        join_with_timeout(
            server,
            "sse_rejects_status_content_type_and_idle_peer status loop",
        );
    }

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = accept_with_timeout(&listener).unwrap();
        let mut request = [0; 1024];
        let read = socket.read(&mut request).unwrap();
        assert!(read > 0, "SSE request should be received");
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
        socket.flush().unwrap();
        // Send nothing more and hold the socket open without closing: a close
        // would let hyper surface a connection-closed error that races and
        // masks the 20ms idle deadline under worker starvation (the body read
        // is polled before the idle timer). Holding the connection open leaves
        // only the idle able to fire, so the assertion is schedule-independent.
        thread::sleep(std::time::Duration::from_millis(500));
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
    join_with_timeout(
        server,
        "sse_rejects_status_content_type_and_idle_peer idle server",
    );

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = accept_with_timeout(&listener).unwrap();
        let mut request = [0; 1024];
        let read = socket.read(&mut request).unwrap();
        assert!(read > 0, "SSE request should be received");
        // Read the request then hold the socket open WITHOUT writing a response
        // or closing it: a close at 80ms would surface a connection error that
        // races the 20ms opening idle deadline under worker starvation and,
        // because the response arm is polled before the idle arm, would mask
        // the idle timeout. Holding the connection open leaves only the
        // opening idle able to fire, so the assertion is schedule-independent.
        thread::sleep(std::time::Duration::from_millis(500));
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
    join_with_timeout(
        server,
        "sse_rejects_status_content_type_and_idle_peer opening server",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn sse_script_timeout_shortens_the_host_stream_duration() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = accept_with_timeout(&listener).unwrap();
        let mut request = [0; 1024];
        // Read the request so the client's request write completes, then hold
        // the socket open WITHOUT writing a response or closing it. A close
        // would race the 20ms client budget: under worker starvation the
        // hyper connection-close error and the elapsed budget are both ready
        // when the client resumes, and `timeout_at` polls the inner future
        // first, masking the deadline with a spurious connection-closed error.
        // Holding the connection open leaves only the budget able to fire, so
        // the assertion is schedule-independent. The bounded sleep keeps the
        // accept thread from outliving the test (socket timeouts already
        // armed by `accept_with_timeout`).
        assert!(socket.read(&mut request).unwrap() > 0);
        thread::sleep(std::time::Duration::from_millis(500));
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
    join_with_timeout(
        server,
        "sse_script_timeout_shortens_the_host_stream_duration server",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn sse_host_stream_duration_caps_script_timeout_while_opening() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = accept_with_timeout(&listener).unwrap();
        let mut request = [0; 1024];
        // Read the request then hold the socket open without a response or
        // close (see `sse_script_timeout_shortens_the_host_stream_duration`):
        // the 20ms host budget must be the only thing able to fire during
        // opening, never a connection-closed error racing it under starvation.
        assert!(socket.read(&mut request).unwrap() > 0);
        thread::sleep(std::time::Duration::from_millis(500));
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
    join_with_timeout(
        server,
        "sse_host_stream_duration_caps_script_timeout_while_opening server",
    );
}

/// A stalled TLS handshake (the server accepts the TCP connection but never
/// speaks TLS) must surface the *connect-phase* error, never the SSE total
/// deadline. Connection establishment is bounded by `connect_timeout` only, so
/// a connect that never completes reports `HTTP connect deadline exceeded`
/// regardless of how long the stream budget is.
#[tokio::test(flavor = "current_thread")]
async fn sse_stalled_tls_connect_reports_connect_deadline_not_total() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        // Accept the TCP connection and then hold the socket open without
        // reading or writing: the client's TLS connect future stays pending
        // waiting for a ServerHello until its connect deadline fires. The
        // bounded sleep keeps the accept thread from outliving the test.
        let (mut socket, _) = accept_with_timeout(&listener).unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();
        let mut buf = [0_u8; 1024];
        // Read the ClientHello if the client sends one; never reply. A read
        // timeout (client sent nothing yet) is fine; a successful read just
        // means the ClientHello arrived. Either way we never write a
        // ServerHello, so the TLS handshake stalls until the connect deadline.
        let _ = socket.read(&mut buf);
        thread::sleep(std::time::Duration::from_millis(300));
    });
    let mut connect_config = config(port);
    connect_config.allowed_schemes = vec!["https".into()];
    connect_config.connect_timeout = std::time::Duration::from_millis(200);
    // The stream budget and idle timeout are far longer than the connect
    // timeout, so only the connect deadline can expire.
    connect_config.max_stream_duration = std::time::Duration::from_secs(5);
    connect_config.stream_idle_timeout = std::time::Duration::from_secs(5);
    let source = format!(
        r#"use http; fn go(item: map) -> map {{ {{action:"continue"}} }} http::client::sse({{"method":"GET","url":"https://127.0.0.1:{port}/events"}}, go);"#
    );
    let error = match run_sse_source(&source, connect_config).await {
        Ok(_) => panic!("a stalled TLS connect must time out"),
        Err(error) => error,
    };
    assert!(
        matches!(
            &error,
            VmError::HostError(message) if message == "HTTP connect deadline exceeded"
        ),
        "stalled connect must report the connect deadline, got {error}"
    );
    assert!(
        !error.to_string().contains("SSE total deadline exceeded"),
        "a connect timeout must not be mislabelled as the SSE total deadline: {error}"
    );
    join_with_timeout(
        server,
        "sse_stalled_tls_connect_reports_connect_deadline server",
    );
}

/// A server that accepts the connection and reads the request but withholds the
/// response headers must expire the *response* budget and surface the SSE total
/// deadline, distinct from the connect-phase error. The response budget starts
/// when the request is actually written, so only the stream duration can fire.
#[tokio::test(flavor = "current_thread")]
async fn sse_withheld_headers_reports_total_deadline_not_connect() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = accept_with_timeout(&listener).unwrap();
        let mut request = [0_u8; 1024];
        // Read the request so the client's request write completes, then never
        // write any response: the response-header wait stalls.
        assert!(socket.read(&mut request).unwrap() > 0);
        thread::sleep(std::time::Duration::from_millis(500));
    });
    let mut deadline_config = config(port);
    // The response budget (stream duration) is short, while the connect
    // timeout and idle timeout are long, so only the response budget can fire.
    deadline_config.max_stream_duration = std::time::Duration::from_millis(200);
    deadline_config.connect_timeout = std::time::Duration::from_secs(5);
    deadline_config.stream_idle_timeout = std::time::Duration::from_secs(5);
    let source = format!(
        r#"use http; fn go(item: map) -> map {{ {{action:"continue"}} }} http::client::sse({{"method":"GET","url":"http://127.0.0.1:{port}/events"}}, go);"#
    );
    let error = match run_sse_source(&source, deadline_config).await {
        Ok(_) => panic!("withheld response headers must time out"),
        Err(error) => error,
    };
    assert!(
        matches!(
            &error,
            VmError::HostError(message) if message == "SSE total deadline exceeded"
        ),
        "withheld headers must report the SSE total deadline, got {error}"
    );
    assert!(
        !error.to_string().contains("HTTP connect deadline exceeded"),
        "a response-budget expiry must not be mislabelled as a connect timeout: {error}"
    );
    join_with_timeout(server, "sse_withheld_headers_reports_total_deadline server");
}

#[tokio::test(flavor = "current_thread")]
async fn sse_total_deadline_expires_despite_periodic_progress_below_idle_timeout() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = accept_with_timeout(&listener).unwrap();
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
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
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
    join_with_timeout(
        server,
        "sse_total_deadline_expires_despite_periodic_progress_below_idle_timeout server",
    );
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
        let (mut first, _) = accept_with_timeout(&listener).unwrap();
        let mut request = [0; 1024];
        assert!(first.read(&mut request).unwrap() > 0);
        let first = thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(80));
            drop(first);
        });

        let (mut second, _) = accept_with_timeout(&listener).unwrap();
        let mut request = [0; 1024];
        assert!(second.read(&mut request).unwrap() > 0);
        second
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\n\r\n",
            )
            .unwrap();
        join_with_timeout(first, "sse_total_deadline_releases first drop-thread");
    });
    let source = format!(
        r#"use http; http::client::sse({{"method":"GET","url":"http://127.0.0.1:{port}/events"}}, |item| {{action:"continue"}});"#
    );
    let compiled = compile_source(&source).unwrap();
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
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
    // The reset is asynchronous (the worker tears down and releases the
    // connection permit on a separate thread); drive it to completion before
    // reusing the VM so the second stream deterministically acquires the
    // released permit even under scheduler pressure.
    vm.reset_for_reuse();
    drive_reset(&mut vm).await;
    assert!(vm.is_reusable(), "VM should be reusable after reset");
    // The second stream's purpose is only to prove the released permit allows
    // a fresh stream; give it a generous budget so a loaded CI (server OS
    // thread contending with parallel tests) is not penalised by the tight
    // 20ms total deadline the first stream deliberately trips.
    vm.configure_http(config(port)).unwrap();
    drive(&mut vm)
        .await
        .expect("the second stream should acquire the released permit");
    assert_eq!(field(&vm.stack()[0], "outcome"), &Value::string("eof"));
    join_with_timeout(
        server,
        "sse_total_deadline_releases_the_connection_permit_for_reuse server",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn sse_callback_stop_after_deadline_fails_and_releases_permit_without_another_poll() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut first, _) = accept_with_timeout(&listener).unwrap();
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

        let (mut second, _) = accept_with_timeout(&listener).unwrap();
        let mut request = [0; 1024];
        assert!(second.read(&mut request).unwrap() > 0);
        second
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\n\r\n",
            )
            .unwrap();
        join_with_timeout(first, "sse_callback_stop first drop-thread");
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
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
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
    // Drive the reset to completion (the worker tears down and releases the
    // connection permit asynchronously) so the next stream deterministically
    // acquires the released permit even under scheduler pressure.
    drive_reset(&mut vm).await;
    assert!(vm.is_reusable(), "VM should be reusable after reset");
    // The second stream's purpose is only to prove the released permit allows
    // a fresh stream; give it a generous budget so a loaded CI (server OS
    // thread contending with parallel tests) is not penalised by the tight
    // 100ms total deadline the first stream deliberately trips.
    let second_config = config(port);
    vm.configure_http(second_config).unwrap();
    drive(&mut vm)
        .await
        .expect("the next stream should acquire the released permit");
    assert_eq!(wait_calls.load(Ordering::SeqCst), 2);
    assert_eq!(field(&vm.stack()[0], "outcome"), &Value::string("stopped"));
    join_with_timeout(server, "sse_callback_stop_after_deadline_fails server");
}

#[tokio::test(flavor = "current_thread")]
async fn sse_callback_continue_after_deadline_fails_before_another_network_poll() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = accept_with_timeout(&listener).unwrap();
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
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
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
    join_with_timeout(server, "sse_callback_continue_after_deadline_fails server");
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
    let first = recv_with_timeout(&source_requests, "sse_revalidates_redirects source hop")
        .to_ascii_lowercase();
    assert!(first.starts_with("post /start http/1.1"));
    assert!(first.ends_with("payload"));
    assert!(first.contains("authorization: bearer secret"));
    assert!(first.contains("cookie: a=b"));
    let second = recv_with_timeout(&target_requests, "sse_revalidates_redirects target hop")
        .to_ascii_lowercase();
    assert!(second.starts_with("post /final http/1.1"));
    assert!(second.ends_with("payload"));
    assert!(!second.contains("authorization:"));
    assert!(!second.contains("cookie:"));
    join_with_timeout(source_server, "sse_revalidates_redirects source server");
    join_with_timeout(target, "sse_revalidates_redirects target server");
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
        let (mut stream, _) = accept_with_timeout(&listener).unwrap();
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
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
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
    join_with_timeout(server, "sse_silent_server_reset_cancels_worker server");
}

// ----------------------------------------------------------------------
// Helper watchdog unit tests.
//
// These prove that the bounded server I/O helpers convert what would be an
// unbounded hang (a peer that stalls mid-head or mid-body) into a
// deterministic, diagnosable panic, and that the cross-thread waits
// (`recv_with_timeout`, `join_with_timeout`) terminate bounded. They use a
// short synthetic deadline so they run in milliseconds, not the 10s shared
// watchdog.
// ----------------------------------------------------------------------

#[test]
fn watchdog_converts_partial_head_stall_into_bounded_panic() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let peer = thread::spawn(move || {
        let (mut socket, _) = accept_with_timeout(&listener).unwrap();
        // Send only a partial head (no terminating blank line), then stall.
        socket
            .write_all(b"POST /start HTTP/1.1\r\nContent-Length: 7\r\nHost: x\r\n")
            .unwrap();
        // Hold the connection open without sending the blank line or any more
        // bytes, so an unbounded reader would block forever. Outlasts the
        // client deadline so the stall is what terminates the read, yet
        // finishes within the bounded join window.
        thread::sleep(std::time::Duration::from_secs(8));
    });
    let mut client = std::net::TcpStream::connect(address).unwrap();
    client
        .set_read_timeout(Some(std::time::Duration::from_millis(100)))
        .unwrap();
    // A few seconds: large enough not to spurious-fire under heavy parallel
    // test load, small enough to fail fast when the helper is correct.
    let short_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        read_request_head_impl(&mut client, short_deadline, "partial-head stall test")
    }));
    join_with_timeout(peer, "partial-head stall peer");
    let message = match result {
        Err(payload) => payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default(),
        Ok(_) => panic!("partial-head stall must panic via the watchdog, not hang or return"),
    };
    assert!(
        message.contains("watchdog") && message.contains("partial head"),
        "expected a watchdog diagnostic, got: {message}"
    );
}

#[test]
fn watchdog_converts_partial_body_stall_into_bounded_panic() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let peer = thread::spawn(move || {
        let (mut socket, _) = accept_with_timeout(&listener).unwrap();
        // Send only 3 of the 7 declared body bytes, then stall with the
        // connection still open.
        socket.write_all(b"pay").unwrap();
        // Hold the connection open so an unbounded reader would block forever;
        // outlasts the client deadline so the stall is what terminates the
        // read, yet finishes within the bounded join window.
        thread::sleep(std::time::Duration::from_secs(8));
    });
    let mut client = std::net::TcpStream::connect(address).unwrap();
    client
        .set_read_timeout(Some(std::time::Duration::from_millis(100)))
        .unwrap();
    let short_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        read_exact_body_impl(&mut client, 7, short_deadline, "partial-body stall test")
    }));
    join_with_timeout(peer, "partial-body stall peer");
    let message = match result {
        Err(payload) => payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default(),
        Ok(_) => panic!("partial-body stall must panic via the watchdog, not hang or return"),
    };
    assert!(
        message.contains("watchdog") && message.contains("received 3 of 7 bytes"),
        "expected received/expected progress in the diagnostic, got: {message}"
    );
}

#[test]
fn watchdog_converts_truncated_body_eof_into_bounded_panic() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let peer = thread::spawn(move || {
        let (mut socket, _) = accept_with_timeout(&listener).unwrap();
        // Send fewer bytes than declared, then close (EOF mid-body).
        socket.write_all(b"pay").unwrap();
    });
    let mut client = std::net::TcpStream::connect(address).unwrap();
    client
        .set_read_timeout(Some(std::time::Duration::from_millis(100)))
        .unwrap();
    let short_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        read_exact_body_impl(&mut client, 7, short_deadline, "truncated-body test")
    }));
    join_with_timeout(peer, "truncated-body peer");
    let message = match result {
        Err(payload) => payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default(),
        Ok(_) => panic!("truncated body must panic, never silently truncate"),
    };
    assert!(
        message.contains("UnexpectedEof mid-body") && message.contains("received 3 of 7 bytes"),
        "expected an EOF diagnostic with received/expected progress, got: {message}"
    );
}

#[test]
fn recv_with_timeout_panics_bounded_on_disconnect() {
    let (sender, receiver) = mpsc::channel::<String>();
    drop(sender); // Disconnect immediately: recv must panic, not hang.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        recv_with_timeout(&receiver, "disconnect test")
    }));
    let message = match result {
        Err(payload) => payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default(),
        Ok(_) => panic!("disconnected recv must panic"),
    };
    assert!(
        message.contains("disconnected"),
        "expected a disconnected diagnostic, got: {message}"
    );
}

#[test]
fn join_with_timeout_panics_bounded_on_stuck_thread() {
    // A thread that does not finish by the deadline must be reported by the
    // bounded join instead of hanging the test binary. Use a short synthetic
    // deadline so the unit test is fast; the thread is a bounded sleeper that
    // finishes on its own shortly after (no permanent leak).
    let stuck = thread::spawn(|| {
        thread::sleep(std::time::Duration::from_millis(2000));
    });
    let short_deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        join_with_timeout_impl(stuck, short_deadline, "stuck thread test")
    }));
    let message = match result {
        Err(payload) => payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default(),
        Ok(_) => panic!("a stuck thread must produce a bounded join panic"),
    };
    assert!(
        message.contains("did not finish"),
        "expected a bounded join diagnostic, got: {message}"
    );
}
