# pd-vm JIT Non-Yielding Host Call Implementation Plan

> **For Hermes:** Implement only after prelinked call sites and the lightweight static synchronous ABI are complete.

## Goal

Allow SSA traces to execute eligible static synchronous host calls and continue in the same native trace, with exact-once side effects, correct Value ownership, direct error propagation, and safe host rebinding.

## Architecture

The recorder will consult an immutable snapshot of VM-bound call-site capabilities. Calls whose descriptor is `StaticSyncArgsFunction` become effectful `SsaHostCall` instructions; all other external calls remain trace boundaries. Native lowering materializes arguments into owned/borrowed Value slots, invokes a thin `extern "C"` trampoline through the VM call-site table, imports the zero/one return into SSA state, and continues. Errors return `STATUS_ERROR` and never replay the host in the interpreter.

## Dependencies

Required first:

1. `plans/pd_vm_prelinked_host_call_sites_plan.md`
2. `plans/pd_vm_lightweight_synchronous_host_abi_plan.md`

Collection local mutation is independent, though both features must preserve ordered SSA effects if present in one trace.

## Current State

- `src/vm/jit/recorder.rs` marks builtins as non-yielding and all external calls as yielding.
- Unsupported calls terminate the trace at the call IP after materializing stack/locals.
- native trace compilation receives `JitTrace`, not VM binding metadata.
- the native trace cache can reuse code across VM instances of the same Program.
- existing bridge errors use shared runtime plumbing that must not cause cross-VM error confusion.

## Eligibility

First release admits only call sites with all of:

- static function binding;
- lightweight sync args ABI;
- no VM reference;
- no Halt/Yield/Pending result type;
- valid bind-time arity;
- return arity represented by `CallReturn::{None, One}`.

Dynamic, VM-aware, stack-borrowed, async, yieldable, pending, and halting calls remain boundaries.

## Core Correctness Rules

- A side-effecting host executes exactly once.
- Native success advances logical IP to the instruction after the call.
- A later guard exit resumes after the call with the host result materialized.
- Host error propagates as `VmError`; interpreter does not retry the call.
- Rebinding to another target with the same compatible ABI redirects safely through the VM table.
- Rebinding to an incompatible ABI invalidates the trace before execution.
- Two VMs sharing one Program call their own bound targets.
- Argument and return Arc clone/drop behavior matches interpreter execution.
- Fuel/epoch accounting at the call boundary matches the documented JIT policy.
- Initial implementation may disable linked-trace handoff for traces containing effectful host calls while still continuing within the current trace.

## Native ABI

Use a thin internal ABI, for example:

```rust
extern "C" fn pd_vm_native_sync_host_call(
    vm: *mut Vm,
    call_site: u32,
    args: *const Value,
    argc: u32,
    out: *mut Value,
    returned: *mut u8,
) -> i32;
```

The exact signature may change after layout review. Required properties:

- no Rust fat pointers cross generated-code boundaries;
- output ownership is explicit;
- zero return is distinct from returned null;
- integer status reports success/error;
- error storage is VM-local or otherwise concurrency-safe;
- no unwind crosses `extern "C"`.

## Non-Goals

- Yieldable or pending calls inside traces
- Dynamic/stateful host calls in the first release
- Calling Rust typed implementations directly from Cranelift
- Treating non-suspending calls as pure
- Reordering, eliminating, or common-subexpression-folding host calls
- Enabling linked handoff across effectful calls in the first release

---

### Task 1: Add Exact-Once And Cache-Safety RED Tests

**Files:**
- Modify: `tests/jit/jit_tests.rs`
- Modify: `tests/jit/perf_tests.rs`

Add tests for:

- a hot loop with one sync host call per iteration;
- host result feeding arithmetic/branch operations in the same loop;
- side-effect counter equals loop iteration count;
- a guard failure after a successful call does not repeat it;
- host error executes once and returns the original error;
- zero return, returned null, scalar return, heap return;
- same Program bound differently in two VMs;
- rebind after trace warmup;
- concurrent VMs running the same Program.

Before implementation, assert the trace currently terminates at the call boundary or records `has_yielding_call`.

Add an ignored benchmark comparing interpreter, current call-boundary JIT, and eventual in-trace JIT.

Run:

```bash
cargo test --locked --features cranelift-jit --test jit_tests sync_host_trace
```

Expected: new continuation assertions fail.

Commit:

```bash
git add tests/jit
git commit -m "test: specify in-trace synchronous host calls"
```

### Task 2: Expose Bound Call-Site Capability Snapshots To The Recorder

**Files:**
- Modify: `src/vm/mod.rs`
- Modify: `src/vm/host.rs`
- Modify: `src/vm/jit/trace.rs`
- Modify: `src/vm/jit/recorder.rs`
- Test: `tests/jit/jit_tests.rs`

Add a compact recorder-facing snapshot containing, per import:

- call-site index;
- argc;
- ABI/capability class;
- return arity/type hint if available;
- host-binding generation or compatibility signature.

Do not give the recorder raw pointers to dynamic objects. Change trace observation/compilation APIs so they receive this snapshot from `Vm`; `Program` alone cannot prove an external call is non-suspending.

Run:

```bash
cargo test --locked --features cranelift-jit --test jit_tests sync_host_metadata
```

Expected: recorder sees eligible/ineligible classifications correctly.

Commit:

```bash
git add src/vm tests/jit
git commit -m "feat: expose bound host capabilities to JIT"
```

### Task 3: Add Effectful `SsaHostCall`

**Files:**
- Modify: `src/vm/jit/ir.rs`
- Modify: `src/vm/jit/recorder.rs`
- Modify: `src/vm/jit/liveness.rs`
- Modify: `src/vm/jit/deopt.rs`
- Test: `tests/jit/jit_tests.rs`

Suggested IR shape:

```rust
SsaInstKind::HostCall {
    call_site: u16,
    args: Vec<SsaValueId>,
    return_kind: SsaHostReturnKind,
    resume_ip: usize,
}
```

If allocation in `Vec` is undesirable, use a compact boxed slice or fixed small-vector representation consistent with the existing IR style.

Verifier requirements:

- all args are defined and materializable;
- call-site arity matches arg count;
- return representation matches declared return kind;
- the instruction is effectful and ordered;
- exits after the call use post-call state.

Recorder behavior:

- eligible sync static call: emit `SsaHostCall`, pop args symbolically, push optional result, continue;
- all other host calls: preserve current boundary behavior;
- zero-benefit trace policy should count useful work after/before the in-trace call normally.

Run:

```bash
cargo test --locked --features cranelift-jit --test jit_tests sync_host_ssa
```

Expected: SSA text includes `host_call`; trace continues to later operations.

Commit:

```bash
git add src/vm/jit tests/jit
git commit -m "feat: record synchronous host calls in SSA"
```

### Task 4: Implement A Concurrency-Safe Native Trampoline

**Files:**
- Modify: `src/vm/native/bridge.rs`
- Modify: `src/vm/host.rs`
- Modify: `src/vm/mod.rs`
- Test: `tests/jit/jit_tests.rs`

Add the thin `extern "C"` trampoline. It must:

1. validate VM pointer and call-site index;
2. verify the descriptor remains sync-compatible;
3. construct a borrowed argument slice from explicit Value slots;
4. execute the host once;
5. write zero/one return with explicit ownership;
6. store error without process-global cross-VM confusion;
7. return a simple status.

Prefer VM-local pending native error storage. If shared bridge error plumbing remains, prove concurrent calls cannot take another VM's error.

Handle panic policy explicitly. Either require `panic=abort` at this boundary or wrap host execution with an approved containment strategy; no unwind may cross generated code.

Run:

```bash
cargo test --locked --features cranelift-jit --test jit_tests sync_host_trampoline
cargo test --locked --features cranelift-jit --test jit_tests concurrent_sync_host
```

Expected: correct returns/errors under sequential and concurrent execution.

Commit:

```bash
git add src/vm tests/jit
git commit -m "feat: add native synchronous host trampoline"
```

### Task 5: Lower `SsaHostCall` In Cranelift

**Files:**
- Modify: `src/vm/jit/native/lower.rs`
- Modify as needed: `src/vm/jit/native/mod.rs`
- Modify: `src/vm/jit/runtime.rs`
- Test: `tests/jit/jit_tests.rs`

For each host call:

1. materialize tagged argument Values into owned temporary slots;
2. call the imported trampoline with VM pointer and call-site index;
3. branch on status;
4. on error, clean owned temporaries and return `STATUS_ERROR`;
5. on success, import zero/one return into SSA;
6. update logical IP/resume state;
7. continue lowering the current block.

All owned argument/output temporaries must participate in the same cleanup discipline used by other heap-returning SSA helpers.

Do not side-exit to the interpreter at the original call IP on error. Do not hardcode the host implementation pointer in machine code.

Initially set trace metadata so linked handoff remains disabled when `has_call` is true, even though `has_yielding_call` is false. Continuing inside the current trace is required; cross-trace chaining can be measured later.

Run:

```bash
cargo test --locked --features cranelift-jit --test jit_tests sync_host_native
```

Expected: native execution count increases; host loop stays within the trace; exact-once tests pass.

Commit:

```bash
git add src/vm/jit tests/jit
git commit -m "perf: continue JIT traces through sync host calls"
```

### Task 6: Add Post-Call Deopt And Rebind Guards

**Files:**
- Modify: `src/vm/jit/deopt.rs`
- Modify: `src/vm/jit/native/lower.rs`
- Modify: `src/vm/jit/runtime.rs`
- Modify: `src/vm/host.rs`
- Test: `tests/jit/jit_tests.rs`

Add tests where a successful host call is followed by:

- type guard failure;
- branch side exit;
- collection helper error;
- trace-length exit.

Verify interpreter-visible stack/locals contain the host result and resume after the call.

At trace entry or call site, compare the compiled capability signature with current binding state. Compatible target replacement may remain indirect; incompatible ABI/flags must block/invalidate and return to normal recording/interpreter behavior without executing the call twice.

Run:

```bash
cargo test --locked --features cranelift-jit --test jit_tests sync_host_deopt
cargo test --locked --features cranelift-jit --test jit_tests sync_host_rebind
```

Expected: no stale target and no replay.

Commit:

```bash
git add src/vm tests/jit
git commit -m "fix: preserve sync host effects across JIT exits"
```

### Task 7: Align Interruption, Reset, And Metrics

**Files:**
- Modify: `src/vm/jit/runtime.rs`
- Modify: `src/vm/jit/trace.rs`
- Modify: `src/vm/mod.rs`
- Test: `tests/jit/jit_tests.rs`

Verify:

- fuel accounting at host-call boundaries;
- epoch behavior under the current JIT policy;
- `reset_for_reuse` retains valid call-site bindings and traces;
- rebind generation changes after reset still invalidate correctly;
- metrics distinguish in-trace sync host calls from call-boundary exits;
- trace dumps show call-site index/ABI class without raw addresses.

Run:

```bash
cargo test --locked --features cranelift-jit --test jit_tests sync_host_interrupt
cargo test --locked --features cranelift-jit --test jit_tests sync_host_reset
```

Expected: interruption and reset tests pass.

Commit:

```bash
git add src/vm tests/jit
git commit -m "test: align sync host traces with VM lifecycle"
```

### Task 8: Full Verification And Performance Gate

Run:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
cargo test --locked --features cranelift-jit --test jit_tests sync_host
cargo test --locked --release --features cranelift-jit --test jit_tests perf_sync_host_trace -- --ignored --nocapture
```

Report:

- interpreter ns/call;
- JIT call-boundary ns/call before this plan;
- JIT in-trace ns/call after this plan;
- trace exits per operation;
- boxed argument/result materializations;
- machine-code size change;
- behavior with a later guard failure.

Commit final docs/metrics changes:

```bash
git add .
git commit -m "test: verify in-trace synchronous host calls"
```

## Acceptance Criteria

- Eligible static sync calls become `SsaHostCall` and continue in the current trace.
- Yieldable/dynamic/VM-aware calls remain boundaries.
- Side effects and errors execute exactly once.
- Host errors propagate directly without interpreter replay.
- Post-call deopt restores result, stack, locals, and resume IP correctly.
- No native trace embeds a target pointer without binding identity/invalidation coverage.
- Same-Program/different-VM bindings execute the correct target.
- Rebind to incompatible capabilities invalidates or blocks the trace.
- Argument/return ownership passes drop and concurrent VM tests.
- Release benchmark demonstrates the measured benefit and trace-exit reduction.
