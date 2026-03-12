# pd-edge HTTP/2 Support Plan

## Summary

Add real HTTP/2 support to `pd-edge` by separating generic HTTP exchange semantics from version-specific transport realizations.

The key design constraint is that HTTP/2 must not be modeled as "HTTP/1.1 with a different wire format". It needs its own session DAG, its own stream DAGs, and shared session state so multiple VM-visible exchanges can multiplex over one upstream connection.

The recommended model is:

- `http` = protocol-independent request/response exchange semantics
- `http1` = one realization of `http`
- `http2` = another realization of `http`, with a session DAG plus many child stream DAGs

That gives the three properties this effort needs:

- HTTP/2 can attach below `http` and above TCP or TLS without being fused to either layer
- HTTP/2 can multiplex many streams over one session
- HTTP/2 streams can detach into the same generic `http` exchange layer that HTTP/1.1 uses

## Goals

- Support upstream HTTP/2 as a first-class protocol family, not an opaque client implementation detail.
- Preserve the existing VM-facing `http::exchange::*` model as the stable semantic API for request and response work.
- Add real multiplexing so multiple exchanges can share one HTTP/2 session.
- Allow HTTP/2 to attach to TLS plaintext after ALPN `h2` or directly to TCP for cleartext prior-knowledge `h2c`.
- Make downgrade or alternate realization explicit: a generic `http` exchange may be carried by `http1` or `http2`.
- Keep the design consistent with the current DAG model in [pd-edge/README.md](pd-edge/README.md).

## Non-Goals For The First Milestone

- HTTP/2 WebSocket support via RFC 8441 extended CONNECT
- HTTP/3 or QUIC transport work
- Full downstream connection-scoped VM hosting for many concurrent streams on one persistent client session
- Exposing every HTTP/2 frame primitive to VM code
- Replacing the existing `http::exchange::*` ABI with a version-specific surface

## Current State

The code now has a generic `http::exchange::*` ABI in [pd-edge-abi/src/abi_spec/http.exchange.rs](pd-edge-abi/src/abi_spec/http.exchange.rs) backed by an explicit internal split between generic HTTP exchange state in [pd-edge/src/abi_impl/http/state.rs](pd-edge/src/abi_impl/http/state.rs) and carrier-specific policy in [pd-edge/src/abi_impl/http1/mod.rs](pd-edge/src/abi_impl/http1/mod.rs) and [pd-edge/src/abi_impl/http2/mod.rs](pd-edge/src/abi_impl/http2/mod.rs).

Implemented today:

- upstream HTTP/2 session reuse and multiplex over the generic exchange ABI
- downstream HTTP/2 session tracking outside per-request `ProxyVmContext`
- declared internal `Http2SessionGoal` and `Http2StreamGoal` advancement paths
- explicit stream carrier refs attached to upstream exchanges and to real downstream HTTP/2 requests
- GOAWAY and stream reset modeled as session or stream frontier transitions

Still intentionally missing:

- a VM-visible `http2::session::*` namespace
- full connection-scoped downstream VM hosting for long-lived multi-stream sessions
- every possible HTTP/2 frame primitive or policy surface

So the current answer to "do we already have an HTTP/2 DAG?" is: yes, internally, at the carrier/session/stream layer. The VM-facing surface remains the generic `http::exchange::*` API.

## Recommended End State

### 1. Split generic HTTP from HTTP/1 and HTTP/2

Refactor the internal model into three layers:

- `http`: generic message or exchange semantics
- `http1`: HTTP/1.1 request/response carrier and upgrade-aware connection DAG
- `http2`: HTTP/2 session DAG plus per-stream child DAGs

Recommended attachment points:

- `tcp.connected -> http1 connection` for plain HTTP/1.1
- `tls.plaintext ready + negotiated_alpn=http/1.1 -> http1 connection`
- `tcp.connected + prior knowledge preface -> http2 session`
- `tls.plaintext ready + negotiated_alpn=h2 -> http2 session`
- `http2 stream n -> http exchange n`
- `http1 request-response cycle -> http exchange n`

The generic `http` layer stays where the VM does request and response work. Version-specific layers only provide carrier semantics and lifecycle.

### 2. Model HTTP/2 as session plus streams

HTTP/2 needs two nested DAGs:

- `http2 session`
  - configured
  - preface sent or received
  - settings exchanged
  - open
  - closing
  - closed or failed
- `http2 stream`
  - allocated
  - headers sent
  - request body stream
  - response headers ready
  - response body stream
  - half-closed
  - closed or reset

Important ownership rule:

- TCP and TLS own bytes
- HTTP/2 session owns frame parsing, flow control, and stream ID allocation
- generic `http` exchange owns request and response semantics for one logical message

### 2A. Express HTTP/2 in the goal or advance model

HTTP/2 should not be implemented as a pile of direct helper calls such as "open session", "allocate stream", or "send request". It should publish goals just like the other DAG families.

Recommended internal goals:

- `http2.session.attached`
- `http2.session.open`
- `http2.session.draining`
- `http2.stream.attached`
- `http2.stream.request_committed`
- `http2.stream.response_head_ready`
- `http2.stream.response_body_ready`
- `http2.stream.closed`
- `http2.stream.reset`

Recommended detach goals:

- `http.exchange.request_ready`
- `http.exchange.response_ready`

Meaning:

- asking for `http.exchange.response_ready` on an exchange carried by HTTP/2 is allowed to transitively advance:
  - `http2.session.open`
  - `http2.stream.attached`
  - `http2.stream.request_committed`
  - `http2.stream.response_head_ready`
- asking for `http.exchange.body.next_chunk` is allowed to transitively advance `http2.stream.response_body_ready`
- asking for `http.exchange.send` on a dynamic outbound exchange is allowed to attach the exchange either to an `http1` carrier or to an `http2` stream, depending on carrier policy

This is the core architectural requirement: callers request the exported HTTP exchange goal, while the runtime chooses and advances the underlying carrier DAG.

### 2B. Define the HTTP/2 session DAG frontier

Recommended session nodes:

- `session candidate`
- `session attachable`
- `session preface sent or received`
- `peer settings received`
- `session open`
- `session draining`
- `session closed`
- `session failed`

Recommended exported capabilities:

- `stream attach allowed`
- `response frames readable`
- `goaway observed`

Legal advance paths to `http2.session.open`:

- cleartext prior-knowledge path
  - `tcp.connected`
  - client preface sent
  - settings exchanged
  - `session open`
- TLS ALPN path
  - `tls.plaintext ready`
  - `negotiated_alpn = h2`
  - settings exchanged
  - `session open`
- session reuse path
  - existing pooled session is already at `session open`
  - `advance(http2.session.open)` is idempotently satisfied by reuse

This mirrors the TLS reuse model already described in the DAG docs: the goal stays the same even when the chosen forward path differs.

### 2C. Define the HTTP/2 stream DAG frontier

Recommended stream nodes:

- `stream reserved`
- `stream attached to exchange`
- `request headers sent`
- `request body open`
- `request closed`
- `response head ready`
- `response body open`
- `half closed local`
- `half closed remote`
- `stream closed`
- `stream reset`

Recommended exported capabilities:

- `http exchange request carrier attached`
- `http exchange response head readable`
- `http exchange response body readable`

Important rule:

- once an exchange is attached to an HTTP/2 stream, fallback to HTTP/1.1 is no longer legal for that exchange
- fallback from HTTP/2 to HTTP/1.1 must happen before `http2.stream.attached` is published

### 2D. Define the detach chain explicitly

The runtime should model these detach edges:

- `tcp.connected -> http2.session.attachable`
- `tls.plaintext ready + negotiated_alpn=h2 -> http2.session.attachable`
- `http2.session.open -> stream attach allowed`
- `http2.stream.attached -> http.exchange.request_ready`
- `http2.stream.response_head_ready -> http.exchange.response_ready`
- `http2.stream.response_body_open -> http.exchange.body stream`

This makes HTTP/2 a real sibling carrier to HTTP/1.1 rather than an implementation detail hidden under the generic HTTP exchange layer.

### 3. Keep `http::exchange::*` as the primary VM ABI

The current `http::exchange::*` surface is already close to the right semantic abstraction.

Recommendation:

- keep `http::exchange::*` as the stable VM-visible API
- let the runtime choose whether an exchange is realized through `http1` or `http2`
- only add HTTP/2-specific host APIs when there is a clear need for explicit session control or introspection

Possible additive ABI later, if needed:

- `http::exchange::set_version(exchange, "auto" | "1.1" | "2")`
- `http::exchange::get_version(exchange)`
- `http2::session::*` introspection for stream counts, peer settings, or stream IDs

The first milestone should avoid overcommitting to a large `http2::*` ABI until the internal model is proven.

## Internal Architecture Changes

### A. Split `HttpOutboundExchangeNode`

`HttpOutboundExchangeNode` in [pd-edge/src/abi_impl/http/state.rs](pd-edge/src/abi_impl/http/state.rs) should stop owning its own transport DAGs directly.

Replace the current per-exchange ownership with:

- `HttpExchangeState`
  - request draft
  - response snapshot
  - response body reader
  - selected carrier kind
  - latency accounting
- `HttpCarrierRef`
  - `Http1DefaultUpstream`
  - `Http1DynamicConnection(handle)`
  - `Http2Stream { session_handle, stream_id }`

This is the core change needed to make one HTTP/2 session carry many exchanges.

The key missing piece today is that `HttpCarrierRef::Http2Stream { session_handle, stream_id }` is only implicit. It must become explicit in the exchange state so the generic exchange DAG can ask its carrier to satisfy goals instead of hardcoding HTTP/2 request startup inline.

### B. Add shared upstream HTTP session state

`SharedState` in [pd-edge/src/runtime.rs](pd-edge/src/runtime.rs) currently shares a `reqwest::Client` and the TLS session cache across requests.

HTTP/2 needs a new shared upstream session manager, for example:

```rust
pub(crate) struct SharedHttpUpstreamSessions {
    http1: ...,
    http2: ...,
}
```

The HTTP/2 manager should:

- key sessions by origin plus protocol configuration
- reuse one open session across multiple exchanges when legal
- allocate stream IDs
- track session health and GOAWAY state
- know when a new session must be created instead of reusing an old one

Recommended state split:

```rust
struct Http2SessionState {
    frontier: Http2SessionFrontier,
    peer_settings: ...,
    goaway: Option<Http2GoawayState>,
    streams: HashMap<u32, Http2StreamState>,
    next_local_stream_id: u32,
}

struct Http2StreamState {
    frontier: Http2StreamFrontier,
    exchange_handle: i64,
    reset: Option<Http2ResetState>,
}
```

The important point is that frontier state must be stored in terms of DAG nodes and exported capabilities, not just ad hoc booleans.

Recommended session-pool key inputs:

- target origin
- cleartext vs TLS
- negotiated or requested protocol version
- TLS verification and certificate configuration
- ALPN policy

### C. Stop relying on `reqwest` for the HTTP/2 path

`reqwest` is fine for opaque HTTP behavior, but it is the wrong abstraction for a visible HTTP/2 DAG with multiplex and shared session state.

Recommendation:

- keep `reqwest` for the HTTP/1 path during migration if that reduces churn
- implement the HTTP/2 path with `hyper` or `h2` directly so the runtime owns the session lifecycle

The HTTP/2 path needs explicit access to:

- connection startup
- request stream creation
- response stream bodies
- GOAWAY and reset handling
- shared connection task lifetime

Without that, multiplex will remain accidental or opaque rather than explicit.

### D. Introduce `http1` and `http2` internal modules

Recommended new modules under [pd-edge/src/abi_impl/](pd-edge/src/abi_impl/):

- `http/`
  - generic exchange state and version-agnostic helpers
- `http1/`
  - HTTP/1 realization, upgrade handling, and current request-response carrier logic
- `http2/`
  - session pool, session DAG, stream DAG, and stream-to-exchange attachment

This should be accompanied by documentation updates in:

- [pd-edge/README.md](pd-edge/README.md)
- [pd-edge/docs/full-dag.md](pd-edge/docs/full-dag.md)

## Downstream And Upstream Strategy

### Upstream first

Upstream HTTP/2 should land before downstream HTTP/2 session hosting.

Reason:

- upstream already fits the current `http::exchange::*` abstraction
- multiplex value is immediate for dynamic exchanges
- downstream full session hosting requires connection-scoped state outside one `ProxyVmContext`

### Downstream second

The current downstream runtime creates one `ProxyVmContext` per request in [pd-edge/src/runtime/http_plane/proxy_path.rs](pd-edge/src/runtime/http_plane/proxy_path.rs).

That is compatible with individual HTTP/2 streams as long as the server stack does the demultiplexing first, but it is not enough to represent a visible downstream HTTP/2 session DAG with shared settings, stream IDs, resets, and GOAWAY.

Recommended downstream rollout:

- Phase 1: downstream HTTP/2 requests can enter the runtime as generic HTTP requests, with version metadata preserved
- Phase 2: add an explicit downstream HTTP/2 session store so the DAG model can represent the client-side session, not just isolated requests

For the full DAG end state, downstream needs the same goal model as upstream:

- one connection-scoped HTTP/2 session DAG outside `ProxyVmContext`
- one stream DAG per downstream request
- per-request `ProxyVmContext` attaches to `HttpCarrierRef::Http2Stream { session_id, stream_id }`
- GOAWAY and stream-reset events update the session and stream frontiers without corrupting unrelated requests

## Detach Semantics

The term "detach" should be made explicit in the docs and code:

- `tcp` or `tls` publishes a capability that permits an HTTP carrier to attach
- `http2 session` publishes a stream capability
- `http2 stream` detaches into a generic `http exchange`
- generic `http exchange` may later resolve to an HTTP/1.1 carrier instead when version selection or fallback requires it

The important rule is:

- `http` is not "HTTP/1.1"
- `http1` and `http2` are both carriers for `http`

## Protocol Selection

Recommended policy order for outbound exchanges:

- explicit per-exchange version override, if the ABI later adds one
- otherwise automatic selection based on:
  - target scheme
  - ALPN result
  - cleartext prior knowledge rules
  - runtime feature availability

Expected behavior:

- `https` + ALPN `h2` => use HTTP/2
- `https` + ALPN `http/1.1` => use HTTP/1.1
- `http` + explicit prior knowledge or h2c support => use HTTP/2
- otherwise => use HTTP/1.1

Fallback from attempted HTTP/2 to HTTP/1.1 must happen at the carrier-selection layer, not by changing the generic `http` exchange ABI.

## Multiplex Model

Real multiplex means:

- two or more `http::exchange` handles can be active at the same time
- they can share one `http2 session`
- progress on one response body must not force completion of another stream first
- stream resets or GOAWAY affect only the relevant exchange or the session according to HTTP/2 rules

This is different from the current dynamic exchange model, which gives isolation but not shared-session concurrency.

## Feature Gating

Recommended new feature:

- `http2`

Suggested relationship:

- `http` stays as the generic semantic layer
- `http1` can remain implicit for now or become an explicit internal feature later
- `http2` should be optional at first
- `tls` remains independent, because cleartext `h2c` is still a valid attach path

First recommended default:

- keep `http2` off by default until the upstream path is stable

## Rollout Plan

### Phase 1: DAG and state refactor

- Update docs to split `http`, `http1`, and `http2`
- Refactor `HttpOutboundExchangeNode` into a transport-agnostic exchange state
- Move current HTTP/1-specific logic out of the generic HTTP state path
- Add new internal carrier references

Exit criteria:

- current HTTP/1 behavior still works
- no multiplex yet
- docs reflect the new model accurately

### Phase 2: upstream HTTP/2 session implementation

- Add `http2` feature and internal `http2` module
- Build a shared upstream HTTP/2 session manager in `SharedState`
- Implement stream allocation and stream-to-exchange attachment
- Route multiple dynamic exchanges over one session when the target and config permit reuse

Exit criteria:

- multiple `http::exchange::new()` handles can share one HTTP/2 session
- independent response bodies can be consumed correctly
- session reuse and session failure behavior are covered by tests

### Phase 3: protocol selection and fallback

- Add explicit runtime policy for `http1` vs `http2`
- Respect ALPN and cleartext prior-knowledge rules
- Add clean fallback from failed or unavailable HTTP/2 negotiation to HTTP/1.1 where legal

Exit criteria:

- automatic selection works
- fallback works without changing VM programs
- TLS ALPN state and carrier selection stay consistent

### Phase 4: downstream HTTP/2 request support

- Accept downstream HTTP/2 requests as first-class generic HTTP requests
- Preserve version information in the request head
- Ensure normal request and response handling works for downstream HTTP/2 streams

Exit criteria:

- downstream HTTP/2 requests can run existing VM programs unchanged
- request metadata reflects version `2`
- no connection-scoped downstream HTTP/2 DAG yet

### Phase 5: downstream HTTP/2 session DAG

- Add a downstream HTTP/2 session store outside per-request `ProxyVmContext`
- Represent GOAWAY, stream resets, and shared session state explicitly
- Decide whether any session introspection should become VM-visible
- Replace coarse downstream lifecycle tracking with real session and stream frontier tracking
- Attach each downstream request context to an explicit HTTP/2 stream carrier ref

Exit criteria:

- docs and runtime both model downstream HTTP/2 as a real session, not just opaque server behavior
- the runtime can satisfy `http.exchange.*` goals by advancing either an HTTP/1 or HTTP/2 carrier without special-case request startup code

## Test Plan

Add or expand tests in [pd-edge/tests/proxy_tests/http.rs](pd-edge/tests/proxy_tests/http.rs) and related support code for:

- upstream exchange works over negotiated HTTP/2
- two dynamic exchanges multiplex over one upstream HTTP/2 session
- one slow stream does not block another completed stream from being read
- fallback to HTTP/1.1 works when HTTP/2 is unavailable
- TLS ALPN `h2` is reflected in transport state when HTTP/2 is selected
- downstream request version `2` is visible to VM code
- existing generic `http::exchange::*` programs continue to work on both HTTP/1.1 and HTTP/2
- GOAWAY and reset handling fail the right exchanges without corrupting unrelated streams

Add targeted regression coverage for:

- `http::exchange::new()` semantics under multiplex
- response body chunk reads on multiple streams
- interaction with TLS session reuse and ALPN caching
- interaction with feature-reduced builds where `http2` is disabled

## Risks

### Risk 1: keeping HTTP/2 hidden behind `reqwest`

If the runtime keeps treating HTTP/2 as an opaque client transport, the code may "speak h2" without ever gaining a real HTTP/2 DAG or multiplex model.

### Risk 2: mixing generic HTTP state with carrier state again

If `http` continues to own HTTP/1-specific assumptions, HTTP/2 support will become a pile of exceptions instead of a clean sibling DAG.

### Risk 3: downstream scope explosion

Trying to deliver full downstream session hosting in the first patch series will likely stall the work. Upstream HTTP/2 should land first.

### Risk 4: WebSocket confusion

Current WebSocket support is HTTP/1.1 upgrade-based. HTTP/2 does not use the same upgrade path, so the existing websocket DAG must not be silently treated as HTTP/2-compatible.

## Recommended Commit Split

1. docs and DAG refactor for `http` vs `http1` vs `http2`
2. internal generic-exchange refactor without behavior change
3. `http2` feature and upstream session manager scaffolding
4. real upstream HTTP/2 multiplex path
5. protocol selection and fallback
6. downstream HTTP/2 request-path support
7. later downstream session DAG work

## Recommendation

Implement this as an upstream-first, generic-HTTP-first refactor.

That keeps the VM surface stable, gives real multiplex instead of fake parallel exchanges, and leaves room for downstream HTTP/2 session hosting without forcing the entire runtime to be redesigned in one patch.
