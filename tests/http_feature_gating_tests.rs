#[test]
fn http_callables_follow_the_http_client_feature_gate() {
    for name in ["http::client::request", "http::client::sse"] {
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
    for name in ["http::client::request", "http::client::sse"] {
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
