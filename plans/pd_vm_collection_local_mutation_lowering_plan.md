# pd-vm Collection Local Mutation Lowering Implementation Plan

> **For Hermes:** Use `subagent-driven-development` to implement this plan task by task, with RED/GREEN verification for every behavioral change.

## Goal

Replace clone-and-rebind collection assignment paths with local-target bytecode that evaluates the key/index and right-hand side first, then mutates the array or map stored in a local slot through an `Arc::get_mut` hot path with `Arc::make_mut` fallback.

## Architecture

RustScript indexed assignment currently loses its local-mutation shape by becoming a generic `set(container, key, value)` call whose result is assigned back to the same local. The compiler will recognize that exact rebind shape and emit local-target bytecode only when the target collection type is proven. The interpreter, VMBC tooling, AOT, and trace JIT will share narrow mutation helpers so array bounds, map-null deletion, COW, ownership, and error behavior remain identical.

## Tech Stack

- Rust 2024 workspace
- `Arc<Vec<Value>>` arrays and `Arc<VmMap>` maps
- stack VM bytecode and VMBC wire format
- whole-program AOT and SSA trace JIT/Cranelift
- Rust unit, integration, wire, compiler, VM, and JIT tests

---

## Current State

The current indexed-assignment path is:

1. `parse_index_assign_with_terminator` constructs `set(Expr::Var(local), key, rhs)` and wraps it in `Stmt::Assign` in `src/compiler/parser/expressions.rs`.
2. `compile_direct_call` evaluates call arguments left to right, so the local collection is cloned onto the operand stack before the key and RHS are evaluated in `src/compiler/codegen.rs`.
3. `builtin_set` takes the stack copy, calls `Arc::make_mut`, mutates it, and returns the collection through `CallReturn` in `src/builtins/runtime/core.rs`.
4. `assign_expr_to_slot` stores the returned collection into the same local.

This pays Arc count traffic, generic builtin dispatch, argument extraction, return handling, and a final local store for an operation whose target is already known.

The availability analyzer already rejects persistent collection aliases before mutation. Runtime COW fallback is still required because `Program` and `Vm` can be constructed through APIs that do not originate from the RustScript frontend, and host-returned/restored values can be shared.

## Scope And Instruction Contract

### Required first-wave instructions

```text
ARRAY_SET_LOCAL slot
  input stack:  [..., index, rhs]
  output stack: [...]
  effect: replace array[index], or append when index == len

ARRAY_PUSH_LOCAL slot
  input stack:  [..., rhs]
  output stack: [...]
  effect: append rhs to the local array

MAP_SET_LOCAL slot
  input stack:  [..., key, rhs]
  output stack: [...]
  effect: insert/replace key, or remove key when rhs is null
```

All three instructions use one local-slot operand in the first implementation, matching the current short local encoding. If wide local opcodes land first, add matching wide forms or route through the shared local-operand design from that work; do not invent a second incompatible width scheme.

### `ARRAY_SWAP_LOCAL` design checkpoint

Do not assign an opcode value until its semantics are approved. The name could mean either:

1. swap two elements: `[..., lhs_index, rhs_index] -> [...]`; or
2. replace one element and return the prior value: `[..., index, rhs] -> [..., old]`.

The implementation task must record the chosen stack effect, bounds behavior, source/IR producer, and drop contract. If no current frontend or VM optimization emits it, defer the opcode rather than shipping dead wire-format surface. This decision does not block `ARRAY_SET_LOCAL`, `ARRAY_PUSH_LOCAL`, or `MAP_SET_LOCAL`.

## Semantic Invariants

- Evaluate key/index before RHS, preserving the current left-to-right order after removal of the side-effect-free target-local load.
- Complete RHS evaluation before borrowing or taking mutable access to the target local.
- `a[i] = a[j]` reads `a[j]` before mutation begins.
- Negative array indices fail with the current error class and message policy.
- `index < len` replaces, `index == len` appends, and `index > len` fails.
- Failed type, conversion, or bounds validation leaves the local value unchanged.
- Map assignment of `null` removes the key.
- Map heap-key identity semantics remain unchanged.
- Unique Arc values mutate without cloning their payload.
- Shared Arc values detach through `Arc::make_mut` and preserve COW behavior.
- Replaced values and detached backing stores follow the existing drop contract exactly once.
- Generic `set()` and `array_push()` remain available for expression forms and dynamic containers.
- No global `Expr::Var` to `Expr::MoveVar` rewrite is allowed.

## Non-Goals

- Removing `Arc` from `Value`
- Changing RustScript assignment syntax
- Making generic `set()` mutate arbitrary values by reference
- Adding a general reference or pointer bytecode model
- Rewriting Rust `HashMap` internals in Cranelift
- Combining this work with host-call ABI changes

---

### Task 1: Add Baseline Compiler And Runtime Characterization

**Objective:** Capture the current generic-call shape and runtime semantics before adding bytecode.

**Files:**
- Modify: `tests/compiler/compiler_rustscript_tests.rs`
- Modify: `tests/vm/vm_runtime_tests.rs`
- Modify: `tests/jit/perf_tests.rs` if present; otherwise add the ignored benchmark beside existing perf tests under `tests/jit/`

**Step 1: Write compiler-shape tests**

Add sources covering:

```rust
let mut a = [10, 20, 30];
a[1] = a[2];
a[1];
```

and:

```rust
let mut m = { key: 1 };
m.key = 2;
m.key;
```

Assert the baseline disassembly contains the current `call set` form. Name the tests so the later RED commit can invert the assertion to require local-target opcodes.

**Step 2: Write semantic characterization tests**

Cover array replace, append-at-len, negative index, index greater than len, map insert, map overwrite, and map null deletion.

**Step 3: Add an ignored mutation benchmark**

Measure at least:

- unique array replace loop
- unique array append loop
- unique map overwrite loop
- generic `set()` control path

Report ns/op or total duration plus operation count. Keep timing assertions out of normal CI.

**Step 4: Run the baseline**

```bash
cargo test --locked --test compiler_tests rustscript_move_and_alias_runtime_cases_work
cargo test --locked --test vm_tests
cargo test --locked --test jit_tests collection_local_mutation -- --ignored --nocapture
```

Expected: semantic tests pass; disassembly confirms the generic path; benchmark prints baseline data.

**Step 5: Commit**

```bash
git add tests/compiler tests/vm tests/jit
git commit -m "test: characterize local collection mutation"
```

---

### Task 2: Define Bytecode And VMBC Surface

**Objective:** Add local-target instruction encoding and prove every wire/tooling consumer decodes it consistently.

**Files:**
- Modify: `src/bytecode.rs`
- Modify: `src/assembler.rs`
- Modify: `src/vmbc.rs`
- Modify: `tests/wire/wire_tests.rs`
- Modify: `tests/wire/assembler_vmbc_edge_tests.rs`

**Step 1: Write failing wire tests**

Add tests that construct each instruction through `BytecodeBuilder`, then verify:

- exact encoded operand length
- assembler mnemonic round trip
- disassembly text including the local slot
- `infer_local_count` includes the instruction operand
- truncated operands are rejected
- VMBC encode/decode preserves the code bytes

Include a program whose only reference to the highest local is `MAP_SET_LOCAL` so local inference cannot accidentally rely on `Ldloc`/`Stloc`.

**Step 2: Run RED**

```bash
cargo test --locked --test wire_tests collection_local
```

Expected: compilation fails because opcode variants and builders do not exist.

**Step 3: Add opcode variants and builders**

Add the three approved variants after the existing opcode range, with one-byte local operands. Update:

- `TryFrom<u8>`
- `operand_len`
- mnemonic render/parse
- both bytecode builder surfaces
- any opcode exhaustiveness checks

**Step 4: Update VMBC analysis and disassembly**

Treat all approved local-target instructions as one-byte-local instructions for bounds/truncation and max-local analysis.

Decide VMBC compatibility explicitly:

- if the wire version promises decoder compatibility across opcode additions, bump the version and reject older/newer versions clearly;
- if the existing contract permits additive opcodes under the same version, document that an older decoder will reject the new opcode.

Do not silently claim old decoders can execute the new code.

**Step 5: Run GREEN**

```bash
cargo test --locked --test wire_tests collection_local
cargo test --locked --test wire_tests
```

Expected: all wire tests pass.

**Step 6: Commit**

```bash
git add src/bytecode.rs src/assembler.rs src/vmbc.rs tests/wire
git commit -m "feat: define local collection mutation bytecode"
```

---

### Task 3: Implement Atomic Interpreter Mutation Helpers

**Objective:** Execute local mutation without cloning on the unique-Arc path while preserving COW and errors.

**Files:**
- Modify: `src/vm/mod.rs`
- Modify: `src/builtins/runtime/core.rs`
- Modify: `tests/vm/vm_runtime_tests.rs`
- Modify: `tests/vm/drop_contract_tests.rs`

**Step 1: Write failing direct-bytecode tests**

Use `BytecodeBuilder` to test each opcode independently of the compiler. Include:

- unique Arc path
- shared Arc fallback created through direct VM values
- wrong local type
- negative and too-large array index
- map null deletion
- stack underflow
- invalid local index

Verify errors leave both local and unaffected stack prefix unchanged according to the VM instruction contract.

**Step 2: Add drop-contract RED tests**

Enable drop-contract events and verify array replacement, array append, map replacement, map removal, and shared detach do not double-drop or leak the prior value.

**Step 3: Extract shared semantic helpers**

Move bounds/key/update logic into narrow helpers callable by both generic builtins and local-target execution. Keep validation before COW detach.

Suggested internal shape:

```rust
fn array_set_in_place(array: &mut SharedArray, index: Value, rhs: Value) -> VmResult<()>;
fn array_push_in_place(array: &mut SharedArray, rhs: Value);
fn map_set_in_place(map: &mut SharedMap, key: Value, rhs: Value);
```

The helpers should try `Arc::get_mut` first and call `Arc::make_mut` only when shared.

**Step 4: Dispatch opcodes in the interpreter**

Pop RHS/key only after confirming stack depth. Validate the local slot and collection type before committing mutation. Avoid leaving a local as `Null` on any error path.

**Step 5: Run GREEN**

```bash
cargo test --locked --test vm_tests collection_local
cargo test --locked --test drop_contract_tests collection_local
```

Expected: all new runtime and drop-contract tests pass.

**Step 6: Commit**

```bash
git add src/vm/mod.rs src/builtins/runtime/core.rs tests/vm
git commit -m "feat: execute local collection mutation bytecode"
```

---

### Task 4: Lower Exact Local-Rebind Shapes

**Objective:** Emit local-target bytecode without changing general move semantics or expression behavior.

**Files:**
- Modify: `src/compiler/codegen.rs`
- Modify as required for preserved type facts: `src/compiler/typing/helpers.rs`
- Test: `tests/compiler/compiler_rustscript_tests.rs`

**Step 1: Turn compiler-shape tests RED**

Change the Task 1 assertions to require:

- typed array assignment emits `array_set_local <slot>`
- typed map/member assignment emits `map_set_local <slot>`
- disassembly no longer contains `call set` for those exact statement forms

Add a control where `set(container, key, rhs)` is used as an expression and must remain a generic call.

**Step 2: Add evaluation-order tests**

Use host calls or local functions that record/encode order in their returned values. Prove:

1. key runs before RHS;
2. RHS completes before target mutation;
3. `a[i] = a[j]` observes the pre-mutation RHS;
4. an RHS error leaves the target local untouched.

**Step 3: Recognize the exact compiler IR shape**

Add one helper that matches only:

```text
Stmt::Assign {
  index: target,
  expr: Call(Set, [Var(source), key, rhs]),
  ...
}
where source == target
```

Use compiler type state to select array or map. If the type is unknown/dynamic, preserve generic lowering. Do not mutate parser `Expr::Var` nodes and do not broaden the optimization to nested or aliased expressions.

For `ARRAY_PUSH_LOCAL`, recognize only an exact same-local rebind of `array_push(local, rhs)` or an explicit indexed-append IR form with proven semantics. Do not rewrite array literal constructor chains.

**Step 4: Compile key and RHS before local access**

Emit key/index and RHS in source order, followed by the local-target opcode. The opcode itself reads the local; codegen must not emit an earlier `ldloc target`.

**Step 5: Run GREEN**

```bash
cargo test --locked --test compiler_tests collection_local
cargo test --locked --test compiler_tests rustscript_move_and_alias
cargo test --locked --test compiler_tests rustscript_mutability
```

Expected: optimized forms use local-target bytecode; move, alias, and mutability tests remain green.

**Step 6: Commit**

```bash
git add src/compiler tests/compiler
git commit -m "perf: lower collection assignment into local mutation"
```

---

### Task 5: Add AOT Support Or Explicit Compile Rejection

**Objective:** Keep whole-program AOT behavior defined for new bytecode.

**Files:**
- Modify: `src/vm/aot/ir.rs`
- Modify: `src/vm/aot/compile.rs`
- Modify as needed: `src/vm/aot/cfg.rs`
- Test: AOT coverage under `tests/jit/jit_tests.rs` or the current AOT test module

**Step 1: Write failing AOT parity tests**

Compile and execute array replace/append and map insert/remove under AOT. Compare final stack and locals with interpreter execution.

**Step 2: Extend AOT IR decoding**

Add typed AOT instruction variants carrying the local slot. Ensure CFG instruction sizing uses `OpCode::operand_len()`.

**Step 3: Lower through narrow runtime helpers**

The first implementation may call dedicated collection mutation intrinsics. It must not route through generic import lookup or change host-call semantics.

If AOT support is intentionally unavailable on a build profile, return a clear compile-time unsupported-op error before execution; never misdecode the operand as another opcode.

**Step 4: Run GREEN**

```bash
cargo test --locked --features cranelift-jit aot_collection_local
```

Expected: AOT parity tests pass on supported targets.

**Step 5: Commit**

```bash
git add src/vm/aot tests/jit
git commit -m "feat: lower local collection mutation in AOT"
```

---

### Task 6: Add Trace Recorder And Native JIT Support

**Objective:** Prevent the new instruction from becoming a permanent trace boundary in collection-heavy loops.

**Files:**
- Modify: `src/vm/jit/recorder.rs`
- Modify: `src/vm/jit/ir.rs`
- Modify: `src/vm/jit/liveness.rs`
- Modify: `src/vm/jit/native/lower.rs`
- Modify: `src/vm/native/bridge.rs`
- Test: `tests/jit/jit_tests.rs`

**Step 1: Write failing trace-shape tests**

Require collection loops to record typed local mutation operations rather than report unsupported opcode or generic builtin call boundaries.

**Step 2: Add effectful SSA instructions**

Use explicit side-effecting variants carrying:

- target local slot
- key/index SSA value
- RHS SSA value
- bytecode IP for error/deopt reporting

Mutation rebinds the symbolic local to the same logical heap value with updated effect state. Keep instruction order observable; do not allow it to move across host calls or guards.

**Step 3: Add native helpers**

Call narrow array/map helpers with explicit Value ownership. Preserve COW and write the updated local state so any later deopt sees the mutated collection.

**Step 4: Add post-mutation exit snapshots**

A guard after mutation must resume after the mutation and must not apply it twice.

**Step 5: Run GREEN**

```bash
cargo test --locked --features cranelift-jit --test jit_tests collection_local
```

Expected: interpreter and JIT results match; trace dump contains typed local mutation; helper/exit counters meet the test contract.

**Step 6: Commit**

```bash
git add src/vm/jit src/vm/native tests/jit
git commit -m "perf: keep local collection mutation inside JIT traces"
```

---

### Task 7: Verify Compatibility Profiles And Measure The Result

**Objective:** Prove the feature across workspace profiles and quantify the gain independently from host-call work.

**Files:**
- Modify only if needed: benchmark/docs touched by prior tasks

**Step 1: Run formatting and static checks**

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
```

**Step 2: Run core tests**

```bash
cargo test --locked --all-targets
cargo test --locked --features cranelift-jit --test jit_tests collection_local
cargo test --locked --test wire_tests
```

**Step 3: Run portability checks**

Use the workspace's existing commands for:

- no-default-features/no-std crate checks
- wasm target checks
- AOT artifact tests

At minimum, ensure every crate that decodes `OpCode` compiles with exhaustive matches updated.

**Step 4: Run release benchmarks**

```bash
cargo test --locked --release --features cranelift-jit --test jit_tests collection_local_mutation -- --ignored --nocapture
```

Record before/after values for unique Arc and shared Arc cases. The acceptance claim must separate:

- dispatch/rebind savings
- payload-clone avoidance
- JIT trace-continuation savings

**Step 5: Final audit**

Search for every exhaustive `OpCode` match and verify the new instructions are handled or deliberately rejected with a test.

**Step 6: Commit final docs/benchmark adjustments**

```bash
git add .
git commit -m "test: verify local collection mutation paths"
```

## Acceptance Criteria

- Typed local array/map assignments no longer compile through generic `set()` plus `stloc`.
- `a[i] = a[j]` reads RHS before mutation and produces interpreter/JIT/AOT-identical results.
- Unique Arc mutation performs no backing collection clone.
- Shared Arc mutation preserves COW through `Arc::make_mut`.
- Array replace/append/error and map insert/remove semantics match the prior builtin behavior.
- Error paths leave the target local unchanged.
- VMBC assembler, disassembler, validation, and local inference understand every shipped opcode.
- Drop-contract tests prove no double drop or missing overwrite accounting.
- Generic expression-level `set()` remains available and unchanged.
- Release benchmark output shows the effect without timing assertions in normal CI.
