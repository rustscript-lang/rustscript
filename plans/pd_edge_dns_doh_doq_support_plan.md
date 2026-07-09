# pd-edge DNS / DoH / DoQ Support Plan

## Summary

Add DNS to `pd-edge` as a protocol family with one shared semantic DAG and multiple carrier
realizations:

- raw DNS over UDP
- raw DNS over TCP
- DNS over HTTPS (DoH)
- DNS over QUIC (DoQ)

The key design constraint is that these must not be modeled as unrelated features.

Recommended conceptual layering:

- `dns` = query, response, record, and rcode semantics
- `udp` = datagram carrier for raw DNS
- `tcp` = byte-stream carrier for raw DNS over TCP
- `http` = carrier for DoH
- `quic` = carrier for DoQ

That keeps the current DAG approach intact while allowing different transports to satisfy the same
DNS query goals.

## Goals

- Support outbound DNS resolution over UDP and TCP.
- Support outbound DoH and DoQ using the existing HTTP and QUIC foundations already present in
  `pd-edge`.
- Keep the DAG approach explicit: DNS must attach and detach against `udp`, `tcp`, `http`, and
  `quic` rather than bypassing them.
- Add a VM-visible `dns` ABI that is transport-aware but still semantic-first.
- Leave room for downstream DNS, DoH, and DoQ listener support later.
- Keep transport selection and fallback policy explicit.

## Non-Goals For The First Milestone

- Full recursive resolver behavior
- DNSSEC validation
- Zone transfer support
- Authoritative zone storage
- Shared resolver cache with eviction or negative-cache policy
- DNS over TLS (DoT)
- Full EDNS option policy surface

## Current State

The runtime already has the lower-level families needed for the carriers:

- UDP and TCP transport state live under
  [`pd-edge/src/abi_impl/transport/`](../pd-edge/src/abi_impl/transport/)
- generic HTTP exchange state lives under
  [`pd-edge/src/abi_impl/http/`](../pd-edge/src/abi_impl/http/)
- HTTP/3 and QUIC-backed runtime work already exists under
  [`pd-edge/src/abi_impl/http3/`](../pd-edge/src/abi_impl/http3/)

Important current constraints:

- there is no `dns` namespace in [`pd-edge-abi/src/abi_spec/`](../../pd-edge/pd-edge-abi/src/abi_spec/)
- there is no `dns`, `doh`, or `doq` implementation under [`pd-edge/src/abi_impl/`](../pd-edge/src/abi_impl/)
- downstream UDP is still a reserved placeholder in the one-shot HTTP runtime, so raw downstream
  DNS listener support is not already available

So the current answer to "can DNS just be a helper around UDP sockets?" is: only for the smallest
possible client case. It does not fit the current architectural bar for reusable protocol families.

## Recommended End State

### 1. Split semantic DNS from carrier-specific realizations

Recommended layering:

- `dns` = question, answer, rcode, truncation, and response semantics
- `dns-udp` = raw DNS over UDP
- `dns-tcp` = raw DNS over TCP
- `doh` = DNS over HTTP request or response exchange
- `doq` = DNS over QUIC request streams

This keeps the ownership boundary clear:

- transport DAGs own bytes or datagrams
- HTTP owns request or response exchange semantics for DoH
- QUIC owns transport state for DoQ
- DNS owns query identity, answers, errors, and transport-independent response semantics

### 2. Define the DNS query frontier explicitly

Recommended generic DNS query nodes:

- `query draft`
- `carrier selected`
- `carrier attached`
- `wire request committed`
- `response headers available`
- `answer records available`
- `complete`
- `failed`

Recommended exported capabilities:

- `wire message readable`
- `rcode readable`
- `question readable`
- `answer set readable`

Carrier-specific nodes stay below this layer.

### 3. Define attach and detach edges for each carrier

Recommended raw DNS attach edges:

- `udp.connected or target-configured -> dns.query.attachable`
- `tcp.connected -> dns.query.attachable`

Recommended DoH attach edges:

- `http exchange attached -> dns.query.attachable`
- `tcp.connected -> tls.plaintext ready when secure -> http exchange attached -> dns.query.attachable`

Recommended DoQ attach edges:

- `quic connection ready -> dns.query.attachable`
- `udp socket ready -> quic connection ready -> dns.query.attachable`

Recommended detach edges:

- `dns.query.complete -> carrier remains reusable according to transport policy`
- `dns.query.failed -> carrier may remain reusable or may fail with the protocol-specific reason`

Important rule:

- DoH must attach through the HTTP DAG, not directly to TLS
- DoQ must attach through the QUIC DAG, not directly to UDP datagrams
- raw DNS over UDP and raw DNS over TCP remain separate carrier realizations even though they share
  the same semantic DNS query DAG above them

### 4. Add a VM-visible `dns` ABI

The first milestone should keep the VM surface simple and semantic-first:

- `dns::query::new()`
- `dns::query::default_upstream()`
- `dns::query::set_name(query, name)`
- `dns::query::set_type(query, rrtype)`
- `dns::query::set_class(query, rrclass)`
- `dns::query::set_transport(query, "udp" | "tcp" | "https" | "quic")`
- `dns::query::set_target(query, host, port)`
- `dns::query::set_path(query, path)` for DoH endpoints
- `dns::query::set_header(query, name, value)` for DoH metadata
- `dns::query::set_wire_base64(query, message)`
- `dns::query::send(query)`
- `dns::query::get_rcode(query)`
- `dns::query::get_answers(query)`
- `dns::query::get_wire_base64(query)`

That lets the runtime support both:

- high-level common queries
- raw wire-message passthrough when the VM wants full control

### 5. Make fallback policy explicit

Transport choice must not be hidden.

Recommended policy:

- `udp` is the default raw DNS path when the caller asks for raw DNS without a transport override
- `tcp` may be selected explicitly or after a transport policy fallback such as truncation
- `https` is an explicit DoH path, not an automatic side effect of TLS
- `quic` is an explicit DoQ path, not an automatic side effect of HTTP/3 enablement

This follows the same rule already used by the current protocol DAG design:

- callers ask for goals
- runtime chooses among legal forward paths
- cross-carrier fallback happens before the final carrier attachment is published

## Upstream And Downstream Strategy

### 1. Start with upstream DNS and DoH first

Upstream raw DNS over UDP and TCP and upstream DoH are the smallest useful features because:

- outbound UDP and TCP already exist
- outbound HTTP exchanges already exist
- no downstream listener runtime redesign is needed

### 2. Add upstream DoQ after the generic DNS layer is proven

DoQ should reuse the same semantic DNS query DAG, but it still needs a clean QUIC attach path.

It should not land as a one-off client hidden inside a helper.

### 3. Add downstream DoH before raw downstream DNS

Downstream DoH can reuse the current HTTP admission path:

- HTTP request admitted
- DoH handler attaches the DNS semantic layer
- VM sees a DNS query rather than raw HTTP details if it opts into the DNS API

Raw downstream DNS requires new UDP and TCP listener work, so it should follow later.

### 4. Add raw downstream DNS and DoQ listeners later

Raw downstream DNS and DoQ both require connection or datagram hosting outside the current one-shot
HTTP runtime. That is useful work, but it should not block upstream resolution support.

## Internal Architecture Changes

### A. Add new internal modules

Recommended new modules:

- `pd-edge/src/abi_impl/dns/mod.rs`
- `pd-edge/src/abi_impl/dns/model.rs`
- `pd-edge/src/abi_impl/dns/codec.rs`
- `pd-edge/src/abi_impl/dns/upstream.rs`
- `pd-edge/src/abi_impl/dns/downstream.rs`
- `pd-edge/src/abi_impl/dns/doh.rs`
- `pd-edge/src/abi_impl/dns/doq.rs`

The split should reflect the DAG model:

- `model.rs` owns generic DNS nodes and refs
- `codec.rs` owns wire parsing and serialization
- `doh.rs` owns HTTP attach work
- `doq.rs` owns QUIC attach work

### B. Extend the ABI source of truth

Add a `dns` ABI spec file under [`pd-edge-abi/src/abi_spec/`](../../pd-edge/pd-edge-abi/src/abi_spec/).

That keeps the same repo-wide rule intact:

- the VM-facing namespace is declared in `pd-edge-abi`
- `pd-edge` implements the runtime behavior

### C. Add downstream listener work where transport admission is missing

Downstream raw DNS support needs runtime changes beyond the HTTP plane:

- UDP listener admission for datagram-oriented DNS server work
- TCP listener admission for raw DNS-over-TCP
- later DoQ listener admission over QUIC

This work should align with the same attach and detach rules already used by the current transport
and HTTP runtime.

## Milestones

### Milestone 0: Groundwork

- add `dns` feature scaffolding
- define generic DNS query frontiers
- add ABI symbols and stubs
- update [`pd-edge/README.md`](../pd-edge/README.md) and
  [`pd-edge/docs/full-dag.md`](../pd-edge/docs/full-dag.md) with DNS attach and detach edges

### Milestone 1: Upstream raw DNS over UDP and TCP

- add wire codec and query state
- support explicit UDP and TCP transport selection
- add tests for common A, AAAA, TXT, and CNAME-style queries against local fixtures

### Milestone 2: Upstream DoH and downstream DoH

- attach DNS query state to existing HTTP exchanges
- support `application/dns-message` style requests first
- add downstream DoH request handling on the HTTP data plane

### Milestone 3: Upstream and downstream DoQ

- attach DNS query state to QUIC-backed request streams
- add DoQ client support first
- add listener support once the QUIC admission path is ready for downstream hosting

### Milestone 4: Raw downstream DNS listeners and optional policy features

- add raw UDP and TCP DNS listeners
- add explicit truncation and fallback handling
- later consider caching, policy, and resolver helpers

## Testing Plan

### Unit tests

- DNS header and question encoding or decoding
- response parsing and record extraction
- truncation handling
- generic DNS frontier transitions independent of the carrier

### Upstream integration tests

- UDP and TCP raw DNS queries against local fixtures
- DoH queries over HTTP/1.1, HTTP/2, and later HTTP/3 where the chosen DoH carrier permits
- DoQ queries over one QUIC connection with multiple independent exchanges

### Downstream integration tests

- downstream DoH requests attach cleanly through the HTTP DAG
- downstream raw DNS datagrams map to the DNS query DAG once UDP listener support exists
- downstream DoQ attaches through QUIC rather than bypassing it

## Risks

### Risk 1: implementing four unrelated protocol paths

If raw DNS, DoH, and DoQ are added as separate helpers, the runtime will lose the benefit of one
shared semantic DNS layer.

### Risk 2: hiding transport fallback inside helpers

Fallback such as UDP-to-TCP after truncation must remain an explicit carrier-selection rule.

### Risk 3: confusing DoH with generic HTTP proxying

DoH is an HTTP carrier for DNS semantics. It should not remain only an ad hoc `http::exchange`
program pattern if the goal is first-class protocol support.

### Risk 4: coupling DoQ directly to UDP

DoQ must attach through a real QUIC layer, not as a raw datagram trick.

## Recommendation

Implement DNS as one semantic DAG family with explicit carrier attach points to `udp`, `tcp`,
`http`, and `quic`; land upstream raw DNS and DoH first; then extend the same model to DoQ and
downstream listeners.
