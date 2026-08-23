# Scoped host resources and the host extension SDK

RustScript's `pd-vm` core is **host-agnostic**: `src/vm`, the generic resource/operation cores and
`ExecutionScope` never import or dispatch a concrete host library (`rusqlite`, `hyper`, `tokio::net`,
`tokio::process`, platform process/thread implementations). Concrete capabilities — SQLite, file/socket/
process I/O, HTTP/SSE — are supplied by *same-crate standard builtins* (and by external host crates)
that consume the generic scoped host SDK documented here.

The architecture is a Deno/Wasmtime-style hybrid:

- the core owns an object-safe [`HostResource`] interface and a typed, generational
  [`ResourceTable`] of erased resources;
- host extensions register arbitrary concrete resources and dynamic [`HostOperation`] drivers;
- one [`ExecutionScope`] binds the resource table and operation registry to a single VM invocation;
- `Vm` reset closes the old scope and builds a fresh guest execution state; it never queries host
  module history, resource classes or host-function history.

## `HostResource` implementation guide

A concrete resource implements the object-safe trait:

```rust
use vm::{CloseProgress, HostResource, ResourceCloseReason, ResourceResult};

struct MyResource { /* owned native state */ }

impl HostResource for MyResource {
    fn begin_close(&mut self, reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        // 1. Synchronously issue any cancel/close request to the underlying
        //    work (interrupt a query, signal a thread, close a socket).
        // 2. Return CloseProgress::Ready if nothing remains, or
        //    CloseProgress::Pending to drive poll_close afterwards.
        Ok(CloseProgress::Ready)
    }

    fn poll_close(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<ResourceResult<()>> {
        // Only called after begin_close returned Pending. Drive the close to
        // completion; return Poll::Pending and register cx.waker() if the
        // underlying work is still running.
        std::task::Poll::Ready(Ok(()))
    }
}
```

Contract rules:

- `begin_close` **must be idempotent** and must synchronously issue the cancel/close request. A
  resource that needs asynchronous teardown returns `Pending` and completes it in `poll_close`.
- `poll_close` is called only after `begin_close` returned `Pending`.
- A concrete `Drop` remains the **last-resort guard** for memory/OS-handle safety, but the VM may
  only reuse a resource (and its slot) once `poll_close` completes.
- Resources should override `resource_type_key()` with their stable catalog key (for example
  `"sqlite.connection"`) so exact host-import schemas can validate them; the default returns `None`
  (legacy typed APIs only).
- The core records only *generic* close errors; concrete host crates own error mapping and
  diagnostics.

The `HostResource` bound is `Any + Send + 'static`. A resource must be `Send` because the table
(and thus the `Vm`) is `Send`; it is deliberately **`!Sync`** — the table is owned and mutated by a
single thread, and Rust references are only borrowed for the duration of one host call.

## Typed vs raw handles

- A [`Resource<T>`] is the **typed host-side token**: a `Copy` capability keyed by a
  [`ResourceHandle`]. Duplicating the token does **not** duplicate ownership of the underlying
  resource.
- A [`ResourceHandle`] is the **raw guest-facing token**: an opaque integer that carries only
  `arena/scope identity | slot index | generation`. It can cross the host boundary as a script
  `Value::Int`, but it encodes **no** domain resource type.
- The table validates a typed access in order: handle encoding → arena/scope identity → slot index
  and generation → slot state `Open` → slot `TypeId` equals `TypeId::of::<T>()` → ownership
  transition. Passing a `Resource<File>` where a `Resource<SqliteConnection>` is expected returns a
  typed `ResourceTypeMismatch` and leaves the original resource untouched (no ownership consumed,
  no borrow changed, no cleanup run, no generation advanced).
- Raw handles are valid only within the current VM execution scope. They must never be persisted or
  shared across VMs; an old-scope handle is rejected with `ResourceHandleWrongTable` and a reused
  slot with a stale generation is rejected with `ResourceStale`.

Host functions borrow a resource for the duration of one call through `ResourceTable::get` /
`get_mut`, returning `ResourceRef<'a, T>` / `ResourceMut<'a, T>`. **Rust borrows never outlive a
yield or a pending operation**; asynchronous work must hold its own state or a `Resource<T>` handle.

## Parent/child rules

Resources can be registered as children of a parent:

- `push_child_resource::<T, P>(value, &parent)` links `T` under an open `P`; the parent cannot be
  closed while the child is live.
- Explicit single-resource close of a parent with live children returns
  `ResourceHasChildren`.
- **Scope shutdown uses a deterministic post-order (child-first) order**: every leaf is begun, then
  its parent once the child has completed. This guarantees a Pending child can never prevent its
  parent's `begin_close` from running before the owning tables fall through to their `Drop` guards.
- Closing a resource cancels operations associated with that exact handle (generic association; the
  core never dispatches on a resource class).

## Scope ownership, reset and the Vm Drop contract

Every `Vm` owns exactly one `ExecutionScope` (resource table + operation registry + close state).
`HostContext` (obtained via `vm.host_context()`) is the guarded mutation surface: it pushes
resources, starts operations, borrows/validates typed resources, installs module state and begins
single-resource closes — without ever exposing `HostRuntime` private fields.

Reuse is an explicit two-phase contract:

- `Vm::begin_reset_for_reuse(reason, deadline)` begins scope shutdown (Active → Closing, sealing new
  inserts). First reason/deadline wins; repeated begins are idempotent.
- `Vm::poll_reset_for_reuse(cx, now)` drives the close to quiescence. Only when the scope is
  `Quiescent` (operations drained, resources closed) is a fresh `Active` scope installed and the
  guest execution state rewound. While pending, the VM is `Resetting` and never lent out of a pool.
- `Vm::reset_for_reuse()` is the synchronous compat entry; with genuinely pending resources it
  returns a structured `ResetPending` and the VM stays `Resetting` until driven through the poll API.
  It never busy-loops.

**`Vm` Drop** (plan section 5.3): dropping a `Vm` synchronously begins the execution-scope close
with `ResourceCloseReason::VmDrop` and drives one round of the close pipeline with a no-op waker —
cancelling every pending operation with `OperationCancelReason::VmDrop` and issuing child-first
`begin_close` to every live resource with `ResourceCloseReason::VmDrop`. Drop never blocks, never
claims quiescence and never recycles; genuinely event-driven `Pending` resources stay `Closing` and
are released by their own `Drop` guards. Guest-owned local handles are released (exactly-once) with
the ownership-release reason before the scope shutdown, and scope shutdown closes anything that
survived.

Module policy (e.g. `SqlitePolicy`, `IoPolicy`, `HttpConfig`) lives in **persistent per-VM module
state** ([`HostModuleState`]): it survives scope close and reset, never participates in resource
close, and is keyed by `TypeId`.

## Synchronous and asynchronous close examples

Synchronous close (a resource that tears down inline):

```rust
// Scope shutdown calls begin_close on every live resource (child first) with
// the shutdown reason; a Ready resource is reclaimed immediately.
```

Asynchronous close (a cooperative worker thread):

```rust
impl HostResource for WorkerResource {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.cancelled.store(true, Ordering::SeqCst); // cooperative cancel
        Ok(CloseProgress::Pending)
    }
    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        if self.join_done() {
            Poll::Ready(Ok(()))
        } else {
            cx.waker().wake_by_ref(); // re-poll when progress is possible
            Poll::Pending
        }
    }
}
```

During a reset, `poll_reset_for_reuse` re-polls pending resources with the caller's waker until
quiescence (or the recycle deadline). The core never force-kills a thread; a worker that does not
join before the host recycle deadline causes the VM to be discarded (poisoned).

## Cleanup failure and the poisoned VM

Shutdown is best-effort: an error on one resource never skips the remaining resources. The terminal
scope outcome carries the first typed error plus the failure count.

- A **cleanup error** or **recycle deadline** during reset moves the VM to `VmResetState::Poisoned`.
  The old scope (with its recorded error) is preserved for diagnostics; the VM can be dropped but
  never runs again and never returns to a pool.
- An **explicit single-resource close failure** stays local to that resource: the error is returned
  to the caller, the resource stays open, and scope shutdown retries the idempotent close request.
- A poisoned VM reports the failure through `Vm::reset_error()` / `Vm::reset_state()` and rejects
  `run`/`resume`/reuse with a structured `NotReusable` error.

## `HostOperation` and `HostContext` usage

A pending host operation is an object-safe driver:

```rust
use vm::operation::{HostOperation, OperationCancelReason, OperationResult};
use std::task::{Context, Poll};

struct MyOp;
impl HostOperation for MyOp {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<OperationResult<()>> { Poll::Pending }
    fn cancel(&mut self, reason: OperationCancelReason) -> OperationResult<()> { Ok(()) }
}
```

Operations are registered through `HostContext::start_operation(OperationSpec::new(driver))`; a spec
may carry a deadline, an associated `ResourceHandle` (closing that resource cancels the operation)
and a one-shot cleanup. Scope shutdown cancels every pending operation with a single typed reason and
drains the registry. There is no static owner→poller table and no global token tree.

`HostContext` also provides:

- `push_resource` / `push_resource_with_key` / `push_child_resource` — insert resources;
- `get` / `get_mut` — call-scoped typed borrows;
- `close_resource::<T>(handle, reason)` — explicit single-resource close;
- `mark_resource_guest_owned(handle)` — exact host-return ownership transfer;
- `set_module_state` / `module_state` / `module_state_mut` — persistent typed module state;
- `execution_scope()` — read-only scope observations (counts, state, terminal outcome).

External host crates compose through the `HostExtension` trait: `register(registry)` registers exact
host functions from a `HostApiCatalog` (via `catalog_import_schemas`), and `install(vm)` installs
persistent module state. `Vm::install_extension(&extension)` runs both steps transactionally. The
exact schemas — parameter labels, type schemas, passing modes and the catalog fingerprint — must
match byte-for-byte what the compiler embeds in the program's `HostImport`, so a registry compiled
against a different catalog is rejected at bind time.

## Feature matrix

| Feature set | `src/vm` / resource / operation / scope | Standard builtins | Compiler / catalog |
|---|---|---|---|
| `pd-vm --no-default-features` | generic core only; no OS hosts | none | catalog wire types available |
| `pd-vm --no-default-features --features runtime` | generic core only | `io::*` surface | `HostApiCatalog` snapshot |
| `+ sqlite` | generic core only (no `cfg(feature = "sqlite")` in `src/vm`) | SQLite builtin (rusqlite, optional dep) | sqlite surface in the catalog |
| `+ http-client` | generic core only (no `cfg(feature = "http-client")` in `src/vm`) | HTTP/SSE builtin (hyper/rustls) | http surface in the catalog |
| `pd-vm-nostd` | n/a (no compiler/VM; VMBC v13 decoder only) | none | decodes exact `HostImport` schemas |
| `pd-vm-wasm` (`runtime` feature) | generic core compiled to wasm32 | io surface when enabled | — |

The `sqlite` / `http-client` features only decide whether the same-crate standard builtin is
compiled and registered by default; they never enter the resource/reset architecture. `src/vm`,
`src/vm/resource`, `src/vm/operation` and `ExecutionScope` carry no `cfg(feature = "sqlite")` or
`cfg(feature = "http-client")` and never import the concrete host libraries. `tests/
core_host_boundary_tests.rs` enforces this at the source level.

Enabling a standard extension in an embedding:

```toml
pd-vm = { git = "https://github.com/rustscript-lang/rustscript", package = "pd-vm",
          features = ["sqlite", "http-client"] }
```

then the standard catalog is available through `builtins::runtime::standard_host_catalog()` and the
standard compile entry installs the same snapshot, so compiled `HostImport` fingerprints match the
registered exact schemas for any combination of enabled features.
