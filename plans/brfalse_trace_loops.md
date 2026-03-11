# Support Backward `brfalse` Trace Loops

## Summary
- Remove the shared NYI for backward `brfalse` by teaching the trace pipeline to represent a conditional in-trace back-edge instead of rejecting it.
- Keep this change incremental: no compiler/lowering work now, no whole-program CFG rewrite, and no changes to existing `while` lowering. The new IR shape is a future hook for later tail-recursive or alternate lowering work.
- Primary touch points: [trace.rs](pd-vm/src/vm/jit/trace.rs), [runtime.rs](pd-vm/src/vm/jit/runtime.rs), [codegen.rs](pd-vm/src/vm/jit/native/codegen.rs), [bridge.rs](pd-vm/src/vm/jit/native/bridge.rs), [aot.rs](pd-vm/src/vm/jit/aot.rs).

## Public API / Type Changes
- Add `TraceStep::LoopIfFalse { target_ip: usize, exit_ip: usize }`.
- Remove `JitNyiReason::BackwardGuard` and all snapshot/docs references to that NYI.
- Keep `JitTraceTerminal` unchanged; traces ending in `LoopIfFalse` use `JitTraceTerminal::BranchExit` because the trace only returns to the VM on fallthrough exit.
- Bump AOT bundle version from `6` to `7`.
- Reserve AOT step tag `69` for `LoopIfFalse`.
- Reserve native helper opcode `26` as `OP_LOOP_IF_FALSE`.

## Implementation Plan
1. Refactor backward-branch recording in [trace.rs](pd-vm/src/vm/jit/trace.rs) into one shared helper used by both `compile_trace` and `compile_aot_block`, so the two paths cannot drift again.
2. Change `Brfalse` lowering rules as follows:
   - Forward target: keep current `GuardFalse` / `GuardTrue` join-path behavior unchanged.
   - Backward target that matches an earlier `step_ip` already recorded in the current trace: emit `LoopIfFalse { target_ip, exit_ip: fallthrough_ip }`, stop recording immediately, and finish the trace as `BranchExit`.
   - Backward target that is not already inside the current trace: emit ordinary `GuardFalse { exit_ip: target }` and continue recording the fallthrough path instead of rejecting it.
3. Make `finish_trace` validate every `LoopIfFalse.target_ip` against final `step_ips` and reject only malformed traces loaded from bugs or corrupted AOT data, not valid backward guards.
4. Make `optimize_trace_steps` branch-aware: collect all `LoopIfFalse.target_ip` values first and forbid any fusion that would erase a targeted `step_ip`. The first step of a fused window may still be a target because its IP survives; any targeted later step in the window must block the fusion.
5. Update [runtime.rs](pd-vm/src/vm/jit/runtime.rs) so `execute_jit_trace` uses a manual `while step_index < trace.steps.len()` loop instead of `for`, builds a local `ip -> step_index` map once per trace execution, and handles `LoopIfFalse` by:
   - popping a bool,
   - rewinding `step_index` to the mapped target on `false`,
   - setting `vm.ip = exit_ip` and returning `ExecOutcome::Continue` on `true`.
6. Leave hot-root detection unchanged. `scan_loop_headers()` and `scan_program_block_roots()` already recognize backward `brfalse` targets, so no root-scanning redesign is needed.
7. Extend native codegen in [cranelift.rs](pd-vm/src/vm/jit/native/cranelift.rs) and [codegen.rs](pd-vm/src/vm/jit/native/codegen.rs) with a two-pass layout:
   - pre-scan the trace for all `LoopIfFalse.target_ip` values and resolve them to target step indices from `step_ips`,
   - allocate Cranelift blocks for step `0` plus every resolved loop target,
   - emit linear steps as today, but emit `LoopIfFalse` as a conditional branch to the target block on `false` and a normal trace exit to `exit_block` on `true`.
8. Add `emit_inline_loop_if_false` by cloning the structure of `emit_inline_guard_false`, except the fast path branches to a target block instead of a side-exit block. Its slow path should use `OP_LOOP_IF_FALSE` via `emit_helper_step_from_call_tuple`, with `next_block` set to the loop target block; the helper returns `STATUS_CONTINUE` for the loop-taken path and `STATUS_TRACE_EXIT` after setting `vm.ip = exit_ip` for the fallthrough-exit path.
9. Extend [bridge.rs](pd-vm/src/vm/jit/native/bridge.rs) with `OP_LOOP_IF_FALSE` and bridge-name reporting. The helper semantics are: `false` => pop bool and return `STATUS_CONTINUE`; `true` => `jump_to(exit_ip)` and return `STATUS_TRACE_EXIT`; invalid stack/type => existing error path.
10. Update [aot.rs](pd-vm/src/vm/jit/aot.rs) encode/decode/validation for the new step tag, and validate that every `LoopIfFalse.target_ip` resolves to an earlier `step_ip` within the same trace.
11. Update diagnostics and docs: add `trace_step_name = "loop_if_false"`, remove backward-`brfalse` from `nyi_reference()`, and remove the README NYI note at [README.md](pd-vm/README.md#L698).

## Tests and Acceptance Criteria
- Replace the current NYI tests in [jit_nyi_edge_tests.rs](pd-vm/tests/jit/jit_nyi_edge_tests.rs) with positive tests that hand-build bytecode containing a backward `brfalse` to the trace root and assert:
  - correct final VM result,
  - at least one successful trace compile,
  - no `BackwardGuard` attempt remains,
  - native executions occur on supported targets.
- Add a second hand-built test where the backward `brfalse` targets an earlier non-root step in the same trace to prove the implementation is not hard-coded to `root_ip`.
- Add a test where the backward `brfalse` target is outside the currently recorded trace and assert it lowers to `GuardFalse`, not `LoopIfFalse`.
- Add a trace-interpreter-path test by forcing `execute_jit_trace` and verifying `LoopIfFalse` loops correctly without native code.
- Add AOT coverage for `prepare_aot()`, bundle encode/decode, and execution of a trace containing `LoopIfFalse`.
- Keep existing loop/join-path tests green, especially the `GuardTrue` join-path case and nested-loop trace tests in [jit_tests.rs](pd-vm/tests/jit/jit_tests.rs).

## Assumptions and Defaults
- Compiler output stays unchanged in this effort; frontends still emit forward-guard/backward-`br` loop shapes, and no tail-recursive lowering is added now.
- The future hook is structural, not end-to-end: branch recording/codegen helpers should be written so adding `LoopIfTrue` or a generalized in-trace jump later is additive, but only `LoopIfFalse` is implemented now.
- Internal looping is only formed when the backward target is already inside the current trace. Everything else remains an exit to preserve the current trace-based architecture.
- No compatibility layer for old AOT bundles is needed beyond the version bump; version `6` should fail cleanly once `7` is introduced.
