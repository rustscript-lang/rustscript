# Proxy Quickstart

This proxy has two listeners:

- Data plane: `0.0.0.0:8080` (default)
- Admin endpoint: `127.0.0.1:8081` (default)

The admin endpoint accepts compiled VM bytecode via `PUT /program`.
`pd-edge` works standalone without `pd-controller`; in that mode you load programs directly through this admin endpoint.

## Codebase Layout

- `pd-edge/src/runtime.rs`: proxy skeleton (data/admin HTTP routes, upload, forwarding)
- `pd-edge/src/host_abi.rs`: all VM host ABI functions and registration
- `pd-edge/src/debug_session.rs`: on-demand debugger session lifecycle and VM attach logic

## Logging

Proxy emits colored logs (via `tracing`) for:

- access logs (method, path, status, latency) on both data/admin endpoints
- program load success/fail and validation/decode errors
- debug session start/stop and debugger attach events

Set log level with `RUST_LOG`, for example:

```powershell
$env:RUST_LOG="info"
cargo run -p pd-edge
```

## ABI Source of Truth

Proxy host-call ABI is centralized in the `edge_abi` crate:

- Rust constants + metadata: `edge_abi::FUNCTIONS`
- ABI version: `edge_abi::ABI_VERSION`
- Machine-readable manifest: [`pd-edge-abi/abi.json`](../pd-edge-abi/abi.json)

For Rust runtime embedding, use:

```rust
edge::register_host_module(&mut vm, context)?;
```

instead of registering host functions one-by-one.

## Sample Program

`pd-edge/examples/build_sample_program.rs` does:

1. Reads source from default path `examples/sample_proxy_program.rss` (resolved from the `pd-edge` crate root)
2. Compiles it with `vm::compile_source_file` (extension-driven flavor detection)
3. Encodes bytecode and uploads to `http://127.0.0.1:8081/program`

The sample source (`sample_proxy_program.rss`) declares required proxy host
functions in fixed ABI order and then:

1. Reads `x-client-id`
2. Allows at most 3 requests per 60-second window per client id using `http::rate_limit::allow`
3. Short-circuits with:
- `x-vm: allowed` + body `request allowed` when under limit
- `x-vm: rate-limited` + body `rate limit exceeded` when over limit

## Docker image

Release workflow publishes `fffonion/pd-edge:<tag>` and `fffonion/pd-edge:latest`.

Run standalone edge with published image:

```powershell
docker run --rm -p 8080:8080 -p 8081:8081 fffonion/pd-edge:latest
```

## Run + Upload (PowerShell)

1. Start the proxy:

```powershell
cargo run -p pd-edge
```

Version metadata:

```powershell
cargo run -p pd-edge -- --version
```

2. In another terminal, compile and upload sample source:

```powershell
cargo run -p pd-edge --example build_sample_program
```

Alternative sample source flavors are also available:
- `pd-edge/examples/sample_proxy_program.js`
- `pd-edge/examples/sample_proxy_program.lua`
- `pd-edge/examples/sample_proxy_program.scm`

You can pass a relative sample path explicitly, for example:

```powershell
cargo run -p pd-edge --example build_sample_program -- examples/sample_proxy_program.js
```

Expected output includes `control response: 204 No Content`.

### Compile with `pd-vm-run` and upload via Admin API

This path is useful when you want to run `pd-edge` without controller and explicitly manage bytecode artifacts.

1. Emit VMBC bytecode from source:

```powershell
New-Item -ItemType Directory -Force out | Out-Null
cargo run -p pd-vm --bin pd-vm-run -- --emit-vmbc out/sample_proxy_program.vmbc examples/sample_proxy_program.rss
```

2. Upload the compiled VMBC bytes to local admin API:

```powershell
curl -X PUT "http://127.0.0.1:8081/program" `
  -H "content-type: application/octet-stream" `
  --data-binary "@out/sample_proxy_program.vmbc"
```

Expected response status: `204 No Content`.

3. Hit data plane to verify:

```powershell
curl -i "http://127.0.0.1:8080/anything" -H "x-client-id: demo-client"
```

First 3 responses for the same `x-client-id`:

- Status: `200 OK`
- Header: `x-vm: allowed`
- Body: `request allowed`

4th response within 60 seconds:

- Status: `200 OK`
- Header: `x-vm: rate-limited`
- Body: `rate limit exceeded`

## Optional CLI Overrides

```powershell
cargo run -p pd-edge -- --data-addr "0.0.0.0:9000" --admin-addr "127.0.0.1:9001" --max-program-bytes "1048576"
```

If you do not provide `--control-plane-url`, `pd-edge` stays in standalone mode and only uses local admin APIs.

### Active Data-Plane Control RPC

The data plane can actively dial a remote control-plane endpoint and poll for commands.

Edge identity includes:

- edge UUID:
  - if `--edge-id` is provided, that UUID is used
  - otherwise `pd-edge` loads UUID from `--edge-id-path` (default `.pd-edge/edge-id`)
  - if the file does not exist, a new UUID is generated and persisted
- edge friendly name:
  - if `--edge-name` is provided, that value is used
  - otherwise `pd-edge` defaults to hostname (`HOSTNAME`/`COMPUTERNAME`)

Example with explicit UUID:

```powershell
cargo run -p pd-edge -- --control-plane-url "http://127.0.0.1:9100" --edge-id "3f626ca0-c2ec-41a6-a5da-6fbc53aa857f" --control-plane-poll-interval-ms "1000" --control-plane-rpc-timeout-ms "5000"
```

Example with persisted UUID file:

```powershell
cargo run -p pd-edge -- --control-plane-url "http://127.0.0.1:9100" --edge-id-path ".pd-edge/edge-id" --control-plane-poll-interval-ms "1000" --control-plane-rpc-timeout-ms "5000"
```

When `--control-plane-url` is set, `pd-edge` sends:

- `POST /rpc/v1/edge/poll`
- `POST /rpc/v1/edge/result`

### RPC Command Types

Poll responses can include one command:

- `apply_program`: push base64 VM bytecode and apply it
- `start_debug_session`: starts debug attach mode and returns generated nonce header/value
- `stop_debug_session`
- `get_health`
- `get_metrics`
- `get_telemetry`
- `ping`

`start_debug_session` auto-generates a random nonce (default header `x-pd-debug-nonce`) on the data plane. The debug session is automatically removed once the debugger client disconnects.

### Built-In Admin APIs

Local admin endpoint also exposes:

- `GET /healthz`
- `GET /metrics`
- `GET /telemetry`
- existing `PUT /program` and `/debug/session` lifecycle endpoints

## On-Demand VM Debugging

Start a debugger session on admin endpoint and target only requests that include a specific header.

1. Start debugger session:

```powershell
curl -X PUT "http://127.0.0.1:8081/debug/session" \
  -H "content-type: application/json" \
  -d "{\"header_name\":\"x-debug-vm\",\"header_value\":\"on\",\"tcp_addr\":\"127.0.0.1:9002\",\"stop_on_entry\":true}"
```

2. Connect a terminal client to debugger TCP port (example with netcat):

```bash
nc 127.0.0.1 9002
```

3. Send a matching request to data plane:

```powershell
curl -i "http://127.0.0.1:8080/anything" -H "x-debug-vm: on" -H "x-client-id: demo-client"
```

The VM for that request will attach to debugger and accept iterative `pdb` commands such as:
`break`, `break line`, `step`, `next`, `out`, `stack`, `locals`, `where`, `funcs`, `continue`.

4. Check session status:

```powershell
curl "http://127.0.0.1:8081/debug/session"
```

5. Stop debugger session:

```powershell
curl -X DELETE "http://127.0.0.1:8081/debug/session"
```
