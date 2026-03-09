# pd-vm Lua Performance Improvement Plan

## Summary

Improve `pd-vm` performance for Lua numeric loop workloads in two tracks at the same time:

- Make the benchmark and diagnostics rigorous enough to separate compile cost, warmup cost, interpreter steady-state cost, and JIT steady-state cost.
- Improve both the interpreter and the JIT for integer-heavy loop code, with the first priority on changes that reduce per-iteration dispatch, stack/local traffic, and VM re-entry.

The target workload is the existing same-source Lua hot-loop benchmark already added to `vm-compare-bench`: one shared Lua source measured in four modes (`pd-vm`, `pd-vm-jit`, `luajit -joff`, LuaJIT JIT), excluding compile time and excluding warmup runs from the timed section.

## Key Changes

### 1. Benchmark and diagnostics hardening

- Keep the current same-source Lua comparison as the canonical benchmark shape in `vm-compare-bench`.
- Preserve the current timing contract: compile outside the timer, warm runs outside the timer, timed runs only over steady-state execution.
- Extend the benchmark output to record internal `pd-vm` execution diagnostics for the timed section:
  - JIT native trace count before and after timing
  - JIT native exec count delta for the timed section
  - JIT native bridge stats snapshot for the timed section when JIT is enabled
  - interpreter opcode counts for the timed section when JIT is disabled
- Add a compact per-run artifact format in `results/` that records:
  - workload parameters
  - exact timing methodology
  - `pd-vm` internal counters
  - normalized cost per inner-loop iteration
- Continue updating the top-level benchmark markdown with the latest recorded result and methodology notes.

### 2. Interpreter performance work

- Add opcode profiling support to the interpreter loop in `pd-vm/src/vm/mod.rs` behind an explicit benchmark/debug toggle so the benchmark can identify the hottest opcode patterns for the shared Lua loop workload.
- Use those measurements to introduce fused superinstructions for the dominant integer-loop patterns emitted by the Lua frontend. The first fused forms should cover:
  - repeated local load/store arithmetic updates
  - integer compare + branch patterns used by `while`
  - integer increment/update patterns for loop counters
- Move from “generic opcode + operand hint” execution toward true specialized integer opcodes for the Lua numeric fast path where type stability is already known at compile time.
- Keep the interpreter stack/local model semantically unchanged, but add hot-path helpers that avoid unnecessary `Value` boxing/unboxing and repeated `Vec` traffic for integer operations.
- Inline the hottest integer ops directly in the interpreter loop and keep cold generic/type-error paths out of line.

### 3. Compiler and lowering work for interpreter speed

- Improve Lua lowering so integer loop kernels emit fewer `ldloc` and `stloc` round-trips and fewer stack-machine bookkeeping ops.
- Teach the compiler to emit specialized integer opcodes or fused ops for proven-int loop variables instead of relying only on runtime operand hints.
- Add or extend peephole optimization to collapse common Lua loop bytecode shapes before execution.
- Preserve existing semantics and fallback behavior: any path that is not provably specialized must continue to use the generic opcodes.

### 4. JIT performance work

- Fix the highest-value JIT design issue first: loop-back traces must remain in native code instead of returning to the VM on every backedge.
  - Replace the current `JumpToRoot -> trace exit -> VM resumes` behavior with a native self-loop/root-block jump for loop-back traces.
  - Keep guards and exits correct, but do not bounce through `run_internal` for the common loop-back path.
- Reduce native trace dependence on boxed `Value` stack/local traffic for integer-heavy traces.
  - Keep trace-local integer values unboxed in native SSA/register form where possible.
  - Spill back to VM stack/locals only at guards, exits, calls, and unsupported operations.
- Add trace diagnostics that distinguish:
  - native loop-back iterations
  - helper/bridge slow-path hits
  - guard exits
  - unsupported-step fallbacks
- Do not attempt broad semantic expansion in the first pass; focus on integer arithmetic, integer comparisons, local loads/stores, and loop control used by the benchmarked Lua workload.

## Test Plan

- Keep existing `vm-compare-bench` correctness tests passing.
- Keep the ignored LuaJIT comparison test passing in release mode, with the same-source methodology.
- Add benchmark assertions for methodology:
  - compile happens before timing
  - warm runs happen before timing
  - result totals match across all four engines
- Add interpreter profiling tests that validate opcode and superinstruction counters on a stable fixture.
- Add JIT tests that validate:
  - loop-back traces stay native without VM re-entry on the hot loop path
  - guard exits still resume correctly
  - native exec counters increase during the timed section
- Acceptance criteria for the shared Lua hot-loop benchmark:
  - `pd-vm-jit` remains correct and faster than `pd-vm` on the same source
  - interpreter changes reduce normalized ns per inner iteration versus the current baseline
  - JIT changes reduce normalized ns per inner iteration versus the current same-source baseline
  - benchmark artifacts in `results/` and the top-level benchmark markdown are updated after measurement

## Assumptions and defaults

- The first optimization target is integer-heavy Lua loop code, not general dynamic-language behavior.
- No language semantics should change; all specialization must preserve current observable behavior and fall back to generic execution when needed.
- Benchmark decisions are fixed:
  - same Lua source across all four engine modes
  - compile excluded from timing
  - warmup excluded from timing
  - release-mode benchmark is the source of record
- The first implementation pass should prioritize highest-ROI changes in this order:
  1. benchmark diagnostics
  2. interpreter opcode profiling
  3. interpreter superinstructions and typed integer opcodes
  4. JIT loop-back native residency
  5. JIT unboxed integer trace fast path
