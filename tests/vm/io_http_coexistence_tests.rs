//! IO and HTTP coexistence tests for the async host-adapter build.
#![cfg(all(feature = "http-client", not(target_family = "wasm")))]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::thread;
use std::time::Duration;

use vm::{
    CallReturn, HostAsyncBridge, HostFunctionRegistry, HostFuture, HostFutureOutput, HostOpId,
    HttpConfig, HttpHostExt, IoHostExt, IoPolicy, ResourceTypeKey, Value, Vm, VmError, VmResult,
    VmStatus, compile_source, register_http_builtin_module, standard_host_catalog,
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

fn bind_default_host_registry(vm: &mut Vm) {
    HostFunctionRegistry::new()
        .bind_vm_cached(vm)
        .expect("default IO and HTTP host functions should bind");
}

fn local_http_config(port: u16) -> HttpConfig {
    HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        ..HttpConfig::default()
    }
}

fn spawn_http_server(requests: usize) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("test listener should bind");
    let port = listener
        .local_addr()
        .expect("test listener should have an address")
        .port();
    let server = thread::spawn(move || {
        for _ in 0..requests {
            let (mut stream, _) = listener.accept().expect("HTTP request should arrive");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream
                    .read(&mut buffer)
                    .expect("HTTP request should be readable");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            assert!(request.starts_with(b"GET / HTTP/1.1"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-Test: yes\r\n\r\nhello")
                .expect("HTTP response should be writable");
        }
    });
    (port, server)
}

async fn drive_vm_to_halt(vm: &mut Vm) -> VmResult<()> {
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

async fn finish_reset(vm: &mut Vm) -> VmResult<()> {
    vm.reset_for_reuse()?;
    if vm.scope_reset_pending() {
        std::future::poll_fn(|cx| vm.poll_reset_for_reuse(cx)).await?;
    }
    assert!(vm.is_reusable(), "VM should be reusable after reset");
    Ok(())
}

fn response_field<'a>(value: &'a Value, key: &str) -> &'a Value {
    let Value::Map(map) = value else {
        panic!("expected response map, got {value:?}");
    };
    map.get(&Value::string(key))
        .unwrap_or_else(|| panic!("response missing field {key}"))
}

fn http_request_source(port: u16) -> String {
    format!(
        "use http; http::client::request({{\"method\": \"GET\", \"url\": \"http://127.0.0.1:{port}/\"}});"
    )
}

#[tokio::test(flavor = "current_thread")]
async fn io_and_http_both_register_via_shared_vm() {
    let compiled = compile_source(
        r#"
        use io;
        use http;
        io::exists("/");
        "#,
    )
    .expect("IO and HTTP source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.configure_io(IoPolicy {
        allowed_roots: vec!["/".to_string()],
        ..IoPolicy::default()
    });
    vm.configure_http(HttpConfig::default())
        .expect("HTTP configuration should be valid");
    install_host_driver(&mut vm);
    bind_default_host_registry(&mut vm);

    drive_vm_to_halt(&mut vm)
        .await
        .expect("IO call should complete beside registered HTTP");
    assert_eq!(vm.stack().last(), Some(&Value::Bool(true)));
}

#[tokio::test(flavor = "current_thread")]
async fn io_and_http_execute_together() {
    let (port, server) = spawn_http_server(1);
    let source = format!(
        r#"
        use io;
        use http;
        let exists = io::exists("/");
        let response = http::client::request({{"method": "GET", "url": "http://127.0.0.1:{port}/"}});
        response["status"];
        "#
    );
    let compiled = compile_source(&source).expect("combined source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.configure_io(IoPolicy {
        allowed_roots: vec!["/".to_string()],
        ..IoPolicy::default()
    });
    vm.configure_http(local_http_config(port))
        .expect("HTTP configuration should be valid");
    install_host_driver(&mut vm);
    bind_default_host_registry(&mut vm);

    drive_vm_to_halt(&mut vm)
        .await
        .expect("IO and HTTP calls should complete");
    server.join().expect("HTTP server should finish");
    assert_eq!(vm.stack().last(), Some(&Value::Int(200)));
}

#[tokio::test(flavor = "current_thread")]
async fn io_policy_persists_independently_of_http_config() {
    let compiled = compile_source(
        r#"
        use io;
        use http;
        io::exists("/forbidden");
        "#,
    )
    .expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.configure_io(IoPolicy::default());
    vm.configure_http(HttpConfig::default())
        .expect("HTTP configuration should be valid");
    install_host_driver(&mut vm);
    bind_default_host_registry(&mut vm);

    let first_error = drive_vm_to_halt(&mut vm)
        .await
        .expect_err("default IO policy should reject the path");
    assert!(first_error.to_string().contains("allowed roots"));
    finish_reset(&mut vm)
        .await
        .expect("reset should retire the failed IO invocation");
    let second_error = drive_vm_to_halt(&mut vm)
        .await
        .expect_err("IO policy should remain restrictive after reset");
    assert!(second_error.to_string().contains("allowed roots"));
}

#[tokio::test(flavor = "current_thread")]
async fn http_config_persists_independently_of_io_config() {
    let (port, server) = spawn_http_server(2);
    let compiled = compile_source(&http_request_source(port)).expect("HTTP source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.configure_io(IoPolicy {
        allowed_roots: vec!["/".to_string()],
        ..IoPolicy::default()
    });
    vm.configure_http(local_http_config(port))
        .expect("HTTP configuration should be valid");
    install_host_driver(&mut vm);
    bind_default_host_registry(&mut vm);

    drive_vm_to_halt(&mut vm)
        .await
        .expect("first HTTP request should complete");
    finish_reset(&mut vm)
        .await
        .expect("reset should retire the first HTTP invocation");
    drive_vm_to_halt(&mut vm)
        .await
        .expect("HTTP configuration should persist for the second invocation");
    server.join().expect("HTTP server should finish");
}

#[tokio::test(flavor = "current_thread")]
async fn io_and_http_coexist_through_vm_reset_cycle() {
    let compiled = compile_source(
        r#"
        use io;
        use http;
        io::exists("/");
        "#,
    )
    .expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.configure_io(IoPolicy {
        allowed_roots: vec!["/".to_string()],
        ..IoPolicy::default()
    });
    vm.configure_http(HttpConfig::default())
        .expect("HTTP configuration should be valid");
    install_host_driver(&mut vm);
    bind_default_host_registry(&mut vm);

    drive_vm_to_halt(&mut vm)
        .await
        .expect("first invocation should complete");
    finish_reset(&mut vm)
        .await
        .expect("reset should reach generic scope quiescence");
    drive_vm_to_halt(&mut vm)
        .await
        .expect("second invocation should use the replacement scope");
}

#[tokio::test(flavor = "current_thread")]
async fn worker_cleanup_reaches_quiescence_after_io_and_http() {
    let (port, server) = spawn_http_server(1);
    let source = format!(
        r#"
        use io;
        use http;
        let response = http::client::request({{"method": "GET", "url": "http://127.0.0.1:{port}/"}});
        let exists = io::exists("/");
        response["status"];
        "#
    );
    let compiled = compile_source(&source).expect("combined source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.configure_io(IoPolicy {
        allowed_roots: vec!["/".to_string()],
        ..IoPolicy::default()
    });
    vm.configure_http(local_http_config(port))
        .expect("HTTP configuration should be valid");
    install_host_driver(&mut vm);
    bind_default_host_registry(&mut vm);

    drive_vm_to_halt(&mut vm)
        .await
        .expect("combined invocation should complete");
    server.join().expect("HTTP server should finish");
    finish_reset(&mut vm)
        .await
        .expect("reset should wait for all worker and transport state");
}

#[test]
fn io_and_http_resource_type_keys_are_disjoint() {
    let catalog = standard_host_catalog();
    let io_keys = ["io.file", "io.socket", "io.process", "io.worker", "io.pipe"];
    let http_keys = ["http.request", "http.response", "http.sse"];
    for key in catalog
        .resources()
        .iter()
        .map(|resource| resource.key.as_str())
    {
        if io_keys.contains(&key) {
            assert!(!http_keys.contains(&key));
        }
        if http_keys.contains(&key) {
            assert!(!io_keys.contains(&key));
        }
    }
    for key in io_keys {
        let _ = ResourceTypeKey::new(key).expect("IO resource key should be valid");
    }
    for key in http_keys {
        let _ = ResourceTypeKey::new(key).expect("HTTP resource key should be valid");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn io_and_http_module_states_are_independent() {
    let compiled = compile_source(
        r#"
        use io;
        use http;
        true;
        "#,
    )
    .expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.configure_io(IoPolicy {
        allowed_roots: vec!["/safe".to_string()],
        max_read_bytes: 4096,
        max_write_bytes: 4096,
        ..IoPolicy::default()
    });
    vm.configure_http(HttpConfig::default())
        .expect("HTTP configuration should be valid");
    install_host_driver(&mut vm);
    bind_default_host_registry(&mut vm);
    drive_vm_to_halt(&mut vm)
        .await
        .expect("module-only invocation should complete");
    finish_reset(&mut vm)
        .await
        .expect("independent module state should permit reset");
}

#[test]
fn explicit_standard_catalog_emits_exact_http_import_schema() {
    let catalog = standard_host_catalog();
    let compiled = vm::compile_source_with_flavor_and_options(
        r#"
        use http;
        http::client::request({"method": "GET", "url": "http://127.0.0.1:1/"});
        "#,
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
    )
    .expect("catalog-backed HTTP source should compile");
    let index = compiled
        .program
        .imports
        .iter()
        .position(|import| import.name == "http::client::request")
        .expect("HTTP request should be a host import");
    let schema = compiled
        .program
        .host_import_schemas()
        .get(index)
        .and_then(Option::as_ref)
        .expect("HTTP import should carry its exact schema");
    assert_eq!(schema.fingerprint, catalog.fingerprint());

    let mut registry = HostFunctionRegistry::empty();
    register_http_builtin_module(&mut registry).expect("HTTP exact registration should succeed");
    let mut vm = Vm::new(compiled.program);
    registry
        .bind_vm_cached(&mut vm)
        .expect("catalog-backed HTTP import should exact-bind");
}

#[tokio::test(flavor = "current_thread")]
async fn combined_standard_http_exact_bind_executes_with_io_surface_present() {
    let (port, server) = spawn_http_server(1);
    let catalog = standard_host_catalog();
    let source = format!(
        r#"
        use io;
        use http;
        let response = http::client::request({{"method": "GET", "url": "http://127.0.0.1:{port}/"}});
        response;
        "#
    );
    let compiled = vm::compile_source_with_flavor_and_options(
        &source,
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
    )
    .expect("combined catalog source should compile");
    assert!(
        compiled
            .program
            .host_import_schemas()
            .iter()
            .all(Option::is_some)
    );
    let mut registry = HostFunctionRegistry::empty();
    register_http_builtin_module(&mut registry).expect("HTTP exact registration should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.configure_io(IoPolicy {
        allowed_roots: vec!["/".to_string()],
        ..IoPolicy::default()
    });
    vm.configure_http(local_http_config(port))
        .expect("HTTP configuration should be valid");
    install_host_driver(&mut vm);
    registry
        .bind_vm_cached(&mut vm)
        .expect("combined exact HTTP import should bind");
    drive_vm_to_halt(&mut vm)
        .await
        .expect("combined exact HTTP request should complete");
    server.join().expect("HTTP server should finish");
    assert_eq!(response_field(&vm.stack()[0], "status"), &Value::Int(200));
}

#[test]
fn standard_catalog_contains_io_and_http_surfaces() {
    let catalog = standard_host_catalog();
    for name in ["io::open", "http::client::request"] {
        assert!(
            !catalog.functions_named(name).is_empty(),
            "standard catalog should contain {name}"
        );
    }
}
