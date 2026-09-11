//! HTTP/SSE named-struct host contract: catalog identity, field access,
//! object-literal params, and dynamic header maps.
//!
//! Runtime values remain `Value::Map`. SSE inbound events stay maps because
//! they are a tagged union (`open` / `event` / `end`).

#![cfg(feature = "http-client")]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::task::{Context, Poll};
use std::thread;

use vm::compiler::TypeSchema;
use vm::{
    CallReturn, HostAsyncBridge, HostFunctionRegistry, HostFuture, HostFutureOutput, HostOpId,
    HostStructSchema, HostTypeSchema, HttpConfig, HttpHostExt, Value, Vm, VmError, VmResult,
    VmStatus, catalog_import_schemas, compile_source, http_host_catalog,
    register_http_builtin_module,
};

fn opt(inner: HostTypeSchema) -> HostTypeSchema {
    HostTypeSchema::Optional(Box::new(inner))
}

fn map_string() -> HostTypeSchema {
    HostTypeSchema::Map(Box::new(HostTypeSchema::String))
}

fn map_unknown() -> HostTypeSchema {
    HostTypeSchema::Map(Box::new(HostTypeSchema::Unknown))
}

fn struct_by_name<'a>(catalog: &'a vm::HostApiCatalog, name: &str) -> &'a HostStructSchema {
    catalog
        .struct_named(name)
        .unwrap_or_else(|| panic!("catalog must declare named struct {name}"))
}

fn field_names(schema: &HostStructSchema) -> Vec<&str> {
    schema
        .fields
        .iter()
        .map(|field| field.name.as_str())
        .collect()
}

fn field_ty<'a>(schema: &'a HostStructSchema, name: &str) -> &'a HostTypeSchema {
    &schema
        .fields
        .iter()
        .find(|field| field.name == name)
        .unwrap_or_else(|| panic!("{} must declare field {name}", schema.name))
        .ty
}

fn compile_ok(source: &str) {
    compile_source(source).unwrap_or_else(|err| panic!("expected compile success, got {err}"));
}

fn compile_err(source: &str) -> String {
    match compile_source(source) {
        Ok(_) => panic!("expected compile error"),
        Err(err) => err.to_string(),
    }
}

#[test]
fn http_catalog_declares_fixed_shape_named_structs() {
    let catalog = http_host_catalog();
    let names: Vec<&str> = catalog.structs().iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "HttpRequest",
            "HttpResponse",
            "SseRequest",
            "SseCallbackAction",
            "SseSummary"
        ]
    );

    let request = struct_by_name(&catalog, "HttpRequest");
    assert_eq!(field_names(request), ["method", "url", "headers", "body"]);
    assert_eq!(field_ty(request, "method"), &HostTypeSchema::String);
    assert_eq!(field_ty(request, "url"), &HostTypeSchema::String);
    assert_eq!(field_ty(request, "headers"), &opt(map_string()));
    assert_eq!(field_ty(request, "body"), &opt(HostTypeSchema::Unknown));

    let sse_request = struct_by_name(&catalog, "SseRequest");
    assert_eq!(
        field_names(sse_request),
        ["method", "url", "headers", "body", "timeout_ms"]
    );
    assert_eq!(field_ty(sse_request, "method"), &HostTypeSchema::String);
    assert_eq!(field_ty(sse_request, "url"), &HostTypeSchema::String);
    assert_eq!(field_ty(sse_request, "headers"), &opt(map_string()));
    assert_eq!(field_ty(sse_request, "body"), &opt(HostTypeSchema::Unknown));
    assert_eq!(
        field_ty(sse_request, "timeout_ms"),
        &opt(HostTypeSchema::Int)
    );

    let response = struct_by_name(&catalog, "HttpResponse");
    assert_eq!(field_names(response), ["status", "headers", "body", "url"]);
    assert_eq!(field_ty(response, "status"), &HostTypeSchema::Int);
    assert_eq!(field_ty(response, "headers"), &map_unknown());
    assert_eq!(field_ty(response, "body"), &HostTypeSchema::Bytes);
    assert_eq!(field_ty(response, "url"), &HostTypeSchema::String);

    let action = struct_by_name(&catalog, "SseCallbackAction");
    assert_eq!(field_names(action), ["action"]);
    assert_eq!(field_ty(action, "action"), &HostTypeSchema::String);

    let summary = struct_by_name(&catalog, "SseSummary");
    assert_eq!(
        field_names(summary),
        [
            "outcome",
            "status",
            "headers",
            "url",
            "items",
            "bytes_received",
            "bytes_sent"
        ]
    );
    assert_eq!(field_ty(summary, "outcome"), &HostTypeSchema::String);
    assert_eq!(field_ty(summary, "status"), &HostTypeSchema::Int);
    assert_eq!(field_ty(summary, "headers"), &map_unknown());
    assert_eq!(field_ty(summary, "url"), &HostTypeSchema::String);
    assert_eq!(field_ty(summary, "items"), &HostTypeSchema::Int);
    assert_eq!(field_ty(summary, "bytes_received"), &HostTypeSchema::Int);
    assert_eq!(field_ty(summary, "bytes_sent"), &HostTypeSchema::Int);
}

#[test]
fn http_request_and_sse_use_named_request_response_and_action_types() {
    let catalog = http_host_catalog();
    let request = catalog
        .function("http::client::request")
        .expect("http::client::request");
    assert_eq!(
        request.params[0].ty,
        struct_by_name(&catalog, "HttpRequest").as_type()
    );
    assert_eq!(
        request.return_type,
        struct_by_name(&catalog, "HttpResponse").as_type()
    );

    let sse = catalog
        .function("http::client::sse")
        .expect("http::client::sse");
    assert_eq!(
        sse.params[0].ty,
        struct_by_name(&catalog, "SseRequest").as_type()
    );
    assert_eq!(
        sse.params[1].ty,
        HostTypeSchema::Callable {
            params: vec![map_unknown()],
            result: Box::new(struct_by_name(&catalog, "SseCallbackAction").as_type()),
        }
    );
    assert_eq!(
        sse.return_type,
        struct_by_name(&catalog, "SseSummary").as_type()
    );
}

#[test]
fn compiler_import_schemas_preserve_named_identity() {
    let catalog = http_host_catalog();
    let request = &catalog_import_schemas(&catalog, "http::client::request")[0];
    assert_eq!(
        request.params[0].schema,
        TypeSchema::Named("HttpRequest".into(), vec![])
    );
    assert_eq!(
        request.return_type,
        TypeSchema::Named("HttpResponse".into(), vec![])
    );
    let sse = &catalog_import_schemas(&catalog, "http::client::sse")[0];
    assert_eq!(
        sse.params[0].schema,
        TypeSchema::Named("SseRequest".into(), vec![])
    );
    assert_eq!(
        sse.params[1].schema,
        TypeSchema::Callable {
            params: vec![TypeSchema::Map(Box::new(TypeSchema::Unknown))],
            result: Box::new(TypeSchema::Named("SseCallbackAction".into(), vec![])),
        }
    );
    assert_eq!(
        sse.return_type,
        TypeSchema::Named("SseSummary".into(), vec![])
    );
}

#[test]
fn object_literal_is_accepted_for_named_http_request() {
    compile_ok(
        r#"
        use http;
        http::client::request({ method: "GET", url: "http://127.0.0.1:1/x" });
        "#,
    );
}

#[test]
fn object_literal_with_dynamic_headers_is_accepted() {
    compile_ok(
        r#"
        use http;
        http::client::request({
            method: "POST",
            url: "http://127.0.0.1:1/x",
            headers: { "content-type": "application/json" },
            body: "{}"
        });
        "#,
    );
}

#[test]
fn object_literal_with_byte_body_is_accepted() {
    compile_ok(
        r#"
        use http;
        http::client::request({
            method: "POST",
            url: "http://127.0.0.1:1/x",
            body: b"raw-body"
        });
        "#,
    );
}

#[test]
fn object_literal_missing_required_method_is_rejected() {
    let message = compile_err(
        r#"
        use http;
        http::client::request({ url: "http://127.0.0.1:1/x" });
        "#,
    );
    assert!(
        message.contains("no host function `http::client::request` matches the arguments"),
        "missing method must name the host function, got {message}"
    );
    assert!(
        message.contains("expected HttpRequest"),
        "missing method must name HttpRequest, got {message}"
    );
    assert!(
        message.contains("url: string"),
        "missing method diagnostic must show the found object, got {message}"
    );
}

#[test]
fn buffered_request_rejects_sse_only_timeout_ms() {
    let message = compile_err(
        r#"
        use http;
        http::client::request({
            method: "GET",
            url: "http://127.0.0.1:1/x",
            timeout_ms: 20
        });
        "#,
    );
    assert!(
        message.contains("no host function `http::client::request` matches the arguments"),
        "SSE-only timeout_ms must not match HttpRequest, got {message}"
    );
    assert!(
        message.contains("expected HttpRequest"),
        "buffered request mismatch must name HttpRequest, got {message}"
    );
    assert!(
        message.contains("timeout_ms"),
        "buffered request mismatch must mention timeout_ms, got {message}"
    );
}

#[test]
fn optional_null_headers_and_body_compile_for_buffered_request() {
    compile_ok(
        r#"
        use http;
        http::client::request({
            method: "GET",
            url: "http://127.0.0.1:1/x",
            headers: null,
            body: null
        });
        "#,
    );
}

#[test]
fn optional_null_timeout_compiles_for_sse_request() {
    compile_ok(
        r#"
        use http;
        fn on_event(item: map) -> SseCallbackAction {
            { action: "continue" }
        }
        http::client::sse(
            { method: "GET", url: "http://127.0.0.1:1/events", timeout_ms: null },
            on_event
        );
        "#,
    );
}

#[test]
fn named_http_response_allows_field_access() {
    compile_ok(
        r#"
        use http;
        let response = http::client::request({ method: "GET", url: "http://127.0.0.1:1/x" });
        let status = response.status;
        let url = response.url;
        let body = response.body;
        "#,
    );
}

#[test]
fn named_http_response_rejects_unknown_field() {
    let message = compile_err(
        r#"
        use http;
        let response = http::client::request({ method: "GET", url: "http://127.0.0.1:1/x" });
        response.not_a_field;
        "#,
    );
    assert!(
        message.contains("field 'not_a_field' is not declared"),
        "unknown field must name the missing field, got {message}"
    );
}

#[test]
fn named_http_response_rejects_unknown_string_index() {
    let message = compile_err(
        r#"
        use http;
        let response = http::client::request({ method: "GET", url: "http://127.0.0.1:1/x" });
        response["not_a_field"];
        "#,
    );
    assert!(
        message.contains("field 'not_a_field' is not declared"),
        "unknown string index must name the missing field, got {message}"
    );
}

#[test]
fn dynamic_response_headers_remain_indexable_maps() {
    compile_ok(
        r#"
        use http;
        let response = http::client::request({ method: "GET", url: "http://127.0.0.1:1/x" });
        let headers = response.headers;
        headers["content-type"];
        "#,
    );
}

#[test]
fn sse_object_literal_and_action_struct_compile() {
    compile_ok(
        r#"
        use http;
        fn on_event(item: map) -> SseCallbackAction {
            { action: "continue" }
        }
        http::client::sse(
            { method: "GET", url: "http://127.0.0.1:1/events" },
            on_event
        );
        "#,
    );
}

#[test]
fn sse_map_returning_callback_is_rejected_with_action_schema() {
    let message = compile_err(
        r#"
        use http;
        fn on_event(item: map) -> map {
            { action: "continue" }
        }
        http::client::sse(
            { method: "GET", url: "http://127.0.0.1:1/events" },
            on_event
        );
        "#,
    );
    assert!(
        message.contains("no host function `http::client::sse` matches the arguments"),
        "map-returning callback must fail the SSE host call, got {message}"
    );
    assert!(
        message.contains("SseCallbackAction"),
        "SSE callback mismatch must name SseCallbackAction, got {message}"
    );
    assert!(
        message.contains("map"),
        "SSE callback mismatch must mention the found map result, got {message}"
    );
}

#[test]
fn sse_summary_allows_field_access() {
    compile_ok(
        r#"
        use http;
        fn on_event(item: map) -> SseCallbackAction {
            { action: "stop" }
        }
        let summary = http::client::sse(
            { method: "GET", url: "http://127.0.0.1:1/events" },
            on_event
        );
        let outcome = summary.outcome;
        let items = summary.items;
        let received = summary.bytes_received;
        let sent = summary.bytes_sent;
        "#,
    );
}

#[test]
fn sse_inbound_event_stays_a_dynamic_map() {
    compile_ok(
        r#"
        use http;
        fn on_event(item: map) -> SseCallbackAction {
            if item["kind"] == "event" {
                print(item["data"]);
            }
            { action: "continue" }
        }
        http::client::sse(
            { method: "GET", url: "http://127.0.0.1:1/events", timeout_ms: 20 },
            on_event
        );
        "#,
    );
}

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

fn standard_http_registry() -> HostFunctionRegistry {
    let mut registry = HostFunctionRegistry::new();
    register_http_builtin_module(&mut registry).expect("register HTTP");
    registry
}

async fn drive_vm_to_halt(vm: &mut Vm) -> Result<(), VmError> {
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

fn local_http_config(port: u16) -> HttpConfig {
    HttpConfig {
        allowed_schemes: vec!["http".into()],
        allowed_hosts: vec!["127.0.0.1".into()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        ..HttpConfig::default()
    }
}

fn bind_http_vm(source: &str, port: u16) -> Vm {
    let compiled = compile_source(source).expect("source should compile");
    let mut vm = Vm::try_new(compiled.program).expect("vm");
    vm.configure_http(local_http_config(port)).expect("config");
    vm.set_async_bridge(Box::<TokioHostDriver>::default());
    standard_http_registry()
        .bind_vm_cached(&mut vm)
        .expect("bind");
    vm
}

fn spawn_ok_server() -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream.read(&mut buffer).expect("read");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nX-Test: yes\r\n\r\nok")
            .expect("write");
    });
    (port, server)
}

fn spawn_post_body_server(expected: &'static [u8]) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream.read(&mut buffer).expect("read");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                let header_end = header_end + 4;
                let headers = std::str::from_utf8(&request[..header_end]).expect("headers utf8");
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        (name.eq_ignore_ascii_case("content-length"))
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .expect("content-length");
                while request.len() < header_end + content_length {
                    let read = stream.read(&mut buffer).expect("read body");
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                assert_eq!(&request[header_end..header_end + content_length], expected);
                break;
            }
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .expect("write");
    });
    (port, server)
}

#[tokio::test(flavor = "current_thread")]
async fn named_response_field_access_reads_runtime_map_carrier() {
    let (port, server) = spawn_ok_server();
    let source = format!(
        r#"
        use http;
        let response = http::client::request({{ method: "GET", url: "http://127.0.0.1:{port}/" }});
        response.status;
        "#
    );
    let mut vm = bind_http_vm(&source, port);
    drive_vm_to_halt(&mut vm).await.expect("request");
    server.join().expect("server");
    assert_eq!(vm.stack()[0], Value::Int(200));
}

#[tokio::test(flavor = "current_thread")]
async fn byte_request_body_is_accepted_at_runtime() {
    let (port, server) = spawn_post_body_server(b"raw-body");
    let source = format!(
        r#"
        use http;
        let response = http::client::request({{
            method: "POST",
            url: "http://127.0.0.1:{port}/",
            body: b"raw-body"
        }});
        response.status;
        "#
    );
    let mut vm = bind_http_vm(&source, port);
    drive_vm_to_halt(&mut vm)
        .await
        .expect("byte body request should complete");
    server.join().expect("server");
    assert_eq!(vm.stack()[0], Value::Int(200));
}

#[tokio::test(flavor = "current_thread")]
async fn null_headers_are_treated_as_omitted() {
    let (port, server) = spawn_ok_server();
    let source = format!(
        r#"
        use http;
        let response = http::client::request({{
            method: "GET",
            url: "http://127.0.0.1:{port}/",
            headers: null
        }});
        response.status;
        "#
    );
    let mut vm = bind_http_vm(&source, port);
    drive_vm_to_halt(&mut vm)
        .await
        .expect("null headers must match omitted headers");
    server.join().expect("server");
    assert_eq!(vm.stack()[0], Value::Int(200));
}

#[test]
fn null_sse_timeout_is_treated_as_omitted() {
    let source = r#"
        use http;
        fn on_event(item: map) -> SseCallbackAction { { action: "continue" } }
        http::client::sse(
            { method: "GET", url: "http://127.0.0.1:1/events", timeout_ms: null },
            on_event
        );
    "#;
    let compiled = compile_source(source).expect("null timeout_ms should compile");
    let mut vm = Vm::try_new(compiled.program).expect("vm");
    vm.set_http_max_in_flight(0);
    vm.configure_http(local_http_config(1)).expect("config");
    standard_http_registry()
        .bind_vm_cached(&mut vm)
        .expect("bind");
    let error = vm
        .run()
        .expect_err("zero in-flight must reject after timeout parse");
    assert!(
        error.to_string().contains("in-flight request limit"),
        "null timeout_ms must be omitted rather than type-mismatch, got {error}"
    );
}

#[test]
fn sse_callback_runtime_schema_rejects_arbitrary_map_result() {
    let compiled = compile_source(
        r#"
        pub fn callback(item: map) -> map { { action: "continue" } }
        "#,
    )
    .expect("map callback should compile in isolation");
    let mut vm = Vm::try_new(compiled.program).expect("vm");
    assert_eq!(vm.run().expect("run"), VmStatus::Halted);
    let callback = vm
        .resolve_exported_callable("callback")
        .expect("export callback");
    vm.validate_stream_callback_value(&callback)
        .expect("generic stream still accepts fn(map) -> map");
    let error = vm
        .validate_sse_callback_value(&callback)
        .expect_err("SSE must reject arbitrary map results");
    assert!(
        matches!(error, VmError::TypeMismatch("fn(map) -> SseCallbackAction")),
        "SSE callback diagnostic must name SseCallbackAction, got {error:?}"
    );
}

#[test]
fn sse_callback_runtime_schema_accepts_named_action() {
    let compiled = compile_source(
        r#"
        use http;
        pub fn callback(item: map) -> SseCallbackAction { { action: "continue" } }
        "#,
    )
    .expect("named action callback should compile");
    let mut vm = Vm::try_new(compiled.program).expect("vm");
    assert_eq!(vm.run().expect("run"), VmStatus::Halted);
    let callback = vm
        .resolve_exported_callable("callback")
        .expect("export callback");
    vm.validate_sse_callback_value(&callback)
        .expect("SseCallbackAction must be accepted");
}
