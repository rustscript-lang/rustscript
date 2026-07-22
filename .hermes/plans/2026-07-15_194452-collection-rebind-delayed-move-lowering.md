# Collection Rebind Delayed-Move Lowering Implementation Plan

> **For Hermes:** Use `subagent-driven-development` to implement this plan task-by-task, with `test-driven-development` for each behavior change.

**Goal:** Lower same-local collection rebinds as an existing-bytecode ownership transfer so `set` / `array_push` receives the unique collection without introducing opcodes, runtime pattern matching, or deep COW copies.

**Architecture:** Keep the target local readable while evaluating the key and RHS. Immediately before the existing builtin `Call`, emit the existing move-clear sequence `ldc null; stloc target`; the collection handle already on the stack then becomes the owned call argument. The existing post-call `stloc target` writes the result back. Alias legality remains owned by the current lifetime pass.

**Tech Stack:** Rust, RustScript compiler IR/codegen, existing `Ldloc`/`Ldc`/`Stloc`/`Call`, Arc COW collections, interpreter/JIT/AOT/no-std compatibility tests.

---

## Root cause

The current pipeline loses move intent at lowering:

1. `parse_index_assign_with_terminator` builds `set(Expr::Var(target), key, value)` and wraps it in `Stmt::Assign { index: target, ... }`.
2. The lifetime pass recognizes `Set` / `ArrayPush` collection mutation and calls `require_collection_mutation_permitted`, so aliases are already rejected.
3. `rewrite_local_source_move_on_rebind` only rewrites a whole RHS shaped as `Expr::Var(source)` and explicitly skips `source == target`; it never rewrites the nested call’s first argument.
4. Codegen compiles that first `Expr::Var(target)` through `emit_copy_ldloc`.
5. The target local remains populated at `Arc::make_mut`, so the stack handle and local handle force a deep COW detach despite lifetime analysis having proved the mutation legal.

Correct existing-bytecode lowering:

```text
ldloc target       # temporary Arc-handle copy; target remains readable
<evaluate key>
<evaluate rhs>
ldc null
stloc target       # delayed move: stack handle now owns the collection
call set/3
stloc target       # rebind result
```

For `ArrayPush`, omit the key. No collection contents are copied. Delaying the clear until after argument evaluation preserves `a[i] = a[j]` and side-effecting key/RHS behavior.

## Constraints

- Do not modify `OpCode`, opcode values, assembler/disassembler syntax, VMBC, AOT IR, native bridge op tables, or no-std opcode tables.
- Do not add runtime call-pattern recognition or Arc-pointer matching.
- Do not add hidden locals or stack-reordering instructions.
- Do not alter parser evaluation order or lifetime alias policy.
- Activate only when local move semantics are enabled, the assignment destination equals the collection source, and the call is built-in `Set` or `ArrayPush` with exact arity.
- Keep compatibility-language/plugin compilation on the existing generic path when local move semantics are disabled.
- The transient `Null` is a runtime ownership transfer; do not change compiler `type_state` before the final rebind.

---

### Task 1: Add a RED bytecode-shape regression test

**Objective:** Prove current codegen leaves the target local populated at the collection builtin call.

**Files:**
- Modify: `tests/compiler/compiler_common_tests.rs`

**Step 1: Extend the test decoder to expose `Call` operands**

Update `DecodedInstr` / `decode_instructions` to record the existing call index and argc without changing production code.

**Step 2: Add `same_local_collection_set_clears_target_immediately_before_call`**

Compile:

```rust
let mut a = [10, 20];
a[0] = a[1];
a[0];
```

Resolve `a` through debug info and find `BuiltinFunction::Set.call_index()`. Assert the instruction window ending at the assignment call is:

```text
Ldc <Null>
Stloc <a>
Call <Set, argc=3>
Stloc <a>
```

Also assert an `Ldloc <a>` occurs before key/RHS evaluation, so the move remains delayed rather than clearing `a` before `a[1]` is read.

**Step 3: Run the RED test**

```bash
cargo test --locked --test compiler_tests \
  same_local_collection_set_clears_target_immediately_before_call -- --nocapture
```

Expected RED: current output contains `Call(Set)` followed by `Stloc(a)` but lacks the immediate `Ldc Null; Stloc(a)` pair before the call.

**Step 4: Commit the RED test only after confirming the expected failure**

Do not modify production code in this step.

---

### Task 2: Implement delayed-move lowering in codegen

**Objective:** Transfer the target local’s ownership immediately before the existing builtin call.

**Files:**
- Modify: `src/compiler/codegen.rs`
- Test: `tests/compiler/compiler_common_tests.rs`

**Step 1: Factor direct-call emission from argument compilation**

Split the current `compile_direct_call` responsibilities:

```rust
fn compile_direct_call(&mut self, index: u16, args: &[Expr]) -> Result<(), CompileError> {
    for arg in args {
        self.compile_scalar_expr(arg)?;
    }
    self.emit_direct_call(index, args)
}

fn emit_direct_call(&mut self, index: u16, args: &[Expr]) -> Result<(), CompileError> {
    let argc = u8::try_from(args.len()).map_err(|_| CompileError::CallArityOverflow)?;
    if let Some(builtin) = BuiltinFunction::from_call_index(index) {
        debug_assert!(builtin.accepts_arity(argc));
        self.record_builtin_call_operand_types(args);
        self.assembler.call(index, argc);
        return Ok(());
    }
    let remapped_index = self.call_index_remap.get(&index).copied().unwrap_or(index);
    self.assembler.call(remapped_index, argc);
    Ok(())
}
```

Keep all existing call-index remapping and operand-type recording behavior.

**Step 2: Add exact same-local collection matching**

Add a codegen helper:

```rust
fn try_compile_same_local_collection_rebind(
    &mut self,
    target: LocalSlot,
    expr: &Expr,
) -> Result<bool, CompileError>
```

Return `false` unless all conditions hold:

- `self.enable_local_move_semantics`;
- `expr` is `Expr::Call(index, _, args)`;
- builtin is `Set` with three args or `ArrayPush` with two args;
- `args[0]` is exactly `Expr::Var(target)`.

Matching exact parser IR is intentional. Do not broaden to borrow wrappers or unrelated host calls.

**Step 3: Emit delayed ownership transfer**

For a match:

```rust
for arg in args {
    self.compile_scalar_expr(arg)?;
}
self.assembler.push_const(Value::Null);
self.emit_stloc(target)?;
self.emit_direct_call(*index, args)?;
Ok(true)
```

This order keeps `target` available throughout key/RHS evaluation. Do not call `emit_move_ldloc`, since it clears immediately after loading the first argument.

**Step 4: Integrate before the generic RHS path**

In `assign_expr_to_slot`, replace the unconditional expression compilation with:

```rust
if !self.try_compile_same_local_collection_rebind(slot, expr)? {
    self.compile_scalar_expr(expr)?;
}
self.emit_stloc(slot)?;
```

Keep all callable/type inference and final `type_state` updates unchanged. The existing final `emit_stloc(slot)` performs the result rebind.

**Step 5: Run GREEN tests**

```bash
cargo test --locked --test compiler_tests \
  same_local_collection_set_clears_target_immediately_before_call -- --nocapture
cargo test --locked --test compiler_tests rustscript_local_move -- --nocapture
```

Expected: delayed-clear shape passes; existing local-move tests remain green.

**Step 6: Commit**

```bash
git add src/compiler/codegen.rs tests/compiler/compiler_common_tests.rs
git commit -m "perf: lower collection rebinds as delayed moves"
```

---

### Task 3: Cover self-reference, aliases, array push, and compatibility mode

**Objective:** Lock down language semantics and fallback boundaries.

**Files:**
- Modify: `tests/compiler/compiler_common_tests.rs`
- Modify: `tests/compiler/compiler_rustscript_tests.rs`
- Modify only if a failing test exposes an implementation defect: `src/compiler/codegen.rs`

**Step 1: Add self-reference behavior cases**

Add runtime cases for:

```rust
let mut a = [10, 20];
a[0] = a[1];
a[0];
```

and a key/RHS expression that records evaluation order through ordinary RustScript functions/counters. Assert key evaluates before RHS and both can read `a` before the delayed move.

**Step 2: Add map and append cases**

Cover:

- map field overwrite and null deletion;
- array `index == len` append through `Set`;
- explicit same-local `a = array_push(a, value)` if accepted by current frontend.

Assert each same-local call has `Ldc Null; Stloc target` immediately before `Call`, followed by the normal result `Stloc target`.

**Step 3: Preserve alias rejection**

Retain and narrow-run the existing cases where `let b = a; a[0] = ...` or mutable-borrow aliases cause compile errors. The delayed move relies on the lifetime pass’s existing `require_collection_mutation_permitted` proof.

```bash
cargo test --locked --test compiler_tests collection_alias -- --nocapture
```

**Step 4: Verify fallback boundaries**

Add tests proving no delayed clear is emitted when:

- move semantics are disabled by a compatibility frontend;
- the assignment destination differs from the collection source;
- the call is not built-in `Set` / `ArrayPush`;
- arity or IR shape differs.

**Step 5: Run targeted suite and commit**

```bash
cargo test --locked --test compiler_tests collection -- --nocapture
cargo test --locked --test compiler_tests indexed_assignment -- --nocapture
```

```bash
git add tests/compiler/compiler_common_tests.rs \
  tests/compiler/compiler_rustscript_tests.rs src/compiler/codegen.rs
git commit -m "test: cover delayed collection move semantics"
```

---

### Task 4: Verify engine compatibility and absence of deep COW lowering

**Objective:** Confirm the compiler-only change works on every execution path with unchanged bytecode ABI.

**Files:**
- Modify: `tests/vm/functional_parity_tests.rs`
- Modify: `examples/mini_bench.rs`
- Do not modify: VM opcode/wire/runtime files

**Step 1: Add interpreter/JIT parity**

Run repeated same-local array/map mutation with JIT disabled and enabled. Compare final stack and locals.

```bash
cargo test --locked --test vm_tests \
  interpreter_and_jit_match_for_delayed_collection_move -- --nocapture
```

**Step 2: Add a focused release workload**

Extend `examples/mini_bench.rs` with a collection-rebind loop using ordinary RustScript indexed assignment. Keep timing assertions out of CI.

Compare `fe93300` and the implementation commit with identical release settings and at least seven trials. Report medians separately for interpreter and JIT.

**Step 3: Verify unchanged ABI/runtime scope**

```bash
git diff --exit-code fe93300 -- \
  src/bytecode.rs src/assembler.rs src/vmbc.rs \
  src/vm pd-vm-nostd/src/program.rs pd-vm-nostd/src/vm.rs
```

Expected: no production diff in opcode, VMBC, interpreter, JIT/native bridge, or no-std runtime files.

**Step 4: Run full verification**

```bash
cargo fmt --all -- --check
cargo test --locked --lib
cargo test --locked --test compiler_tests
cargo test --locked --test vm_tests
cargo test --locked --test jit_tests
cargo test --locked -p pd-vm-nostd
cargo test --locked --all-targets
cargo clippy --locked --all-targets --all-features -- -D warnings
```

**Step 5: Commit benchmark/parity tests**

```bash
git add tests/vm/functional_parity_tests.rs examples/mini_bench.rs
git commit -m "bench: measure delayed collection moves"
```

---

## Acceptance criteria

- Same-local `Set` / `ArrayPush` uses only existing opcodes.
- Target remains readable during key/RHS evaluation.
- `Ldc Null; Stloc target` occurs immediately before the builtin call.
- At `Arc::make_mut`, the target local no longer retains the stack collection handle; alias-free source therefore avoids deep COW detachment.
- The ordinary post-call `Stloc target` restores the updated collection.
- Existing alias rejection remains the ownership proof.
- Compatibility frontends with move semantics disabled retain their current lowering.
- Parser, lifetime model, runtime dispatch, opcode tables, VMBC, AOT/native lowering, and no-std runtime remain unchanged.
