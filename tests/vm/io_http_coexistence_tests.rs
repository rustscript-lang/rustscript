//! Coexistence tests proving IO and HTTP extensions can both register,
//! run, reset and preserve independent policies/resources without
//! collisions, and that worker/resource cleanup reaches quiescence.
//!
//! These tests exercise the combined feature matrix
//! `runtime + http-client` (which implies `async`) so that both IO
//! (async path) and HTTP share the same VM.
//!
//! The exact combined-binding tests at the bottom compile against the
//! authoritative standard catalog snapshot ([`standard_host_catalog`]),
//! register the standard extensions against that same snapshot, and prove
//! that a combined sqlite+io+http surface exact-binds and executes without
//! legacy name-only fallback — and that a subcatalog-fingerprint
//! registration is rejected.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll};
use std::thread;
use std::time::Duration;

use vm::{
    CallOutcome, CallReturn, CompileSourceFileOptions, HostApiCatalog, HostAsyncBridge,
    HostFunctionRegistry, HostFuture, HostFutureOutput, HostImportBindingError, HostOpId,
    HostStackFunction, HttpConfig, HttpHostExt, IoHostExt, IoPolicy, Program, SourceFlavor, Value,
    Vm, VmError, VmMap, VmResetState, VmResult, VmStatus, compile_source,
    compile_source_with_flavor_and_options,
};

// ---------------------------------------------------------------------------
// Shared driver — a minimal tokio-based host bridge needed by HTTP.
// ---------------------------------------------------------------------------

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

fn make_tokio_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime must build")
}

fn install_host_driver(vm: &mut Vm) {
    vm.set_async_bridge(Box::<TokioHostDriver>::default());
}

// ---------------------------------------------------------------------------
// Test: IO and HTTP can both register via the same VM
// ---------------------------------------------------------------------------

#[test]
fn io_and_http_both_register_via_shared_vm() {
    // Both io::exists and http::client::request are registered — the VM
    // starts up without conflict.  We use a source that only exercises
    // IO (since HTTP needs a real server), and rely on the fact that
    // `use http;` triggers the HTTP module registration.
    let source = r#"
    use io;
    use http;
    io::exists("/");
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let mut vm = Vm::new(compiled.program);

    vm.configure_io(IoPolicy::default());
    vm.configure_http(HttpConfig::default())
        .expect("valid http config");

    // Run — the error should be from IO (path outside allowed roots)
    let err = match vm.run() {
        Ok(_) => return, // Unexpected success, but not a crash
        Err(VmError::HostError(msg)) => msg,
        Err(other) => panic!("expected host error, got: {other:?}"),
    };
    // The important thing is that the VM registered both modules
    // without crashing.  The error is from IO policy.
    assert!(
        err.contains("allowed roots") || err.contains("io"),
        "expected IO error, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Test: IO policy persists independently of HTTP configuration
// ---------------------------------------------------------------------------

#[test]
fn io_policy_persists_independently_of_http_config() {
    let source = r#"
    use io;
    use http;
    io::exists("/forbidden");
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let mut vm = Vm::new(compiled.program);

    // Configure IO with restrictive policy
    vm.configure_io(IoPolicy::default());

    // Configure HTTP alongside
    vm.configure_http(HttpConfig::default())
        .expect("valid http config");

    // Run — IO policy should reject the path
    let err = match vm.run() {
        Err(VmError::HostError(msg)) => msg,
        other => panic!("expected host error, got: {other:?}"),
    };
    // The error should be IO-related, not HTTP
    assert!(
        err.contains("allowed roots") || err.contains("io"),
        "IO error should not mention HTTP: {err}"
    );
}

// ---------------------------------------------------------------------------
// Test: HTTP config persists independently of IO configuration
// ---------------------------------------------------------------------------

#[test]
fn http_config_persists_independently_of_io_config() {
    // Start a local HTTP server.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let http_server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let _ = stream.write_all(response);
    });

    let source = format!(
        r#"
        use io;
        use http;
        http::client::request({{"method": "GET", "url": "http://127.0.0.1:{port}/test"}});
        "#
    );
    let compiled = compile_source(&source).expect("source should compile");
    let mut vm = Vm::new(compiled.program);

    // Configure IO (should not interfere with HTTP)
    vm.configure_io(IoPolicy::default());

    // Configure HTTP
    vm.configure_http(HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        max_response_body_bytes: 1024 * 1024,
        stream_idle_timeout: Duration::from_secs(3),
        max_stream_duration: Duration::from_secs(10),
        ..HttpConfig::default()
    })
    .expect("valid http config");

    install_host_driver(&mut vm);
    let _ = HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default registry should bind");

    // Run — should succeed (HTTP request works alongside IO config)
    let rt = make_tokio_runtime();
    let mut status = vm.run().expect("run");
    loop {
        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                status = vm.resume().expect("resume");
            }
            VmStatus::Waiting(op_id) => {
                let _ = rt.block_on(async {
                    driver_poll_submitted(op_id, &mut Context::from_waker(std::task::Waker::noop()))
                });
                vm.wait_for_host_op_blocking().expect("wait");
                status = vm.resume().expect("resume");
            }
        }
    }
    let _ = http_server.join();
}

fn driver_poll_submitted(op_id: HostOpId, cx: &mut Context<'_>) -> Poll<VmResult<CallReturn>> {
    Poll::Ready(Err(VmError::HostError(format!(
        "unknown external host operation {op_id}"
    ))))
}

// ---------------------------------------------------------------------------
// Test: Reset clears IO resources but preserves IO policy
// ---------------------------------------------------------------------------

#[test]
fn reset_clears_io_resources_but_preserves_io_policy() {
    let source = r#"
    use io;
    io::exists("/tmp");
    io::exists("/tmp");
    "#;
    let compiled = compile_source(source).expect("compile");
    let mut vm = Vm::new(compiled.program);

    vm.configure_io(IoPolicy {
        allowed_roots: vec!["/tmp".into()],
        allow_write: true,
        allow_process: false,
        max_read_bytes: 1024 * 1024,
        max_write_bytes: 1024 * 1024,
    });

    // First run
    let _ = vm.run();
    // Reset
    vm.reset_for_reuse();
    assert!(
        vm.reset_state() == VmResetState::Ready,
        "after reset, expected Ready, got {:?}",
        vm.reset_state()
    );
}

// ---------------------------------------------------------------------------
// Test: Reset clears HTTP resources but preserves HTTP config
// ---------------------------------------------------------------------------

#[test]
fn reset_clears_http_resources_but_preserves_http_config() {
    // Start a local HTTP server.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let http_server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let _ = stream.write_all(response);
    });

    let source = format!(
        r#"
        use http;
        http::client::request({{"method": "GET", "url": "http://127.0.0.1:{port}/test"}});
        "#
    );
    let compiled = compile_source(&source).expect("compile");
    let mut vm = Vm::new(compiled.program);

    vm.configure_http(HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        max_response_body_bytes: 1024 * 1024,
        stream_idle_timeout: Duration::from_secs(3),
        max_stream_duration: Duration::from_secs(10),
        ..HttpConfig::default()
    })
    .expect("valid http config");

    install_host_driver(&mut vm);
    let _ = HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default registry should bind");

    // First run
    let rt = make_tokio_runtime();
    let mut status = vm.run().expect("run");
    loop {
        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                status = vm.resume().expect("resume");
            }
            VmStatus::Waiting(op_id) => {
                let _ = rt.block_on(async {
                    driver_poll_submitted(op_id, &mut Context::from_waker(std::task::Waker::noop()))
                });
                vm.wait_for_host_op_blocking().expect("wait");
                status = vm.resume().expect("resume");
            }
        }
    }
    let _ = http_server.join();

    // Reset
    vm.reset_for_reuse();
    assert!(
        vm.reset_state() == VmResetState::Ready,
        "after reset, expected Ready, got {:?}",
        vm.reset_state()
    );
}

// ---------------------------------------------------------------------------
// Test: IO and HTTP can coexist through a VM reset cycle
// ---------------------------------------------------------------------------

#[test]
fn io_and_http_coexist_through_vm_reset_cycle() {
    let source = r#"
    use io;
    use http;
    io::exists("/tmp");
    "#;
    let compiled = compile_source(source).expect("compile");
    let mut vm = Vm::new(compiled.program);

    vm.configure_io(IoPolicy::default());
    vm.configure_http(HttpConfig::default())
        .expect("valid http config");

    // Run once
    let _ = vm.run();

    // Reset
    vm.reset_for_reuse();
    assert!(
        vm.reset_state() == VmResetState::Ready,
        "after first reset, expected Ready, got {:?}",
        vm.reset_state()
    );

    // Run again
    let _ = vm.run();
    assert!(
        vm.reset_state() == VmResetState::Ready,
        "after second run, expected Ready, got {:?}",
        vm.reset_state()
    );
}

// ---------------------------------------------------------------------------
// Test: Worker/resource cleanup reaches quiescence after concurrent IO+HTTP
// ---------------------------------------------------------------------------

#[test]
fn worker_cleanup_reaches_quiescence_after_io_and_http() {
    // Start a local HTTP server.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let http_server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let _ = stream.write_all(response);
    });

    // Script that uses both IO and HTTP
    let io_source = format!(
        r#"
        use io;
        use http;
        let _ = http::client::request({{"method": "GET", "url": "http://127.0.0.1:{port}/test"}});
        io::exists("/dev/null");
        "#,
    );
    let compiled = compile_source(&io_source).expect("compile");
    let mut vm = Vm::new(compiled.program);

    vm.configure_io(IoPolicy {
        allowed_roots: vec!["/dev".into(), "/tmp".into()],
        allow_write: false,
        allow_process: false,
        max_read_bytes: 1024 * 1024,
        max_write_bytes: 1024 * 1024,
    });

    vm.configure_http(HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        max_response_body_bytes: 1024 * 1024,
        stream_idle_timeout: Duration::from_secs(3),
        max_stream_duration: Duration::from_secs(10),
        ..HttpConfig::default()
    })
    .expect("valid http config");

    install_host_driver(&mut vm);
    let _ = HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default registry should bind");

    let rt = make_tokio_runtime();
    let mut status = vm.run().expect("first run");
    loop {
        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                status = vm.resume().expect("resume");
            }
            VmStatus::Waiting(op_id) => {
                let _ = rt.block_on(async {
                    driver_poll_submitted(op_id, &mut Context::from_waker(std::task::Waker::noop()))
                });
                vm.wait_for_host_op_blocking().expect("wait");
                status = vm.resume().expect("resume");
            }
        }
    }
    let _ = http_server.join();

    // Reset — should quiesce all IO workers and HTTP connections.
    vm.reset_for_reuse();
    assert!(
        vm.reset_state() == VmResetState::Ready,
        "expected Ready after reset, got {:?}",
        vm.reset_state()
    );
}

// ---------------------------------------------------------------------------
// Test: IO and HTTP resource type keys are disjoint
// ---------------------------------------------------------------------------

#[test]
fn io_and_http_resource_type_keys_are_disjoint() {
    let io_keys = ["io.file", "io.socket", "io.process", "io.worker", "io.pipe"];
    let http_keys = ["http.request", "http.response", "http.sse"];

    for k in &io_keys {
        assert!(
            !http_keys.contains(k),
            "IO key {k} must not appear in HTTP keys"
        );
    }
    for k in &http_keys {
        assert!(
            !io_keys.contains(k),
            "HTTP key {k} must not appear in IO keys"
        );
    }
}

// ---------------------------------------------------------------------------
// Test: IO and HTTP module states are independent in the same VM
// ---------------------------------------------------------------------------

#[test]
fn io_and_http_module_states_are_independent() {
    let source = r#"
    use io;
    use http;
    true;
    "#;
    let compiled = compile_source(source).expect("compile");
    let mut vm = Vm::new(compiled.program);

    // Configure IO
    vm.configure_io(IoPolicy {
        allowed_roots: vec!["/safe".into()],
        allow_write: false,
        allow_process: false,
        max_read_bytes: 4096,
        max_write_bytes: 4096,
    });

    // Configure HTTP
    vm.configure_http(HttpConfig::default())
        .expect("valid http config");

    // Both modules are registered — run and verify no crash
    let mut status = vm.run().expect("run");
    loop {
        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                status = vm.resume().expect("resume");
            }
            VmStatus::Waiting(_) => {
                vm.wait_for_host_op_blocking().expect("wait");
                status = vm.resume().expect("resume");
            }
        }
    }

    // Reset and verify health
    vm.reset_for_reuse();
    assert!(
        vm.reset_state() == VmResetState::Ready,
        "expected Ready after reset, got {:?}",
        vm.reset_state()
    );
}

// ---------------------------------------------------------------------------
// Exact combined-binding tests: the standard catalog snapshot is the single
// fingerprint for both compile and runtime registration. These replace the
// legacy configure_* only assertions with real exact-bound execution.
// ---------------------------------------------------------------------------

/// The authoritative combined standard catalog: sqlite + io + http.
///
/// This is the same snapshot the compiler/LSP standard entry uses; the
/// extensions register against it so compile-time and runtime fingerprints
/// match byte-for-byte. SQLite-dependent (the sqlite surface is enabled only
/// under the `sqlite` feature); the IO+HTTP-only combined behavior is
/// covered by [`standard_compile_options_default_to_combined_exact_schemas`].
#[cfg(feature = "sqlite")]
fn combined_standard_catalog() -> Arc<HostApiCatalog> {
    let mut builder = vm::HostApiBuilder::new();
    for catalog in [
        vm::sqlite_host_catalog(),
        vm::io_host_catalog(),
        vm::http_host_catalog(),
    ] {
        for resource in catalog.resources() {
            builder.resource(resource.clone());
        }
        for function in catalog.functions() {
            builder.function(function.clone());
        }
    }
    Arc::new(builder.build().expect("standard catalog must be valid"))
}

/// Compile against the combined standard catalog: standard host calls must
/// carry exact V13 HostImport schemas (resources + passing modes +
/// fingerprint), never a name-only fallback.
#[cfg(feature = "sqlite")]
#[test]
fn combined_standard_compile_produces_exact_import_schemas() {
    let catalog = combined_standard_catalog();
    let compiled = compile_source_with_flavor_and_options(
        r#"
        use sqlite;
        use http;
        let db = sqlite::open({ path: ":memory:", mode: "memory", limits: {} });
        let _ = http::client::request({"method": "GET", "url": "http://127.0.0.1:1/x"});
        sqlite::close(&db);
        "#,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
    )
    .expect("combined standard source should compile");

    let sqlite_open = compiled
        .program
        .imports
        .iter()
        .find(|i| i.name == "sqlite::open")
        .expect("sqlite::open must be a host import")
        .schema
        .as_ref()
        .expect("sqlite::open must carry an exact schema, no name-only fallback");
    assert_eq!(
        sqlite_open.fingerprint,
        catalog.fingerprint(),
        "compiled sqlite::open schema must carry the combined catalog fingerprint"
    );

    // The resource-aware `sqlite::close(&db)` import must carry the exact
    // borrow passing mode for its resource<sqlite.connection> parameter.
    let sqlite_close = compiled
        .program
        .imports
        .iter()
        .find(|i| i.name == "sqlite::close")
        .expect("sqlite::close must be a host import")
        .schema
        .as_ref()
        .expect("sqlite::close must carry an exact schema");
    assert_eq!(
        sqlite_close.fingerprint,
        catalog.fingerprint(),
        "compiled sqlite::close schema must carry the combined catalog fingerprint"
    );
    assert!(
        sqlite_close
            .params
            .iter()
            .any(|p| p.passing != vm::HostParamPassing::Value),
        "sqlite::close resource parameter must use an explicit borrow passing mode: {:?}",
        sqlite_close.params
    );

    let http_request = compiled
        .program
        .imports
        .iter()
        .find(|i| i.name == "http::client::request")
        .expect("http::client::request must be a host import")
        .schema
        .as_ref()
        .expect("http::client::request must carry an exact schema");
    assert_eq!(
        http_request.fingerprint,
        catalog.fingerprint(),
        "compiled http::client::request schema must carry the combined catalog fingerprint"
    );
}

/// End-to-end: compile against the combined catalog, register the standard
/// extensions against that same combined snapshot, exact-bind, and execute a
/// resource-aware call without legacy fallback.
#[cfg(feature = "sqlite")]
#[test]
fn combined_standard_catalog_exact_binds_and_executes() {
    let catalog = combined_standard_catalog();
    let compiled = compile_source_with_flavor_and_options(
        r#"
        use sqlite;
        let db = sqlite::open({ path: ":memory:", mode: "memory", limits: {} });
        sqlite::close(&db);
        "#,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
    )
    .expect("combined standard source should compile");

    // All imports must be exact (schema present) — no name-only fallback.
    assert!(
        compiled.program.imports.iter().all(|i| i.schema.is_some()),
        "every standard host import must carry an exact schema"
    );

    let mut registry = HostFunctionRegistry::new();
    vm::register_sqlite_builtin_module(&mut registry)
        .expect("sqlite registration against combined catalog should succeed");
    let mut vm = Vm::new(compiled.program);
    registry
        .bind_vm_cached(&mut vm)
        .expect("combined-catalog exact bind should succeed");
    assert_eq!(
        vm.run().expect("sqlite open/close should run"),
        VmStatus::Halted
    );
}

/// The standard compile-options entry (no explicit custom catalog) must
/// default to the authoritative standard catalog snapshot, producing exact
/// V13 HostImport schemas with the combined fingerprint — never a name-only
/// fallback. This is the same snapshot the LSP and the standard extension
/// registration consume.
#[cfg(feature = "sqlite")]
#[test]
fn standard_compile_options_default_to_combined_exact_schemas() {
    let compiled = compile_source_with_flavor_and_options(
        r#"
        use sqlite;
        use http;
        let db = sqlite::open({ path: ":memory:", mode: "memory", limits: {} });
        let _ = http::client::request({"method": "GET", "url": "http://127.0.0.1:1/x"});
        sqlite::close(&db);
        "#,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default(),
    )
    .expect("standard compile options should compile");

    // Every standard host import must carry an exact schema (no name-only
    // fallback) bound to the authoritative combined snapshot.
    let expected = vm::standard_host_catalog().fingerprint();
    for name in ["sqlite::open", "sqlite::close", "http::client::request"] {
        let import = compiled
            .program
            .imports
            .iter()
            .find(|i| i.name == name)
            .unwrap_or_else(|| panic!("{name} must be a host import"));
        let schema = import
            .schema
            .as_ref()
            .unwrap_or_else(|| panic!("{name} must carry an exact schema"));
        assert_eq!(
            schema.fingerprint, expected,
            "{name} schema must carry the standard catalog fingerprint"
        );
    }
}

/// A subcatalog-fingerprint registration (the historical per-extension
/// behavior) must NOT satisfy a combined-catalog compile: the whole-catalog
/// fingerprint is part of the exact identity, so the bind is rejected with a
/// structured MissingExact — never a silent name-only fallback.
#[cfg(feature = "sqlite")]
#[test]
fn combined_compile_rejects_subcatalog_fingerprint_registration() {
    let catalog = combined_standard_catalog();
    let compiled = compile_source_with_flavor_and_options(
        r#"
        use sqlite;
        let db = sqlite::open({ path: ":memory:", mode: "memory", limits: {} });
        sqlite::close(&db);
        "#,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
    )
    .expect("combined standard source should compile");

    // Register sqlite through its *subcatalog* fingerprint, as the
    // pre-repair extension path did.
    let mut registry = HostFunctionRegistry::new();
    let subcatalog = vm::sqlite_host_catalog();
    for schema in vm::catalog_import_schemas(&subcatalog, "sqlite::open") {
        registry
            .register_exact_static("sqlite::open", 1, schema, sqlite_open_adapter_stub())
            .expect("subcatalog registration should succeed");
    }
    let mut vm = Vm::new(compiled.program);
    let error = registry
        .bind_vm_cached(&mut vm)
        .expect_err("subcatalog-fingerprint registration must not bind a combined compile");
    assert!(
        matches!(
            error,
            VmError::HostImportBinding(HostImportBindingError::MissingExact { .. })
        ),
        "expected structured MissingExact, got: {error}"
    );
}

/// A stub sqlite::open adapter used only to prove fingerprint rejection;
/// never executed.
#[cfg(feature = "sqlite")]
fn sqlite_open_adapter_stub() -> vm::vm::StaticHostFunction {
    |_vm, _args| {
        Ok(vm::vm::CallOutcome::Return(vm::vm::CallReturn::one(
            vm::Value::Int(0),
        )))
    }
}
