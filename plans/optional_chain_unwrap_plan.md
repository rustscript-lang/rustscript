# Rust-Like Optional Chain Unwrap Plan

This plan covers changing optional chaining so values produced by `?.` are no longer treated like plain values. Anything that comes out of an optional chain must be explicitly unwrapped before normal use.

## Goal

Move optional chaining from today's "null-producing expression that often degrades to `unknown`" model to a Rust-like `Option<T>` model at the type layer.

Recommended user-facing direction for RustScript:

```rss
let score_opt = profile?.stats?.score;

let Some(score) = score_opt else {
    return None;
};

Some(score + 1)
```

Also support:

```rss
let score = profile?.stats?.score?;
let score = profile?.stats?.score.unwrap();
let score = profile?.stats?.score.unwrap_or(0);
```

## Recommendation

Use a staged design:

1. Treat `?.` as producing a compiler-tracked optional result instead of `unknown`.
2. Keep optional access explicit in IR until typing/validation is finished.
3. Keep runtime representation incremental in the first pass:
   `None` lowers to `null`, `Some(v)` lowers to `v`.
4. Add Rust-like unwrap syntax in RustScript first:
   - postfix `?` for propagate-on-None
   - `if let Some(x) = expr { ... }`
   - `let Some(x) = expr else { ... };`
   - `match expr { Some(x) => ..., None => ... }`
   - `.unwrap()`,`.unwrap_or(default)`
5. Update the other frontends only after the shared typing rules settle.

This avoids introducing a new runtime `Value::Option` in the first implementation while still giving RustScript a coherent `Option` surface.

## Why This Shape

The current parser lowers optional access immediately into null-guarded expressions in [pd-vm/src/compiler/parser/expressions.rs](pd-vm/src/compiler/parser/expressions.rs). That makes the typer in [pd-vm/src/compiler/typing/context.rs](pd-vm/src/compiler/typing/context.rs) and [pd-vm/src/compiler/typing/validate.rs](pd-vm/src/compiler/typing/validate.rs) reconstruct intent from generated `IfElse` trees, which is exactly why the result often collapses to `unknown`.

The cleaner design is:

- parser keeps `?.` visible in IR
- typer tracks "optional result" directly
- validator rejects use sites that require an unwrap
- lowering/codegen converts validated optional nodes back into the existing null-based runtime behavior

## Proposed Semantics

### Core Rule

Any expression produced by `?.` or `?.[key]` is an optional result.

Allowed on an optional result:

- another optional access: `user?.profile?.name`
- explicit unwrap: `expr?`, `expr.unwrap()`
- defaulting: `expr.unwrap_or(fallback)`
- control-flow refinement:
  - `if let Some(x) = expr`
  - `let Some(x) = expr else`
  - `match expr { Some(x) => ..., None => ... }`
  - `if expr != None` or `if expr != null` only if the temporary lowering model keeps `None == null`

Rejected on an optional result until unwrapped:

- normal member access: `user?.profile.name`
- normal index access: `user?.items[0]`
- direct calls
- arithmetic / logical operators
- passing to a non-optional parameter
- assigning into a local that is then treated as plain `T`

### RustScript Surface Syntax

Recommended first-class syntax:

```rss
let score = profile?.stats?.score?;
let Some(score) = profile?.stats?.score else {
    return None;
};

if let Some(score) = profile?.stats?.score {
    print(score);
}

match profile?.stats?.score {
    Some(score) => score + 1,
    None => 0,
}
```

### Runtime Representation

Phase 1 should not add a new runtime enum value.

Instead:

- `None` lowers to `null`
- `Some(expr)` lowers to `expr`
- the `Option<T>` behavior exists in the parser/type-checker/validator/lowering pipeline

That keeps the VM, bytecode, and host ABI stable while the language semantics shift.

## Implementation Phases

## Phase 1: Preserve Optional Access In IR

Add explicit IR nodes in [pd-vm/src/compiler/ir.rs](pd-vm/src/compiler/ir.rs) instead of lowering `?.` away in [pd-vm/src/compiler/parser/expressions.rs](pd-vm/src/compiler/parser/expressions.rs).

Candidate additions:

```rust
Expr::OptionalGet { container: Box<Expr>, key: Box<Expr> }
Expr::OptionalMember { container: Box<Expr>, member: String }
Expr::OptionSome(Box<Expr>)
Expr::OptionNone
Expr::OptionUnwrap(Box<Expr>)
Expr::OptionUnwrapOr { value: Box<Expr>, fallback: Box<Expr> }
```

Minimum requirement for this phase:

- keep `?.` and `?.[key]` explicit
- do not lower them to nested `IfElse` before typing

## Phase 2: Add Optional Result Tracking To Typing

Extend the typing model in [pd-vm/src/compiler/typing/state.rs](pd-vm/src/compiler/typing/state.rs) and shared inference in [pd-vm/src/compiler/typing/context.rs](pd-vm/src/compiler/typing/context.rs).

Recommended representation:

```rust
struct TypedValue {
    bound: BoundType,
    schema: Option<TypeSchema>,
    optional: bool,
}
```

Do not overload `BoundType::Unknown` to mean "optional".

Typing rules:

- `a?.b` -> `TypedValue { bound: T, optional: true }`
- `a?.b?.c` stays optional across the chain
- plain `a.b` requires `optional == false`
- unwrapping clears `optional` and preserves the concrete inner type/schema
- `None` and `Some` constructs participate in branch unification

Required invariant:

- after any successful unwrap path, the resulting value must be typed as plain `T`
- it must not fall back to `unknown` merely because it originated from `?.`
- the same unwrapped type must be visible to:
  - compile-time validation in the main compiler pipeline
  - emitted type metadata used by the compiler/runtime type map
  - wasm lint/analyzer diagnostics used by the playground/editor

Schema propagation must keep working for cases already covered by:

- [pd-vm/tests/compiler/compiler_rustscript_tests.rs](pd-vm/tests/compiler/compiler_rustscript_tests.rs)

## Phase 3: Validation Rules

Add dedicated validation for "optional result used without unwrap" in [pd-vm/src/compiler/typing/validate.rs](pd-vm/src/compiler/typing/validate.rs).

Required diagnostics:

- member/index access after optional chain without unwrap
- arithmetic on optional result
- function call with optional argument where `T` is required
- direct assignment into plain schema-typed locals if the value is still optional

Suggested diagnostic shape:

```text
optional value must be unwrapped before member access
optional value must be unwrapped before arithmetic
optional value must be handled with ?, if let, let-else, match, unwrap, or unwrap_or
```

Also extend flow refinement so these clear the optional bit:

- `if let Some(x) = expr`
- `let Some(x) = expr else { ... }`
- `match expr { Some(x) => ..., None => ... }`

If Phase 1 keeps `None` lowered to `null`, then `expr != null` can also refine as a transitional rule.

This refinement must produce a real typed binding, not just suppress an error. For example, inside a successful refinement branch, `x` should type-check exactly like a normal non-optional value for member access, arithmetic, argument checking, and schema-based field validation.

## Phase 4: RustScript Syntax

Implement RustScript parser support for:

- postfix `?`
- `Some(expr)` and `None`
- `if let Some(x) = expr`
- `let Some(x) = expr else { ... };`
- `match` arms that bind `Some(name)` and `None`
- optional methods:
  - `.unwrap()`
  - `.unwrap_or(default)`

Primary files:

- [pd-vm/src/compiler/parser/expressions.rs](pd-vm/src/compiler/parser/expressions.rs)
- [pd-vm/src/compiler/parser/statements.rs](pd-vm/src/compiler/parser/statements.rs)

This phase should also update RustScript syntax highlighting:

- [pd-vm/webui/src/monaco/rustscriptLanguage.ts](pd-vm/webui/src/monaco/rustscriptLanguage.ts)
- [pd-controller/webui/src/app/monaco/rustscriptLanguage.ts](pd-controller/webui/src/app/monaco/rustscriptLanguage.ts)

## Phase 5: Lowering And Codegen

After validation succeeds, lower optional nodes back into the existing null-based behavior so runtime semantics stay compatible.

Primary files:

- [pd-vm/src/compiler/typing/helpers.rs](pd-vm/src/compiler/typing/helpers.rs)
- [pd-vm/src/compiler/codegen.rs](pd-vm/src/compiler/codegen.rs)

Goals:

- no runtime `Option` value in Phase 1
- preserve current execution behavior for successful programs
- keep host/runtime interfaces unchanged

## Phase 6: Cross-Frontend Policy

Decide whether JavaScript/Lua/Scheme receive:

- only the stricter shared typing semantics, with null-check-based handling
- or frontend-native sugar later

Recommendation:

- keep the semantic change shared
- ship RustScript syntax first
- defer JS/Lua/Scheme unwrap sugar to a follow-up plan

That keeps scope under control and avoids designing three more surface syntaxes during the core type-system rewrite.

## Test Plan

## Compiler Tests

Add/extend tests in:

- [pd-vm/tests/compiler/compiler_rustscript_tests.rs](pd-vm/tests/compiler/compiler_rustscript_tests.rs)
- [pd-vm/tests/compiler/compiler_javascript_tests.rs](pd-vm/tests/compiler/compiler_javascript_tests.rs)
- [pd-vm/tests/compiler/compiler_lua_tests.rs](pd-vm/tests/compiler/compiler_lua_tests.rs)
- [pd-vm/tests/compiler/compiler_scheme_tests.rs](pd-vm/tests/compiler/compiler_scheme_tests.rs)

Required RustScript coverage:

- `user?.profile.name` is rejected
- `user?.profile?.name` is accepted
- assigning `let name = user?.profile?.name; name.len();` is rejected until unwrap
- `let Some(name) = user?.profile?.name else { ... };` is accepted
- `if let Some(name) = user?.profile?.name { ... }` refines inside the branch
- `match user?.profile?.name { Some(name) => ..., None => ... }` refines correctly
- postfix `?` propagates in functions returning optional results
- `.unwrap()`, and `.unwrap_or(...)` behave correctly
- optional results still preserve schema for nested field access after unwrap
- after unwrap, arithmetic and function argument validation use the concrete inner type rather than `unknown`
- after unwrap, schema-backed member access keeps the resolved field type rather than widening

Cross-frontend baseline coverage:

- chained `?.` still works
- using the optional result as a plain value without handling is rejected with a clear error

## Runtime / Example Tests

Update and extend:

- [pd-vm/tests/example_tests.rs](pd-vm/tests/example_tests.rs)

Required coverage:

- examples still execute after the syntax update
- RustScript example demonstrating explicit optional handling compiles and runs
- optional-chain examples reflect unwrap-required behavior rather than raw null passthrough only

## Lint / Analyzer Tests

Update and extend lint-facing coverage so the editor/playground sees the same post-unwrap types as the compiler.

Primary files:

- [pd-vm/pd-vm-wasm/src/analyzer.rs](pd-vm/pd-vm-wasm/src/analyzer.rs)
- [pd-vm/pd-vm-wasm/src/lib.rs](pd-vm/pd-vm-wasm/src/lib.rs)
- [pd-vm/tests/compiler/diagnostics_tests.rs](pd-vm/tests/compiler/diagnostics_tests.rs)

Required coverage:

- lint reports an error for using an optional result without unwrap
- lint does not report spurious `unknown`-type warnings after a successful unwrap/refinement
- lint field-access diagnostics after unwrap use the concrete schema-backed type
- compiler diagnostics and wasm lint diagnostics agree on the same examples

## Regression Coverage

Keep existing schema-related optional-chain tests green, especially around:

- referenced struct schemas through optional chains
- recursive struct schemas after optional-chain assignment

Those are currently covered in [pd-vm/tests/compiler/compiler_rustscript_tests.rs](pd-vm/tests/compiler/compiler_rustscript_tests.rs).

## Example And Playground Updates

## example_complex.rss

Update:

- [pd-vm/examples/example_complex.rss](pd-vm/examples/example_complex.rss)

Replace the current:

```rss
let stats = profile?.stats;
let chained_score = stats?.score;
```

with an example that demonstrates explicit handling, preferably:

- `let Some(stats) = profile?.stats else { ... };`
- `let chained_score = stats.score;`

or:

- `let chained_score = profile?.stats?.score.unwrap_or(0);`

The example should show the new feature clearly without making the sample overly dense.

## Playground Sample

Update the bundled RustScript sample in:

- [pd-vm/webui/src/playgroundConfig.ts](pd-vm/webui/src/playgroundConfig.ts)

The playground sample should:

- demonstrate an optional chain that now requires handling
- use one idiomatic Rust-like unwrap path
- stay short enough to remain readable in the editor

Keep the playground sample and [pd-vm/examples/example_complex.rss](pd-vm/examples/example_complex.rss) aligned at the feature-story level even if they are not identical line-for-line.

## Documentation Updates

Update:

- [pd-vm/README.md](pd-vm/README.md)
- [pd-vm/src/compiler/frontends/README.md](pd-vm/src/compiler/frontends/README.md)
- [pd-vm/webui/about.html](pd-vm/webui/about.html)

Required doc changes:

- optional chaining no longer produces a plain value
- RustScript now treats `?.` as producing an `Option`-like result
- document the supported unwrap/handling syntax
- update match-pattern documentation if `Some(name)` / `None` binding patterns are added
- update examples so docs do not show stale `?.` behavior

Also check any playground/help text that mentions optional chaining as plain collection access.

## Suggested Rollout Order

1. Preserve optional nodes in IR.
2. Add optional-result tracking in typing.
3. Make unwrapped/refined bindings produce concrete non-optional types in compiler inference and emitted metadata.
4. Add validator errors for missing unwrap.
5. Add RustScript surface syntax for handling/unwrapping.
6. Lower validated option constructs to current runtime null behavior.
7. Update compiler tests and lint/analyzer tests together.
8. Update docs.
9. Update [pd-vm/examples/example_complex.rss](pd-vm/examples/example_complex.rss).
10. Update [pd-vm/webui/src/playgroundConfig.ts](pd-vm/webui/src/playgroundConfig.ts).

## Open Questions

These should be decided before implementation starts:

1. Is `Option<T>` only a RustScript surface concept in Phase 1, or must JS/Lua/Scheme also gain explicit unwrap syntax immediately?
2. Should postfix `?` be limited to function bodies that return optional results, or also be allowed in block expressions that lower to optional?
3. Do we want `.unwrap()` methods in Phase 1, or only `?`, `if let`, `let-else`, and `match`?
4. Should `None` remain interchangeable with `null` in user code during the transition, or should RustScript docs move fully to `Some`/`None` terminology?
5. Should `unwrap_or` be syntax (`??`) or method form (`.unwrap_or(...)`)? For a Rust-like design, method form is the better default.

## Recommended Answer To The Open Questions

1. RustScript gets the new surface syntax first. Shared typing semantics apply across all frontends.
2. Postfix `?` should follow Rust-style propagation rules rather than becoming a generic local unwrap.
3. Ship `.unwrap()` and `.unwrap_or(...)` together with `if let` and `let-else`; they cover common local handling cases.
4. Keep `null` as the runtime lowering target in Phase 1, but document RustScript in `Option` terms.
5. Prefer method form over `??` for this plan because the goal is a more Rust-like surface.
