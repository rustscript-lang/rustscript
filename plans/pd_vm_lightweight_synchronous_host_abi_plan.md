# pd-vm Lightweight Synchronous Host ABI Implementation Plan

> **For Hermes:** Implement after prelinked call-site descriptors and before JIT in-trace host calls.

## Goal

Add a static, args-only host ABI whose type can return zero or one value or an error, while making Halt, Yield, and Pending unrepresentable.

## Architecture

The new ABI reuses `CallReturn::{None, One(Value)}` and adds a dedicated static function-pointer variant to host registration, VM binding, and `BoundCallSite`. Interpreter dispatch can then skip the general `CallOutcome` state machine. Typed Rust functions continue to use generated argument extraction wrappers; native/JIT callers later use a narrow internal C trampoline rather than calling Rust fat-pointer signatures directly.

## Target API

```rust
pub type StaticSyncArgsFunction =
    fn(&[Value]) -> VmResult<CallReturn>;
```

Public registration/binding:

```rust
HostFunctionRegistry::register_static_sync_args(name, arity, function)
Vm::bind_static_sync_args_function(name, function)
Vm::register_static_sync_args_function(function)
```

`VmResult<Value>` may be offered only as a generated adapter for functions guaranteed to return exactly one value. It is not the generic ABI because it cannot represent no return separately from a returned `Value::Null`.

## Eligibility Contract

A function using this ABI:

- is a static function pointer;
- receives no `&mut Vm`;
- receives a borrowed immutable argument slice;
- cannot return Halt, Yield, or Pending;
- may return `VmError`;
- may have external side effects;
- must not retain borrowed argument references after return;
- must not unwind across a native/JIT trampoline.

The ABI says nothing about purity. JIT and optimizer code must preserve exact call order and call count.

## Non-Goals

- Dynamic/stateful synchronous host objects in the first version
- Async or pending completion
- VM-aware synchronous calls
- Direct JIT calls to Rust `fn(&str, &str) -> bool`
- Replacing existing host ABI variants
- Migrating hosts whose behavior can halt, yield, wait, or mutate VM execution state

---

### Task 1: Add ABI Contract Tests

**Files:**
- Modify: `tests/vm/vm_runtime_tests.rs`
- Modify: `tests/jit/perf_tests.rs`

Write RED tests for:

- zero return (`CallReturn::None`);
- returned `Value::Null`;
- ordinary scalar and heap return;
- host error with exact stack/IP behavior;
- 0/1/2 argument calls;
- prelinked registry-plan binding;
- direct VM binding;
- same Program with different sync targets.

Add an ignored benchmark against `ArgsStatic` returning `CallOutcome::Return`.

Run:

```bash
cargo test --locked --test vm_tests static_sync_args
```

Expected: compilation fails because the ABI does not exist.

Commit:

```bash
git add tests
git commit -m "test: specify synchronous host ABI"
```

### Task 2: Add Function Type And Registration Variants

**Files:**
- Modify: `src/vm/host.rs`
- Modify: `src/lib.rs`
- Test: `tests/vm/vm_runtime_tests.rs`

Add `StaticSyncArgsFunction` and corresponding registry/VM target variants. Export only the embedding surfaces required by existing public host APIs. Integrate with `BoundCallSite` from the prelink plan; do not add a second call-site cache.

Run:

```bash
cargo test --locked --test vm_tests static_sync_args_registration
```

Expected: registration and bind tests pass.

Commit:

```bash
git add src/lib.rs src/vm tests/vm
git commit -m "feat: register static synchronous host functions"
```

### Task 3: Add A Narrow Interpreter Execution Path

**Files:**
- Modify: `src/vm/host.rs`
- Test: `tests/vm/vm_runtime_tests.rs`

Dispatch the sync target directly from `BoundCallSite`:

1. borrow the stack tail;
2. invoke the function pointer;
3. on success truncate arguments and push `CallReturn`;
4. on error preserve the established host-error stack/IP contract;
5. never branch through Halt/Yield/Pending handling.

Keep `call_depth` accounting aligned with existing args-only calls.

Run:

```bash
cargo test --locked --test vm_tests static_sync_args
cargo test --locked --test vm_tests args_only_call
```

Expected: sync tests pass and existing yieldable args tests remain green.

Commit:

```bash
git add src/vm/host.rs tests/vm
git commit -m "perf: execute synchronous host returns directly"
```

### Task 4: Generate Typed Sync Adapters

**Files:**
- Modify: `pd-host-function/src/lib.rs`
- Modify as required: `src/builtins/runtime/typed.rs`
- Test: proc-macro tests under `pd-host-function` and runtime integration tests

Extend macro metadata with an explicit opt-in such as:

```rust
#[pd_host_function(name = "namespace::compare", sync)]
fn compare_impl(lhs: VmStringRef<'_>, rhs: VmStringRef<'_>) -> bool;
```

Generate an adapter with the `StaticSyncArgsFunction` shape. Continue to use existing `borrow_arg` extraction and existing `IntoVmValue`/return helpers. Reject invalid combinations at compile time:

- `sync` plus `&mut Vm`;
- `sync` plus a return type that maps to `CallOutcome`;
- signatures requiring taken/mutable args if the borrowed ABI cannot express them.

Do not generate an `extern "C"` typed-fat-pointer entry here. The JIT plan owns the thin native trampoline.

Run:

```bash
cargo test --locked -p pd-host-function
cargo test --locked --test vm_tests typed_static_sync_args
```

Expected: valid typed functions register and run; invalid attribute combinations fail compile tests.

Commit:

```bash
git add pd-host-function src/builtins/runtime/typed.rs tests
git commit -m "feat: generate typed synchronous host adapters"
```

### Task 5: Migrate A Small Proven Host Set

**Files:**
- Modify only concrete host registrations that satisfy the contract
- Test each migrated integration surface

Start with deterministic comparison/query functions that:

- need no VM reference;
- always return normally or error;
- never yield/wait/halt;
- use borrowed arguments.

Do not migrate `runtime::sleep`, `runtime::exit`, IO, transport, or any host that can cross an async boundary.

For each migration, add a parity test comparing old and new result/error behavior before deleting the old registration.

Commit in small groups:

```bash
git commit -m "perf: bind non-suspending comparison hosts"
```

### Task 6: Verify And Benchmark

Run:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
cargo test --locked --release --test jit_tests perf_host_call_steady_state_latency -- --ignored --nocapture
```

Measure:

- old `ArgsStatic` + `CallOutcome`;
- new `StaticSyncArgsFunction` + `CallReturn`;
- typed two-string comparison adapter;
- error path separately from success.

Commit final docs/benchmark changes:

```bash
git add .
git commit -m "test: verify lightweight synchronous host ABI"
```

## Acceptance Criteria

- The ABI type cannot express Halt, Yield, or Pending.
- Zero return and returned null remain distinct.
- Success avoids the full `CallOutcome` state-machine match.
- Host errors preserve stack/IP semantics and execute the host once.
- Typed adapters borrow strings/bytes/collections without payload copies where supported.
- Invalid `sync` macro signatures fail at compile time.
- Existing yieldable host variants remain source-compatible.
- Benchmarks compare like-for-like static args-only calls and show the measured delta.
