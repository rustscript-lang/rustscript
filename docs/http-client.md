# HTTP client callable contract

RustScript exposes HTTP as bounded host imports. The current buffered request API returns one complete response. The target streaming APIs keep one ordinary host call active and invoke a script callable for each protocol item; they expose no response stream or socket object.

The embedding must configure destination policy and grant each available callable explicitly. HTTP configuration and capability bindings are snapshotted when a call is admitted, so later profile or configuration changes cannot widen an active connection.

**Milestone boundary:** At this documentation baseline, the script-facing HTTP API is limited to buffered `http::client::request`. Milestones 1–4 establish the typed callback admission, generic callable stream pump, and shared connection policy used by streaming transports. The SSE and WebSocket sections below define the target contracts delivered by Milestones 5 and 6, respectively.

## Capabilities and profiles

Across the complete Milestones 1–6 contract, the three imports are independent capabilities:

- `http::client::request`
- `http::client::sse`
- `http::client::websocket`

Granting `http::client::request` does not grant either streaming protocol, and granting one streaming protocol does not grant the other. A restricted host-function profile must allow every imported callable used by the program. Profiles remain isolated: a grant or configuration in one VM/profile does not authorize another.

Each available API is a host import gated by the HTTP client feature. The complete three-import contract does not consume or change static builtin IDs. See [Script call frames and callable values](callable-runtime.md) for callable execution and backend behavior.

## Buffered requests

```rust
use http;
use bytes;

let response = http::client::request({
    "method": "POST",
    "url": "https://example.test/v1/messages",
    "headers": {"content-type": "application/json"},
    "body": bytes::from_utf8("{}"),
});
```

`http::client::request(request)` accepts a map with:

- `method`: one of `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, or `OPTIONS`;
- `url`: an `http` or `https` URL admitted by host policy;
- `headers`: an optional string-to-string map;
- `body`: optional bytes or a string.

The response is buffered under the configured response-body limit and returned as:

```rust
{
    "status": 200,
    "headers": {"content-type": "application/json"},
    "body": bytes,
    "url": "https://example.test/v1/messages",
}
```

`url` is the final validated URL after redirects. The request body, response body, response head, redirect count, concurrent connection count, connect phase, and total request duration are bounded. `Host`, `Content-Length`, `Transfer-Encoding`, and `Connection` are client-managed request headers. A limit, policy, transport, TLS, redirect, or timeout failure is a host error and produces no response map.

## Server-sent events

> **Milestone 5 target contract:** `http::client::sse` becomes script-facing when the SSE protocol adapter is integrated; it is not part of the Milestones 1–4 API surface.

```rust
fn on_sse(item: map) -> map {
    if item["kind"] == "event" {
        print(item["data"]);
    }
    return {"action": "continue"};
}

let result = http::client::sse({
    "method": "GET",
    "url": "https://example.test/events",
    "headers": {"accept": "text/event-stream"},
}, on_sse);
```

`http::client::sse(request, on_event)` uses this request map:

| Field | Required | Accepted type and value | Bound or policy |
| --- | --- | --- | --- |
| `method` | yes | string: `GET` or `POST` | Other methods are rejected before transport admission |
| `url` | yes | string containing an `http` or `https` URL | Protocol family and the configured scheme, host, port, and address policy must all admit it |
| `headers` | no | map from string header names to string values | Names and values must be syntactically valid; client-managed request headers remain forbidden, and `Accept: text/event-stream` is supplied when absent |
| `body` | no | bytes or string, including for `POST` | Bounded by `max_request_body_bytes` |
| `timeout_ms` | no | positive integer milliseconds | Optional shortening deadline; its externally supplied embedding/host ceiling is outside `HttpConfig` |

The callback schema is `fn(map) -> map`, and the response must have an event-stream content type. The response head remains bounded by the existing HTTP parser. The target contract adds no configurable request-header byte accounting.

The callback receives exactly one map at a time, in this order:

```rust
// The response was accepted; this precedes every event.
{
    "kind": "open",
    "status": 200,
    "headers": map,
    "url": string,
}

// One parsed event. Absent optional fields are null.
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

The callback must return one of:

```rust
{"action": "continue"}
{"action": "stop"}
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

## WebSocket sessions

> **Milestone 6 target contract:** `http::client::websocket` becomes script-facing when the WebSocket protocol adapter is integrated; it is not part of the Milestones 1–4 API surface.

```rust
fn on_socket(item: map) -> map {
    if item["kind"] == "open" {
        return {"action": "send_text", "text": "hello"};
    }
    if item["kind"] == "text" && item["text"] == "done" {
        return {"action": "close", "code": 1000, "reason": "complete"};
    }
    return {"action": "continue"};
}

let result = http::client::websocket({
    "url": "wss://example.test/socket",
    "headers": {"authorization": "Bearer ..."},
    "protocols": ["example.v1"],
}, on_socket);
```

`http::client::websocket(request, on_event)` uses this request map:

| Field | Required | Accepted type and value | Bound or policy |
| --- | --- | --- | --- |
| `url` | yes | string containing a `ws` or `wss` URL | Protocol family and the configured scheme, host, port, and address policy must all admit it |
| `headers` | no | map from string header names to string values | Names and values must be syntactically valid; client-managed upgrade headers are rejected |
| `protocols` | no | array of syntactically valid subprotocol strings | The peer-selected subprotocol must match this offered list |
| `timeout_ms` | no | positive integer milliseconds | Optional shortening deadline; its externally supplied embedding/host ceiling is outside `HttpConfig` |

The callback schema is `fn(map) -> map`. The handshake response head remains bounded by the existing HTTP parser. The target contract adds no configurable request-header byte accounting.

After a validated `101` upgrade, the callback receives `open`, followed by incoming protocol items:

```rust
{
    "kind": "open",
    "status": 101,
    "headers": map,
    "url": string,
    "protocol": string | null,
}
{"kind": "text", "text": string}
{"kind": "binary", "data": bytes}
{"kind": "ping", "data": bytes}
{"kind": "pong", "data": bytes}
{"kind": "close", "code": int | null, "reason": string}
```

The callback returns exactly one action map:

```rust
{"action": "continue"}
{"action": "stop"}
{"action": "send_text", "text": string}
{"action": "send_binary", "data": bytes}
{"action": "ping", "data": bytes}
{"action": "pong", "data": bytes}
{"action": "close", "code": int, "reason": string}
```

Action rules depend on the current item:

- `open`, `text`, and `binary` permit every declared action;
- on `ping`, `continue` sends the required pong with the same payload; explicit `pong`, `stop`, and `close` are also valid, while application-data sends are rejected;
- on `pong`, `continue`, `ping`, `stop`, and `close` are valid;
- peer `close` is delivered once. `continue` sends the matching acknowledgment, `close` may provide its code and reason, and `stop` drops locally; each action then terminates the call;
- `stop` drops the connection locally and does not synthesize a close callback;
- local `close` sends one close frame and waits only through the configured close-handshake timeout.

Exactly one callback action is applied before another inbound item is polled. Fragmented text or binary frames are reassembled into one callback item under the message limit. Frame, message, control payload, outbound payload, and close code/reason limits apply. For WebSocket, `max_stream_total_bytes` counts combined text/binary application payload bytes sent and received across the entire call; frame and control overhead are bounded separately by protocol and frame caps. Invalid UTF-8 text, invalid actions, protocol violations, write failures, and abnormal transport EOF are host errors.

## Terminal summaries and errors

After callback processing terminates normally, either streaming call returns one summary:

```rust
{
    "outcome": "eof" | "stopped" | "closed",
    "status": int,
    "headers": map,
    "url": string,
    "items": int,
    "bytes_received": int,
    "bytes_sent": int,
}
```

SSE normally reports `eof` or `stopped`; WebSocket may also report `closed` after peer/local close semantics complete. `items` counts delivered callback items. `bytes_received` and `bytes_sent` are observational summary counters. Limit enforcement uses independent entire-call accounting and does not depend on whether or how these counters are displayed.

Transport, parser, destination-policy, timeout, and callback failures stay errors. They are never converted into a successful terminal summary.

## Sequencing, backpressure, and lifecycle

Streaming is a single caller-owned operation:

1. the host polls for one protocol item;
2. the VM invokes `on_event` in a child script frame;
3. the callback returns one action;
4. the host applies that action before polling for another item.

At most one unacknowledged protocol item crosses the host/VM boundary. Decoder scratch space is bounded separately. The network future is not polled while the callback runs, yields, or waits in another async host call. If the callback yields or invokes an ordinary async host function, the callback resumes first; only its final action resumes the outer stream operation. This sequencing supplies backpressure without a background reader or callback queue.

The network future never owns or re-enters the VM. Callback error, protocol completion, configured deadline, VM reset/shutdown/drop, invocation termination, or normal return retires the operation exactly once. The embedding owns pending futures: retiring a call drops its transport and permit, and a late completion cannot re-enter the VM.

`request_timeout` is the total bound for a buffered request and does not apply to streaming. In the target streaming contract, `timeout_ms` is only an additional shortening deadline: it can never extend the embedding invocation deadline or another host bound. Its maximum is embedding-supplied policy outside `HttpConfig`; an implementation must cap a supplied value to that external maximum, or reject `timeout_ms` when no embedding/host maximum exists. Without `timeout_ms`, the embedding invocation deadline and stream idle timeout bound the operation. Network idle time excludes time spent inside the callback, while callback work remains subject to the embedding's invocation deadline. A script cannot disable or raise a host limit. WebSocket close waiting is additionally bounded by `websocket_close_timeout`.

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
| `max_stream_item_bytes` | 1 MiB | SSE event or WebSocket message bound |
| `max_stream_total_bytes` | 64 MiB | Entire-call cumulative application-byte bound: received event-stream bytes for SSE; sent plus received application payload for WebSocket |
| `max_sse_line_bytes` | 64 KiB | SSE line bound |
| `max_websocket_frame_bytes` | 1 MiB | WebSocket frame bound |
| `max_websocket_send_bytes` | 1 MiB | One WebSocket outbound action bound |
| `stream_idle_timeout` | 30 s | Wait-for-network-data bound |
| `websocket_close_timeout` | 5 s | Close-handshake wait bound |

The shared in-flight connection default is 64. Zero values for streaming byte limits or any timeout are invalid configuration; buffered `max_request_body_bytes` and `max_response_body_bytes` may be zero to prohibit request or response payload bytes. `HttpConfig::default()` allows only `https`, so an embedding must explicitly add `wss` before a secure WebSocket URL can pass admission. Embeddings should set explicit host and port allowlists and add `http`, `ws`, or `wss` only when those schemes are required. Buffered HTTP and SSE accept only `http`/`https`; WebSocket accepts only `ws`/`wss`. The recommended secure protocol families are `https` and `wss`; that recommendation does not widen the actual default allowlist.

## Destination policy and protocol transports

Every protocol uses the same admission, address-pinning, and security policy:

- URLs require a host and reject userinfo;
- both the protocol's scheme family and the configured scheme allowlist must admit the URL;
- host and effective port must match their configured allowlists;
- every DNS result is validated, and the selected validated address is pinned for the connection;
- when private addresses are disabled, private, loopback, link-local, multicast, unspecified, documentation, transition, reserved, and other special-use IPv4/IPv6 ranges are rejected; IPv4-mapped IPv6 addresses receive the IPv4 checks;
- the original validated hostname remains the TLS SNI name and HTTP `Host` authority when connecting to a pinned address;
- buffered HTTP and SSE revalidate every redirect and remove `Authorization` and `Cookie` on a cross-origin redirect;
- WebSocket does not automatically redirect. Any handshake path that supports redirects must route every hop through the same validation and cross-origin credential stripping before reconnecting;
- ambient proxy settings are ignored. There is no implicit cookie jar, authentication source, or global proxy state.

The policy snapshot taken at call admission applies for the complete operation.

Protocol transport remains separate from that shared policy. Buffered HTTP and the SSE target use reviewed direct Hyper HTTP/1 over Tokio/Rustls connections. The WebSocket protocol adapter may use Tungstenite over a prevalidated, pinned Tokio/Rustls stream; it must not perform an independent DNS lookup or connection outside the shared admission and pinning path.

## Deliberately absent APIs and semantics

RustScript core provides no script-visible HTTP request ID, response/stream/socket handle, `next`, `next_event`, `next_message`, `close`, or `cancel` callable. Streams cannot detach from their caller. There is no multiplexing, background reader, automatic reconnect, provider/model interpretation, agent loop, or platform retry policy. Applications implement provider-specific JSON, `[DONE]`, tool-call deltas, retry rules, and reconnect decisions in RSS or downstream hosts.

## Cancellation migration

PR #13 introduced HTTP-private pending-operation and abort-handle maps, one abort pair per request, HTTP owner routes, request-local runtimes, and HTTP-synthesized cancellation errors. The callable streaming contract supersedes those mechanisms. Buffered requests, SSE, and WebSocket submit ordinary futures through the embedding-owned async bridge; HTTP has no private pending map, abort map, operation-ID namespace, token owner route, or cancellation state machine.

The later generic `src/builtins/runtime/cancellation.rs` remains for non-HTTP runtime callers until their separate migration. HTTP does not depend on `CancellationToken`, `CancellationReason`, `OperationOwner::Http`, or owner-wide cancellation routing. Embedding-owned retirement of a pending future remains VM lifecycle control and rejects late completion; it is not an HTTP API-level cancellation facility.

## Target and backend notes

The callable pump follows the ordinary host-call suspension boundary for interpreter, Trace JIT, and whole-program AOT execution. Network futures remain outside VM execution, and callback frames use the same wait/yield continuation rules across backends.

`pd-vm-nostd` retains callable metadata and static builtin IDs without including HTTP transport implementations. WebAssembly and other embeddings can expose host imports only when that embedding supplies the capability, policy configuration, and async driving required by this contract. The contract does not imply an HTTP backend on targets where the host has not provided one.
