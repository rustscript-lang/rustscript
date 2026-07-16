# Implementation Plan: Real Script Call Frames

## Goal

Introduce real runtime call frames for script-defined `fn` declarations so a function body is compiled once and invoked many times without duplicating its bytecode at each call site.

This plan is only for real script calls and frames.

Non-goals for this effort:

- Exposing callable `Value`s to every RSS expression/container/return position
- Returning/storing callables in arrays or maps
- Full escaping closure environments in the first milestone
- Host-retained UI/event callback APIs
- Tail-call optimization

First-class runtime callables, escaping closures, callable lifecycle, generic function values, and Rust-side invocation are specified separately in [function_as_value_plan.md](function_as_value_plan.md). That plan depends on the frame model defined here.

## Current State

Today the compiler and VM behave like this:

- Frontend IR keeps function definitions in `FunctionImpl`, plus `Expr::Call`, `Expr::FunctionRef`, and `Expr::LocalCall`.
- Codegen inlines any call whose target has a `FunctionImpl`.
- Final bytecode only emits `OpCode::Call` for builtins and host imports.
- The VM interprets `OpCode::Call` as builtin/host dispatch only.
- `OpCode::Ret` always halts the program; there is no intra-program return path.
- `Program` stores one code blob, one `local_count`, and one import table. It has no script-function table or entry-point metadata.
- Trace JIT and native AOT assume the same host-only call model.

This means:

- Repeated script calls duplicate the callee body in emitted bytecode.
- Recursive script functions are impossible under the current lowering.
- There is no real script stack frame, return address, or frame-local lifetime.

## Recommended End State

### Runtime model

Add real script frames while keeping the semantic opcode surface shared with first-class function values:

- Keep existing `OpCode::Call(index, argc)` for builtin/host import dispatch.
- Initialize capture-free named callable bindings from Program metadata when the Program instance is installed; this needs no creation opcode.
- Add only `OpCode::CallValue(argc)` to invoke those values and enter their script frames.
- Do not add `CallScript`; direct and first-class RSS calls share `CallValue`.
- Reuse `OpCode::Ret`:
  - if no script caller exists, halt the program
  - otherwise pop one script frame, restore the caller frame, and continue at the saved return IP
- Add a VM-side `CallFrame` stack.

### Program model

Add script-function and callable-prototype tables to `Program`. The callable definitions belong to Program; runtime values only reference them by the active Program instance ID and prototype ID:

```rust
pub struct ScriptFunction {
    pub decl_index: u16,
    pub name: String,
    pub arity: u8,
    pub entry_ip: u32,
    pub frame_local_count: u16,
    pub param_slots: Vec<u8>,
}

pub struct Program {
    // existing fields
    pub script_functions: Vec<ScriptFunction>,
    pub callable_prototypes: Vec<CallablePrototype>,
}
```

Recommended first version:

- Keep existing local-slot numbering from the compiler.
- Make `Ldloc`/`Stloc` frame-relative at runtime.
- For each separately emitted script function, set `frame_local_count` to `max(slot_in_footprint) + 1`.
- Do not attempt a dense per-function local remap in the first milestone.

That choice keeps the first implementation small because it does not require rewriting the current lifetime/coloring passes, which still operate on one global local-slot space.

### Code layout

Compile the root body first, then append separately emitted script function bodies to the same `Program.code` blob:

1. Root statements
2. Root `ret`
3. Function body A
4. Function `ret`
5. Function body B
6. Function `ret`
7. ...

Execution still starts at `ip = 0`. For each supported capture-free named function, the compiler assigns a hidden callable binding initialized from Program metadata when the Program instance is installed. Appended function bodies are entered only through `CallValue`.

Preserve current source-order semantics: the parser registers a function before parsing its own body, which permits self-recursion, but statements before the declaration cannot use it. The new runtime binding must not hoist a declaration or create a capturing callable when control does not reach the declaration.

## Scope Split

This feature is safest as a staged rollout.

### Phase 1 scope

Support real script frames for direct named functions that do not require runtime capture environments:

- Top-level functions
- Nested functions with `capture_copies.is_empty()`
- `LocalCall` sites whose callable is statically known to be one of the above functions

Keep current inline lowering for:

- Closures
- Capturing nested `fn` declarations
- Any callable path that still depends on declaration-time capture snapshots

This already delivers the main win: function bodies stop being duplicated in bytecode for the most common direct-call cases.

### Later scope

Extend real frames to capturing nested `fn` declarations by adding declaration environments. That is a second feature, not part of the first milestone.

## Design Details

### 1. Bytecode and metadata

Files:

- `src/bytecode.rs`
- `src/assembler.rs`
- `src/vmbc.rs`
- `src/vm/jit/aot.rs`

Changes:

- Add `ScriptFunction` metadata to `Program`.
- Add only `OpCode::CallValue(u8)` in the real-frame milestone. Environment construction belongs to the later escaping-closure work and must be selected by capture requirements, not source naming.
- Keep existing `OpCode::Call(u16, u8)` unchanged for builtin/host imports.
- Update opcode encoding/decoding and assembler helpers.
- Update VMBC serialization/deserialization for the new function table.
- Bump the VMBC wire version.
- Bump `AOT_VERSION` if AOT bundles persist the expanded `Program` layout.

Recommendation:

- Keep `Program.imports` exactly for host imports.
- Add a separate `Program.script_functions`.
- Do not overload the existing import table to carry both concepts.

### 2. VM frame stack

Files:

- `src/vm/mod.rs`
- `src/vm/host.rs`

Add:

```rust
struct CallFrame {
    return_ip: usize,
    prev_frame_base: usize,
    prev_locals_len: usize,
    function_id: u16,
}
```

And VM state:

- `frame_base: usize`
- `call_frames: Vec<CallFrame>`
- `program_instance: ProgramInstanceId`
- an optional capture-free callable cache owned by the active VM/Program state

Execution model:

- `Ldloc index` resolves the current logical slot through the frame's slot layout.
- `Stloc index` stores through the same slot layout.
- Program installation initializes capture-free named callable bindings with the active `ProgramInstanceId` and prototype IDs.
- `CallValue argc`:
  - consume the callable and arguments
  - reject it with `StaleCallable` unless its Program instance ID equals the active one
  - resolve the callable prototype, then its target `ScriptFunction`
  - validate arity against the prototype
  - save caller frame state
  - extend `locals` by `frame_local_count`, filled with `Null`
  - set `frame_base` to the new frame start
  - move/copy stack args into `param_slots`
  - retain the callable as the active frame root
  - bind it to the function's metadata-designated self slot when the body recursively references its own name
  - jump `ip` to `entry_ip`
- `Ret`:
  - if `call_frames` is empty, halt
  - otherwise truncate locals to `prev_locals_len`, restore `frame_base`, set `ip = return_ip`

Notes:

- This integrates cleanly with the current `Value` stack model.
- Host/builtin calls still use the current stack-based ABI.
- Script calls never switch Programs. Replacing/resetting the Program assigns a new `ProgramInstanceId`, clears the callable cache and callback roots, and invalidates all old values.
- `call_depth()` should be redefined to report total logical call depth, not only host-call nesting.

### 3. Compiler lowering

Files:

- `src/compiler/codegen.rs`
- `src/compiler/pipeline.rs`
- `src/compiler/mod.rs`

Add a new compiler path for separately emitted script functions:

- Partition `FunctionImpl`s into:
  - `runtime_callable`: emit once as a `ScriptFunction`
  - `inline_only`: keep current behavior temporarily
- Record one Program-owned callable prototype and hidden binding for each supported capture-free named function; Program installation initializes those bindings.
- `compile_function_call` becomes:
  - builtin/host import used directly -> existing `Call`
  - supported RSS function/local callable -> `Ldloc` callable slot, compile args, `CallValue`
  - `inline_only` script function -> existing inline path
- After compiling root statements, emit all `runtime_callable` bodies once and record `entry_ip`.
- Keep parser declaration visibility source ordered; Program binding initialization must not make an earlier source reference valid.

Important compiler choice:

- The first implementation should keep current slot numbers inside function bodies.
- Use `collect_function_frame_slots` to compute `frame_local_count`.
- Do not attempt to renumber locals per function yet.

That avoids touching:

- parser local numbering
- lifetime availability
- liveness/coloring
- most type-inference slot bookkeeping

### 4. Function entry type state

Separate function-body emission cannot depend on caller-time `type_state`.

Files:

- `src/compiler/typing.rs`
- `src/compiler/typing/context.rs`
- `src/compiler/typing/collect.rs`
- `src/compiler/codegen.rs`

Required addition:

- Record a function-entry typing snapshot for each separately emitted script function.

At minimum this snapshot must provide:

- known parameter types
- optional/schema state for parameter slots
- callable bindings that are valid at function entry

Reason:

- current codegen uses `type_state` when emitting operand type hints and some type-directed lowering decisions
- inline codegen gets this state from the caller naturally; separate function emission does not

If a full `LocalTypeState` snapshot per function is too expensive initially, a narrower `FunctionEntryState` is acceptable as long as it seeds all type-directed codegen paths that exist today.

### 5. Capturing nested functions

This is the main semantic trap. Current nested `fn` declarations capture outer locals at declaration time, and inline lowering materializes those capture copies into the flat local space.

Recommendation for the first milestone:

- do not materialize capturing nested `fn` declarations in phase 1
- keep them inline-only until the environment/slot-layout design in `function_as_value_plan.md` is added

Recommended later design:

- use the shared environment-binding lowering from `function_as_value_plan.md` when a closure expression or nested declaration captures outer state
- execution snapshots captures into a fresh closure environment
- calling the resulting value uses the same `CallValue` frame path

That later work is closure-adjacent, even if callables are still not first-class `Value`s.

## Backend Strategy

### Interpreter first

Land the interpreter and compiler support before touching the native backends.

### Trace JIT

Files:

- `src/vm/jit/trace.rs`
- `src/vm/jit/runtime.rs`
- `src/vm/jit/native/bridge.rs`
- `src/vm/jit/native/codegen.rs`

First cut:

- mark `CallValue` as unsupported in trace recording/codegen
- bail out to the interpreter when a trace would include it

That keeps the feature deliverable without blocking on native frame support.

Later:

- teach `TraceStep` about script calls and returns
- add frame-stack aware native bridge/runtime handling

### Native AOT

First cut:

- either reject programs containing `CallValue` for native-only AOT
- or force interpreter fallback when indirect script calls are present

Later:

- add script frame metadata to the AOT bundle
- teach generated native code how to enter/leave script frames

## Debugger and tooling

Files:

- `src/debug_info.rs`
- `src/debugger/mod.rs`
- `src/bin/pd-vm-run.rs`

Current debug info is flat and local-index based. Real frames need frame-aware introspection.

Recommended additions:

- function entry/range metadata in `DebugInfo`
- a VM API to inspect the script call stack
- debugger views that show locals for the current frame, not the entire flat local array

This does not need to block the first executable version, but it should be part of the same effort before calling the feature complete.

## Public API and compatibility

Expected breaking or additive changes:

- `Program` grows a `script_functions` table
- VMBC version bump
- possibly AOT version bump
- `CompiledProgram.functions` should stop meaning "all non-inlined declared functions"

Recommended compiler API cleanup:

- keep `CompiledProgram.functions` for host-visible imports only, or rename it
- if script function metadata is exposed publicly, return it in a separate field

This avoids breaking CLI/runtime code that currently treats `CompiledProgram.functions` as host bindings to register.

## Rollout Plan

### Milestone 1: metadata and interpreter semantics

- Add Program-owned named callable prototypes/bindings, `CallValue`, `ScriptFunction`, and VM frame stack support
- Update `Ret` semantics
- Add direct unit tests for recursive and repeated script calls at the bytecode level
- Keep compiler unchanged except for a temporary bytecode constructor test path if needed

### Milestone 2: compiler emission for non-capturing functions

- Partition runtime-callable vs inline-only `FunctionImpl`s
- Emit root code plus appended function bodies
- Initialize one hidden binding per supported named function and emit `Ldloc` + `CallValue` at RSS call sites
- Preserve inline lowering for closures and capturing nested functions

### Milestone 3: typing and debug correctness

- Add function-entry type snapshots
- Restore operand type hints and any type-directed codegen inside separately emitted bodies
- Make debugger/frame inspection useful again

### Milestone 4: backend containment

- Make trace JIT and AOT explicitly reject or side-exit on `CallValue`
- Add tests to ensure no silent miscompile occurs when JIT/AOT sees it

### Milestone 5: capturing nested functions

- Add the shared environment-binding runtime path for every capture-bearing callable
- Migrate capturing nested `fn` declarations off the inline path
- Revisit closure alignment after nested `fn` semantics are stable

## Test Matrix

Add tests for:

- repeated direct calls do not duplicate function bytecode bodies
- direct recursion works
- mutual recursion works for script-callable functions
- locals are isolated between caller and callee frames
- returned values still flow through the existing operand stack
- host calls inside a script-called function still work
- host `Yield` and `Pending` inside a script-called function preserve the script call stack correctly
- inline-only paths still behave exactly as before
- JIT/AOT reject or fall back cleanly when `CallValue` appears

## Risks

### Risk 1: capture semantics creep

Trying to support capturing nested functions in the first patch will turn this into a closure-runtime project. Avoid that.

### Risk 2: type-state regressions

Separately emitted function bodies no longer inherit caller context. If function-entry typing is not restored, operand type hints and type-directed lowering will silently degrade.

### Risk 3: debugger confusion

Flat local displays will become misleading once multiple frames exist.

### Risk 4: backend skew

If interpreter support lands without explicit JIT/AOT rejection, native paths may mis-handle the new opcode.

## Recommendation

Implement this as an interpreter-first, non-capturing-function-first change.

That gives the project:

- real script call frames
- recursive direct function support
- elimination of bytecode duplication for common direct-call cases

without forcing the much larger "callable as real runtime value" design or full closure environments into the same patch series.
