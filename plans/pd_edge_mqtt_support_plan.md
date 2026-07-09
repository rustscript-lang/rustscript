# pd-edge MQTT Support Plan

## Summary

Add real MQTT support to `pd-edge` by modeling MQTT as its own message-oriented DAG family above the
existing transport DAGs instead of as a thin client wrapper.

The key design constraint is that MQTT must not be treated as:

- "just raw TCP with a packet codec"
- "WebSocket with a subprotocol"
- "another one-shot request API"

MQTT is a long-lived session protocol with explicit connect, keepalive, publish, subscribe, and
disconnect frontiers. The implementation should preserve that shape and attach it cleanly to the
existing `tcp`, `tls`, and `websocket` graphs.

Recommended conceptual layering:

- `tcp` = byte-stream transport
- `tls` = secure byte-stream transport over TCP
- `websocket` = optional HTTP-upgrade carrier for MQTT over WebSocket
- `mqtt` = session, publish, subscribe, and delivery semantics

## Goals

- Support outbound MQTT 5 client sessions as a first-class protocol family.
- Support direct MQTT over TCP, MQTT over TLS, and MQTT over WebSocket where the carrier permits.
- Keep the DAG approach explicit: MQTT must publish attach and detach edges against the existing
  `tcp`, `tls`, and `websocket` graphs.
- Add a VM-visible MQTT ABI that is handle-based and consistent with the existing `tcp`,
  `tls`, `http`, `websocket`, and `webrtc` APIs.
- Make long-lived connection hosting possible in `pd-edge-console` and the transport runtime
  without forcing MQTT into the one-shot HTTP request lifecycle.
- Leave room for later broker-facing downstream listener support.

## Non-Goals For The First Milestone

- Full broker implementation with retained-message storage, shared subscriptions, and cluster state
- MQTT-SN
- QoS 2 exactly-once delivery
- Sparkplug B or other profile-specific semantics
- Schema-aware payload decoding
- Session sharing across unrelated VM invocations

## Current State

The runtime already has the right lower-level building blocks:

- handle-based TCP and TLS transport DAGs live under
  [`pd-edge/src/abi_impl/transport/`](../pd-edge/src/abi_impl/transport/)
- outbound WebSocket connections already exist under
  [`pd-edge/src/abi_impl/websocket/`](../pd-edge/src/abi_impl/websocket/)
- raw connection hosting already exists in
  [`pd-edge/src/runtime/transport_plane.rs`](../pd-edge/src/runtime/transport_plane.rs)

Important current constraints:

- there is no `mqtt` namespace in [`pd-edge-abi/src/abi_spec/`](../../pd-edge/pd-edge-abi/src/abi_spec/)
- there is no `mqtt` implementation module under [`pd-edge/src/abi_impl/`](../pd-edge/src/abi_impl/)
- downstream WebSocket frame execution is still incomplete in the one-shot HTTP runtime, so
  downstream MQTT-over-WebSocket server hosting cannot be treated as already solved

So the current answer to "can MQTT be added as a helper around existing socket calls?" is: not if
the runtime wants to preserve the current DAG discipline and expose useful publish or subscribe
semantics to VM code.

## Recommended End State

### 1. Add `mqtt` as a sibling protocol family above `tcp`, `tls`, and `websocket`

MQTT should attach to already-published carrier capabilities:

- `tcp.connected -> mqtt.session.attachable`
- `tls.plaintext ready -> mqtt.session.attachable`
- `websocket.open + negotiated subprotocol mqtt -> mqtt.session.attachable`

Derived carrier path for MQTT over WebSocket:

- `tcp.connected -> tls.plaintext ready if secure -> http upgrade -> websocket.open -> mqtt.session.attachable`

This is the important modeling rule:

- TCP and TLS own transport bytes
- WebSocket owns HTTP upgrade and frame transport when that carrier is used
- MQTT owns CONNECT, CONNACK, keepalive, publish, subscribe, ack, and disconnect semantics

MQTT must not be fused back into the transport DAGs just because the packets travel on a byte
stream.

### 2. Model MQTT as session plus delivery frontiers

Recommended MQTT session frontier:

- `session configured`
- `carrier attached`
- `connect sent`
- `connack received`
- `session open`
- `keepalive pending`
- `disconnect sent`
- `closed`
- `failed`

Recommended publish or subscribe frontiers:

- `publish queued`
- `publish committed`
- `delivery observed`
- `delivery acknowledged`
- `subscription requested`
- `subscription active`
- `subscription removed`

The runtime does not need to expose every wire packet to the VM in the first milestone, but it does
need explicit frontiers so host calls can request meaningful goals such as "session open",
"subscription active", or "next delivery available".

### 3. Define MQTT attach and detach semantics explicitly

Recommended attach edges:

- `tcp.connected -> mqtt.session.attachable`
- `tls.plaintext ready -> mqtt.session.attachable`
- `websocket.open -> mqtt.session.attachable`

Recommended detach edges:

- `mqtt.closed -> carrier remains open or may close according to policy`
- `mqtt.failed -> carrier may still exist, but the MQTT DAG is terminal`
- `mqtt.publish delivery available -> VM-visible event queue`

Important rule:

- attaching MQTT does not replace the underlying `tcp`, `tls`, or `websocket` history
- leaving MQTT does not imply the carrier is done
- WebSocket-carried MQTT must still leave through `websocket.closed`, not directly through TCP

### 4. Add a VM-visible MQTT ABI

The first milestone should prefer a small, explicit handle-based namespace:

- `mqtt::connection::new()`
- `mqtt::connection::default_upstream()`
- `mqtt::connection::set_scheme(connection, "mqtt" | "mqtts" | "ws" | "wss")`
- `mqtt::connection::set_target(connection, host, port)`
- `mqtt::connection::set_client_id(connection, client_id)`
- `mqtt::connection::set_username(connection, username)`
- `mqtt::connection::set_password(connection, password)`
- `mqtt::connection::set_keep_alive_secs(connection, secs)`
- `mqtt::connection::set_clean_start(connection, enabled)`
- `mqtt::connection::connect(connection)`
- `mqtt::connection::get_phase(connection)`
- `mqtt::connection::disconnect(connection, reason_code, reason_text)`
- `mqtt::connection::publish_text(connection, topic, payload, qos, retain)`
- `mqtt::connection::publish_binary_base64(connection, topic, payload, qos, retain)`
- `mqtt::connection::subscribe(connection, filter, qos)`
- `mqtt::connection::unsubscribe(connection, filter)`
- `mqtt::connection::read_event(connection)`

`read_event(connection)` may return a map in the first milestone so the runtime can prove the event
model before splitting into more message-specific handle families.

### 5. Keep connection ownership explicit

Unlike HTTP/2 or HTTP/3, MQTT sessions are intentionally stateful across time:

- subscriptions live on the session
- keepalive timers live on the session
- in-flight acknowledgements live on the session

So the default rule should be:

- do not share MQTT sessions across unrelated VM invocations
- do not add a global reuse pool in the first milestone
- keep ownership local to the current VM run, console session, or transport connection runner

## Runtime Strategy

### 1. Start with outbound client support first

The first useful milestone is an outbound MQTT client path usable from:

- `pd-edge-console`
- direct VM tests
- request-scoped HTTP programs that do connect, publish, and disconnect work

This is much smaller than immediate broker hosting and still proves the MQTT DAG model.

### 2. Add persistent connection hosting second

MQTT becomes much more valuable once the runtime can host long-lived sessions.

That should happen in the transport runtime, not in the one-shot HTTP request runtime:

- `pd-edge/src/runtime/transport_plane.rs` already owns raw connection hosting
- MQTT can attach there without pretending to be HTTP
- the VM runner can remain active while publish or subscribe events continue over one carrier

### 3. Add downstream broker-facing listener support later

Broker-facing downstream mode should become a dedicated listener or a transport-runtime mode:

- plain MQTT listener over TCP
- secure MQTT listener over TLS
- later MQTT-over-WebSocket listener once downstream WebSocket execution is complete

The runtime should not block outbound support on full broker-mode work.

## Internal Architecture Changes

### A. Add new internal modules

Recommended new modules:

- `pd-edge/src/abi_impl/mqtt/mod.rs`
- `pd-edge/src/abi_impl/mqtt/model.rs`
- `pd-edge/src/abi_impl/mqtt/codec.rs`
- `pd-edge/src/abi_impl/mqtt/upstream.rs`
- `pd-edge/src/abi_impl/mqtt/downstream.rs`

The target shape is:

- `model.rs` owns frontier and state definitions
- `codec.rs` owns packet encode or decode work
- `upstream.rs` owns outbound client progression
- `downstream.rs` owns listener-side admission and session hosting

### B. Extend the ABI source of truth

Add a new ABI spec file under [`pd-edge-abi/src/abi_spec/`](../../pd-edge/pd-edge-abi/src/abi_spec/) for
`mqtt`.

This keeps the current rule intact:

- VM-visible protocol namespaces are declared centrally in `pd-edge-abi`
- runtime modules in `pd-edge` implement those host calls

### C. Add connection-scoped runners where needed

A request-scoped VM context is not enough for long-lived subscriptions.

Recommended runtime work:

- add a connection-scoped MQTT runner under the transport runtime
- keep packet IO and keepalive timers owned by the MQTT session state
- avoid using the one-shot HTTP finalization path as the main MQTT execution model

## Milestones

### Milestone 0: Groundwork

- add `mqtt` feature scaffolding
- define MQTT session and delivery frontiers
- add ABI symbols and no-op stubs
- update [`pd-edge/README.md`](../pd-edge/README.md) and
  [`pd-edge/docs/full-dag.md`](../pd-edge/docs/full-dag.md) with MQTT attach and detach edges

### Milestone 1: Outbound MQTT client over TCP and TLS

- add outbound connection handles
- implement CONNECT, CONNACK, PUBLISH, SUBSCRIBE, PING, and DISCONNECT
- support QoS 0 and QoS 1 only
- add console and direct-VM integration tests

### Milestone 2: MQTT over WebSocket

- allow MQTT attach from outbound WebSocket carriers
- require explicit subprotocol negotiation
- add tests for `ws://` and `wss://` carriers

### Milestone 3: Persistent session hosting in the transport runtime

- add connection-scoped MQTT VM runners
- support long-lived subscribe loops
- make message delivery work without forcing reconnect per VM invocation

### Milestone 4: Downstream broker-facing listeners

- add plain MQTT and secure MQTT listeners
- expose downstream handle admission and attach to the MQTT DAG
- decide whether any broker-specific state should become VM-visible

## Testing Plan

### Unit tests

- packet codec parsing and serialization
- session frontier transitions
- QoS 0 and QoS 1 acknowledgement behavior
- keepalive timeout and disconnect classification

### Upstream integration tests

- connect, publish, and disconnect against a local fixture
- subscribe and receive deliveries on one session
- TLS carrier attach works and negotiated state remains observable through the transport DAG
- MQTT-over-WebSocket attaches only after successful WebSocket open and subprotocol negotiation

### Downstream integration tests

- downstream listener admits a broker-facing session on TCP and TLS
- one connection can carry multiple publishes and subscriptions without resetting the VM
- disconnect and keepalive expiry produce the expected terminal frontier

## Risks

### Risk 1: forcing MQTT into the one-shot HTTP model

If the implementation treats MQTT as "HTTP but longer-lived", subscriptions and keepalive behavior
will become awkward immediately.

### Risk 2: hiding MQTT behind an opaque client library

If a high-level MQTT runtime owns reconnects, inflight state, and acknowledgement behavior, the
edge runtime will lose the explicit DAG control it already uses elsewhere.

### Risk 3: adding session reuse too early

Unlike HTTP session reuse, MQTT session reuse changes semantics because subscriptions and in-flight
state are carried on the session.

### Risk 4: treating WebSocket-carried MQTT as the default shape

MQTT-over-WebSocket is important, but it should remain an alternate carrier, not the canonical
session model.

## Recommendation

Implement MQTT as a true child DAG over `tcp`, `tls`, and `websocket`, start with outbound client
support plus connection-scoped runners, and explicitly defer broker-state-heavy work until the
session model is proven.
