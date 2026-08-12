#[test]
fn http_callables_follow_the_http_client_feature_gate() {
    for name in [
        "http::client::request",
        "http::client::sse",
        "http::client::websocket",
    ] {
        let published = vm::default_host_callables()
            .iter()
            .any(|callable| callable.name == name);
        assert_eq!(published, cfg!(feature = "http-client"), "{name}");
    }
}

#[cfg(feature = "http-client")]
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
