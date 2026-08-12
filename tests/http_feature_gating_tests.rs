#[test]
fn http_callables_follow_the_http_client_feature_gate() {
    let published = vm::default_host_callables()
        .iter()
        .any(|callable| callable.name == "http::client::request");
    assert_eq!(published, cfg!(feature = "http-client"));
}
