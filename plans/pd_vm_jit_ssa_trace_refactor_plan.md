# pd-vm JIT SSA Trace Refactor Plan

## Summary

Replace the current trace recorder and trace IR with an SSA-based pipeline that keeps hot trace
values in typed virtual registers, lowers directly to Cranelift-friendly SSA, and only
materializes boxed `Value` state at guards, exits, calls, yields, and unsupported boundaries.

This is a replacement refactor, not an additive feature. The end state must not keep the old
`TraceStep` recorder path alive as a backward-compatibility layer.

Explicit policy for this work:

- no backward compatibility
- no legacy mode support
- no permanent runtime toggle between old trace IR and new trace IR
- no versioned internal trace formats
- no attempt to preserve `TraceStep` as a stable internal contract

The interpreter remains the semantic fallback for unsupported behavior, but the old trace
recording/lowering architecture does not survive the refactor.

## Why This Refactor Is Needed

The current native JIT already has useful type specialization, but the core trace model is still a
stack/local machine:

- `src/vm/jit/trace.rs` records `TraceStep`, which is fundamentally bytecode-shaped.
- `src/vm/jit/native/codegen.rs` lowers one step at a time and frequently reloads and
  restores `Value` slot state.
- `src/vm/native/inline.rs` still treats stack and local slots as the primary execution
  model, including repeated tag checks and `Value` copies.

That leaves the biggest performance win on the table:

- `Ldloc` and `Stloc` still represent VM memory traffic instead of symbolic dataflow.
- int and float fast paths still repeatedly cross the tagged `Value` boundary.
- loop traces do not yet have a single end-to-end SSA dataflow model that can stay unboxed across
  the whole hot path.
- the native backend is forced to reconstruct stack-machine behavior instead of consuming a cleaner
  IR.

LuaJIT-style wins come from recording typed SSA values and reconstructing boxed state only when the
trace must exit. That is the architectural change this plan targets.

## Goals

- Record traces into an SSA IR instead of a bytecode-shaped step list.
- Represent stack slots and locals symbolically during trace recording.
- Keep proven ints, floats, bools, and eligible references unboxed in the trace IR.
- Emit guards once, then reuse typed SSA values for downstream operations.
- Materialize boxed VM state only at deopt exits, calls, yields, helper fallbacks, and other hard
  boundaries.
- Keep loop backedges native.
- Preserve existing language semantics and interpreter correctness.
- Keep Cranelift as the machine-code backend and let Cranelift handle physical register allocation.

## Non-goals

- This is not a whole-program AOT plan.
- This is not a `Value` representation rewrite.
- This is not a NaN-boxing project.
- This is not a broad language-semantics expansion.
- This is not a permanent dual-maintenance plan for `TraceStep` and SSA traces.

## Target Architecture

### 1. New trace IR

Introduce a new IR under `src/vm/jit/` with explicit SSA value IDs and blocks.

Suggested new modules:

- `src/vm/jit/ir.rs`
- `src/vm/jit/recorder.rs`
- `src/vm/jit/deopt.rs`
- `src/vm/jit/liveness.rs`
- `src/vm/jit/native/lower.rs`

Core IR concepts:

- `TraceId`
- `TraceBlockId`
- `TraceValueId`
- `TraceExitId`
- `TraceType`
- `TraceInst`
- `TraceGuard`
- `TraceSnapshot`
- `TraceMaterialization`

The IR should be block-based even if the first supported shape is still "hot loop plus exits". Do
not build another linear-only IR that will need a second rewrite to model header phis, loop-carried
values, or branch-local value versions.

### 2. Symbolic stack and locals during recording

The recorder should stop treating `stack: Vec<Value>` and `locals: Vec<Value>` as the working trace
representation.

Instead, the recorder maintains:

- symbolic operand stack: `Vec<TraceValueId>`
- symbolic locals: `Vec<TraceValueId>`
- value metadata for each SSA ID:
  - representation
  - guard provenance
  - materialization rule
  - side-effect dependencies if needed

`Ldloc` becomes "read current SSA value bound to local N", not "generate a future load from VM
memory". `Stloc` becomes "rebind local N to SSA value X", not "generate a future store immediately".

### 3. Typed value representations

Use explicit IR representations so the recorder and lowerer can stay unboxed as long as possible.

Initial representation set:

- `TaggedValue`
- `I64`
- `F64`
- `Bool`
- `HeapPtr { tag }`

The initial scope should be conservative:

- ints, floats, bools first
- immutable string/bytes/array/map references second
- helper fallback for cases that still require generic boxed execution

Do not try to model every future specialization in the first pass.

### 4. Guard and deopt model

Every assumption that lets the trace stay unboxed must be tied to an exit snapshot.

Required behavior:

- tag guards produce typed SSA values
- overflow-sensitive ops either guard or exit
- division/modulo zero checks exit
- unsupported builtin/call/type cases exit or flush then call
- each exit carries a full materialization plan for live stack and local state

The deopt contract should be:

1. Reconstruct VM `stack`, `locals`, and `ip` exactly.
2. Return to the existing interpreter/runtime resume path.
3. Preserve drop/clone behavior and all visible side effects.

### 5. Materialization policy

Materialization must be explicit and lazy.

Do not eagerly box results after each op. Instead:

- symbolic stack/local state references SSA values
- boxing happens only when an exit path needs real VM state
- calls and helper bridges flush live values before control leaves the native trace domain
- loop backedges stay in SSA form when possible

This is the core rule of the refactor. If a design choice forces boxing at every arithmetic or
local-update step, it is the wrong design.

### 6. Effect ordering

SSA for scalars is not enough on its own because the trace still has ordered side effects:

- host and builtin calls
- collection helpers
- drop-contract event accounting
- writes that become externally visible before the next guard

Use a simple explicit effect token chain if needed. Full memory SSA is not required initially, but
the IR must have a clear way to represent ordered effects without collapsing everything back into a
step machine.

## Implementation Plan

### Phase 1. Diagnostics and invariants first

- Add trace debug dumps for current recorder output, guard exits, and native lower output.
- Add counters for:
  - boxed loads from VM stack/locals
  - boxed stores back to VM stack/locals
  - guard exits
  - helper fallback count
  - native loop-back count
- Add benchmark fixtures for integer-heavy loops and mixed-type guard churn.

Deliverable:

- a stable before/after measurement harness for boxed traffic and trace exits

### Phase 2. Introduce the new IR in parallel with no public compatibility promise

- Add the new SSA IR types and builders.
- Add pretty-print/debug output for the new IR.
- Add a verifier:
  - SSA defs dominate uses
  - block parameters are type-consistent
  - exits reference only live values
  - materialization plans are complete

Important:

- This phase may temporarily coexist in source with `TraceStep`, but only as implementation
  scaffolding while the cutover is incomplete.
- It must not become a supported permanent dual path.

### Phase 3. Rewrite the trace recorder as an abstract interpreter

- Replace step-oriented recording with symbolic execution over bytecode.
- Track the current symbolic stack and locals at each bytecode position inside the trace.
- Create SSA values for:
  - constants
  - local reads
  - arithmetic ops
  - comparisons
  - boolean logic
  - loop-carried values
- Insert guards where type stability is required to remain unboxed.
- Introduce block parameters or phis for loop headers and in-trace joins.

Priority workload:

- Lua integer loop kernels
- typed local arithmetic updates
- compare-and-branch loop control

### Phase 4. Build deopt snapshots and materialization

- Define how each `TraceValueId` becomes a boxed `Value` when required.
- Build exit snapshots for all guards and hard exits.
- Handle loop exits, side exits, and helper-call exits uniformly.
- Ensure `ip`, stack length, local bindings, and pending side effects are reconstructed exactly.

Special attention:

- `Arc`-backed heap values must keep correct ownership semantics.
- overwrite/drop accounting must remain correct when locals or stack slots are re-bound symbolically.
- values that never need boxing on the hot path must still box correctly on exits.

### Phase 5. Replace per-step native lowering with SSA lowering

- Add a lowerer from the new IR to Cranelift under `src/vm/jit/native/lower.rs`.
- Stop lowering through the old `emit_inline_or_helper_step` shape for the new traces.
- Map:
  - SSA ints -> Cranelift integer values
  - SSA floats -> Cranelift float values
  - SSA bools -> Cranelift booleans/i8 as appropriate
  - boxed or heap-backed values -> tagged pointers / materialization helpers as needed
- Lower guards into side exits using the new snapshots.
- Lower loop backedges as native block jumps, not VM exits.

Cranelift remains responsible for physical register allocation. The refactor goal is to feed
Cranelift a trace IR with clean SSA value lifetimes, not to build a custom regalloc.

### Phase 6. Runtime cutover

- Switch the JIT runtime to consume the new trace artifact.
- Remove code paths that interpret `TraceStep` as the primary trace contract.
- Keep interpreter fallback for non-traced or unsupported behavior.
- Update trace compile caches and metadata to store only the new trace type.

Required cutover rule:

- once the new recorder and lowerer are correct for the supported trace set, delete the old trace
  IR path instead of keeping a "legacy trace backend"

### Phase 7. Delete obsolete trace infrastructure

- Remove `TraceStep`-specific optimization passes that only exist to compensate for the old
  bytecode-shaped IR.
- Remove dead helper glue that only exists to bridge old trace artifacts into native lowering.
- Shrink or split `src/vm/jit/trace.rs` and `src/vm/jit/native/codegen.rs` around the
  new module boundaries.
- Re-run benchmarks and update docs/plans that still describe the old architecture as current.

## Code Shape After Refactor

Suggested target ownership:

- `src/vm/jit/ir.rs`
  - trace IR data structures
  - verifier
  - debug printer
- `src/vm/jit/recorder.rs`
  - hot-loop recording
  - abstract interpretation
  - guard insertion
  - block construction
- `src/vm/jit/deopt.rs`
  - exit snapshots
  - materialization planning
  - deopt metadata
- `src/vm/jit/native/lower.rs`
  - Cranelift lowering from SSA IR
- `src/vm/jit/runtime.rs`
  - runtime integration
  - trace execution and side-exit resume

Likely removals or major shrinkage:

- `src/vm/jit/trace.rs`
- `src/vm/jit/native/codegen.rs`

## Testing Plan

- Unit tests for IR construction and SSA verification.
- Unit tests for symbolic stack/local rebinding.
- Unit tests for guard snapshot completeness.
- Unit tests for materialization of:
  - int
  - float
  - bool
  - heap reference values
- Native JIT tests that validate:
  - loop locals stay unboxed across backedges
  - `Ldloc`/`Stloc`-heavy traces stop generating per-step VM slot traffic
  - guards reconstruct interpreter-visible state exactly
  - helper/call boundaries flush live values correctly
- Benchmark checks for:
  - reduced stack/local memory traffic
  - fewer helper fallbacks on numeric loops
  - native loop-back execution without VM re-entry on the hot path

## Acceptance Criteria

- The supported numeric-loop benchmark traces keep loop-carried scalars in SSA form for the entire
  hot region.
- Generated native code no longer reloads and reboxes locals on each arithmetic step.
- `JumpToRoot`-style loop behavior is represented as native control flow, not a VM round-trip.
- Guard exits resume correctly with fully reconstructed boxed VM state.
- The new SSA recorder is the only supported trace recorder in-tree.
- No legacy compatibility mode remains for the old trace IR.

## Main Risks

### 1. Incorrect deopt state

The highest risk is reconstructing the wrong stack/local state at a guard exit. This must be
verified aggressively before chasing benchmark wins.

### 2. Ownership and drop semantics

This VM stores heap values in `Arc`-backed `Value` variants. Keeping references unboxed in the
trace is only safe if clone/drop behavior at materialization and overwrite points stays exact.

### 3. Refactor sprawl

`trace.rs`, `runtime.rs`, and `native/codegen.rs` are already large. Without early module splits,
the rewrite will become hard to reason about and harder to delete cleanly.

### 4. Overbuilding the first version

The first cut should target integer and float loop kernels plus the minimum deopt model needed for
correctness. Do not block the refactor on fully general heap-effect optimization.

## Recommended Implementation Order

1. Add diagnostics and IR verifier.
2. Add SSA IR types and debug printer.
3. Rewrite recorder for numeric loop traces.
4. Add deopt snapshots and materialization.
5. Add SSA native lowering.
6. Cut runtime over to the new artifact.
7. Delete the old trace IR path.

## Bottom Line

The right architectural move is to stop treating the trace recorder as a smarter bytecode copier.
It needs to become an abstract interpreter that records typed SSA values, preserves them across the
whole hot trace, and reconstructs boxed VM state only when leaving the trace.

That is a whole-refactor project, and it should be executed as a replacement, not a compatibility
exercise.
