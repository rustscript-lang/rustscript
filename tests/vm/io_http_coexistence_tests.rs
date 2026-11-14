//! IO and HTTP coexistence tests for the async host-adapter build.
#![cfg(all(feature = "http-client", not(target_family = "wasm")))]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use vm::{
    HostFunctionRegistry, HttpConfig, HttpHostExt, IoHostExt, IoPolicy, ResourceTypeKey, Value, Vm,
    VmResult, VmStatus, compile_source, register_http_builtin_module, standard_host_catalog,
};
#[cfg(all(feature = "sqlite", not(target_family = "wasm")))]
use vm::{SqliteHostExt, SqlitePolicy, VmError};

#[path = "../support/http_test_bridge.rs"]
mod http_test_bridge;
use http_test_bridge::TokioHostDriver;

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

/// Guest source that issues one HTTP GET through the resource request API and
/// leaves the response status as the final stack value.
fn http_request_source(port: u16) -> String {
    format!(
        "use http; \
         let req = http::request::new(\"GET\", \"http://127.0.0.1:{port}/\"); \
         let response = http::client::request(req); \
         http::response::status(&response);"
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
    http_test_bridge::install(&mut vm);
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
        let req = http::request::new("GET", "http://127.0.0.1:{port}/");
        let response = http::client::request(req);
        http::response::status(&response);
        "#
    );
    let compiled = compile_source(&source).expect("combined source should compile");
    let mut vm = Vm::new(compiled.program);
    http_test_bridge::install(&mut vm);
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
    http_test_bridge::install(&mut vm);
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
    http_test_bridge::install(&mut vm);
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
    http_test_bridge::install(&mut vm);
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
        let req = http::request::new("GET", "http://127.0.0.1:{port}/");
        let response = http::client::request(req);
        let exists = io::exists("/");
        http::response::status(&response);
        "#
    );
    let compiled = compile_source(&source).expect("combined source should compile");
    let mut vm = Vm::new(compiled.program);
    http_test_bridge::install(&mut vm);
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
    http_test_bridge::install(&mut vm);
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
        let req = http::request::new("GET", "http://127.0.0.1:1/");
        http::client::request(req);
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
    http_test_bridge::install(&mut vm);
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
        let req = http::request::new("GET", "http://127.0.0.1:{port}/");
        let response = http::client::request(req);
        http::response::status(&response);
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
    http_test_bridge::install(&mut vm);
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
    assert_eq!(vm.stack().last(), Some(&Value::Int(200)));
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

// ---------------------------------------------------------------------------
// G3: IO + HTTP + SQLite in one VM, under the shared standard catalog.
//
// Everything below is compiled only when the SQLite domain is part of the
// feature union, so the HTTP-only target stays independently usable. The
// HTTP-only path above keeps proving IO/HTTP without SQLite; these cases add
// the genuine third domain rather than assuming an import or a registration
// equals coexistence.
// ---------------------------------------------------------------------------

/// One real payload read through IO, echoed through loopback HTTP, stored and
/// read back through SQLite. The text is stable so the final guest value and
/// the server-side request body can both be asserted byte-for-byte.
#[cfg(all(feature = "sqlite", not(target_family = "wasm")))]
const TRIPLE_DOMAIN_PAYLOAD: &str = "g3-triple-domain-payload-4271";

/// A TEMP-owned fixture root for the triple-domain cases: one IO root and one
/// SQLite database root beneath it. Cleanup is unconditional so a failed case
/// does not leave files behind.
#[cfg(all(feature = "sqlite", not(target_family = "wasm")))]
struct TripleDomainFixture {
    root: std::path::PathBuf,
    sqlite_root: std::path::PathBuf,
}

#[cfg(all(feature = "sqlite", not(target_family = "wasm")))]
impl TripleDomainFixture {
    fn new(label: &str) -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rustscript-g3-{label}-{}-{nonce}",
            std::process::id()
        ));
        let sqlite_root = root.join("sqlite");
        std::fs::create_dir_all(&sqlite_root)
            .expect("triple-domain fixture root should be created");
        Self { root, sqlite_root }
    }

    fn io_path(&self) -> String {
        self.root.join("input.txt").display().to_string()
    }

    fn sqlite_path(&self) -> String {
        self.sqlite_root.display().to_string()
    }

    fn write_input(&self, text: &str) {
        std::fs::write(self.root.join("input.txt"), text).expect("IO fixture should be writable");
    }
}

#[cfg(all(feature = "sqlite", not(target_family = "wasm")))]
impl Drop for TripleDomainFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A loopback server that reads each POST body and answers with `echo:<body>`
/// plus one empty-valued header. The thread returns every request body it
/// observed, so the caller can prove the bytes that travelled through IO
/// actually reached the wire.
#[cfg(all(feature = "sqlite", not(target_family = "wasm")))]
fn spawn_echo_server(requests: usize) -> (u16, thread::JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("echo listener should bind");
    let port = listener
        .local_addr()
        .expect("echo listener should have an address")
        .port();
    let server = thread::spawn(move || {
        let mut observed = Vec::new();
        for _ in 0..requests {
            let (mut stream, _) = listener.accept().expect("echo request should arrive");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            let head_end = loop {
                if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    break position + 4;
                }
                let read = stream
                    .read(&mut buffer)
                    .expect("echo request headers should be readable");
                assert_ne!(read, 0, "client closed before sending HTTP headers");
                request.extend_from_slice(&buffer[..read]);
            };
            let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
            assert!(
                head.starts_with("POST /echo HTTP/1.1"),
                "unexpected echo request line: {head}"
            );
            let content_length = head
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find_map(|(name, value)| {
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .expect("POST request should carry a body length");
            let mut body = request[head_end..].to_vec();
            while body.len() < content_length {
                let read = stream
                    .read(&mut buffer)
                    .expect("echo request body should be readable");
                assert_ne!(read, 0, "client closed before sending the HTTP body");
                body.extend_from_slice(&buffer[..read]);
            }
            assert_eq!(body.len(), content_length, "request body must be complete");
            observed.push(body.clone());
            let mut payload = b"echo:".to_vec();
            payload.extend_from_slice(&body);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nX-Empty: \r\n\r\n",
                payload.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("echo response head should be writable");
            stream
                .write_all(&payload)
                .expect("echo response body should be writable");
        }
        observed
    });
    (port, server)
}

/// Compiles one guest source against the shared standard catalog and proves
/// every emitted host import carries that catalog's exact schema. The same
/// catalog then installs the three module surfaces into one registry, so
/// compilation and binding share one contract.
#[cfg(all(feature = "sqlite", not(target_family = "wasm")))]
fn compile_with_shared_catalog(
    source: &str,
    expected_imports: &[&str],
) -> (Arc<vm::HostApiCatalog>, vm::Program) {
    let catalog = standard_host_catalog();
    let compiled = vm::compile_source_with_flavor_and_options(
        source,
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
    )
    .expect("standard-catalog triple-domain source should compile");
    assert!(
        compiled
            .program
            .host_import_schemas()
            .iter()
            .all(Option::is_some),
        "every triple-domain host import must carry an exact catalog schema"
    );
    assert!(
        compiled
            .program
            .host_import_schemas()
            .iter()
            .flatten()
            .all(|schema| schema.fingerprint == catalog.fingerprint()),
        "every import schema must come from the shared standard catalog"
    );
    let imports: Vec<&str> = compiled
        .program
        .imports
        .iter()
        .map(|import| import.name.as_str())
        .collect();
    for expected in expected_imports {
        assert!(
            imports.contains(expected),
            "the program must import {expected}, got {imports:?}"
        );
    }
    (catalog, compiled.program)
}

/// Binds the same shared catalog into one registry through the installed
/// standard module descriptors: IO, HTTP and SQLite coexist in one dispatch
/// table and bind by exact schema, not by name/arity alone.
#[cfg(all(feature = "sqlite", not(target_family = "wasm")))]
fn bind_shared_catalog(vm: &mut Vm, catalog: &Arc<vm::HostApiCatalog>) {
    let mut registry = HostFunctionRegistry::empty();
    for module_name in ["io", "http", "sqlite"] {
        let module = vm::standard_host_modules()
            .iter()
            .find(|module| module.name == module_name)
            .expect("the standard module must be composed");
        let descriptor = module
            .catalog_module()
            .expect("the standard module must publish a catalog surface");
        descriptor
            .install_from_catalog(&mut registry, catalog)
            .expect("the standard module must install from the shared catalog");
    }
    registry
        .bind_vm_cached(vm)
        .expect("the three exact catalog surfaces must bind in one registry");
}

#[cfg(all(feature = "sqlite", not(target_family = "wasm")))]
#[tokio::test(flavor = "current_thread")]
async fn triple_domain_single_vm_typed_dataflow_uses_shared_catalog() {
    let fixture = TripleDomainFixture::new("triple-dataflow");
    fixture.write_input(TRIPLE_DOMAIN_PAYLOAD);
    let (port, server) = spawn_echo_server(1);
    let source = format!(
        r#"
        use io;
        use http;
        use sqlite;
        use bytes;

        fn read_and_close(path: string) -> Result<string, string> {{
            let source = io::open(path, "r")?;
            let payload = io::read_all(source)?;
            io::close(source)?;
            Result::Ok(payload)
        }}
        let payload = match read_and_close("{fixture_path}") {{
            Result::Ok(text) => text,
            Result::Err(message) => message,
        }};

        let mut request = http::request::new("POST", "http://127.0.0.1:{port}/echo");
        let unit: () = http::request::set_body_text(&mut request, payload);
        let response = http::client::request(request);
        let status = http::response::status(&response);
        let echoed_bytes = http::response::body(&response);
        let echoed = bytes::to_utf8(echoed_bytes);
        let empty_values = http::response::header_values(&response, "x-empty");
        let absent_values = http::response::header_values(&response, "x-absent");

        let limits: SqliteLimits = {{}};
        let options: SqliteOpenOptions = {{ path: Option::Some("state.db"), mode: Option::Some("read_write_create"), limits: Option::Some(limits) }};
        let db = sqlite::open(options);
        let no_params: [SqliteValue] = [];
        sqlite::execute(&db, "CREATE TABLE payloads (id INTEGER PRIMARY KEY, value TEXT)", no_params);

        let text_value: SqliteValue = {{ kind: "text", text_value: Option::Some(echoed) }};
        let text_params: [SqliteValue] = [text_value];
        let inserted = sqlite::execute(&db, "INSERT INTO payloads (id, value) VALUES (1, ?1)", text_params);

        let null_value: SqliteValue = {{ kind: "null" }};
        let null_params: [SqliteValue] = [null_value];
        let null_inserted = sqlite::execute(&db, "INSERT INTO payloads (id, value) VALUES (2, ?1)", null_params);

        let empty_value: SqliteValue = {{ kind: "text", text_value: Option::Some("") }};
        let empty_params: [SqliteValue] = [empty_value];
        sqlite::execute(&db, "INSERT INTO payloads (id, value) VALUES (3, ?1)", empty_params);

        let query_limits: SqliteLimits = {{}};
        let queried = sqlite::query(&db, "SELECT id, value FROM payloads ORDER BY id", [], query_limits);
        let rows = queried.rows;
        assert(rows.length == 3);
        let text_cell = rows[0].cells[1];
        let stored = match text_cell.text_value {{ Option::Some(value) => value, Option::None => "" }};
        let null_cell = rows[1].cells[1];
        assert(null_cell.kind == "null");
        let null_int_absent = match null_cell.int_value {{ Option::Some(value) => false, Option::None => true }};
        let null_float_absent = match null_cell.float_value {{ Option::Some(value) => false, Option::None => true }};
        let null_text_absent = match null_cell.text_value {{ Option::Some(value) => false, Option::None => true }};
        let null_blob_absent = match null_cell.blob_value {{ Option::Some(value) => false, Option::None => true }};
        assert(null_int_absent);
        assert(null_float_absent);
        assert(null_text_absent);
        assert(null_blob_absent);
        let empty_cell = rows[2].cells[1];
        assert(empty_cell.kind == "text");
        let empty_present = match empty_cell.text_value {{ Option::Some(value) => value == "", Option::None => false }};
        assert(empty_present);
        let cursor = match queried.next_cursor {{ Option::Some(value) => value, Option::None => -1 }};
        assert(cursor == 3);

        let missing_limits: SqliteLimits = {{}};
        let missing = sqlite::query(&db, "SELECT id, value FROM payloads WHERE id = 99", [], missing_limits);
        assert(missing.rows.length == 0);
        let missing_absent = match missing.next_cursor {{ Option::Some(value) => false, Option::None => true }};
        assert(missing_absent);

        assert(status == 200);
        assert(empty_values.length == 1);
        assert(empty_values[0] == "");
        assert(absent_values.length == 0);
        assert(inserted.rows_affected == 1);
        assert(null_inserted.rows_affected == 1);
        let close_unit: () = sqlite::close(db);
        stored;
        "#,
        fixture_path = fixture.io_path(),
        port = port
    );
    let (catalog, program) = compile_with_shared_catalog(
        &source,
        &[
            "io::open",
            "io::read_all",
            "io::close",
            "http::request::new",
            "http::request::set_body_text",
            "http::client::request",
            "http::response::status",
            "http::response::body",
            "http::response::header_values",
            "sqlite::open",
            "sqlite::execute",
            "sqlite::query",
            "sqlite::close",
        ],
    );
    let mut vm = Vm::new(program);
    http_test_bridge::install(&mut vm);
    vm.configure_io(IoPolicy {
        allowed_roots: vec![fixture.root.display().to_string()],
        ..IoPolicy::default()
    });
    vm.configure_http(local_http_config(port))
        .expect("HTTP configuration should be valid");
    vm.configure_sqlite(SqlitePolicy {
        database_root: Some(fixture.sqlite_path()),
        ..SqlitePolicy::default()
    });
    bind_shared_catalog(&mut vm, &catalog);

    drive_vm_to_halt(&mut vm)
        .await
        .expect("the three-domain program should complete");
    let observed = server.join().expect("echo server should finish");
    assert_eq!(
        observed,
        vec![TRIPLE_DOMAIN_PAYLOAD.as_bytes().to_vec()],
        "the HTTP request body must be the exact bytes read through IO"
    );
    let expected = Value::String(Arc::new(format!("echo:{TRIPLE_DOMAIN_PAYLOAD}")));
    assert_eq!(
        vm.stack().last(),
        Some(&expected),
        "the final value must be the SQLite text cell echoed through HTTP"
    );
    assert_eq!(
        vm.execution_scope().operations().len(),
        0,
        "no composed host operation may remain pending"
    );
    assert_eq!(
        vm.execution_scope().resources().len(),
        1,
        "the immutable HTTP response is the only live resource after IO and SQLite close"
    );
}

/// The composed error/cleanup path: all three domains are configured and
/// active, the SQLite connection exists, and a policy violation in IO fails the
/// invocation. The domain resources then survive until the established reset
/// retires them, the same VM runs again with every policy intact, and a token
/// minted before reset cannot be revived afterwards.
#[cfg(all(feature = "sqlite", not(target_family = "wasm")))]
struct RestResourceProbe;

#[cfg(all(feature = "sqlite", not(target_family = "wasm")))]
impl vm::resource::HostResource for RestResourceProbe {
    fn begin_close(
        &mut self,
        _reason: vm::resource::ResourceCloseReason,
    ) -> vm::resource::ResourceResult<vm::resource::CloseProgress> {
        Ok(vm::resource::CloseProgress::Ready)
    }
}

#[cfg(all(feature = "sqlite", not(target_family = "wasm")))]
#[tokio::test(flavor = "current_thread")]
async fn triple_domain_error_cleanup_reset_and_reuse_keep_all_policies() {
    let fixture = TripleDomainFixture::new("triple-lifecycle");
    fixture.write_input(TRIPLE_DOMAIN_PAYLOAD);
    let (port, server) = spawn_echo_server(2);
    let source = format!(
        r#"
        use io;
        use http;
        use sqlite;

        fn read_payload(path: string) -> Result<string, string> {{
            let source = io::open(path, "r")?;
            io::read_all(source)
        }}
        let payload = match read_payload("{fixture_path}") {{
            Result::Ok(text) => text,
            Result::Err(message) => message,
        }};

        let mut request = http::request::new("POST", "http://127.0.0.1:{port}/echo");
        let unit: () = http::request::set_body_text(&mut request, payload);
        let response = http::client::request(request);
        let status = http::response::status(&response);

        let limits: SqliteLimits = {{}};
        let options: SqliteOpenOptions = {{ path: Option::Some("state.db"), mode: Option::Some("read_write_create"), limits: Option::Some(limits) }};
        let db = sqlite::open(options);
        let no_params: [SqliteValue] = [];
        sqlite::execute(&db, "CREATE TABLE IF NOT EXISTS items (value TEXT)", no_params);

        let write_probe = io::open("{fixture_path}", "w");
        status;
        "#,
        fixture_path = fixture.io_path(),
        port = port
    );
    let (catalog, program) = compile_with_shared_catalog(
        &source,
        &[
            "io::open",
            "io::read_all",
            "http::request::new",
            "http::request::set_body_text",
            "http::client::request",
            "http::response::status",
            "sqlite::open",
            "sqlite::execute",
        ],
    );
    let mut vm = Vm::new(program);
    http_test_bridge::install(&mut vm);
    vm.configure_io(IoPolicy {
        allowed_roots: vec![fixture.root.display().to_string()],
        ..IoPolicy::default()
    });
    vm.configure_http(local_http_config(port))
        .expect("HTTP configuration should be valid");
    vm.configure_sqlite(SqlitePolicy {
        database_root: Some(fixture.sqlite_path()),
        ..SqlitePolicy::default()
    });
    bind_shared_catalog(&mut vm, &catalog);

    let first_error = drive_vm_to_halt(&mut vm)
        .await
        .expect_err("a write without the IO write capability must fail");
    let first_message = match first_error {
        VmError::HostError(message) => message,
        other => panic!("the composed policy error must stay on the fatal host channel: {other:?}"),
    };
    assert!(
        first_message.contains("write capability"),
        "the IO policy error must name the write capability, got: {first_message}"
    );
    assert_eq!(
        vm.execution_scope().operations().len(),
        0,
        "the failed composed invocation must not leave a pending operation"
    );
    assert_eq!(
        vm.execution_scope().resources().len(),
        3,
        "the live IO file, HTTP response and SQLite connection must remain until reset"
    );

    // Mint a scope capability before reset; it must not be revivable afterwards.
    let probe = vm
        .host_context()
        .push_resource(RestResourceProbe)
        .expect("the pre-reset probe resource should register");
    finish_reset(&mut vm)
        .await
        .expect("reset must retire the failed composed invocation");
    assert_eq!(
        vm.execution_scope().resources().len(),
        0,
        "reset must close every domain and probe resource"
    );
    assert_eq!(
        vm.execution_scope().operations().len(),
        0,
        "reset must leave no pending operation"
    );
    assert!(
        vm.is_reusable(),
        "the VM must be reusable after the composed error reset"
    );
    assert!(
        vm.http_is_configured(),
        "the HTTP module state must survive reset"
    );
    assert_eq!(
        vm.sqlite_policy().database_root.as_deref(),
        Some(fixture.sqlite_path().as_str()),
        "the SQLite database root must survive reset"
    );
    let stale = vm
        .host_context()
        .take_resource::<RestResourceProbe>(probe.handle());
    assert!(
        stale.is_err(),
        "a capability minted before reset must not be revived by VM reuse"
    );

    let second_error = drive_vm_to_halt(&mut vm)
        .await
        .expect_err("the same policy violation must remain in force after reuse");
    let second_message = match second_error {
        VmError::HostError(message) => message,
        other => panic!("the reused failure must stay on the fatal host channel: {other:?}"),
    };
    assert!(
        second_message.contains("write capability"),
        "the IO policy must persist across reset, got: {second_message}"
    );
    let observed = server.join().expect("echo server should finish");
    assert_eq!(
        observed.len(),
        2,
        "both invocations must have performed their real HTTP request"
    );
    for body in observed {
        assert_eq!(body, TRIPLE_DOMAIN_PAYLOAD.as_bytes());
    }
    finish_reset(&mut vm)
        .await
        .expect("the second composed error must also reset cleanly");
    assert_eq!(
        vm.execution_scope().resources().len(),
        0,
        "the reused invocation must leave no live resource"
    );
}

/// The composed catalog keeps each module's resource keys and named-struct
/// table; merging the three surfaces must not overwrite a key or a name.
#[cfg(all(feature = "sqlite", not(target_family = "wasm")))]
#[test]
fn triple_domain_catalog_keys_and_named_structs_coexist() {
    let catalog = standard_host_catalog();
    for key in [
        "io.file",
        "http.request",
        "http.response",
        "http.headers",
        "http.sse_summary",
        "sqlite.connection",
    ] {
        assert!(
            catalog.resource(key).is_some(),
            "the composed catalog must publish the {key} resource"
        );
    }
    let keys: Vec<&str> = catalog
        .resources()
        .iter()
        .map(|resource| resource.key.as_str())
        .collect();
    let unique: std::collections::BTreeSet<&str> = keys.iter().copied().collect();
    assert_eq!(
        keys.len(),
        unique.len(),
        "composed resource keys must be unique: {keys:?}"
    );
    for name in [
        "SqliteLimits",
        "SqliteOpenOptions",
        "SqliteExecuteResult",
        "SqliteValue",
        "SqliteRow",
        "SqliteQueryResult",
        "SqliteStatement",
        "SqliteTransactionResult",
        "JitConfig",
    ] {
        assert!(
            catalog.struct_named(name).is_some(),
            "the composed catalog must keep the {name} named struct"
        );
        assert!(
            catalog.resource(name).is_none(),
            "the {name} named struct must not be overwritten by a resource key"
        );
    }
    for name in [
        "io::open",
        "io::read_all",
        "io::close",
        "http::client::request",
        "http::response::body",
        "sqlite::open",
        "sqlite::execute",
        "sqlite::query",
        "sqlite::close",
    ] {
        assert!(
            !catalog.functions_named(name).is_empty(),
            "the composed catalog must keep {name}"
        );
    }
    let modules = vm::standard_catalog_modules();
    assert_eq!(
        catalog.functions().len(),
        modules
            .iter()
            .map(|module| module.functions.len())
            .sum::<usize>(),
        "the composed catalog must be the exact merge of every catalog surface"
    );
}
