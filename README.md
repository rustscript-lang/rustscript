# project-d

`project-d` is a Rust workspace for programmable edge data planes driven by a VM and a central controller.

## Workspace layout

- `pd-vm`: VM runtime, compiler (`.rss`, `.js`, `.lua`, `.scm`), debugger, and tools.
- `pd-edge-abi`: ABI contract shared by VM host functions and edge runtime.
- `pd-edge`: edge data plane runtime (data listener + local admin endpoint + active control-plane RPC client).
- `pd-controller`: control plane service (RPC endpoints, state persistence, program/version management, remote debug orchestration, and Web UI at `/ui`).

## Prerequisites

- Rust (edition 2024 compatible toolchain)
- Bun (for `pd-controller/webui`)

## Build and test

```powershell
cargo build --workspace
cargo test --workspace
```

## Published Docker images

On tagged releases, GitHub Actions publishes:

- `fffonion/pd-edge:<tag>` and `fffonion/pd-edge:latest`
- `fffonion/pd-controller:<tag>` and `fffonion/pd-controller:latest`

Example:

```powershell
docker pull fffonion/pd-edge:latest
docker pull fffonion/pd-controller:latest
```

## Run with Docker

1. Create a shared Docker network:

```bash
docker network create project-d-net
```

2. Start controller:

```bash
docker run -d --name pd-controller \
  --network project-d-net \
  -p 9100:9100 \
  -e CONTROLLER_ADDR=0.0.0.0:9100 \
  -e CONTROLLER_STATE_PATH=/data/state.json \
  -v pd-controller-data:/data \
  fffonion/pd-controller:latest
```

3. Start edge and connect it to controller:

```bash
docker run -d --name pd-edge \
  --network project-d-net \
  -p 8080:8080 \
  -p 8081:8081 \
  -v pd-edge-data:/var/lib/pd-edge \
  fffonion/pd-edge:latest \
  --control-plane-url http://pd-controller:9100 \
  --edge-name edge-docker-1 \
  --edge-id-path /var/lib/pd-edge/edge-id \
  --data-addr 0.0.0.0:8080 \
  --admin-addr 0.0.0.0:8081 \
  --control-plane-poll-interval-ms 1000 \
  --control-plane-rpc-timeout-ms 5000
```

4. Open WebUI:

```text
http://127.0.0.1:9100/ui
```

## Run from Release Binaries

1. Download artifacts from GitHub Releases:

```bash
TAG=v0.1.0
curl -LO https://github.com/fffonion/project-d/releases/download/${TAG}/pd-controller-${TAG}-linux-x86_64.tar.gz
curl -LO https://github.com/fffonion/project-d/releases/download/${TAG}/pd-edge-${TAG}-linux-x86_64.tar.gz
curl -LO https://github.com/fffonion/project-d/releases/download/${TAG}/pd-vm-run-${TAG}-linux-x86_64.tar.gz
```

2. Extract binaries:

```bash
tar -xzf pd-controller-${TAG}-linux-x86_64.tar.gz
tar -xzf pd-edge-${TAG}-linux-x86_64.tar.gz
tar -xzf pd-vm-run-${TAG}-linux-x86_64.tar.gz
```

3. Run controller:

```bash
CONTROLLER_ADDR=0.0.0.0:9100 \
CONTROLLER_DEFAULT_POLL_MS=1000 \
CONTROLLER_STATE_PATH=.pd-controller/state.json \
./pd-controller
```

4. Run edge:

```bash
./pd-edge \
  --control-plane-url http://127.0.0.1:9100 \
  --edge-name edge-local-1 \
  --edge-id-path .pd-edge/edge-id \
  --data-addr 0.0.0.0:8080 \
  --admin-addr 127.0.0.1:8081 \
  --max-program-bytes 1048576 \
  --control-plane-poll-interval-ms 1000 \
  --control-plane-rpc-timeout-ms 5000
```

5. Open WebUI:

```text
http://127.0.0.1:9100/ui
```

6. `pd-vm-run` CLI examples:

```bash
./pd-vm-run --version
./pd-vm-run --emit-vmbc out/program.vmbc ./program.rss
./pd-vm-run --jit-hot-loop 2 --jit-dump ./program.rss
```

## Key runtime behavior

- Edge identity:
  - UUID is generated/persisted at `.pd-edge/edge-id` by default, or can be set with `--edge-id`.
  - Friendly name defaults to hostname, or can be set with `--edge-name`.
- Edge local listeners:
  - Data plane: `--data-addr` (default `0.0.0.0:8080`)
  - Admin endpoint: `--admin-addr` (default `127.0.0.1:8081`)
  - Program size limit: `--max-program-bytes` (default `1048576`)
- Controller persistence (default base path `.pd-controller/state.json`) is split as:
  - core: `state.json`
  - programs: `state.programs.json`
  - timeseries: `state.timeseries.bin`

## Useful docs

- [pd-controller README](pd-controller/README.md)
- [pd-edge README](pd-edge/README.md)
- [pd-vm README](pd-vm/README.md)
- ABI manifest: `pd-edge-abi/abi.json`
