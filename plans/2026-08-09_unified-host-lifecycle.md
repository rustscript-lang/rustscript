# Unified Host Resource, Operation, and Cancellation Plan

**Goal:** Make every privileged runtime subsystem use one resource arena, operation registry, cancellation model, and cleanup lifecycle.

**Architecture:** `HostRuntime` owns typed opaque resources and pending operations. IO, HTTP, SQLite, streams, processes, and future task operations register through the same interfaces. Cancellation carries a structured reason and propagates from run to operation to resource cleanup.

**Tech Stack:** Rust 2024, existing runtime hosts, `HostOpId`, HTTP client, SQLite, VM reset/drop tests.

**Status:** Completed

---

## Independence and dependency

- Depends on the HostRuntime/RunContext ownership contract from `2026-08-09_vm-runtime-decomposition.md`.
- Capability authorization can be implemented in parallel if it targets the same HostRuntime boundary.
- Agent lifecycle consumes this contract but is not implemented here.

## Scope boundary

### In scope

- One opaque resource handle format and typed resource validation.
- One pending-operation registry and owner dispatch.
- Cancellation tree/reasons, deadlines, cleanup, and terminal state.
- Migration of existing IO, HTTP, and SQLite state.
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
  poll(id)
  complete(id, result)
  cancel(id, reason)
  cancel_all(reason)

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

### Milestone 4: Migrate HTTP

**Files:**
- Modify: `src/builtins/runtime/http.rs`
- Modify: HTTP tests

1. Register requests/streams as operations/resources.
2. Connect abort handles and resolver work to the run token.
3. Enforce in-flight permits in HostRuntime.
4. Remove HTTP-specific pending dispatch from `runtime/mod.rs`.

### Milestone 5: Migrate IO and other existing resources

**Files:**
- Modify IO runtime modules and VM host polling

Move file/iterator/callback resources and pending operations to the same arena/registry where applicable. Delete subsystem counters/maps after migration.

### Milestone 6: Centralize wait/poll/cancel

**Files:**
- Modify: `src/builtins/runtime/mod.rs`
- Modify: `src/vm/host.rs`
- Modify: `src/vm/mod.rs` or new component files

1. Replace subsystem `if` chains with registry owner dispatch.
2. Let Instance wait on an operation ID while HostRuntime owns operation state.
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

- Production IO/HTTP/SQLite paths all use the shared arena and registry.
- No subsystem owns an independent public operation-ID namespace.
- Cancellation reasons remain structured from run through cleanup.
- Close/reset/drop invoke cleanup once and reject stale handles afterward.
- A terminal run cannot receive a late operation result.
- Generic resource/operation code has production callers and no broad dead-code warnings.
- Per-subsystem polling/cancellation chains are removed.
