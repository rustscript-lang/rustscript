# Analysis of Type Inference & Lifetime (Borrow/Liveness/Drop) Systems

Based on a thorough review of the current `pd-vm` compiler analysis passes (`opt.rs`, `liveness.rs`, `availability.rs`) and test coverage, Phase 1 and Phase 2 of the type inference plan are largely implemented. We have loop fixpoint analysis, dense host/builtin return type signatures, and homogeneous container `[T]` tracking. Liveness mapping and drop contract tests demonstrate rigorous memory auto-nulling and drop correctness.

However, several notable gaps and future improvement opportunities remain across the type checker and lifetime state machines.

## 1. Type Inference Gaps (`opt.rs`)

### Gap 1.1: Lack of Flow-Sensitive Type Narrowing
**Current State:**
The inference engine computes `if/else` branch types but does **not** narrow types based on conditional checks. If you write `if x != null { ... }`, the type state for `x` inside the `then` branch is exactly the same as before the branch.
**Improvement Plan:**
- Introduce restricted condition-based narrowing in `TypeContext::apply_stmts` or `legalize_stmts`.
- E.g., match `Expr::NotEq(x, Expr::Null)` to update `state.set(x, NonNullType)` within the `then_branch`.
- This would significantly reduce the need for `Unknown` fallback when mixed variant types start appearing.

### Gap 1.2: Recursive Function Return Type Fallback
**Current State:**
In `TypeContext::infer_function_return`, if `self.active_functions.contains(&index)`, the compiler breaks the cycle by immediately returning `BoundType::Unknown` (lines 668-670).
**Improvement Plan:**
- Implement a fixpoint iteration for recursive functions similar to loops (`try_stabilize_loop_state`).
- Seed recursive calls with `BoundType::Unknown` initially, infer the body, and run iteratively until the inferred return type converges.

### Gap 1.3: Implicit Closure Signatures
**Current State:**
Closures do not bind parameter types structurally. Their return type is inferred at individual call sites mapping given arguments to the closure body via `infer_closure_return`.
**Improvement Plan:**
- We support higher-order functions now (e.g., passing functions as parameters). The `TypeMap` could record `ValueType::Callable(args, ret)` instead of falling back dynamically.

### Gap 1.4: Limited Object/Map Field Type Tracking
**Current State:**
`BoundType::MapOf(T)` assumes uniform map values. However, `pd-vm` scripts frequently use structure-like maps `{"key1": 1, "key2": "str"}` (`MapOf(Unknown)`).
**Improvement Plan:**
- Introduce `BoundType::Object(HashMap<String, BoundType>)` or a small shape tracking mechanism for map literals with statically known string keys.
- This allows `field.access` (using `Expr::MoveField` or `Get`) to statically resolve to specific types (e.g. `Int` for `x.id` and `String` for `x.name`).

---

## 2. Lifetime & Borrowing Analysis Gaps (`availability.rs` & `liveness.rs`)

### Gap 2.1: Flow Allocation and the `suppress_clears` Loop Problem
**Current State:**
`LivenessRewriter::rewrite_block` computes clears with `suppress_clears: bool` for post and block iterations inside loops (`try_stabilize_loop_state`). This protects loop-carried values but can over-extend the lifetime of temporaries inside the loop condition or post expression until the entire loop terminates.
**Improvement Plan:**
- The condition/post expressions inside `For` and `While` should emit localized block scopes or strictly managed drops, instead of suppressing clears entirely and batching drops.
- This would result in slightly lower peak memory usage for complex loop predicates.

### Gap 2.2: Deep Collection Aliasing Escapes
**Current State:**
`FlowState::collection_aliases` tracks slots that alias the same memory (e.g., `let a = { }; let b = a;`). However, this is heavily restricted to direct slot-to-slot assignments. 
If an object is pushed into a container (`array_push(arr, obj)`) or returned from a function, the alias tracking is lost, falling back to dynamic reference counts or conservative moves in `availability.rs`.
**Improvement Plan:**
- Expand alias graph analysis to track container captures (`arr` contains alias of `obj`).
- Add borrow-check invariants that prevent mutating the container while the extracted element is borrowed.

### Gap 2.3: Redundant Drops on Move
**Current State:**
The AST includes `Stmt::Drop` for auto-nulling variables that are no longer live. For variables that have been explicitly `Moved` (tracked via `availability.rs` `moved_local_definite`), they are already essentially `Null`.
**Improvement Plan:**
- `LivenessRewriter` could consume the `FlowState::moved_definite` output from `availability.rs` to optimize away `Stmt::Drop` requests for variables strictly proven to be mathematically moved.

## 3. Recommended Next Steps (Implementation Phasing)

| Phase   | Area       | Feature                                | Justification                                |
|---------|------------|----------------------------------------|----------------------------------------------|
| **1**   | Type Infer | Shape / Record Types (`BoundType`)     | Unlocks static dispatch for struct-like maps |
| **1**   | Type Infer | Flow-Sensitive Null/Type Narrowing     | Massively cuts down on dynamic checking UI noise |
| **2**   | Liveness   | Elide Drops for compiler `Moved` slots | Free performance optimization in `liveness.rs` |
| **3**   | Type Infer | Recursive function type fixpoint       | Eliminates remaining `Unknown` in recursion  |
| **4**   | Borrowing  | Advanced Container Aliasing graph      | Difficult, but allows safer array mutability |

This plan focuses purely on analysis and IR generation logic and does not imply any wire format bumps or runtime changes.
