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
    let callable = vm::default_host_callables()
        .iter()
        .find(|callable| callable.name == "http::client::sse")
        .expect("SSE callable should be published");
    assert_eq!(
        callable
            .signature
            .params
            .iter()
            .map(|param| (param.name, param.ty.display_label(), param.optional))
            .collect::<Vec<_>>(),
        [
            ("request", "map".to_string(), false),
            ("on_event", "fn(map) -> map".to_string(), false),
        ]
    );
    assert_eq!(callable.signature.return_type, "map");
    assert_eq!(callable.host_execution, vm::HostExecution::MaySuspend);
}
