# The Protocol DAG: How pd-edge Models Connection Lifecycles

*Auto-advancing state graphs, detachable sub-graphs, and why it removes the need for VM forking*

---

## The Problem With Flat State Machines

A proxy deals with layered protocols. One request may involve TCP, TLS, and HTTP, while upgrades such as WebSocket or CONNECT tunnels partially break that layering. A single flat connection state enum quickly becomes hard to reason about because each layer advances on a different timeline.

pd-edge models this with a protocol DAG instead of one monolithic state machine.

## The DAG Structure

The runtime keeps transport and exchange state as related but distinct graph components:

- TCP and TLS each keep their own downstream and upstream flow state.
- HTTP exchanges carry request, response, and attached transport state.
- Dynamic handles create additional branches when the script opens new exchanges or sockets.
- Protocol upgrades can detach a higher-layer branch and continue on the lower transport layer.

This is the main advantage of the DAG shape: layers can advance independently without collapsing all protocol state into one enum.

## Auto-Advancement: Lazy State Seeding

The graph does not need to materialize everything eagerly. When a request arrives through the built-in HTTP path, TCP and TLS may already be established by the surrounding runtime.

The transport graph is only seeded when the script actually asks for transport state. In practice that means APIs such as:

- `tcp::stream::get_phase(tcp::stream::downstream())`
- `tls::session::get_alpn(tls::session::from_socket(tcp::stream::downstream()))`

If a script only reads and rewrites HTTP-layer state, it never pays to materialize transport details it does not use.

## Concrete Example: HTTPS Reverse Proxy

Consider a script that sets an upstream target and forwards whether the downstream connection is using TLS:

```rust
use http;
use tcp;
use tls;

let upstream = http::exchange::default_upstream();
http::exchange::set_target(upstream, "backend.internal", 8080);
http::exchange::set_scheme(upstream, "http");

let downstream = tcp::stream::downstream();
let downstream_tls = tls::session::from_socket(downstream);
let proto = if tls::session::is_present(downstream_tls) { "https" } else { "http" };
http::exchange::set_header(upstream, "X-Forwarded-Proto", proto);
```

The sequence is:

1. **The HTTP exchange state already exists.** Request metadata and the default upstream node are present.
2. **The first TLS query seeds transport state.** The runtime reconstructs the downstream transport view from the current request context.
3. **The script configures the upstream exchange.** The transport details remain attached to the exchange rather than becoming ad hoc side state.
4. **The runtime resolves the request through the graph.** TCP and TLS flows advance as required by the selected upstream path.

The script expresses intent. The runtime owns the protocol mechanics.

## Detachable Sub-Graphs

Some protocol transitions are not simple phase changes. A WebSocket upgrade, for example, ends the HTTP exchange but continues on the same underlying transport. CONNECT tunneling does something similar for byte streams.

The DAG makes those transitions explicit:

- the HTTP branch reaches a terminal point
- a new lower-level branch is attached
- the transport state continues without pretending the HTTP exchange still exists

That is cleaner than encoding upgrades as special cases inside one large flat state machine.

## Dynamic Exchanges as Graph Branches

Scripts can create additional upstream exchanges. Each new exchange becomes its own branch with its own request, response, and attached transport state.

For example:

```rust
use http;

let auth = http::exchange::new();
http::exchange::set_target(auth, "auth-service.internal", 9090);
http::exchange::set_method(auth, "POST");
http::exchange::set_header(auth, "content-type", "application/json");
http::exchange::set_body(auth, http::request::get_header("Authorization"));

http::exchange::send(auth);
let status = http::exchange::get_status(auth);
if status != 200 {
    http::response::set_status(403);
    return;
}

let upstream = http::exchange::default_upstream();
http::exchange::set_target(upstream, "backend.internal", 8080);
```

The important point is structural, not stylistic: dynamic exchanges become separate branches rather than hidden mutable side tables with no lifecycle model.

## TLS Session Reuse and Why VM Forking Is Not Needed

One tempting optimization would be to freeze a VM after an expensive setup phase and fork it for later requests. `pd-edge` does not need that for transport reuse.

The reusable state that matters is runtime infrastructure state:

- TLS session cache entries
- connection pools
- downstream and upstream session stores

The per-request VM itself is cheap to reset or recreate. Each request gets a fresh execution state, while TLS resumption happens through the transport layer's own cache.

That separation is important:

- protocol reuse belongs in the runtime
- request execution state belongs in the VM

By keeping those concerns apart, the system avoids VM snapshot invalidation problems while still capturing the real reuse win.

## Where This Falls Short

- **Scripts still re-execute per request.** Expensive pure setup inside the script is not automatically shared across requests.
- **The graph is structurally richer than a flat state machine.** That improves correctness but increases runtime design complexity.
- **Session reuse helps only when the workload has reusable transport context.** High-cardinality upstream patterns still pay more setup cost.

## Summary

The protocol DAG lets `pd-edge` model transport and application-layer progress without flattening every lifecycle into one enum. Lazy seeding keeps unused transport state cheap, detachable branches make upgrades explicit, and transport-level caches capture reuse where it actually belongs. That is why session reuse lives in the runtime rather than in frozen VM state.

## Related

- [Async Suspension Without Coroutines](./v05-async-suspension.md)
- [Host Function ABI](./v06-host-function-abi.md)
- [Why We Didn't Choose a Functional Language](./v03-why-not-functional.md)
