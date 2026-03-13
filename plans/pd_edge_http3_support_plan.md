# pd-edge HTTP/3 Support Plan

## Summary

Add real HTTP/3 support to `pd-edge` by extending the current generic HTTP exchange model with a QUIC-backed carrier, while keeping the VM-facing request and exchange APIs mostly version-agnostic.

The key design constraint is that HTTP/3 must not be modeled as:

- "HTTP/2 with a different transport"
- "TLS over UDP"
- "just another `reqwest` mode"

HTTP/3 needs its own carrier DAG above QUIC, and QUIC needs its own transport DAG above UDP.

Recommended conceptual layering:

- `udp` = datagram socket semantics
- `quic` = encrypted, multiplexed transport over UDP
- `http3` = HTTP semantics over QUIC streams and control streams
- `http` = generic request and response exchange semantics exposed to VM code

That gives the properties this effort needs:

- downstream HTTP/3 can enter the runtime without being forced through the TCP or TLS byte-stream DAGs
- upstream HTTP/3 can reuse one QUIC connection for many outbound exchanges
- generic `http::request::*`, `http::response::*`, and `http::exchange::*` remain the primary VM surface
- fallback between HTTP/1.1, HTTP/2, and HTTP/3 stays explicit and happens before a carrier is attached

## Goals

- Support upstream HTTP/3 as a first-class carrier for outbound exchanges.
- Support downstream HTTP/3 ingress on the data-plane listener side.
- Preserve the generic `http::exchange::*` and downstream request/response ABI as the default VM-facing model.
- Add shared upstream HTTP/3 session reuse and stream multiplexing.
- Track downstream HTTP/3 sessions and request streams outside per-request `ProxyVmContext`, the same way HTTP/2 now does.
- Make carrier attachment explicit in generic exchange state so HTTP/1.1, HTTP/2, and HTTP/3 are sibling realizations.
- Keep the design aligned with the layered DAG model in [pd-edge/README.md](../pd-edge/README.md) and [pd-edge/docs/full-dag.md](../pd-edge/docs/full-dag.md).

## Non-Goals For The First Milestone

- WebTransport, HTTP Datagrams, or QUIC datagram APIs
- Extended CONNECT or WebSocket over HTTP/3
- 0-RTT request replay support
- QUIC connection migration policy exposed to VM code
- A full VM-visible `quic::*` or `http3::*` ABI namespace
- Replacing the existing `http::exchange::*` surface with version-specific APIs
- Full connection-scoped downstream VM hosting across many long-lived streams
- Automatic upstream Alt-Svc discovery and caching

## Current State

The current runtime already has the right high-level split for extending to HTTP/3:

- generic HTTP exchange state lives in [pd-edge/src/abi_impl/http/state.rs](../pd-edge/src/abi_impl/http/state.rs)
- HTTP/2 carrier policy lives in [pd-edge/src/abi_impl/http2/model.rs](../pd-edge/src/abi_impl/http2/model.rs), [pd-edge/src/abi_impl/http2/upstream.rs](../pd-edge/src/abi_impl/http2/upstream.rs), and [pd-edge/src/abi_impl/http2/downstream.rs](../pd-edge/src/abi_impl/http2/downstream.rs)
- downstream HTTP requests are admitted through [pd-edge/src/runtime/http_plane/proxy_path.rs](../pd-edge/src/runtime/http_plane/proxy_path.rs)
- shared runtime stores are initialized in [pd-edge/src/runtime.rs](../pd-edge/src/runtime.rs)

Implemented today:

- generic `http::exchange::*` remains the stable VM-visible API
- upstream HTTP/2 uses a shared session store and explicit stream carrier refs
- downstream HTTP/2 sessions are tracked outside per-request VM context
- `HttpCarrierRef` already distinguishes downstream and upstream HTTP/2 streams
- `http_version_label()` already knows how to label `Version::HTTP_3` as `"3"`

Still missing for HTTP/3:

- any QUIC transport model under `udp`
- any internal `http3` session or stream DAG
- any shared upstream HTTP/3 session pool
- any downstream HTTP/3 listener or tracker
- any `HttpCarrierKind::Http3` or `HttpCarrierRef::*Http3Stream(...)`

Important existing constraints:

- downstream HTTP today is served through the `axum` or `hyper` TCP server path in [pd-edge/src/runtime/http_plane/proxy_path.rs](../pd-edge/src/runtime/http_plane/proxy_path.rs)
- upstream HTTP/1.1 and fallback HTTP/2 still rely on `reqwest::Client` in [pd-edge/src/abi_impl/http/state.rs](../pd-edge/src/abi_impl/http/state.rs)
- the current UDP subsystem in [pd-edge/src/abi_impl/transport/state.rs](../pd-edge/src/abi_impl/transport/state.rs) is datagram-oriented and does not model QUIC connection state

So the current answer to "can HTTP/3 just reuse the HTTP/2 design?" is: only at the generic exchange boundary. Below that, HTTP/3 needs a new transport and carrier stack.

## Recommended End State

### 1. Add `quic` as an internal layer between `udp` and `http3`

Recommended layering:

- `udp`: bind, target selection, connected socket, datagram send or receive
- `quic`: connection identity, TLS 1.3 crypto, connection IDs, stream allocation, connection close
- `http3`: control streams, settings, request stream attachment, GOAWAY, reset semantics
- `http`: request and response semantics for one logical exchange

Recommended attachment points:

- downstream UDP socket or listener -> QUIC connection
- upstream UDP socket or dial policy -> QUIC connection
- QUIC connection open -> HTTP/3 session attachable
- HTTP/3 request stream attached -> generic HTTP exchange attached

The critical rule is:

- QUIC owns transport and crypto
- HTTP/3 owns HTTP framing and request stream semantics
- generic `http` owns the VM-visible request and response model

Do not route HTTP/3 through the existing `tls` byte-stream DAG. QUIC uses TLS 1.3 internally, but it is not a reusable plaintext-over-stream transport like the current TCP plus TLS path.

### 2. Model QUIC and HTTP/3 as separate internal DAG families

Recommended internal DAGs:

- `quic connection`
  - candidate
  - handshake in progress
  - 1-RTT ready
  - open
  - draining
  - closed or failed
- `quic stream`
  - reserved
  - open
  - half closed local or remote
  - reset or stopped
  - closed
- `http3 session`
  - attached
  - control streams opened
  - settings exchanged
  - open
  - draining after GOAWAY
  - closed or failed
- `http3 request stream`
  - attached to exchange
  - request head committed
  - request body open
  - response head ready
  - response body ready
  - closed or reset

This is the HTTP/3 equivalent of the current HTTP/2 split in [pd-edge/src/abi_impl/http2/model.rs](../pd-edge/src/abi_impl/http2/model.rs), but with QUIC separated from HTTP semantics instead of being hidden inside one carrier module.

### 3. Extend generic HTTP carrier bookkeeping

`HttpCarrierKind` and `HttpCarrierRef` in [pd-edge/src/abi_impl/http/state.rs](../pd-edge/src/abi_impl/http/state.rs) should grow a third carrier family:

- `HttpCarrierKind::Http3`
- `HttpCarrierRef::DownstreamHttp3Stream(Http3StreamRef)`
- `HttpCarrierRef::UpstreamHttp3Stream(Http3StreamRef)`

`Http3StreamRef` should mirror the existing HTTP/2 shape:

```rust
pub(crate) struct Http3StreamRef {
    pub(crate) session_id: u64,
    pub(crate) stream_id: u64,
}
```

This is the core attachment point that lets the generic exchange layer stay version-agnostic while still delegating body reads, resets, and session-level control to the correct carrier.

### 4. Keep `http::*` and `http::exchange::*` as the primary VM ABI

The current VM-facing surface is already close to the right abstraction for HTTP/3.

Recommendation:

- keep downstream request access under `http::request::*`
- keep downstream response output under `http::response::*`
- keep outbound operations under `http::exchange::*`
- keep `http::request::get_http_version()` and `http::exchange::get_http_version()` as the default way to observe whether a request or exchange used HTTP/1.1, HTTP/2, or HTTP/3

Possible additive ABI later, if needed:

- `http::exchange::set_version(exchange, "auto" | "1.1" | "2" | "3")`
- `http::exchange::get_version(exchange)`
- `http3::session::*` introspection
- `quic::connection::*` introspection

For the first milestone, avoid adding a large new ABI surface. The generic HTTP ABI is sufficient if the runtime can choose and expose the realized version.

## Upstream And Exchange Strategy

### Upstream should land before downstream

HTTP/3 should follow the same rollout order as HTTP/2, but even more strongly:

- upstream already matches the generic exchange abstraction
- upstream immediately benefits from multiplexing and connection reuse
- downstream needs a new UDP and QUIC listener path rather than the current `hyper` TCP server path
- upstream can isolate the new carrier behind `http::exchange::*` without changing downstream admission yet

### 1. Add explicit upstream HTTP/3 selection policy

The current HTTP/2 selector in [pd-edge/src/abi_impl/http2/model.rs](../pd-edge/src/abi_impl/http2/model.rs) is based on target scheme and ALPN hints.

HTTP/3 should not auto-activate for every `https://` target. Initial policy should be explicit:

- no cleartext HTTP/3 mode
- only `https://` targets are eligible
- first milestone uses explicit version preference or explicit ALPN `h3`
- no upstream Alt-Svc cache in milestone one

Recommended new mode enum:

- `Http3UpstreamMode::Disabled`
- `Http3UpstreamMode::Preferred`
- `Http3UpstreamMode::Required`

Semantics:

- `Disabled`: use existing HTTP/1.1 or HTTP/2 selection
- `Preferred`: try HTTP/3 first, then fall back to HTTP/2 or HTTP/1.1 before stream attach
- `Required`: fail if HTTP/3 cannot be established

This is the main reason to add `http::exchange::set_version(...)` early for outbound exchanges. ALPN-only inference is too low-level for a transport choice as significant as QUIC.

### 2. Add shared upstream QUIC and HTTP/3 session state

`SharedState` in [pd-edge/src/runtime.rs](../pd-edge/src/runtime.rs) currently keeps:

- `client: reqwest::Client`
- `upstream_client_cache`
- `tls_session_cache`
- `upstream_http_sessions` for HTTP/2

Add parallel stores for HTTP/3:

- `upstream_http3_sessions`
- optionally a lower-level `upstream_quic_connections` if the `http3` session store does not itself own the QUIC connection

Recommended session key components:

- origin authority
- SNI or logical target
- certificate verification policy
- hostname verification policy
- trusted CA bundle or client identity policy
- explicit local bind or attached transport policy, if supported

Important rule:

- once an exchange is attached to an HTTP/3 stream, fallback is no longer legal for that exchange
- fallback from HTTP/3 to HTTP/2 or HTTP/1.1 must happen before `HttpCarrierRef::UpstreamHttp3Stream(...)` is published

### 3. Keep `reqwest` for HTTP/1.1 and HTTP/2 fallback

Do not try to force milestone-one HTTP/3 through the existing `reqwest::Client` path in [pd-edge/src/abi_impl/http/state.rs](../pd-edge/src/abi_impl/http/state.rs).

Recommended split:

- keep `reqwest` for HTTP/1.1 and HTTP/2 fallback
- keep the explicit `hyper`-based HTTP/2 path for shared HTTP/2 session reuse
- add a separate explicit HTTP/3 client path using a QUIC plus HTTP/3 stack

That preserves the current working fallback behavior while giving HTTP/3 a carrier that actually matches its transport model.

### 4. Reuse current exchange semantics above the carrier

Generic outbound flow should stay:

- request draft created
- method, target, headers, body configured
- `http::exchange::send(handle)`
- response headers available
- response body streamed through the generic exchange body APIs

Internal detach path for an HTTP/3-backed exchange:

- exchange policy selects HTTP/3
- runtime advances QUIC connection to open
- runtime advances HTTP/3 session to open
- runtime allocates an HTTP/3 request stream
- runtime attaches the request stream to the exchange
- exchange response and body readiness are satisfied through the generic HTTP exchange APIs

This should mirror the current `start_upstream_response_via_http2(...)` split in [pd-edge/src/abi_impl/http/state.rs](../pd-edge/src/abi_impl/http/state.rs), but with an HTTP/3 carrier module and QUIC-backed body source.

### 5. Reuse TLS policy only as configuration input, not as a transport DAG

Current VM programs already use `tls::session::*` setters to control verification and ALPN for outbound exchanges.

Recommended HTTP/3 behavior:

- allow those setters to remain the source of outbound crypto policy
- realize the valid subset through QUIC TLS 1.3 configuration
- reject incompatible options during HTTP/3 setup

Examples of incompatible or restricted behavior:

- HTTP/3 must require TLS 1.3
- raw upstream TLS byte-stream attach semantics do not apply
- HTTP/3 cannot run on a preexisting TCP transport attachment

So the existing TLS API can remain useful as configuration input, but QUIC and HTTP/3 must still be separate internal DAGs.

## Downstream Strategy

### 1. Treat downstream HTTP/3 as a new ingress mode, not as HTTP auto-promotion from TCP

The current downstream HTTP runtime in [pd-edge/src/runtime/http_plane/proxy_path.rs](../pd-edge/src/runtime/http_plane/proxy_path.rs) has two main admission shapes:

- plain HTTP over TCP
- HTTPS over TCP plus TLS, with optional auto-promotion into HTTP

HTTP/3 needs a third admission shape:

- HTTP/3 over QUIC on UDP

This should not go through:

- downstream TCP
- downstream TLS plaintext promotion
- `http::downstream::attach_transport()` from the current TCP or TLS transport DAG

For milestone one, downstream HTTP/3 should enter directly as HTTP ingress with HTTP/3 carrier metadata already attached.

### 2. Add a downstream HTTP/3 listener path alongside the current TCP listener

Recommended runtime shape:

- keep the existing TCP listener for HTTP/1.1 and HTTP/2
- add a UDP listener for HTTP/3
- run a QUIC accept loop on the UDP listener
- run an HTTP/3 request accept loop per QUIC connection
- adapt each accepted HTTP/3 request stream into the existing per-request VM execution path

Recommended new runtime entrypoints:

- `serve_http3_proxy(...)`
- `serve_http3_connection(...)`

These should feed the same VM request execution path currently used by `data_plane_handler`, but with HTTP/3-specific carrier metadata and response writer plumbing.

### 3. Track downstream HTTP/3 sessions outside `ProxyVmContext`

HTTP/2 already established the correct pattern:

- per-request VM context for request-local state
- shared session tracker outside the VM context for connection-scoped state

HTTP/3 should follow the same model with a dedicated store:

- `SharedHttp3DownstreamSessions`
- `DownstreamHttp3ConnectionTracker`
- `Http3DownstreamStreamAttachment`

The tracker should record:

- session ID
- peer address
- active request stream count
- last path
- last error
- QUIC close state
- HTTP/3 GOAWAY or request-stream reset state

Then `ProxyVmContext` attaches:

- `HttpCarrierRef::DownstreamHttp3Stream(Http3StreamRef)`

This keeps downstream session ownership aligned with the current HTTP/2 design rather than bloating `ProxyVmContext`.

### 4. Keep the first downstream milestone per-request, not connection-scoped

Downstream HTTP/3 should initially behave like downstream HTTP/2 today:

- one VM execution per admitted request stream
- shared session tracking outside the VM context
- no connection-scoped VM lifecycle across all streams on one QUIC connection

This is enough for:

- `http::request::get_http_version()` returning `"3"`
- generic request and response host calls working for HTTP/3 requests
- correct carrier bookkeeping and response streaming

It avoids the much larger problem of making the VM itself own one long-lived client-facing QUIC session.

### 5. Advertise downstream HTTP/3 explicitly

If the data plane enables HTTP/3, it should also be able to advertise it.

Recommended behavior:

- add an optional HTTP/3 listener address, usually on the same numeric port as HTTPS but on UDP
- when enabled, optionally emit `Alt-Svc` on HTTP/1.1 and HTTP/2 responses unless VM code already sets its own value

Upstream Alt-Svc caching can wait. Downstream advertisement is useful immediately and is independent of the outbound exchange path.

## Internal Architecture Changes

### A. Add new internal modules

Recommended new modules:

- `pd-edge/src/abi_impl/quic/`
  - `mod.rs`
  - `model.rs`
  - `upstream.rs`
  - `downstream.rs`
- `pd-edge/src/abi_impl/http3/`
  - `mod.rs`
  - `model.rs`
  - `upstream.rs`
  - `downstream.rs`

Even if milestone one keeps some QUIC types physically inside `http3/`, the target architecture should reserve a separate internal `quic` layer.

### B. Extend runtime services and store limits

`RuntimeServices` and `SharedState` should add HTTP/3-aware stores in:

- [pd-edge/src/abi_impl/http/state.rs](../pd-edge/src/abi_impl/http/state.rs)
- [pd-edge/src/runtime.rs](../pd-edge/src/runtime.rs)
- [pd-edge/src/cache.rs](../pd-edge/src/cache.rs)

Recommended new store limits:

- `DEFAULT_UPSTREAM_HTTP3_REUSE_STORE_CAPACITY`
- `DEFAULT_DOWNSTREAM_HTTP3_SESSION_STORE_CAPACITY`

And corresponding `RuntimeStoreLimits` fields plus CLI flags similar to the current HTTP/2 store limits.

### C. Extend carrier-aware response body readers

`UpstreamResponseSource` and related body-tracking code in [pd-edge/src/abi_impl/http/state.rs](../pd-edge/src/abi_impl/http/state.rs) should gain an HTTP/3-backed variant similar to the current explicit HTTP/2 response path.

This is where generic body APIs continue to work while carrier-specific reset, GOAWAY, and EOF behavior are translated into generic exchange semantics.

### D. Keep feature gating runtime-only for the first milestone

Unlike HTTP/2, HTTP/3 does not need a brand-new VM ABI to be useful on day one.

Recommendation:

- add a `pd-edge` runtime feature such as `http3`
- do not add `edge_abi/http3` or `vm/http3` yet unless a new VM-visible namespace is introduced

The first milestone can run entirely on the existing generic HTTP ABI and version metadata.

### E. Update sample echo and test fixtures

Extend [pd-edge/src/sample_echo.rs](../pd-edge/src/sample_echo.rs) with:

- an HTTP/3 listener
- shared certificate configuration for HTTPS and HTTP/3
- request and response version reporting that reaches `"3"` on the new path

This gives the repo a local multi-protocol fixture for:

- downstream HTTP/3 ingress tests
- upstream HTTP/3 exchange tests
- mixed-version fallback tests

## Milestones

### Milestone 0: Groundwork

- add `http3` feature and dependencies
- define `Http3StreamRef`, `Http3SessionGoal`, `Http3StreamGoal`, and frontiers
- add `HttpCarrierKind::Http3` and carrier refs
- add runtime stores and store limits
- add version-selection scaffolding for outbound exchanges

### Milestone 1: Upstream HTTP/3 exchange support

- implement explicit HTTP/3 selection policy for outbound exchanges
- add shared upstream QUIC plus HTTP/3 session reuse
- attach outbound exchanges to HTTP/3 request streams
- stream response headers and body through the generic HTTP exchange APIs
- preserve fallback to HTTP/2 or HTTP/1.1 before stream attachment
- add integration tests for reuse, multiplexing, body streaming, and fallback

This milestone should deliver real user value without changing downstream admission.

### Milestone 2: Downstream HTTP/3 ingress

- add UDP plus QUIC plus HTTP/3 listener support to the data plane
- adapt HTTP/3 request streams into the current per-request VM execution path
- add downstream HTTP/3 session and stream tracking outside `ProxyVmContext`
- expose version `"3"` through `http::request::get_http_version()`
- optionally advertise `Alt-Svc`
- add integration tests for same-program handling across HTTP/1.1, HTTP/2, and HTTP/3

### Milestone 3: Optional explicit introspection and control

- add `http::exchange::set_version(...)` if it did not land earlier
- add optional `http3::session::*` or `quic::connection::*` introspection
- add richer telemetry and metrics
- consider upstream Alt-Svc cache and policy

## Testing Plan

### Unit tests

- HTTP/3 mode selection and fallback policy
- carrier ref attachment and version labeling
- QUIC and HTTP/3 frontier transitions
- reset, GOAWAY, and close classification

### Upstream integration tests

- one program creates multiple exchanges to the same origin and they multiplex over one HTTP/3 session
- later exchanges reuse an existing healthy HTTP/3 session
- independent response-body reads stay independent across streams
- required HTTP/3 mode fails cleanly when the peer does not negotiate `h3`
- preferred HTTP/3 mode falls back to HTTP/2 or HTTP/1.1 before stream attachment

### Downstream integration tests

- downstream HTTP/3 requests expose version metadata to VM programs
- the same VM program handles HTTP/1.1, HTTP/2, and HTTP/3 requests correctly
- request bodies and response bodies stream correctly over downstream HTTP/3
- session tracker records stream lifecycle outside the VM context
- graceful connection close and request-stream reset states are captured

### Fixture and sample tests

- extend `sample_echo` tests with HTTP/3 listener coverage
- add local HTTP/3 upstream fixtures similar to the current HTTP/2 sample servers in [pd-edge/tests/proxy_tests/support.rs](../pd-edge/tests/proxy_tests/support.rs)

## Recommended Library Direction

Recommended implementation direction:

- keep `axum`, `hyper`, and `reqwest` for current HTTP/1.1 and HTTP/2 paths
- add an explicit QUIC plus HTTP/3 stack for HTTP/3 itself, for example `quinn` plus `h3` or `h3-quinn`

The practical consequence is that downstream HTTP/3 will use a separate accept loop from the current `hyper` TCP server path, and upstream HTTP/3 will use a separate client path from the current `reqwest` fallback path.

That is not a downside. It reflects the protocol boundary correctly and keeps the current HTTP/1.1 and HTTP/2 paths stable while HTTP/3 lands incrementally.

## Recommended Rollout Order

1. Add carrier and store scaffolding first.
2. Land upstream HTTP/3 exchange support next.
3. Land downstream HTTP/3 ingress after the upstream carrier is proven.
4. Add optional version-control and introspection APIs only if the generic HTTP ABI proves insufficient.

This keeps the first milestone scoped, preserves the current generic HTTP model, and avoids forcing QUIC into abstractions that were built for TCP plus TLS byte streams.
