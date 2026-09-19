#![cfg(all(feature = "http-client", not(target_family = "wasm")))]

use std::fs;
use std::path::PathBuf;

fn source(path: &str) -> String {
    fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path))
        .unwrap_or_else(|error| panic!("failed to read {path}: {error}"))
}

#[test]
fn http_hosts_are_macro_owned_async_functions_without_private_drivers() {
    let module = source("src/builtins/runtime/http/mod.rs");
    let request = source("src/builtins/runtime/http/request.rs");
    let sse = source("src/builtins/runtime/http/sse.rs");
    let cargo = source("Cargo.toml");

    assert!(
        module.contains("async fn builtin_http_client_request"),
        "the buffered host implementation must be an async macro function"
    );
    assert!(
        sse.contains("async fn builtin_http_client_sse"),
        "the SSE host implementation must be an async macro function"
    );
    assert!(
        request.contains("hyper_util::client::legacy::Client"),
        "HTTP transport and pooling must be owned by hyper-util"
    );
    assert!(
        module.contains("client: request::HttpClient"),
        "the cloneable Hyper client must live in per-VM HTTP module state"
    );
    assert!(
        cargo.contains("\"client-legacy\"") && cargo.contains("\"http1\""),
        "the HTTP feature must enable Hyper's maintained pooled client"
    );

    for (path, text) in [
        ("src/builtins/runtime/http/mod.rs", module.as_str()),
        ("src/builtins/runtime/http/request.rs", request.as_str()),
        ("src/builtins/runtime/http/sse.rs", sse.as_str()),
    ] {
        for forbidden in [
            "submit_host_future",
            "HostAsyncBridge",
            "std::thread",
            "JoinHandle",
            "tokio::runtime",
            "runtime_block_on",
            "HostOperation",
            "HostResource",
            "AtomicWaker",
        ] {
            assert!(
                !text.contains(forbidden),
                "{path} must not contain HTTP/SSE-owned async plumbing: {forbidden}"
            );
        }
    }

    for forbidden in [
        "HttpRequestResource",
        "HttpResponseResource",
        "SseStreamResource",
        "SseScopeOperation",
        "BufferedRequestShared",
        "SseShared",
    ] {
        assert!(
            !module.contains(forbidden) && !request.contains(forbidden) && !sse.contains(forbidden),
            "transient HTTP/SSE operation or resource state remains: {forbidden}"
        );
    }

    assert!(
        sse.contains("impl HostStreamDriver for SseStreamDriver"),
        "SSE may retain only the generic callable-stream continuation driver"
    );
    assert!(
        sse.contains("submit_callable_stream"),
        "SSE callback re-entry must use the generic callable-stream continuation"
    );
}
