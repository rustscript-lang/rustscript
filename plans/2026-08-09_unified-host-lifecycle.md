# Unified Host Resource, Operation, and Cancellation Plan

**Goal:** Make every privileged runtime subsystem use one resource arena, operation registry, cancellation model, and cleanup lifecycle.

**Architecture:** `HostRuntime` owns typed opaque resources, operation identity, and cancellation state. Blocking IO and SQLite resources use the shared arena/registry. Async host futures are submitted to and driven by the embedding `HostAsyncBridge`; the VM retains only script suspension and lifecycle identity. Cancellation carries a structured reason and propagates from run to bridge operation and resource cleanup.

**Tech Stack:** Rust 2024, existing runtime hosts, `HostOpId`, HTTP client, SQLite, VM reset/drop tests.

**Status:** Completed

**Async correction:** The original HTTP/IO migration used subsystem pollers and, for HTTP, a per-request thread/runtime. Those transitional paths are superseded by `2026-08-09_http-transport-security-executor.md`. Completion of this plan does not authorize VM-, HTTP-, or IO-owned async executors.

**Directory correction:** Generic async host contracts and VM lifecycle glue belong under `src/vm/async_host/`; each embedding's concrete driver belongs in its own dedicated async-host folder. `host.rs` and subsystem modules must remain binding/business-logic surfaces rather than async driver containers.

---

## Independence and dependency

- Depends on the HostRuntime/RunContext ownership contract from `2026-08-09_vm-runtime-decomposition.md`.
- Capability authorization can be implemented in parallel if it targets the same HostRuntime boundary.
- Agent lifecycle consumes this contract but is not implemented here.

## Scope boundary

### In scope

- One opaque resource handle format and typed resource validation.
- One operation-ID/cancellation registry, with async future dispatch delegated to the embedding host bridge.
- Cancellation tree/reasons, deadlines, cleanup, and terminal state.
- Migration of blocking IO and SQLite state plus lifecycle identity for host-driven async operations.
- Removal of unused generic substrate and subsystem-specific duplicate registries.

### Out of scope

- Agent subagent semantics or provider retries.
- New filesystem/process/task host APIs.
- Source-language futures, `async`, or `await` syntax.
- Sharing mutable resources across VMs.

## Target contracts

```text
ResourceArena
  insert(type, value, cleanup)
  get(handle, expected_type)
  close(handle, reason)
  close_all(reason)

OperationRegistry
  start(owner, cancellation, cleanup)
  complete(id, result)
  cancel(id, reason)
  cancel_all(reason)

HostAsyncBridge
  submit(id, future)
  poll(id, waker)
  cancel(id, reason)

CancellationToken
  parent
  reason
  deadline
  child tokens
```

Handles must encode enough table/generation/type identity to reject stale, forged, cross-type, and cross-VM use.

## Implementation route

### Milestone 1: Freeze lifecycle semantics with tests

Add tests for:

- stale handle after close;
- handle reuse with generation change;
- wrong resource type;
- cross-VM handle rejection;
- operation completion/cancel race;
- reset/drop cleanup exactly once;
- parent cancellation propagation;
- timeout, user stop, resource close, and VM reset reasons.

### Milestone 2: Replace the unused generic substrate

**Files:**
- Modify: `src/builtins/runtime/resource.rs`
- Modify: `src/builtins/runtime/cancellation.rs`
- Modify: `src/vm/host_runtime.rs`

1. Store opaque host resources, not only language `Value` objects.
2. Define a resource type identifier and cleanup contract.
3. Make operation owner/poll/cancel routing data-driven.
4. Remove APIs that remain unused after the contract is fixed.

### Milestone 3: Migrate SQLite

**Files:**
- Modify: `src/builtins/runtime/sqlite.rs`
- Modify: SQLite tests

1. Replace SQLite-local handle counters and connection maps with ResourceArena handles.
2. Replace SQLite-local pending-op maps/signals with OperationRegistry.
3. Register `InterruptHandle` cancellation cleanup.
4. Ensure reset/drop waits only for the bounded documented grace period and no operation can re-enter a completed run.
5. Preserve path, SQL, row, byte, transaction, and authorizer limits.

### Milestone 4: Establish HTTP lifecycle identity

**Files:**
- Modify: `src/builtins/runtime/http.rs`
- Modify: HTTP tests

1. Allocate HTTP-visible wait identities through the shared operation registry.
2. Connect run cancellation/reset/drop to the host async bridge.
3. Do not store or poll HTTP futures through HTTP-specific resources.
4. Remove HTTP-specific pending dispatch from `runtime/mod.rs` during the async host migration.

### Milestone 5: Migrate IO and other existing resources

**Files:**
- Modify IO runtime modules and VM host polling

Keep blocking file/iterator/callback resources in the shared arena/registry. Async IO futures use the embedding host bridge under the `async` feature. Delete subsystem counters/maps and pollers after migration.

### Milestone 6: Centralize wait/poll/cancel

**Files:**
- Modify: `src/builtins/runtime/mod.rs`
- Modify: `src/vm/host.rs`
- Modify: `src/vm/mod.rs` or new component files

1. Replace subsystem `if` chains with one bridge dispatch for async host operations.
2. Let Instance wait on an operation ID while HostRuntime owns lifecycle state and the embedding owns the future.
3. Route run cancellation, deadline, resource close, reset, and drop through one cancellation API.
4. Guarantee one terminal transition and one cleanup execution.

### Milestone 7: Verification

```bash
cargo fmt --all -- --check
cargo test --locked --test runtime_context_tests
cargo test --locked --test runtime_host_tests
cargo test --locked --test http_host_tests --features http-client
cargo test --locked --test sqlite_host_tests --features sqlite
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
git diff --check
```

## Target criteria

- Production blocking IO/SQLite paths use the shared arena and registry; async IO/HTTP use shared lifecycle IDs plus the embedding host bridge.
- No subsystem owns an independent public operation-ID namespace.
- Cancellation reasons remain structured from run through cleanup.
- Close/reset/drop invoke cleanup once and reject stale handles afterward.
- A terminal run cannot receive a late operation result.
- Generic resource/operation code has production callers and no broad dead-code warnings.
- Per-subsystem async polling chains are removed.
- No HTTP/IO path creates a private thread, runtime, oneshot completion scheduler, or executor.
