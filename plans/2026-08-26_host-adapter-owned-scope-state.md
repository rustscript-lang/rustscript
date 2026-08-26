# Host-Adapter-Owned Scope State Implementation Plan

> **For Hermes:** Use subagent-driven development to implement this plan task by task, with strict RED/GREEN verification and two-stage review before publishing the rewritten stack.

**Goal:** Remove all concrete host-adapter state and feature knowledge from the generic VM runtime, allowing each host function or adapter to declare typed scope-local state backed by the generic resource arena while keeping persistent policy in one generic module-state store.

**Architecture:** `ExecutionScope` remains the VM's generic resource/operation lifecycle owner and gains typed scope-state access backed by its `ResourceTable`. A host adapter obtains its state lazily through generic APIs at the point of use; scope close/reset destroys that state through the ordinary resource close path. Persistent policy/configuration uses one typed `ModuleStateStore`. `src/vm/**` never imports or pattern-matches IO, SQLite, HTTP, SSE, or other concrete adapter types and never carries adapter feature guards.

**Tech Stack:** Rust 2024, RustScript VM, `ExecutionScope`, `ResourceTable`, `HostResource`, `HostContext`, Cargo feature composition, stacked GitHub PRs #16/#18/#23/#24/#26.

---

## 1. Architectural invariants

### 1.1 Dependency direction

The permitted dependency direction is:

```text
host adapter / builtin
    -> generic HostContext / ExecutionScope / ResourceTable APIs
    -> generic VM lifecycle primitives
```

The reverse dependency is forbidden. In particular, files under `src/vm/**` must not:

- import `crate::builtins::*` or a concrete host library;
- mention `IoState`, `IoPolicy`, `SqliteState`, `SqlitePolicy`, `HttpHostState`, `HttpConfig`, SSE state, or future adapter state/policy types;
- inspect adapter `TypeId`s to preserve or reset selected state;
- contain `cfg(feature = "sqlite")`, `cfg(feature = "http-client")`, or equivalent adapter feature guards;
- add concrete adapter fields to `HostRuntime`, `Vm`, `ExecutionScope`, `HostContext`, or another generic VM structure.

`ExecutionScope` is allowed in generic VM code because it is the host-agnostic lifecycle primitive. Its implementation must remain independent of concrete host adapters and Cargo adapter features.

### 1.2 Feature-guard boundary

A host-adapter feature guard is allowed only where the build genuinely composes or exposes that adapter:

- Cargo dependency/feature declarations (`Cargo.toml`, `Cargo.lock`);
- build-time feature composition where required (`build.rs`);
- builtin module declarations and re-exports (`src/builtins/mod.rs`, `src/builtins/runtime/mod.rs`);
- standard host-function registration/composition (`src/builtins/runtime/standard_composition.rs`, `src/builtins/runtime/host.rs`, or the adapter-owned registration module);
- crate-level public re-export of an enabled adapter API, when such an export already belongs to the public surface;
- the concrete adapter implementation and its adapter-specific tests.

A guard is not allowed around a generic helper merely because its only current consumer is a guarded adapter. Generic helpers must compile and be tested independently of every concrete adapter feature.

These rules apply equally to SQLite, IO, HTTP/SSE, and future host adapters. IO may be unconditionally composed under the runtime feature today; that does not grant it fields or branches in generic VM structures.

### 1.3 State lifetimes

There are exactly two host-owned state lifetimes:

1. **Scope-local state**
   - pending-result maps;
   - connection/permit counters tied to live scope resources;
   - adapter operation bookkeeping;
   - ephemeral pools, workers, and per-invocation mutable state.

   Scope-local state is created lazily by its adapter and stored as a resource-arena-owned typed state entry. It is closed and destroyed by ordinary `ExecutionScope` shutdown. VM reset contains no adapter-specific reset branch.

2. **Persistent module state**
   - `IoPolicy`;
   - `SqlitePolicy`;
   - `HttpConfig`;
   - external extension configuration intended to survive `Vm::reset_for_reuse()`.

   Persistent state lives in the single generic `ModuleStateStore`. It never participates in resource close and survives execution-scope replacement until explicitly replaced/removed or until the VM is dropped.

No third `host_function_state` type map is permitted.

---

## 2. Target generic API

### 2.1 Arena-backed typed scope state

Add a private generic resource wrapper and a type-indexed singleton mapping inside the generic lifecycle layer. The exact internal representation may vary, but the public behavior must match:

```rust
impl ExecutionScope {
    pub fn scope_state_or_insert_with<T, F>(&mut self, init: F)
        -> ExecutionScopeResult<&mut T>
    where
        T: Send + 'static,
        F: FnOnce() -> T;

    pub fn scope_state<T>(&self) -> Option<&T>
    where
        T: Send + 'static;

    pub fn scope_state_mut<T>(&mut self) -> Option<&mut T>
    where
        T: Send + 'static;

    pub fn take_scope_state<T>(&mut self) -> Option<T>
    where
        T: Send + 'static;
}
```

The `scope_` prefix is required because `ExecutionScope::state()` already reports the generic Active/Closing/Quiescent lifecycle phase; Rust does not overload methods by generic arity.

Required semantics:

- one scope-state value per concrete `T` per `ExecutionScope`;
- lazy initialization executes at most once while the state is present;
- state is physically owned by the resource arena and participates in arena identity/generation validation;
- state insertion is rejected when the scope is Closing or Quiescent;
- scope close removes state through the same generic resource close sweep;
- a fresh scope cannot observe state or handles from the previous scope;
- no adapter name, feature, or type appears in the implementation;
- ordinary resources of the same payload type cannot collide with scope-state identity (use an internal wrapper or a dedicated generic state key);
- failure to insert leaves no stale type-index entry;
- `take_state` removes both arena entry and type index atomically.

Expose equivalent generic wrappers through `HostContext` once that public SDK exists:

```rust
context.scope_state_or_insert_with::<IoState, _>(IoState::default)?;
context.scope_state::<IoState>();
context.scope_state_mut::<IoState>();
context.take_scope_state::<IoState>();
```

Same-crate adapters that precede the public `HostContext` layer may call crate-private generic VM/host-context forwarding methods. Those forwarding methods must remain type-generic and feature-neutral.

### 2.2 Single persistent module-state store

Retain one generic `ModuleStateStore` keyed by `TypeId`, with typed set/get/get_mut/remove operations. Move its earliest required implementation into the owning lower stack layer if IO/SQLite persistent policy needs it before PR #18; PR #18 then exposes the same store through `HostContext` instead of introducing another map.

The store must not use `Arc::get_mut(...).expect(...)` as a uniqueness invariant. Prefer uniquely owned `Box<dyn Any + Send>` entries unless a demonstrated concurrent sharing requirement exists.

### 2.3 HostRuntime target shape

After the refactor, `HostRuntime` may own only generic runtime machinery:

```rust
pub(crate) struct HostRuntime {
    // host-function symbols/bindings and capability data
    // generic async bridge/stream drivers
    execution_scope: ExecutionScope,
    module_state_store: ModuleStateStore,
    // generic operation ids and print sink
}
```

It must not contain:

```rust
io_state: IoState,
sqlite_state: SqliteState,
host_function_state: HashMap<TypeId, ...>,
```

`reset_execution_scope()` must close/replace only generic scope structures. It must not preserve HTTP state by concrete `TypeId`, reconstruct IO/SQLite state, or branch on adapter features.

---

## 3. Stack ownership and commit boundaries

The final stack remains:

```text
master
  -> PR #16 (4 commits)
  -> PR #18 (1 commit)
  -> PR #23 (1 commit)
  -> PR #24 (1 commit)
  -> PR #26 (1 commit)
```

PR #28 remains absorbed/merged with zero delta.

### PR #16 commit 1 — VM decomposition

`refactor(vm): split runtime state from VM facade`

- Keep this commit mechanical where practical.
- If `HostRuntime` currently gains concrete adapter fields in this commit, remove those fields and initializers here.
- The new plan document may be introduced in this commit or commit 2; choose the first commit where the architectural boundary becomes meaningful.

### PR #16 commit 2 — generic lifecycle and state primitives

`feat(vm): add scoped resource and operation lifecycle`

- Add arena-backed typed scope state to `ResourceTable`/`ExecutionScope`.
- Add the single generic persistent state store at its earliest required layer.
- Make `HostRuntime::reset_execution_scope` wholly generic.
- Add feature-boundary architecture tests.
- Remove concrete HTTP-preservation logic and the duplicate `host_function_state` map if present at this layer.

### PR #16 commit 3 — IO migration

`refactor(io): migrate IO onto scoped lifecycle`

- Move `IoState` from `HostRuntime` into lazily declared arena-backed scope state.
- Move `IoPolicy` into the persistent module-state store.
- Remove `vm.host.io_state` access.
- Ensure pending IO workers and result maps close/quiesce through generic scope lifecycle.
- Keep IO registration/composition in the builtin layer.

### PR #16 commit 4 — SQLite migration

`feat(sqlite): add scoped SQLite host functions`

- Move `SqliteState` from `HostRuntime` into lazily declared arena-backed scope state.
- Move `SqlitePolicy` into persistent module state.
- Move `configure_sqlite`, `clear_sqlite`, and policy access implementations out of `src/vm/**` into the SQLite adapter module or an adapter-owned extension trait/`impl Vm` block.
- Keep `cfg(feature = "sqlite")` at concrete module registration/export/composition boundaries only.
- Preserve close behavior for queued and running operations.

### PR #18 — public host-extension SDK and HTTP generic state use

`feat(host): add capability profiles and async host execution`

- Expose the existing single module-state store through `HostContext`.
- Expose generic scope-state methods through `HostContext`.
- Remove any second module-state or host-function-state storage.
- Ensure external `HostExtension` implementations can declare both persistent module state and scope-local state without private `HostRuntime` access.
- Migrate HTTP runtime counters/permits/bookkeeping to scope state and `HttpConfig` to persistent module state.
- Keep HTTP feature guards in builtin registration/export/adapter files; generic host SDK files remain feature-neutral.

### PR #23/#24/#26 — cascade only where required

- Replay invocation streaming, compiler ownership, and HTTP/SSE changes on the rewritten lower layers.
- #26 contains HTTP/SSE adapter implementation that depends on the public generic API, but does not add feature-specific branches to generic VM code.
- Preserve one commit per PR and independent CI validity at every PR head.

---

## 4. TDD implementation tasks

### Task 1: Lock the feature boundary with failing architecture tests

**Files:**
- Modify: `tests/host_binding_generation_tests.rs`
- Modify or create: `tests/host_context_arch_tests.rs`
- Create if clearer: `tests/host_feature_boundary_arch_tests.rs`

**RED tests:**

1. Scan all Rust sources under `src/vm/**` and fail on:
   - `crate::builtins`;
   - adapter state/policy/config symbols;
   - `cfg(feature = "sqlite")`;
   - `cfg(feature = "http-client")`;
   - concrete adapter `TypeId` references.
2. Assert `HostRuntime` has no concrete adapter state fields and no duplicate type map.
3. Assert the allowed guard locations are adapter composition/registration/export files only.

Run the focused architecture tests and confirm they fail against the current stack for the expected `host_runtime.rs` and `vm/mod.rs` references.

### Task 2: Add arena-backed typed state to the generic lifecycle

**Files:**
- Modify: `src/vm/resource/table.rs`
- Modify: `src/vm/resource/mod.rs` and related private resource files as needed
- Modify: `src/vm/execution_scope.rs`
- Modify: `tests/vm/execution_scope_tests.rs`
- Modify: `tests/vm/resource_table_tests.rs`

**RED/GREEN slices:**

1. lazy insertion and repeated typed access;
2. separate state for separate scopes;
3. insertion rejection after close begins;
4. close/reset drops state exactly once;
5. stale state token/index cannot alias a fresh scope;
6. ordinary resource and scope-state payload types do not collide;
7. failed initialization/insertion leaves no stale index;
8. `take_state` atomically removes state and index.

### Task 3: Consolidate persistent state and generic reset

**Files:**
- Modify: `src/vm/host_runtime.rs`
- Modify: `src/vm/host_context.rs` when present in the layer
- Modify: `src/vm/mod.rs`
- Modify: focused host runtime/context tests

**RED/GREEN slices:**

1. persistent state survives scope reset;
2. scope state is destroyed by reset;
3. generic reset source contains no adapter name, feature, or concrete `TypeId` branch;
4. one persistent state store provides internal and public SDK access;
5. removal returns uniquely owned state without `Arc::get_mut` assumptions.

### Task 4: Migrate IO

**Files:**
- Modify: `src/builtins/runtime/io/mod.rs`
- Modify: `src/builtins/runtime/io/blocking.rs`
- Modify: `src/builtins/runtime/io/async_io.rs`
- Modify: `src/builtins/runtime/io_wasm.rs`
- Modify: IO lifecycle tests

**RED/GREEN slices:**

1. first IO operation lazily creates `IoState`;
2. repeated operations reuse the same state in one scope;
3. reset removes pending-result state and a later operation gets fresh state;
4. IO policy survives reset via module state;
5. cancellation and quiescence remain correct;
6. no `vm.host.io_state` reference remains.

### Task 5: Migrate SQLite and move its public control API

**Files:**
- Modify: `src/builtins/runtime/sqlite.rs`
- Modify: `src/builtins/runtime/mod.rs`
- Modify: `src/vm/mod.rs`
- Modify: `tests/builtins/sqlite_scope_lifecycle_tests.rs`
- Modify: architecture tests

**RED/GREEN slices:**

1. first SQLite operation lazily creates `SqliteState`;
2. configured policy survives reset;
3. runtime pending state does not survive reset;
4. clear/replace policy is adapter-owned and affects subsequent opens;
5. queued and active operations quiesce before scope close completes;
6. SQLite-disabled builds compile generic VM code unchanged;
7. no SQLite symbol or feature guard remains under `src/vm/**`.

### Task 6: Expose generic state through HostContext and migrate HTTP/SSE

**Files:**
- Modify: `src/vm/host_context.rs`
- Modify: `src/vm/host_extension.rs`
- Modify: `src/builtins/runtime/http/mod.rs`
- Modify: HTTP/SSE operation modules as needed
- Modify: `tests/host_context_arch_tests.rs`
- Modify: `tests/host_sdk_tests.rs`
- Modify: external extension fixture tests
- Modify: HTTP/SSE lifecycle tests

**RED/GREEN slices:**

1. external extension lazily declares scope state through public `HostContext`;
2. scope state closes on reset while extension policy persists;
3. HTTP config persists without a concrete reset exception;
4. HTTP permits/bookkeeping reset through resource close;
5. no HTTP feature guard or concrete HTTP state reference exists under `src/vm/**`.

### Task 7: Rewrite and validate every stack layer

For each final PR head, in a detached isolated worktree run the exact CI workflow commands, including:

```bash
cargo fmt --all -- --check
cargo test -p pd-vm-nostd --no-default-features
cargo tree -p pd-vm-wasm --target wasm32-unknown-unknown --no-default-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build -p pd-vm-cli --release
cargo check -p pd-vm-wasm --target wasm32-unknown-unknown
cargo check -p pd-vm-wasm --target wasm32-unknown-unknown --features runtime
```

Also test feature isolation explicitly:

```bash
cargo check --workspace --no-default-features
cargo check --workspace --features runtime
cargo check --workspace --features runtime,sqlite
cargo check --workspace --features runtime,http-client
```

Use only feature combinations actually defined by the workspace; adjust package selection where a workspace-wide combination is invalid, and record the exact equivalent command.

Acceptance:

- #16 contains exactly four commits;
- #18/#23/#24/#26 contain exactly one commit each over their configured base;
- every PR head is independently formatted, compilable, and CI-green;
- `git grep` confirms no concrete adapter state/policy or adapter feature guard under `src/vm/**`;
- PR #28 remains zero-delta/merged;
- remote updates use one atomic `--force-with-lease` push after local verification;
- GitHub push and pull-request suites pass on every open PR.

---

## 5. Required architecture checks

Before publish, all of the following searches must return no disallowed match:

```bash
git grep -n -E 'IoState|SqliteState|HttpHostState|IoPolicy|SqlitePolicy|HttpConfig' -- src/vm
git grep -n -E 'cfg\([^)]*feature = "(sqlite|http-client)"' -- src/vm
git grep -n 'crate::builtins' -- src/vm
git grep -n -E 'host_function_state|io_state:|sqlite_state:' -- src/vm
```

A match in comments or an architecture test fixture must be classified explicitly; production generic VM code has zero matches.

Allowed adapter guards must be reviewed by location rather than count. Every guard must correspond to one of:

- dependency/build composition;
- builtin module declaration/export;
- concrete host-function registration/composition;
- concrete adapter implementation/test compilation.

No generic API or helper may be guarded solely because its first consumer is an adapter.

---

## 6. Review and publication

1. Implement each owning stack scope with strict RED/GREEN evidence.
2. Run spec-compliance review against this plan.
3. Address findings and repeat spec review until clean.
4. Run code-quality review focused on lifecycle, borrow safety, state indexing, close idempotence, and feature boundaries.
5. Address findings and repeat quality review until clean.
6. Main agent verifies diff, commit ownership, every PR-head CI matrix, and forbidden-symbol scans.
7. Back up current remote refs.
8. Atomically force-with-lease update #16/#18/#23/#24/#26 branches.
9. Verify GitHub PR heads, bases, commit counts, and CI; do not publish a partial stack.

## 7. Risks and mitigations

- **Borrow conflicts between adapter state and resource/operation access:** expose closure-based accessors where returning `&mut T` would hold a borrow across another scope mutation; test real host-function flows.
- **Type-index drift after close/take/failure:** update arena entry and type index in one generic operation; test failed insertion and close retries.
- **State close ordering:** operations drain before resources; scope state that tracks operation results must remain available until operation drain completes, then close with resources.
- **Policy/runtime conflation:** persistent policy and scope runtime state use distinct concrete types and stores; tests assert opposite reset behavior.
- **Intermediate PR breakage:** run exact CI at every PR head, not only stack tip.
- **Feature leakage returning later:** architecture tests scan the entire generic VM tree and apply equally to future adapters.
