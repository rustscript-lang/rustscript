# pd-edge

`pd-edge` is the edge runtime crate for running VM programs at the edge.

This crate now ships two binaries with different scopes:

- `pd-edge-http-proxy`: full HTTP data plane runtime (proxy path + admin API + optional active control-plane client)
- `pd-edge-console`: interactive local console runtime (stdin/stdout/stderr host APIs + optional active control-plane client)

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

### `pd-edge-console`

```text
Usage: pd-edge-console [options]

--program <PATH>                      Optional source/.vmbc to load at startup
--max-program-bytes <BYTES>           Max program size in bytes (default: 1048576)
--control-plane-url <URL>             Enable active control-plane RPC client
--edge-id <UUID>                      Explicit edge UUID for active control-plane mode
--edge-name <NAME>                    Edge display name (default: hostname)
--edge-id-path <PATH>                 UUID persistence path (default: .pd-edge/edge-id)
--control-plane-poll-interval-ms <MS> Poll interval for active control-plane mode
--control-plane-rpc-timeout-ms <MS>   RPC timeout for active control-plane mode
-V, --version
-h, --help
```

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

Detailed report with charts: [`docs/HTTP_PROXY_PERF_REPORT_2026-03-07.md`](docs/HTTP_PROXY_PERF_REPORT_2026-03-07.md)

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
  --fuel-latency-fuels "1,8,64,512,4096,50000" \
  --fuel-latency-check-intervals "1,4,16,64" \
  --json-out target/http_proxy_fuel_sweep_async.json

cargo run -p pd-edge --example http_proxy_perf_framework --release -- \
  --vm-execution-mode threading \
  --fuel-latency-sweep \
  --scenario no_host_calls_program \
  --requests 3000 \
  --warmup-requests 300 \
  --concurrency 64 \
  --fuel-latency-fuels "1,8,64,512,4096,50000" \
  --fuel-latency-check-intervals "1,4,16,64" \
  --json-out target/http_proxy_fuel_sweep_threading.json
```

Second harness (VM-only microbenchmark in `pd-vm`):

```bash
cargo test -p pd-vm --release --test perf_tests perf_cooperative_fuel_configuration_impacts_latency -- --ignored --nocapture
```

### Latest Snapshot (2026-03-07, Local Windows x86_64 Dev Machine)

Harness A: HTTP end-to-end (`pd-edge/examples/http_proxy_perf_framework.rs`)

| Scenario | Async (RPS / p50 / p95 / p99 ms) | Threading (RPS / p50 / p95 / p99 ms) |
|---|---:|---:|
| `raw_no_program` | `80,753 / 1.485 / 2.612 / 3.195` | `84,774 / 1.414 / 2.544 / 3.207` |
| `no_host_calls_program` | `31,214 / 3.918 / 6.972 / 8.831` | `26,598 / 3.812 / 10.454 / 30.124` |
| `host_calls_terminate` | `25,383 / 4.768 / 8.925 / 11.742` | `27,021 / 4.172 / 9.454 / 14.503` |

Fuel sweep (fixed interval `1`, scenario `no_host_calls_program`):

| Fuel | Async (p50 / p95 / p99 ms / RPS) | Threading (p50 / p95 / p99 ms / RPS) |
|---:|---:|---:|
| `1` | `error` | `error` |
| `8` | `error` | `error` |
| `64` | `2.942 / 6.614 / 9.705 / 18,565` | `3.717 / 5.890 / 8.271 / 16,682` |
| `512` | `1.845 / 3.838 / 5.393 / 30,841` | `1.953 / 6.535 / 11.394 / 24,239` |
| `4096` | `1.575 / 3.193 / 4.811 / 36,327` | `1.577 / 9.448 / 15.266 / 24,069` |
| `50000` | `1.663 / 3.420 / 4.755 / 34,399` | `2.275 / 6.642 / 12.567 / 22,095` |

Fuel-check-interval sweep (fixed fuel `50000`, scenario `no_host_calls_program`):

| Interval | Async (p50 / p95 / p99 ms / RPS) | Threading (p50 / p95 / p99 ms / RPS) |
|---:|---:|---:|
| `1` | `1.668 / 3.005 / 4.271 / 35,320` | `2.095 / 5.502 / 11.535 / 24,528` |
| `4` | `1.682 / 2.890 / 3.966 / 35,947` | `1.695 / 5.410 / 10.466 / 28,842` |
| `16` | `1.711 / 3.021 / 4.313 / 34,910` | `1.531 / 4.711 / 8.795 / 32,238` |
| `64` | `1.808 / 3.670 / 5.534 / 32,247` | `1.356 / 4.129 / 9.361 / 35,537` |

Harness B: VM microbenchmark (`pd-vm/tests/perf_tests.rs::perf_cooperative_fuel_configuration_impacts_latency`)

Baseline (`fuel disabled`): median latency `42,667 us`

Fuel sweep (`fixed_check_interval=1`):

| Fuel | Median Latency (us) | Median Yields | Slowdown vs Baseline |
|---:|---:|---:|---:|
| `1` | `76,903` | `4,100,147` | `1.80x` |
| `2` | `59,548` | `2,050,073` | `1.40x` |
| `4` | `49,541` | `1,025,036` | `1.16x` |
| `8` | `46,577` | `512,518` | `1.09x` |
| `16` | `43,663` | `256,259` | `1.02x` |
| `32` | `43,189` | `128,129` | `1.01x` |
| `64` | `42,624` | `64,064` | `1.00x` |
| `128` | `42,370` | `32,032` | `0.99x` |
| `256` | `41,822` | `16,016` | `0.98x` |
| `512` | `42,189` | `8,008` | `0.99x` |
| `1024` | `42,078` | `4,004` | `0.99x` |
| `2048` | `47,115` | `2,002` | `1.10x` |
| `4096` | `57,562` | `1,001` | `1.35x` |
| `8192` | `56,234` | `500` | `1.32x` |

Fuel-check-interval sweep (`fixed_fuel=4096`):

| Interval | Median Latency (us) | Median Yields | Slowdown vs Baseline |
|---:|---:|---:|---:|
| `1` | `50,944` | `1,001` | `1.19x` |
| `2` | `53,379` | `1,001` | `1.25x` |
| `4` | `55,211` | `1,001` | `1.29x` |
| `8` | `60,637` | `1,001` | `1.42x` |
| `16` | `58,917` | `1,001` | `1.38x` |
| `32` | `52,209` | `1,001` | `1.22x` |
| `64` | `53,674` | `1,000` | `1.26x` |
| `128` | `55,537` | `1,000` | `1.30x` |
| `256` | `51,369` | `1,000` | `1.20x` |

Artifacts written by these runs:

- `target/http_proxy_perf_mode_async.json`
- `target/http_proxy_perf_mode_threading.json`
- `target/http_proxy_fuel_sweep_async.json`
- `target/http_proxy_fuel_sweep_threading.json`
- `target/pd_vm_perf_cooperative_fuel_2026-03-07.txt`

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
- `pd-edge/src/runtime.rs`: shared runtime state, telemetry, program apply/load, exports
- `pd-edge/src/runtime/http_plane/`: HTTP data/admin plane handlers
- `pd-edge/src/abi_impl/runtime.rs`: protocol-independent runtime host ABI
- `pd-edge/src/abi_impl/http/`: HTTP-specific host ABI
- `pd-edge/src/active_control_plane.rs`: active control-plane poll/report loop
- `pd-edge/src/debug_session.rs`: on-demand debug session lifecycle
