# pd-edge DAG Implementation Review

## 1. `ProxyVmContext` is a God Object

**File:** `pd-edge/src/abi_impl/http/state.rs:562-605`

`ProxyVmContext` has **40+ fields** covering TCP, TLS, UDP, HTTP exchanges, WebSocket, WebRTC, proxy streams, rate limiting, and IO handles — all in one flat struct. This violates the layered-DAG model's own principle that *each subsystem owns its own state*.

The struct is lock-guarded as a whole (`Arc<Mutex<ProxyVmContext>>`), meaning any host call that reads even one field takes a lock on the entire context.

**Improvement:** Split into typed, independently-lockable subsystem structs:

```rust
pub struct ProxyVmContext {
    pub request:     Arc<Mutex<HttpRequestState>>,
    pub tcp:         Arc<Mutex<TcpSubsystemState>>,
    pub tls:         Arc<Mutex<TlsSubsystemState>>,
    pub http:        Arc<Mutex<HttpExchangeSubsystemState>>,
    pub udp:         Arc<Mutex<UdpSubsystemState>>,
    pub websocket:   Arc<Mutex<WebSocketSubsystemState>>,
    pub webrtc:      Arc<Mutex<WebRtcSubsystemState>>,  // cfg-gated
    pub proxy:       Arc<Mutex<ProxyStreamSubsystemState>>,
    pub rate_limiter: SharedRateLimiter,
    pub io:          Arc<Mutex<EdgeIoSubsystemState>>,
}
```

This allows subsystem-independent host calls (e.g. a UDP read and an HTTP response write) to run concurrently without contention.

---

## 2. Body-Reading Code is Duplicated Between Inbound and Upstream

**File:** `http/state.rs:84-190` (`InboundRequestBodyState`) and `http/state.rs:304-456` (`UpstreamResponseBodyState`)

Both structs implement the same interface — `read_next_chunk`, `read_next_line`, `read_all`, `eof`, `ensure_readable_byte` — with the same buffering logic (`buffered: Vec<u8>`, `read_offset: usize`, `eof: bool`). The only difference is the source (`Body::frame()` vs `reqwest::Response::chunk()` vs `hyper::body::Incoming::frame()`).

**Improvement:** Extract a shared `BufferedByteStream` struct with a pluggable source trait:

```rust
struct BufferedByteStream {
    buffered: Vec<u8>,
    read_offset: usize,
    eof: bool,
}

impl BufferedByteStream {
    async fn pull_next_chunk_from<S: AsyncByteSource>(&mut self, src: &mut S) -> Result<(), VmError>;
    async fn read_next_chunk(&mut self, max_bytes: usize) -> Result<Vec<u8>, VmError>;
    async fn read_next_line(&mut self) -> Result<Vec<u8>, VmError>;
    async fn read_all(&mut self) -> Result<Vec<u8>, VmError>;
    async fn eof(&mut self) -> Result<bool, VmError>;
}
```

Eliminates ~150 lines of duplicated logic.

---

## 3. Pervasive `if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE` Branching

**File:** `http/state.rs` — appears ~25 times throughout

Almost every exchange-touching function repeats:

```rust
if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
    // operate on guard.outbound_request / guard.upstream_response
} else {
    let exchange = guard.outbound_exchanges.get_mut(&handle).ok_or_else(|| ...)?;
    // operate on exchange.request / exchange.response
}
```

This is a missing abstraction: the default upstream is conceptually just another exchange stored separately instead of in the `outbound_exchanges` map.

**Improvement:** Pre-populate `outbound_exchanges.insert(1, HttpOutboundExchangeState::from_request_head(...))` in `from_http_request`. Remove the duplicate in-struct fields (`outbound_request`, `upstream_response`, `upstream_latency_ms`, `default_upstream_attached_transport`, `default_upstream_websocket`). The entire `if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE` branching pattern disappears.

---

## 4. `TcpSocketState` and `UdpSocketState` Are Near-Identical

**File:** `transport/state.rs:188-413`

Both structs share the same fields (`present`, `bind_address`, `target`, `local_address`, `peer_address`, `failure_message`) and the same methods (`set_bind_address`, `set_target`, `mark_connected`, `mark_closed`, `mark_failed`, `is_present`, `phase`, `bind_address`, `target`, `local_address`, `peer_address`). The only differences are the `phase` enum type and that TCP adds `read_eof` + `mark_upgraded_tls`/`mark_http_attached`.

**Improvement:** Extract a shared `SocketAddressState` inner struct:

```rust
struct SocketAddressState {
    present: bool,
    bind_address: Option<String>,
    target: Option<String>,
    local_address: Option<String>,
    peer_address: Option<String>,
    failure_message: Option<String>,
}

struct TcpSocketState { core: SocketAddressState, phase: TcpSocketPhase, read_eof: bool }
struct UdpSocketState { core: SocketAddressState, phase: UdpSocketPhase }
```

Eliminates ~100 lines of duplicated implementation.

---

## 5. WebSocket Read Methods Triplicate Ping/Pong/Close Handling

**File:** `websocket/state.rs:128-243`

`read_text`, `read_binary_bytes`, and `read_binary_base64` are three separate async methods with identical `Ping → auto-pong`, `Pong → skip`, `Close → record_close_frame + return None`, `Err → set eof + return Err`, `None → set eof + return None` boilerplate. Only the `Message::Text` vs `Message::Binary` arms differ.

**Improvement:** Factor out a `read_next_frame` method that returns a typed enum:

```rust
enum FrameEvent { Text(String), Binary(Vec<u8>), Eof, Error(VmError) }

async fn read_next_frame(&mut self) -> FrameEvent { /* ping/pong/close handled once here */ }
```

Then `read_text`, `read_binary_bytes`, `read_binary_base64` each just call `read_next_frame` in a loop and assert the type. Eliminates ~80 lines of duplication and makes it easier to expose ping to the VM later.

---

## 6. WebRTC Phase Tracking is Partially Out-of-Band (Correctness Risk)

**File:** `webrtc/mod.rs:221-239`

The `WebRtcConnectionState::phase` field can diverge from the actual runtime state stored asynchronously in `WebRtcIoState`. Phase is correct only *after* calling `refresh_async_state()`, which is called in some places but not all. `is_open()` bypasses phase and queries `io.is_open()` directly, while `eof()` calls `refresh_async_state()` first — inconsistent.

The same `refresh_*` pattern exists independently in `WebSocketConnectionState::refresh_close_state()` — duplicated pattern across two subsystems.

**Improvement:** Either make `phase()` take `&mut self` and refresh inline, or store phase in a shared `Arc<AtomicU8>` between the DAG node and the runtime state so reads are always current. Also unify the `refresh_async_state` / `refresh_close_state` pattern into a shared trait used by both WebSocket and WebRTC.

---

## 7. Proxy Blocked-Retry Uses 1ms Sleep Loop

**File:** `proxy.rs:293-317`

When the pipe's read side is blocked (exchange write side not yet closed), the code busy-waits:

```rust
ProxyReadStep::Blocked => {
    sleep(BLOCKED_RETRY_DELAY).await;  // 1ms polling
}
```

On a high-concurrency proxy, thousands of blocked pipes each fire a 1ms timer — wasted wakeups and scheduler pressure.

**Improvement:** Add a `Notify` to the exchange state that fires when the write side is closed. The `Blocked` path then `notified().await` instead of sleeping. This is exactly the pattern WebRTC already uses with `inbox_notify` and `open_notify`.

---

## 8. Custom TLS Client Rebuilt Per-Request

**File:** `http/state.rs:1355-1433`

When `tls_flow.requires_custom_client()` is true (custom cert, disabled verification, pinned CA, etc.), a brand-new `reqwest::Client` is built on every exchange request. `reqwest::Client` construction allocates a full connection pool and TLS configuration — expensive for a hot path.

**Improvement:** Cache the custom `reqwest::Client` using a key derived from `TlsSessionCacheKey` (already computed for TLS session caching). A parallel `SharedUpstreamClientCache` keyed by `TlsSessionCacheKey` would let requests with the same TLS configuration reuse the pooled client.

---

## Summary

| # | Issue | Kind | Impact |
|---|-------|------|--------|
| 1 | `ProxyVmContext` god object + coarse lock | Architecture | Lock contention, modularity |
| 2 | Body-read logic duplicated inbound/upstream | Code quality | ~150 lines |
| 3 | `if DEFAULT_UPSTREAM_EXCHANGE_HANDLE` everywhere | Architecture | ~25 branch sites |
| 4 | `TcpSocketState`/`UdpSocketState` near-identical | Code quality | ~100 lines |
| 5 | WebSocket reads triplicate Ping/Pong/Close | Code quality | ~80 lines |
| 6 | WebRTC phase staleness / `refresh_*` duplication | Correctness risk | Latent divergence bug |
| 7 | Proxy blocked-retry uses 1ms sleep loop | Performance | CPU wakeups at scale |
| 8 | Custom TLS client rebuilt per-request | Performance | Latency, GC pressure |

**Priority order:** #6 (correctness) > #3 (architecture, eliminates the most code) > #7, #8 (performance) > #1 (architecture, bigger refactor) > #2, #4, #5 (code quality).
