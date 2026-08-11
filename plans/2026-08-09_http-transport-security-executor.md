# Async Host Transport Security Plan

**Goal:** Make HTTP a host-driven async host function, provide feature-selected blocking/async IO implementations, and preserve transport security without any HTTP-owned scheduler.

**Architecture:** `#[pd_host_function] async fn` produces a generic async host factory. The VM allocates an operation ID and hands the resulting `'static` future to the embedding's `HostAsyncBridge`; the host owns submission, waking, polling, cancellation, and reactor/executor integration. HTTP contains only policy, request construction, transport, redirect, deadline, and response decoding logic. Edge scopes and `SharedProxyVmContext` expansion remain owned by `pd-edge`.

**Tech Stack:** Rust 2024, proc macros, `Future`, `HostAsyncBridge`, Reqwest/Tokio under the `async` feature, local HTTP fixtures.

---

## Independence and dependency

- Depends on the capability profile and unified host lifecycle.
- Adds a generic async host ABI before migrating IO or HTTP.
- Requires a coordinated `pd-edge` adapter migration because the current core proc-macro contains Edge-specific expansion.
- Independent of agent provider protocols and source-language `async`/`await` syntax.

## Hard ownership rules

1. The VM owns script suspension state and operation-ID allocation.
2. The embedding host owns future storage, waking, polling, cancellation, and executor/reactor integration.
3. A subsystem async host function owns only its future body and typed policy/context snapshot.
4. HTTP must not create a thread, Tokio runtime, oneshot completion scheduler, private pending map, private poller, or independent operation-ID namespace.
5. Core `pd-host-function` must not contain Edge scope enums, `SharedProxyVmContext`, `crate::abi_impl` paths, or pd-edge registry generation.
6. With the `async` feature disabled, IO keeps its blocking implementation and HTTP is absent from callable metadata and runtime registration.
7. With the `async` feature enabled, IO binds its async implementation and HTTP is available only through the async host ABI.
8. Async operation driving code lives in dedicated folders: core contracts/lifecycle under `src/vm/async_host/`, and the pd-edge driver under `pd-edge/src/async_host/`. It must not accumulate in `host.rs`, `abi_impl/mod.rs`, HTTP, or IO modules.

## Scope boundary

### In scope

- Generic async `#[pd_host_function(name = "...")]` expansion using owned arguments.
- Host-driven future submission/poll/cancel ABI.
- Migration of pd-edge scope expansion to a pd-edge-owned proc-macro adapter.
- Feature-selected blocking/async IO implementations.
- Async-only generic HTTP host.
- Complete IPv4/IPv6 special-use classification.
- Async DNS, total/connect/first-byte/idle deadlines, destination pinning, redirect revalidation, and body limits.
- Cancellation/reset/drop tests across host-driver and transport phases.

### Out of scope

- A VM-owned Tokio runtime or process executor.
- HTTP-specific scheduling infrastructure.
- Provider JSON, retries, model selection, SSE semantic parsing, or agent loops.
- Ambient proxy support by default.
- Script-controlled policy relaxation.
- Source-language futures or `await` syntax.

## Implementation route

### Milestone 1: Freeze generic async host semantics with RED tests

**Files:**
- Modify: `pd-host-function/src/lib.rs`
- Modify: `tests/host_binding_generation_tests.rs`
- Modify: VM host lifecycle tests

Cover:

- ordinary owned-argument async signatures are accepted;
- borrowed typed parameters and raw borrowed args are rejected for async hosts;
- generated wrappers submit exactly one `'static` future to the installed host driver;
- missing driver fails before a pending operation becomes visible;
- completion returns exactly one terminal result;
- cancellation/reset/drop cancel the driver operation exactly once;
- rejected submission retires the operation ID and leaves no waiting state;
- interpreter/JIT/AOT all suspend through the same `CallOutcome::Pending` boundary.

### Milestone 2: Add the host-driven async ABI

**Files:**
- Create: `src/vm/async_host/mod.rs`
- Create supporting files under `src/vm/async_host/` for lifecycle/bridge concerns when needed
- Modify: `src/vm/host.rs` only to remove superseded inline async lifecycle code
- Modify: `src/vm/host_runtime.rs`
- Modify: `src/vm/mod.rs`
- Modify: `build.rs`
- Modify: `pd-host-function/src/lib.rs`

1. Define the boxed `'static` host future output contract.
2. Extend or replace `HostAsyncBridge` with explicit submit, poll, and cancel operations.
3. Allocate IDs through the shared operation registry before submission and retire them on every fallible path.
4. Generate an async host adapter that takes owned script arguments, constructs a future, and submits it through the VM boundary.
5. Classify async generated hosts as suspension-capable and exclude them from non-yielding native fast paths.
6. Keep cached registry binding and direct VM binding equivalent.

### Milestone 3: Return Edge scope ownership to pd-edge

**Core files:**
- Remove: `pd-host-function/src/edge.rs`
- Modify: `pd-host-function/src/lib.rs`

**pd-edge files:**
- Create a pd-edge-owned proc-macro adapter crate or equivalent owned macro module.
- Create: `src/async_host/mod.rs` and focused driver/operation files beneath that folder.
- Migrate `scope = runtime/http/http_extension/transport`, bind parameters, registry generation, and `SharedProxyVmContext` preparation.
- Adapt `VmAsyncOpBridge` to the generic host submit/poll/cancel contract.
- Remove future storage, operation allocation, reactor entry, and bridge polling logic from `src/abi_impl/mod.rs`.

The core proc-macro must retain only name-based generic sync/async host expansion.

### Milestone 4: Provide blocking and async IO implementations

**Files:**
- Modify: IO runtime modules, build generation, and IO tests

1. Keep blocking IO available without the `async` feature.
2. Under `async`, bind the same script-facing IO API to async host functions with owned parameters.
3. Make build-time callable discovery honor the active feature so only one implementation enters metadata/registration.
4. Route async IO futures through the embedding host driver.
5. Preserve canonical path, process permission, byte, line, and handle policies in both variants.

### Milestone 5: Migrate HTTP to an async host function

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/builtins/runtime/http.rs`
- Modify: `src/builtins/runtime/mod.rs`
- Modify: `tests/vm/http_host_tests.rs`

1. Gate HTTP dependencies, callable metadata, configuration, and tests on `async`.
2. Convert `http::client::request` to a true async host function with an owned request and immutable policy/context snapshot.
3. Delete `schedule_request`, `HttpCompletion`, `HttpRequestResource`, oneshot completion, per-request threads/runtimes, and HTTP-specific pending polling.
4. Resolve DNS inside the submitted future and count it against the total deadline.
5. Validate and pin every resolved address before connection while preserving TLS hostname/SNI verification.
6. Repeat policy, credential stripping, pinning, and deadline checks for every redirect.
7. Enforce request/header/body, connect, first-byte, idle, total, and response-byte limits.
8. Disable ambient environment proxies unless the embedding supplies explicit policy.

### Milestone 6: Cancellation and lifecycle convergence

1. Propagate run cancellation/deadline/reset/drop to `HostAsyncBridge::cancel_op_with_reason`.
2. Ensure driver completion after a terminal run cannot re-enter the VM.
3. Verify cancellation during DNS, connect, response headers, and body streaming.
4. Confirm no HTTP or IO async subsystem resource remains after completion/cancellation.
5. Remove runtime owner-poller routing that became obsolete after host-driver migration.

### Milestone 7: Verification

Core:

```bash
cargo fmt --all -- --check
cargo test --locked -p pd-host-function
cargo test --locked --test host_binding_generation_tests --all-features
cargo test --locked --test io_builtin_edge_tests --all-features
cargo test --locked --test http_host_tests --features async
cargo test --locked --workspace --all-features
cargo test --locked --workspace --no-default-features --tests --no-run
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --locked --workspace --all-features --no-deps
git diff --check
```

pd-edge:

```bash
cargo fmt --all -- --check
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
git diff --check
```

All temporary and target directories must be under `/mnt/TEMP/rustscript/` and removed after final review.

## Target criteria

- A generic ordinary async host function compiles and suspends through the host driver.
- The host driver, not VM/HTTP/IO, stores and drives submitted futures.
- Async driving code is isolated in the dedicated core and pd-edge async-host folders.
- Core proc-macro code has no pd-edge scope or context knowledge.
- IO has blocking and async implementations selected by `async`.
- HTTP is unavailable without `async` and uses no private scheduler.
- DNS time counts against the total deadline.
- Every connected address is validated and pinned before connection.
- Special-use IPv4/IPv6 ranges are denied by default.
- Redirects repeat destination and credential validation.
- Cancellation during every transport phase reaches the driver and finishes within the documented bound.
- Reset/drop leave no future, operation, permit, stream, or response body live.
- HTTP host code contains no provider or agent policy.
