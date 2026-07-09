# Frontend-Only RSS Generics Plan

Add explicit generics to RustScript / RSS as a frontend-only feature, with analyzer support for:

- generic RSS functions
- generic RSS structs
- selected generic host functions such as `json::decode::<T>(...)`

## Goal

Support these flows:

```rss
use json;

struct MyStruct { name: string }
struct Box<T> { value: T }

fn myfn<T>(v: T) {
  let b = v;
  b
}

let v = myfn::<string>("hello");
let boxed: Box<string> = { value: "hello" };
let payload = json::decode::<MyStruct>(text);
```

Desired behavior:

- `fn a<T>()`, `fn a<T, U>()`, and `fn a<T>(value: T)` parse successfully
- `struct Box<T> { value: T }` and `Box<string>` parse successfully
- `T` and `U` are valid type names inside generic function bodies and generic RSS parameter schemas
- `let b = v` inside an instantiated generic function preserves the schema of `v`, so if `v: T` then `b` is also `T`
- `myfn::<string>("hello")` makes the result infer as `string`
- `Box<string>` preserves the instantiated field schema, so `.value` is known as `string`
- `json::decode::<MyStruct>(text)` makes the result infer as schema `MyStruct`
- generics stay compile-time only in the base version; bytecode and VM runtime remain unchanged

## Base Scope

The base rollout is still explicit-only.

Supported:

- explicit type params on RSS functions
- generic params in RSS parameter schema positions such as `v: T`
- explicit type params on RSS structs
- explicit type args on RSS function calls
- propagation of instantiated generic schemas through local `let` bindings inside generic RSS functions
- explicit type args in schema positions such as `Box<string>`
- explicit type args on supported host functions

Not in the base rollout:

- trait bounds, `where` clauses, or variance rules
- generic function values such as `let f = myfn::<string>;`
- runtime specialization or monomorphized bytecode
- runtime schema-aware host execution
- inference of missing type args from value arguments
- inference of missing type args from expected result context

Once a call site explicitly supplies type args, ordinary local inference inside that instantiated body is still in scope. For example, `let b = v` should preserve `T` when `v: T`.

## Follow-On Scope

The next step after the explicit-only base should add:

1. inference of type args from value arguments
2. inference of type args from expected result context

Examples for that follow-on work:

```rss
fn id<T>(value: T) {
  value
}

let s = id("hello");
```

```rss
fn make<T>() {
  let value: T = source();
  value
}

let s: string = make();
```

Those are not part of the base implementation, but they should be documented as the next planned phase.

## Current State

The current compiler already has most of the machinery needed for compile-time-only generics, but the information is spread across parser, IR, typing, and pipeline layers.

- The parser supports declared local schemas and named struct schemas in [src/compiler/parser/statements.rs](../src/compiler/parser/statements.rs), but every non-primitive type name is currently treated as either a struct schema reference or an error.
- Struct schemas are currently stored as `HashMap<String, TypeSchema>` in [src/compiler/ir.rs](../src/compiler/ir.rs), which is sufficient for concrete structs but not for `struct Box<T> { ... }` plus `Box<string>`.
- Function declarations only carry `name`, `arity`, `args`, `exported`, and coarse `return_type` in [src/compiler/ir.rs](../src/compiler/ir.rs), so parameter schema annotations such as `v: T` are not yet represented.
- Call expressions do not carry explicit type arguments in [src/compiler/ir.rs](../src/compiler/ir.rs).
- The analyzer already propagates call-site information into RSS function bodies in [src/compiler/typing/context.rs](../src/compiler/typing/context.rs), but its observation maps are keyed only by function index, so different generic instantiations would currently collapse together.
- Local inference is still primarily driven by coarse bound types, so `let b = v` inside a generic instantiation needs explicit schema propagation to preserve `b` as `T` instead of only a coarse runtime category.
- Host functions already expose coarse return types and parameter signatures through [src/compiler/parser/symbols.rs](../src/compiler/parser/symbols.rs), [src/compiler/typing/helpers.rs](../src/compiler/typing/helpers.rs), [src/builtins/metadata.rs](../src/builtins/metadata.rs), and [build.rs](../build.rs), but they do not expose generic metadata or return-schema templates.

## High-Level Design

### 1. Keep generics in the schema layer

This feature fits best as a schema-level extension, not as a new runtime type system.

The schema model needs at least:

```rust
TypeSchema::GenericParam(String)
```

Reusing `TypeSchema::Named("T")` is not sufficient because the current parser and validator treat named schemas as struct references.

Generic structs also require named type application in schema positions. The current `TypeSchema::Named(String)` form is too weak for:

```rss
Box<string>
Pair<string, int>
```

So the schema model should be extended to represent named-schema application, for example:

```rust
TypeSchema::Named {
    name: String,
    type_args: Vec<TypeSchema>,
}
```

or an equivalent dedicated node.

### 2. Add first-class generic struct metadata

The current `struct_schemas: HashMap<String, TypeSchema>` representation is not expressive enough once structs themselves become generic.

The IR should carry explicit struct declaration metadata, for example:

```rust
StructDecl {
    name: String,
    type_params: Vec<String>,
    body_schema: TypeSchema,
}
```

That gives the compiler somewhere to store:

- the generic parameter list for `struct Box<T>`
- the uninstantiated schema body
- the information needed to later instantiate `Box<string>`

### 3. Keep generics erased before bytecode

The base feature should not change:

- VM values
- bytecode encoding
- host import layout
- runtime function dispatch

Generics affect only:

- syntax and parsing
- type/schema validation
- call-site type inference
- lints, hover, and type hints

### 4. Use explicit instantiation, not global merging

The current analyzer merges observed function argument types per function index. That is not sufficient for generics:

- `id::<string>()`
- `id::<int>()`

must be analyzed as two distinct instantiations even though both target the same function index.

The generic feature therefore needs an instantiation key such as:

```rust
GenericInstanceKey {
    owner: GenericOwner,
    type_args: Vec<TypeSchema>,
}
```

where `GenericOwner` can represent at least:

- a generic RSS function
- a generic struct instantiation

All generic-aware inference and validation should be keyed by that instance, not only by function index or bare struct name.

### 5. Preserve generic schema identity across local bindings

Once a generic function is instantiated, local bindings derived from other locals or parameters must carry the same instantiated schema, not only the coarse `BoundType`.

This matters for flows such as:

```rss
fn myfn<T>(v: T) {
  let b = v;
  b
}
```

If `v` is schema `T`, then `b` must also be tracked as `T`. Otherwise return inference, diagnostics, and hover output will collapse too early.

## Implementation Phases

## Phase 1: Parser And IR

Files:

- [src/compiler/ir.rs](../src/compiler/ir.rs)
- [src/compiler/parser/mod.rs](../src/compiler/parser/mod.rs)
- [src/compiler/parser/cursor.rs](../src/compiler/parser/cursor.rs)
- [src/compiler/parser/statements.rs](../src/compiler/parser/statements.rs)
- [src/compiler/parser/expressions.rs](../src/compiler/parser/expressions.rs)
- [src/compiler/linker.rs](../src/compiler/linker.rs)

Changes:

1. Extend `FunctionDecl` with `type_params: Vec<String>` and parameter declaration metadata that can carry optional schemas such as `v: T`.
2. Add explicit generic struct declaration metadata in [src/compiler/ir.rs](../src/compiler/ir.rs).
3. Extend `Expr::Call` and `Expr::LocalCall` to carry `type_args: Vec<TypeSchema>`.
4. Parse optional generic params after the function name:

```rss
fn a<T>() {}
fn a<T, U>() {}
```

5. Parse optional declared schemas on RSS function params and allow generic params in those schema positions:

```rss
fn myfn<T>(v: T) {}
fn pair<T, U>(left: T, right: U) {}
```

6. Parse optional generic params after the struct name:

```rss
struct Box<T> { value: T }
struct Pair<T, U> { left: T, right: U }
```

7. Parse optional turbofish syntax immediately before call parentheses:

```rss
myfn::<string>("hello")
json::decode::<MyStruct>(text)
```

8. Parse named type application in schema positions:

```rss
let boxed: Box<string> = { value: "hello" };
let pair: Pair<string, int> = { left: "x", right: 1 };
```

9. Track active type-param names while parsing generic function bodies, generic function parameter schemas, and generic struct bodies so `T` is accepted as a schema reference inside those scopes.
10. Update schema-reference validation in the parser so active generic params are not rejected as unknown struct schemas.
11. Update linker merge logic in [src/compiler/linker.rs](../src/compiler/linker.rs) so it preserves and validates generic structs and generic functions when imported modules are merged.

Notes:

- No lexer change is required. The existing token stream already provides `::`, `<`, `>`, and identifiers separately.
- The base rollout should reject wrong generic-arg counts at parse or early validation time.

## Phase 2: Generic-Aware Schema Resolution

Files:

- [src/compiler/typing/context.rs](../src/compiler/typing/context.rs)
- [src/compiler/typing/helpers.rs](../src/compiler/typing/helpers.rs)
- [src/compiler/typing/state.rs](../src/compiler/typing/state.rs)

Changes:

1. Add a generic substitution environment to `TypeContext`, mapping generic params to concrete `TypeSchema` values.
2. Make `resolve_schema(...)` substitute `GenericParam(T)` before normal named-struct resolution.
3. Add generic struct instantiation so:
   - `Box<string>` resolves to the instantiated body `{ value: string }`
   - `Pair<string, int>` resolves to `{ left: string, right: int }`
4. Update `bound_type_from_schema(...)` so:
   - bound generic params resolve to the bound schema's coarse type
   - unbound generic params behave as opaque / unknown at the coarse `BoundType` level
5. Update schema mismatch logic in [src/compiler/typing/helpers.rs](../src/compiler/typing/helpers.rs) so:
   - `T` matches the instantiated concrete schema for that call
   - instantiated generic structs are compared as their resolved field schemas
   - unresolved generic params are treated as opaque placeholders rather than structs
6. Update field/index access inference so:
   - `T` does not expose fields unless it resolves to a concrete schema in the current instantiation
   - `Box<string>.value` resolves as `string`
7. Update local-binding schema propagation so:
   - `let b = v` copies the instantiated schema of `v`, not only its coarse bound type
   - when `v: T`, `b` is tracked as `T` and can be returned as `T`

Why this matters:

- The analyzer must preserve the fact that a slot is `T`, even when its coarse runtime category is only `string`, `map`, or `unknown`, and even when that slot is copied through `let b = v`.
- The analyzer must also preserve instantiated generic struct shapes so `boxed.value` resolves correctly for `Box<string>`.

## Phase 3: RSS Function Explicit Instantiation And Return Inference

Files:

- [src/compiler/typing/context.rs](../src/compiler/typing/context.rs)
- [src/compiler/typing/collect.rs](../src/compiler/typing/collect.rs)
- [src/compiler/typing/helpers.rs](../src/compiler/typing/helpers.rs)
- [src/compiler/typing.rs](../src/compiler/typing.rs)

Changes:

1. Introduce an instantiation key for generic RSS function calls:

```rust
(function_index, type_args)
```

2. Update `infer_function_return(...)` so it accepts explicit type args, creates a generic substitution environment, seeds declared parameter schemas and the function body under that environment, and infers the return from the instantiated body.
3. Do not rely on the existing "observed parameter types per function index" maps for generic functions. Those maps will conflate distinct instantiations.
4. Either:
   - maintain generic observation and validation caches per instantiation, or
   - analyze generic functions on demand at each call site and cache the result by instantiation key
5. Preserve the instantiated schema on local derived bindings and on the return value so this works:

```rss
fn myfn<T>(v: T) {
  let b = v;
  b
}

let v = myfn::<string>("hello");
```

and `v` is inferred as `string`.

Recommendation:

- Keep non-generic RSS functions on the current fast path.
- Put generic RSS functions on a separate instantiation-aware path instead of rewriting the whole inference pipeline at once.

## Phase 4: Generic Struct Validation And Diagnostics

Files:

- [src/compiler/typing/context.rs](../src/compiler/typing/context.rs)
- [src/compiler/typing/helpers.rs](../src/compiler/typing/helpers.rs)
- [src/compiler/pipeline.rs](../src/compiler/pipeline.rs)

Changes:

1. Ensure declared local schemas such as `Box<string>` survive legalization and validation without collapsing to a bare `map`.
2. Instantiate generic struct bodies before schema mismatch checks so this works:

```rss
struct Box<T> { value: T }

let boxed: Box<string> = { value: "hello" };
boxed.value;
```

3. Ensure diagnostics explain instantiated schemas accurately when fields mismatch:

```rss
struct Box<T> { value: T }

let boxed: Box<string> = { value: 123 };
```

should report the instantiated `string` expectation, not only a generic `T`.
4. Keep hover and local type hints useful for instantiated generic structs.

## Phase 5: Host Function Generic Metadata

Files:

- [src/builtins/metadata.rs](../src/builtins/metadata.rs)
- [build.rs](../build.rs)
- [pd-host-function/src/lib.rs](../pd-host-function/src/lib.rs)
- [src/compiler/typing/helpers.rs](../src/compiler/typing/helpers.rs)
- [src/compiler/typing/context.rs](../src/compiler/typing/context.rs)

Baseline target:

```rss
let payload = json::decode::<MyStruct>(text);
```

Changes:

1. Extend callable metadata so selected host functions can declare:
   - generic arity
   - how type args map into the return schema
2. Minimal metadata is enough for the base rollout:

```text
json::decode<T>(string) -> T
```

3. Update host-signature lookup so the analyzer can see this metadata for imported host functions.
4. When a host call includes explicit type args:
   - validate generic arity
   - use the return-schema template to produce the inferred schema and coarse bound type
5. For host calls without generic metadata, reject turbofish usage explicitly instead of silently ignoring it.

Important scope rule:

- In the base version this is still analyzer-only. `json::decode::<MyStruct>` produces the same runtime VM value as today's `json::decode`; the new behavior is compile-time type and schema knowledge.

## Phase 6: Pipeline, Hints, And Tooling

Files:

- [src/compiler/pipeline.rs](../src/compiler/pipeline.rs)
- [pd-vm-wasm/src/lib.rs](../pd-vm-wasm/src/lib.rs)
- [pd-vm-wasm/src/completions.rs](../pd-vm-wasm/src/completions.rs)
- [pd-controller/webui/src/app/monaco/completionCatalog.ts](../pd-controller/webui/src/app/monaco/completionCatalog.ts)

Changes:

1. Ensure generic metadata survives the parse -> legalize -> validate -> infer -> compile pipeline.
2. Keep local type hints correct for instantiated results such as `v = myfn::<string>("hello")`.
3. Keep local type hints inside generic RSS functions correct for derived locals such as `let b = v`, where both locals should still render as `T` under the active instantiation.
4. Keep local type hints correct for instantiated generic structs such as `boxed: Box<string>`.
5. Prefer rendering local hover and type hints as `T`, `Box<string>`, or `MyStruct` when schema knowledge exists, rather than collapsing everything to only coarse `ValueType`.
6. Update completion and hover strings so generic RSS functions, generic RSS structs, and generic host functions display their type params.

This phase is not required for semantic correctness, but without it the feature will feel incomplete in the editor.

## Next Step: Infer Type Args From Value Arguments And Expected Context

This is the follow-on after the explicit-only base.

### Step 7A: infer from value arguments

Target examples:

```rss
fn id<T>(value: T) {
  value
}

let s = id("hello");
```

```rss
fn pair<T, U>(left: T, right: U) {
  { left: left, right: right }
}

let p = pair("a", 1);
```

Implications:

- Call-site inference should build on typed function parameters such as `value: T`; the base generic work should already parse and represent those parameter schemas in explicit-instantiation mode.
- The analyzer needs a unification step from actual argument schemas to generic params.
- Partial inference must define clear rules for conflicts and missing information.

### Step 7B: infer from expected result context

Target examples:

```rss
fn make<T>() {
  let value: T = source();
  value
}

let s: string = make();
```

```rss
use json;

struct Profile { name: string }

let payload: Profile = json::decode(text);
```

Implications:

- Validation and inference need a notion of expected type flowing backward from assignment targets or declared schemas.
- This is broader than call-site syntax and should be implemented only after explicit instantiation is stable.
- Expected-context inference should stay secondary to explicit type args. `::<...>` must always win when both are present.

## Stretch Goal: Runtime Schema-Aware Host Execution

This is the part that is not frontend-only.

Goal:

- allow `json::decode::<MyStruct>(text)` to hand `MyStruct`'s shape to the host implementation itself, so deserialization can validate the target shape instead of only returning a generic VM map

Likely work:

1. Thread instantiated schema metadata from the compiler into host-call metadata or a compiler primitive lowering.
2. Extend the host-call interface so a builtin or host wrapper can inspect the requested schema.
3. Teach `json::decode` to validate or deserialize against that schema during execution.

This should be a separate phase because it touches runtime call plumbing and host-function APIs rather than only parser and analyzer state.

## Risks

### Risk 1: generic instantiations collapsing together

The current analyzer stores observed function argument types by function index only. That is incompatible with multiple instantiations of the same generic function and must not be reused unchanged.

### Risk 2: overloading `TypeSchema::Named`

If `T` is represented as `Named("T")`, parser validation and schema resolution will mis-handle it as a struct name.

### Risk 3: editor hints showing only coarse runtime types

If the feature stops at `BoundType`, local hover output will degrade to `map` or `unknown` instead of showing useful instantiated schemas.

### Risk 4: generic structs widen the IR and schema model

Adding generic structs means the current plain `struct_schemas: HashMap<String, TypeSchema>` representation is no longer enough. The plan must account for dedicated struct declaration metadata and schema instantiation, not only parser sugar.

### Risk 5: follow-on inference phases need typed parameters or back-propagation rules

Inferring `T` from `id("hello")` and `let s: string = make()` requires more than explicit turbofish parsing. Those follow-on phases need either typed RSS parameters, expected-type propagation, or both.

### Risk 6: local copies can lose generic schema identity

If local binding inference only copies coarse `BoundType`, then `let b = v` inside `fn myfn<T>(v: T)` will degrade `b` from schema `T` to only a runtime category. That would break return inference, diagnostics, and editor hints for generic locals.

## Verification Plan

Add tests in:

- [tests/compiler/type_inference_tests.rs](../tests/compiler/type_inference_tests.rs)
- [tests/compiler/compiler_rustscript_tests.rs](../tests/compiler/compiler_rustscript_tests.rs)
- [tests/compiler/diagnostics_tests.rs](../tests/compiler/diagnostics_tests.rs)

Coverage:

1. Parse success:
   - `fn a<T>() {}`
   - `fn a<T, U>() {}`
   - `fn myfn<T>(v: T) { let b = v; b }`
   - `struct Box<T> { value: T }`
   - `let boxed: Box<string> = { value: "hello" };`
   - `json::decode::<Profile>(text)`
2. Parse failure:
   - wrong type-arg count on generic function call
   - wrong generic-arg count on generic struct instantiation
   - duplicate type param names
   - turbofish on non-generic RSS function
3. RSS inference:
   - `myfn::<string>("hello")` returns `string`
   - inside `fn myfn<T>(v: T) { let b = v; b }`, `b` is tracked as `T`
   - same generic function instantiated as `string` and `int` in the same source
4. Generic struct schema inference:
   - `Box<string>` exposes `.value` as `string`
   - `Pair<string, int>` exposes each field with the correct instantiated type
5. Host inference:
   - `json::decode::<Profile>(text)` exposes `Profile` fields in analyzer and hover
6. Diagnostics:
   - field access on opaque `T` without a concrete bound remains rejected
   - generic struct field mismatch reports the instantiated expected schema
   - generic-arity mismatch produces a clear error
7. Follow-on tests for the next step:
   - `id("hello")` infers `T = string`
   - `let s: string = make();` infers `T = string`

## Recommended Delivery Order

1. Phase 1: parser and IR
2. Phase 2: generic-aware schema resolution
3. Phase 3: RSS function explicit instantiation and return inference
4. Phase 4: generic struct validation and diagnostics
5. Phase 5: host-function generic metadata, starting with `json::decode`
6. Phase 6: hints and completions polish
7. Next step: infer from value arguments and expected context
8. Stretch runtime-aware host execution only after the frontend-only version is stable

This order gives usable explicit RSS generics first, includes generic structs in the base schema model, then brings `json::decode::<T>` online, then improves editor ergonomics, and only after that adds inference from arguments or expected context.
