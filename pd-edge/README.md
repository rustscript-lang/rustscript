# pd-edge

`pd-edge` is the edge runtime crate for running VM programs at the edge.

This crate now ships three binaries with different scopes:

- `pd-edge-http-proxy`: full HTTP data plane runtime (proxy path + admin API + optional active control-plane client)
- `pd-edge-console`: interactive local console runtime (stdin/stdout/stderr host APIs + optional active control-plane client)
- `pd-edge-sample-echo-server`: local multi-protocol echo server for manual transport and host-ABI testing

## Contents

- [Binary Scope](#binary-scope)
  - [pd-edge-http-proxy](#pd-edge-http-proxy)
  - [pd-edge-console](#pd-edge-console)
  - [pd-edge-sample-echo-server](#pd-edge-sample-echo-server)
- [Quick Start](#quick-start)
  - [HTTP Proxy Mode](#http-proxy-mode)
  - [Console Mode](#console-mode)
  - [Sample Echo Server](#sample-echo-server)
- [HTTP Proxy Admin API](#http-proxy-admin-api)
- [CLI](#cli)
  - [pd-edge-http-proxy](#pd-edge-http-proxy-1)
  - [pd-edge-console](#pd-edge-console-1)
  - [pd-edge-sample-echo-server](#pd-edge-sample-echo-server-1)
- [Active Control-Plane RPC](#active-control-plane-rpc)
  - [Example](#example)
- [HTTP Proxy Performance Framework](#http-proxy-performance-framework)
  - [Latest Snapshot (2026-03-11, Local Windows x86_64 Dev Machine)](#latest-snapshot-2026-03-11-local-windows-x8664-dev-machine)
- [Layered DAGs](#layered-dags)
- [ABI Source of Truth](#abi-source-of-truth)
- [Release Artifacts](#release-artifacts)
- [Docker](#docker)
- [Codebase Layout](#codebase-layout)

## Binary Scope

### `pd-edge-http-proxy`

- Handles HTTP traffic on a data plane listener
- Exposes local admin APIs for program upload, health, metrics, telemetry, and debug session lifecycle
- Can run standalone (admin upload only) or with active control-plane RPC
- Registers HTTP host ABI + runtime host ABI + built-in IO overrides

Default listeners:

- Data plane: `0.0.0.0:8080`
- Admin API: `127.0.0.1:8081`

### `pd-edge-console`

- No HTTP proxy listeners
- Runs an interactive shell to load and execute VM programs locally
- Supports console host APIs: `console::stdin::read_line()`, `console::stdin::read_all()`, `console::stdout::write(text)`, `console::stdout::flush()`, `console::stderr::write(text)`, `console::stderr::flush()`
- Registers runtime host ABI (`runtime::sleep`, `rate_limit::allow`) plus the console host APIs above

### `pd-edge-sample-echo-server`

- Starts separate listeners for TCP, UDP, TLS, HTTP, HTTPS, WebSocket, secure WebSocket, WebRTC signaling, and a CONNECT forward proxy
- Echoes request bytes, datagrams, HTTP bodies, WebSocket frames, and WebRTC data-channel messages
- Uses a generated self-signed certificate for TLS, HTTPS, and `wss://`
- Enables manual end-to-end testing of the feature-gated transport surfaces without uploading a VM program

## Quick Start

### HTTP Proxy Mode

1. Start proxy + admin endpoints:

```powershell
cargo run -p pd-edge --bin pd-edge-http-proxy
```

2. Compile and upload sample program:

```powershell
cargo run -p pd-edge --example build_sample_program
```

3. Send traffic to data plane:

```powershell
curl -i "http://127.0.0.1:8080/anything" -H "x-client-id: demo-client"
```

The sample program in `examples/sample_proxy_program.*` uses `rate_limit::allow` and writes response headers/body via `http::response::*`.

### Console Mode

Start the interactive console:

```powershell
cargo run -p pd-edge --bin pd-edge-console
```

Optional: preload a local source or `.vmbc` program:

```powershell
cargo run -p pd-edge --bin pd-edge-console -- --program path\to\program.rss
```

Interactive commands:

- `.help`
- `.status`
- `.load <PATH>`
- `.run`
- `.quit`

### Sample Echo Server

Start the multi-protocol sample server with all current listeners enabled:

```powershell
cargo run -p pd-edge --bin pd-edge-sample-echo-server --features "webrtc http2"
```

Default listeners:

- TCP: `127.0.0.1:7001`
- UDP: `127.0.0.1:7002`
- TLS: `127.0.0.1:7003`
- HTTP: `127.0.0.1:7004`
- HTTPS: `127.0.0.1:7005`
- WebSocket: `127.0.0.1:7006`
- WSS: `127.0.0.1:7007`
- WebRTC signaling: `http://127.0.0.1:7008/offer`
- CONNECT forward proxy: `127.0.0.1:7009`

Notes:

- With feature `http2`, the HTTP listener also accepts cleartext h2c prior-knowledge requests on the same port.
- With feature `http2`, the HTTPS listener negotiates `h2` or `http/1.1` via ALPN on the same port.
- Without feature `http2`, the HTTP and HTTPS listeners remain HTTP/1.1 only.
- The forward proxy listener accepts `CONNECT` and then tunnels raw TCP bytes, which makes it usable with `examples/sample_forward_proxy_program.rss`.

## HTTP Proxy Admin API

Admin endpoints are served by `pd-edge-http-proxy` only:

- `PUT /program` (requires `content-type: application/octet-stream`)
- `GET /healthz`
- `GET /metrics`
- `GET /telemetry`
- `PUT /debug/session`
- `GET /debug/session`
- `DELETE /debug/session`

Program upload limit defaults to `1048576` bytes and can be changed with `--max-program-bytes`.

## CLI

### `pd-edge-http-proxy`

```text
Usage: pd-edge-http-proxy [options]

--proxy-addr <ADDR>                   Proxy/data-plane listen address (default: 0.0.0.0:8080)
--data-addr <ADDR>                    Alias for --proxy-addr
--admin-addr <ADDR>                   Admin listen address (default: 127.0.0.1:8081)
--max-program-bytes <BYTES>           Max program/upload size in bytes (default: 1048576)
--vm-fuel <UNITS>                     Enable cooperative VM fuel slices per request
--vm-fuel-check-interval <OPS>        Fuel check interval when --vm-fuel is enabled (default: 1)
--vm-epoch-deadline <TICKS>           Enable cooperative VM epoch slices per request (1 tick = 1ms wall clock)
--vm-epoch-check-interval <OPS>       Epoch check interval when --vm-epoch-deadline is enabled (default: 1)
--vm-execution-mode <MODE>            VM execution mode: async|threading (default: async)
--control-plane-url <URL>             Enable active control-plane RPC client
--edge-id <UUID>                      Explicit edge UUID for active control-plane mode
--edge-name <NAME>                    Edge display name (default: hostname)
--edge-id-path <PATH>                 UUID persistence path (default: .pd-edge/edge-id)
--control-plane-poll-interval-ms <MS> Poll interval for active control-plane mode
--control-plane-rpc-timeout-ms <MS>   RPC timeout for active control-plane mode
-V, --version
-h, --help
```

Notes:

- `--vm-fuel` and `--vm-epoch-deadline` are mutually exclusive.
- `--vm-fuel-check-interval` and `--vm-epoch-check-interval` are mutually exclusive.
- `--vm-epoch-check-interval` requires `--vm-epoch-deadline`.
- In epoch mode, the edge runtime advances the shared VM epoch every `1ms` with a Tokio timer, so `1` epoch tick maps to `1ms` of wall-clock time in `pd-edge`.

### `pd-edge-console`

```text
Usage: pd-edge-console [options]

--program <PATH>                      Optional source/.vmbc to load at startup
--max-program-bytes <BYTES>           Max program size in bytes (default: 1048576)
--vm-fuel <UNITS>                     Enable cooperative VM fuel slices per run
--vm-fuel-check-interval <OPS>        Fuel check interval when --vm-fuel is enabled (default: 1)
--vm-epoch-deadline <TICKS>           Enable cooperative VM epoch slices per run (1 tick = 1ms wall clock)
--vm-epoch-check-interval <OPS>       Epoch check interval when --vm-epoch-deadline is enabled (default: 1)
--control-plane-url <URL>             Enable active control-plane RPC client
--edge-id <UUID>                      Explicit edge UUID for active control-plane mode
--edge-name <NAME>                    Edge display name (default: hostname)
--edge-id-path <PATH>                 UUID persistence path (default: .pd-edge/edge-id)
--control-plane-poll-interval-ms <MS> Poll interval for active control-plane mode
--control-plane-rpc-timeout-ms <MS>   RPC timeout for active control-plane mode
-V, --version
-h, --help
```

The same interruption exclusivity rules apply in console mode: fuel and epoch cannot both be enabled for the same VM run.

### `pd-edge-sample-echo-server`

```text
Usage: pd-edge-sample-echo-server [options]

--tcp-addr <ADDR>                        TCP echo listen address (default: 127.0.0.1:7001)
--udp-addr <ADDR>                        UDP echo listen address (default: 127.0.0.1:7002)
--tls-addr <ADDR>                        TLS echo listen address (default: 127.0.0.1:7003)
--http-addr <ADDR>                       HTTP echo listen address (default: 127.0.0.1:7004)
--https-addr <ADDR>                      HTTPS echo listen address (default: 127.0.0.1:7005)
--websocket-addr, --ws-addr <ADDR>       WebSocket echo listen address (default: 127.0.0.1:7006)
--websocket-tls-addr, --wss-addr <ADDR>  Secure WebSocket echo listen address (default: 127.0.0.1:7007)
--webrtc-addr <ADDR>                     WebRTC signaling listen address (default: 127.0.0.1:7008)
--forward-proxy-addr <ADDR>              CONNECT forward proxy listen address (default: 127.0.0.1:7009)
-V, --version
-h, --help
```

Notes:

- TLS, HTTPS, and WSS listeners use a generated self-signed certificate.
- With feature `http2`, the HTTP listener also accepts cleartext h2c prior knowledge.
- With feature `http2`, the HTTPS listener negotiates h2 or HTTP/1.1 via ALPN.
- Without feature `http2`, the HTTP and HTTPS listeners serve HTTP/1.1 only.
- The forward proxy listener accepts `CONNECT` and then tunnels raw TCP bytes.
- The WebRTC listener accepts `POST /offer` and returns an SDP answer for a data-channel echo peer.
- Feature-gated listeners are only enabled when their crate feature is compiled in.

## Active Control-Plane RPC

Both binaries can run with active control-plane polling when `--control-plane-url` is set.

- Poll endpoint: `POST /rpc/v1/edge/poll`
- Result endpoint: `POST /rpc/v1/edge/result`

If `--control-plane-url` is not provided, control-plane-related flags are rejected.

Supported command types:

- `apply_program`
- `start_debug_session`
- `debug_command`
- `stop_debug_session`
- `get_health`
- `get_metrics`
- `get_telemetry`
- `ping`

`debug_command` supports both structured commands and raw debugger text. Raw text is passed through to the
interactive VM debugger, so remote clients can issue commands such as `epoch`, `epoch tick 1`,
`epoch deadline 3`, `epoch clear`, `fuel`, or `continue`.

### Example

```powershell
cargo run -p pd-edge --bin pd-edge-http-proxy -- `
  --control-plane-url "http://127.0.0.1:9100" `
  --edge-id "3f626ca0-c2ec-41a6-a5da-6fbc53aa857f" `
  --control-plane-poll-interval-ms "1000" `
  --control-plane-rpc-timeout-ms "5000"
```

## HTTP Proxy Performance Framework

Run the built-in framework to benchmark these scenarios:

- raw `pd-edge-http-proxy` (no program loaded)
- proxy with a compute-only program (no host calls; baseline workload)
- proxy with additive async HTTP host calls on top of the same workload (`get_method/get_path/get_header/get_body` + `set_header/set_body`) and explicit termination (no upstream target)

Detailed report with charts: [`docs/HTTP_PROXY_PERF_REPORT_2026-03-11.md`](docs/HTTP_PROXY_PERF_REPORT_2026-03-11.md)

Build the proxy binary once before running benchmarks so the framework does not accidentally run a stale executable:

```bash
cargo build -p pd-edge --bin pd-edge-http-proxy --release
```

Standard mode comparison (same benchmark shape, different VM execution mode):

```bash
cargo run -p pd-edge --example http_proxy_perf_framework --release -- \
  --vm-execution-mode async \
  --requests 12000 \
  --warmup-requests 2000 \
  --concurrency 128 \
  --json-out target/http_proxy_perf_mode_async.json

cargo run -p pd-edge --example http_proxy_perf_framework --release -- \
  --vm-execution-mode threading \
  --requests 12000 \
  --warmup-requests 2000 \
  --concurrency 128 \
  --json-out target/http_proxy_perf_mode_threading.json
```

Fuel-impact latency sweep (proxy harness, scenario `no_host_calls_program`):

```bash
cargo run -p pd-edge --example http_proxy_perf_framework --release -- \
  --vm-execution-mode async \
  --fuel-latency-sweep \
  --scenario no_host_calls_program \
  --requests 3000 \
  --warmup-requests 300 \
  --concurrency 64 \
  --fuel-latency-fuels "1,2,4,8,10,16,32,64,512,4096,50000" \
  --fuel-latency-check-intervals "1,4,16,64" \
  --json-out target/http_proxy_fuel_sweep_async.json

cargo run -p pd-edge --example http_proxy_perf_framework --release -- \
  --vm-execution-mode threading \
  --fuel-latency-sweep \
  --scenario no_host_calls_program \
  --requests 3000 \
  --warmup-requests 300 \
  --concurrency 64 \
  --fuel-latency-fuels "1,2,4,8,10,16,32,64,512,4096,50000" \
  --fuel-latency-check-intervals "1,4,16,64" \
  --json-out target/http_proxy_fuel_sweep_threading.json
```

Second harness (VM-only microbenchmark in `pd-vm`):

```bash
cargo test -p pd-vm --release --test jit_tests perf_cooperative_fuel_configuration_impacts_latency -- --ignored --nocapture
```

### Latest Snapshot (2026-03-11, Local Windows x86_64 Dev Machine)

Harness A: HTTP end-to-end (`pd-edge/examples/http_proxy_perf_framework.rs`)

| Scenario | Async (RPS / p50 / p95 / p99 ms) | Threading (RPS / p50 / p95 / p99 ms) |
|---|---:|---:|
| `raw_no_program` | `98,941 / 1.233 / 2.032 / 2.523` | `104,751 / 1.144 / 1.983 / 2.433` |
| `no_host_calls_program` | `43,846 / 2.801 / 4.866 / 5.945` | `42,757 / 2.613 / 6.357 / 9.942` |
| `host_calls_terminate` | `42,250 / 2.937 / 5.002 / 6.171` | `41,393 / 2.749 / 6.115 / 9.715` |

Fuel sweep (fixed interval `1`, scenario `no_host_calls_program`):

| Fuel | Async (p50 / p95 / p99 ms / RPS) | Threading (p50 / p95 / p99 ms / RPS) |
|---:|---:|---:|
| `1` | `105.298 / 138.863 / 146.916 / 600` | `76.729 / 124.234 / 135.187 / 715` |
| `8` | `9.134 / 11.424 / 27.760 / 6,688` | `9.254 / 15.927 / 27.765 / 5,827` |
| `64` | `2.054 / 3.890 / 5.488 / 27,917` | `2.156 / 3.773 / 4.252 / 27,694` |
| `512` | `1.376 / 2.521 / 3.460 / 42,025` | `1.501 / 2.678 / 3.070 / 39,060` |
| `4096` | `1.172 / 3.052 / 4.685 / 44,031` | `1.319 / 3.478 / 4.366 / 40,453` |
| `50000` | `1.423 / 2.424 / 2.990 / 42,500` | `1.073 / 3.328 / 6.703 / 44,639` |

Fuel-check-interval sweep (fixed fuel `50000`, scenario `no_host_calls_program`):

| Interval | Async (p50 / p95 / p99 ms / RPS) | Threading (p50 / p95 / p99 ms / RPS) |
|---:|---:|---:|
| `1` | `1.491 / 2.738 / 3.577 / 39,865` | `1.048 / 3.239 / 6.844 / 45,973` |
| `4` | `1.391 / 2.585 / 3.354 / 42,300` | `1.077 / 3.207 / 5.883 / 45,722` |
| `16` | `1.350 / 2.438 / 3.047 / 43,372` | `1.077 / 3.184 / 6.435 / 45,500` |
| `64` | `1.324 / 2.328 / 2.987 / 45,168` | `1.044 / 3.111 / 6.276 / 46,496` |

Harness B: VM microbenchmark (`pd-vm/tests/jit/perf_tests.rs::perf_cooperative_fuel_configuration_impacts_latency`)

Baseline (`fuel disabled`): median latency `16,487 us`

Fuel sweep (`fixed_check_interval=1`):

| Fuel | Median Latency (us) | Median Yields | Slowdown vs Baseline |
|---:|---:|---:|---:|
| `1` | `66,886` | `4,100,091` | `4.06x` |
| `2` | `48,050` | `2,050,045` | `2.91x` |
| `4` | `40,954` | `1,025,022` | `2.48x` |
| `8` | `37,086` | `512,511` | `2.25x` |
| `16` | `35,881` | `256,255` | `2.18x` |
| `32` | `34,548` | `128,127` | `2.10x` |
| `64` | `33,860` | `64,063` | `2.05x` |
| `128` | `33,601` | `32,031` | `2.04x` |
| `256` | `33,025` | `16,015` | `2.00x` |
| `512` | `33,544` | `8,007` | `2.03x` |
| `1024` | `33,002` | `4,003` | `2.00x` |
| `2048` | `33,222` | `2,001` | `2.02x` |
| `4096` | `32,882` | `1,000` | `1.99x` |
| `8192` | `32,980` | `500` | `2.00x` |

Fuel-check-interval sweep (`fixed_fuel=4096`):

| Interval | Median Latency (us) | Median Yields | Slowdown vs Baseline |
|---:|---:|---:|---:|
| `1` | `33,223` | `1,000` | `2.02x` |
| `2` | `36,420` | `1,000` | `2.21x` |
| `4` | `32,622` | `1,000` | `1.98x` |
| `8` | `32,898` | `1,000` | `2.00x` |
| `16` | `33,880` | `1,000` | `2.05x` |
| `32` | `33,361` | `1,000` | `2.02x` |
| `64` | `32,521` | `1,000` | `1.97x` |
| `128` | `32,741` | `1,000` | `1.99x` |
| `256` | `32,604` | `1,000` | `1.98x` |

Artifacts written by these runs:

- `target/http_proxy_perf_mode_async.json`
- `target/http_proxy_perf_mode_threading.json`
- `target/http_proxy_fuel_sweep_async.json`
- `target/http_proxy_fuel_sweep_threading.json`
- `target/pd_vm_perf_cooperative_fuel_2026-03-11.txt`

## Layered DAGs

Current state:

- HTTP is implemented today as an explicit graph in [`pd-edge/src/abi_impl/http/state.rs`](src/abi_impl/http/state.rs).
- Carrier-specific internals now split explicitly into [`pd-edge/src/abi_impl/http1/mod.rs`](src/abi_impl/http1/mod.rs) and [`pd-edge/src/abi_impl/http2/mod.rs`](src/abi_impl/http2/mod.rs). The generic exchange DAG remains in [`pd-edge/src/abi_impl/http/state.rs`](src/abi_impl/http/state.rs).
- HTTP/2 support now has explicit runtime-owned carrier state under the generic HTTP exchange DAG. When feature `http2` is enabled, outbound exchanges can negotiate `h2` through a shared upstream session pool, and the data-plane server also tracks downstream HTTP/2 sessions outside per-request VM contexts while VM code stays on `http::exchange::*`.
- TCP, TLS, and UDP transport state live in [`pd-edge/src/abi_impl/transport/state.rs`](src/abi_impl/transport/state.rs), with UDP host ABI in [`pd-edge/src/abi_impl/transport/udp.rs`](src/abi_impl/transport/udp.rs).
- WebSocket is implemented today as an explicit child DAG over outbound HTTP-upgrade handles in [`pd-edge/src/abi_impl/websocket/state.rs`](src/abi_impl/websocket/state.rs).
- WebRTC is implemented today as a request-scoped peer-connection/data-channel DAG in [`pd-edge/src/abi_impl/webrtc/mod.rs`](src/abi_impl/webrtc/mod.rs).
- Full cross-protocol graph: [`pd-edge/docs/full-dag.md`](docs/full-dag.md).
- `http`, `http2`, `tls`, `websocket`, and `webrtc` are feature-gated DAG families. The default build enables `http`, `tls`, and `websocket`.
- `SharedState` now carries both a shared upstream HTTP session pool and a downstream HTTP/2 session store so carrier-specific state is not owned solely by per-request `ProxyVmContext`.
- TCP, UDP, TLS, outbound HTTP exchanges, WebSocket connections, and WebRTC connections are exposed to programs through handle-based host calls:
  - `tcp::stream::downstream()` returns reserved socket handle `0`
  - `tcp::stream::default_upstream()` returns reserved socket handle `1`
  - `tcp::stream::{read, write, eof}` operate on those socket handles
  - `udp::socket::downstream()` returns reserved socket handle `0`
  - `udp::socket::default_upstream()` returns reserved socket handle `1`
  - `udp::socket::new()` allocates independent outbound UDP socket handles starting at `2`
  - `udp::socket::{bind, set_target, connect, get_phase, get_local_addr, get_peer_addr, send_text, recv_text, send_binary_base64, recv_binary_base64, close}` operate on any UDP socket handle
  - `tls::session::from_socket(sock)` projects a TLS-session handle from a socket handle
  - `tls::session::{set_alpn, set_verify, set_verify_hostname, set_trusted_certificate, set_certificate, set_private_key, set_sni, set_min_version, set_max_version}` configure the next outbound TLS handshake for that session handle
  - `tls::session::{is_present, handshake, get_phase, get_peer_name, get_server_name, get_alpn, get_peer_certificate, is_session_reused}` read back negotiated or observed TLS state
  - `http::exchange::default_upstream()` returns outbound exchange handle `1`
  - `http::exchange::new()` allocates independent outbound exchange handles starting at `2`
  - `http::exchange::{set_*, send, get_*}` operates on any outbound exchange handle
  - `websocket::connection::downstream()` returns reserved websocket handle `0`
  - `websocket::connection::default_upstream()` returns reserved websocket handle `1`
  - `websocket::connection::new()` allocates independent outbound websocket handles starting at `2`
  - `websocket::connection::{set_target, set_header, set_subprotocols, connect, send_text, read_text, send_binary_base64, read_binary_base64, eof, close, get_phase, get_subprotocol}` operate on any outbound websocket handle
  - `webrtc::connection::downstream()` returns reserved webrtc handle `0`
  - `webrtc::connection::default_upstream()` returns reserved webrtc handle `1`
  - `webrtc::connection::new()` allocates independent outbound webrtc handles starting at `2`
  - `webrtc::connection::{set_ice_servers, set_data_channel_label, set_remote_description, create_offer, create_answer, connect, send_text, read_text, send_binary_base64, read_binary_base64, eof, close, get_phase}` operate on any outbound webrtc handle
- Directional `http::upstream::*` APIs remain as aliases over handle `1`.
- Current runtime boundary:
  - generic `http::exchange::*` is the stable VM-facing request/response surface
  - feature `http2` currently adds explicit upstream session pooling plus downstream session tracking under the generic HTTP layer; it does not yet expose a VM-visible `http2::session::*` namespace
  - outbound UDP sockets are executable today
  - downstream UDP handle `0` is reserved but inactive in the current one-shot HTTP runtime
  - outbound WebSocket connections are executable today
  - downstream handle `0` currently exposes upgrade-candidate detection and phase inspection, but not a post-`101` frame loop yet
  - outbound WebRTC peer connections and data channels are executable today
  - downstream WebRTC handle `0` is reserved but inactive in the current one-shot HTTP runtime
  - `wss://` currently uses the default verifier/client configuration only; custom TLS-session overrides are rejected for websocket connects until the manual websocket TLS connector reaches parity with the HTTP client path
  - `proxy::pipe` and `proxy::tunnel` remain byte-stream only; UDP datagrams and WebRTC message queues are not adapted into that layer today

Core model:

- Each subsystem owns its own forward-only DAG.
- A node in one DAG may export a capability that becomes the ingress node of a deeper DAG, or the VM may allocate a sibling DAG instance directly such as a UDP socket, HTTP exchange, or WebRTC connection.
- The same DAG schema can be instantiated multiple times, for example once for downstream and once for upstream.
- Callers may jump directly to any reachable node if the jump is monotonic: all prerequisite edges are already satisfied or can be advanced forward by the engine.
- No subsystem is allowed to move another subsystem backward.
- Not every DAG is a byte stream. UDP preserves datagram boundaries, and WebRTC preserves data-channel message boundaries.

### TCP DAG

TCP is the outer transport DAG. It owns socket lifecycle, byte availability, and connection teardown.

Rules:

- `tcp.connected` is the outer capability that permits deeper protocol parsing.
- `tcp.rx` and `tcp.tx` are monotonic byte streams. Reading and writing advance offsets; they do not rewind.
- Half-close and full close are terminal forward states, not side channels.
- TLS attaches to the TCP DAG through the exported byte-stream capability, not by bypassing TCP state.
- The program-facing API is handle-based rather than direction-based. `0` and `1` are just the predefined handles for the current downstream and default upstream sockets.

```mermaid
flowchart TD
    A["tcp listener or tcp dial pending"] --> B["tcp accepted or tcp outbound dialed"]
    B --> C["tcp connected"]
    C --> D["tcp rx byte stream"]
    C --> E["tcp tx byte stream"]
    D --> F["tcp remote eof"]
    E --> G["tcp local eof"]
    C --> H["tcp error"]
    C --> I["tls ingress may attach here"]
    F --> J["tcp closed"]
    G --> J
    H --> J
```

### UDP DAG

UDP is a sibling transport DAG to TCP. It owns local bind configuration, remote target selection, datagram send and receive progress, and close or failure state.

Rules:

- `udp.bound` and `udp.target configured` are independent forward edges. A handle may set only a bind address, only a target, or both.
- `udp.connected` may be reached explicitly with `udp::socket::connect(handle)` or implicitly on the first `send_*` / `recv_*` call because the current runtime lazily connects outbound sockets.
- Datagram boundaries are preserved. `recv_text` and `recv_binary_base64` each consume at most one datagram.
- The program-facing API is handle-based: `0` is the reserved downstream placeholder, `1` is the default upstream socket, and `2+` are dynamically allocated sockets.
- In the current one-shot HTTP runtime, only outbound UDP handles execute the full DAG.
- UDP does not currently enter the proxy byte-stream layer because `proxy::pipe` and `proxy::tunnel` are stream-oriented.

```mermaid
flowchart TD
    A["udp socket allocated"] --> B["udp bind configured"]
    A --> C["udp target configured"]
    B --> C
    C --> D["udp connected"]
    D --> E["udp tx datagrams"]
    D --> F["udp rx datagrams"]
    D --> G["udp failed"]
    E --> H["udp closed"]
    F --> H
    G --> H
```

### TLS DAG

TLS is a DAG over the TCP byte stream. Its job is not just handshake; it also owns the mapping between ciphertext records and plaintext application bytes.

Rules:

- TLS enters only after `tcp.connected` exists and a TLS parser is attached to the TCP byte stream.
- `tls.session.selected` is a logical node, not a hardcoded code path. It may be reached by a full handshake or by session reuse.
- Outbound TLS configuration lives on the session node itself: ALPN policy, verification flags, trusted CA bundle, client certificate/key, SNI enablement, and min/max TLS version.
- The current runtime can enforce verification, trust roots, client authentication, SNI enablement, and TLS version bounds per session. `set_alpn` is currently a negotiated-ALPN policy check over the resulting exchange, not a low-level custom ClientHello generator.
- `tls.plaintext` is the exported capability for HTTP or another application protocol.
- `tls.close_notify`, transport close, and handshake failure are forward exits from the TLS DAG.
- The program-facing API is `tls::session::*`; session handles are derived from socket handles so a future subrequest can reuse the same calls.

```mermaid
flowchart TD
    A["tcp connected"] --> B["tls configured"]
    B --> C["tls client hello prepared"]
    C --> D["tls client hello sent"]
    D --> E["tls server hello received"]
    E --> F["tls server certificate received"]
    F --> G{"verification policy"}
    G -->|verify enabled| H["tls server certificate verified"]
    G -->|verify relaxed| I["tls verification skipped"]
    H --> J{"ALPN policy satisfied"}
    I --> J
    J -->|yes| K["tls plaintext ready"]
    J -->|no| L["tls failed"]
    K --> M["http ingress may attach here"]
    K --> N["tls close notify"]
    N --> O["tls closed"]
    L --> O
```

TLS session reuse is exactly why advancement must be generic. The system should not special-case reuse at every call site. Instead, the DAG engine should be able to satisfy the goal `tls.handshake complete` by selecting one of multiple legal forward paths:

- full handshake
- resumed session
- future variants such as 0-RTT, if supported later

### HTTP DAG

HTTP is the application DAG over the plaintext stream. Today it is represented in [`pd-edge/src/abi_impl/http/state.rs`](src/abi_impl/http/state.rs) as the generic request/response exchange layer, while HTTP/1.1 and HTTP/2 are carrier realizations beneath it.

Rules:

- `request.head` is eager and immutable. It comes from already-parsed request metadata, so exposing it is not additional network I/O.
- `request.body` starts unread and only advances when a host call or resolver consumes bytes.
- `exchange[1].request` starts as the default upstream draft seeded from `request.head`.
- `exchange[2+]` are additional outbound request/response DAG instances allocated by the VM.
- The VM-facing ABI stays generic: `http::exchange::*` and `http::upstream::*` do not hardcode HTTP/1.1 versus HTTP/2.
- `http::exchange::get_http_version()` and `http::upstream::response::get_http_version()` expose the realized response version after the exchange starts.
- With feature `http2`, outbound HTTPS exchanges can negotiate `h2` and dynamic exchanges may share one upstream connection when the client path permits multiplex.
- `response.output` starts empty and is populated by VM host calls for local response construction.
- Each `exchange[n].response` starts as `NotStarted` and becomes `Ready` only when the DAG actually needs response data for that handle.
- What exists today:
  - internal `http2.session.*` and `http2.stream.*` goals drive attachment, request commitment, response-head readiness, response-body readiness, close, and reset progression
  - explicit stream carrier refs are attached to upstream exchanges and real downstream HTTP/2 requests
  - upstream HTTP/2 session reuse and multiplex over the generic exchange ABI
  - downstream HTTP/2 request admission plus explicit session or stream frontier tracking in the data plane
  - GOAWAY and reset are modeled as session or stream frontier transitions rather than opaque connection failure flags
- What is still intentionally not exposed yet:
  - a VM-visible `http2::session::*` namespace
  - full connection-scoped downstream VM hosting for long-lived multi-stream sessions
- VM execution is not itself a DAG goal. Host calls read or mutate these nodes and may force progression, but execution is runtime control flow rather than a protocol frontier.

```mermaid
flowchart TD
    A["http ingress admitted"] --> B["request head ready"]
    A --> C["request body unread stream"]
    B --> D["exchange 1 request draft"]
    B --> E["exchange n allocated"]
    E --> F["exchange n request draft"]
    B --> G["response output draft"]
    C -. request body may feed .-> D
    C -. request body may feed .-> F
    C -. request body may feed .-> G
    D --> H["exchange 1 response ready"]
    F --> I["exchange n response ready"]
    H --> J["client response committed"]
    G --> J
    I -. response data may be copied into .-> G
```

### WebSocket DAG

WebSocket is a child DAG over an HTTP upgrade-capable request. It owns the `101` handshake, negotiated subprotocol, and bidirectional frame stream after upgrade.

Rules:

- The websocket DAG is entered only from an HTTP request node that is being used as an upgrade request.
- A websocket handle is the same reserved or dynamic handle number that already names the underlying outbound exchange: `0` for downstream, `1` for the default upstream exchange, and `2+` for additional allocated exchanges.
- `websocket::connection::connect(handle)` is an explicit `advance(websocket.open)` request. `send_*` and `read_*` can also advance implicitly if the handle is configured but not open yet.
- `send_text`, `read_text`, `send_binary_base64`, and `read_binary_base64` advance the frame-stream nodes, not the HTTP body DAG.
- `close` is a forward edge into `websocket.closing` and then `websocket.closed`.
- Today, only outbound handles execute the full frame DAG. Downstream handle `0` exposes the ingress node `upgrade-observed` so the model is documented and testable, but post-upgrade downstream frame execution still needs a persistent VM session runner.

```mermaid
flowchart TD
    A["http exchange n upgrade request"] --> B["websocket upgrade prepared"]
    B --> C["websocket handshake started"]
    C --> D["websocket open"]
    D --> E["websocket rx frame stream"]
    D --> F["websocket tx frame stream"]
    D --> G["websocket subprotocol negotiated"]
    E --> H["websocket close received"]
    F --> I["websocket close sent"]
    D --> J["websocket failed"]
    H --> K["websocket closed"]
    I --> K
    J --> K
```

### WebRTC DAG

WebRTC is a request-scoped peer-connection DAG that owns ICE configuration, SDP signaling state, data-channel readiness, and message I/O. Unlike WebSocket, it is not entered through an HTTP upgrade; the VM allocates or selects a WebRTC handle directly and drives signaling through host calls.

Rules:

- A WebRTC handle is handle-based: `0` is the reserved downstream placeholder, `1` is the default upstream connection, and `2+` are dynamically allocated outbound connections.
- `set_ice_servers` and `set_data_channel_label` configure the connection before the peer connection is created. Those fields become read-only once the peer exists.
- `set_remote_description`, `create_offer`, and `create_answer` advance the signaling nodes. `create_offer` also ensures the local data channel exists before the local description is published.
- `connect` is the explicit request to wait for `webrtc.open`. `send_*` and `read_*` can also force that advancement implicitly because they require an open data channel.
- `send_text`, `read_text`, `send_binary_base64`, and `read_binary_base64` advance message queues while preserving message boundaries.
- Today, only outbound handles execute the full DAG. Downstream handle `0` is reserved so the ABI shape is stable, but the current one-shot HTTP runtime does not host a downstream peer connection yet.
- WebRTC remains outside the proxy byte-stream layer today because data-channel messages are not adapted into `proxy::pipe` or `proxy::tunnel`.

```mermaid
flowchart TD
    A["webrtc handle allocated or selected"] --> B["webrtc configured"]
    B --> C["remote description set"]
    B --> D["local offer or answer created"]
    C --> D
    D --> E["peer connecting"]
    E --> F["data channel open"]
    F --> G["rx message queue"]
    F --> H["tx message queue"]
    F --> I["webrtc failed"]
    G --> J["webrtc closed"]
    H --> J
    I --> J
```

### Entering And Leaving A Deeper DAG

A deeper DAG is entered when the outer DAG publishes an ingress capability for it. Some DAGs in the current runtime, such as UDP and WebRTC, are sibling DAGs instead: the VM allocates them directly inside the same request scope rather than descending from a parent byte stream.

Examples:

- TCP exports `tcp.connected` plus the TCP byte stream. TLS can attach there.
- TLS exports `tls.plaintext stream`. HTTP can attach there.
- HTTP exports `http upgrade ready`. WebSocket can attach there.
- The VM may allocate a UDP socket directly. That starts a sibling transport DAG rather than descending through HTTP.
- The VM may allocate a WebRTC connection directly. Signaling and data-channel progression happen on that sibling DAG rather than through an HTTP upgrade.
- HTTP may export a response body stream that is handed back upward to TLS plaintext framing, then to TCP transmission.

Rules for entering:

- Entering a deeper DAG or allocating a sibling DAG does not replace the outer DAGs that already exist. TCP/TLS/HTTP still own their own transport and buffering history, while UDP and WebRTC own independent handle-scoped state.
- A deeper DAG can request more outer progress, but only through declared advance goals. A sibling DAG advances independently once its handle has been allocated and configured.
- The inner DAG cannot mutate outer history that has already been published.

Rules for leaving:

- You leave a deeper DAG when it reaches an exported egress node such as `http message complete`, `websocket.closed`, `tls close_notify`, or `tls handshake error`. Sibling DAGs such as UDP and WebRTC leave through their own terminal nodes such as `udp.closed`, `udp.failed`, `webrtc.closed`, or `webrtc.failed`.
- Leaving does not imply the outer DAG is done. The outer DAG may continue with another message, another handshake branch, or connection teardown, while sibling DAGs may continue or terminate independently.
- Returning to a shallower layer is valid if it is still a forward move in the combined graph.

### Generic Advancement Mechanism

To avoid writing one-off state machine code for every feature, each DAG should expose goals rather than bespoke step functions.

Suggested model:

- `advance(goal)`: move the subsystem to a requested node or exported capability.
- `goal` is declared in terms of a node, not a concrete procedure.
- each goal has one or more legal transition paths with explicit prerequisites and side effects.
- transitions are idempotent once their output node is published.
- transitions may consume I/O, parse buffered data, allocate derived state, or reuse cached state.

In that model, TLS session reuse becomes a normal transition set:

- `advance(tls.handshake complete)` may choose `full_handshake`
- or `advance(tls.handshake complete)` may choose `resume_session`

The caller asks for the node, not the path.

HTTP/2 now follows the same rule internally:

- `advance(http.exchange.response_ready)` on an HTTP/2-carried exchange may satisfy itself through
  - `advance(http2.session.open)`
  - `advance(http2.stream.attached)`
  - `advance(http2.stream.request_committed)`
  - `advance(http2.stream.response_head_ready)`
- `advance(http.exchange.body.next_chunk)` may satisfy itself through `advance(http2.stream.response_body_ready)`
- the generic exchange state stores explicit `HttpCarrierRef::UpstreamHttp2Stream { .. }` and `HttpCarrierRef::DownstreamHttp2Stream { .. }` attachments when those carriers exist

This is currently an internal runtime model. VM code still talks to the generic `http::exchange::*` surface rather than a separate `http2::*` ABI.

### Downstream And Upstream As Independent DAG Instances

The clean model is to treat downstream and upstream as separate DAG instances with explicit connection points.

- downstream TCP/TLS/HTTP describe bytes and messages coming from the client toward the VM
- upstream TCP/TLS/HTTP describe bytes and messages going from the VM toward the origin
- upstream UDP sockets and upstream WebRTC connections are sibling DAG families connected through VM host calls rather than attached to the HTTP byte-stream chain
- downstream UDP and downstream WebRTC handles currently exist only as reserved placeholders in the one-shot HTTP runtime
- runtime orchestration connects these DAG instances, but that control flow is not itself a DAG edge or goal
- [`pd-edge/docs/full-dag.md`](docs/full-dag.md) now shows those two views as separate downstream and upstream/exchange graphs rather than one merged graph

```mermaid
flowchart LR
    subgraph Downstream["Downstream"]
        D1["tcp downstream"]
        D2["tls downstream"]
        D3["http downstream request and response"]
        D1 --> D2 --> D3
    end

    subgraph Upstream["Upstream"]
        U3["http upstream request and response"]
        U2["tls upstream"]
        U1["tcp upstream"]
        UU["udp upstream sockets"]
        UW["webrtc upstream connections"]
        U3 --> U2 --> U1
    end
```

This gives the flexibility target:

- you can jump into TCP, UDP, TLS, HTTP, WebSocket, or WebRTC as long as the target node is reachable from the current frontier and that DAG family is compiled in
- you can jump back out from WebSocket to HTTP, then to TLS or TCP, when the inner DAG has exported a forward egress node; UDP and WebRTC exit through their own terminal nodes instead of rejoining the byte-stream stack
- you can materialize only the parts you need, late, including sibling DAGs that have no parent byte stream
- you do not need a custom driver for every feature such as TLS session reuse, early response generation, UDP datagram I/O, or WebRTC signaling and open-state transitions
- directional convenience APIs may remain as aliases, but the stable abstraction is the handle-based `tcp::stream::*` / `udp::socket::*` / `tls::session::*` / `http::exchange::*` / `websocket::connection::*` / `webrtc::connection::*` surface

Node ownership in current code:

- TCP, TLS, and UDP transport nodes and state: [`pd-edge/src/abi_impl/transport/`](src/abi_impl/transport/)
- HTTP nodes and resolver: [`pd-edge/src/abi_impl/http/state.rs`](src/abi_impl/http/state.rs)
- HTTP validation and map conversion helpers: [`pd-edge/src/abi_impl/http/helpers.rs`](src/abi_impl/http/helpers.rs)
- HTTP host-call entrypoints that mutate or read nodes: [`pd-edge/src/abi_impl/http/`](src/abi_impl/http/)
- WebSocket nodes and frame IO: [`pd-edge/src/abi_impl/websocket/`](src/abi_impl/websocket/)
- WebRTC nodes, signaling state, and data-channel IO: [`pd-edge/src/abi_impl/webrtc/`](src/abi_impl/webrtc/)
- data-plane orchestration and exchange resolution: [`pd-edge/src/runtime/http_plane/proxy_path.rs`](src/runtime/http_plane/proxy_path.rs)

## ABI Source of Truth

Host-call ABI metadata is centralized in `pd-edge-abi`:

- Rust constants + metadata: `edge_abi::FUNCTIONS`
- ABI version: `edge_abi::ABI_VERSION`
- Manifest: [`pd-edge-abi/abi.json`](../pd-edge-abi/abi.json)

For embedding with a VM:

```rust
let async_ops = edge::new_shared_vm_async_ops();
edge::register_http_plane_host_module(&mut vm, context, async_ops)?;
```

Use `register_host_module` when only runtime ABI is needed.

## Release Artifacts

Release workflow publishes Linux tarballs for:

- `pd-edge-http-proxy-<tag>-linux-x86_64.tar.gz`
- `pd-edge-console-<tag>-linux-x86_64.tar.gz`
- `pd-controller-<tag>-linux-x86_64.tar.gz`
- `pd-vm-run-<tag>-linux-x86_64.tar.gz`

## Docker

Docker release image currently packages `pd-edge-http-proxy`:

- `fffonion/pd-edge:<tag>`
- `fffonion/pd-edge:latest`

Run:

```powershell
docker run --rm -p 8080:8080 -p 8081:8081 fffonion/pd-edge:latest
```

## Codebase Layout

- `pd-edge/src/bin/pd-edge-http-proxy.rs`: HTTP proxy binary entrypoint and CLI
- `pd-edge/src/bin/pd-edge-console.rs`: console binary entrypoint and CLI
- `pd-edge/src/bin/pd-edge-sample-echo-server.rs`: multi-protocol sample echo server binary and CLI
- `pd-edge/src/sample_echo.rs`: shared TCP/UDP/TLS/HTTP/WebSocket/WebRTC sample echo server implementation
- `pd-edge/src/runtime.rs`: shared runtime state, telemetry, program apply/load, exports
- `pd-edge/src/runtime/http_plane/`: HTTP data/admin plane handlers
- `pd-edge/src/abi_impl/runtime.rs`: protocol-independent runtime host ABI
- `pd-edge/src/abi_impl/transport/`: TCP/TLS/UDP transport DAG state and host ABI
- `pd-edge/src/abi_impl/http/`: HTTP-specific host ABI
- `pd-edge/src/abi_impl/websocket/`: WebSocket DAG state and host ABI
- `pd-edge/src/abi_impl/webrtc/`: WebRTC DAG state and host ABI
- `pd-edge/src/active_control_plane.rs`: active control-plane poll/report loop
- `pd-edge/src/debug_session.rs`: on-demand debug session lifecycle
