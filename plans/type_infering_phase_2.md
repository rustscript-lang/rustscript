# Reducing the Remaining ~10% Dynamic Dispatch

Four improvements to the type inference pass in [opt.rs](pd-vm/src/compiler/opt.rs), ordered by impact.

---

## Part 1: Loop Variable Type Stability (Fixpoint Analysis)

**Problem**: After `For` and `While` loops, `state.clear()` wipes **all** type knowledge. A simple counter loop like `for (let i = 0; i < n; i = i + 1)` makes `i` and every other variable `Unknown` for all subsequent code.

**Approach**: Replace the nuclear `state.clear()` with a **two-pass fixpoint iteration**: run the loop body twice, and if the type state converges (identical before and after the second pass), keep it. Only slots that were actually reassigned to a different type become `Unknown`.

#### [MODIFY] [opt.rs](pd-vm/src/compiler/opt.rs)

In [legalize_stmts](pd-vm/src/compiler/opt.rs#526-587), [collect_stmt_types](pd-vm/src/compiler/opt.rs#1093-1174), and [validate_stmts](pd-vm/src/compiler/opt.rs#588-664) — the three places that handle `For`/`While`:

```rust
// Before:
Stmt::For { init, condition, post, body, .. } => {
    // ... process init, condition, body, post ...
    state.clear();  // ← nukes everything
}

// After:
Stmt::For { init, condition, post, body, .. } => {
    legalize_stmts(std::slice::from_mut(init), state, context);
    let pre_loop = state.clone();
    let _ = legalize_expr(condition, state, context);
    legalize_stmts(body, state, context);
    legalize_stmts(std::slice::from_mut(post), state, context);
    // Second pass: run loop body again to detect type instability
    let _ = legalize_expr(condition, state, context);
    legalize_stmts(body, state, context);
    legalize_stmts(std::slice::from_mut(post), state, context);
    // Merge: keep only types that are stable across iterations
    state.merge_from_branches(&pre_loop, state);
}
```

The key insight: [merge_from_branches](pd-vm/src/compiler/opt.rs#115-139) already does the right thing — if a slot has the same type in both states, it keeps it; otherwise it becomes `Unknown`. After two iterations, a stable loop counter stays `Int`.

`While` gets the same treatment but without [init](pd-vm/src/compiler/mod.rs#2278-2292)/`post`.

> [!IMPORTANT]
> This changes semantics: code after a loop will now have type info for variables that were type-stable through the loop. Validate carefully that the merge logic doesn't produce false positives (claiming a type that could actually be wrong at runtime).

**Verification**: Add tests for:
- [for](pd-vm/src/vm/mod.rs#843-860) loop with stable `Int` counter → type preserved after loop
- `while` loop with stable `Float` accumulator → type preserved
- Loop that reassigns a variable from `Int` to `String` → falls to `Unknown`
- Nested loops → outer loop type stability still works

---

## Part 2: More Builtin Return Types

**Problem**: Several builtins return `Unknown` in [infer_call_like_expr_type](pd-vm/src/compiler/opt.rs#246-282) even though their return types are statically known.

#### [MODIFY] [opt.rs](pd-vm/src/compiler/opt.rs)

Expand the `match builtin` in `TypeContext::infer_call_like_expr_type`:

```rust
match builtin {
    // Already covered:
    // ArrayNew → Array, MapNew → Map, Len/Count → Int,
    // FormatTemplate/ToString/TypeOf → String, ArrayPush → Array, Set → infer

    // Add these:
    BuiltinFunction::Concat => BoundType::String,
    BuiltinFunction::Keys => BoundType::Array,
    BuiltinFunction::Slice if args.len() == 3 => {
        // slice preserves the container type
        self.infer_expr_type(&args[0], state)
    }
    BuiltinFunction::Assert => BoundType::Null,
    // Namespace builtins (from the generated catalog):
    // HasKey → Bool, Contains → Bool, StartsWith/EndsWith → Bool
    // Split → Array, Replace → String, Trim/Upper/Lower → String
    // ParseInt → Int, ParseFloat → Float
    // JsonEncode → String, JsonDecode → Unknown (genuinely unknown)
    // etc. — extend as each namespace's return types are known
    _ => BoundType::Unknown,
}
```

> [!TIP]
> The namespace builtins are generated from [build.rs](pd-vm/build.rs). You could add a [return_type](pd-vm/tests/compiler/type_inference_tests.rs#174-234) field to `NamespaceMemberDecl` so the build script generates return type metadata automatically. But for now, manually expanding the match arms is simpler and lower risk.

**Verification**: Add tests asserting `typeof(concat("a", "b"))` is inferred as `String` at compile time (constant-folded by existing `TypeOf` folding), and that `keys(m)` infers as `Array`, etc.

---

## Part 3: Host Function Return Type Signatures

**Problem**: External host functions (non-builtin `Call`) return `Unknown` because [HostImport](pd-vm/src/bytecode.rs#114-118) only has [name](pd-vm/src/compiler/opt.rs#22-34) and [arity](pd-vm/build.rs#413-422).

#### [MODIFY] [bytecode.rs](pd-vm/src/bytecode.rs)

```rust
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostImport {
    pub name: String,
    pub arity: u8,
    pub return_type: ValueType,  // new field, defaults to Unknown
}
```

#### [MODIFY] [opt.rs](pd-vm/src/compiler/opt.rs)

In `TypeContext::infer_call_like_expr_type`, for non-builtin `Expr::Call`:

```rust
Expr::Call(index, args) => {
    if let Some(builtin) = BuiltinFunction::from_call_index(*index) {
        // ... existing builtin handling ...
    } else {
        // Check if we have function impl (user-defined)
        let ret = self.infer_function_return(*index, args, state);
        if ret != BoundType::Unknown { return ret; }
        // Fall back to host import return type metadata
        // (requires threading host import info into TypeContext)
        BoundType::Unknown
    }
}
```

> [!NOTE]
> This requires the [TypeContext](pd-vm/src/compiler/opt.rs#154-158) to have access to the host import table during type inference. Thread `imports: &[HostImport]` into [TypeContext](pd-vm/src/compiler/opt.rs#154-158), or pass the remapped host import return types as a `HashMap<u16, ValueType>` alongside the [FrontendIr](pd-vm/src/compiler/ir.rs#171-178).

#### Impact on pd-edge

The pd-edge host function registration (`register_host_fn`) would need to accept an optional return type:

```rust
// Before:
runtime.register_host_fn("ngx_var", 1, |args| { ... });

// After:
runtime.register_host_fn_typed("ngx_var", 1, ValueType::String, |args| { ... });
```

This is a **public API extension** (additive, non-breaking) — the existing `register_host_fn` would default to `ValueType::Unknown`.

**Verification**: Register a host function with `return_type: ValueType::Int`, call it in a script, and verify the result variable has inferred type `Int` in the [TypeMap](pd-vm/src/bytecode.rs#33-37).

---

## Part 4: `Get` Return Type from Homogeneous Containers

**Problem**: [get(array, index)](pd-vm/src/compiler/opt.rs#64-70) and [get(map, key)](pd-vm/src/compiler/opt.rs#64-70) always return `Unknown`, even when the container only holds values of one type.

**Approach**: Extend `BoundType::Array` and `BoundType::Map` with optional element type tracking:

#### [MODIFY] [opt.rs](pd-vm/src/compiler/opt.rs)

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BoundType {
    Unknown,
    Null,
    Int,
    Float,
    Bool,
    String,
    Array,              // element type unknown (heterogeneous)
    ArrayOf(SimpleType), // homogeneous array where every element is this type
    Map,
    MapOf(SimpleType),   // homogeneous map values
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SimpleType { Int, Float, Bool, String, Null }
```

Then in [infer_call_like_expr_type](pd-vm/src/compiler/opt.rs#246-282):
- `ArrayPush(arr, elem)`: if [arr](pd-vm/src/bytecode.rs#43-46) is `ArrayOf(T)` and `elem` is `T`, result is `ArrayOf(T)`; otherwise `Array`
- `Get(container, key)`: if container is `ArrayOf(T)` or `MapOf(T)`, return `T`; else `Unknown`
- `ArrayNew`: becomes `ArrayOf(Unknown)` (empty, compatible with any element type on first push)

> [!WARNING]
> This is the most complex change. The `BoundType` enum grows, [merge_from_branches](pd-vm/src/compiler/opt.rs#115-139) needs to handle `ArrayOf` merges (same element type → keep; different → demote to `Array`), and all downstream conversions (`BoundType → ValueType`) need updating. Consider deferring this until after Parts 1-3 show measurable wins.

**Verification**: Test that `let a = array_new(); a = array_push(a, 1); a = array_push(a, 2); get(a, 0) + 3` produces typed `Add(Int, Int)`.

---

## Summary & Priority

| Part | Effort | Impact | Risk |
|---|---|---|---|
| 1. Loop fixpoint | Medium | **High** — loops are the hot path | Low — falls back safely |
| 2. Builtin return types | Low | Medium — covers common stdlib | Very low — pure addition |
| 3. Host function return types | Medium | Medium — depends on pd-edge adoption | Low — additive API |
| 4. Homogeneous container tracking | High | Medium — helps [get()](pd-vm/src/compiler/opt.rs#64-70) chains | Medium — enum change ripples |

Recommended order: **1 → 2 → 3 → 4**. Parts 1+2 can likely be done together in one commit.
