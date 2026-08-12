# Frame-Aware Local Allocation and Callable Slot Reduction Implementation Plan

**Goal:** Correct named-script-call liveness for real call frames, report aggregate frame-local overflow with real counts, and stop reserving one hidden local for every directly called named function.

**Architecture:** Treat each script invocation frame as a separate local-address space. Caller liveness includes argument evaluation and values used after the call, while callee body locals are analyzed inside the callee frame. Keep conservative ownership rules for dynamic callables and captures until their frame/environment behavior is proved separately. After correctness is established, lower eligible named calls through an additive direct-script-call opcode and materialize `Value::Callable` locals only where runtime identity or an environment is required.

**Tech Stack:** Rust 2024, RustScript frontend/IR/lifetime passes, bytecode assembler, interpreter, VMBC, Trace JIT, AOT, debugger, wasm analyzer, and `pd-vm-nostd`.

---

## Status and dependency

- Status: proposed.
- Execute this plan before `2026-08-11_wide-local-bytecode.md`.
- The frame-aware correction and diagnostic milestones are correctness work and may land before direct-call optimization.
- The direct-call milestone depends on frame-aware allocation being verified independently.
- The agent storage program is a regression shape, not an owner of compiler policy. Core tests must use self-contained RustScript fixtures or generated sources.

## Observed baseline

The production-shaped storage source currently merges to:

```text
frontend locals:                       205
named script function implementations: 77
```

The existing compiler produces:

```text
31 dispatch branches: 178 data slots + 77 callable slots = 255 frame slots
32 dispatch branches: 181 data slots + 77 callable slots = 258 frame slots
```

`Compiler::prepare_named_callables` rejects the second program because `Ldloc` and `Stloc` still use one-byte operands. It reports `LocalSlot::MAX` (`65535`) as a sentinel, hiding the actual total of 258.

A diagnostic experiment showed:

```text
remove only caller-live += callee-footprint: 19 data + 77 callable = 96
also remove named-call cross-frame edges:      6 data + 77 callable = 83
```

The experiment passed the complete `compiler_tests` and `vm_tests` integration targets, but it is evidence only. This plan requires dedicated ownership, capture, drop, recursion, module, JIT, AOT, and no-std coverage before changing production behavior.

## Root cause to preserve in tests

Real script frames were introduced in commit `0a8652c`. Runtime entry allocates a new `local_base`, resizes the locals array for the callee frame, copies parameters/captures into that frame, and restores the caller frame on return.

The lifetime pipeline still carries two pre-frame assumptions for known named calls:

1. `LivenessRewriter::add_expr_uses` unions the callee's transitive footprint into the caller live set.
2. `LocalSlotAllocator::collect_expr_constraints` adds caller/callee cross-live graph edges.

Those assumptions make locals from separate frames interfere. Recursive call footprints can become `full_footprint`, magnifying the same problem. A separate cost comes from `prepare_named_callables`: every function implementation gets one hidden callable local, every call loads that local, and every runtime frame initializes all root callable bindings.

## Semantic invariants

The implementation must preserve all of the following:

- Each execution frame has an independent relative local namespace and `local_base`.
- Arguments are fully evaluated in the caller before callee frame entry.
- Caller locals used after return remain live across the call.
- Callee locals are dropped according to the callee frame's own control flow.
- Copy, move, borrow, and borrow-mut capture cells retain existing alias and drop behavior.
- Capturing named functions and dynamic local callables retain environment identity.
- Recursion retains depth checks, self identity where required, and frame isolation.
- Exported callables remain resolvable through the public embedding API.
- Programs with more than 256 genuinely simultaneous locals in one frame continue to fail until the wide-local plan lands.
- Interpreter, JIT, AOT, no-std, VMBC, debugger, REPL, and wasm consumers remain behaviorally aligned.

## Scope boundary

### In scope

- Frame-aware named-call liveness and interference constraints.
- Focused cleanup of named-call-only transitive footprint machinery after all callers are audited.
- Accurate aggregate frame-local diagnostics.
- Selective materialization of hidden named callable slots.
- An additive direct script call opcode for non-capturing statically resolved named calls.
- Required wire, interpreter, JIT, AOT, debugger, wasm, and no-std support for that opcode.
- Regression fixtures representing large dispatch across one file and semantic modules.
- Documentation of local allocation, frame ownership, and callable materialization.

### Out of scope

- `Ldloc` or `Stloc` operands wider than `u8`.
- More than 65,536 local slots.
- New language syntax.
- Changes to invocation item streams, host capabilities, agent lifecycle, or storage schemas.
- Rewriting dynamic `LocalCall` or closure-call conservatism without separate capture evidence.
- Inlining named functions as a substitute for frame-aware allocation.
- Agent-specific compiler exceptions, source-name checks, or compatibility wrappers.
- A generic register allocator or SSA rewrite.

## Target architecture

### Local pressure

For each named script function and the root body:

```text
same-frame live ranges -> one interference graph domain
caller arguments       -> caller domain
callee body locals     -> callee domain
capture environment    -> explicit capture cells and capture metadata
```

The compiler may still assign one shared relative slot number to locals from different functions because runtime frame bases separate them.

### Named call lowering

Use two paths:

```text
Direct non-capturing named call
  arguments
  CallScript(prototype_id, argc)

Runtime-valued call
  load/materialize Value::Callable
  arguments
  CallValue(argc)
```

A function requires callable materialization when any of these holds:

- it is exported under the current `ExportedCallable { local_slot }` contract;
- it is referenced as a value;
- it captures an environment;
- a dynamic call site can target it;
- its runtime self identity is required by a capturing/dynamic recursion path.

Plain direct calls, including non-capturing direct recursion, use `CallScript` and do not require a hidden local.

## Milestone 1: Lock the failure shape and true-limit control

**Objective:** Add RED tests that distinguish cross-frame over-allocation from genuine same-frame local pressure.

**Files:**

- Modify: `tests/compiler/compiler_common_tests.rs`
- Modify: `tests/compiler/compiler_rustscript_tests.rs`
- Modify: `tests/compiler/module_import_tests.rs`
- Create: `tests/fixtures/modules/frame_local_dispatch/main.rss`
- Create: module files under `tests/fixtures/modules/frame_local_dispatch/`

**Steps:**

1. Add a generated single-file program with roughly 77 named functions and a 32-branch dispatcher. Each callee owns two parameters and one local. The dispatcher must return a deterministic scalar so the test executes after compilation.
2. Add a semantic-module fixture with the same call graph split across multiple modules. This proves the result is independent of linker local-base assignment and import discovery order.
3. Assert both programs compile and execute under the interpreter. On the current tree, record RED as aggregate `LocalSlotOverflow(LocalSlot::MAX)`.
4. Assert `Program.local_count` stays bounded by per-frame pressure plus currently required callable slots. Before direct-call optimization, use an upper bound such as 100 rather than an exact coloring number.
5. Keep and strengthen the existing generated test with 257 values simultaneously live in one function. Assert it fails with a real frame-limit error after Milestone 3.
6. Add a 256-live boundary case that compiles and reads the highest short slot.

**Focused RED command:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/frame-local-target \
  cargo test --locked --test compiler_tests frame_local_dispatch
```

Expected before implementation: the 32-branch cases fail at compile time; the true 257-live control continues to fail for the intended reason.

**Commit after GREEN:**

```bash
git add tests/compiler/compiler_common_tests.rs \
        tests/compiler/compiler_rustscript_tests.rs \
        tests/compiler/module_import_tests.rs \
        tests/fixtures/modules/frame_local_dispatch/
git commit -m "test(compiler): cover frame-local pressure across named calls"
```

Do not commit a branch on which the new success cases remain failing.

## Milestone 2: Make named-call liveness frame-aware

**Objective:** Stop treating a statically resolved callee body as live inside its caller frame.

**Files:**

- Modify: `src/compiler/lifetime/liveness.rs`
- Modify if comments/contracts require it: `src/compiler/lifetime/availability.rs`
- Test: `tests/compiler/compiler_common_tests.rs`
- Test: `tests/compiler/compiler_rustscript_tests.rs`
- Test: `tests/vm/drop_contract_tests.rs` through the `vm_tests` target

**Steps:**

1. In `LivenessRewriter::add_expr_uses`, classify `Expr::Call(index, ..., args)` using `function_impls.contains_key(index)`.
2. For a known named script call, add only caller-side argument uses. Do not union `function_footprint(index)` into the caller live set.
3. Continue analyzing each `FunctionImpl` body independently through `rewrite_function_impl` and `function_body_live_out`.
4. Preserve persistent capture sources and captured slots through `persistent_capture_slots`, function declaration rewriting, and closure environment metadata.
5. Leave `Expr::LocalCall` and unknown dynamic targets on their existing conservative path in this milestone.
6. Add runtime tests where:
   - a caller local remains usable after a callee returns;
   - caller and callee locals are assigned the same relative slot but retain different values;
   - copy/move/borrow/borrow-mut captures observe existing behavior;
   - direct and mutual recursion retain independent frame values;
   - cancellation/yield in a callee resumes with caller locals intact;
   - drop-contract counts do not double-drop or omit caller/callee heap values.
7. Run the large dispatch tests and inspect `Program.local_count`; expected data pressure should fall from 181 to approximately 19 even before removing the allocator cross-edge.
8. Remove `LivenessRewriter` footprint fields or methods only when repository search proves they have no remaining closure/dynamic-call use. Do not delete shared capture analysis merely because named calls no longer need it.

**Focused commands:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/frame-local-target \
  cargo test --locked --test compiler_tests frame_local
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/frame-local-target \
  cargo test --locked --test compiler_tests named_function
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/frame-local-target \
  cargo test --locked --test vm_tests drop_contract
```

Each command must select at least one test.

**Commit:**

```bash
git add src/compiler/lifetime/liveness.rs \
        src/compiler/lifetime/availability.rs \
        tests/compiler/compiler_common_tests.rs \
        tests/compiler/compiler_rustscript_tests.rs \
        tests/vm/drop_contract_tests.rs
git commit -m "fix(compiler): make named-call liveness frame-aware"
```

## Milestone 3: Remove stale named-call interference edges

**Objective:** Make graph coloring match the runtime frame boundary without weakening dynamic callable safety.

**Files:**

- Modify: `src/compiler/lifetime/liveness.rs`
- Test: compiler and module tests from Milestone 1

**Steps:**

1. In `LocalSlotAllocator::collect_expr_constraints`, stop adding caller-live versus callee-footprint edges for statically resolved named calls.
2. Continue collecting constraints for argument expressions in the caller.
3. Continue building cliques and def/live edges within each function body.
4. Preserve explicit capture-copy interference and persistent capture slots.
5. Preserve conservative `LocalCall` and closure-call handling until a separate test proves a narrower rule.
6. Add a test that two functions with disjoint execution frames reuse the same relative slots even when one calls the other recursively.
7. Add a negative control where two values truly overlap within one function and must receive different slots.
8. Assert the large storage-shaped fixture falls to a small per-frame data count, expected near 6. Avoid making the exact greedy-color result a public contract; assert a conservative upper bound such as 20.
9. Search for remaining named-call transitive-footprint use. Retain any helper still required by dynamic closures or capture lifetime analysis.

**Focused command:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/frame-local-target \
  cargo test --locked --test compiler_tests frame_local_slot_reuse
```

**Commit:**

```bash
git add src/compiler/lifetime/liveness.rs \
        tests/compiler/compiler_common_tests.rs \
        tests/compiler/compiler_rustscript_tests.rs \
        tests/compiler/module_import_tests.rs
git commit -m "fix(compiler): isolate named-call interference by frame"
```

## Milestone 4: Report real aggregate frame-local pressure

**Objective:** Replace the `65535` sentinel diagnostic with actionable counts while preserving individual operand overflow errors.

**Files:**

- Modify: `src/compiler/mod.rs`
- Modify: `src/compiler/codegen.rs`
- Modify: `src/compiler/diagnostics.rs`
- Modify: `tests/common/mod.rs`
- Modify: `tests/compiler/diagnostics_tests.rs`
- Modify: `pd-vm-wasm/src/analyzer.rs` if it matches compiler errors exhaustively

**Design:**

Add a dedicated error shape, for example:

```rust
CompileError::FrameLocalLimitExceeded {
    data_slots: usize,
    callable_slots: usize,
    total_slots: usize,
    max_slots: usize,
}
```

Keep `CompileError::LocalSlotOverflow(slot)` for a concrete local index that cannot be emitted by the current ISA.

**Steps:**

1. In `prepare_named_callables`, compute `data_slots`, materialized callable slots, total, and maximum before mutating callable metadata.
2. Return `FrameLocalLimitExceeded` with actual values when the aggregate exceeds 256.
3. Remove uses of `LocalSlot::MAX` as an aggregate sentinel.
4. Render a diagnostic such as:

```text
frame requires 258 local slots (181 data + 77 callable); short bytecode supports 256
```

5. Preserve source diagnostics where an owning function/source span is available; otherwise use a program-level diagnostic without pretending slot 65535 exists.
6. Update wasm/common error mappings and snapshot tests.
7. Add direct tests for arithmetic overflow separately from the ordinary 256 ceiling.
8. Confirm no error text parser is introduced in core or downstream tests.

**Focused command:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/frame-local-target \
  cargo test --locked --test compiler_tests frame_local_limit_diagnostic
```

**Commit:**

```bash
git add src/compiler/mod.rs src/compiler/codegen.rs src/compiler/diagnostics.rs \
        tests/common/mod.rs tests/compiler/diagnostics_tests.rs \
        pd-vm-wasm/src/analyzer.rs
git commit -m "fix(compiler): report actual frame-local pressure"
```

## Milestone 5: Classify named functions that require runtime materialization

**Objective:** Separate statically called script functions from named functions that need a `Value::Callable` identity.

**Files:**

- Modify: `src/compiler/ir.rs`
- Modify: `src/compiler/parser/` consumers only if use metadata is unavailable after parsing
- Modify: `src/compiler/linker.rs`
- Modify: `src/compiler/pipeline.rs`
- Modify: `src/compiler/codegen.rs`
- Test: compiler, module, exported callable, capture, and recursion tests

**Steps:**

1. Add a compiler-internal use classification keyed by function index or `SymbolId`. Suggested facts:

```text
called_directly
referenced_as_value
exported
captures_environment
dynamic_target_required
runtime_self_required
```

2. Collect value references from `Expr::FunctionRef`, exported declarations, closure/capture metadata, and any dynamic callable assignment.
3. Carry the classification through semantic module merge using resolved function identity, never source names.
4. Define `requires_callable_slot` from the semantic facts. Do not infer it from call count or source spelling.
5. Keep one prototype for every script function. Allocate a hidden local only for `requires_callable_slot` functions.
6. Keep exported functions materialized under the current `ExportedCallable { local_slot }` API in this plan. On-demand exported prototype creation is a later API proposal.
7. Ensure capturing named functions retain declaration-time environment construction and cannot use an environment-free direct call path.
8. Add tests for:
   - direct-only helper: no hidden slot;
   - exported direct helper: hidden slot retained;
   - function stored in a local/map/array: hidden slot retained;
   - capturing named function: hidden slot and environment retained;
   - non-capturing direct recursion: no hidden slot required after `CallScript` exists;
   - capturing recursion: runtime self slot retained;
   - same names in different modules: classification follows `SymbolId`.

This milestone may introduce metadata and tests before changing call lowering, but every commit must remain executable. If the compiler cannot omit a slot until `CallScript` exists, keep allocation behavior unchanged and commit only the classification plus passing tests of the classification helper.

**Commit:**

```bash
git add src/compiler/ir.rs src/compiler/parser/ src/compiler/linker.rs \
        src/compiler/pipeline.rs src/compiler/codegen.rs \
        tests/compiler/ tests/wire/
git commit -m "refactor(compiler): classify named callable materialization"
```

Stage explicit files rather than directory globs during execution.

## Milestone 6: Add direct script-call bytecode and interpreter support

**Objective:** Call eligible environment-free named functions by prototype ID without loading a hidden callable local.

**Files:**

- Modify: `src/bytecode.rs`
- Modify: `src/assembler.rs`
- Modify: `src/compiler/codegen.rs`
- Modify: `src/vm/mod.rs`
- Modify: `src/vm/instance.rs` only if shared frame-entry logic belongs there
- Modify: `src/vmbc.rs`
- Modify: `src/debug_info.rs`
- Modify: debug-related bytecode scanners in `src/vmbc.rs` and `src/cli.rs`
- Modify: `src/cli.rs`
- Modify: `pd-vm-wasm/src/analyzer.rs`
- Test: compiler, VM, wire, debugger, REPL, wasm tests

**ISA contract:**

Reserve the next opcode after `CallValue`:

```text
CallScript = 0x1A
operands   = prototype_id:u32 little-endian, argc:u8
length     = 5 bytes
```

Do not repurpose `Call`, which remains host/builtin-only. Do not change `CallValue`.

**Steps:**

1. Add assembler emission and decoding for `CallScript`.
2. Add a shared VM helper that enters a script frame from `(prototype_id, optional environment, operands, continuation)`.
3. Route `CallValue` and `CallScript` through that helper. `CallScript` supplies no callable environment and must reject prototypes that require captures.
4. Preserve arity validation, depth limits, interruption ticks, return continuation, stack cleanup, and drop-contract behavior.
5. In `compile_function_call`, emit argument expressions followed by `CallScript` for an eligible function. Keep `Ldloc + CallValue` for materialized or capturing functions.
6. Omit hidden slots and root bindings for direct-only functions. Recompute `frame_local_count` from data slots plus materialized callable slots.
7. Retain `ExportedCallable.local_slot` and public `resolve_exported_callable` behavior.
8. Update opcode walkers, jump/region validation, debugger stepping, disassembly, and wasm analysis for the five-byte operand.
9. Bump VMBC from V11 to V12 and regenerate wire fixtures. The V12 decoder must reject malformed/truncated `CallScript` operands deterministically. Compatibility policy must follow the current release plan; do not silently decode V11 bytes under changed semantics.
10. Add tests proving:
    - direct-only functions emit `CallScript` and no `Ldloc` target slot;
    - environment-bearing functions still emit `CallValue`;
    - direct recursion and mutual recursion work;
    - exported function resolution remains unchanged;
    - malformed prototype IDs and arity produce typed VM errors;
    - short programs with no script calls retain unchanged instruction bytes apart from the declared VMBC version policy.

**Focused commands:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/frame-local-target \
  cargo test --locked --test compiler_tests direct_script_call
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/frame-local-target \
  cargo test --locked --test vm_tests call_script
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/frame-local-target \
  cargo test --locked --test wire_tests call_script
```

**Commit:**

```bash
git add src/bytecode.rs src/assembler.rs src/compiler/codegen.rs \
        src/vm/mod.rs src/vm/instance.rs src/vmbc.rs src/debug_info.rs \
        src/cli.rs pd-vm-wasm/src/analyzer.rs tests/
git commit -m "feat(vm): call static script functions by prototype"
```

During execution replace directory entries with the exact changed paths.

## Milestone 7: Add no-std, Trace JIT, native, and AOT parity

**Objective:** Ensure `CallScript` is a supported semantic operation across every execution backend.

**Files:**

- Modify: `pd-vm-nostd/src/program.rs`
- Modify: `pd-vm-nostd/src/vm.rs`
- Modify: `pd-vm-nostd/src/vmbc.rs`
- Modify: `pd-vm-nostd/src/error.rs` if new validation errors are needed
- Modify: `src/vm/jit/trace.rs`
- Modify: `src/vm/jit/recorder.rs`
- Modify: `src/vm/jit/inline.rs`
- Modify: `src/vm/jit/native/` lowering and runtime files
- Modify: `src/vm/native/bridge.rs`
- Modify: `src/vm/aot/cfg.rs`
- Modify: `src/vm/aot/ir.rs`
- Modify: `src/vm/aot/ssa.rs`
- Modify: `src/vm/aot/compile.rs`
- Modify: `src/vm/aot/runtime.rs`
- Modify: `src/vm/aot/artifact.rs`
- Test: no-std, JIT, native bridge, AOT, artifact, and backend parity tests

**Steps:**

1. Mirror the opcode and operand layout in no-std. Share semantic expectations through fixtures, not source-code dependency.
2. Teach every bytecode scanner to skip five operand bytes and preserve call boundaries.
3. Record `CallScript` with prototype identity and call-site IP. Reuse existing callable-frame JIT machinery instead of creating a second frame model.
4. Update inline candidate analysis to resolve the direct prototype without reading a source callable local.
5. Lower native/AOT direct calls through existing environment-free function-item paths.
6. Preserve deopt/exit restoration, frame keys, stack bases, return IPs, interruption checks, and typed call errors.
7. Increment `NATIVE_CALLABLE_ABI_VERSION`, AOT artifact version/ABI, and program/native cache revisions exactly once for the new opcode semantics.
8. Add parity tests for interpreter, trace/native JIT, AOT, and no-std using direct calls, recursion, nested calls, cancellation checks, and failure exits. Prefix the AOT-focused test names with `aot_call_script` so the verification command selects them explicitly.
9. Confirm JIT/AOT never reinterpret `CallScript` as host `Call` or dynamic `CallValue`.

**Focused commands:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/frame-local-target \
  cargo test --locked --test jit_tests call_script --features cranelift-jit
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/frame-local-target \
  cargo test --locked aot_call_script --features cranelift-jit
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/frame-local-target \
  cargo test --locked -p pd-vm-nostd call_script
```

**Commit:**

```bash
git add pd-vm-nostd/src/ src/vm/jit/ src/vm/native/ src/vm/aot/ \
        tests/jit/ tests/wire/ pd-vm-nostd/tests/
git commit -m "feat(vm): support direct script calls across backends"
```

Stage exact files during execution.

## Milestone 8: Documentation and cleanup

**Objective:** Remove obsolete assumptions and document the final pressure model.

**Files:**

- Modify: compiler lifetime module documentation
- Modify: callable/runtime documentation under `docs/`
- Modify: VMBC/opcode documentation
- Modify: `README.md` only if it states the old all-function hidden-slot model

**Steps:**

1. Document same-frame interference and cross-frame reuse.
2. Document which named functions receive hidden callable slots.
3. Document `Call`, `CallValue`, and `CallScript` ownership separately.
4. Remove dead named-call transitive-footprint caches and comments only after repository search proves no remaining use.
5. Remove temporary diagnostics and instrumentation.
6. Verify no agent/storage operation names appear in compiler logic.

**Commit:**

```bash
git add src/compiler/lifetime/ docs/ README.md
git commit -m "docs(compiler): define frame-local allocation boundaries"
```

Stage exact changed files only.

## Verification matrix

Use one isolated target directory for every Cargo command:

```bash
export CARGO_TARGET_DIR=/mnt/TEMP/rustscript/frame-local-target
```

### Focused correctness

```bash
cargo fmt --all -- --check
cargo test --locked --test compiler_tests frame_local
cargo test --locked --test compiler_tests named_function
cargo test --locked --test compiler_tests closure
cargo test --locked --test vm_tests call_script
cargo test --locked --test vm_tests drop_contract
cargo test --locked --test compiler_tests module_import
cargo test --locked --test wire_tests call_script
```

If `module_import_tests` is a module inside `compiler_tests` rather than a standalone Cargo target, run the corresponding `compiler_tests` filter and require at least one selected test.

### Backend and target parity

```bash
cargo test --locked --test jit_tests call_script --features cranelift-jit
cargo test --locked aot_call_script --features cranelift-jit
cargo test --locked -p pd-vm-nostd
cargo test --locked -p pd-vm-wasm
```

### Full gates

```bash
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
git diff --check
```

### Required observations

- Every filtered command reports at least one selected test.
- The 32-branch single-file and module fixtures compile and execute.
- The 257-simultaneous-local control still fails before wide locals land.
- Aggregate diagnostics print real data/callable/total counts.
- Direct-only named functions do not consume hidden callable slots.
- Exported and captured named functions retain runtime callable identity.
- No backend silently falls back because it cannot decode `CallScript`.
- Worktree is clean after each scoped commit.

## Stop conditions

Stop and report the exact blocker before continuing if any of these occurs:

- Removing named-call footprint propagation breaks capture ownership that cannot be represented through existing frame/cell metadata.
- `CallScript` requires a second call-frame implementation instead of reusing the existing callable entry helper.
- A backend cannot preserve return/deopt/drop/cancellation behavior for direct calls without a private executor or compatibility side channel.
- Selective materialization changes exported callable identity or public embedding behavior without an explicit API decision.
- Intermediate commits cannot pass their focused default-feature gates.

## Target criteria

- Known named callees no longer inflate caller live sets with callee body locals.
- Locals from separate script frames can reuse relative slot numbers.
- Dynamic callable and capture paths retain conservative correctness.
- The storage-shaped 32-branch fixture remains below 256 slots without wide bytecode.
- Aggregate overflow reports actual counts, never slot 65535 as a placeholder.
- Direct-only non-capturing functions use `CallScript` and allocate no hidden callable local.
- Functions requiring value identity/environment/export remain materialized and use `CallValue` where appropriate.
- Interpreter, JIT, AOT, no-std, debugger, REPL, wasm, and VMBC agree on direct-call semantics.
- Genuine same-frame pressure beyond 256 remains rejected until the wide-local plan is implemented.
