# Function as Value Implementation Plan

**Goal:** Add first-class RSS function values with Rust-like semantics, existing RustScript capture behavior, Program-owned definitions, generic typing, deterministic lifecycle, and Rust-host callbacks.

## 1. Fix the semantic contract

**Target:** Define behavior before choosing runtime instructions.

**Complete:**

- Add characterization tests for named functions, closure expressions, nested captures, recursion, assignment, return, containers, and generics.
- Model a capture-free named `fn` as a Program-owned function item. Model a capture-bearing value as a closure with an environment. Preserve capturing nested named functions as a RustScript extension.
- Select runtime representation from capture/environment requirements, never from `named` or `anonymous` source spelling.
- Keep the model close to Rust: function-item identity remains available to inference; closure instances preserve capture state; `TypeSchema::Callable` remains Fn-like until distinct function-pointer and `Fn`/`FnMut`/`FnOnce` types exist.
- Document the implementation comparison: Rust separates function items, pointers, and closures; CLR uses methods/function pointers plus delegate/environment objects; Lua instantiates function prototypes with `OP_CLOSURE`.

**Acceptance:** Tests define identity, capture sharing, mutation, recursion, coercion, and diagnostics without assuming a closure opcode.

## 2. Implement callable values and real frames

**Target:** Execute function values without retaining old Programs.

**Complete:**

- Add Program-owned callable prototypes, script entry points, parameter/local layouts, capture layouts, source metadata, and instantiated generic signatures.
- Add Program-owned `Value::Callable { prototype_id, kind, env }`. Callable values follow their Program/Store lifetime and never retain an old `Program`.
- Add frame-relative locals, a real execution-frame stack, and one `Ret` rule: complete the active frame and follow its typed continuation. Root execution uses an explicit `Halt` continuation; nested RSS calls use `ResumeBytecode`; Rust invocation uses `ReturnToHost`. Never infer return meaning from an empty call stack.
- Add `CallValue(argc)` as the common RSS function-item/closure invocation path; retain existing `Call` for direct host/builtin imports.
- Initialize capture-free named bindings from Program metadata.
- Add one shared environment-binding path for every capture-bearing callable. Introduce a dedicated runtime operation only if existing VM operations cannot bind captures.
- Keep callable values inside their owning Program/Store. Program replacement and reset invalidate the old callable registry as a lifecycle operation; callable dispatch does not compare stale identities.

**Acceptance:** Named calls, recursion, closure calls, arity failures, return values, yield/pending, and resume all use real frames.

## 3. Integrate captures, types, and generics

**Target:** Preserve lifecycle analysis while allowing callable values to flow through the compiler.

**Complete:**

- Convert current `CaptureBindingMode::{Copy, Borrow, BorrowMut, Move}` analysis into explicit environment layouts:
  - `Copy` stores an independent value in the closure environment;
  - `Borrow` and `BorrowMut` hoist the source local into one shared cell used by the outer frame and every capturing closure, matching C# display-class sharing and Rust reference-capture behavior;
  - `Move` transfers the value into a closure-owned cell and invalidates the source according to existing move rules.
- Route `Ldloc`/`Stloc` through frame or captured cells. Aliases of one closure share cells, separate closure evaluations receive separate environments, and outer-frame reads/writes observe borrowed capture updates.
- Add coarse runtime callable typing and precise parameter/result/capture schemas.
- Merge compatible callable signatures across branches, matches, and loops; keep exact target identity as inference/optimization metadata only.
- Support explicit `name::<T>` values, unambiguous contextual resolution, substituted schemas, generic higher-order functions, and erased runtime bodies.
- Add focused errors for unresolved generics, incompatible signatures, invalid calls, callable map keys, and callable constant serialization.

**Acceptance:** Escaping closures, mutable captures, branch merges, strict/dynamic checks, and generic function values pass compiler and runtime tests.

## 4. Complete lifecycle and Rust callback APIs

**Target:** Support host-retained UI/event callbacks and Program replacement safely.

**Complete:**

- Release environments correctly on clone, overwrite, `Drop`, unwind, collection removal, return, cancellation, and reset; reject ownership cycles until cycle collection exists.
- On Program replacement, REPL installation, or reuse reset: cancel callback work, remove listeners, clear callable roots/persisted callables, drop the old Program, and invalidate all external old handles.
- Add Rust APIs to resolve exported RSS functions and invoke function items or closures through one resumable path.
- Add typed `ScriptCallback<Args, Ret>` with Store ownership and copied callable schema. It follows the Program registry and stores no stale Program identity.
- Serialize callbacks through the Store queue; define FIFO/coalescing, cross-thread enqueue, unsubscribe, teardown, error, and return-value policies.
- Verify that the final Rust-held callback releases captures exactly once.

**Acceptance:** A Rust UI host can retain, invoke, suspend/resume, unsubscribe, and invalidate RSS callbacks without retaining old executable code.

## 5. Finish backend parity and verification

**Target:** Apply one semantic contract across every supported runtime path.

**Complete:**

- Apply a hard internal format break: bump VMBC, AOT artifact and native ABI, debugger recording, and Program/native trace cache revisions; accept only the new formats, regenerate fixtures, and add no backward decoder or migration path.
- Audit existing opcode interactions: keep `Call` host-only, constrain branches to one root/function region, resolve `Ldloc`/`Stloc` through one slot-location map, define callable `Ceq` identity and `Pop`/`Dup` lifecycle, reject callable `Ldc` constants, and remove or redesign the current `Call + Ret` fusion.
- Implement callable creation, environment access, `CallValue`, script frame entry/return, recursion, errors, and suspension/resume completely in both Trace JIT and AOT.
- Trace JIT must record and compile through callable operations and callee returns. `CallValue`, closure environments, and callable frame transitions must not terminate recording, produce a feature side exit, or hand execution to the interpreter.
- Lower dynamic callable dispatch to native target resolution/calls that return to the same compiled trace. AOT must emit equivalent callable/frame/environment operations with no rejection or interpreter fallback.
- Update REPL replacement, debugger frames, stack traces, formatting, recording/replay policy, and diagnostics.
- Replace old function-value rejection tests only after equivalent positive and focused negative coverage exists.
- Run focused stage tests, native trace-continuity and AOT parity tests, then `cargo fmt --all -- --check`, callable/compiler/type/lifecycle/wire/no-std suites, `cargo test --workspace`, Clippy with `-D warnings`, and the release build.

**Acceptance:** Interpreter, Trace JIT, and AOT produce identical results and lifecycle behavior. Callable creation, invocation, nested calls, recursion, captures, and returns remain inside native execution without feature-induced side exits, trace interruption, rejection, or interpreter fallback.
