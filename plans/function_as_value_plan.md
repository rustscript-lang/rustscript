# Function as Value Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Make named functions, host functions, and closures real runtime `Value`s that can be assigned, returned, stored in collections, retained by Rust host/UI APIs as event callbacks, preserved across suspension, invoked from RSS or Rust embedding code, and preserve RustScript lifecycle, typing, and generic instantiation semantics. Callable definitions belong to the currently installed Program; replacing that Program invalidates all existing callable values and callbacks.

**Architecture:** Build on real script call frames, but keep the bytecode surface small: retain existing `Call` for builtin/host imports, add `CallValue` for RSS callable invocation, and add `MakeClosure` only where evaluating an anonymous function or capturing nested function must allocate a fresh environment-bound instance. Capture-free named/host functions are Program-owned definitions whose runtime bindings are initialized with the Program, so they require no creation opcode. Existing `Ldloc`/`Stloc` resolve frame-owned or captured cells, and `Ret` becomes frame-aware. Keep generic substitution and callable signatures in the compiler/schema layer; runtime code remains erased and generic function values are monomorphic at the value-creation site.

**Tech Stack:** Rust 2024, RustScript frontend IR/parser/type analysis, pd-vm bytecode/interpreter, VMBC, pd-vm-nostd, trace JIT, SSA/AOT, debugger and REPL.

---

## 1. Scope and semantic contract

### 1.1 Supported values

The completed feature supports all of these:

```rss
fn add_one(value: int) -> int {
    value + 1
}

let named: fn(int) -> int = add_one;
let closure: fn(int) -> int = |value| value + 1;
let selected = if condition => { named } else => { closure };
let functions = [named, closure];
let table = {"add": named, "inc": closure};

fn make_adder(amount: int) -> fn(int) -> int {
    |value| value + amount
}

let add_two = make_adder(2);
add_two(40);
functions[0](41);
table["inc"](41);
```

It also preserves already accepted higher-order flows such as:

```rss
fn apply_twice(func: fn(int) -> int, value: int) -> int {
    let once = func(value);
    func(once)
}

apply_twice(add_one, 40);
```

The current implementation accepts that last form by keeping `InferredCallable` and `CallableBinding` compiler metadata and inlining the target. The completed feature must produce the same result through runtime callable values and indirect dispatch.

### 1.2 Rust embedding contract

Rust embedding code must be able to obtain and invoke both exported RSS functions and closure values returned by RSS:

```rss
export fn add_one(value: int) -> int {
    value + 1
}

export fn make_adder(amount: int) -> fn(int) -> int {
    |value| value + amount
}
```

```rust
let compiled = compile_source(source)?;
let mut vm = Vm::new(compiled.program);

let add = vm.exported_callable("add_one", &[])?;
let first = vm.invoke_callable(&add, [Value::Int(41)])?;
assert_eq!(first.returned_one()?, Value::Int(42));

let factory = vm.exported_callable("make_adder", &[])?;
let closure_result = vm.invoke_callable(&factory, [Value::Int(2)])?;
let add_two = closure_result.returned_one()?;
let second = vm.invoke_callable(&add_two, [Value::Int(40)])?;
assert_eq!(second.returned_one()?, Value::Int(42));
```

The exact API names may follow existing `Vm`/`Store` naming, but the contract is fixed:

- Rust can resolve an exported RSS function without running the root script body.
- Rust can pass raw `Value` arguments and receive zero/one/many returned `Value`s.
- Rust can retain an RSS closure as `Value::Callable` and invoke it later on the owning VM/Store while the same Program instance remains installed.
- RSS can pass a named function or closure into a Rust host listener API, which retains a typed callback handle and invokes it for later queued events.
- Generic exported functions accept explicit `TypeSchema` arguments for validation and callable-schema instantiation; runtime dispatch remains erased.
- Script calls that yield or wait expose a resumable status instead of pretending to be synchronous.
- Initial support rejects invocation while the VM is already executing; reentrant RSS calls from a host callback are a later feature.
- A callback handle invoked through a different owning Store or after Program replacement produces a specific error.

### 1.3 Initial exclusions

The first complete rollout excludes:

- callable serialization as a VMBC constant or JSON value
- callable values as map keys
- reflection that exposes captured values to ordinary scripts
- trait bounds, `where` clauses, callable variance, or overload sets
- runtime generic specialization and monomorphized bytecode
- native execution of indirect calls in the first interpreter-complete milestone
- cyclic closure environments or a cycle collector

Arrays and maps may contain callable values as elements/values. Attempting to use a callable as a map key must produce a deterministic runtime error.

### 1.4 Equality, formatting, and runtime type

Define these semantics explicitly:

- `type(callable)` returns `"function"`.
- Capture-free named/host function values compare equal when their Program instance IDs and callable prototype IDs match.
- Capturing named functions and closure literals compare by callable-object identity; aliases of one value compare equal.
- Two separately evaluated closure literals compare unequal, even when their code and captures are equal.
- Callable ordering is unsupported.
- Callable hashing as a script map key is rejected.
- Debug/CLI formatting uses `<fn module::name>` for named callables and `<closure source:line>` for closures; capture contents are omitted.
- JSON encode/decode rejects callable schemas and values with a precise path diagnostic.

## 2. Current implementation baseline

The plan must preserve the following existing behavior rather than replacing it blindly.

### 2.1 Parser and IR

Current files:

- `src/compiler/ir.rs`
- `src/compiler/parser/mod.rs`
- `src/compiler/parser/expressions.rs`
- `src/compiler/parser/statements.rs`
- `src/compiler/parser/symbols.rs`
- `src/compiler/linker.rs`

Current state:

- `Expr::FunctionRef(u16)` represents a named function reference.
- `Expr::LocalCall(LocalSlot, Vec<TypeSchema>, Vec<Expr>)` represents a call through a local.
- `Expr::Closure(ClosureExpr)` and `Expr::ClosureCall(...)` represent closure syntax and statically resolved closure calls.
- `ClosureExpr` stores parameter slots, capture-copy pairs, and one expression body.
- Nested `fn` bodies use `FunctionImpl`, which already records parameter slots, capture copies, body statements, and the result expression.
- `FunctionDecl` already carries argument schemas, a return schema, and `type_params`.
- Generic direct calls already carry explicit `type_args`.
- The parser currently rejects `let f = id::<string>;` and type arguments on a local callable.

### 2.2 Compile-time callable tracking

Current files:

- `src/compiler/typing/state.rs`
- `src/compiler/typing/helpers.rs`
- `src/compiler/typing/context.rs`
- `src/compiler/typing/collect.rs`
- `src/compiler/typing/validate.rs`
- `src/compiler/codegen.rs`

Current state:

- `LocalTypeState` stores `InferredCallable::Function` or `InferredCallable::Closure` separately from ordinary value types.
- Branch merging preserves only the same named function target; differing functions and closures lose callable identity.
- `TypeSchema::Callable { params, result }` already exists and is encoded in VMBC type metadata.
- `TypeMap.callable_slots` marks callable locals, but `ValueType` has no callable variant.
- `CodeGenerator.callable_bindings` makes local calls work by statically selecting and inlining a function or closure.
- Scalar compilation rejects `Expr::FunctionRef`, `Expr::Closure`, and callable `Expr::Var` with `CallableUsedAsValue`.

The runtime implementation must make compile-time identity an optimization only. Correctness may no longer depend on `callable_bindings` retaining one exact target through every branch.

### 2.3 Capture and lifecycle analysis

Current files:

- `src/compiler/lifetime/availability.rs`
- `src/compiler/lifetime/availability/captures.rs`
- `src/compiler/lifetime/liveness.rs`
- `src/compiler/codegen.rs`
- `tests/vm/drop_contract_tests.rs`
- `tests/vm/runtime_state_edge_tests.rs`

Current state:

- Parser capture discovery allocates hidden capture slots and records `(source_slot, captured_slot)` pairs.
- Availability analysis classifies capture use as copy, borrow, mutable borrow, or move.
- Codegen materializes captures into hidden locals at closure binding or nested-function declaration.
- Liveness keeps persistent capture slots alive and later emits `Stmt::Drop` to clear hidden locals.
- Yield/pending tests verify that hidden closure/call state survives suspension and is cleared after resume.

Real closure values must replace hidden-global capture persistence with environment ownership while retaining the same copy/move diagnostics and drop timing.

### 2.4 Runtime and wire format

Current files:

- `src/bytecode.rs`
- `src/assembler.rs`
- `src/vm/mod.rs`
- `src/vmbc.rs`
- `pd-vm-nostd/src/lib.rs`
- `pd-vm-nostd/src/vmbc.rs`

Current state:

- `Value` contains only null, scalar, string/bytes, array, and map variants.
- `ValueType` ends at `Map`.
- `Program` has one code blob, one local count, constants, host imports, debug metadata, and type metadata.
- `OpCode::Call` invokes builtins/host imports.
- `OpCode::Ret` halts root execution.
- There is no script-function table, script frame stack, closure environment, or indirect call opcode.

`plans/real_script_call_frames_plan.md` is therefore a prerequisite for frame metadata, frame-relative locals, and frame-aware `Ret`. Its opcode design is aligned with this plan: RSS functions are entered through `CallValue`, without a separate `CallScript`.

## 3. Runtime representation

### 3.1 Program-owned callable definitions

A callable definition is part of `Program`, alongside its code and function metadata:

```rust
pub struct Program {
    // existing fields
    pub script_functions: Vec<ScriptFunction>,
    pub callable_prototypes: Vec<CallablePrototype>,
}

pub struct CallablePrototype {
    pub kind: CallableKind,
    pub target: PrototypeTarget,
    pub arity: u8,
    pub capture_layout: CaptureLayout,
    pub signature: TypeSchema,
}

pub enum CallableKind {
    Named,
    ClosureLiteral,
}

pub enum PrototypeTarget {
    Script { function_id: u16 },
    Host { import_id: u16 },
}
```

`Program` owns the callable prototypes, target descriptors, code, schemas, and capture layouts. Installing a Program into `Vm`/`Store` assigns a fresh opaque `ProgramInstanceId`. Replacing the Program, resetting the execution instance, or installing a new REPL compilation assigns a new ID and invalidates every callable created under the previous ID.

There is no cross-Program invocation and no retained old Program. Program replacement first cancels pending callback work and removes listener registrations, then drops the old Program and its VM-owned callable tables. A stale Rust-held value may still own its closure environment until that value is dropped, but invoking it returns `StaleCallable` without reading its prototype ID from the new Program.

### 3.2 Callable value

Add one heap-backed VM value that refers to the active Program without owning it:

```rust
pub struct CallableValue {
    pub program_instance: ProgramInstanceId,
    pub prototype_id: u16,
    pub env: Option<SharedClosureEnv>,
}

pub type SharedCallable = Arc<CallableValue>;
```

The prototype table contains callable kind, target descriptor, capture layout, arity, and callable schema. Capture-free named functions and host function values use `env: None`; capturing closures use `Some(env)`. `CallableKind` preserves identity semantics for a capture-free closure literal, which still compares by callable-object identity rather than as a named function. Generic references that occur in RSS source are interned as distinct prototypes when their substituted schemas differ, while those prototypes may point to the same erased script body. Do not duplicate target or signature metadata in every runtime value.

Then extend:

```rust
Value::Callable(SharedCallable)
ValueType::Callable
BoundType::Callable
```

`TypeSchema::Callable` remains the precise structural signature. `BoundType::Callable` and `ValueType::Callable` provide the coarse runtime category, replacing the current `Unknown + callable_slots` split.

### 3.3 Closure environment

Use an owned, stateful environment shared by every alias of one closure/nested captured function:

```rust
pub struct ClosureEnv {
    pub captures: Box<[ClosureCell]>,
}

pub struct ClosureCell {
    value: RuntimeCell<Value>,
}

pub type SharedClosureEnv = Arc<ClosureEnv>;
```

`RuntimeCell` is a short-lived interior-mutation abstraction: a std synchronization cell for the main VM and the project-approved single-thread/no-std equivalent for embedded builds. Never hold a cell guard across a host call, script call, yield, pending suspension, or debugger callback.

Semantics:

- Every evaluated closure literal creates one environment snapshot.
- A nested `fn` with captures creates a closure target backed by the same environment model; a top-level/capture-free `fn` remains a plain script target.
- Captured values are copied or moved into environment cells according to existing availability analysis.
- Closure aliases share the same environment and therefore share captured mutable state.
- Separately evaluated closure literals/nested function instances have independent environments.
- Compile captured reads/writes through the callable frame's slot map. Existing `Ldloc`/`Stloc` resolve ordinary slots to frame storage and captured slots to shared environment cells, so writes become visible to later calls immediately.
- Borrow and mutable-borrow syntax inside the callable borrows one environment cell only for the duration of the operation; the environment never stores a pointer into a caller frame.
- Returning or storing a closure therefore cannot leave dangling frame references.
- Existing behavior is normative: a captured nested function counter called twice currently produces first `1`, then `2`; the runtime-value implementation must preserve that state progression.
- A `Stloc` resolved to a captured cell must reject writes that would create a callable/environment ownership cycle, including a callable hidden inside an array/map, until cycle collection exists.

Add characterization tests for repeated invocation of a nested function that writes a captured local, alias calls that observe the same state, and separately created closure instances that do not share state.

### 3.4 Drop and reset behavior

Required lifecycle rules:

- Cloning a callable increments only its callable/environment reference counts; it never retains the Program.
- Dropping the last closure callable drops every captured `Value` exactly once.
- `Stmt::Drop`, local overwrite, stack pop, container replacement/removal, VM error unwind, and normal frame return all release callable references through ordinary `Value` destruction.
- A suspended host call keeps the complete script frame stack, current callable, and closure environment alive.
- `reset_for_reuse`, Program replacement, and REPL Program installation increment `ProgramInstanceId`, cancel pending callback work, remove listeners, and clear operand stack, locals, call frames, pending host state, and active closure roots before accepting new state.
- Program replacement drops the old Program after VM/Store-owned callback roots are removed. External stale callable values retain only their own environments and fail with `StaleCallable` when invoked.
- Runtime errors during indirect calls unwind frame/environment roots without double-drop.
- The initial design must prevent callable/environment cycles; if a future mutation API can create one, it must reject the write or add cycle management first.

## 4. Bytecode and frame semantics

### 4.1 Prerequisite frame model

Implement the frame metadata from `plans/real_script_call_frames_plan.md` first:

- `Program.script_functions`
- frame-relative local slot resolution
- `CallFrame`
- frame-aware `Ret`
- recursion and suspension-safe frame stacks

The frame plan must not add a separate direct-script call opcode. Script entry is shared with first-class callable invocation.

### 4.2 New opcodes

The real-frame milestone adds only:

```text
CallValue argc
```

The full first-class callable feature adds one creation instruction for runtime closure instances:

```text
MakeClosure prototype_id
```

Contracts:

- Program installation resolves every capture-free named/host callable prototype into the active Program state and initializes its compiler-assigned hidden binding. Loading or copying those ordinary function values uses existing `Ldloc`/`Stloc`; named functions never execute `MakeClosure`.
- `MakeClosure prototype_id` is emitted only when evaluating an anonymous function literal, including a capture-free literal that still requires fresh identity, or a nested named function whose current lexical captures must be bound. It reads the capture layout from Program metadata, creates one environment instance when captures exist, and pushes a callable tagged with the current `ProgramInstanceId`. The instruction has no inline `capture_count` or capture operands.
- `CallValue argc` first requires the callable's Program instance ID to equal the active one. It then resolves the prototype from the current Program, validates target/arity, and dispatches to either a script frame or the existing host-call machinery. A mismatched ID returns `StaleCallable`; it never switches Programs or reads the same prototype index from a replacement Program.

Use one documented stack order. Recommended order is `callee, arg0, ..., argN`, with `CallValue` consuming the arguments and callee as one logical call record.

Keep existing instructions:

- known builtin/host import -> existing `Call(index, argc)`
- local/captured reads and writes -> existing `Ldloc` / `Stloc`
- script/closure return -> existing frame-aware `Ret`

All RSS function calls, including statically known named calls, lower through callable values and `CallValue`. Capture-free named/host callable bindings are initialized from Program metadata when the Program instance is installed; later calls use `Ldloc` plus `CallValue`. The compiler/JIT may attach known-target metadata and specialize dispatch later without adding a semantic `CallScript` opcode.

Declaration timing preserves the current frontend behavior:

- Function names are source ordered. The parser registers a declaration before parsing its own body, so self-recursion is valid, while a statement before the declaration cannot refer to it.
- Program initialization may prepare capture-free named bindings because this has no capture or identity side effect; parser source ordering still controls which references exist.
- An anonymous function literal executes `MakeClosure` whenever evaluated so separate evaluations remain distinct, even with no captures.
- A capturing nested named function also executes `MakeClosure` at its declaration because its environment depends on the current activation. This is the same environment-binding operation as an anonymous closure, not ordinary named-function loading.
- `CallValue` binds the active callable into the callee's metadata-designated self slot when recursion needs a value. Recursive calls then use ordinary `Ldloc` plus `CallValue`; no recursion-specific opcode is added.
- Add parser/compiler tests that lock source-order rejection, self-recursion, declaration-in-branch timing, anonymous-function identity, and repeated references to one capture-free named declaration.

### 4.3 Reference-VM rationale

This follows the small common core used by other bytecode runtimes:

- Lua uses `OP_CLOSURE` to materialize a function prototype and `OP_CALL` to invoke the resulting function value.
- Wren emits `CODE_CLOSURE` even for functions without upvalues so every called function has one uniform closure representation; its VM then calls that closure through the normal function-call path.
- WebAssembly typed function references separate creation (`ref.func`) from invocation (`call_ref`).
- CPython also separates function construction from generic `CALL`.

Lua/Wren expose dedicated upvalue load/store opcodes. pd-vm can avoid those initially because real frames already require a local-slot resolver. `ScriptFunction.slot_layout` marks each logical local slot as frame-owned or captured, and existing `Ldloc`/`Stloc` resolve through that layout. If profiling later proves this indirection expensive, specialized capture opcodes may be introduced as an optimization; they are not part of the semantic baseline.

References:

- [Lua 5.4 opcodes](https://www.lua.org/source/5.4/lopcodes.h.html)
- [Wren opcodes](https://github.com/wren-lang/wren/blob/main/src/vm/wren_opcodes.h)
- [Wren closure/upvalue implementation](https://github.com/wren-lang/wren/blob/main/src/vm/wren_vm.c)
- [WebAssembly typed function references](https://github.com/WebAssembly/spec/blob/main/proposals/function-references/Overview.md)
- [CPython bytecode instructions](https://docs.python.org/3/library/dis.html)

### 4.4 Call frame roots

Extend `CallFrame` with:

- return IP
- previous frame base/local length
- active callable root
- optional closure environment
- function/prototype ID for diagnostics

For a closure call, attach the shared environment to the new frame and bind parameters into ordinary frame locals. `ScriptFunction.slot_layout` maps captured logical slots to the environment; ordinary `Ldloc`/`Stloc` use the same frame accessor for both storage kinds. Keep the callable root in the frame until `Ret` so its environment cannot be released mid-call. Every frame in one invocation uses the same active Program instance.

## 5. Parser and IR changes

### 5.1 Preserve value-producing expressions

Modify:

- `src/compiler/ir.rs`
- `src/compiler/parser/expressions.rs`
- `src/compiler/parser/statements.rs`
- `src/compiler/linker.rs`
- `src/compiler/source_loader/line_map.rs`

Changes:

1. Keep `Expr::FunctionRef`, but attach explicit type arguments or a resolved callable-instance key.
2. Keep `Expr::Closure`, but assign every closure body a compiler function ID/prototype instead of relying on inline-only AST storage.
3. Represent all calls uniformly enough that codegen can choose direct or indirect dispatch without parser-only semantic differences.
4. Permit callable expressions in arrays, map values, returns, branch results, match results, and ordinary assignments.
5. Keep map-key validation separate so callable keys remain rejected.
6. Preserve source spans for callable creation and invocation diagnostics.

### 5.2 Generic function reference syntax

Accept explicit generic function values:

```rss
fn id<T>(value: T) -> T { value }
let string_id: fn(string) -> string = id::<string>;
```

Rules:

- `name::<T>` without `(...)` creates a monomorphic callable value.
- A local callable is already instantiated; `local::<T>(...)` remains invalid.
- A bare generic function reference is allowed only when all type parameters can be resolved from an expected callable schema.
- Otherwise emit a diagnostic requiring explicit type arguments.
- Runtime target code is still erased; type arguments affect compiler validation/schema metadata only.

### 5.3 Closure syntax and contextual typing

Keep existing `|x| expr` syntax. Add contextual typing:

```rss
let parse: fn(string) -> int = |text| text.length;
```

The expected callable schema seeds closure parameter and result validation. An escaping closure whose signature cannot be inferred from a declaration, return schema, container schema, or call site must produce a precise “callable signature required” diagnostic.

## 6. Type system, callable schemas, and generics

### 6.1 Coarse and precise callable types

Modify:

- `src/compiler/ir.rs`
- `src/compiler/typing/state.rs`
- `src/compiler/typing/context.rs`
- `src/compiler/typing/helpers.rs`
- `src/compiler/typing/collect.rs`
- `src/compiler/typing/validate.rs`
- `src/compiler/pipeline.rs`
- `src/bytecode.rs`

Use:

- `BoundType::Callable` for flow analysis
- `ValueType::Callable` for runtime/type-map metadata
- `TypeSchema::Callable` for parameter/result structure

`TypeMap.callable_slots` may remain for one VMBC compatibility version, but new code must derive callable status from `ValueType::Callable` and/or `local_schemas`. Remove the redundant bitmap only in a separately versioned cleanup.

### 6.2 Assignment and branch rules

Initial callable schema compatibility is invariant after generic substitution:

- parameter list length must match
- each parameter schema must match exactly after resolution
- result schema must match exactly after resolution
- `Unknown` may defer a dynamic check in non-strict mode
- strict mode rejects unresolved callable signatures at escaping boundaries

Branch and loop merges:

- same schema, different targets -> preserve callable schema and coarse callable type, discard exact target identity
- same named target -> preserve exact target as an optimization
- incompatible schemas -> existing branch mismatch diagnostic
- callable vs non-callable -> type mismatch

This removes the current behavior where differing callable targets collapse to unknown/non-callable state.

### 6.3 Callable parameters and returns

Validate all of these structurally:

```rss
fn apply<T, U>(f: fn(T) -> U, value: T) -> U {
    f(value)
}

fn choose(flag: bool) -> fn(int) -> int {
    if flag => { add_one } else => { |x| x + 2 }
}
```

Known indirect calls get compile-time arity and schema checks. Calls through `unknown` values retain a runtime callable/arity check in dynamic mode.

### 6.4 Generic function values

Build on the existing frontend-only generic machinery in `plans/rss_frontend_generics_plan.md`:

```rust
GenericCallableInstanceKey {
    function_index: u16,
    type_args: Vec<TypeSchema>,
}
```

Rules:

1. Resolve and validate type arguments when the function value is created.
2. Substitute generic parameters into the callable schema.
3. Store the instantiated schema in `LocalTypeState`, inferred hints, hover data, and container schemas.
4. Erase type arguments before runtime dispatch; the callable runtime target remains one script/host target.
5. Compile generic script bodies conservatively with unresolved operand hints where one erased body serves multiple instantiations.
6. Cache validation/return inference by generic instance key, not bare function index.
7. Generic closures declared inside generic functions inherit the active substitution environment.
8. Generic host function values use existing analyzer-only metadata. Future runtime schema-aware hosts remain a separate feature.

Examples that must pass:

```rss
fn id<T>(value: T) -> T { value }
let f = id::<string>;
let out: string = f("ok");
```

```rss
fn apply<T, U>(f: fn(T) -> U, value: T) -> U { f(value) }
let out = apply::<int, string>(|x| "n=" + x, 42);
```

Negative examples:

```rss
let f = id;                 // unresolved T
let f = id::<string, int>;  // wrong generic arity
let f = id::<string>;
f::<int>(1);                // local callable cannot be re-instantiated
```

## 7. Lifetime, ownership, and closure migration

### 7.1 Replace hidden persistent capture slots

Modify:

- `src/compiler/lifetime/availability.rs`
- `src/compiler/lifetime/availability/captures.rs`
- `src/compiler/lifetime/liveness.rs`
- `src/compiler/codegen.rs`

Migration:

1. Keep parser capture discovery temporarily.
2. Convert capture pairs into an ordered `CaptureLayout` per callable prototype.
3. Availability analysis decides copy/move at `MakeClosure` execution.
4. Liveness treats the resulting callable local as the owner; source captures need remain live only until closure creation.
5. Remove closure captures from `persistent_capture_slots` once no inline-only closure path depends on them.
6. Preserve an inline direct-call optimization only after the runtime path passes all lifecycle tests; optimization must not change capture timing.

### 7.2 Move and borrow semantics

- A named function value is always cheap-copyable.
- A closure value is cheap-copyable because the callable object is shared.
- Creating a closure may move non-copyable captures when local move semantics require it.
- A moved source local becomes unavailable immediately after closure creation.
- Captured shared strings/bytes/arrays/maps retain current Arc/COW behavior.
- Borrow expressions in a closure operate through the active environment cell and cannot outlive the call operation.
- Captured assignment updates the shared environment immediately, so closure/nested-function aliases observe the same state on the next call.
- Returning a closure with any unresolved borrowed-frame capture is rejected before codegen.

### 7.3 Containers and alias analysis

Update collection and move analysis so:

- inserting a callable transfers/clones the callable `Value` exactly like other heap values
- removing/moving a callable from a collection follows current collection move rules
- callable environment ownership is not represented as a normal collection alias edge
- captured collections continue to use existing COW semantics
- callable map keys are rejected before `VmMap` hashing

## 8. VMBC, AOT artifacts, and no-std

### 8.1 Wire format

Modify:

- `src/vmbc.rs`
- `src/bytecode.rs`
- `tests/wire/wire_tests.rs`
- `tests/wire/wire_error_tests.rs`

Changes:

- encode/decode script-function and callable-prototype tables
- encode new opcodes
- add `ValueType::Callable`
- retain `TypeSchema::Callable` encoding
- bump the VMBC version
- reject `Value::Callable` in the constant encoder with an explicit wire error
- hard-code new opcode/value-type tags in compatibility tests

### 8.2 AOT artifact

Modify:

- `src/vm/aot/artifact.rs`
- `src/vm/aot/ir.rs`
- `src/vm/aot/runtime.rs`

Bump the artifact version when program metadata expands. The first version may reject `MakeClosure` and `CallValue` in native-only compilation, but the rejection must be explicit and tested.

### 8.3 no-std parity

Modify:

- `pd-vm-nostd/src/lib.rs`
- `pd-vm-nostd/src/vmbc.rs`
- `pd-vm-nostd/tests/embedded_vmbc.rs`
- `pd-vm-nostd/tests/embedded_host.rs`

The embedded runtime must decode the expanded program and execute direct/indirect script calls in the interpreter. Use the project’s existing shared-allocation abstraction; do not assume target atomics beyond what current `Arc`-backed values already require.

## 9. Interpreter, host calls, suspension, and errors

Modify:

- `src/vm/mod.rs`
- `src/vm/host.rs`
- `src/vm/error.rs` or the existing VM error definitions
- `tests/vm/runtime_state_edge_tests.rs`
- `tests/vm/drop_contract_tests.rs`

Required errors:

- non-callable value invoked
- callable arity mismatch
- callable signature mismatch at dynamic boundary
- stale callable from a replaced/reset Program instance
- callback belongs to a different owning Store
- unsupported callable map key
- call-depth limit exceeded

Suspension contract:

- host `Yield` and `Pending` inside direct or indirect script calls preserve every script frame
- pending completion resumes at the host-call continuation without replay
- closure environments stay rooted during suspension
- cancellation/error/reset releases those roots exactly once

## 10. Rust embedding API

Modify:

- `src/vm/mod.rs`
- `src/vm/store.rs`
- `src/compiler/mod.rs`
- `src/bytecode.rs`
- `src/lib.rs`
- `pd-host-function/src/lib.rs`
- host argument extraction/conversion modules used by `borrow_arg` / `take_arg`
- create `tests/vm/rust_callable_api_tests.rs`
- create `tests/vm/ui_callback_integration_tests.rs`
- add `pd-host-function` macro expansion/signature tests

### 10.1 Exported RSS functions

Compilation must preserve an exported callable catalog distinct from host imports:

```rust
pub struct ExportedCallable {
    pub name: String,
    pub prototype_id: u16,
    pub type_params: Vec<String>,
    pub signature: TypeSchema,
}
```

Expose resolution on both `Vm` and `Store`:

```rust
pub fn exported_callable(
    &self,
    name: &str,
    type_args: &[TypeSchema],
) -> VmResult<Value>;
```

Requirements:

- only `export fn` declarations are visible by name
- duplicate/ambiguous export names fail during linking
- generic arity and substitutions are validated before returning the callable
- the returned value is the same `Value::Callable` used inside RSS
- the returned value is tagged with the currently installed `ProgramInstanceId` and becomes stale when that instance is replaced/reset
- resolving a callable does not execute root statements or mutate VM locals/stack

### 10.2 Calling RSS functions and closures from Rust

Provide one raw-value invocation path shared by named functions and closures:

```rust
pub enum ScriptInvocationStatus {
    Returned(Vec<Value>),
    Yielded,
    Waiting(HostOpId),
}

pub fn invoke_callable<I>(
    &mut self,
    callable: &Value,
    args: I,
) -> VmResult<ScriptInvocationStatus>
where
    I: IntoIterator<Item = Value>;
```

`Store` mirrors this API. A convenience `invoke_exported(name, type_args, args)` may resolve and invoke in one call, but must delegate to the same runtime path.

Invocation semantics:

1. Require an idle VM/Store with no active root invocation, waiting host operation, or unfinished script frame.
2. Validate that the value is callable, that its Program instance ID equals the active instance, that any Store-bound callback handle belongs to this Store, and that the prototype accepts the argument count.
3. Seed an invocation-root frame whose return target is the Rust caller rather than a bytecode IP.
4. Execute through the ordinary interpreter loop, fuel/epoch checks, host registry, and direct/indirect call machinery.
5. On return, remove the invocation-root frame and return exactly the function's produced values without leaking them onto an unrelated root stack.
6. On `Yielded` or `Waiting`, keep the invocation root and callable alive; `resume_invocation()` continues the same call and returns another `ScriptInvocationStatus`.
7. On error/cancel/reset, release arguments, frames, results, and closure environments exactly once.

Do not implement Rust invocation by synthesizing source text, recompiling a wrapper, mutating `ip` without a frame, or calling compiler-only `FunctionImpl` data.

### 10.3 Typed convenience and generics

The base API uses raw `Value` and `TypeSchema`. Optional typed wrappers may build on existing host conversion traits later:

```rust
let id = vm.exported_callable("id", &[TypeSchema::String])?;
let result = vm.invoke_callable(&id, [Value::string("ok")])?;
```

Rules:

- Rust supplies explicit type args for generic exports in the base version.
- The export resolver substitutes type params into the callable schema and validates Rust-provided `Value` argument categories before invocation when the schema is concrete.
- Generic type args are not appended to the runtime stack and do not alter host ABI indices.
- A closure returned by a generic RSS function retains its instantiated callable schema in metadata available to Rust diagnostics.
- `Value::Callable` remains opaque to Rust callers except for public name/signature/debug accessors; capture storage is private.

### 10.4 Host-retained UI/event callbacks

The primary embedding use case is an RSS callable passed into a Rust UI/event host API and retained beyond the registering host call:

```rss
ui::on_click(button, |event| ui::set_text(label, event.label));
```

Conceptual Rust API:

```rust
#[pd_host_function(name = "ui::on_click")]
fn on_click(
    button: WidgetId,
    callback: ScriptCallback<(UiEvent,), ()>,
) -> ListenerId {
    ui.register_listener(button, callback)
}
```

This must be a first-class contract, not an accidental consequence of accepting `Value::Any`:

- Add an owned `ScriptCallback<Args, Ret>` (or equivalent erased handle plus schema) that wraps `Value::Callable`, owning Store identity, `ProgramInstanceId`, and the instantiated `TypeSchema::Callable` copied from the active Program's prototype table during extraction.
- Extend `pd-host-function` parameter labeling/extraction so `ScriptCallback<(A, B), R>` contributes `fn(A, B) -> R` host metadata and strict RSS type checking validates the listener registration call.
- Keep raw `Value::Callable` available for dynamic host APIs, but typed UI libraries should not need to downcast `Value` manually.
- A host function may clone and retain the callback after its borrowed `args` slice expires; the callback owns its callable/environment references but does not retain the Program.
- Registration must not invoke the callback synchronously while the VM is inside the host call. Synchronous UI notifications are queued and dispatched after the VM returns to an idle state.
- The owning `Store` maintains a callback/event queue and serializes invocations. If an event arrives while one script callback is yielded or pending, preserve FIFO order or apply an explicitly documented per-listener coalescing policy; never re-enter the active VM.
- Event sources running on another thread enqueue Rust-owned event values onto the Store's execution thread. `ScriptCallback` and event payload transfer must satisfy the chosen `Send` contract without moving the VM itself across unsupported threads.
- `ListenerId`/subscription removal drops the retained callback reference. Widget destruction, explicit unsubscribe, Program replacement, Store reset, and UI shutdown must each release registrations deterministically.
- Prevent ownership cycles such as widget -> listener -> callback -> captured widget handle. Prefer weak host resource handles or explicit subscription ownership; define the cycle policy before exposing strong widget captures.
- Callback errors follow a host-configurable policy (`propagate to event loop`, `report and keep listener`, or `report and disable listener`). Errors must include callback source/function metadata.
- Callback return values are ignored only for `ScriptCallback<Args, ()>`; typed non-unit callback results are returned to the event source.
- Hot reload/REPL Program replacement invalidates and removes every existing listener before the old Program is dropped. Queued events tagged with the old `ProgramInstanceId` are cancelled. An externally retained stale callback returns `StaleCallable`; UI integration must explicitly register callbacks from the new Program.

### 10.5 UI callback tests

Create integration tests that model a minimal event source without depending on a specific UI framework:

- RSS passes a named function to a Rust host listener registration function.
- RSS passes a capturing closure to the listener, the root script returns, and Rust fires the event later.
- Repeated events observe shared mutable capture state.
- Typed event arguments and callback return schema are accepted/rejected correctly in strict mode.
- Generic listener APIs substitute event types into `ScriptCallback<(E,), ()>`.
- Event during registration is queued, not reentrant.
- Event during yield/pending waits behind the active invocation and resumes in defined order.
- Program replacement cancels queued events, removes listeners, drops Store-owned callback roots, and makes external old handles return `StaleCallable`.
- Unsubscribe/widget teardown drops the last callback and its captures exactly once.
- Wrong callback arity/type, non-callable values, cross-Store handles, callback errors, and shutdown races produce deterministic results.
- `pd_host_function` generated metadata renders the precise callable signature instead of `any`.

### 10.6 Rust API tests

Cover:

- resolving and invoking an exported non-generic RSS function
- resolving an explicit generic export such as `id::<string>`
- obtaining a closure returned by RSS and invoking it after the creator frame returned
- invoking a closure that captures string/array/map values
- retaining a closure across yield/pending and resuming from Rust
- repeated invocation and exact result-stack isolation
- wrong arity, wrong argument type, non-callable value, unknown export, unbound generic args, stale Program instance, and cross-Store callback handle
- dropping the Rust-held last callable handle releases captures exactly once
- `Store` and direct `Vm` APIs produce identical results

## 11. JIT and AOT containment

Modify:

- `src/vm/jit/trace.rs`
- `src/vm/jit/recorder.rs`
- `src/vm/jit/ir.rs`
- `src/vm/jit/native/lower.rs`
- `src/vm/aot/ir.rs`

Interpreter-complete milestone:

- tracing stops cleanly before `MakeClosure` and `CallValue`
- active traces side-exit with complete frame/local/environment state
- native entry never receives an unmodeled callable heap pointer
- AOT returns a feature-specific unsupported error or uses interpreter fallback

Later optimization may add guarded direct-target specialization for `CallValue`, but only after interpreter semantics and deoptimization tests pass.

## 12. REPL, debugger, and tooling

### 12.1 REPL and Program replacement

Modify:

- `src/compiler/repl.rs` or current REPL compilation state files
- `src/cli.rs`
- `src/vm/mod.rs`
- REPL tests under `tests/compiler/` and `tests/vm/`

Installing a newly compiled REPL Program creates a fresh `ProgramInstanceId`. Before installation, cancel pending callback work, remove listener registrations, clear persisted callable bindings and callable-containing containers, and drop the prior Program. A callable from an earlier REPL Program is stale and cannot be invoked or rewritten to a new prototype index.

If non-callable REPL locals continue across entries, migration must reject or clear any nested callable found inside arrays/maps as well as direct callable locals. Rust-held old values remain destructible but invocation returns `StaleCallable`.

### 12.2 Debugger

Modify:

- `src/debug_info.rs`
- `src/debugger/mod.rs`
- debugger protocol/recording tests

Expose:

- script/closure frame names
- source range and call site
- frame-local parameters and locals
- closure capture names and types, with values shown only through existing debugger value permissions
- stack traces across direct and indirect calls

Recording/replay must either serialize a debugger-safe callable reference plus program/prototype metadata or explicitly mark callable-containing snapshots unsupported. It must never serialize raw pointers.

### 12.3 Editor hints

Update callable hovers/completions in the current repository surfaces so instantiated signatures render as:

```text
fn(string) -> string
```

Sibling repositories such as `rustscript-spec`, `pd-controller`, and the website require separate explicit scope before edits.

## 13. Implementation tasks

### Task 1: Characterize current callable and capture behavior

**Files:**

- Modify: `tests/compiler/compiler_rustscript_tests.rs`
- Modify: `tests/compiler/type_inference_tests.rs`
- Modify: `tests/vm/drop_contract_tests.rs`
- Modify: `tests/vm/runtime_state_edge_tests.rs`

**Steps:**

1. Add tests for repeated captured mutation, nested-function/closure aliasing, independent factory-created environments, move capture, pending host calls, and current drop counts.
2. Lock the observed nested-function counter result (`1` then `2`) as compatibility behavior.
3. Run the tests against the current compiler and record the accepted/rejected behavior in test names/comments.
4. Do not change semantics until these tests establish the baseline.

Run:

```bash
cargo test --test compiler_rustscript_tests callable -- --nocapture
cargo test --test type_inference_tests callable -- --nocapture
cargo test --test drop_contract_tests closure -- --nocapture
cargo test --test runtime_state_edge_tests closure -- --nocapture
```

### Task 2: Implement real direct script frames

**Files:** Follow `plans/real_script_call_frames_plan.md`.

**Steps:**

1. Add failing bytecode tests for `CallValue`, recursion, frame-local isolation, stale Program IDs, and frame-aware `Ret`.
2. Add Program-owned named callable bindings, `ScriptFunction`, `CallValue`, and interpreter frame state.
3. Update VMBC and no-std decoding.
4. Make JIT/AOT reject or side-exit explicitly.
5. Run direct-call and recursion tests before proceeding.

### Task 3: Add callable runtime value and aggregate lifecycle tests

**Files:**

- Modify: `src/bytecode.rs`
- Modify: `src/vm/mod.rs`
- Modify: value formatting/type/hash helpers
- Test: create `tests/vm/callable_value_tests.rs`

**Steps:**

1. Add failing tests for clone/drop/identity/type/format behavior.
2. Add Program-owned callable prototypes, `ProgramInstanceId`, `Value::Callable`, and `ValueType::Callable` without retaining Program from the value.
3. Reject callable map keys and constant serialization.
4. Verify reset/replacement invalidation, stale invocation errors, and environment drop behavior.

### Task 4: Add indirect invocation

**Files:**

- Modify: `src/bytecode.rs`
- Modify: `src/assembler.rs`
- Modify: `src/vm/mod.rs`
- Modify: `src/compiler/codegen.rs`
- Test: `tests/vm/callable_value_tests.rs`

**Steps:**

1. Add failing tests for `MakeClosure` and `CallValue` stack contracts.
2. Implement runtime anonymous/capturing function creation plus script/host callable dispatch.
3. Keep existing `Call` lowering for direct builtin/host imports; lower every RSS callable through `CallValue`.
4. Verify arity errors and suspension behavior.

### Task 5: Materialize closures and owned environments

**Files:**

- Modify: parser/IR/codegen/lifetime files listed above
- Test: `tests/compiler/compiler_rustscript_tests.rs`
- Test: `tests/vm/drop_contract_tests.rs`

**Steps:**

1. Convert current rejected return/container tests into passing runtime tests.
2. Add `MakeClosure` prototype/capture layouts.
3. Move capture lifetime from hidden persistent slots into closure environments.
4. Verify returned closures after creator-frame return.
5. Verify exact drop behavior and pending/resume roots.

### Task 6: Integrate callable schemas and branch flow

**Files:** typing files listed in section 6.

**Steps:**

1. Add failing tests for two different targets with one callable schema across `if`, `match`, and loops.
2. Add `BoundType::Callable` and `ValueType::Callable` propagation.
3. Make exact target identity optional optimization metadata.
4. Enforce strict callable schema compatibility.
5. Keep dynamic runtime checks for unresolved non-strict calls.

### Task 7: Add generic function values

**Files:**

- Modify: parser expressions and function metadata
- Modify: typing context/helpers/validation
- Modify: inferred hint collection
- Test: generic compiler tests and module import tests

**Steps:**

1. Add parser failures for `id::<string>` value syntax and contextual bare references.
2. Parse and store generic instance keys.
3. Substitute callable schemas at value creation.
4. Add generic higher-order function tests.
5. Verify erased runtime code with multiple instantiations in one program.
6. Verify imported generic function values retain type parameters and instantiated schemas.

### Task 8: Wire/no-std/native containment

**Files:** VMBC, no-std, JIT, and AOT files listed above.

**Steps:**

1. Add wire compatibility failures before changing tags/version.
2. Update VMBC and no-std together.
3. Add interpreter parity tests on std and no-std.
4. Add JIT side-exit and AOT rejection/fallback tests.
5. Verify no native path interprets callable pointers as existing heap tags.

### Task 9: REPL/debugger/tooling

**Steps:**

1. Add a REPL test that defines a closure, installs another compiled Program, and verifies that direct and container-nested old callables are cleared or return `StaleCallable`.
2. Add debugger stack-frame tests for direct and closure calls.
3. Add recording/replay behavior for callable-containing state.
4. Update local type hints and CLI formatting.

### Task 10: Expose RSS functions and closures to Rust

**Files:**

- Modify: `src/vm/mod.rs`
- Modify: `src/vm/store.rs`
- Modify: `src/compiler/mod.rs`
- Modify: `src/lib.rs`
- Modify: `pd-host-function/src/lib.rs`
- Modify: host argument extraction/conversion modules
- Create: `tests/vm/rust_callable_api_tests.rs`
- Create: `tests/vm/ui_callback_integration_tests.rs`
- Add: `pd-host-function` callable-parameter macro tests

**Steps:**

1. Add failing Rust tests for resolving and invoking `export fn` by name.
2. Add the invocation-root frame and raw `Value` result/status API.
3. Add failing tests for receiving an RSS-created closure and invoking it after the creator frame returns.
4. Add explicit generic export type arguments and substituted signature diagnostics.
5. Add owned `ScriptCallback<Args, Ret>` extraction plus precise `pd_host_function` callable metadata.
6. Add a minimal Rust event source: register callback from RSS, return to idle, then fire from Rust.
7. Add FIFO event queue behavior, non-reentrancy, cross-thread enqueue contract, unsubscribe, teardown, and callback error policy tests.
8. Add yield/pending/resume tests through both `Vm` and `Store`.
9. Add stale-Program, cross-Store callback, reentrancy, arity, type, and non-callable errors.
10. Verify listener removal and Rust-held last callback release captured values once.

Run:

```bash
cargo test --test rust_callable_api_tests -- --nocapture
cargo test --test ui_callback_integration_tests -- --nocapture
cargo test -p pd-host-function
```

### Task 11: Documentation and cleanup

**Files:**

- Modify: `README.md`
- Modify: `plans/real_script_call_frames_plan.md`
- Modify/remove obsolete callable compile errors and tests

**Steps:**

1. Document function values, callable schemas, generic instantiation, closure capture timing, and limitations.
2. Remove `CallableUsedAsValue`/`ClosureUsedAsValue` only after every previous rejection site has a replacement test.
3. Keep `NonCallableLocal` as a runtime/compiler diagnostic for invalid dynamic calls.
4. Mark the old inline-only callable path as an optimization or remove it after parity.

## 14. Verification matrix

### Language semantics

- named function assignment/copy/return
- host and builtin function values
- closure assignment/copy/return
- callable array elements and map values
- callable branch/match/loop merges
- nested and recursive calls
- direct vs indirect result parity
- arity and non-callable errors

### Lifecycle

- last-reference capture drop
- overwrite and explicit `Drop`
- container insertion/removal
- creator frame returns before closure call
- shared mutable capture state across aliases and repeated Rust/RSS calls
- independent environments from separate closure/factory evaluations
- ownership-cycle rejection for callable capture writes
- host yield/pending/resume
- runtime error unwind
- VM cancellation/reset/reuse invalidates old callable values
- REPL Program replacement clears persisted callables and rejects stale external handles

### Type system

- declared `fn(...) -> ...` schemas
- contextual closure parameter/result types
- strict mismatch diagnostics
- different targets with one schema
- generic named function values
- generic higher-order functions
- imported generic functions
- unresolved/wrong generic arguments

### Rust embedding

- resolve and invoke exported RSS function by name
- resolve explicit generic exported function with Rust-supplied `TypeSchema` args
- receive an RSS closure as `Value::Callable` and invoke it after its creator returns
- pass a named RSS function or capturing closure into a Rust host listener registration API
- extract typed `ScriptCallback<Args, Ret>` through `pd_host_function` with precise callable metadata
- retain callback after host args expire, then dispatch later from the owning Store
- serialize callback events without VM reentrancy; queue cross-thread sources onto the Store thread
- unsubscribe/widget teardown/Program replacement/Store shutdown release callback captures exactly once
- enforce callback error, return-value, Program invalidation, and ownership-cycle policies
- invoke captured closures repeatedly through both `Vm` and `Store`
- isolate invocation results from root/reused stacks
- yield, pending completion, and resume from Rust
- reject reentrancy, stale Program instances, cross-Store callback handles, wrong arity/type, unknown export, and non-callable values
- release Rust-retained callable captures exactly once

### Backends and formats

- VMBC round-trip and old-version rejection
- no-std decode and execution
- interpreter direct/indirect parity
- trace JIT side exit
- AOT explicit rejection/fallback
- debugger stack and replay behavior

### Required commands

Run narrow tests throughout, then finish with:

```bash
cargo fmt --all -- --check
cargo test --test compiler_rustscript_tests
cargo test --test type_inference_tests
cargo test --test drop_contract_tests
cargo test --test runtime_state_edge_tests
cargo test --test wire_tests
cargo test -p pd-vm-nostd
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace --release
```

If JIT/AOT features are not included by the default workspace command, also run their exact feature suites used by CI.

## 15. Completion criteria

The feature is complete only when:

- callable correctness no longer depends on compiler-only target identity
- named functions and closures are ordinary runtime `Value`s
- returned/stored closures retain valid owned captures
- direct and indirect calls share frame/return/suspension semantics
- callable schemas survive flow merges and strict validation
- explicit generic function values and generic higher-order functions work with erased runtime code
- Rust can resolve exported RSS functions, retain RSS closures, and invoke either through `Vm` and `Store` with resumable status
- Rust host/UI APIs can accept typed RSS callbacks, retain them after registration, queue later events without reentrancy, and release them on unsubscribe/teardown
- callable definitions remain Program-owned, runtime values do not retain Program, and every old callable becomes stale on Program replacement/reset
- VMBC/no-std understand callable metadata and new opcodes
- JIT/AOT fail closed or execute the feature correctly
- REPL/debugger lifecycle is defined and tested
- old rejection tests have been replaced with positive behavior plus focused negative diagnostics
- full workspace formatting, tests, Clippy, and release build pass
