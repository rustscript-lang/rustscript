# Optional Chain Typing Plan

This plan captures the intended optional-chain behavior for `pd-vm` after the current compiler changes.

## Goal

Values produced by `?.` or `?.[key]` must not degrade to `unknown`. The compiler and the wasm lint/analyzer must keep tracking the concrete inner type and schema until the user handles the optional result.

## Shipped Surface

RustScript:

- `?.` and `?.[key]` require a user-declared schema on the container.
- The result of optional chaining is optional, not a plain value.
- Supported handling forms are:
  - `.unwrap_or(fallback)`
  - control-flow refinement with `!= null`
  - `match value { None => ..., Some(name) => ..., _ => ... }`

Examples:

```rss
struct Stats { score: int }
struct Profile { stats: Stats }

let profile: Profile = { stats: { score: 41 } };
let score = profile?.stats?.score.unwrap_or(0);
score + 1;
```

```rss
let score = profile?.stats?.score;
if score != null {
    score + 1;
} else {
    0;
}
```

```rss
match profile?.stats?.score {
    None => 0,
    Some(score) => score + 1,
    _ => 0,
}
```

Other frontends keep shared optional-chain behavior and currently handle optional results with null checks.

## Type-System Contract

- `a?.b` stores the inner type/schema of `b` and marks the result as optional.
- Optionality is tracked separately from `BoundType::Unknown`.
- Binding an optional result into a local preserves:
  - inner concrete type
  - inner schema
  - optional flag
- `.unwrap_or(fallback)` clears the optional flag and yields a concrete value type.
- `if value != null` clears the optional flag for that value inside the non-null branch.
- `Some(name)` binds the inner concrete `T` for that arm while `None` handles the null-backed case.
- After the optional flag is cleared, the value must type-check exactly like plain inner `T`.

Required invariant:

- `.unwrap_or(...)` produces concrete `T` for compiler validation
- `.unwrap_or(...)` produces concrete `T` in emitted operand metadata
- `!= null` refinement produces concrete `T` inside the refined branch
- `Some(name)` match binding produces concrete `T` for compiler validation, emitted metadata, and lint
- wasm lint/analyzer sees the same concrete `T`
- none of those paths may fall back to `unknown` only because the value originated from `?.`

## IR And Lowering

- Keep optional access explicit in IR until typing and validation finish.
- Current IR nodes:
  - `Expr::OptionalGet`
  - `Expr::OptionUnwrapOr`
- Do not eagerly lower `?.` into nested `if` trees before typing.
- After validation, lower optional handling back into the existing `null` runtime behavior.
- Do not introduce a runtime `Option` value in this phase.

## Validation Rules

Reject optional results when they are used as though they were already plain values, including:

- arithmetic and logical operators
- normal member/index access after an optional result
- function arguments that require plain `T`
- other concrete-value contexts

Key diagnostics:

```text
optional value must be unwrapped before binary operation
optional value must be unwrapped before member access
optional access requires a user-declared schema in RustScript
Some(...) and None match patterns require an optional value
```

## Work Areas

1. Parser and IR
   - keep `?.` explicit
   - parse `.unwrap_or(...)`
   - reject `.unwrap()`
2. Typing and validation
   - track optionality separately from concrete type/schema
   - refine `!= null`
   - add `None` / `Some(name)` optional match handling
   - preserve concrete typing across compiler and lint
3. Codegen and lowering
   - compile `OptionalGet`
   - compile `OptionUnwrapOr`
   - preserve current `null`-based runtime behavior
4. Docs and examples
   - document RustScript schema-gated optional chaining
   - show `.unwrap_or(...)`, null-check refinement, and `None` / `Some(name)` matches
5. Tests
   - runtime success for `.unwrap_or(...)`
   - RustScript rejection without declared schema
   - rejection for using optional values without handling
   - inference tests for `.unwrap_or(...)`, `!= null`, and `Some(name)` matches
   - wasm lint tests proving concrete types survive handling
