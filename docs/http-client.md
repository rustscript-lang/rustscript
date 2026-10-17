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

Native hosts expose four fully typed guest resources: `http.request`,
`http.response`, `http.headers`, and `http.sse_summary`. HTTP declares no host
named structs and accepts no map request/callback compatibility surface.
`HttpConfig`, `HttpExtension`, `HttpHostExt`, `register_http_builtin_module`, and
`http_host_catalog` provide embedding policy, registration, and metadata.
Catalog fingerprints use format v3; VMBC and the internal bytecode ABI remain 13.

Both client calls are ordinary `#[pd_host_function] async fn` declarations. The
embedding must install and drive a `HostAsyncBridge` on a live Tokio runtime.
The generated wrappers capture owned policy, request, and callback state before
submission; no VM borrow crosses an await. Builder ownership is consumed once,
including when synchronous admission rejects the call. A buffered result is
inserted on the VM thread by `HostFutureOutput::complete`.

```rust
use http;
let mut request = http::request::new("POST", "https://example.test/v1/messages");
http::request::set_header(&mut request, "content-type", "application/json");
http::request::set_body_text(&mut request, "{}");
let response = http::client::request(request);
let status = http::response::status(&response);
let body = http::response::body(&response);
let final_url = http::response::url(&response);
let names = http::response::header_names(&response);
let values = http::response::header_values(&response, "set-cookie");
```

A request builder accepts `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, or
`OPTIONS`. `set_body_bytes` takes bytes; `set_body_text` takes a string. Mutators
retain the builder's configured limits, and sending revalidates against the
current HTTP policy. Headers are ordered entries, preserving duplicates and
within-name value order. `Host`, `Content-Length`, `Transfer-Encoding`, and
`Connection` are client-managed request headers. Response header names are
normalized and canonically sorted. Each accessor string maps one raw octet to
one Unicode scalar U+0000..U+00FF; encoding those scalars back to octets recovers
the original bytes, including non-UTF-8 headers. The request and response bodies,
response head, redirects, concurrency, and elapsed phases are bounded.

## Server-sent events

SSE consumes the same request builder, restricted to `GET` or `POST`.
`Accept: text/event-stream` is added if absent. A successful response must have
an event-stream content type. The four overloads are:

```text
sse(request, on_event)
sse(request, on_event, timeout_ms)
sse(request, on_event, on_open)
sse(request, on_event, on_open, timeout_ms)
```

`on_event` has type `fn(string, string, string, string) -> bool`: event name,
data, last event ID, and decimal retry interval. Missing event name, ID, or retry
are empty strings. `on_open` has type
`fn(int, resource<http.headers>, string) -> bool`: status, independently owned
immutable headers, and final URL. Both callback kinds return `true` to continue
or `false` to stop. The open callback, when present, runs before all events;
there is no synthetic EOF callback. Clean EOF returns the summary after the last
event callback. Timeout must be positive and can only shorten the host maximum.

```rust
use http;
fn on_event(event: string, data: string, id: string, retry: string) -> bool {
    data != "[DONE]"
}
fn on_open(status: int, headers: resource<http.headers>, url: string) -> bool {
    status == 200
}
let request = http::request::new("GET", "https://example.test/events");
let summary = http::client::sse(request, on_event, on_open, 30000);
let outcome = http::sse_summary::outcome(&summary);
let headers = http::sse_summary::headers(&summary);
let content_types = http::headers::values(&headers, "content-type");
```

SSE parsing accepts an optional UTF-8 BOM and CRLF, CR, or LF lines. Repeated
`data:` fields join with newlines. A blank line dispatches only when data was
seen; partial events at EOF are discarded. Event names reset per dispatch,
while valid IDs and nonnegative decimal retry values persist. Comments and
unknown fields are ignored. Malformed UTF-8 and line, event, or entire-call byte
limit violations are host errors. There is no automatic reconnection; provider
markers such as `[DONE]` remain ordinary data.

## Terminal summaries and errors

`http::client::sse` returns an immutable `resource<http.sse_summary>`. Borrow it
through `http::sse_summary::{outcome,status,headers,url,items,bytes_received,bytes_sent}`.
`outcome` is `"eof"` or `"stopped"`; `items` counts delivered event callbacks,
excluding the open callback. Header access returns an independently owned
`http.headers` resource. `bytes_sent` counts each transmitted body across
redirects, including preserved POST bodies; rewritten GET hops contribute zero.
`bytes_received` counts received application body bytes. Limits use their own
entire-call accounting. Policy, transport, timeout, parser, and callback errors
never become successful summaries. Completion materializes and validates the
resource on the VM thread and rolls back new resources if publication fails.

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
| `max_request_header_count` | 100 | Caller-supplied request header field-count bound |
| `max_request_header_bytes` | 64 KiB | Serialized caller-supplied request header block bound |
| `max_response_body_bytes` | 8 MiB | Buffered response body bound |
| `connect_timeout` | 10 s | DNS/connect/TLS phase bound |
| `request_timeout` | 30 s | Buffered request total duration |
| `max_stream_item_bytes` | 1 MiB | SSE event bound |
| `max_stream_total_bytes` | 64 MiB | Entire-call cumulative received event-stream byte bound |
| `max_sse_line_bytes` | 64 KiB | SSE line bound |
| `max_stream_duration` | 5 min | Host maximum total duration for SSE calls |
| `stream_idle_timeout` | 30 s | Wait-for-network-data bound |

The shared in-flight HTTP call default is 64. Zero values for streaming byte limits or any timeout are invalid configuration; buffered `max_request_body_bytes` and `max_response_body_bytes` may be zero to prohibit request or response payload bytes. `HttpConfig::default()` allows `https`. Embeddings should set explicit host and port allowlists and add `http` only when cleartext transport is required. Buffered HTTP and SSE accept only `http`/`https`.

## Destination policy and protocol transports

Every protocol uses the same admission and connection-time address-validation
policy:

- URLs require a host and reject userinfo;
- both the protocol's scheme family and the configured scheme allowlist must admit the URL;
- host and effective port must match their configured allowlists;
- admission resolves the target and validates every returned address; when Hyper opens a connection, its connector resolves the hostname again and validates every address from that lookup before allowing a connect;
- when private addresses are disabled, private, loopback, link-local, multicast, unspecified, documentation, transition, reserved, and other special-use IPv4/IPv6 ranges are rejected; IPv4-mapped IPv6 addresses receive the IPv4 checks;
- the admitted hostname remains the TLS SNI name and HTTP `Host` authority; the address selected during admission is not pinned to the connection;
- buffered HTTP and SSE revalidate every redirect and remove `Authorization` and `Cookie` on a cross-origin redirect;
- ambient proxy settings are ignored. There is no implicit cookie jar, authentication source, or global proxy state.

The policy snapshot taken at call admission applies for the complete operation.
Every request and redirect target receives an admission-time DNS/private-address
check. Hyper's connector performs its own lookup whenever it opens a new
connection and rejects the lookup if any returned address is disallowed. This
prevents a target from reaching a disallowed private address through DNS
rebinding without claiming that the admission-selected address is the one used
for the socket.

Buffered HTTP and SSE share one cloneable Hyper client stored in per-VM HTTP
module state. Hyper owns HTTP/1 transport setup, connection pooling, idle
connection lifecycle, and pooled-connection retry behavior. VM reset/reuse
retains this library client and its pool; HTTP configuration replacement builds
a new client for the new policy snapshot. The host does not maintain a custom
pool, sender cache, connection worker, private Tokio runtime, or reconnect state
machine.

## Deliberately absent APIs and semantics

The guest resource tokens represent request builders and immutable buffered values; they provide no live stream handle, request ID, `next`, `next_event`, or `cancel` callable. Streams cannot detach from their caller. There is no multiplexing, background reader, automatic reconnect, provider/model interpretation, agent loop, or platform retry policy. Applications implement provider-specific JSON, `[DONE]`, tool-call deltas, retry rules, and reconnect decisions in RSS or downstream hosts.

## Async ownership and cancellation

Buffered requests and SSE opening run as macro-owned async host futures. HTTP has
no private pending map, abort map, operation-ID namespace, token owner route, or
cancellation state machine. After an SSE response opens, only the generic
callable-stream driver retains the Hyper response body, parser, deadlines,
callback continuation, and in-flight permit needed for callback re-entry and
backpressure.

The generic `src/builtins/runtime/cancellation.rs` remains for non-HTTP runtime
callers. Embedding-owned retirement drops a pending HTTP future and rejects late
completion. Dropping an `Invocation` also retires active producer/callback waits
and returns the VM and connection permit for reuse. This lifecycle cleanup is
not an HTTP API-level cancellation facility.

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
