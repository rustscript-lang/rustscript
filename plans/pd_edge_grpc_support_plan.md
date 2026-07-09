# pd-edge gRPC Support Plan

## Summary

Add gRPC to `pd-edge` as a semantic child DAG above the existing HTTP carrier DAGs instead of
treating it as a loose convention over `http::exchange::*`.

The key design constraint is that gRPC must not be modeled as:

- "just POST plus protobuf bytes"
- "HTTP/2 with a different content-type"
- "a transport protocol that replaces HTTP"

gRPC sits above HTTP request or response exchange semantics and adds message framing, metadata,
status, cancellation, and trailer-based completion. The implementation should preserve that layer
boundary and attach it cleanly to the existing `tcp`, `tls`, `http2`, and later `http3` graphs.

## Goals

- Support downstream gRPC requests as a first-class protocol family.
- Support outbound gRPC client calls through existing HTTP exchange machinery.
- Support grpc-gateway-style downstream HTTP or JSON to upstream gRPC bridging as an explicit
  runtime capability rather than as handwritten RSS protobuf glue.
- Keep the DAG approach explicit: gRPC must attach to HTTP carrier state, which itself already
  attaches to `tcp` and `tls`.
- Preserve `http::request::*`, `http::response::*`, and `http::exchange::*` as the carrier-facing
  APIs while adding a higher-level `grpc::*` ABI.
- Keep protobuf decoding, encoding, and HTTP or JSON to gRPC transcoding in native runtime code.
  RSS should orchestrate routing, policy, and method selection, not perform wire-format work.
- Add an RSS-facing convenience layer for gRPC and gateway flows that mirrors the existing
  `edge/http/upstream/*` pattern without moving transcoding into RSS.
- Support unary calls first, with a clean path to server-streaming and client-streaming later.
- Keep the first milestone protobuf-schema-agnostic so the runtime proves transport and framing
  semantics before adding codegen or reflection.

## Non-Goals For The First Milestone

- Protobuf code generation exposed directly to RSS programs
- gRPC reflection
- gRPC-Web or Connect protocol support
- Full grpc-gateway parity with every annotation and mapping variant on day one
- xDS, load balancing, or service discovery policy
- Automatic fallback to HTTP/1.1
- Assuming generic gRPC-over-HTTP/3 support on day one

## Current State

The lower-level carrier pieces are already present:

- generic HTTP exchange state lives in
  [`pd-edge/src/abi_impl/http/state.rs`](../pd-edge/src/abi_impl/http/state.rs)
- HTTP/2 carrier state lives in
  [`pd-edge/src/abi_impl/http2/`](../pd-edge/src/abi_impl/http2/)
- HTTP/3 carrier state lives in
  [`pd-edge/src/abi_impl/http3/`](../pd-edge/src/abi_impl/http3/)

Important current facts:

- response and exchange trailers are already readable on the HTTP side through
  [`pd-edge/src/abi_impl/http/response.rs`](../pd-edge/src/abi_impl/http/response.rs) and
  [`pd-edge/src/abi_impl/http/exchange.rs`](../pd-edge/src/abi_impl/http/exchange.rs)
- compile-time convenience wrappers already exist for HTTP under
  [`pd-edge/src/compile.rs`](../pd-edge/src/compile.rs) and
  [`pd-edge/stdlib/rss/http/upstream/`](../pd-edge/stdlib/rss/http/upstream/)
- there is still no `grpc` namespace in [`pd-edge-abi/src/abi_spec/`](../../pd-edge/pd-edge-abi/src/abi_spec/)
- there is no equivalent `edge/grpc/*` RSS convenience layer today
- there is no descriptor-backed protobuf or JSON transcode engine in the runtime today
- server-side write support for HTTP response trailers is not yet exposed as a first-class host API,
  which is a prerequisite for downstream gRPC status and trailer emission

So the current answer to "can gRPC just be a library over `http::exchange::*`?" is: only
partially. The HTTP layer already gives the right carrier structure, but the runtime still needs a
real gRPC semantic layer and explicit trailer-write support for server responses.

The current answer to "can an RSS program implement grpc-gateway by itself?" is effectively no.
RSS can route and call host APIs, but the runtime does not yet provide the native protobuf,
descriptor, and transcode layer needed to keep wire work out of VM code.

## Recommended End State

### 1. Model gRPC as a child DAG above HTTP carriers

Recommended attach chain for downstream calls:

- `tcp.connected -> tls.plaintext ready or h2c prior knowledge`
- `tls.plaintext ready or cleartext attach -> http2.session.open`
- `http2.stream.attached -> grpc.call.attached`

Future optional attach chain if later gRPC-over-HTTP/3 work is explicitly enabled:

- `udp socket ready -> quic connection ready -> http3 request stream attached -> grpc.call.attached`

Recommended attach chain for outbound calls:

- `http::exchange handle selected`
- `http2 stream or other allowed carrier attached`
- `grpc.call.attached`

The important ownership rule is:

- TCP and TLS own transport
- HTTP owns request or response exchange semantics
- gRPC owns message framing, metadata, grpc-status, grpc-message, cancellation, and trailers

gRPC should remain a semantic child DAG over HTTP, not a competing transport family.

### 2. Define the gRPC call frontier explicitly

Recommended gRPC call nodes:

- `call configured`
- `request metadata committed`
- `request message stream open`
- `response headers ready`
- `response message stream open`
- `response trailers ready`
- `call complete`
- `call cancelled`
- `call failed`

Recommended exported capabilities:

- `next request message writable`
- `next response message readable`
- `grpc status readable`
- `grpc trailers readable`

For unary calls, some of these nodes collapse quickly. For streaming calls, they remain distinct.

### 3. Define gRPC attach and detach edges explicitly

Recommended attach edges:

- `http2.stream.attached -> grpc.call.attached`
- `http3 request stream attached -> grpc.call.attached` only if later runtime policy explicitly
  enables gRPC-over-HTTP/3 work

Recommended detach edges:

- `grpc.response trailers ready -> http exchange response complete`
- `grpc.call.complete -> http stream may remain reusable according to carrier policy`
- `grpc.call.failed -> HTTP stream may terminate while the carrier session remains open`

Important rule:

- gRPC cannot attach directly to `tcp` or `tls`
- gRPC must attach only after the HTTP carrier has published request or response semantics
- detaching from gRPC returns to the HTTP carrier, not directly to transport

### 4. Add a VM-visible `grpc` ABI

The first milestone should keep the ABI explicit and composable with HTTP:

- `grpc::call::downstream()`
- `grpc::call::from_exchange(exchange)`
- `grpc::call::new()`
- `grpc::call::set_service(call, service)`
- `grpc::call::set_method(call, method)`
- `grpc::call::set_header(call, name, value)`
- `grpc::call::send_message_binary_base64(call, payload, end_stream)`
- `grpc::call::read_message_binary_base64(call)`
- `grpc::call::get_status(call)`
- `grpc::call::get_message(call)`
- `grpc::call::get_trailers(call)`
- `grpc::call::set_status(call, status)`
- `grpc::call::set_message(call, message)`
- `grpc::call::set_trailer(call, name, value)`

This keeps the carrier rule explicit:

- HTTP still owns path, authority, and version
- gRPC adds typed call semantics on top of the HTTP exchange

This low-level ABI is necessary for raw gRPC support, but it is not sufficient by itself for a
grpc-gateway-style RSS experience.

### 5. Add a native gateway and transcode layer above raw gRPC

If the end state includes "something like grpc-gateway", the runtime needs a second layer above the
raw gRPC call ABI.

Recommended responsibilities for that layer:

- load or embed descriptor-backed method metadata in native runtime state rather than rebuilding it
  per request in RSS
- decode downstream HTTP or JSON requests into method-aware protobuf messages in native code
- encode upstream gRPC responses back into downstream HTTP or JSON responses in native code
- map `grpc-status`, `grpc-message`, and trailer completion into the downstream HTTP response model
- let RSS pick route, policy, target service, target method, and response handling mode without
  forcing RSS to construct protobuf bytes or parse gRPC frames

Recommended API boundary:

- keep raw `grpc::call::*` for low-level byte-oriented gRPC workflows
- add either a dedicated `grpc::gateway::*` surface or equivalent runtime-owned gateway helpers for
  descriptor-backed HTTP or JSON to gRPC bridging
- keep protobuf decoding, encoding, and transcoding out of RSS even when RSS remains in charge of
  routing and policy

### 6. Keep protobuf and descriptor work runtime-native

The first milestone may still treat gRPC payloads as opaque bytes, but the broader plan should now
assume descriptor-backed runtime work for gateway support later.

That means:

- raw gRPC milestone: binary payloads may travel as raw framed messages and the VM may keep them as
  base64 strings initially
- gateway milestone: descriptor lookup, protobuf parsing, JSON mapping, and protobuf serialization
  happen in native runtime code
- RSS-facing helpers should operate on service or method identity, route intent, and possibly
  structured values, not on handwritten protobuf byte assembly

### 7. Add RSS-facing convenience modules, not RSS-level codecs

The existing HTTP developer experience already includes convenience wrappers compiled in from
[`pd-edge/src/compile.rs`](../pd-edge/src/compile.rs). gRPC should follow the same pattern.

Recommended additions:

- `edge/grpc/client.rss` style wrappers over the low-level `grpc::call::*` ABI
- `edge/grpc/gateway.rss` style wrappers for route and policy orchestration over the native gateway
  layer
- sample RSS programs that demonstrate raw unary gRPC, unary HTTP or JSON gateway bridging, and
  later streaming flows without any protobuf byte handling in RSS

## Downstream And Upstream Strategy

### 1. Unary server and unary client support first

Unary calls are the smallest useful gRPC slice and prove:

- HTTP/2 attach rules
- framing
- status mapping
- trailer emission

### 2. Server-streaming second

Server-streaming should land before full bidi work because it fits the current HTTP response-body
streaming model more naturally than fully symmetric streaming.

### 3. Client-streaming and bidi third

Once message framing and trailer handling are stable, client-streaming and bidi can reuse the same
gRPC frontier with more active message-stream nodes.

## Internal Architecture Changes

### A. Add new internal modules

Recommended new modules:

- `pd-edge/src/abi_impl/grpc/mod.rs`
- `pd-edge/src/abi_impl/grpc/model.rs`
- `pd-edge/src/abi_impl/grpc/framing.rs`
- `pd-edge/src/abi_impl/grpc/client.rs`
- `pd-edge/src/abi_impl/grpc/server.rs`
- `pd-edge/src/abi_impl/grpc/descriptor.rs`
- `pd-edge/src/abi_impl/grpc/transcode.rs`
- `pd-edge/src/abi_impl/grpc/gateway.rs`

The target split is:

- `model.rs` for DAG nodes and call refs
- `framing.rs` for message envelope parsing and serialization
- `client.rs` for outbound progression
- `server.rs` for downstream progression
- `descriptor.rs` for method metadata and descriptor-backed lookup
- `transcode.rs` for native protobuf or JSON decode and encode paths
- `gateway.rs` for HTTP or JSON gateway orchestration over the raw gRPC layer

### B. Extend the HTTP layer for response-trailer writes

Downstream gRPC is blocked until the runtime can emit response trailers explicitly.

Recommended prerequisite work:

- add downstream response trailer mutation APIs to
  [`pd-edge/src/abi_impl/http/response.rs`](../pd-edge/src/abi_impl/http/response.rs)
- ensure streamed responses can finish with trailers
- keep the trailer model valid across HTTP/2 and HTTP/3 carriers

### C. Extend the ABI source of truth

Add `grpc` ABI specs under [`pd-edge-abi/src/abi_spec/`](../../pd-edge/pd-edge-abi/src/abi_spec/).

The runtime-facing relationship should be:

- `grpc` is VM-visible and semantic
- `http` remains VM-visible and carrier-facing
- `grpc` may wrap existing downstream or outbound HTTP exchange refs rather than replacing them

If gateway support is in scope, also add a separate gateway-oriented ABI surface or equivalent
runtime-owned helper contract instead of forcing all gateway behavior through the low-level
base64-message calls alone.

### D. Extend compile-time RSS convenience modules

Add gRPC convenience wrappers to the same compile-time override path that already injects
`edge/http/upstream/*`.

Recommended additions:

- `edge/grpc/client.rss`
- `edge/grpc/gateway.rss`

Those wrappers should stay thin. They should not contain protobuf codecs, JSON transcoding logic, or
gRPC frame parsers.

### E. Add runtime-owned descriptor configuration

Gateway support needs a place to load descriptor-backed method metadata that lives outside request
execution.

Recommended shape:

- descriptor sets or generated method registries are loaded as runtime configuration or compiled
  artifacts
- per-request RSS code selects from that metadata rather than constructing it
- method lookup, body mapping, and protobuf validation happen in native runtime code

## Milestones

### Milestone 0: Groundwork

- add `grpc` feature scaffolding
- define gRPC call frontiers and call refs
- update [`pd-edge/README.md`](../pd-edge/README.md) and
  [`pd-edge/docs/full-dag.md`](../pd-edge/docs/full-dag.md) with gRPC attach and detach edges
- add HTTP response trailer-write support
- decide whether gRPC reuses HTTP exchange handle identity or introduces a distinct call handle
- scaffold `edge/grpc/*` compile-time wrapper modules

### Milestone 1: Unary downstream and outbound calls over HTTP/2

- add `grpc::call::*` ABI
- support unary message framing
- emit and consume gRPC status and trailers
- add downstream server and upstream client integration tests

### Milestone 2: Unary gateway support with native transcoding

- add descriptor-backed method lookup in native runtime code
- add native HTTP or JSON to protobuf request transcoding for unary calls
- add native protobuf to HTTP or JSON response transcoding for unary calls
- add RSS-facing gateway helpers that select route and method without doing protobuf work
- add integration tests for downstream HTTP or JSON request to upstream gRPC unary bridging

### Milestone 3: Server-streaming

- support multiple response messages on one gRPC call
- keep trailer readiness separate from message completion
- ensure HTTP carrier reuse semantics stay correct
- define native downstream HTTP representation for gRPC server-streaming gateway flows rather than
  pushing stream encoding policy into RSS

### Milestone 4: Client-streaming and bidi streaming

- support multiple request messages
- add cancellation and early-close behavior
- verify that one stream failure does not corrupt unrelated HTTP/2 streams

### Milestone 5: Optional higher-level helpers

- broader grpc-gateway annotation coverage
- reflection if explicitly needed
- later gRPC-Web or Connect support if explicitly needed

## Testing Plan

### Unit tests

- message envelope encode or decode
- grpc-status and grpc-message parsing
- trailer mapping and error classification
- frontier transitions for unary and streaming calls
- descriptor lookup and method binding
- native protobuf or JSON transcode behavior
- gateway error mapping between HTTP and gRPC statuses

### Downstream integration tests

- unary downstream gRPC request handled over HTTP/2
- server trailers emitted correctly at end of stream
- downstream cancellation or early client close maps to the expected terminal frontier
- unary downstream HTTP or JSON request bridged to upstream gRPC without protobuf logic in RSS
- downstream HTTP error responses reflect transcode and validation failures deterministically

### Upstream integration tests

- unary outbound call over reused HTTP/2 carrier
- server-streaming outbound call preserves message boundaries
- one failing gRPC stream does not corrupt a sibling HTTP/2 stream on the same carrier session
- gateway path can target an upstream gRPC method through descriptor-backed native transcoding

## Risks

### Risk 1: treating gRPC as headers plus bytes only

If the implementation ignores trailer-driven completion and status semantics, it will fail on real
gRPC behavior immediately.

### Risk 2: trying to hide gRPC entirely inside the generic HTTP APIs

That would erase the semantic DAG boundary and make cancellation, message framing, and status
handling much harder to reason about.

### Risk 3: mixing protobuf tooling into the first milestone

The protocol framing and DAG work is already substantial without pulling in schema generation.

### Risk 4: assuming HTTP/1 fallback exists

gRPC must not silently degrade to HTTP/1.1 in the normal protocol path.

### Risk 5: pushing transcoding into RSS

If RSS code has to decode JSON, understand protobuf descriptors, or manually assemble protobuf
bytes, the system will be slow, fragile, and inconsistent with the DAG layering model.

### Risk 6: exposing only the low-level gRPC ABI

If the implementation stops at `grpc::call::*`, RSS can make raw gRPC calls, but a real
grpc-gateway-style user experience will still be missing.

## Recommendation

Implement gRPC as a true child DAG over the existing HTTP carrier family, land unary HTTP/2 support
first, and make server-side trailer emission an explicit prerequisite rather than an implicit
follow-up.

If gateway support is part of the product target, plan for a second native layer that owns
descriptor lookup plus protobuf or JSON decode and encode work. RSS should remain responsible for
routing and policy, not for transcoding itself.
