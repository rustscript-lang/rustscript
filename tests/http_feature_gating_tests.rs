#[test]
fn http_callables_follow_the_http_client_feature_gate() {
    for name in [
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
        "http::client::sse",
    ] {
        let published = vm::default_host_callables()
            .iter()
            .any(|callable| callable.name == name);
        assert_eq!(
            published,
            cfg!(all(feature = "http-client", not(target_family = "wasm"))),
            "{name}"
        );
    }
}

#[test]
fn http_standard_catalog_entries_follow_the_native_transport_gate() {
    let catalog = vm::standard_host_catalog();
    for name in [
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
        "http::client::sse",
    ] {
        let published = catalog
            .functions()
            .iter()
            .any(|function| function.name == name);
        assert_eq!(
            published,
            cfg!(all(feature = "http-client", not(target_family = "wasm"))),
            "{name}"
        );
    }
}

#[cfg(all(feature = "http-client", not(target_family = "wasm")))]
#[test]
fn buffered_http_catalog_is_keyed_and_map_free() {
    use vm::{HostParamPassing, HostTypeSchema};
    let catalog = vm::http_host_catalog();
    for key in ["http.request", "http.response", "http.headers"] {
        assert!(
            catalog
                .resources()
                .iter()
                .any(|resource| resource.key.as_str() == key),
            "{key}"
        );
    }
    let function = |name| {
        catalog
            .functions()
            .iter()
            .find(|function| function.name == name)
            .unwrap_or_else(|| panic!("missing {name}"))
    };
    for (name, modes, result) in [
        (
            "http::request::new",
            vec![HostParamPassing::Value, HostParamPassing::Value],
            "http.request",
        ),
        (
            "http::request::set_header",
            vec![
                HostParamPassing::BorrowMut,
                HostParamPassing::Value,
                HostParamPassing::Value,
            ],
            "",
        ),
        (
            "http::request::set_body_text",
            vec![HostParamPassing::BorrowMut, HostParamPassing::Value],
            "",
        ),
        (
            "http::request::set_body_bytes",
            vec![HostParamPassing::BorrowMut, HostParamPassing::Value],
            "",
        ),
        (
            "http::client::request",
            vec![HostParamPassing::TakeOwned],
            "http.response",
        ),
        ("http::response::status", vec![HostParamPassing::Borrow], ""),
        ("http::response::url", vec![HostParamPassing::Borrow], ""),
        (
            "http::response::header_values",
            vec![HostParamPassing::Borrow, HostParamPassing::Value],
            "",
        ),
        (
            "http::response::header_names",
            vec![HostParamPassing::Borrow],
            "",
        ),
        ("http::response::body", vec![HostParamPassing::Borrow], ""),
        (
            "http::headers::values",
            vec![HostParamPassing::Borrow, HostParamPassing::Value],
            "",
        ),
        ("http::headers::names", vec![HostParamPassing::Borrow], ""),
    ] {
        let schema = function(name);
        assert_eq!(
            schema
                .params
                .iter()
                .map(|param| param.passing)
                .collect::<Vec<_>>(),
            modes,
            "{name}"
        );
        if !result.is_empty() {
            assert!(
                matches!(&schema.return_type, HostTypeSchema::Resource(key) if key.as_str() == result),
                "{name}"
            );
        }
        assert!(
            schema.params.iter().all(|param| !matches!(
                &param.ty,
                HostTypeSchema::Map(_) | HostTypeSchema::Unknown
            )),
            "{name}"
        );
        assert!(
            !matches!(
                &schema.return_type,
                HostTypeSchema::Map(_) | HostTypeSchema::Unknown
            ),
            "{name}"
        );
    }
}

#[cfg(all(feature = "http-client", not(target_family = "wasm")))]
#[test]
fn sse_callable_metadata_has_exact_stream_schema() {
    let callables = vm::default_host_callables()
        .iter()
        .filter(|callable| callable.name == "http::client::sse")
        .collect::<Vec<_>>();
    assert_eq!(callables.len(), 1);
    assert_eq!(callables[0].host_execution, vm::HostExecution::MaySuspend);
    let catalog = vm::http_host_catalog();
    let schemas = catalog
        .functions()
        .iter()
        .filter(|function| function.name == "http::client::sse")
        .collect::<Vec<_>>();
    use vm::{HostParamPassing, HostParamSchema, HostTypeSchema, ResourceTypeKey};
    let resource = |name| HostTypeSchema::Resource(ResourceTypeKey::new(name).unwrap());
    let callback = |params| HostTypeSchema::Callable {
        params,
        result: Box::new(HostTypeSchema::Bool),
    };
    let request = HostParamSchema::with_passing(
        "request",
        resource("http.request"),
        HostParamPassing::TakeOwned,
    );
    let event = HostParamSchema::value("on_event", callback(vec![HostTypeSchema::String; 4]));
    let open = HostParamSchema::with_passing(
        "on_open",
        callback(vec![
            HostTypeSchema::Int,
            resource("http.headers"),
            HostTypeSchema::String,
        ]),
        HostParamPassing::TakeOwned,
    );
    let timeout = HostParamSchema::value("timeout_ms", HostTypeSchema::Int);
    let expected = [
        vec![request.clone(), event.clone()],
        vec![request.clone(), event.clone(), open.clone()],
        vec![request.clone(), event.clone(), timeout.clone()],
        vec![request, event, open, timeout],
    ];
    assert_eq!(schemas.len(), expected.len());
    for (schema, params) in schemas.iter().zip(expected) {
        assert_eq!(schema.params, params);
        assert_eq!(schema.return_type, resource("http.sse_summary"));
    }
}
