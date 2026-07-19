# pd-vm JIT Short-Function Hot-Loop Inlining Implementation Plan

**Goal:** Inline hot, short RustScript function-item calls into caller traces while preserving real callable semantics and restoring exact VM state on every cold exit, guard miss, interruption, and deoptimization.

**Architecture:** Add per-call-site target profiling and a conservative inline eligibility pass, then let the trace recorder maintain a bounded virtual frame stack across eligible `CallValue`/`Ret` pairs. Normal execution maps arguments, locals, captures, and return values directly in SSA without entering a physical VM frame. Each exit carries virtual-frame snapshots so the existing cold Rust recovery path can reconstruct the physical caller/callee frame chain only when native execution actually leaves the trace.

**Tech Stack:** Rust, pd-vm bytecode and callable metadata, trace JIT SSA, Cranelift native lowering, inherited-state ABI, Criterion, Cargo workspace tests.

---

## 1. Motivation and Current State

The completed inherited-state trace-link work removed repeated full frame restoration between compatible native traces. The remaining sort workload still crosses a `CallValue` boundary for short script functions and therefore retains native call/return bookkeeping that the old flattened compiler path did not pay.

The final inherited-state gate recorded in [`pd_vm_call_overhead_reduction_plan.md`](pd_vm_call_overhead_reduction_plan.md) was:

| Endpoint | Four-run median |
|---|---:|
| crates.io `pd-vm 0.23.1` | `72.091 ms` |
| inherited-state candidate | `134.495 ms` |
| ratio | `1.866x` |

The current recorder terminates a trace at every dynamic script call:

- [`src/vm/jit/recorder.rs`](../src/vm/jit/recorder.rs) lowers `DecodedOp::CallValue` to `SsaTerminator::CallValue` and ends recording.
- [`src/vm/jit/native/lower.rs`](../src/vm/jit/native/lower.rs) materializes the call exit and invokes the inherited frame-enter helper.
- callee `Ret` leaves the physical frame through the inherited frame-leave path before the caller continuation can run.
- [`src/vm/jit/trace.rs`](../src/vm/jit/trace.rs) profiles hot loop entries and trace exits, but does not yet profile call-site target identity.

The target shape is the LuaJIT-style model already identified in the call-overhead plan: a hot caller trace records through a guarded script call, keeps the callee as a virtual frame, records through `Ret`, and reconstructs interpreter frames only on a true exit.

## 2. Scope

### 2.1 Phase-1 supported calls

Phase 1 admits only calls that satisfy every rule below:

- caller is a root-frame hot loop trace
- callable resolves to one immutable `RootCallableBinding`
- target is `CallableTarget::ScriptFunction`
- callable kind is `FunctionItem`
- callable environment is absent
- arity exactly matches the prototype
- callee has one bytecode `Ret` and no fallthrough beyond its `FunctionRegion`
- no recursive edge
- no nested script `CallValue`
- no yielding host import
- no backward branch in the callee
- decoded callee length is at most 32 instructions
- touched callee-local footprint is at most 8 slots, excluding untouched global-layout slots
- combined caller and inlined-callee trace length remains within `JitConfig::max_trace_len`

Specialized non-yielding builtins already supported by the SSA recorder may remain inside an eligible leaf. Mutable collection operations are enabled only after virtual-frame deopt restoration and ownership tests pass.

### 2.2 Later extensions

After phase 1 meets correctness and performance gates:

1. allow inline depth 2 for caller → leaf → leaf
2. admit monomorphic no-capture dynamic callables with an exact identity guard
3. admit immutable copied captures
4. admit simple forward branches
5. consider a two-entry polymorphic inline cache only when profiling proves a meaningful workload

### 2.3 Non-goals

- no source-level call flattening
- no change to interpreter callable semantics
- no inlining of recursive, yielding, polymorphic, or mutable-capture calls in phase 1
- no unbounded bytecode cloning
- no new VM-visible opcode
- no fast path that skips callable arity, depth, ownership, fuel, epoch, or invalidation semantics
- no warm-path Rust helper per inlined static function-item call

## 3. Correctness Invariants

1. **Callable identity:** an inlined call executes only the profiled/proven prototype. A target change takes the existing `CallValue` path.
2. **Frame semantics:** every cold exit can reconstruct the same `ExecutionFrame`, operand stack, locals, continuation IP, prototype ID, and active IP that the interpreter would have at that bytecode point.
3. **Ownership:** each owned heap value has one owner transfer. Borrowed values are never released. A deopt packet cannot classify `BoxHeapPtr` as an inherited owned value.
4. **Capture semantics:** phase 1 accepts no environment. Later captured-call support must guard exact environment identity and preserve capture-cell aliasing.
5. **Depth semantics:** virtual inline depth counts toward `max_script_call_depth`. Phase 1 root-only admission makes the physical depth known; later non-root admission requires a native depth guard.
6. **Interruption:** fuel and epoch exits preserve the same resume IP and do not repeat or skip a side effect.
7. **Mutation ordering:** a collection mutation visible before deopt remains visible exactly once after restoration.
8. **Invalidation:** mode changes, reset, callable release, trace invalidation, or program replacement clear all call-site profiles and inline target references.
9. **Bounded growth:** inline depth, callee instruction count, touched locals, total trace length, compile time, and machine-code growth all have hard caps.
10. **Cold-path reuse:** existing full Rust recovery remains authoritative for unsupported calls and failed guards.

## 4. Data Model

Create [`src/vm/jit/inline.rs`](../src/vm/jit/inline.rs) and register it from [`src/vm/jit/mod.rs`](../src/vm/jit/mod.rs).

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CallSiteKey {
    pub(crate) caller_frame_key: u64,
    pub(crate) call_ip: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct CallSiteProfile {
    pub(crate) prototype_id: u32,
    pub(crate) observations: u64,
    pub(crate) mismatches: u64,
    pub(crate) monomorphic: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InlineRejectReason {
    NonRootCaller,
    UnknownTarget,
    PolymorphicTarget,
    HostTarget,
    CapturedCallable,
    ArityMismatch,
    Recursive,
    NestedScriptCall,
    YieldingCall,
    BackwardBranch,
    MultipleReturns,
    TooManyInstructions,
    TooManyTouchedLocals,
    TraceBudgetExceeded,
}

#[derive(Clone, Debug)]
pub(crate) struct InlineCandidate {
    pub(crate) prototype_id: u32,
    pub(crate) entry_ip: usize,
    pub(crate) end_ip: usize,
    pub(crate) parameter_slots: Vec<u16>,
    pub(crate) touched_locals: Vec<u16>,
    pub(crate) decoded_instruction_count: usize,
}
```

Phase-1 target proof uses `Program::root_callable_bindings` plus the callable value's SSA `source_local`. It must not rely only on `ValueType::Callable`.

For later dynamic targets, retain a strong `SharedCallable` in the profile/trace and compare exact callable identity. Do not persist a naked environment pointer that can be reused after deallocation.

Extend [`src/vm/jit/ir.rs`](../src/vm/jit/ir.rs) so exits can carry zero or more virtual frames, outermost to innermost:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VirtualFrameSnapshot {
    pub(crate) prototype_id: u32,
    pub(crate) call_ip: usize,
    pub(crate) return_ip: usize,
    pub(crate) resume_ip: usize,
    pub(crate) operand_stack: Vec<SsaMaterialization>,
    pub(crate) locals: Vec<SsaMaterialization>,
    pub(crate) dirty_locals: Vec<bool>,
}
```

The exact field placement may be adjusted to avoid duplicating existing `SsaExit` data, but one exit must describe the complete physical frame chain required by recovery.

## 5. Task Plan

### Task 1: Add Call-Site Profiling and Metrics

**Objective:** Measure actual script-call target stability before changing trace shape.

**Files:**
- Create: `src/vm/jit/inline.rs`
- Modify: `src/vm/jit/mod.rs`
- Modify: `src/vm/jit/trace.rs`
- Modify: `src/vm/mod.rs`
- Modify: `src/vm/native/bridge.rs`
- Test: `tests/jit/jit_tests.rs`
- Test: `tests/jit/perf_tests.rs`

**Steps:**

1. Add RED tests proving profiles are keyed by `(caller_frame_key, call_ip)`, not only bytecode IP.
2. Add RED tests for monomorphic, target-change, reset, and invalidation behavior.
3. Record the actual `prototype_id` immediately after `CallValue` callable validation and before physical frame entry.
4. Record observations from both interpreter call entry and `pd_vm_native_enter_call_value` so a native caller still trains the profile.
5. Clear profiles on `TraceJitEngine::clear`, JIT mode changes, program replacement, and `Vm::reset_for_reuse` where current trace state is invalidated.
6. Extend `JitMetrics` and JIT dump output with:
   - `script_call_observations`
   - `monomorphic_call_sites`
   - `polymorphic_call_sites`
   - `inline_attempts`
   - `inline_successes`
   - `inline_rejects` by reason
   - `inline_deopts`
   - `inline_virtual_frame_restores`
7. Keep metrics diagnostic-only; no admission decision may depend on a counter that is absent after deserialization or reset.

**Focused commands:**

```bash
cargo test -p pd-vm --features cranelift-jit --test jit_tests call_site_profile -- --nocapture
cargo test -p pd-vm --features cranelift-jit --test jit_tests trace_jit_executes_call_value_natively_inside_loop -- --nocapture
```

**Expected:** profile tests pass; existing script-call behavior and trace counts remain unchanged.

**Commit:**

```bash
git add src/vm/jit src/vm/mod.rs src/vm/native/bridge.rs tests/jit

git commit -m "feat(jit): profile script call targets"
```

### Task 2: Implement Conservative Inline Eligibility Analysis

**Objective:** Resolve immutable root function items and reject every unsupported callee before recorder mutation.

**Files:**
- Modify: `src/vm/jit/inline.rs`
- Modify: `src/vm/jit/recorder.rs`
- Test: `src/vm/jit/recorder.rs`
- Test: `tests/jit/jit_tests.rs`

**Steps:**

1. Add table-driven RED tests for every `InlineRejectReason`.
2. Decode the target `ScriptFunction` region without changing the active recorder cursor.
3. Count decoded instructions and touched local slots; do not use `CallablePrototype::frame_local_count` as the leaf-local footprint because callable frames use the merged global slot layout.
4. Reject backward branches, region escapes, recursive target IDs, nested `CallValue`, yielding calls, malformed immediates, and multiple returns.
5. Allow only builtin calls already accepted by `emit_specialized_builtin_call`; all other calls reject the candidate.
6. Resolve phase-1 targets only when the callable SSA value has a `source_local` matching an immutable `RootCallableBinding`.
7. Cache immutable candidate analysis by `prototype_id`; clear it with the program/JIT engine.

**Focused commands:**

```bash
cargo test -p pd-vm --lib vm::jit::recorder::tests::inline -- --nocapture
cargo test -p pd-vm --features cranelift-jit --test jit_tests inline_eligibility -- --nocapture
```

**Expected:** analysis tests pass and no trace is inlined yet.

**Commit:**

```bash
git add src/vm/jit/inline.rs src/vm/jit/recorder.rs tests/jit/jit_tests.rs

git commit -m "feat(jit): classify inline script callees"
```

### Task 3: Add Virtual-Frame Snapshots to SSA Exits

**Objective:** Represent deopt state inside an inlined callee without creating a physical frame on the normal path.

**Files:**
- Modify: `src/vm/jit/ir.rs`
- Modify: `src/vm/jit/deopt.rs`
- Modify: `src/vm/jit/recorder.rs`
- Modify: `src/vm/jit/native/lower.rs`
- Modify: `src/vm/native/mod.rs`
- Modify: `src/vm/native/bridge.rs`
- Test: `src/vm/jit/ir.rs`
- Test: `src/vm/jit/deopt.rs`
- Test: `tests/jit/jit_tests.rs`

**Steps:**

1. Add RED verifier tests for malformed frame order, invalid local counts, bad continuation IPs, unsupported ownership classes, and snapshot values missing from an exit input list.
2. Extend `SsaExit` with virtual-frame snapshots and include snapshot values in `exit_inputs` deterministically.
3. Reuse inherited-state representation classification for scalar, pointer, tagged borrowed, and tagged owned values.
4. Reject `BoxHeapPtr` transfer in a virtual snapshot, matching current side-entry ownership restrictions.
5. Add a cold bridge entry that:
   - restores the physical caller through existing sparse/full exit logic
   - validates the callable and prototype
   - creates the physical callee frame with the existing callable-entry semantics
   - replaces callee stack/locals with the snapshot materializations
   - sets `Vm.ip` to the inlined callee exit IP
6. Restore multiple virtual frames outermost to innermost for the future depth-2 extension, even though phase 1 emits one frame.
7. Make validation atomic: reject all metadata before mutating VM stack, locals, capture cells, or frame vectors.

**Required RED/GREEN cases:**

- scalar local deopt inside callee
- owned string/array/map local deopt
- borrowed collection value deopt
- dirty callee local plus clean caller local
- deopt after one visible collection mutation
- invalid packet leaves VM state unchanged
- reset after restored frame releases every owner once

**Focused commands:**

```bash
cargo test -p pd-vm --lib vm::jit::deopt::tests -- --nocapture
cargo test -p pd-vm --lib vm::native::bridge::tests -- --nocapture
cargo test -p pd-vm --features cranelift-jit --test jit_tests inline_deopt -- --nocapture
```

**Expected:** snapshots verify and restore correctly; ordinary exits remain byte-for-byte compatible with zero virtual frames.

**Commit:**

```bash
git add src/vm/jit/ir.rs src/vm/jit/deopt.rs src/vm/jit/recorder.rs \
  src/vm/jit/native/lower.rs src/vm/native tests/jit/jit_tests.rs

git commit -m "feat(jit): restore virtual inline frames on deopt"
```

### Task 4: Record Through a Pure Static Leaf Call and Return

**Objective:** Keep one eligible no-capture leaf call inside the caller SSA trace.

**Files:**
- Modify: `src/vm/jit/recorder.rs`
- Modify: `src/vm/jit/inline.rs`
- Modify: `src/vm/jit/ir.rs`
- Test: `src/vm/jit/recorder.rs`
- Test: `tests/jit/jit_tests.rs`

**Steps:**

1. Add RED tests for a root loop calling a branchless arithmetic leaf.
2. Replace the single symbolic `Frame` with a bounded recorder frame stack.
3. At an eligible `CallValue`:
   - pop the callable and arguments from the caller symbolic stack
   - create a virtual callee frame
   - map arguments to `CallablePrototype::parameter_slots`
   - initialize touched non-parameter locals to `Null`
   - jump the recorder cursor to `ScriptFunction::entry_ip`
   - retain caller `resume_ip` in the virtual frame
4. Record callee instructions into the same SSA graph.
5. At callee `Ret`:
   - move the return value to the caller symbolic stack
   - emit ownership cleanup for callee temporaries/locals
   - restore the caller recorder cursor to `resume_ip`
6. Continue recording to the caller loop backedge.
7. Annotate `op_names` with `inline_call:<prototype_id>` and `inline_ret` for diagnostics.
8. Keep old `SsaTerminator::CallValue` behavior for every rejection.

**Focused commands:**

```bash
cargo test -p pd-vm --features cranelift-jit --test jit_tests trace_jit_inlines_static_leaf -- --nocapture
cargo test -p pd-vm --features cranelift-jit --test jit_tests trace_jit_inline_guard_exit_restores_callee -- --nocapture
```

**Expected:** arithmetic leaf calls show one caller trace containing `inline_call` and no warm-path frame-enter/frame-leave helper.

**Commit:**

```bash
git add src/vm/jit tests/jit/jit_tests.rs

git commit -m "feat(jit): inline static leaf calls in root traces"
```

### Task 5: Lower Inline Frames and Preserve Ownership

**Objective:** Generate native code for inline call/return without physical frame helpers on the normal path.

**Files:**
- Modify: `src/vm/jit/native/lower.rs`
- Modify: `src/vm/jit/native/mod.rs`
- Modify: `src/vm/native/offsets.rs`
- Modify: `src/vm/native/bridge.rs`
- Test: `src/vm/jit/native/lower.rs`
- Test: `src/vm/jit/native/mod.rs`
- Test: `tests/jit/jit_tests.rs`

**Steps:**

1. Add RED native tests that count frame-enter, frame-leave, frame-state, and sparse-restore calls for an inline leaf loop.
2. Lower virtual argument/local values as ordinary SSA values; do not write `Vm.locals` on the normal path.
3. Transfer owned return values from callee to caller and release dead callee owners exactly once.
4. Route only virtual-frame exits to the cold restore bridge from Task 3.
5. Preserve inherited-state direct links after the caller loop backedge.
6. Verify generated code contains no call to:
   - `enter_call_value_inherited`
   - `leave_frame_inherited`
   - `frame_state`
   for the normal inline call/return edge.
7. Include inline code size and compile time in native telemetry.

**Focused commands:**

```bash
cargo test -p pd-vm --lib vm::jit::native -- --nocapture
cargo test -p pd-vm --features cranelift-jit --test jit_tests inline_ownership -- --nocapture
cargo test -p pd-vm --features cranelift-jit --test jit_tests inline_frame_helpers -- --nocapture
```

**Expected:** helper counts are zero on the normal inline edge; drop-contract and deopt tests pass.

**Commit:**

```bash
git add src/vm/jit/native src/vm/native tests/jit/jit_tests.rs

git commit -m "feat(jit): lower virtual leaf call frames"
```

### Task 6: Enable the Mutable Collection Leaf Needed by Sort

**Objective:** Inline the short collection-manipulation function used by the sort workload without changing alias or COW behavior.

**Files:**
- Modify: `src/vm/jit/inline.rs`
- Modify: `src/vm/jit/recorder.rs`
- Modify: `src/vm/jit/native/lower.rs`
- Test: `tests/jit/jit_tests.rs`
- Test: `tests/jit/perf_tests.rs`

**Steps:**

1. Add RED tests for a no-capture `swap`-shaped leaf using array get/set operations.
2. Cover distinct array, shared/COW array, same-index swap, out-of-bounds guard failure, and alias-visible mutation.
3. Ensure every guard before and after the first mutation has a virtual-frame snapshot at the exact callee IP.
4. Verify a deopt after the first mutation does not repeat that mutation when the interpreter resumes.
5. Add a bounded ignored characterization test that reports:
   - inline call count
   - inline deopt count
   - enter/leave helper count
   - frame-state count
   - sparse-restore count
   - native direct links
   - trace count
   - machine-code bytes
6. Keep the eligibility scanner restricted to existing specialized collection operations; generic host/builtin boundaries still reject.

**Focused commands:**

```bash
cargo test -p pd-vm --features cranelift-jit --test jit_tests inline_array_swap -- --nocapture
cargo test -p pd-vm --release --features cranelift-jit --test jit_tests perf_jit_inline_short_leaf -- --ignored --nocapture
```

**Expected:** sorted output and COW/drop semantics match the interpreter; the hot swap edge has no physical frame helper on the normal path.

**Commit:**

```bash
git add src/vm/jit tests/jit/jit_tests.rs tests/jit/perf_tests.rs

git commit -m "feat(jit): inline short collection leaf calls"
```

### Task 7: Add Guarded Monomorphic Dynamic Calls and Depth 2

**Objective:** Extend only after static leaf inlining meets the sort gate.

**Files:**
- Modify: `src/vm/jit/inline.rs`
- Modify: `src/vm/jit/recorder.rs`
- Modify: `src/vm/jit/ir.rs`
- Modify: `src/vm/jit/native/lower.rs`
- Modify: `src/vm/jit/trace.rs`
- Test: `tests/jit/jit_tests.rs`

**Steps:**

1. Gate this task on Task 6 performance evidence.
2. Retain the exact `SharedCallable` target in the trace/profile to prevent pointer reuse.
3. Emit one target-identity guard before entering a dynamic inline frame.
4. Send guard miss to the original caller `CallValue` IP before any callee side effect.
5. Admit only `env == None` in the first dynamic step.
6. Add depth-2 virtual snapshots and reject depth 3.
7. Add recursion and mutual-recursion rejection tests.
8. Add mutable callable rebind and target-change tests; the changed target must execute through the existing call boundary.
9. Add non-root depth-limit guard before permitting later non-root inline support.

**Focused commands:**

```bash
cargo test -p pd-vm --features cranelift-jit --test jit_tests inline_monomorphic -- --nocapture
cargo test -p pd-vm --features cranelift-jit --test jit_tests inline_target_change -- --nocapture
cargo test -p pd-vm --features cranelift-jit --test jit_tests inline_depth -- --nocapture
```

**Expected:** exact target guards preserve semantics; static call sites remain helper-free.

**Commit:**

```bash
git add src/vm/jit tests/jit/jit_tests.rs

git commit -m "feat(jit): guard monomorphic inline calls"
```

### Task 8: Full Verification and Alternating A/B Gate

**Objective:** Prove correctness across targets and retain the optimization only when structural and wall-clock gates pass.

**Files:**
- Modify: `plans/pd_vm_jit_short_function_hot_loop_inline_plan.md` with measured results
- Modify only if needed: `tests/jit/perf_tests.rs`

**Correctness commands:**

```bash
cargo fmt --all -- --check
RUSTFLAGS="-Dwarnings" cargo clippy --workspace --all-targets --all-features
cargo test --workspace
cargo test -p pd-vm --features cranelift-jit --test jit_tests
cargo test -p pd-vm --release --features cranelift-jit --test jit_tests -- --ignored --nocapture
```

Expected JIT baseline before new tests: `128 passed`, `0 failed`, `11 ignored`; the counts increase only by the added inline tests.

**Cross-platform gate:**

- Ubuntu CI
- macOS CI
- Windows CI
- strict Rust lint
- VS Code extension checks

All required checks must pass before merge.

**Sort A/B setup:**

- benchmark repository: `/mnt/TEMP/workspace/script-bench-rs`
- benchmark revision: `6fd2a61f0a2164c40f841d585ac7dbae55362b15`
- workload: `Sort Rust objects`, 10,000 objects
- baseline A: crates.io `pd-vm 0.23.1`
- baseline B: pre-inline master `938174e`
- candidate: inline branch
- CPU affinity: CPU 11, with CPU 3 as a reproduction check
- Criterion sample size: 10
- run order: A/B/candidate/candidate/B/A, then reverse on the next round
- minimum: four valid runs per endpoint

Run each isolated export with:

```bash
taskset -c 11 cargo bench --bench rustscript_jit --features pd_vm_jit -- "Sort Rust objects"
```

The baseline and candidate exports must have separate target directories and lockfiles. The benchmark repository itself must remain unmodified.

**Mandatory structural gates:**

- sorted output validation passes on every run
- eligible hot call sites report `inline_successes > 0`
- normal inline edge reports zero frame-enter/frame-leave helper calls
- virtual-frame restore occurs only on cold exits
- helper calls attributable to the short script call fall by at least 10x
- interpreter fallback remains zero on the measured native path
- no growth in owned-value drop mismatches

**Mandatory performance gates:**

- sort candidate median is at least 20% below pre-inline master median on the same CPU
- sort candidate median is at most `1.25x` crates.io `0.23.1`
- non-call tight-loop, array-builtin, map-builtin, AES, and IFFT JIT medians regress by no more than 5% after alternating reruns
- JIT compile time grows by no more than 20% for the sort program
- installed native code size grows by no more than 30%

**Stretch gate:** sort candidate at or below `1.10x` crates.io `0.23.1`.

If the mandatory sort gate misses, stop feature widening. Record call-site profile, inline/deopt counts, helper counts, trace/code size, and compile time, then identify the largest remaining cost before enabling captures, PICs, deeper inline, or larger callees.

**Final plan update and commit:**

```bash
git add plans/pd_vm_jit_short_function_hot_loop_inline_plan.md tests/jit/perf_tests.rs

git commit -m "docs(plan): record short-call inline gates"
```

## 6. Test Matrix

| Case | Inline? | Required result |
|---|---:|---|
| root loop → no-capture arithmetic leaf | yes | one caller trace, exact result |
| root loop → no-capture array swap leaf | yes after Task 6 | exact COW and mutation semantics |
| root loop → captured closure | no in phase 1 | existing `CallValue` path |
| root loop → yielding host call leaf | no | existing wait/yield behavior |
| root loop → recursive leaf | no | depth semantics unchanged |
| target changes after trace install | guard miss / fallback | new target executes |
| out-of-bounds inside inline callee | deopt | exact callee IP and error |
| fuel expires at inline boundary | deopt/yield | no replayed side effect |
| epoch deadline expires | deopt/yield | resumable exact state |
| reset after inline execution | yes | profiles/traces cleared, owners released once |
| JIT invalidation | yes | inline target references released |
| branchless callee exceeds 32 ops | no | reject reason recorded |
| touched locals exceed 8 | no | reject reason recorded |
| nested eligible leaf at depth 2 | later yes | two virtual snapshots restore correctly |
| nested depth 3 | no | existing physical call path |

## 7. Rollback Boundaries

Each stage is independently removable:

1. profiling has no execution effect
2. eligibility analysis has no execution effect
3. zero-frame exits preserve existing deopt encoding
4. disabling inline admission restores `SsaTerminator::CallValue`
5. cold virtual restore is unreachable when inline admission is off
6. mutable collection admission has its own eligibility switch
7. dynamic target guards and depth 2 remain separately gated

Keep one internal configuration constant for emergency disablement during development. Remove it only after all performance and CI gates pass.

## 8. Definition of Done

- phase-1 eligible leaf calls remain in the caller trace through `Ret`
- warm execution performs no physical frame-enter/frame-leave helper for the inline edge
- every guard, error, fuel exit, epoch exit, and invalidation path restores exact VM state
- static function-item, mutable collection, target-change, depth, ownership, and reset tests pass
- strict clippy, workspace tests, focused JIT tests, ignored perf suite, and cross-platform CI pass
- alternating sort A/B meets all mandatory structural and performance gates
- measured results and any retained scope limits are written back into this plan
