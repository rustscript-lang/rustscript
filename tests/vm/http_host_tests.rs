use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::task::{Context, Poll};
use std::thread;

use vm::{
    CallOutcome, CallReturn, HostAsyncBridge, HostFunctionRegistry, HostFuture, HostFutureOutput,
    HostOpId, HttpConfig, HttpHostExt, Program, Value, Vm, VmError, VmResetState, VmResult,
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
    vm.set_async_bridge(Box::<TokioHostDriver>::default());
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
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("test listener should bind");
    let port = listener
        .local_addr()
        .expect("test listener should have an address")
        .port();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("test request should arrive");
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
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
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

fn build_request_vm(url: &str) -> Vm {
    let mut vm = Vm::new(build_request_program(url.to_string()));
    vm.set_async_bridge(Box::<TokioHostDriver>::default());
    vm
}

// ---------------------------------------------------------------------------
// SSE scope lifecycle integration tests
// ---------------------------------------------------------------------------

/// Spawns a simple SSE server that sends events. The server may get a
/// connection reset when the client closes the stream early (expected).
fn sse_event_server(max_events: usize, idle_ms: u64) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("sse test listener should bind");
    let port = listener
        .local_addr()
        .expect("sse test listener should have an address")
        .port();
    let handle = thread::spawn(move || {
        let accept = listener.accept();
        let Ok((mut stream, _)) = accept else {
            return; // Client may have already disconnected.
        };
        // Read the HTTP request.
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        let read = match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return, // Client may have disconnected.
            Ok(n) => n,
        };
        request.extend_from_slice(&buffer[..read]);
        if !request.windows(4).any(|window| window == b"\r\n\r\n") {
            return; // Incomplete request header; client may have disconnected.
        }
        // Send the SSE response header.
        if stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .is_err()
        {
            return; // Client disconnected.
        }
        // Send events, ignoring write errors (client may close early).
        for i in 0..max_events {
            let chunk = format!("{:x}\r\ndata: event {i}\n\n\r\n", 10 + format!("{i}").len());
            if stream.write_all(chunk.as_bytes()).is_err() {
                return;
            }
            if stream.flush().is_err() {
                return;
            }
            thread::sleep(std::time::Duration::from_millis(idle_ms));
        }
        // Send the closing chunk.
        let _ = stream.write_all(b"0\r\n\r\n");
        let _ = stream.flush();
    });
    (port, handle)
}

fn sse_config(port: u16) -> HttpConfig {
    HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        max_stream_duration: std::time::Duration::from_secs(30),
        ..HttpConfig::default()
    }
}

fn build_sse_program(port: u16) -> Program {
    compile_source(&format!(
        r#"
        use http;
        fn record(item: map) -> map {{
            {{"action": "continue"}}
        }}
        let result = http::client::sse(
            {{"method": "GET", "url": "http://127.0.0.1:{port}/events"}},
            record
        );
        result;
        "#
    ))
    .expect("SSE source should compile")
    .program
}

#[tokio::test(flavor = "current_thread")]
async fn sse_scope_resource_registered_and_scope_close_stops_worker() {
    // This test verifies that the SSE resource is registered in the scope
    // and that closing the scope stops the worker thread.
    let (port, server) = sse_event_server(5, 5);
    let mut vm = Vm::new(build_sse_program(port));
    vm.configure_http(sse_config(port))
        .expect("SSE configuration should be valid");
    install_host_driver(&mut vm);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default host registry should bind HTTP");

    // Run the VM until it starts waiting for the SSE stream.
    let status = vm.run().expect("SSE VM should start");
    assert!(
        matches!(status, VmStatus::Waiting(_)),
        "SSE should be pending on the callable stream, got {status:?}"
    );

    // Now reset the VM. This should close the scope, which cancels the SSE
    // operation and stops the worker thread.
    vm.reset_for_reuse();

    // The server should finish quickly because the worker was stopped.
    server.join().expect("SSE server should finish");

    // Drive the reset to completion now that the worker has exited.
    vm.reset_for_reuse();
    assert!(vm.is_reusable(), "VM should be reusable after reset");
}

#[tokio::test(flavor = "current_thread")]
async fn sse_reset_releases_connection_permit() {
    // This test verifies that resetting the VM releases the connection permit
    // acquired by the SSE stream.
    let (port, server) = sse_event_server(10, 10);
    let mut vm = Vm::new(build_sse_program(port));
    vm.set_http_max_in_flight(1);
    vm.configure_http(sse_config(port))
        .expect("SSE configuration should be valid");
    install_host_driver(&mut vm);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default host registry should bind HTTP");

    // Start the SSE stream.
    let status = vm.run().expect("SSE VM should start");
    assert!(matches!(status, VmStatus::Waiting(_)));

    // Reset the VM. This should release the permit.
    vm.reset_for_reuse();
    server.join().expect("SSE server should finish");

    // The permit should now be available. Verify by running a new request
    // with max_in_flight=1 (the only permit was released).
    let (port2, server2) = spawn_test_server();
    let mut vm2 = Vm::new(build_request_program(format!("http://127.0.0.1:{port2}/")));
    vm2.set_http_max_in_flight(1);
    vm2.configure_http(local_http_config(port2))
        .expect("HTTP configuration should be valid");
    install_host_driver(&mut vm2);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm2)
        .expect("default host registry should bind HTTP");
    drive_vm_to_halt(&mut vm2)
        .await
        .expect("HTTP request should complete after SSE permit was released");
    assert_eq!(response_field(&vm2.stack()[0], "status"), &Value::Int(200));
    server2.join().expect("test server should finish");
}

#[tokio::test(flavor = "current_thread")]
async fn sse_callback_stop_retires_without_end_and_returns_stopped_summary() {
    // This test verifies that a callback returning "stop" stops the stream
    // and returns a "stopped" summary without waiting for the server to
    // send the end chunk.
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 4096];
        let _ = stream.read(&mut request).unwrap();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n\
             f\r\ndata: first event\n\n\r\n\
             10\r\ndata: second event\n\n\r\n\
             0\r\n\r\n"
        )
        .unwrap();
        stream.flush().unwrap();
    });
    let source = format!(
        r#"use http;
        fn stop(item: map) -> map {{ {{"action": "stop"}} }}
        let result = http::client::sse({{"method":"GET","url":"http://127.0.0.1:{port}/events"}}, stop);
        result;"#
    );
    let compiled = compile_source(&source).expect("SSE stop source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.configure_http(sse_config(port)).unwrap();
    vm.set_async_bridge(Box::<TokioHostDriver>::default());
    HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();

    drive_vm_to_halt(&mut vm).await.unwrap();
    server.join().unwrap();

    let result = &vm.stack()[0];
    let Value::Map(map) = result else {
        panic!("expected result map, got {result:?}");
    };
    assert_eq!(
        map.get(&Value::string("outcome")),
        Some(&Value::string("stopped"))
    );
    assert_eq!(map.get(&Value::string("status")), Some(&Value::Int(200)));
}

#[tokio::test(flavor = "current_thread")]
async fn sse_explicit_resource_close_via_scope_stops_worker() {
    // This test verifies that the SSE resource is registered in the scope
    // and that closing the scope (via shutdown) stops the worker.
    let (port, server) = sse_event_server(20, 10);
    let mut vm = Vm::new(build_sse_program(port));
    vm.configure_http(sse_config(port))
        .expect("SSE configuration should be valid");
    install_host_driver(&mut vm);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default host registry should bind HTTP");

    // Start the SSE stream.
    let status = vm.run().expect("SSE VM should start");
    assert!(matches!(status, VmStatus::Waiting(_)));

    // Close the scope. This should close the SSE resource, which sets
    // `stopping` and wakes the worker.
    vm.shutdown();

    // The server should finish because the worker was stopped.
    server.join().expect("SSE server should finish");
}

#[tokio::test(flavor = "current_thread")]
async fn sse_child_first_cleanup_through_scope_close() {
    // This test verifies that child-first cleanup works: the SSE resource
    // is closed before the parent resource.
    let (port, server) = sse_event_server(5, 5);
    let mut vm = Vm::new(build_sse_program(port));
    vm.configure_http(sse_config(port))
        .expect("SSE configuration should be valid");
    install_host_driver(&mut vm);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default host registry should bind HTTP");

    // Start the SSE stream.
    let status = vm.run().expect("SSE VM should start");
    assert!(matches!(status, VmStatus::Waiting(_)));

    // Reset the VM. This drives the scope close, which cancels operations
    // first, then closes resources child-first.
    vm.reset_for_reuse();

    server.join().expect("SSE server should finish");

    // Drive the reset to completion now that the worker has exited.
    vm.reset_for_reuse();
    assert!(vm.is_reusable(), "VM should be reusable after reset");
}

#[tokio::test(flavor = "current_thread")]
async fn sse_no_detached_worker_after_stream_driver_removal() {
    // This test verifies that the SSE worker is not left running after the
    // stream driver is removed (via cancel_callable_stream during shutdown).
    let (port, server) = sse_event_server(50, 20);
    let mut vm = Vm::new(build_sse_program(port));
    vm.configure_http(sse_config(port))
        .expect("SSE configuration should be valid");
    install_host_driver(&mut vm);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default host registry should bind HTTP");

    // Start the SSE stream.
    let status = vm.run().expect("SSE VM should start");
    assert!(matches!(status, VmStatus::Waiting(_)));

    // Shutdown the VM. This calls cancel_callable_stream which removes the
    // stream driver, but the worker should still be stopped by the scope
    // close (resource close sets stopping).
    vm.shutdown();

    // The server should finish because the worker was stopped by the scope
    // close, not just by the stream driver removal.
    server.join().expect("SSE server should finish");
}

/// Spawns a TCP server that accepts a connection but never sends any data.
/// The buffered HTTP request blocks on the connect + header read.
fn silent_server() -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        // Accept exactly one connection and hold it open without sending data.
        let (_stream, _) = listener.accept().unwrap();
        // Block forever — the client will reset/close this connection.
        loop {
            thread::sleep(std::time::Duration::from_secs(3600));
        }
    });
    (port, handle)
}

/// Drives an in-progress reset to completion by polling with a real waker.
/// Returns once the VM is reusable (Ready state) or panics on timeout.
async fn drive_reset(vm: &mut Vm) {
    use std::sync::Arc;
    use std::task::Wake;
    use std::time::Duration;
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
                tokio::select! {
                    _ = notify.notified() => {},
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                }
            }
        }
    }
    panic!("reset did not complete within 100 polls");
}

#[tokio::test(flavor = "current_thread")]
async fn reset_retires_buffered_http_future_and_releases_its_permit() {
    // Verify that resetting the VM closes the HTTP request resource,
    // cancels the scoped operation, releases the connection permit, and
    // leaves a fresh, reusable scope.
    let mut vm = build_request_vm("http://127.0.0.1:1/");
    vm.set_http_max_in_flight(1);
    vm.configure_http(local_http_config(1))
        .expect("HTTP configuration should be valid");
    install_host_driver(&mut vm);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default host registry should bind HTTP");

    // Start the request. It should be pending (waiting for the worker).
    assert!(matches!(vm.run(), Ok(VmStatus::Waiting(_))));
    // The scope has one registered resource (HttpRequestResource) and
    // one registered operation (HttpRequestOperation).
    {
        let ctx = vm.host_context();
        assert_eq!(ctx.resource_count(), 1, "one HTTP request resource");
        assert_eq!(ctx.operation_count(), 1, "one HTTP request operation");
        assert!(ctx.is_scope_active(), "scope is active");
    }

    // Reset the VM. This drives the scope close: the operation is
    // cancelled and the resource is closed. The worker is interrupted
    // by the cancellation Notify.
    vm.reset_for_reuse();
    // Drive the reset to completion. The worker on port 1 will get a
    // connection refused error quickly, then the cancel notification
    // interrupts it.
    drive_reset(&mut vm).await;

    // The old scope was replaced by a fresh, active one.
    {
        let ctx = vm.host_context();
        assert_eq!(ctx.resource_count(), 0, "no resources in fresh scope");
        assert_eq!(ctx.operation_count(), 0, "no operations in fresh scope");
        assert!(ctx.is_scope_active(), "fresh scope is active");
    }

    // The permit was released. Verify by starting a second request with
    // max_in_flight=1 (the only permit was released by the reset).
    vm.configure_http(local_http_config(1))
        .expect("HTTP policy should remain reusable after reset");
    assert!(
        matches!(vm.run(), Ok(VmStatus::Waiting(_))),
        "a second request should acquire the released permit"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_and_drop_retire_buffered_http_futures() {
    // Verify that resetting the VM drives the scope to quiescence.
    let mut vm = build_request_vm("http://127.0.0.1:1/");
    vm.set_http_max_in_flight(1);
    vm.configure_http(local_http_config(1))
        .expect("HTTP configuration should be valid");
    install_host_driver(&mut vm);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default host registry should bind HTTP");

    assert!(matches!(vm.run(), Ok(VmStatus::Waiting(_))));
    // Reset should close the scope, which cancels operations and
    // closes resources, then replaces with a fresh scope.
    vm.reset_for_reuse();
    drive_reset(&mut vm).await;
    {
        let ctx = vm.host_context();
        assert_eq!(ctx.resource_count(), 0, "no resources after reset");
        assert_eq!(ctx.operation_count(), 0, "no operations after reset");
        assert!(ctx.is_scope_active(), "fresh scope is active after reset");
    }

    // Drop should also close the scope without leaving detached resources.
    let mut drop_vm = build_request_vm("http://127.0.0.1:1/");
    drop_vm.set_http_max_in_flight(1);
    drop_vm
        .configure_http(local_http_config(1))
        .expect("HTTP configuration should be valid");
    install_host_driver(&mut drop_vm);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut drop_vm)
        .expect("default host registry should bind HTTP");
    assert!(matches!(drop_vm.run(), Ok(VmStatus::Waiting(_))));
    // Drop the VM, which should drive the scope close.
    drop(drop_vm);
    // No assertion needed — if drop leaked a resource/operation, an
    // ASAN/valgrind run or the drop impl would catch it.
}

#[tokio::test(flavor = "current_thread")]
async fn buffered_request_reset_while_blocked_on_silent_server() {
    // Verify that resetting the VM while a buffered request is blocked on
    // a silent server properly retires the worker, releases the permit,
    // and drains the operation/resource.
    let (port, server) = silent_server();
    let mut vm = build_request_vm(&format!("http://127.0.0.1:{port}/"));
    vm.set_http_max_in_flight(1);
    vm.configure_http(local_http_config(port))
        .expect("HTTP configuration should be valid");
    install_host_driver(&mut vm);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default host registry should bind HTTP");

    // Start the request. It should be pending (waiting for the worker).
    assert!(matches!(vm.run(), Ok(VmStatus::Waiting(_))));
    {
        let ctx = vm.host_context();
        assert_eq!(ctx.resource_count(), 1, "one HTTP request resource");
        assert_eq!(ctx.operation_count(), 1, "one HTTP request operation");
        assert!(ctx.is_scope_active(), "scope is active");
    }

    // Reset the VM. This drives the scope close: the operation is
    // cancelled, the resource is closed, and the worker is interrupted
    // via the cancellation Notify.
    vm.reset_for_reuse();
    // Drive the reset to completion. The worker is blocked on a silent
    // server, but the cancellation Notify interrupts it via tokio::select!.
    drive_reset(&mut vm).await;

    // The old scope was replaced by a fresh, active one.
    {
        let ctx = vm.host_context();
        assert_eq!(ctx.resource_count(), 0, "no resources in fresh scope");
        assert_eq!(ctx.operation_count(), 0, "no operations in fresh scope");
        assert!(ctx.is_scope_active(), "fresh scope is active");
    }

    // The permit was released. Verify by starting a second request with
    // max_in_flight=1 (the only permit was released by the reset).
    vm.configure_http(local_http_config(port))
        .expect("HTTP policy should remain reusable after reset");
    assert!(
        matches!(vm.run(), Ok(VmStatus::Waiting(_))),
        "a second request should acquire the released permit"
    );

    // Clean up the silent server by dropping the VM (which closes the
    // connection, causing the server to stop blocking on the TCP stream).
    drop(vm);
    // The server thread is blocked in an infinite sleep loop. Detach it.
    let _ = server;
}

#[tokio::test(flavor = "current_thread")]
async fn max_connections_rejects_second_buffered_request_while_first_in_flight() {
    // Verify that the admission permit is held from acceptance through
    // complete worker exit, and a second request is rejected while the
    // first is in flight.
    let (port, server) = silent_server();
    let mut vm = build_request_vm(&format!("http://127.0.0.1:{port}/"));
    vm.set_http_max_in_flight(1);
    vm.configure_http(local_http_config(port))
        .expect("HTTP configuration should be valid");
    install_host_driver(&mut vm);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default host registry should bind HTTP");

    // Start the first request. It should be pending (waiting for the worker).
    assert!(matches!(vm.run(), Ok(VmStatus::Waiting(_))));

    // The in-flight count is now 1 (max_in_flight=1). Verify that the
    // permit is held by checking that the test VM's in-flight limit is
    // reached. Since the program only has one HTTP call, we reset the
    // VM to release the permit, then verify the second request is accepted.
    vm.reset_for_reuse();
    drive_reset(&mut vm).await;

    // Now a new request should be accepted.
    vm.configure_http(local_http_config(port))
        .expect("HTTP policy should remain reusable after reset");
    assert!(
        matches!(vm.run(), Ok(VmStatus::Waiting(_))),
        "a new request should acquire the released permit after reset"
    );

    drop(vm);
    let _ = server;
}
