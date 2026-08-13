# Callable-Driven Streaming HTTP Client Implementation Plan

**Goal:** Extend RustScript's HTTP client with SSE and WebSocket streaming through script callables, without exposing script-visible request handles, `next_*`, `close`, or cancellation APIs.

**Architecture:** Keep `http::client::request(request)` as the bounded buffered request API. Add `http::client::sse(request, on_event)` and `http::client::websocket(request, on_event)` as long-running ordinary host calls. A generic VM stream pump alternates between polling one host-produced item and invoking one script callable; the callable's returned action controls continuation and WebSocket writes. Network futures never own or re-enter the VM, callback execution never polls the network stream, and the one-item handoff provides backpressure.

**Tech Stack:** Rust 2024, RustScript callable values and typed callable schemas, `HostAsyncBridge`, Reqwest/Tokio, `futures-util`, `tokio-tungstenite`, local TCP/SSE/WebSocket fixtures.

---

## Status and supersession

This plan supersedes the HTTP transport portion of `2026-08-09_http-transport-security-executor.md` after that plan established the generic async-host boundary and migrated the buffered client. It preserves that plan's host-driven executor, feature gating, destination policy, DNS pinning, redirect, deadline, byte-limit, and lifecycle cleanup requirements.

This plan deliberately removes script-visible cancellation from the HTTP client design. Internal VM lifecycle cleanup remains mandatory: reset, shutdown, drop, invocation termination, and configured deadlines must drop the stream operation and socket. That internal cleanup is not an HTTP callable and has no script-level request ID.

## PR #13 cancellation disposition

GitHub PR #13 (`feat(http): add a bounded cancellable host client`, head `475e5aa`) introduced the first HTTP client together with HTTP-private asynchronous operation ownership:

- `HttpState::pending_ops` and `HttpState::abort_handles`;
- one `AbortHandle`/`Abortable` pair per request;
- HTTP-specific `cancel_pending_op` and `cancel_all_pending_ops` routing;
- a private thread and Tokio runtime per request;
- cancellation errors synthesized by the HTTP subsystem.

Those mechanisms are superseded by this plan. Buffered HTTP, SSE, and WebSocket all submit ordinary futures through the embedding-owned async bridge. Reset, shutdown, drop, invocation termination, or deadline retirement removes and drops the submitted future; dropping the future releases the Reqwest response stream or WebSocket transport. HTTP does not retain a second pending-operation map, abort-handle map, operation-ID namespace, cancellation token tree, or cancellation error state machine.

The later `src/builtins/runtime/cancellation.rs` did not originate in PR #13. It was added by the unified host-runtime lifecycle work and currently also serves Invocation, IO, SQLite, resources, and the generic host bridge. This plan has the following boundary:

1. Remove every HTTP dependency on `CancellationToken`, `CancellationReason`, `OperationOwner::Http`, and `cancel_operations_by_owner`.
2. Use absolute protocol deadlines (`timeout_at`/equivalent), bounded idle timers, callback actions, protocol close/EOF, and future drop as HTTP terminal mechanisms.
3. Keep the embedding's ability to retire a pending future and reject a late completion. This is operation lifecycle control, not a script-visible HTTP cancellation facility.
4. Do not delete `cancellation.rs` while IO, SQLite, Invocation, resources, or the generic host bridge still import it.
5. After those non-HTTP callers are migrated, delete the generic cancellation-token tree and split any remaining responsibilities into run termination state, async-host operation identity, and resource-specific cleanup. That repository-wide simplification is a follow-up and must not be force-fitted into the HTTP transport implementation.

pd-edge did not need `cancellation.rs` for its async HTTP hosts: its generated async wrapper schedules a future through `SharedVmAsyncOps`/`schedule_current_future_call`, and the embedding owns that future's lifecycle. The same ownership model is the reference for this plan. RustScript core still guarantees exactly one terminal transition and prevents a dropped operation from re-entering the VM; it does not require a cooperative token check inside every network stage.

## Dependency order

1. The current callable implementation must remain frame-aware and able to pass closures/named functions as values.
2. The current host-driven async ABI remains the only executor boundary.
3. Implement the generic callable stream pump before either protocol adapter.
4. Implement SSE before WebSocket; SSE validates ordered delivery and backpressure without duplex command handling.
5. WebSocket reuses the same pump and adds callback-returned outbound actions.

No source-language `async`, iterator, generator, resource handle, or structured-task feature is required.

## Script-facing contract

### Buffered HTTP

```rust
use http;

let response = http::client::request({
    "method": "POST",
    "url": "https://example.test/v1/messages",
    "headers": {"content-type": "application/json"},
    "body": bytes::from_utf8("{}"),
});
```

The existing response map stays stable:

```rust
{
    "status": 200,
    "headers": {"content-type": "application/json"},
    "body": bytes,
    "url": "https://example.test/v1/messages",
}
```

### SSE

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

The callback receives exactly one map at a time:

```rust
// Response accepted, before the first event.
{
    "kind": "open",
    "status": 200,
    "headers": map,
    "url": string,
}

// One parsed SSE event. "event" is per-dispatch state, reset to null at
// every dispatch boundary (including a blank line that dispatches no
// event); "id" and "retry_ms" are persistent stream state, retaining the
// last valid values seen so far and null only before any value has been
// seen.
{
    "kind": "event",
    "event": string | null,
    "data": string,
    "id": string | null,
    "retry_ms": int | null,
}

// Clean EOF after all preceding events were acknowledged by the callback.
{"kind": "end"}
```

SSE parsing follows the event-stream grammar:

- UTF-8 text with an optional leading BOM;
- `\r\n`, `\r`, and `\n` line endings;
- repeated `data:` fields joined with `\n`, removing the final join newline at dispatch;
- `event`, `id`, and decimal non-negative `retry` fields;
- comment and unknown fields ignored;
- a blank line dispatches an event only when at least one `data:` field was seen;
- malformed UTF-8, an over-limit line, event, or cumulative stream is a host error;
- no automatic reconnection and no interpretation of provider-specific `[DONE]` values.

### WebSocket

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

The callback receives:

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

The callback returns one explicit action map:

```rust
{"action": "continue"}
{"action": "stop"}
{"action": "send_text", "text": string}
{"action": "send_binary", "data": bytes}
{"action": "ping", "data": bytes}
{"action": "pong", "data": bytes}
{"action": "close", "code": int, "reason": string}
```

Rules:

- `stop` ends locally and drops the connection without synthesizing a close callback;
- `close` sends one close frame, waits up to the configured close timeout for the peer close, then returns;
- on `open`, `text`, and `binary`, every declared action is allowed;
- on `ping`, `continue` sends the protocol-required pong with the same payload; `pong`, `stop`, and `close` are also allowed, while application data sends are rejected;
- on `pong`, `continue`, `ping`, `stop`, and `close` are allowed;
- peer close is delivered once; `continue` sends the matching close acknowledgment, `close` may supply the acknowledgment code/reason, and `stop` drops locally; all three then return;
- control-frame payload, close-code, close-reason, message, frame, and cumulative-byte limits are enforced;
- fragmented frames are reassembled into one text/binary callback item under the configured message limit;
- invalid UTF-8 text, protocol violation, invalid callback action, write failure, or abnormal transport EOF is a host error;
- no reconnect, multiplexing, background reader, or script-visible socket object.

### Return value

Both streaming calls return one terminal summary map after callback processing ends:

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

Transport, parser, policy, timeout, and callback errors remain errors; they are not converted into a successful summary.

## Hard invariants

1. No network future stores `&mut Vm`, `Store`, `ScriptCallback`, interpreter frame pointers, or a VM-owning closure.
2. No async future calls `Vm::invoke_callable`, `Vm::start_callable`, `Store::poll_callbacks`, or equivalent VM entry points.
3. At most one unacknowledged protocol item is retained between host and VM. Optional decoder scratch is bounded separately.
4. While a callback is running or waiting in another host call, the streaming network future is not polled. Callback completion is the backpressure acknowledgment.
5. The stream host call occupies the caller's existing pending call boundary; the callback runs as a child script frame and returns to that stream continuation.
6. The callback may yield or invoke ordinary async host functions. Nested waiting resumes the callback first, then returns its action to the stream pump.
7. Callback panic/error, VM shutdown, reset/drop, invocation termination, deadline, protocol terminal state, and normal completion retire the operation exactly once.
8. HTTP configuration and capability binding are snapshotted at call admission. Mutating later registry/config state cannot widen an active connection.
9. Existing `http::client::request` generated host identity, signature, result shape, and buffered semantics stay unchanged; it has no script-visible request ID.
10. Protocol-specific provider logic remains in RSS or downstream hosts.

## Configuration additions

Extend `HttpConfig` with explicit bounded defaults:

```rust
pub max_stream_item_bytes: usize;       // SSE event or WS message
pub max_stream_total_bytes: usize;      // entire streaming call
pub max_sse_line_bytes: usize;
pub max_websocket_frame_bytes: usize;
pub max_websocket_send_bytes: usize;
pub max_stream_duration: Duration;      // 5 minute host total-duration cap
pub stream_idle_timeout: Duration;
pub websocket_close_timeout: Duration;
```

`request_timeout` remains the total duration for buffered requests. Streaming calls have a positive `max_stream_duration` host limit, defaulting to 5 minutes. At admission, compute one absolute deadline from the smaller of that host limit and optional positive request-map `timeout_ms`; the script cannot disable or extend the host limit. The SSE implementation enforces this deadline while opening and reading. Milestone 6 WebSocket integration must apply the same field and capping rule. Embedding invocation retirement may still terminate a call sooner, and idle timeout remains a separate progress-based bound.

Allowed schemes are protocol-aware:

- buffered request and SSE accept `http`/`https` only;
- WebSocket accepts `ws`/`wss` only;
- `https` and `wss` remain the default public-network schemes;
- host/port/address policy is shared across all four schemes.

## Implementation route

### Milestone 1: Freeze callable-pump semantics with RED tests

**Objective:** Specify VM alternation, callback suspension, result delivery, and cleanup before transport code exists.

**Files:**
- Create: `tests/vm/host_stream_callback_tests.rs`
- Modify: `Cargo.toml`
- Modify: `src/vm/tests.rs`
- Modify: `src/vm/instance.rs`

**Tests:**

1. A synthetic host stream delivers three maps to a closure in order and returns its summary.
2. The producer is polled only after the preceding callback returns `continue`.
3. A callback that enters `VmStatus::Waiting` resumes and returns an action without corrupting the outer stream call frame.
4. A callback that yields resumes before the producer is polled again.
5. `stop` prevents a queued fourth producer item from being observed.
6. Callback type mismatch and invalid action abort the stream call with no stale frame, item, or operation.
7. reset/shutdown/drop from producer-wait and callback-wait phases release the operation and callable once.
8. Interpreter, JIT, and AOT enter the same stream continuation through the ordinary host-call boundary.

**RED command:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/target-http-stream \
  cargo test --locked --test host_stream_callback_tests --all-features
```

Expected: compile/test failure because the generic stream continuation does not exist.

**Commit:** `test(vm): define callable stream pump contract`

### Milestone 2: Add typed callable host parameters

**Objective:** Preserve `fn(map) -> map` in host metadata and reject incompatible handlers before network admission.

**Files:**
- Modify: `pd-host-function/src/lib.rs`
- Modify: `build.rs`
- Modify: `src/compiler/parser/symbols.rs`
- Modify: host callable metadata/signature structures generated by `build.rs`
- Modify: `tests/host_binding_generation_tests.rs`
- Modify: `tests/compiler/compiler_rustscript_tests.rs`

**Implementation:**

1. Add an owned callable argument wrapper used only for synchronous admission, for example `VmCallable`, whose extracted value must be `Value::Callable`.
2. Teach proc-macro and build-time type-label generation to encode callable parameters rather than `any`; include parameter and result schemas in `HostCallableSignature`.
3. Declare both streaming APIs as accepting `VmCallable<fn(map) -> map>` or equivalent generated schema metadata.
4. Reuse compiler contextual callable typing so named generic functions and closures specialize against the expected callback schema.
5. At binding/admission validate callable kind, arity, parameter schema, and result schema. Dynamic/unknown callables still receive runtime result validation.
6. Async wrappers may move the callable value into VM-owned stream state, but must not move it into the network future.

**Tests:**

- correct closure and named function compile;
- wrong arity, parameter type, and return type fail with callable-specific diagnostics;
- runtime-created unknown callable with a non-map action fails before a second producer poll;
- generated docs show `fn(map) -> map` for both APIs.

**Commands:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/target-http-stream \
  cargo test --locked -p pd-host-function
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/target-http-stream \
  cargo test --locked --test host_binding_generation_tests --all-features
```

**Commit:** `feat(host): preserve callable parameter schemas`

### Milestone 3: Implement the generic host-to-callable stream pump

**Objective:** Alternate one host item and one script callback using VM-owned continuation state.

**Files:**
- Create: `src/vm/async_host/stream.rs`
- Modify: `src/vm/async_host/mod.rs`
- Modify: `src/vm/instance.rs`
- Modify: `src/vm/mod.rs`
- Modify: `src/vm/host.rs`
- Modify: `src/vm/host_runtime.rs`
- Modify: `src/vm/native/bridge.rs`
- Modify: JIT/AOT host-call exit handling only where conformance tests expose a gap
- Modify: `tests/vm/host_stream_callback_tests.rs`

**Core state machine:**

```text
AwaitItem
  -> ItemReady(item)
  -> RunCallback(item)
  -> CallbackWaiting | CallbackYielded | ActionReady(action)
  -> ApplyAction(action)
  -> AwaitItem | Complete(summary) | Error
```

**Implementation:**

1. Introduce a generic host stream operation trait/envelope whose driver poll returns exactly one of `Pending`, `Item(Value)`, `Complete(Value)`, or `Error(VmError)`.
2. Store the callback value, current item, phase, and parent host-call continuation in VM instance state; store transport/decoder state in the host driver.
3. Extend host-operation output with a stream item boundary. Do not represent each item as a completed operation ID.
4. When an item arrives, start the callable with `FrameContinuation::ReturnToHost` adapted to resume the stream state rather than halting the whole outer call.
5. If the callable waits or yields, retain the outer stream continuation and resume the callable through existing VM machinery.
6. Validate the returned map through a protocol-supplied action decoder, apply the action to the driver, then permit the next producer poll.
7. On every terminal path, remove stream state, callback, item, pending write, and operation ownership exactly once.
8. Keep ordinary async host futures and invocation item streams unchanged.

**GREEN command:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/target-http-stream \
  cargo test --locked --test host_stream_callback_tests --all-features
```

Expected: all callable-pump contract tests pass.

**Commit:** `feat(vm): add host-driven callable stream pump`

### Milestone 4: Factor shared HTTP connection policy

**Objective:** Make buffered HTTP, SSE, and WebSocket use one admission and connection-security path.

**Files:**
- Create: `src/builtins/runtime/http/config.rs`
- Create: `src/builtins/runtime/http/policy.rs`
- Create: `src/builtins/runtime/http/request.rs`
- Move/Modify: `src/builtins/runtime/http.rs` to `src/builtins/runtime/http/mod.rs`
- Modify: `src/builtins/runtime/cancellation.rs`
- Modify: `build.rs`
- Modify: `src/builtins/runtime/mod.rs`
- Modify: `tests/vm/http_host_tests.rs`

**Implementation:**

1. Move request parsing, header restrictions, URL/userinfo validation, DNS lookup, special-address rejection, address pinning, redirect validation, credential stripping, permit accounting, and deadline helpers into shared modules.
2. Parameterize allowed scheme families so all protocols use the same host/port/address decisions.
3. Add the bounded streaming configuration fields and validation; zero limits are rejected during configuration.
4. Preserve buffered request behavior byte-for-byte, including final URL and response header conversion.
5. Keep ambient proxies disabled. Do not add cookie jars, automatic auth, or global connection state.
6. Add policy tests proving `ws`/`wss` cannot bypass host, port, private-address, or DNS pinning rules.
7. Gate HTTP runtime modules, dependencies, generated metadata, and registration on `http-client`; `async` alone must not publish HTTP callables.
8. Replace the current single-file `build.rs` HTTP source entry with explicit `CARGO_FEATURE_HTTP_CLIENT` DefaultHost source entries for `http/mod.rs`, `http/sse.rs`, and `http/websocket.rs`; build-time callable discovery is not recursive.
9. Remove `HttpRequestContext.cancellation`, HTTP `CancellationToken` parameters/checks, `OperationOwner::Http`, and HTTP owner-wide cancellation routing. Do not create replacement HTTP abort handles or a private pending-operation registry.
10. Wrap buffered request execution in one absolute deadline and let the embedding-owned future release DNS, connect, response body, permit, SSE, and WebSocket state when retired.
11. Preserve `cancellation.rs` for its remaining non-HTTP production callers. Whole-file deletion requires a separate verified migration of Invocation, IO, SQLite, resources, and generic host-bridge state.

**Commands:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/target-http-stream \
  cargo test --locked --test http_host_tests --features http-client
```

Expected: existing buffered tests plus shared-policy tests pass.

The HTTP tests must additionally prove that reset/shutdown/drop retire an active buffered request through the async bridge, release its in-flight permit, ignore late completion, and leave no HTTP-owned abort handle, token, or pending-op entry.

**Commit:** `refactor(http): share bounded connection policy`

### Milestone 5: Add callable-driven SSE

**Objective:** Parse bounded event streams and deliver one normalized event per callback invocation.

**Files:**
- Create: `src/builtins/runtime/http/sse.rs`
- Modify: `src/builtins/runtime/http/mod.rs`
- Modify: `build.rs`
- Modify: `Cargo.toml`
- Modify: `tests/vm/http_host_tests.rs`
- Create: `tests/vm/http_sse_tests.rs`

**Implementation:**

1. Register `http::client::sse` as a feature-gated DefaultHost callable alongside `http::client::request`. Host imports use the registry/profile binding path and do not consume static builtin IDs.
2. Admit only GET/POST requests using `http`/`https`; set `Accept: text/event-stream` when absent and reject a response whose content type is not `text/event-stream`.
3. Reuse shared redirects and connection policy before exposing `open`.
4. Decode chunks incrementally with bounded line and event buffers; split UTF-8 only after full code points are available.
5. Emit `open`, parsed `event` items, and `end` through the generic pump. Do not buffer the whole body.
6. Decode callback maps into only `continue` or `stop`; all other actions are errors.
7. Track wire bytes and delivered item count for the terminal summary.
8. Apply the admission-time absolute deadline continuously while opening and reading, using `min(max_stream_duration, timeout_ms)` when `timeout_ms` is supplied. Apply idle timeout separately while waiting for bytes. Time spent inside the callback is excluded from network idle accounting but remains subject to the embedding invocation deadline.

**Tests:**

- events split across arbitrary chunks and UTF-8 boundaries;
- CR/LF variants, BOM, comments, repeated data, id, event, retry, and clean EOF;
- content-type rejection, malformed UTF-8, oversized line/event/total stream;
- callback ordering, callback async wait, callback stop, callback error;
- no producer poll while callback is active;
- redirect revalidation and stripped credentials;
- disconnect before complete event and clean EOF after complete event;
- permit and operation cleanup on every terminal path.

**Command:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/target-http-stream \
  cargo test --locked --test http_sse_tests --features http-client
```

**Commit:** `feat(http): add callable-driven SSE client`

### Milestone 6: Add callable-driven WebSocket

**Objective:** Support bounded full-duplex WebSocket sessions where callback return maps serialize all outbound actions.

**Files:**
- Create: `src/builtins/runtime/http/websocket.rs`
- Modify: `src/builtins/runtime/http/mod.rs`
- Modify: `build.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Create: `tests/vm/http_websocket_tests.rs`

**Implementation:**

1. Add optional `tokio-tungstenite`/`tungstenite` dependencies to `http-client`; disable connector features that create a second policy path and select the Rustls integration compatible with current Reqwest TLS.
2. Register `http::client::websocket` as a feature-gated DefaultHost callable. Keep discovery, generated metadata, direct binding, cached binding, and capability-profile admission equivalent.
3. Parse `ws`/`wss`, headers, and optional subprotocols. Reject userinfo and client-managed upgrade headers.
4. Resolve and validate the destination through shared policy, connect to the validated address, and preserve the original hostname for TLS/SNI and HTTP Host.
5. Perform the upgrade with no automatic redirect. If redirect support is retained by the selected handshake path, each redirect must re-enter shared validation and credential stripping before reconnecting.
6. Validate status 101 and selected subprotocol before emitting `open`.
7. Convert incoming protocol messages to one callback item. Reassemble fragmented text/binary messages under limits.
8. Decode callback action maps. Apply exactly one outbound action before polling another inbound item; bound send payload and cumulative bytes.
9. Handle ping/pong explicitly through callback actions. The implementation may send protocol-required close acknowledgments automatically, but must not hide application data messages.
10. Implement the close handshake and configured timeout; return `closed` only after local/peer close semantics are satisfied.
11. Drop the connection on `stop`, callback error, VM lifecycle teardown, deadline, or protocol error without invoking a callback after terminal state.

**Tests:**

- handshake, headers, SNI/Host, and subprotocol selection;
- text/binary and fragmented message delivery;
- send_text/send_binary/ping/pong/close action ordering;
- peer close and local close handshake;
- stop, invalid action, invalid close code/reason, oversized frame/message/send/total bytes;
- idle timeout and abnormal EOF;
- host/port/private-address/DNS policy cannot be bypassed;
- callback waits/yields without concurrent socket polling;
- reset/shutdown/drop cleanup and no late callback.

**Command:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/target-http-stream \
  cargo test --locked --test http_websocket_tests --features http-client
```

**Commit:** `feat(http): add callable-driven WebSocket client`

### Milestone 7: Capability, docs, and backend conformance

**Objective:** Make the two new calls explicit capabilities and document their bounded callable contract.

**Files:**
- Create: `docs/http-client.md`
- Modify: `docs/callable-runtime.md`
- Modify: `README.md`
- Modify: `tests/host_binding_generation_tests.rs`
- Modify: the existing VM backend parity tests that own host-call suspension

**Implementation:**

1. Treat each callable as an independent capability:
   - `http::client::request`
   - `http::client::sse`
   - `http::client::websocket`
2. A profile granting buffered HTTP does not grant either streaming protocol.
3. Document callback schemas, action maps, terminal summaries, limits, and lifecycle behavior.
4. Document that API-level cancellation, handles, detached streams, reconnection, and provider semantics are absent.
5. Document the PR #13 migration: HTTP-private abort handles and the later generic cancellation-token dependency are removed, while embedding-owned future retirement remains part of VM lifecycle.
6. Verify interpreter/JIT/AOT callable-pump parity and keep no-std builds free of HTTP implementations. Existing static builtin IDs remain untouched because these APIs are host imports.

**Commands:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/target-http-stream \
  cargo test --locked --test host_binding_generation_tests --all-features
```

Keep capability-profile cases in the existing `host_binding_generation_tests` target; do not create a duplicate integration target solely for these cases.

**Commit:** `docs(http): define streaming callable contract`

### Milestone 8: Full verification and cleanup

Run all commands with generated output and target directories under `/mnt/TEMP/rustscript/`:

```bash
cargo fmt --all -- --check
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/target-http-stream cargo test --locked -p pd-host-function
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/target-http-stream cargo test --locked --test host_binding_generation_tests --all-features
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/target-http-stream cargo test --locked --test host_stream_callback_tests --all-features
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/target-http-stream cargo test --locked --test http_host_tests --features http-client
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/target-http-stream cargo test --locked --test http_sse_tests --features http-client
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/target-http-stream cargo test --locked --test http_websocket_tests --features http-client
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/target-http-stream cargo test --locked --workspace --all-features
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/target-http-stream cargo test --locked --workspace --no-default-features --tests --no-run
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/target-http-stream cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/target-http-stream RUSTDOCFLAGS='-D warnings' cargo doc --locked --workspace --all-features --no-deps
git diff --check
```

Then remove `/mnt/TEMP/rustscript/target-http-stream` and any local fixture output.

## Rejected designs

### Script-visible response/socket handles

Rejected because they require handle ownership, repeated `next_*` calls, close/cancel races, detached-resource policy, and cleanup across arbitrary script control flow.

### Script-visible `cancel(request_id)`

Rejected because the stream call already has deterministic terminal paths: callback action, peer EOF/close, timeout, VM lifecycle termination, or error. An extra request-ID namespace adds race states without enabling SSE/WebSocket delivery.

### Network future directly invoking the callable

Rejected because it would re-enter a mutable VM from a future that is itself being driven for that VM, allow concurrent interpreter/network progress, and bypass the existing frame/wait/yield state machine.

### Unbounded callback queue

Rejected because it disconnects network pressure from script processing and allows a fast peer to consume host memory while the callback is waiting.

### Provider-specific SSE parsing in core

Rejected because `[DONE]`, tool-call deltas, retry policy, and provider JSON ownership belong to RSS/downstream code.

## Target criteria

- Buffered `http::client::request` remains compatible and bounded.
- RSS can process SSE events and WebSocket messages before connection EOF.
- Streaming uses callable values with an explicit `fn(map) -> map` contract.
- A callback can wait or yield and then resume the outer stream call correctly.
- One-item alternation proves backpressure; no unbounded callback queue exists.
- Scripts receive no HTTP request ID, stream/socket handle, `next_*`, `close`, or cancel callable.
- HTTP owns no pending-operation map, abort-handle map, `CancellationToken`, `OperationOwner::Http`, or owner-wide cancellation route.
- Async bridge retirement drops active HTTP/SSE/WebSocket futures, rejects late completion, and releases permits and transports exactly once.
- SSE and WebSocket terminal summaries are deterministic and delivered once.
- Every network path enforces scheme, host, port, DNS/address, TLS hostname, redirect, header, byte, idle, and duration policy.
- VM reset/shutdown/drop and invocation termination release active streams internally with no late callback.
- Each streaming protocol is independently capability-gated.
- No provider, model, agent-loop, reconnect, or platform policy enters RustScript core.
- All temporary output is removed after verification.
