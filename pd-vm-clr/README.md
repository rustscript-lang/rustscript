# pd-vm-clr

CLR runtime and compiler support for PD VM bytecode.

## Projects

- `PdVm.Runtime`
  - CLR-side VM value model, builtin implementations, host dispatch, and program execution helpers.
- `PdVm.Compiler`
  - Reads `VMBC` bytecode and emits CLR assemblies (`.dll`) that implement `IPdVmProgram`.
- `PdVm.Runner`
  - Small CLI for compile/run/compile-run flows.
- `PdEdge.Http`
  - Minimal HTTP/1 proxy runtime for PD Edge scripts on CLR.
  - Project name is `PdEdge.Http`, but the built executable name is `pd-edge-http-minimal-clr` so the existing Rust perf harness can target it directly.
- `PdVm.Tests`
  - Compiler/runtime tests.
- `PdEdge.Http.Tests`
  - HTTP proxy parity tests.

## Requirements

- .NET 9 SDK
- Rust toolchain if you want:
  - `PdEdge.Http --program-source ...`
  - the Rust HTTP perf harness

`PdEdge.Http` prefers the prebuilt Rust helper at `target/debug/examples/compile_to_file(.exe)` for source compilation and falls back to `cargo run -p pd-edge --example compile_to_file -- ...` when that binary is not present.

## Build

From the repo root:

```powershell
dotnet build pd-vm-clr\pd-vm-clr.sln
```

Release build for the proxy runtime:

```powershell
dotnet build pd-vm-clr\PdEdge.Http\PdEdge.Http.csproj -c Release
```

## Test

Build first, then run tests without rebuilding:

```powershell
dotnet test pd-vm-clr\pd-vm-clr.sln --no-build
```

Or run just the HTTP runtime tests:

```powershell
dotnet test pd-vm-clr\PdEdge.Http.Tests\PdEdge.Http.Tests.csproj --no-build
```

## Compile Source To VMBC

For a plain PD VM source file, emit `VMBC` with `pd-vm-run`:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- `
  --emit-vmbc path\to\program.vmbc `
  path\to\program.rss
```

For a PD Edge HTTP proxy script, use the Edge helper so imports are checked against the Edge ABI:

```powershell
cargo run -p pd-edge --example compile_to_file -- `
  path\to\program.rss `
  path\to\program.vmbc
```

If `target\debug\examples\compile_to_file.exe` already exists, you can run it directly instead of `cargo run`:

```powershell
.\target\debug\examples\compile_to_file.exe path\to\program.rss path\to\program.vmbc
```

Use the Edge path for scripts that will run inside `PdEdge.Http`.

Example source files are in `pd-vm-clr/examples/`:

- `pdedge-http-local-ok.rss`
- `pdedge-http-proxy-http1.rss`
- `pdedge-http-proxy-http1-body-read.rss`

## Compile VMBC To CLR

Compile a `VMBC` file to a CLR assembly:

```powershell
dotnet run --project pd-vm-clr\PdVm.Runner -- compile input.vmbc output.dll
```

Run a compiled CLR assembly:

```powershell
dotnet run --project pd-vm-clr\PdVm.Runner -- run output.dll
```

Compile and run in one step:

```powershell
dotnet run --project pd-vm-clr\PdVm.Runner -- compile-run input.vmbc output.dll
```

Optional execution cap:

```powershell
dotnet run --project pd-vm-clr\PdVm.Runner -- run output.dll --max-steps 1000000
```

## Run The Minimal HTTP Proxy

Run with a precompiled `VMBC` program:

```powershell
dotnet run --project pd-vm-clr\PdEdge.Http -- `
  --program-vmbc path\to\program.vmbc `
  --data-addr 127.0.0.1:8080
```

Run with a source program:

```powershell
dotnet run --project pd-vm-clr\PdEdge.Http -- `
  --program-source path\to\program.rss `
  --data-addr 127.0.0.1:8080
```

Useful flags:

- `--proxy-addr <ADDR>`
  - Alias for `--data-addr`.
- `--vm-execution-mode async|threading`
  - Request-time VM execution strategy.
- `--max-steps <N>`
  - Per-request VM instruction cap. Default is `10000000`.
- `--disable-logging`
  - Suppresses console log output.

Compatibility flags accepted for parity with `pd-edge-http-minimal`:

- `--vm-fuel`
- `--vm-fuel-check-interval`
- `--vm-jit`

These are currently parsed but treated as no-ops in the CLR runtime.

## PdEdge.Http Scope

`PdEdge.Http` is intentionally minimal:

- HTTP/1.1 only
- No admin API
- No metrics
- No debugger
- No control plane
- No TLS
- No HTTP/2
- No HTTP/3

It supports the minimal host surface needed for the standalone proxy flow:

- request getters such as `http::request::get_method`, `get_path`, `get_header`, `get_body`
- response setters such as `http::response::set_status`, `set_header`, `set_headers`, `set_body`
- default upstream preparation via `http::exchange::prepare_default_upstream`
- native forwarding via `proxy::stream::*` and `proxy::forward_native`

## Benchmark With The Existing Rust Harness

Build the proxy in Release first:

```powershell
dotnet build pd-vm-clr\PdEdge.Http\PdEdge.Http.csproj -c Release
```

Then run the Rust benchmark harness against the built executable:

```powershell
cargo run -p pd-edge --example http_proxy_perf_framework -- `
  --binary d:\Workspace\project-d\pd-vm-clr\PdEdge.Http\bin\Release\net9.0\pd-edge-http-minimal-clr.exe `
  --skip-build `
  --scenario http_proxy `
  --requests 2000 `
  --warmup-requests 200 `
  --concurrency 32
```

Body-read scenario:

```powershell
cargo run -p pd-edge --example http_proxy_perf_framework -- `
  --binary d:\Workspace\project-d\pd-vm-clr\PdEdge.Http\bin\Release\net9.0\pd-edge-http-minimal-clr.exe `
  --skip-build `
  --scenario http_proxy_body_read `
  --requests 2000 `
  --warmup-requests 200 `
  --concurrency 32
```

## Current Status

- `PdVm` CLR compile/run flow is working.
- `PdEdge.Http` local-response and native-forward proxy paths are covered by tests.
- The Rust HTTP perf harness can drive the CLR proxy binary directly.
