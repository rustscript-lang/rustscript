# pd-vm Prelinked Host Call Sites Implementation Plan

> **For Hermes:** Implement with RED/GREEN tests and independent performance measurements. This plan must land before in-trace non-suspending host calls.

## Goal

Replace repeated steady-state host-call resolution and ABI-kind probing with a bound call-site table that caches target, arity, ABI kind, and capability flags while preserving dynamic host state, rebind behavior, and cross-VM safety.

## Architecture

`HostBindingPlan` already resolves names and validates import arity during binding, so this work extends the existing cache rather than adding a parallel resolver. Each VM import receives a `BoundCallSite` descriptor. Interpreter dispatch indexes this descriptor once and invokes the selected ABI directly. Static targets may store function pointers; dynamic/stateful targets retain a VM host slot with explicit lifetime and invalidation rules.

## Current State

- `HostBindingPlan` stores registry slots and `resolved_calls` in `src/vm/host.rs`.
- `resolve_call_target` still checks binding dirtiness, reads the import, rechecks arity, and indexes `resolved_calls` on each call.
- Dispatch then probes whether the target uses args or stack borrowing before matching `VmHostFunction` again.
- The same `Program` can be instantiated by multiple VMs with different host bindings.
- Native trace cache identity does not currently encode a directly embedded host function pointer.

## Target Data Model

```rust
struct BoundCallSite {
    argc: u8,
    target: BoundHostTarget,
    flags: HostCallFlags,
}

enum BoundHostTarget {
    VmDynamic(u16),
    VmStatic(StaticHostFunction),
    StackDynamic(u16),
    StackStatic(StaticHostStackFunction),
    ArgsDynamic(u16),
    ArgsStatic(StaticHostArgsFunction),
    // Added by the lightweight sync ABI plan:
    SyncArgsStatic(StaticSyncArgsFunction),
}

struct HostCallFlags {
    may_suspend: bool,
    may_halt: bool,
    needs_vm: bool,
    stateful: bool,
}
```

The first implementation may use an internal enum or packed flags. Public exposure is unnecessary until a real embedding API needs introspection.

## Safety Rules

- Bind-time arity validation remains authoritative.
- Bytecode validation still rejects malformed call operands.
- Dynamic host objects remain owned by `Vm`; descriptors never outlive them.
- Replacing or rebinding a host marks call-site descriptors dirty and increments a host-binding generation.
- JIT/native code must not hardcode a target while its cache identity ignores the binding generation.
- Two VMs using one `Program` may bind different functions and must call their own targets.
- Legacy positional registration remains supported until a separate compatibility decision removes it.

## Non-Goals

- Adding a new synchronous return ABI
- Executing host calls inside SSA traces
- Changing `CallOutcome` semantics
- Turning dynamic/stateful host objects into raw unmanaged pointers
- Removing import metadata from `Program`

---

### Task 1: Add Dispatch Baselines

**Files:**
- Modify: `tests/jit/perf_tests.rs`
- Modify: `tests/vm/vm_runtime_tests.rs`

Add ignored steady-state benchmarks for all current ABI kinds, with 0, 1, and 2 arguments. Add behavioral tests for two differently bound VMs sharing one `Program`, legacy positional binding, registry-plan binding, and explicit rebinding.

Run:

```bash
cargo test --locked --test jit_tests perf_host_call_steady_state_latency -- --ignored --nocapture
cargo test --locked --test vm_tests host_binding
```

Expected: baseline measurements print; existing binding behavior passes.

Commit:

```bash
git add tests
git commit -m "test: characterize host call-site dispatch"
```

### Task 2: Introduce `BoundCallSite`

**Files:**
- Modify: `src/vm/host.rs`
- Modify: `src/vm/mod.rs`
- Test: `tests/vm/vm_runtime_tests.rs`

Write RED unit tests that inspect internal call-site kind/arity after each registration path. Add the descriptor types and replace `resolved_calls: Vec<u16>` with, or augment it by, `bound_call_sites: Vec<BoundCallSite>`.

Keep descriptor construction centralized. Do not duplicate classification in `HostBindingPlan`, `ensure_call_bindings`, and direct bind methods.

Run:

```bash
cargo test --locked --test vm_tests bound_call_site
```

Expected: descriptors match every ABI kind and import arity.

Commit:

```bash
git add src/vm tests/vm
git commit -m "refactor: describe bound host call sites"
```

### Task 3: Extend `HostBindingPlan`

**Files:**
- Modify: `src/vm/host.rs`
- Test: `tests/vm/vm_runtime_tests.rs`

Have plan preparation validate names/arity once and carry enough metadata to instantiate VM-local descriptors. Preserve registry-slot deduplication for dynamic factories. Verify plan-cache invalidation after registry replacement.

Run:

```bash
cargo test --locked --test vm_tests host_binding_plan
```

Expected: cached and uncached bind paths build equivalent descriptor tables.

Commit:

```bash
git add src/vm/host.rs tests/vm
git commit -m "perf: prelink host binding plans"
```

### Task 4: Dispatch Directly From Descriptors

**Files:**
- Modify: `src/vm/host.rs`
- Modify: `src/vm/mod.rs`
- Test: `tests/vm/vm_runtime_tests.rs`

Write RED tests that use counters or test-only instrumentation to prove steady-state calls do not enter name resolution, import arity validation, or repeated ABI probes.

Change `execute_host_call` so external imports:

1. ensure the table once when dirty;
2. index one `BoundCallSite`;
3. execute its target branch directly.

Retain builtin dispatch separately. Preserve stack, call depth, Yield, Pending, Halt, and error behavior byte for byte.

Run:

```bash
cargo test --locked --test vm_tests host_call
cargo test --locked --test jit_tests aot_replays_host
```

Expected: all host state-machine tests pass.

Commit:

```bash
git add src/vm tests
git commit -m "perf: dispatch prelinked host call sites"
```

### Task 5: Add Binding Generation And Invalidation

**Files:**
- Modify: `src/vm/mod.rs`
- Modify: `src/vm/host.rs`
- Modify: `src/vm/jit/runtime.rs`
- Test: `tests/vm/vm_runtime_tests.rs`
- Test: `tests/jit/jit_tests.rs`

Add a monotonically changing `host_binding_generation` or equivalent identity. Increment it when a bind operation can change target, ABI kind, arity, or capability flags. Rebuild descriptors before the next call.

If native traces currently contain no host target, generation may initially be diagnostic metadata. Define the invalidation hook now so the later `SsaHostCall` plan cannot reuse a trace compiled against incompatible call-site capabilities.

Test:

- same Program, two VMs, different functions;
- rebind before first run;
- rebind after warmup/JIT compilation;
- replacement with a different ABI kind;
- reset/reuse without rebind retains valid descriptors.

Run:

```bash
cargo test --locked --test vm_tests host_rebind
cargo test --locked --features cranelift-jit --test jit_tests host_rebind
```

Expected: no stale target executes.

Commit:

```bash
git add src/vm tests
git commit -m "fix: invalidate prelinked calls after host rebind"
```

### Task 6: Verify And Benchmark

Run:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
cargo test --locked --release --test jit_tests perf_host_call_steady_state_latency -- --ignored --nocapture
```

Report separately:

- bind/load cost per import;
- interpreter steady-state ns/call for each ABI;
- descriptor memory per import;
- rebind rebuild cost.

Commit any final benchmark/docs updates:

```bash
git add .
git commit -m "test: verify prelinked host call dispatch"
```

## Acceptance Criteria

- Import names and arity are resolved during bind, not repeated in the normal call path.
- Each external call performs one call-site table lookup and one target dispatch.
- Static and dynamic VM/stack/args ABIs preserve current semantics.
- Dynamic host object lifetimes remain VM-owned and safe across vector growth/rebuild.
- Rebind cannot execute a stale target.
- Two VMs sharing one Program can use different bindings safely.
- Existing Yield, Pending, Halt, error, AOT, reset, and async bridge tests remain green.
- Performance results describe the remaining dispatch gain accurately and do not claim removal of a name lookup that was already cached.
