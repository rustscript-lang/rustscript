#![cfg(all(feature = "http-client", not(target_family = "wasm")))]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;
use vm::{HostFunctionRegistry, HttpConfig, HttpHostExt, Value, Vm, VmStatus, compile_source};

fn config(port: u16) -> HttpConfig {
    HttpConfig {
        allowed_schemes: vec!["http".into()],
        allowed_hosts: vec!["127.0.0.1".into()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        ..Default::default()
    }
}

fn server(body: &'static [u8]) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let thread = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = [0; 4096];
        assert!(stream.read(&mut request).unwrap() > 0);
        stream.write_all(body).unwrap();
    });
    (port, thread)
}

async fn run(source: &str, port: u16) -> Vm {
    let mut vm = Vm::new(compile_source(source).unwrap().program);
    vm.configure_http(config(port)).unwrap();
    HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();
    let mut status = vm.run().unwrap();
    loop {
        status = match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => vm.resume().unwrap(),
            VmStatus::Waiting(_) => {
                vm.await_waiting_host_op().await.unwrap();
                vm.resume().unwrap()
            }
        };
    }
    vm
}

#[test]
fn resource_sse_compile_gate_rejects_old_map_and_wrong_callbacks() {
    for source in [
        "use http; fn event(a: string,b: string,c: string,d: string) -> bool { true } http::client::sse({method: \"GET\", url: \"http://example.com\"}, event);",
        "use http; fn event(a: string,b: string,c: string,d: string) -> int { 1 } let req = http::request::new(\"GET\", \"http://example.com\"); http::client::sse(req, event);",
        "use http; fn event(a: string,b: string,c: string,d: string) -> bool { true } fn open(status: int, headers: int, url: string) -> bool { true } let req = http::request::new(\"GET\", \"http://example.com\"); http::client::sse(req, event, open);",
    ] {
        assert!(compile_source(source).is_err(), "must reject: {source}");
    }
    compile_source("use http; fn event(a: string,b: string,c: string,d: string) -> bool { true } let req = http::request::new(\"GET\", \"http://example.com\"); http::client::sse(req, event);").unwrap();
}

#[test]
fn resource_sse_permission_and_timeout_admission_precede_network() {
    let source = r#"use http;
        fn event(a: string, b: string, c: string, d: string) -> bool { true }
        let req = http::request::new("GET", "http://127.0.0.1:1/events");
        http::client::sse(req, event, 0);"#;
    let mut vm = Vm::new(compile_source(source).unwrap().program);
    vm.configure_http(config(1)).unwrap();
    vm.set_http_max_in_flight(0);
    HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();
    let error = vm.run().unwrap_err();
    assert!(error.to_string().contains("positive"), "{error}");
    assert_eq!(vm.host_context().operation_count(), 0);
    assert_eq!(vm.host_context().resource_count(), 0);

    let program = compile_source(source).unwrap().program;
    let mut restricted = Vm::new(program);
    let mut registry = HostFunctionRegistry::restricted();
    registry.set_capability_profile(
        vm::CapabilityProfile::builder()
            .allow_host_import("http::request::new")
            .build(),
    );
    let error = registry.bind_vm_cached(&mut restricted).unwrap_err();
    assert!(error.to_string().contains("http::client::sse"), "{error}");
    assert_eq!(restricted.host_context().resource_count(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn resource_sse_default_binding_dispatches_open_overload() {
    let (port, server) =
        server(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\n\r\n");
    let source = format!(
        r#"use http;
        fn event(a: string, b: string, c: string, d: string) -> bool {{ true }}
        fn opened(status: int, headers: resource<http.headers>, url: string) -> bool {{ status == 200 }}
        let req = http::request::new("GET", "http://127.0.0.1:{port}/events");
        let summary = http::client::sse(req, event, opened);
        http::sse_summary::status(&summary);"#
    );
    let mut vm = Vm::new(compile_source(&source).unwrap().program);
    vm.configure_http(config(port)).unwrap();
    let mut status = vm.run().unwrap();
    loop {
        status = match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => vm.resume().unwrap(),
            VmStatus::Waiting(_) => {
                vm.await_waiting_host_op().await.unwrap();
                vm.resume().unwrap()
            }
        };
    }
    server.join().unwrap();
    assert_eq!(vm.stack().last(), Some(&Value::Int(200)));
}

#[tokio::test(flavor = "current_thread")]
async fn resource_sse_event_false_stops_with_optional_timeout() {
    let (port, server) = server(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 18\r\n\r\ndata: x\n\ndata: y\n\n");
    let source = format!(
        r#"use http;
        fn event(a: string, b: string, c: string, d: string) -> bool {{ b != "x" }}
        let req = http::request::new("GET", "http://127.0.0.1:{port}/events");
        let summary = http::client::sse(req, event, 5000);
        [http::sse_summary::outcome(&summary), http::sse_summary::items(&summary)];"#
    );
    let vm = run(&source, port).await;
    server.join().unwrap();
    assert_eq!(
        vm.stack().last(),
        Some(&Value::array(vec![Value::string("stopped"), Value::Int(1)]))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn resource_sse_open_false_stops_before_event() {
    let (port, server) = server(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 9\r\n\r\ndata: x\n\n");
    let source = format!(
        r#"use http;
        fn event(a: string, b: string, c: string, d: string) -> bool {{ let x = 1 / 0; true }}
        fn opened(status: int, headers: resource<http.headers>, url: string) -> bool {{ false }}
        let req = http::request::new("GET", "http://127.0.0.1:{port}/events");
        let summary = http::client::sse(req, event, opened);
        [http::sse_summary::outcome(&summary), http::sse_summary::items(&summary)];"#
    );
    let vm = run(&source, port).await;
    server.join().unwrap();
    assert_eq!(
        vm.stack().last(),
        Some(&Value::array(vec![Value::string("stopped"), Value::Int(0)]))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn resource_sse_disconnect_aborts_and_releases_scope() {
    let (port, server) = server(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n9\r\ndata: x\n");
    let source = format!(
        r#"use http;
        fn event(a: string, b: string, c: string, d: string) -> bool {{ true }}
        let req = http::request::new("GET", "http://127.0.0.1:{port}/events");
        http::client::sse(req, event);"#
    );
    let mut vm = Vm::new(compile_source(&source).unwrap().program);
    vm.configure_http(config(port)).unwrap();
    HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();
    let mut status = vm.run().unwrap();
    let error = loop {
        match status {
            VmStatus::Halted => panic!("truncated stream must fail"),
            VmStatus::Yielded => status = vm.resume().unwrap(),
            VmStatus::Waiting(_) => match vm.await_waiting_host_op().await {
                Ok(()) => match vm.resume() {
                    Ok(next) => status = next,
                    Err(error) => break error,
                },
                Err(error) => break error,
            },
        }
    };
    assert!(
        error.to_string().contains("connection") || error.to_string().contains("body"),
        "{error}"
    );
    vm.reset_for_reuse().unwrap();
    std::future::poll_fn(|cx| vm.poll_reset_for_reuse(cx))
        .await
        .unwrap();
    assert_eq!(vm.host_context().resource_count(), 0);
    assert_eq!(vm.host_context().operation_count(), 0);
    server.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn resource_sse_reset_closes_pending_connection_and_scope() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = [0; 4096];
        assert!(stream.read(&mut bytes).unwrap() > 0);
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
        tx.send(()).unwrap();
        assert_eq!(stream.read(&mut bytes).unwrap(), 0);
    });
    let source = format!(
        r#"use http;
        fn event(a: string, b: string, c: string, d: string) -> bool {{ true }}
        let req = http::request::new("GET", "http://127.0.0.1:{port}/events");
        http::client::sse(req, event);"#
    );
    let mut vm = Vm::new(compile_source(&source).unwrap().program);
    vm.configure_http(config(port)).unwrap();
    HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();
    assert!(matches!(vm.run().unwrap(), VmStatus::Waiting(_)));
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    vm.reset_for_reuse().unwrap();
    std::future::poll_fn(|cx| vm.poll_reset_for_reuse(cx))
        .await
        .unwrap();
    assert_eq!(vm.host_context().operation_count(), 0);
    assert_eq!(vm.host_context().resource_count(), 0);
    server.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn resource_sse_without_open_callback_still_returns_headers() {
    let (port, server) = server(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nX-Marker: yes\r\nContent-Length: 0\r\n\r\n");
    let source = format!(
        r#"use http;
        fn event(a: string, b: string, c: string, d: string) -> bool {{ true }}
        let req = http::request::new("GET", "http://127.0.0.1:{port}/events");
        let summary = http::client::sse(req, event);
        http::headers::values(&http::sse_summary::headers(&summary), "x-marker");"#
    );
    let vm = run(&source, port).await;
    server.join().unwrap();
    assert_eq!(
        vm.stack().last(),
        Some(&Value::array(vec![Value::string("yes")]))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn resource_sse_open_headers_events_and_summary_are_independent() {
    let (port, server) = server(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nX-Raw: \x80\r\nX-Raw: \xc3\xa9\r\nContent-Length: 29\r\n\r\nevent: named\ndata: hi\nid: 7\n\n");
    let source = format!(
        r#"use http;
        let req = http::request::new("GET", "http://127.0.0.1:{port}/events");
        fn opened(status: int, headers: resource<http.headers>, url: string) -> bool {{
            status == 200 && url == "http://127.0.0.1:{port}/events" &&
            http::headers::values(&headers, "x-raw") == ["", "Ã©"]
        }}
        fn event(kind: string, data: string, id: string, retry: string) -> bool {{
            kind == "named" && data == "hi" && id == "7" && retry == ""
        }}
        let summary = http::client::sse(req, event, opened);
        [http::sse_summary::outcome(&summary), http::sse_summary::status(&summary),
         http::headers::values(&http::sse_summary::headers(&summary), "x-raw"),
         http::sse_summary::url(&summary), http::sse_summary::items(&summary),
         http::sse_summary::bytes_received(&summary), http::sse_summary::bytes_sent(&summary)];"#
    );
    let vm = run(&source, port).await;
    server.join().unwrap();
    let Some(Value::Array(values)) = vm.stack().last() else {
        panic!("stack: {:?}", vm.stack())
    };
    assert_eq!(values[0], Value::string("eof"));
    assert_eq!(values[1], Value::Int(200));
    assert_eq!(
        values[2],
        Value::array(vec![Value::string("\u{80}"), Value::string("\u{c3}\u{a9}")])
    );
    assert_eq!(
        values[3],
        Value::string(format!("http://127.0.0.1:{port}/events"))
    );
    assert_eq!(values[4], Value::Int(1));
    assert_eq!(values[6], Value::Int(0));
}
