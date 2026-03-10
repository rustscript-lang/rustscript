# Implementation Plan: Real Script Call Frames

## Goal

Introduce real runtime call frames for script-defined `fn` declarations so a function body is compiled once and invoked many times without duplicating its bytecode at each call site.

This plan is only for real script calls and frames.

Non-goals for this effort:

- First-class callable `Value`s
- `call_indirect`-style dynamic dispatch
- Returning/storing callables in arrays or maps
- Full closure/runtime environment objects in the first milestone
- Tail-call optimization

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

Add real script frames while keeping host/builtin calls unchanged:

- Keep `OpCode::Call` for builtin/host import dispatch.
- Add a new `OpCode::CallScript` with operands `(script_function_id: u16, argc: u8)`.
- Reuse `OpCode::Ret`:
  - if no script caller exists, halt the program
  - otherwise pop one script frame, restore the caller frame, and continue at the saved return IP
- Add a VM-side `CallFrame` stack.

### Program model

Add a script-function table to `Program`:

```rust
pub struct ScriptFunction {
    pub decl_index: u16,
    pub name: String,
    pub arity: u8,
    pub entry_ip: u32,
    pub frame_local_count: u16,
    pub param_slots: Vec<u8>,
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

Execution still starts at `ip = 0`. Appended function bodies are only reachable through `CallScript`.

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

- `pd-vm/src/bytecode.rs`
- `pd-vm/src/assembler.rs`
- `pd-vm/src/vmbc.rs`
- `pd-vm/src/vm/jit/aot.rs`

Changes:

- Add `ScriptFunction` metadata to `Program`.
- Add `OpCode::CallScript`.
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

- `pd-vm/src/vm/mod.rs`
- `pd-vm/src/vm/host.rs`

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

Execution model:

- `Ldloc index` reads `locals[frame_base + index]`
- `Stloc index` writes `locals[frame_base + index]`
- `CallScript`:
  - validate arity against `Program.script_functions`
  - save caller frame state
  - extend `locals` by `frame_local_count`, filled with `Null`
  - set `frame_base` to the new frame start
  - move/copy stack args into `param_slots`
  - jump `ip` to `entry_ip`
- `Ret`:
  - if `call_frames` is empty, halt
  - otherwise truncate locals to `prev_locals_len`, restore `frame_base`, set `ip = return_ip`

Notes:

- This integrates cleanly with the current `Value` stack model.
- Host/builtin calls still use the current stack-based ABI.
- `call_depth()` should be redefined to report total logical call depth, not only host-call nesting.

### 3. Compiler lowering

Files:

- `pd-vm/src/compiler/codegen.rs`
- `pd-vm/src/compiler/pipeline.rs`
- `pd-vm/src/compiler/mod.rs`

Add a new compiler path for separately emitted script functions:

- Partition `FunctionImpl`s into:
  - `script_callable`: emit once, invoked by `CallScript`
  - `inline_only`: keep current behavior
- `compile_function_call` becomes:
  - builtin/host import -> `Call`
  - script function in `script_callable` -> `CallScript`
  - script function in `inline_only` -> existing inline path
- After compiling root statements, emit all `script_callable` bodies once and record `entry_ip`

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

- `pd-vm/src/compiler/typing.rs`
- `pd-vm/src/compiler/typing/context.rs`
- `pd-vm/src/compiler/typing/collect.rs`
- `pd-vm/src/compiler/codegen.rs`

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

- do not move capturing `fn` declarations to `CallScript`
- keep them inline-only until a declaration-environment design is added

Recommended later design:

- add a per-activation declaration environment table, keyed by function declaration index
- executing `Stmt::FuncDecl` snapshots the captures into the current activation
- calling a capturing nested function loads its declaration environment into the callee frame before jumping

That later work is closure-adjacent, even if callables are still not first-class `Value`s.

## Backend Strategy

### Interpreter first

Land the interpreter and compiler support before touching the native backends.

### Trace JIT

Files:

- `pd-vm/src/vm/jit/trace.rs`
- `pd-vm/src/vm/jit/runtime.rs`
- `pd-vm/src/vm/jit/native/bridge.rs`
- `pd-vm/src/vm/jit/native/codegen.rs`

First cut:

- mark `CallScript` as unsupported in trace recording/codegen
- bail out to the interpreter when a trace would include `CallScript`

That keeps the feature deliverable without blocking on native frame support.

Later:

- teach `TraceStep` about script calls and returns
- add frame-stack aware native bridge/runtime handling

### Native AOT

First cut:

- either reject programs containing `CallScript` for native-only AOT
- or force interpreter fallback when script-call opcodes are present

Later:

- add script frame metadata to the AOT bundle
- teach generated native code how to enter/leave script frames

## Debugger and tooling

Files:

- `pd-vm/src/debug_info.rs`
- `pd-vm/src/debugger/mod.rs`
- `pd-vm/src/bin/pd-vm-run.rs`

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

- Add `CallScript`, `ScriptFunction`, and VM frame stack support
- Update `Ret` semantics
- Add direct unit tests for recursive and repeated script calls at the bytecode level
- Keep compiler unchanged except for a temporary bytecode constructor test path if needed

### Milestone 2: compiler emission for non-capturing functions

- Partition script-callable vs inline-only `FunctionImpl`s
- Emit root code plus appended function bodies
- Emit `CallScript` for direct and statically-known local calls to script-callable functions
- Preserve inline lowering for closures and capturing nested functions

### Milestone 3: typing and debug correctness

- Add function-entry type snapshots
- Restore operand type hints and any type-directed codegen inside separately emitted bodies
- Make debugger/frame inspection useful again

### Milestone 4: backend containment

- Make trace JIT and AOT explicitly reject or side-exit on `CallScript`
- Add tests to ensure no silent miscompile occurs when JIT/AOT sees the new opcode

### Milestone 5: capturing nested functions

- Add declaration environments
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
- JIT/AOT reject or fall back cleanly when `CallScript` appears

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
