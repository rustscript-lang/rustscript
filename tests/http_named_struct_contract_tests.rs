//! HTTP host contract: the resource API's catalog identity, typed member
//! surfaces, exact compile diagnostics, permission preflight, and runtime
//! resource lifecycle.
//!
//! The HTTP module is resource-based: it publishes **no** named structs. A
//! request is an owned `resource<http.request>` builder handed to
//! `http::client::request`, a buffered response is a `resource<http.response>`,
//! an SSE stream returns a `resource<http.sse_summary>` and takes positional
//! `fn(string, string, string, string) -> bool` callbacks. Public values stay
//! fully typed at the guest boundary, so every shape mismatch below is rejected
//! by the compiler overload solver rather than at runtime.

#![cfg(all(feature = "http-client", not(target_family = "wasm")))]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use vm::compiler::{
    CompileSourceFileOptions, SourceFlavor, compile_source_with_flavor_and_options,
};
use vm::{
    CapabilityProfile, HostFunctionRegistry, HostParamPassing, HostTypeSchema, HttpConfig,
    HttpHostExt, ResourceTypeKey, Value, Vm, VmError, VmStatus, catalog_import_schemas,
    compile_source, http_host_catalog, standard_host_catalog,
};

fn key(name: &str) -> ResourceTypeKey {
    ResourceTypeKey::new(name).expect("static resource key")
}

fn resource(name: &str) -> HostTypeSchema {
    HostTypeSchema::Resource(key(name))
}

const RESOURCE_KEYS: [&str; 7] = [
    "http.internal.request_worker",
    "http.internal.response_stream",
    "http.sse",
    "http.request",
    "http.response",
    "http.headers",
    "http.sse_summary",
];

const FUNCTION_NAMES: [&str; 20] = [
    "http::request::new",
    "http::request::set_header",
    "http::request::set_body_text",
    "http::request::set_body_bytes",
    "http::client::request",
    "http::response::status",
    "http::response::url",
    "http::response::header_values",
    "http::response::header_names",
    "http::response::body",
    "http::headers::values",
    "http::headers::names",
    "http::sse_summary::outcome",
    "http::sse_summary::status",
    "http::sse_summary::headers",
    "http::sse_summary::url",
    "http::sse_summary::items",
    "http::sse_summary::bytes_received",
    "http::sse_summary::bytes_sent",
    "http::client::sse",
];

fn event_callback() -> HostTypeSchema {
    HostTypeSchema::Callable {
        params: vec![HostTypeSchema::String; 4],
        result: Box::new(HostTypeSchema::Bool),
    }
}

fn open_callback() -> HostTypeSchema {
    HostTypeSchema::Callable {
        params: vec![
            HostTypeSchema::Int,
            resource("http.headers"),
            HostTypeSchema::String,
        ],
        result: Box::new(HostTypeSchema::Bool),
    }
}

fn compile_ok(source: &str) {
    compile_with_standard_catalog(source)
        .unwrap_or_else(|err| panic!("expected compile success, got {err}"));
}

fn compile_err(source: &str) -> String {
    match compile_with_standard_catalog(source) {
        Ok(_) => panic!("expected compile error"),
        Err(err) => err.to_string(),
    }
}

fn compile_with_standard_catalog(source: &str) -> Result<vm::CompiledProgram, vm::SourcePathError> {
    compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(standard_host_catalog()),
    )
}

#[test]
fn http_catalog_declares_resources_and_no_named_structs() {
    let catalog = http_host_catalog();

    assert!(
        catalog.structs().is_empty(),
        "the resource HTTP surface must not publish named structs, found {:?}",
        catalog
            .structs()
            .iter()
            .map(|schema| schema.name.as_str())
            .collect::<Vec<_>>()
    );

    let keys: Vec<String> = catalog
        .resources()
        .iter()
        .map(|resource| resource.key.to_string())
        .collect();
    assert_eq!(keys, RESOURCE_KEYS);

    for resource in catalog.resources() {
        assert!(
            !resource.description.trim().is_empty(),
            "resource `{}` must come from a HostResourceType declaration",
            resource.key
        );
    }

    let names: Vec<&str> = catalog
        .functions()
        .iter()
        .map(|function| function.name.as_str())
        .collect();
    for name in FUNCTION_NAMES {
        assert!(
            names.contains(&name),
            "catalog must declare `{name}`, got {names:?}"
        );
    }
}

#[test]
fn buffered_request_takes_an_owned_builder_and_returns_a_response_resource() {
    let catalog = http_host_catalog();
    let request = catalog
        .function("http::client::request")
        .expect("http::client::request");
    assert_eq!(request.params.len(), 1);
    assert_eq!(request.params[0].name, "request");
    assert_eq!(request.params[0].ty, resource("http.request"));
    assert_eq!(request.params[0].passing, HostParamPassing::TakeOwned);
    assert_eq!(request.return_type, resource("http.response"));
}

#[test]
fn request_builder_members_use_borrow_mut_and_borrow_predicates() {
    let catalog = http_host_catalog();
    let new = catalog
        .function("http::request::new")
        .expect("http::request::new");
    assert_eq!(new.params.len(), 2);
    assert_eq!(new.params[0].ty, HostTypeSchema::String);
    assert_eq!(new.params[1].ty, HostTypeSchema::String);
    assert_eq!(new.return_type, resource("http.request"));

    for (name, arity) in [
        ("http::request::set_header", 3),
        ("http::request::set_body_text", 2),
        ("http::request::set_body_bytes", 2),
    ] {
        let schema = catalog.function(name).unwrap_or_else(|| panic!("{name}"));
        assert_eq!(schema.params.len(), arity, "{name} arity");
        assert_eq!(schema.params[0].ty, resource("http.request"), "{name}");
        assert_eq!(
            schema.params[0].passing,
            HostParamPassing::BorrowMut,
            "{name} must borrow the builder mutably"
        );
    }

    for name in [
        "http::response::status",
        "http::response::url",
        "http::response::header_names",
        "http::response::body",
    ] {
        let schema = catalog.function(name).unwrap_or_else(|| panic!("{name}"));
        assert_eq!(schema.params[0].ty, resource("http.response"), "{name}");
        assert_eq!(
            schema.params[0].passing,
            HostParamPassing::Borrow,
            "{name} must borrow the response"
        );
    }

    let status = catalog.function("http::response::status").unwrap();
    assert_eq!(status.return_type, HostTypeSchema::Int);
    let url = catalog.function("http::response::url").unwrap();
    assert_eq!(url.return_type, HostTypeSchema::String);
    let body = catalog.function("http::response::body").unwrap();
    assert_eq!(body.return_type, HostTypeSchema::Bytes);

    for name in [
        "http::response::header_values",
        "http::response::header_names",
        "http::headers::values",
        "http::headers::names",
    ] {
        let schema = catalog.function(name).unwrap_or_else(|| panic!("{name}"));
        assert_eq!(
            schema.return_type,
            HostTypeSchema::Array(Box::new(HostTypeSchema::String)),
            "{name} must return ordered header strings"
        );
    }

    let values = catalog.function("http::response::header_values").unwrap();
    assert_eq!(values.params[0].ty, resource("http.response"));
    assert_eq!(values.params[1].ty, HostTypeSchema::String);
    let headers = catalog.function("http::headers::values").unwrap();
    assert_eq!(headers.params[0].ty, resource("http.headers"));
    assert_eq!(headers.params[1].ty, HostTypeSchema::String);
}

#[test]
fn sse_overloads_use_resource_request_summary_and_positional_callbacks() {
    let catalog = http_host_catalog();
    let overloads = catalog.functions_named("http::client::sse");
    assert_eq!(
        overloads.len(),
        4,
        "the SSE surface must keep four overloads (event, +open, +timeout, +both)"
    );

    for overload in &overloads {
        assert_eq!(overload.params[0].name, "request");
        assert_eq!(overload.params[0].ty, resource("http.request"));
        assert_eq!(overload.params[0].passing, HostParamPassing::TakeOwned);
        assert_eq!(overload.params[1].name, "on_event");
        assert_eq!(overload.params[1].ty, event_callback());
        assert_eq!(overload.return_type, resource("http.sse_summary"));
    }

    let arities: Vec<usize> = overloads.iter().map(|schema| schema.params.len()).collect();
    assert_eq!(arities, [2, 3, 3, 4]);

    // The overload declaration order is: event, event+timeout, event+open,
    // event+open+timeout. Compare each positional slot without relying on
    // `Ord`, which the schema types do not implement.
    for overload in &overloads {
        let slots: Vec<(&str, HostTypeSchema, HostParamPassing)> = overload
            .params
            .iter()
            .map(|param| (param.name.as_str(), param.ty.clone(), param.passing))
            .collect();
        match slots.len() {
            2 => assert_eq!(
                slots,
                vec![
                    (
                        "request",
                        resource("http.request"),
                        HostParamPassing::TakeOwned
                    ),
                    ("on_event", event_callback(), HostParamPassing::Value),
                ]
            ),
            3 => assert!(
                slots
                    == vec![
                        (
                            "request",
                            resource("http.request"),
                            HostParamPassing::TakeOwned
                        ),
                        ("on_event", event_callback(), HostParamPassing::Value),
                        ("on_open", open_callback(), HostParamPassing::TakeOwned),
                    ]
                    || slots
                        == vec![
                            (
                                "request",
                                resource("http.request"),
                                HostParamPassing::TakeOwned
                            ),
                            ("on_event", event_callback(), HostParamPassing::Value),
                            ("timeout_ms", HostTypeSchema::Int, HostParamPassing::Value),
                        ],
                "a three-slot SSE overload must add open or timeout, got {slots:?}"
            ),
            4 => assert_eq!(
                slots,
                vec![
                    (
                        "request",
                        resource("http.request"),
                        HostParamPassing::TakeOwned
                    ),
                    ("on_event", event_callback(), HostParamPassing::Value),
                    ("on_open", open_callback(), HostParamPassing::TakeOwned),
                    ("timeout_ms", HostTypeSchema::Int, HostParamPassing::Value),
                ]
            ),
            other => panic!("unexpected SSE overload arity {other}"),
        }
    }

    let event_only = overloads
        .iter()
        .find(|schema| schema.params.len() == 2)
        .expect("the event-only overload exists");
    assert_eq!(event_only.params[1].passing, HostParamPassing::Value);

    let open_overload = overloads
        .iter()
        .find(|schema| schema.params.len() == 3 && schema.params[2].name == "on_open")
        .expect("the open overload exists");
    assert_eq!(
        open_overload.params[2].passing,
        HostParamPassing::TakeOwned,
        "the open callback is consumed like the event callback"
    );

    let timeout_overload = overloads
        .iter()
        .find(|schema| schema.params.len() == 3 && schema.params[2].name == "timeout_ms")
        .expect("the timeout overload exists");
    assert_eq!(timeout_overload.params[2].ty, HostTypeSchema::Int);
    assert_eq!(timeout_overload.params[2].passing, HostParamPassing::Value);
}

#[test]
fn sse_summary_accessors_read_the_typed_summary_resource() {
    let catalog = http_host_catalog();
    for name in [
        "http::sse_summary::outcome",
        "http::sse_summary::status",
        "http::sse_summary::headers",
        "http::sse_summary::url",
        "http::sse_summary::items",
        "http::sse_summary::bytes_received",
        "http::sse_summary::bytes_sent",
    ] {
        let schema = catalog.function(name).unwrap_or_else(|| panic!("{name}"));
        assert_eq!(schema.params.len(), 1, "{name} arity");
        assert_eq!(schema.params[0].ty, resource("http.sse_summary"), "{name}");
        assert_eq!(
            schema.params[0].passing,
            HostParamPassing::Borrow,
            "{name} must borrow the summary"
        );
    }
    assert_eq!(
        catalog
            .function("http::sse_summary::outcome")
            .unwrap()
            .return_type,
        HostTypeSchema::String
    );
    for name in [
        "http::sse_summary::status",
        "http::sse_summary::items",
        "http::sse_summary::bytes_received",
        "http::sse_summary::bytes_sent",
    ] {
        assert_eq!(
            catalog.function(name).unwrap().return_type,
            HostTypeSchema::Int,
            "{name} must return an int"
        );
    }
    assert_eq!(
        catalog
            .function("http::sse_summary::url")
            .unwrap()
            .return_type,
        HostTypeSchema::String
    );
    assert_eq!(
        catalog
            .function("http::sse_summary::headers")
            .unwrap()
            .return_type,
        resource("http.headers")
    );
}

#[test]
fn compiler_import_schemas_preserve_resource_identity_and_module_fingerprint() {
    let catalog = http_host_catalog();
    let request = &catalog_import_schemas(&catalog, "http::client::request")[0];
    assert_eq!(request.params[0].name, "request");
    assert_eq!(request.params[0].schema, resource("http.request"));
    assert_eq!(request.params[0].passing, HostParamPassing::TakeOwned);
    assert_eq!(request.return_type, resource("http.response"));
    assert_eq!(request.fingerprint, catalog.fingerprint());

    let sse = &catalog_import_schemas(&catalog, "http::client::sse")[0];
    assert_eq!(sse.params[0].schema, resource("http.request"));
    assert_eq!(sse.params[1].schema, event_callback());
    assert_eq!(sse.return_type, resource("http.sse_summary"));
    assert_eq!(sse.fingerprint, catalog.fingerprint());

    let headers = &catalog_import_schemas(&catalog, "http::sse_summary::headers")[0];
    assert_eq!(headers.params[0].schema, resource("http.sse_summary"));
    assert_eq!(headers.return_type, resource("http.headers"));
    assert_eq!(headers.fingerprint, catalog.fingerprint());
}

#[test]
fn object_literal_request_is_rejected_in_favour_of_the_owned_builder() {
    let message = compile_err(
        r#"
        use http;
        http::client::request({ method: "GET", url: "http://127.0.0.1:1/x" });
        "#,
    );
    assert!(
        message.contains("no host function `http::client::request` matches the arguments"),
        "the dynamic object request must fail the typed host call, got {message}"
    );
    assert!(
        message.contains("expected resource<http.request>"),
        "the diagnostic must name the expected request resource, got {message}"
    );
    assert!(
        message.contains("found object<method: string, url: string>"),
        "the diagnostic must show the found object literal, got {message}"
    );
}

#[test]
fn scalar_and_bytes_arguments_are_rejected_for_the_owned_builder() {
    let scalar = compile_err(
        r#"
        use http;
        let mut request = http::request::new("POST", "http://127.0.0.1:1/x");
        http::request::set_body_text(&mut request, "raw-body");
        http::client::request("raw-body");
        "#,
    );
    assert!(
        scalar.contains("expected resource<http.request>, found string"),
        "a bare string must not match the request builder, got {scalar}"
    );

    let map_body = compile_err(
        r#"
        use http;
        http::client::request({ method: "POST", url: "http://127.0.0.1:1/x", body: "raw-body" });
        "#,
    );
    assert!(
        map_body.contains("expected resource<http.request>, found object<"),
        "a map carrier request must be rejected, got {map_body}"
    );
}

#[test]
fn passing_a_borrow_where_ownership_is_required_is_rejected() {
    let message = compile_err(
        r#"
        use http;
        let mut request = http::request::new("GET", "http://127.0.0.1:1/x");
        http::request::set_header(request, "x-test", "value");
        "#,
    );
    assert!(
        message.contains("no host function `http::request::set_header` matches the arguments"),
        "a bare binding must not satisfy `borrow_mut`, got {message}"
    );
    assert!(
        message.contains("best candidate is `http::request::set_header(resource<http.request> borrow_mut, string, string)`"),
        "the diagnostic must show the borrow_mut contract, got {message}"
    );

    let copied = compile_err(
        r#"
        use http;
        let request = http::request::new("GET", "http://127.0.0.1:1/x");
        let first = http::client::request(request);
        let second = http::client::request(request.copy());
        "#,
    );
    assert!(
        copied.contains("expected passing take_owned, found passing value"),
        "a copy must not satisfy an owned resource parameter, got {copied}"
    );
}

#[test]
fn reusing_a_consumed_builder_is_reported_as_a_move() {
    let message = compile_err(
        r#"
        use http;
        let request = http::request::new("GET", "http://127.0.0.1:1/x");
        http::client::request(request);
        http::client::request(request);
        "#,
    );
    assert!(
        message.contains("local 'request' was moved earlier"),
        "the owned builder must be consumed exactly once, got {message}"
    );
    assert!(
        message.contains("use 'request.copy()' to copy it before moving"),
        "the move diagnostic must suggest an explicit copy, got {message}"
    );
}

#[test]
fn wrong_resource_kind_is_rejected_by_each_accessor_family() {
    for (source, expected, candidate) in [
        (
            r#"use http; let request = http::request::new("GET", "http://127.0.0.1:1/x"); http::response::status(&request);"#,
            "expected resource<http.response>, found resource<http.request>",
            "best candidate is `http::response::status(resource<http.response> borrow)`",
        ),
        (
            r#"use http; let request = http::request::new("GET", "http://127.0.0.1:1/x"); http::headers::values(&request, "x-test");"#,
            "expected resource<http.headers>, found resource<http.request>",
            "best candidate is `http::headers::values(resource<http.headers> borrow, string)`",
        ),
        (
            r#"use http; let request = http::request::new("GET", "http://127.0.0.1:1/x"); http::sse_summary::status(&request);"#,
            "expected resource<http.sse_summary>, found resource<http.request>",
            "best candidate is `http::sse_summary::status(resource<http.sse_summary> borrow)`",
        ),
    ] {
        let message = compile_err(source);
        assert!(
            message.contains(expected),
            "the diagnostic must name both resource kinds, got {message}"
        );
        assert!(
            message.contains(candidate),
            "the diagnostic must show the borrowed candidate, got {message}"
        );
    }
}

#[test]
fn sse_callback_shape_mismatches_name_the_positional_signature() {
    let wrong_arity = compile_err(
        r#"
        use http;
        fn on_event(kind: string) -> bool { true }
        let request = http::request::new("GET", "http://127.0.0.1:1/events");
        http::client::sse(request, on_event);
        "#,
    );
    assert!(
        wrong_arity.contains("no host function `http::client::sse` matches the arguments"),
        "an incompatible callback must fail the SSE host call, got {wrong_arity}"
    );
    assert!(
        wrong_arity.contains("argument 1: expected fn(string, string, string, string) -> bool, found fn(string) -> bool"),
        "the diagnostic must name the positional callback contract, got {wrong_arity}"
    );

    let wrong_result = compile_err(
        r#"
        use http;
        fn on_event(kind: string, data: string, id: string, retry: string) -> int { 1 }
        let request = http::request::new("GET", "http://127.0.0.1:1/events");
        http::client::sse(request, on_event);
        "#,
    );
    assert!(
        wrong_result.contains("found fn(string, string, string, string) -> int"),
        "a non-bool callback result must be reported, got {wrong_result}"
    );

    let map_input = compile_err(
        r#"
        use http;
        fn on_event(item: map) -> bool { true }
        let request = http::request::new("GET", "http://127.0.0.1:1/events");
        http::client::sse(request, on_event);
        "#,
    );
    assert!(
        map_input.contains("found fn(map<unknown>) -> bool"),
        "the legacy map-input callback must be rejected, got {map_input}"
    );

    let map_result = compile_err(
        r#"
        use http;
        fn on_event(kind: string, data: string, id: string, retry: string) -> map {
            { action: "continue" }
        }
        let request = http::request::new("GET", "http://127.0.0.1:1/events");
        http::client::sse(request, on_event);
        "#,
    );
    assert!(
        map_result.contains("found fn(string, string, string, string) -> map<unknown>"),
        "the legacy map-returning callback must be rejected, got {map_result}"
    );
}

#[test]
fn sse_open_callback_must_take_a_headers_resource() {
    let message = compile_err(
        r#"
        use http;
        fn on_event(kind: string, data: string, id: string, retry: string) -> bool { true }
        fn on_open(status: int, headers: string, url: string) -> bool { status == 200 }
        let request = http::request::new("GET", "http://127.0.0.1:1/events");
        http::client::sse(request, on_event, on_open, 500);
        "#,
    );
    assert!(
        message.contains("argument 2: expected fn(int, resource<http.headers>, string) -> bool"),
        "the open callback must require the headers resource, got {message}"
    );
    assert!(
        message.contains("best candidate is `http::client::sse(resource<http.request> take_owned, fn(string, string, string, string) -> bool, fn(int, resource<http.headers>, string) -> bool take_owned, int)`"),
        "the diagnostic must show the open overload, got {message}"
    );
}

#[test]
fn sse_argument_arity_and_slot_order_are_enforced() {
    let too_few = compile_err(
        r#"
        use http;
        fn on_event(kind: string, data: string, id: string, retry: string) -> bool { true }
        let request = http::request::new("GET", "http://127.0.0.1:1/events");
        http::client::sse(request);
        "#,
    );
    assert!(
        too_few.contains("host function 'http::client::sse' has no overload with 1 argument(s); declared arities: 2, 3, 4"),
        "the arity diagnostic must list every SSE overload, got {too_few}"
    );

    let too_many = compile_err(
        r#"
        use http;
        fn on_event(kind: string, data: string, id: string, retry: string) -> bool { true }
        fn on_open(status: int, headers: resource<http.headers>, url: string) -> bool { true }
        let request = http::request::new("GET", "http://127.0.0.1:1/events");
        http::client::sse(request, on_event, on_open, 500, 1);
        "#,
    );
    assert!(
        too_many.contains("host function 'http::client::sse' has no overload with 5 argument(s); declared arities: 2, 3, 4"),
        "a fifth argument must be rejected, got {too_many}"
    );

    let swapped = compile_err(
        r#"
        use http;
        fn on_event(kind: string, data: string, id: string, retry: string) -> bool { true }
        fn on_open(status: int, headers: resource<http.headers>, url: string) -> bool { true }
        let request = http::request::new("GET", "http://127.0.0.1:1/events");
        http::client::sse(request, on_event, 500, on_open);
        "#,
    );
    assert!(
        swapped.contains(
            "argument 2: expected fn(int, resource<http.headers>, string) -> bool, found int"
        ),
        "the third positional argument must be the open callback, got {swapped}"
    );

    let arity = compile_err(r#"use http; http::request::new("GET");"#);
    assert!(
        arity.contains("host function 'http::request::new' has no overload with 1 argument(s); declared arities: 2"),
        "a short builder call must report the declared arity, got {arity}"
    );
}

#[test]
fn resource_programs_compile_with_borrows_and_owned_handoffs() {
    compile_ok(
        r#"
        use http;
        let mut request = http::request::new("POST", "http://127.0.0.1:1/x");
        http::request::set_header(&mut request, "content-type", "application/json");
        http::request::set_body_text(&mut request, "{}");
        let response = http::client::request(request);
        let status = http::response::status(&response);
        let url = http::response::url(&response);
        let body = http::response::body(&response);
        let values = http::response::header_values(&response, "x-test");
        let names = http::response::header_names(&response);
        [status, url, body, values, names];
        "#,
    );
}

#[test]
fn byte_body_builder_compiles_with_the_typed_bytes_parameter() {
    compile_ok(
        r#"
        use http;
        let mut request = http::request::new("POST", "http://127.0.0.1:1/x");
        http::request::set_body_bytes(&mut request, b"raw-body");
        "#,
    );
}

#[test]
fn sse_event_timeout_and_open_overloads_compile() {
    const CALLBACKS: &str = r#"
        use http;
        fn on_event(kind: string, data: string, id: string, retry: string) -> bool { true }
        fn on_open(status: int, headers: resource<http.headers>, url: string) -> bool {
            status == 200 && url != "" && http::headers::names(&headers) != []
        }
    "#;

    // One call per program: the frontend rejects the same import name being
    // imported more than once per module, so each overload is exercised on its
    // own module.
    compile_ok(&format!(
        "{CALLBACKS}
        let request = http::request::new(\"GET\", \"http://127.0.0.1:1/events\");
        http::client::sse(request, on_event);
        "
    ));
    compile_ok(&format!(
        "{CALLBACKS}
        let request = http::request::new(\"GET\", \"http://127.0.0.1:1/events\");
        http::client::sse(request, on_event, 5000);
        "
    ));
    compile_ok(&format!(
        "{CALLBACKS}
        let request = http::request::new(\"GET\", \"http://127.0.0.1:1/events\");
        http::client::sse(request, on_event, on_open);
        "
    ));
    compile_ok(&format!(
        "{CALLBACKS}
        let request = http::request::new(\"GET\", \"http://127.0.0.1:1/events\");
        http::client::sse(request, on_event, on_open, 5000);
        "
    ));
}

#[test]
fn sse_overloads_coexist_in_one_module_when_the_import_name_repeats_once() {
    // Two SSE calls of different arities are admitted; the frontend only
    // rejects a *duplicate* import declaration of the same name.
    compile_ok(
        r#"
        use http;
        fn on_event(kind: string, data: string, id: string, retry: string) -> bool { true }
        let first = http::request::new("GET", "http://127.0.0.1:1/events");
        let summary = http::client::sse(first, on_event);
        http::sse_summary::items(&summary);
        "#,
    );
}

#[test]
fn sse_summary_accessors_and_independent_headers_compile() {
    compile_ok(
        r#"
        use http;
        fn on_event(kind: string, data: string, id: string, retry: string) -> bool { true }
        let request = http::request::new("GET", "http://127.0.0.1:1/events");
        let summary = http::client::sse(request, on_event);
        let outcome = http::sse_summary::outcome(&summary);
        let status = http::sse_summary::status(&summary);
        let url = http::sse_summary::url(&summary);
        let items = http::sse_summary::items(&summary);
        let received = http::sse_summary::bytes_received(&summary);
        let sent = http::sse_summary::bytes_sent(&summary);
        let headers = http::sse_summary::headers(&summary);
        let marker = http::headers::values(&headers, "x-marker");
        let names = http::headers::names(&headers);
        [outcome, status, url, items, received, sent, marker, names];
        "#,
    );
}

#[test]
fn http_named_struct_names_are_unknown_to_guest_source() {
    let event = compile_err(r#"use http; fn on_event(item: SseEvent) -> bool { true }"#);
    assert!(
        event.contains("unknown struct schema 'SseEvent'"),
        "the resource surface must not expose an SseEvent struct, got {event}"
    );
    let request = compile_err(r#"use http; fn take(request: HttpRequest) -> int { 0 }"#);
    assert!(
        request.contains("unknown struct schema 'HttpRequest'"),
        "the resource surface must not expose an HttpRequest struct, got {request}"
    );
}

#[test]
fn guest_struct_declaration_of_a_former_http_name_stays_a_guest_struct() {
    // The HTTP module publishes no named structs, so a guest declaration of a
    // name the removed struct surface used to own is an ordinary guest struct
    // rather than a duplicate-catalog collision.
    let compiled = compile_with_standard_catalog(
        r#"
        struct SseEvent { kind: string }
        fn ident(item: SseEvent) -> SseEvent { item }
        ident({ kind: "event" });
        "#,
    )
    .unwrap_or_else(|err| panic!("a guest SseEvent must compile, got {err}"));
    assert!(
        compiled
            .program
            .named_struct_decls()
            .contains_key("SseEvent"),
        "the guest declaration must own the VMBC struct table entry"
    );
    assert!(
        http_host_catalog().structs().is_empty(),
        "the catalog must not collide with the guest declaration"
    );
}

#[test]
fn guest_vmbc_table_carries_no_http_resource_names() {
    let compiled = compile_source(
        r#"
        use http;
        fn on_event(kind: string, data: string, id: string, retry: string) -> bool { true }
        let request = http::request::new("GET", "http://127.0.0.1:1/events");
        http::client::sse(request, on_event);
        "#,
    )
    .unwrap_or_else(|err| {
        panic!("default compile_source must admit the resource SSE call, got {err}")
    });
    assert!(
        compiled.program.named_struct_decls().is_empty(),
        "the guest VMBC table must stay empty for a resource-only program, found {:?}",
        compiled
            .program
            .named_struct_decls()
            .keys()
            .collect::<Vec<_>>()
    );
    for name in RESOURCE_KEYS {
        assert!(
            !compiled.program.named_struct_decls().contains_key(name),
            "guest table must not carry resource key {name} as a struct"
        );
    }
}

#[test]
fn default_compile_source_emits_the_exact_http_module_schema() {
    let compiled = compile_source(
        r#"
        use http;
        fn on_event(kind: string, data: string, id: string, retry: string) -> bool { true }
        let request = http::request::new("GET", "http://127.0.0.1:1/events");
        http::client::sse(request, on_event);
        "#,
    )
    .unwrap_or_else(|err| {
        panic!("default compile_source must admit the resource SSE call, got {err}")
    });

    let index = compiled
        .program
        .imports
        .iter()
        .position(|import| import.name == "http::client::sse")
        .expect("default compile must admit http::client::sse");
    let schema = compiled
        .program
        .host_import_schemas()
        .get(index)
        .and_then(Option::as_ref)
        .expect("default compile must emit the exact SSE import schema");
    assert_eq!(schema.fingerprint, http_host_catalog().fingerprint());
    assert_eq!(schema.params[0].schema, resource("http.request"));
    assert_eq!(schema.params[1].schema, event_callback());
    assert_eq!(schema.return_type, resource("http.sse_summary"));

    let mut vm = Vm::new(compiled.program);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default registry must exact-bind catalog-backed SSE");
}

#[test]
fn options_compile_without_an_explicit_catalog_emits_the_same_schemas() {
    let compiled = compile_source_with_flavor_and_options(
        r#"
        use http;
        let mut request = http::request::new("GET", "http://127.0.0.1:1/x");
        http::request::set_header(&mut request, "x-test", "value");
        "#,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default(),
    )
    .unwrap_or_else(|err| panic!("default options compile must admit the builder, got {err}"));

    let names: Vec<&str> = compiled
        .program
        .imports
        .iter()
        .map(|import| import.name.as_str())
        .collect();
    assert_eq!(names, ["http::request::new", "http::request::set_header"]);
    for schema in compiled.program.host_import_schemas().iter().flatten() {
        assert_eq!(
            schema.fingerprint,
            http_host_catalog().fingerprint(),
            "{} must carry the HTTP module fingerprint",
            schema.name
        );
    }
}

#[test]
fn default_registry_binds_the_resource_surface_and_reports_a_live_builder() {
    let program = compile_source(
        r#"
        use http;
        let mut request = http::request::new("POST", "http://127.0.0.1:1/x");
        http::request::set_header(&mut request, "x-test", "value");
        http::request::set_body_bytes(&mut request, b"payload");
        42;
        "#,
    )
    .expect("resource source compiles")
    .program;
    let mut vm = Vm::new(program);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default registry must bind the resource surface");
    assert_eq!(vm.run().expect("run"), VmStatus::Halted);
    assert_eq!(vm.stack().last(), Some(&Value::Int(42)));
    assert_eq!(
        vm.host_context().resource_count(),
        1,
        "the owned builder must stay live in the execution scope"
    );
    vm.reset_for_reuse().expect("reset");
    assert_eq!(vm.host_context().resource_count(), 0);
}

#[test]
fn method_validation_happens_at_the_host_boundary_not_the_call_site() {
    let program =
        compile_source(r#"use http; http::request::new("TRACE", "http://127.0.0.1:1/x");"#)
            .expect("the method string is a guest value, so it must compile")
            .program;
    let mut vm = Vm::new(program);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("bind");
    let error = vm.run().expect_err("TRACE must be rejected by the host");
    assert!(
        error
            .to_string()
            .contains("HTTP method 'TRACE' is not allowed"),
        "{error}"
    );
    assert_eq!(vm.host_context().resource_count(), 0);
}

#[test]
fn permissions_preflight_every_http_import_before_any_mutation() {
    let program = compile_source(
        r#"
        use http;
        let mut request = http::request::new("GET", "http://127.0.0.1:1/x");
        http::request::set_header(&mut request, "x-test", "value");
        "#,
    )
    .expect("resource source compiles")
    .program;
    let mut vm = Vm::new(program);
    let mut registry = HostFunctionRegistry::restricted();
    registry.set_capability_profile(
        CapabilityProfile::builder()
            .allow_host_import("http::request::new")
            .build(),
    );
    let error = registry
        .bind_vm_cached(&mut vm)
        .expect_err("the ungranted import must be reported");
    assert!(
        error.to_string().contains("http::request::set_header"),
        "{error}"
    );
    assert_eq!(
        vm.host_context().resource_count(),
        0,
        "a rejected preflight must not leave resources behind"
    );
    assert_eq!(vm.host_context().operation_count(), 0);
}

#[test]
fn full_grant_preflight_binds_every_http_import() {
    let program = compile_source(
        r#"
        use http;
        let mut request = http::request::new("GET", "http://127.0.0.1:1/x");
        http::request::set_header(&mut request, "x-test", "value");
        "#,
    )
    .expect("resource source compiles")
    .program;
    let mut vm = Vm::new(program);
    let mut registry = HostFunctionRegistry::restricted();
    registry.set_capability_profile(
        CapabilityProfile::builder()
            .allow_host_import("http::request::new")
            .allow_host_import("http::request::set_header")
            .build(),
    );
    registry
        .bind_vm_cached(&mut vm)
        .expect("every granted import must bind");
}

#[test]
fn sse_runtime_callback_validators_keep_the_named_legacy_contract() {
    let program = compile_source(r#"pub fn callback(item: map) -> map { { action: "continue" } }"#)
        .expect("map callback compiles in isolation")
        .program;
    let mut vm = Vm::new(program);
    assert_eq!(vm.run().expect("run"), VmStatus::Halted);
    let callback = vm
        .resolve_exported_callable("callback")
        .expect("export callback");
    vm.validate_stream_callback_value(&callback)
        .expect("the generic stream still accepts fn(map) -> map");
    let error = vm
        .validate_sse_callback_value(&callback)
        .expect_err("SSE must reject the legacy map action");
    assert!(
        matches!(
            error,
            VmError::TypeMismatch("fn(SseEvent) -> SseCallbackAction")
        ),
        "the SSE callback diagnostic must name the removed named contract, got {error:?}"
    );
}

#[test]
fn generic_stream_callback_mismatch_describes_named_compatibility() {
    let program = compile_source(r#"pub fn callback(value: int) -> int { value }"#)
        .expect("mismatched callback compiles in isolation")
        .program;
    let mut vm = Vm::new(program);
    assert_eq!(vm.run().expect("run"), VmStatus::Halted);
    let callback = vm
        .resolve_exported_callable("callback")
        .expect("export callback");
    let error = vm
        .validate_stream_callback_value(&callback)
        .expect_err("the generic stream must reject an incompatible callback");
    assert!(
        matches!(
            error,
            VmError::TypeMismatch(
                "callable stream callback must accept one map or named input and return a map, named value, or object"
            )
        ),
        "the generic stream diagnostic must describe named compatibility, got {error:?}"
    );
    assert!(
        vm.validate_sse_callback_value(&callback).is_err(),
        "the SSE validator must reject the same callback"
    );
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

fn bind_resource_vm(source: &str, port: u16) -> Vm {
    let compiled = compile_source(source).expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.configure_http(local_http_config(port)).expect("config");
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("the default registry must bind the resource surface");
    vm
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
                let body_start = header_end + 4;
                let headers = std::str::from_utf8(&request[..body_start]).expect("headers utf8");
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        (name.eq_ignore_ascii_case("content-length"))
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .expect("content-length");
                while request.len() < body_start + content_length {
                    let read = stream.read(&mut buffer).expect("read body");
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                assert_eq!(&request[body_start..body_start + content_length], expected);
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
async fn owned_builder_completes_a_buffered_request_at_runtime() {
    let (port, server) = spawn_ok_server();
    let source = format!(
        r#"
        use http;
        let mut request = http::request::new("GET", "http://127.0.0.1:{port}/");
        http::request::set_header(&mut request, "x-test", "yes");
        let response = http::client::request(request);
        http::response::status(&response);
        "#
    );
    let mut vm = bind_resource_vm(&source, port);
    drive_vm_to_halt(&mut vm).await.expect("request");
    server.join().expect("server");
    assert_eq!(vm.stack().last(), Some(&Value::Int(200)));
}

#[tokio::test(flavor = "current_thread")]
async fn byte_request_body_is_accepted_at_runtime() {
    let (port, server) = spawn_post_body_server(b"raw-body");
    let source = format!(
        r#"
        use http;
        let mut request = http::request::new("POST", "http://127.0.0.1:{port}/");
        http::request::set_body_bytes(&mut request, b"raw-body");
        let response = http::client::request(request);
        http::response::status(&response);
        "#
    );
    let mut vm = bind_resource_vm(&source, port);
    drive_vm_to_halt(&mut vm)
        .await
        .expect("byte body request should complete");
    server.join().expect("server");
    assert_eq!(vm.stack().last(), Some(&Value::Int(200)));
}

#[tokio::test(flavor = "current_thread")]
async fn text_request_body_is_accepted_at_runtime() {
    let (port, server) = spawn_post_body_server(b"payload");
    let source = format!(
        r#"
        use http;
        let mut request = http::request::new("POST", "http://127.0.0.1:{port}/");
        http::request::set_body_text(&mut request, "payload");
        let response = http::client::request(request);
        http::response::status(&response);
        "#
    );
    let mut vm = bind_resource_vm(&source, port);
    drive_vm_to_halt(&mut vm)
        .await
        .expect("text body request should complete");
    server.join().expect("server");
    assert_eq!(vm.stack().last(), Some(&Value::Int(200)));
}
