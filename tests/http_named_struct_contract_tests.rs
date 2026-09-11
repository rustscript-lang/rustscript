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
    HostStructField, HostStructSchema, HostTypeSchema, HttpConfig, HttpHostExt, Value, Vm, VmError,
    VmResult, VmStatus, catalog_import_schemas, compile_source, http_host_catalog,
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

fn http_request_struct() -> HostStructSchema {
    HostStructSchema::new(
        "HttpRequest",
        vec![
            HostStructField::new("method", HostTypeSchema::String),
            HostStructField::new("url", HostTypeSchema::String),
            HostStructField::new("headers", opt(map_string())),
            HostStructField::new("body", opt(HostTypeSchema::String)),
            HostStructField::new("timeout_ms", opt(HostTypeSchema::Int)),
        ],
    )
}

fn http_response_struct() -> HostStructSchema {
    HostStructSchema::new(
        "HttpResponse",
        vec![
            HostStructField::new("status", HostTypeSchema::Int),
            HostStructField::new("headers", map_unknown()),
            HostStructField::new("body", HostTypeSchema::Bytes),
            HostStructField::new("url", HostTypeSchema::String),
        ],
    )
}

fn sse_callback_action_struct() -> HostStructSchema {
    HostStructSchema::new(
        "SseCallbackAction",
        vec![HostStructField::new("action", HostTypeSchema::String)],
    )
}

fn sse_summary_struct() -> HostStructSchema {
    HostStructSchema::new(
        "SseSummary",
        vec![
            HostStructField::new("outcome", HostTypeSchema::String),
            HostStructField::new("status", HostTypeSchema::Int),
            HostStructField::new("headers", map_unknown()),
            HostStructField::new("url", HostTypeSchema::String),
            HostStructField::new("items", HostTypeSchema::Int),
            HostStructField::new("bytes_received", HostTypeSchema::Int),
            HostStructField::new("bytes_sent", HostTypeSchema::Int),
        ],
    )
}

fn struct_by_name<'a>(catalog: &'a vm::HostApiCatalog, name: &str) -> &'a HostStructSchema {
    catalog
        .struct_named(name)
        .unwrap_or_else(|| panic!("catalog must declare named struct {name}"))
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
            "SseCallbackAction",
            "SseSummary"
        ]
    );
    assert_eq!(
        struct_by_name(&catalog, "HttpRequest").fields,
        http_request_struct().fields
    );
    assert_eq!(
        struct_by_name(&catalog, "HttpResponse").fields,
        http_response_struct().fields
    );
    assert_eq!(
        struct_by_name(&catalog, "SseCallbackAction").fields,
        sse_callback_action_struct().fields
    );
    assert_eq!(
        struct_by_name(&catalog, "SseSummary").fields,
        sse_summary_struct().fields
    );
}

#[test]
fn http_request_and_sse_use_named_request_response_and_action_types() {
    let catalog = http_host_catalog();
    let request = catalog
        .function("http::client::request")
        .expect("http::client::request");
    assert_eq!(request.params[0].ty, http_request_struct().as_type());
    assert_eq!(request.return_type, http_response_struct().as_type());

    let sse = catalog
        .function("http::client::sse")
        .expect("http::client::sse");
    assert_eq!(sse.params[0].ty, http_request_struct().as_type());
    assert_eq!(
        sse.params[1].ty,
        HostTypeSchema::Callable {
            params: vec![map_unknown()],
            result: Box::new(sse_callback_action_struct().as_type()),
        }
    );
    assert_eq!(sse.return_type, sse_summary_struct().as_type());
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
        TypeSchema::Named("HttpRequest".into(), vec![])
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
fn object_literal_missing_required_method_is_rejected() {
    let message = compile_err(
        r#"
        use http;
        http::client::request({ url: "http://127.0.0.1:1/x" });
        "#,
    );
    assert!(
        message.contains("HttpRequest")
            || message.contains("method")
            || message.contains("match")
            || message.contains("field"),
        "missing method should not match HttpRequest, got {message}"
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
        message.contains("not_a_field")
            || message.contains("field")
            || message.contains("HttpResponse"),
        "unknown field must be rejected, got {message}"
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
        message.contains("not_a_field")
            || message.contains("field")
            || message.contains("HttpResponse"),
        "unknown string index must be rejected, got {message}"
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

#[tokio::test(flavor = "current_thread")]
async fn named_response_field_access_reads_runtime_map_carrier() {
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

    let source = format!(
        r#"
        use http;
        let response = http::client::request({{ method: "GET", url: "http://127.0.0.1:{port}/" }});
        response.status;
        "#
    );
    let compiled = compile_source(&source).expect("named field access should compile");
    let mut vm = Vm::try_new(compiled.program).expect("vm");
    vm.configure_http(HttpConfig {
        allowed_schemes: vec!["http".into()],
        allowed_hosts: vec!["127.0.0.1".into()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        ..HttpConfig::default()
    })
    .expect("config");
    vm.set_async_bridge(Box::<TokioHostDriver>::default());
    standard_http_registry()
        .bind_vm_cached(&mut vm)
        .expect("bind");
    drive_vm_to_halt(&mut vm).await.expect("request");
    server.join().expect("server");
    assert_eq!(vm.stack()[0], Value::Int(200));
}
