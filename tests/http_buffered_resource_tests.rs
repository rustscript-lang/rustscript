#![cfg(all(feature = "http-client", not(target_family = "wasm")))]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use vm::{HostFunctionRegistry, HttpConfig, HttpHostExt, Value, Vm, VmStatus, compile_source};

fn run_synchronous_http(
    source: &str,
    config: HttpConfig,
    max_in_flight: Option<usize>,
) -> (Vm, vm::VmError) {
    let program = compile_source(source)
        .expect("resource source compiles")
        .program;
    let mut vm = Vm::new(program);
    vm.configure_http(config).unwrap();
    if let Some(limit) = max_in_flight {
        vm.set_http_max_in_flight(limit);
    }
    HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();
    let error = vm.run().expect_err("synchronous rejection");
    (vm, error)
}

#[test]
fn buffered_resource_header_and_body_limits_reject_before_network() {
    let source = r#"use http;
        let mut req = http::request::new("POST", "http://127.0.0.1:1/");
        http::request::set_header(&mut req, "X-One", "first");
        http::request::set_header(&mut req, "X-Two", "second");"#;
    let config = HttpConfig {
        max_request_header_count: 1,
        ..Default::default()
    };
    let (mut vm, error) = run_synchronous_http(source, config, None);
    assert!(
        error.to_string().contains("header count exceeds limit"),
        "{error}"
    );
    assert_eq!(vm.host_context().resource_count(), 1);
    vm.reset_for_reuse().unwrap();
    assert_eq!(vm.host_context().resource_count(), 0);

    let source = r#"use http;
        let mut req = http::request::new("POST", "http://127.0.0.1:1/");
        http::request::set_body_text(&mut req, "long");"#;
    let config = HttpConfig {
        max_request_body_bytes: 3,
        ..Default::default()
    };
    let (_, error) = run_synchronous_http(source, config, None);
    assert!(error.to_string().contains("body exceeds limit"), "{error}");
}

#[test]
fn buffered_resource_admission_rejects_without_worker_or_live_builder() {
    let source = r#"use http;
        let req = http::request::new("GET", "http://127.0.0.1:1/");
        http::client::request(req);"#;
    let config = HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![1],
        allow_private_ips: true,
        ..Default::default()
    };
    let (mut vm, error) = run_synchronous_http(source, config, Some(0));
    assert!(
        error.to_string().contains("in-flight request limit"),
        "{error}"
    );
    assert_eq!(vm.host_context().resource_count(), 0);
    assert_eq!(vm.host_context().operation_count(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn buffered_resource_reset_cancels_pending_worker_and_closes_transport() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (ready, seen) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut buffer = [0; 1024];
        assert!(stream.read(&mut buffer).unwrap() > 0);
        ready.send(()).unwrap();
        assert_eq!(
            stream.read(&mut buffer).unwrap(),
            0,
            "reset closes pending request"
        );
    });
    let source = format!(
        r#"use http;
        let req = http::request::new("GET", "http://127.0.0.1:{port}/");
        http::client::request(req);"#
    );
    let mut vm = Vm::new(compile_source(&source).unwrap().program);
    vm.configure_http(HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        ..Default::default()
    })
    .unwrap();
    HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();
    assert!(matches!(vm.run(), Ok(VmStatus::Waiting(_))));
    seen.recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    vm.reset_for_reuse().unwrap();
    std::future::poll_fn(|cx| vm.poll_reset_for_reuse(cx))
        .await
        .unwrap();
    assert_eq!(vm.host_context().resource_count(), 0);
    assert_eq!(vm.host_context().operation_count(), 0);
    server.join().unwrap();
}

#[test]
fn buffered_resource_default_direct_binding_returns_live_builder() {
    let program = compile_source(
        r#"use http;
        let mut req = http::request::new("POST", "http://127.0.0.1:1/");
        http::request::set_header(&mut req, "x-test", "value");
        http::request::set_body_bytes(&mut req, b"payload");
        42;"#,
    )
    .unwrap()
    .program;
    let mut vm = Vm::new(program);
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack().last(), Some(&Value::Int(42)));
    assert_eq!(vm.host_context().resource_count(), 1);
    vm.reset_for_reuse().unwrap();
    assert_eq!(vm.host_context().resource_count(), 0);
}

#[test]
fn buffered_resource_permissions_preflight_each_import() {
    use vm::CapabilityProfile;

    let program = compile_source(
        r#"use http;
        let mut req = http::request::new("GET", "http://127.0.0.1:1/");
        http::request::set_header(&mut req, "x-test", "value");"#,
    )
    .unwrap()
    .program;
    let mut vm = Vm::new(program);
    let mut registry = HostFunctionRegistry::restricted();
    registry.set_capability_profile(
        CapabilityProfile::builder()
            .allow_host_import("http::request::new")
            .build(),
    );
    let error = registry.bind_vm_cached(&mut vm).unwrap_err();
    assert!(
        error.to_string().contains("http::request::set_header"),
        "{error}"
    );
    assert_eq!(vm.host_context().resource_count(), 0);
}

#[test]
fn buffered_resource_signatures_reject_maps_wrong_keys_and_reuse() {
    for source in [
        "use http; http::client::request({method: \"GET\", url: \"http://example.com\"});",
        "use http; let mut req = http::request::new(\"GET\", \"http://example.com\"); http::request::set_header(req, \"x\", \"y\");",
        "use http; let req = http::request::new(\"GET\", \"http://example.com\"); http::response::status(&req);",
        "use http; let req = http::request::new(\"GET\", \"http://example.com\"); http::client::request(req); http::client::request(req);",
    ] {
        assert!(compile_source(source).is_err(), "must reject: {source}");
    }
}

#[test]
fn buffered_resource_request_reads_status_body_and_repeated_headers() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut chunk = [0; 1024];
        loop {
            let read = stream.read(&mut chunk).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        assert!(String::from_utf8_lossy(&request).contains("x-test: one"));
        stream.write_all(b"HTTP/1.1 201 Created\r\nX-Repeat: first\r\nX-Repeat: second\r\nContent-Length: 2\r\n\r\nok").unwrap();
    });
    let source = format!(
        r#"
        use http;
        let mut req = http::request::new("GET", "http://127.0.0.1:{port}/");
        http::request::set_header(&mut req, "x-test", "one");
        let response = http::client::request(req);
        let status = http::response::status(&response);
        let body = http::response::body(&response);
        let values = http::response::header_values(&response, "X-Repeat");
        [status, body, values];
    "#
    );
    let program = compile_source(&source)
        .expect("buffered resource program compiles")
        .program;
    let mut vm = Vm::new(program);
    vm.configure_http(HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        ..Default::default()
    })
    .unwrap();
    HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();
    let mut state = vm.run().expect("run request");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    loop {
        state = match state {
            VmStatus::Halted => break,
            VmStatus::Yielded => vm.resume().unwrap(),
            VmStatus::Waiting(_) => {
                runtime
                    .block_on(vm.await_waiting_host_op())
                    .expect("await worker");
                vm.resume().expect("resume worker")
            }
        };
    }
    server.join().unwrap();
    assert_eq!(
        vm.stack().last(),
        Some(&Value::array(vec![
            Value::Int(201),
            Value::bytes(b"ok".to_vec()),
            Value::array(vec![Value::string("first"), Value::string("second")])
        ])),
        "stack: {:?}",
        vm.stack()
    );
    assert_eq!(vm.host_context().resource_count(), 1);
    vm.reset_for_reuse().unwrap();
    assert_eq!(vm.host_context().resource_count(), 0);
}

#[test]
fn buffered_resource_header_values_recover_raw_response_octets() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut chunk = [0; 1024];
        loop {
            let read = stream.read(&mut chunk).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nX-Raw: \x80\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
    });
    let source = format!(
        r#"use http;
        let req = http::request::new("GET", "http://127.0.0.1:{port}/");
        let response = http::client::request(req);
        http::response::header_values(&response, "X-Raw");"#
    );
    let mut vm = Vm::new(compile_source(&source).unwrap().program);
    vm.configure_http(HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        ..Default::default()
    })
    .unwrap();
    HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();
    let mut state = vm.run().expect("run request");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    loop {
        state = match state {
            VmStatus::Halted => break,
            VmStatus::Yielded => vm.resume().unwrap(),
            VmStatus::Waiting(_) => {
                runtime
                    .block_on(vm.await_waiting_host_op())
                    .expect("await worker");
                vm.resume().expect("resume worker")
            }
        };
    }
    server.join().unwrap();
    let Some(Value::Array(values)) = vm.stack().last() else {
        panic!("expected header values: {:?}", vm.stack());
    };
    let [Value::String(value)] = values.as_slice() else {
        panic!("expected one string header value: {values:?}");
    };
    assert!(!value.contains('\u{fffd}'));
    let octets: Vec<u8> = value
        .chars()
        .map(|scalar| u8::try_from(u32::from(scalar)).expect("one scalar per raw byte"))
        .collect();
    assert_eq!(octets, [0x80]);
}
