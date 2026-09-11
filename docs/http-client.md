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

```rust
use http;

let response = http::client::request({
    method: "POST",
    url: "https://example.test/v1/messages",
    headers: {"content-type": "application/json"},
    body: "{}",
});
let status = response.status;
let body = response.body;
```

`http::client::request` accepts an `HttpRequest` object with:

- `method`: one of `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, or `OPTIONS`;
- `url`: an `http` or `https` URL admitted by host policy;
- `headers`: an optional string-to-string map;
- `body`: optional string.

The response is an `HttpResponse` with field access:

```rust
response.status   // int
response.headers  // map (dynamic string keys)
response.body     // bytes
response.url      // string, the final validated URL after redirects
```

Use field access (`response.status`). Index `response.headers` as a map. Unknown string indexes on the named response are rejected. The request body, response body, response head, redirect count, concurrent connection count, connect phase, and total request duration are bounded. `Host`, `Content-Length`, `Transfer-Encoding`, and `Connection` are client-managed request headers. A limit, policy, transport, TLS, redirect, or timeout failure is a host error and produces no response.

## Server-sent events

`http::client::sse` is available with the `http-client` feature.

```rust
fn on_sse(item: map) -> SseCallbackAction {
    if item["kind"] == "event" {
        print(item["data"]);
    }
    return { action: "continue" };
}

let result = http::client::sse({
    method: "GET",
    url: "https://example.test/events",
    headers: { accept: "text/event-stream" },
}, on_sse);
let outcome = result.outcome;
```

`http::client::sse(request, on_event)` uses the same `HttpRequest` object as buffered requests, plus optional `timeout_ms`:

| Field | Required | Accepted type and value | Bound or policy |
| --- | --- | --- | --- |
| `method` | yes | string: `GET` or `POST` | Other methods are rejected before transport admission |
| `url` | yes | string containing an `http` or `https` URL | Protocol family and the configured scheme, host, port, and address policy must all admit it |
| `headers` | no | map from string header names to string values | Names and values must be syntactically valid; client-managed request headers remain forbidden, and `Accept: text/event-stream` is supplied when absent |
| `body` | no | string, including for `POST` | Bounded by `max_request_body_bytes` |
| `timeout_ms` | no | positive integer milliseconds | Caps this optional shortening deadline by `HttpConfig::max_stream_duration` |

The callback schema is `fn(map) -> SseCallbackAction`. Inbound events stay maps because they are a tagged union (`open` / `event` / `end`). The response must have an event-stream content type. The response head remains bounded by the existing HTTP parser. The contract adds no configurable request-header byte accounting.

The callback receives exactly one map at a time, in this order:

```rust
// The response was accepted; this precedes every event.
{
    "kind": "open",
    "status": 200,
    "headers": map,
    "url": string,
}

// One parsed event. "event" is per-dispatch state, reset to null at every
// dispatch boundary (including a blank line that dispatches no event); "id"
// and "retry_ms" are persistent stream state, retaining the last valid
// values seen so far and null only before any value has been seen.
{
    "kind": "event",
    "event": string | null,
    "data": string,
    "id": string | null,
    "retry_ms": int | null,
}

// Clean EOF, after every preceding event callback completed.
{"kind": "end"}
```

The callback must return an `SseCallbackAction`:

```rust
{ action: "continue" }
{ action: "stop" }
```

`continue` acknowledges the item and permits the next network poll. `stop` ends the call locally. Any other shape or action is a callback error.

SSE parsing follows the event-stream grammar:

- UTF-8 text may start with one byte-order mark;
- `\r\n`, `\r`, and `\n` line endings are recognized;
- repeated `data:` fields are joined with `\n`, with the final join newline removed at dispatch;
- `event`, `id`, and decimal non-negative `retry` fields are normalized into the event map;
- comments and unknown fields are ignored;
- a blank line dispatches only after at least one `data:` field;
- malformed UTF-8, an over-limit line or event, and cumulative received event-stream application bytes exceeding the call limit are host errors.

There is no automatic reconnection. Values such as a provider's `[DONE]` marker remain ordinary event data.

## Terminal summaries and errors

After callback processing terminates normally, the SSE call returns an `SseSummary`:

```rust
result.outcome        // "eof" | "stopped"
result.status         // int
result.headers        // map
result.url            // string
result.items          // int
result.bytes_received // int
result.bytes_sent     // int
```

`items` counts delivered callback items. `bytes_received` and `bytes_sent` are observational summary counters. Limit enforcement uses independent entire-call accounting and does not depend on whether or how these counters are displayed.

Transport, parser, destination-policy, timeout, and callback failures stay errors. They are never converted into a successful terminal summary.

## Sequencing, backpressure, and lifecycle

Streaming is a single caller-owned operation:

1. the host polls for one protocol item;
2. the VM invokes `on_event` in a child script frame;
3. the callback returns one action;
4. the host applies that action before polling for another item.

At most one unacknowledged protocol item crosses the host/VM boundary. Decoder scratch space is bounded separately. The network future is not polled while the callback runs, yields, or waits in another async host call. If the callback yields or invokes an ordinary async host function, the callback resumes first; only its final action resumes the outer stream operation. This sequencing supplies backpressure without a background reader or callback queue.

The network future never owns or re-enters the VM. Callback error, protocol completion, configured deadline, VM reset/shutdown/drop, invocation termination, or normal return retires the operation exactly once. The embedding owns pending futures: retiring a call drops its transport and permit, and a late completion cannot re-enter the VM.

`request_timeout` is the total bound for a buffered request and does not apply to SSE. `max_stream_duration` is the host-controlled absolute total-duration bound for each SSE call. SSE computes one admission-time deadline from the smaller of `max_stream_duration` and optional positive `timeout_ms`; the script value can only shorten the call and cannot disable or extend the host maximum. DNS, TCP, TLS, active reads, callback execution, and callback waits all count against the same deadline. Embedding invocation retirement may terminate the call sooner. `stream_idle_timeout` remains a separate wait-for-network-progress bound and resets only after progress; periodic traffic cannot extend the total deadline. Network idle time excludes time spent inside the callback, while callback work remains inside the total deadline.

## Configuration defaults

`HttpConfig` uses explicit bounded defaults. Streaming byte limits and all timeout fields must remain positive:

| Field | Default | Purpose |
| --- | ---: | --- |
| `allowed_schemes` | `https` | Scheme allowlist; protocol-family checks still apply |
| `allowed_hosts` | empty | Destination host allowlist; empty denies every host |
| `allowed_ports` | empty | Destination port allowlist; empty denies every port |
| `allow_private_ips` | `false` | Reject private and other special-use addresses |
| `max_redirects` | 5 | Buffered/SSE redirect bound |
| `max_request_body_bytes` | 1 MiB | Request body bound |
| `max_response_body_bytes` | 8 MiB | Buffered response body bound |
| `connect_timeout` | 10 s | DNS/connect/TLS phase bound |
| `request_timeout` | 30 s | Buffered request total duration |
| `max_stream_item_bytes` | 1 MiB | SSE event bound |
| `max_stream_total_bytes` | 64 MiB | Entire-call cumulative received event-stream byte bound |
| `max_sse_line_bytes` | 64 KiB | SSE line bound |
| `max_stream_duration` | 5 min | Host maximum total duration for SSE calls |
| `stream_idle_timeout` | 30 s | Wait-for-network-data bound |

The shared in-flight connection default is 64. Zero values for streaming byte limits or any timeout are invalid configuration; buffered `max_request_body_bytes` and `max_response_body_bytes` may be zero to prohibit request or response payload bytes. `HttpConfig::default()` allows `https`. Embeddings should set explicit host and port allowlists and add `http` only when cleartext transport is required. Buffered HTTP and SSE accept only `http`/`https`.

## Destination policy and protocol transports

Every protocol uses the same admission, address-pinning, and security policy:

- URLs require a host and reject userinfo;
- both the protocol's scheme family and the configured scheme allowlist must admit the URL;
- host and effective port must match their configured allowlists;
- every DNS result is validated, and the selected validated address is pinned for the connection;
- when private addresses are disabled, private, loopback, link-local, multicast, unspecified, documentation, transition, reserved, and other special-use IPv4/IPv6 ranges are rejected; IPv4-mapped IPv6 addresses receive the IPv4 checks;
- the original validated hostname remains the TLS SNI name and HTTP `Host` authority when connecting to a pinned address;
- buffered HTTP and SSE revalidate every redirect and remove `Authorization` and `Cookie` on a cross-origin redirect;
- ambient proxy settings are ignored. There is no implicit cookie jar, authentication source, or global proxy state.

The policy snapshot taken at call admission applies for the complete operation.

Buffered HTTP and SSE use direct Hyper HTTP/1 over Tokio/Rustls connections and perform no independent DNS lookup outside the shared admission and pinning path.

## Deliberately absent APIs and semantics

RustScript core provides no script-visible HTTP request ID, response/stream handle, `next`, `next_event`, or `cancel` callable. Streams cannot detach from their caller. There is no multiplexing, background reader, automatic reconnect, provider/model interpretation, agent loop, or platform retry policy. Applications implement provider-specific JSON, `[DONE]`, tool-call deltas, retry rules, and reconnect decisions in RSS or downstream hosts.

## Cancellation migration

PR #13 introduced HTTP-private pending-operation and abort-handle maps, one abort pair per request, HTTP owner routes, request-local runtimes, and HTTP-synthesized cancellation errors. The callable streaming contract supersedes those mechanisms. Buffered requests and SSE submit ordinary futures through the embedding-owned async bridge; HTTP has no private pending map, abort map, operation-ID namespace, token owner route, or cancellation state machine.

The generic `src/builtins/runtime/cancellation.rs` remains for non-HTTP runtime callers. HTTP does not depend on `CancellationToken`, `CancellationReason`, `OperationOwner::Http`, or owner-wide cancellation routing. Embedding-owned retirement of a pending future remains VM lifecycle control and rejects late completion; dropping an `Invocation` also retires active producer/callback waits and returns the VM and connection permit for reuse. This lifecycle cleanup is not an HTTP API-level cancellation facility.

## Target and backend notes

The callable pump follows the ordinary host-call suspension boundary for interpreter, Trace JIT, and whole-program AOT execution. Network futures remain outside VM execution, and callback frames use the same wait/yield continuation rules across backends.

`pd-vm-nostd` retains callable metadata and static builtin IDs without including HTTP transport implementations. WebAssembly and other embeddings can expose host imports only when that embedding supplies the capability, policy configuration, and async driving required by this contract. The contract does not imply an HTTP backend on targets where the host has not provided one.

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
