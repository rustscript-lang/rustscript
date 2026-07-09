# pd-vm AOT SSA Compile Plan

## Summary

Extend whole-program AOT compilation to use an SSA-based IR and SSA-based Cranelift lowering,
instead of lowering a stack-machine `AotInstruction` stream one step at a time.

The goal is the same architectural win the trace JIT is already pursuing:

- keep hot locals and stack values symbolic and typed
- lower normal branches and loop backedges as native CFG edges
- only box or flush VM state at real boundaries:
  - host or builtin calls
  - yield or pending exits
  - fuel or epoch interruption exits at host call boundaries
  - hard error exits
  - explicit resume checkpoints

This should be implemented as an AOT-specific SSA pipeline under `src/vm/aot/`, not by
forcing whole-program AOT through the trace-specific `jit::ir::SsaTrace` model.

Explicit policy for this work:

- this is a one-way refactor
- no backward compatibility promise for the old segment-based internal compiler pipeline
- no deprecation shims, legacy toggles, or compatibility branches in the final design
- no permanent dual-maintenance of stack-machine AOT lowering and SSA AOT lowering
- AOT epoch interruption is only checked at host call boundaries; `epoch_check_interval` is
  ignored for AOT execution
- AOT fuel interruption is only checked at host call boundaries; `fuel_check_interval` is ignored
  for AOT execution
- if the persisted `.pat` format must change, bump the artifact version and fail older artifacts
  cleanly instead of carrying a compatibility decoder

## Current Baseline

Today the AOT path already has a real CFG, but it does not keep execution state in SSA:

- `src/vm/aot/cfg.rs` builds bytecode basic blocks.
- `src/vm/aot/ir.rs` lowers those blocks into `AotIrBlock` plus stack-machine
  `AotInstruction`.
- `src/vm/aot/compile.rs` then:
  - splits blocks into per-IP `AotSegment`s
  - dispatches by `vm.ip`
  - emits each step through `emit_step`
  - handles branches through `emit_terminal`

That design preserves arbitrary resume points, but it also keeps the AOT backend tied to:

- boxed VM stack and local traffic
- bytecode-step execution order as the main lowering unit
- helper-driven branch semantics such as `OP_JUMP` and `OP_GUARD_FALSE`
- `build_segments` as a mandatory layer between CFG and machine code

In contrast, the trace JIT already has the right conceptual pieces:

- typed SSA values in `src/vm/jit/ir.rs`
- block params and SSA terminators
- exit materialization plans
- direct SSA-to-Cranelift lowering in `src/vm/jit/native/lower.rs`

The AOT plan should reuse those ideas, and selectively extract neutral helpers, without trying to
reuse trace-specific exit semantics wholesale.

## Design Position

### 1. Do not reuse `SsaTrace` directly

`SsaTrace` is still trace-oriented:

- it assumes one root trace
- it encodes trace exits and linked-trace handoff
- its runtime contract is "deopt or chain", not "resume arbitrary whole-program IP"

Whole-program AOT needs:

- normal CFG control flow for the entire program
- explicit resume checkpoints keyed by bytecode IP
- call, yield, pending, and interruption boundaries that return to the existing VM runtime

The right shape is:

- keep AOT-specific orchestration in `vm/aot`
- extract only neutral SSA pieces if duplication becomes real

### 2. Preserve the current AOT runtime contract first

The public/runtime contract already exists and should remain stable during the refactor:

- `Vm::compile_aot`
- `Vm::aot_resume_ips`
- `Vm::execute_aot_entry`
- `.pat` artifact encode/load in `src/vm/aot/artifact.rs`

That means the SSA design must preserve resumability by `vm.ip`, even when hot-path execution no
longer mutates boxed VM state after every bytecode step.

Intentional runtime-policy changes for this refactor:

- fuel checks happen only at host call boundaries, not at configurable interval checkpoints inside
  native loops
- epoch checks happen only at host call boundaries, not at configurable interval checkpoints inside
  native loops

### 3. The key mechanism is checkpointed SSA, not per-step lowering

The AOT SSA path should not resume by re-entering the middle of generated machine instructions.
Instead, it should resume through explicit checkpoint blocks:

- each resumable bytecode IP maps to a small native entry block
- that block loads boxed VM stack and local state into SSA values
- it jumps into the SSA continuation block for that logical program point

This preserves the current resume table while allowing the hot path between checkpoints to stay in
SSA form.

## Target Architecture

### AOT SSA IR

Add a new IR under `src/vm/aot/`, for example:

- `aot/ssa.rs`
- or `aot/ir/ssa.rs` if the module split grows

Suggested core types:

- `AotSsaProgram`
- `AotSsaBlockId`
- `AotSsaValueId`
- `AotSsaValueRepr`
- `AotSsaInst`
- `AotSsaTerminator`
- `AotCheckpointId`
- `AotCheckpointSnapshot`

Suggested representations:

- `Tagged`
- `I64`
- `F64`
- `Bool`
- `HeapPtr(ValueType)`

Suggested extra AOT-only metadata:

- `resume_ip -> checkpoint` map
- block predecessor lists
- stack height and local binding state per checkpoint
- effectful boundary markers for calls and helper-backed heap operations

### SSA Builder

Build SSA from `AotCfg` by abstract interpretation, not by replaying `AotInstruction` as native
steps.

The builder should track:

- symbolic operand stack: `Vec<AotSsaValueId>`
- symbolic locals: `Vec<AotSsaValueId>`
- current effect token if ordering becomes necessary
- per-bytecode-IP checkpoint state for any resumable position

At joins and loop headers:

- create block params for live locals
- create block params for live stack values
- enforce stack height agreement across predecessors
- use type-map information where available, but keep `Tagged` fallback where not proven

### Resume and Exit Model

Separate two concepts that are currently conflated by segments:

- normal control flow inside the compiled program
- runtime exits from native code

Normal control flow should become:

- `Br` -> native jump
- `Brfalse` -> native two-way branch
- loop backedges -> native jump with block args
- fallthrough -> direct native jump

Runtime exits should be limited to:

- host or builtin call boundaries
- yield or pending returns
- fuel interruption returns at host call boundaries
- epoch interruption returns at host call boundaries
- fatal error returns

Every exit that can return control to the VM must carry a complete boxing plan for the current
stack, locals, and `ip`.

Fuel-specific rule:

- do not add SSA machinery for fine-grained fuel checkpoints inside native loops
- perform fuel checks only at host call boundaries

### Cranelift Lowering

The new lowering path should:

- create one Cranelift block per SSA block
- create one small dispatch or checkpoint-entry block per resumable `vm.ip`
- lower SSA ops directly to Cranelift values
- materialize boxed VM state only in checkpoint-exit blocks and call boundaries

This path should replace the current dependence on:

- `build_segments`
- `emit_terminal` for normal branch semantics
- `step_to_call_tuple`
- helper-driven `STATUS_TRACE_EXIT` as an internal branch mechanism

## Phases

### Phase 0. Instrument and freeze invariants

- Add AOT debug dumps for:
  - CFG block count
  - segment count
  - resumable IP count
  - current bridge helper counts
- Add targeted tests covering:
  - diamond joins
  - loop-carried locals
  - yield replay at call IP
  - pending resume at post-call IP
  - fuel yield at host call boundaries with `fuel_check_interval` intentionally ignored
  - epoch yield at host call boundaries with `epoch_check_interval` intentionally ignored
  - artifact roundtrip

Deliverable:

- a stable behavioral and perf baseline before changing IR shape

### Phase 1. Add AOT SSA IR and verifier

- Introduce `AotSsaProgram` and related types under `vm/aot`.
- Add a verifier for:
  - SSA defs dominate uses
  - block arg arity and repr consistency
  - checkpoint snapshots reference only in-scope values
  - stack height and local state agreement at joins
- Add text rendering similar to `trace.ssa_text()` for debugability.

Deliverable:

- checked, printable AOT SSA data structures with no lowering yet

### Phase 2. Build SSA from CFG

- Keep `AotCfg` as the starting point.
- Extend it or derive side tables for:
  - predecessors
  - reverse postorder
  - call-site replay IPs
  - post-call resume IPs
  - host-call epoch checkpoint IPs
  - host-call fuel checkpoint IPs
- Replace `AotInstruction`-oriented lowering with an abstract interpreter that:
  - reads bytecode once
  - constructs SSA values for locals, stack values, arithmetic, compares, and branches
  - records checkpoint snapshots at every resumable IP

Initial scope:

- ints, floats, bools
- local loads and stores
- loop control
- typed numeric compare and arithmetic
- conservative `Tagged` fallback for unsupported dynamic cases

Deliverable:

- whole-program SSA for the numeric and branch-heavy subset already covered well by AOT tests

### Phase 3. Introduce checkpoint-entry lowering

- For every resumable IP, emit a checkpoint entry block that:
  - loads boxed VM stack and locals
  - reconstructs SSA values for that logical point
  - jumps to the SSA continuation block
- Keep the externally visible `resume_ips` surface unchanged.
- Preserve the current AOT runtime behavior for:
  - direct `run()`
  - `resume()`
  - yield replay
  - pending continuation

This is the critical compatibility phase. The machine code can change radically as long as the
resume table and supported runtime semantics do not.

Deliverable:

- SSA AOT that still resumes by `vm.ip` like the current implementation

### Phase 4. Lower SSA blocks directly to Cranelift

- Add `aot/native/lower.rs` or extend `aot/compile.rs` with SSA lowering helpers.
- Reuse neutral pieces from `vm/native` and, where justified, extract small helpers from
  `jit/native/lower.rs`.
- Lower:
  - numeric unbox and box
  - typed arithmetic
  - typed comparisons
  - block params and block jumps
  - checkpoint exits
- Remove helper-mediated normal branch lowering for SSA blocks.

Temporary migration rule:

- allow falling back to the current stack-machine AOT path for unsupported instructions or runtime
  modes only while the refactor is in flight
- remove that fallback before the refactor is considered complete; it is not part of the target
  architecture

Deliverable:

- first end-to-end SSA-backed whole-program AOT compiler

### Phase 5. Call boundaries and effect ordering

- Model host and builtin calls as explicit flush boundaries.
- Before a call that may yield, wait, or error:
  - materialize boxed stack and locals as required by the existing helper ABI
  - preserve current `vm.ip` semantics for replay or resume
- Before a host call boundary, check fuel and yield there if depleted.
- Before a host call boundary, check epoch deadline and yield there if expired.
- Do not thread fine-grained fuel checkpoint logic through SSA blocks.
- After a continue return:
  - reload or thread result values back into SSA state

If necessary, add a simple effect token or explicit side-effect ordering discipline for:

- heap allocation helpers
- collection helpers
- string and bytes operations
- any boundary where drop or ownership semantics matter

Deliverable:

- SSA AOT remains correct for calls, yield, pending, and heap-backed operations
- fuel interruption works only at host call boundaries
- epoch interruption works only at host call boundaries

### Phase 6. Expand type coverage and remove the old path

- Extend from numeric-only SSA to:
  - string and bytes typed ops
  - heap pointer representations where stable
  - helper-backed collection operations where beneficial
- Remove `build_segments` and segment-driven codegen once parity is reached.
- Delete the old stack-machine AOT lowering code instead of leaving deprecation scaffolding behind.
- Simplify `dump_aot_info()` to report the SSA lowering mode and checkpoint inventory instead of
  segment-derived internals.

End state:

- one whole-program AOT compiler path
- no permanent segment-based lowering path
- no compatibility or deprecation code for the removed internal path

## Neutral Helper Extraction

The AOT SSA path and the JIT SSA path should share only genuinely neutral pieces. Candidates:

- box and unbox helpers
- materialization helpers
- native layout and offset helpers
- selected Cranelift utility routines for typed arithmetic and comparisons

Avoid extracting whole trace concepts into a fake "shared" layer. Shared code should not require a
`jit vs aot` switch to make sense.

## Main Risks

### 1. Resume precision versus SSA win

If resumability is kept at every bytecode IP, checkpoint metadata can grow quickly.

Mitigation:

- keep the first SSA AOT implementation correct first
- measure checkpoint count and code size
- only coarsen resume granularity later if the runtime contract is intentionally changed

This risk is intentionally accepted for host-call-boundary interruption handling:

- AOT will not attempt fine-grained fuel checkpoints inside native loops
- AOT will not attempt fine-grained epoch checkpoints inside native loops
- long native regions without host calls may run past interpreter-style epoch interval behavior
- long native regions without host calls may run past interpreter-style fuel interval behavior

### 2. Call replay and pending semantics

AOT already has working semantics for:

- yield replay at call IP
- pending resume at resume IP

SSA must not regress this. Treat call boundaries as first-class checkpoint sites from the start.
They are also the only fuel and epoch interruption sites in the target design.

### 3. Drop and ownership semantics

The current segment path benefits from operating directly on boxed VM state. SSA reduces that
traffic, which means boxing, cloning, and overwrite semantics become more explicit.

Mitigation:

- keep heap values conservative early
- test against drop-contract parity
- only widen unboxed or borrowed heap support after parity is proven

### 4. Premature IR sharing with trace JIT

The shapes are similar, but the runtime contracts are different. Forcing one IR too early risks
contaminating both pipelines.

Mitigation:

- share small neutral helpers first
- only unify IR pieces after both paths stabilize

## Acceptance Criteria

- `Vm::compile_aot`, `Vm::run`, `Vm::resume`, and `.pat` artifact load/save continue to work.
- `aot_resume_ips()` remains meaningful and correct.
- Normal branches and loop backedges no longer require helper-mediated branch semantics inside AOT
  SSA-compiled regions.
- Numeric loop kernels keep locals and stack values in SSA form across the loop body.
- Host yield and pending behavior remain bytecode-compatible with the current AOT runtime.
- Fuel interruption yields only at host call boundaries, and AOT intentionally ignores
  `fuel_check_interval`.
- Epoch interruption yields only at host call boundaries, and AOT intentionally ignores
  `epoch_check_interval`.
- Existing whole-program AOT tests continue to pass, with new coverage for SSA checkpoints and
  join correctness.

## Recommended Implementation Order

1. Add AOT SSA IR, verifier, and text dump.
2. Build CFG-to-SSA with checkpoint snapshots, but do not lower it yet.
3. Lower a numeric subset end-to-end behind a temporary internal fallback.
4. Add checkpoint-entry lowering and preserve current resume semantics.
5. Port calls, yield, pending, and host-call epoch boundaries.
6. Expand typed heap-backed ops and remove segment lowering.
