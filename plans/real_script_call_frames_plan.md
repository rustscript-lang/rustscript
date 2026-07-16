# Real Script Call Frames Implementation Plan

**Goal:** Compile every RSS function body once and execute direct calls, function values, closures, recursion, suspension, and Rust invocation through one real-frame model with identical interpreter, Trace JIT, and AOT behavior.

First-class value typing, generic schemas, lifecycle, and retained callback APIs are completed by [function_as_value_plan.md](function_as_value_plan.md). This plan defines the shared bytecode/frame contract they use.

## 1. Freeze the opcode and return contracts

**Target:** Add one opcode and remove context-dependent return semantics.

**Complete:**

- Keep `Call(index: u16, argc: u8)` exclusively for direct builtin/host-import dispatch.
- Add only `CallValue(argc: u8)` for every RSS function-item/closure invocation and host function used as a value.
- Define the `CallValue` stack ABI: before execution the stack tail is `callee, arg0, ..., argN`; the opcode removes that segment, enters the selected target, and eventually replaces it with exactly one result (`Null` for unit).
- Give `Ret` one semantic: complete the active execution frame and transfer its normalized result to that frame's continuation.
- Represent root execution explicitly as a frame whose continuation is `Halt`; represent nested RSS calls with `ResumeBytecode { return_ip, ... }`; represent Rust/API invocation roots with `ReturnToHost`.
- Never define `Ret` by checking whether the script-frame stack happens to be empty. Root `Ret` produces `VmStatus::Halted` because its explicit continuation says so.
- Reuse existing root bytecode ending in `Ret`; no `Halt` or `CallScript` opcode is added.
- Treat `CallValue` as a cross-cutting bytecode ABI change, with these required edits:
  - opcode definition/decoding: `src/bytecode.rs` (`OpCode`, `TryFrom<u8>`, operand length, mnemonic, decoded instruction metadata);
  - assembly: `src/assembler.rs` builders, textual parser, disassembler output, truncation/error tests;
  - Program/value model: callable prototypes, script-function regions, `Value::Callable`, `ValueType::Callable`, clone/drop/equality/formatting, Program cache hashing;
  - compiler: `src/compiler/ir.rs`, `codegen.rs`, `pipeline.rs`, typing, lifetime/capture analysis, linker remapping, source/debug metadata;
  - interpreter/runtime: `src/vm/mod.rs`, `host.rs`, `store.rs`, frame entry/return, suspension/resume, reset/replacement, Rust invocation roots;
  - Trace JIT: recorder/decoder, trace IR, SSA/liveness/deopt, native lowering, runtime dispatch, cache keys, diagnostics and trace-continuity counters;
  - AOT: CFG function regions, IR/SSA, native compile/runtime bridges, artifact metadata, dynamic target dispatch and continuation handling;
  - tooling: VMBC validation, no-std decode/execution, debugger/recording/replay, CLI inspection, REPL state, tests and fixtures.
- Apply a hard internal format break; no backward decoder, migration, opcode reinterpretation, or compatibility branch is required:
  - bump VMBC `ENCODE_VERSION` from 8 to 9 and accept only version 9;
  - bump AOT artifact `VERSION` from 2 to 3 and native `ABI_VERSION` from 1 to 2 because Program/VM/native layouts and return control flow change;
  - bump debugger recording `PDRC` from 2 to 3, accept only version 3, and remove legacy version-1 recording acceptance;
  - add the bytecode ABI revision to Program/native trace cache keys together with callable tables, function regions, and slot layouts so old in-memory entries cannot match;
  - regenerate internal fixtures/artifacts and make every old format fail with its existing unsupported-version error.

**Acceptance:** `Ret` has one frame-completion rule in the interpreter, Trace IR, AOT IR, debugger, and public invocation API; root halt and nested return are selected only by typed continuation metadata.

## 2. Add Program-owned targets and real frames

**Target:** Separate executable definitions from runtime callable instances without retaining old Programs.

**Complete:**

- Add `Program.script_functions` and `Program.callable_prototypes` containing entry IP, arity, frame-local count, parameter slots, slot-location layout, capture layout, source metadata, and instantiated generic signature.
- Add `Value::Callable { program_instance, prototype_id, kind, env }`; it stores no `Arc<Program>`.
- Assign a fresh `ProgramInstanceId` on Program installation/replacement/reset. `CallValue` validates it before prototype lookup and returns `StaleCallable` for old handles.
- Add an execution-frame stack. Each frame records:
  - typed continuation (`Halt`, `ResumeBytecode`, or `ReturnToHost`);
  - prototype/function identity;
  - frame base and local extent;
  - operand-stack base;
  - active callable/environment root;
  - recursion, fuel/epoch, suspension, and debug state required for exact resume.
- Resolve logical locals through metadata such as `SlotLocation::Frame(offset)` or `SlotLocation::Capture(cell)`. A logical slot must map to exactly one storage class in each prototype.
- On `CallValue`, validate value kind, Program instance, arity, and recursion limit; preserve caller state; allocate callee locals; bind arguments/self/environment; then enter the target.
- On `Ret`, normalize the result, release callee roots/locals, restore the typed continuation, and either resume bytecode, return to Rust, or report halt.
- Emit root code and every RSS function body once in one code blob, with explicit root/function regions.
- Validate `Br` and `Brfalse` targets remain inside their current root/function region; cross-function control flow is valid only through `CallValue` and `Ret`.

**Acceptance:** Repeated calls share one body; direct recursion, mutual recursion, nested calls, local isolation, return values, errors, yield/pending, cancellation, and resume preserve exact frame and ownership state.

## 3. Lower every RSS callable path to the shared ABI

**Target:** Remove script-body inlining as a runtime fallback while preserving capture behavior.

**Complete:**

- Record one Program-owned prototype for every function item or closure body and one script-function entry for every emitted RSS body.
- Initialize capture-free named bindings from Program metadata without a creation opcode.
- Lower direct or indirect RSS calls by loading the applicable callable value and emitting `CallValue`; keep `Call` only for direct host/builtin imports.
- Preserve source-order visibility: Program binding initialization must not make a reference before its declaration valid.
- Bind recursive self-reference through prototype/frame metadata, not a separate opcode.
- Convert `capture_copies` into environment/slot layouts while preserving `CaptureBindingMode`:
  - `Copy` receives an independent captured value;
  - `Borrow`/`BorrowMut` share one cell with the outer frame and aliases;
  - `Move` transfers the value into the environment and invalidates the source.
- Give separate closure evaluations separate environments and aliases of one callable the same environment.
- Seed separately emitted bodies from function-entry typing snapshots instead of caller-time `type_state`.
- Keep generic substitution in schemas/prototypes and runtime dispatch erased.
- Allow implementation sequencing internally, but do not mark the feature complete while any RSS function/closure call still depends on inline-body fallback.

**Acceptance:** Named functions, closure expressions, capturing nested functions, returned/stored callables, branch/loop merges, generics, recursion, and escaping environments all use `CallValue` plus real frames.

## 4. Resolve interactions with existing opcodes and native backends

**Target:** Prevent hidden semantic conflicts and keep calls inside native execution.

**Complete:**

- Audit existing opcodes under callable values:
  - `Call` retains its host-import index namespace and never dispatches RSS prototypes;
  - `Ldloc`/`Stloc` use the current prototype's unambiguous slot-location map;
  - `Br`/`Brfalse` cannot cross root/function regions;
  - `Ldc` cannot deserialize Program-bound callable constants;
  - `Pop` and `Dup` apply normal callable/environment clone/drop ownership;
  - `Ceq` compares function items by Program instance/prototype identity and closure aliases by callable/environment identity; separate closure evaluations compare unequal;
  - arithmetic, bitwise, ordering, and shift opcodes reject callable operands through existing type-error paths.
- Remove or redesign the current interpreter `Call + Ret` fusion. It may run only when typed continuation/frame semantics, fuel ticks, epoch checks, result normalization, and lifecycle behavior remain identical to unfused execution.
- Change the current interpreter mapping from unconditional `Ret -> Halted` to frame completion.
- Change AOT CFG/SSA so `Ret` terminates the current function region and returns to its native continuation; only root continuation reports halt.
- Change Trace JIT so `CallValue`, callee execution, and `Ret` remain in one compiled trace; recording resumes at the caller continuation after return.
- Lower dynamic callable target resolution to native dispatch. Target variation, nested calls, captures, errors, and suspension must not produce feature side exits, trace interruption, interpreter handoff, or AOT rejection.
- Preserve frame bases, locals, operand state, callable/environment roots, fuel/epoch accounting, host-call continuation, and suspension state in all three backends.

**Acceptance:** Opcode behavior is determined by explicit Program/frame metadata, and interpreter, Trace JIT, and AOT produce identical values, errors, ticks, traces, and drops without callable-induced fallback.

## 5. Complete tooling and verification

**Target:** Make the new frame model observable, version-safe, and releasable.

**Complete:**

- Update assembler/disassembler, operand decoding, VMBC validation, no-std execution, AOT bundles, REPL replacement, recording/replay, formatter output, and source diagnostics.
- Add function-region/prototype/frame data to debug info, stack traces, current-frame local inspection, and Rust invocation status.
- Add focused tests for root `Ret`, nested `Ret`, Rust-root `Ret`, `CallValue` stack shape, stale handles, arity/type errors, region-confined branches, slot-location validation, callable equality, `Pop`/`Dup` lifecycle, and rewritten `Call + Ret` optimization.
- Add interpreter/JIT/AOT parity tests for direct/dynamic calls, recursion, captures, host calls, errors, yield/pending/resume, cancellation, reset, and exact drop counts.
- Instrument native tests to assert zero callable-induced side exits, trace breaks, interpreter handoffs, and rejected valid AOT Programs.
- Run focused suites during implementation, then formatting, workspace tests, Clippy with `-D warnings`, and release builds.

**Acceptance:** All RSS callable paths use real frames, `Ret` has no context-dependent bytecode meaning, existing opcodes have explicit callable/frame contracts, and every supported backend passes the same behavior and lifecycle gates.
