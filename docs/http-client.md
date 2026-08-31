# Native HTTP client and SSE feature

The `http-client` Cargo feature enables the buffered HTTP request and callable
SSE host builtins on supported native targets. The feature name remains valid
on every target so workspace feature selection stays uniform, but the native
transport implementation is target-gated.

## Target boundary

The transport is compiled when both conditions hold:

- the `http-client` feature is enabled; and
- the target is not in Rust's `wasm` target family (`not(target_family = "wasm")`).

On `wasm32-unknown-unknown` and other wasm-family targets, enabling
`http-client` is intentionally a no-op for transport publication. Cargo does
not build the native Tokio networking, Hyper, Rustls, URL, or HTTP-body
transport dependencies. The HTTP module, `HttpConfig`/`HttpExtension`/
`HttpHostExt` exports, HTTP builtins and callables, HTTP catalog functions, and
LSP standard-catalog entries are absent. Browser or other wasm networking must
be supplied by the embedding host instead.

The build script uses the same target-family boundary when it selects host
source files for generated dispatch and catalog metadata. This keeps the
compiled runtime surface and generated metadata synchronized.

## Native API

On a supported native target, enabling `http-client` preserves the public API:

- `HttpConfig` controls request and stream limits, redirects, timeouts, and
  capability policy;
- `HttpExtension` and `HttpHostExt` install the native HTTP host integration;
- `register_http_builtin_module` and `http_host_catalog` expose the native
  resource schema and callable metadata;
- `http::client::request` returns a bounded response map; and
- `http::client::sse` drives a bounded SSE stream through a script callback.

The HTTP and SSE behavior, resource lifecycle, cancellation, and native async
bridge contracts are unchanged by the wasm boundary. See
[`callable-runtime.md`](callable-runtime.md) for the general callable and
host-runtime contract.

## Feature checks

For a native HTTP build:

```bash
cargo test -p pd-vm --no-default-features --features runtime,http-client --test http_feature_gating_tests
```

For the wasm gating check, keep the feature enabled while selecting a wasm
package or target:

```bash
cargo check -p pd-vm --no-default-features --features runtime,http-client \
  --target wasm32-unknown-unknown
```

This verifies that feature selection is accepted without publishing the native
transport surface.
