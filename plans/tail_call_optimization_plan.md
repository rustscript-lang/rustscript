# Tail Call Optimization Implementation Plan

**Goal:** Eliminate script-frame growth for semantically valid RSS tail calls, including self-recursion, mutual recursion, closures, and generic function values, while preserving current return, capture, fuel, debugger, JIT, AOT, and `no_std` behavior.

## Current state

RustScript currently has no tail-call or tail-recursion optimization:

- the compiler emits `CallValue(argc)` for script calls and emits `Ret` separately;
- `Vm::execute_call_value` enters a new script frame for every script target;
- direct tail recursion therefore reaches `VmError::CallStackOverflow` at the configured script-call-depth limit;
- the interpreter's current `Call + Ret` fusion only completes a returned host/builtin call and does not reuse script frames;
- Trace JIT and AOT model `CallValue` as a real frame transition and likewise grow the script frame stack.

The configurable call-depth limit remains a safety guard for non-tail recursion after this work.

## 1. Define the tail-call semantic contract

**Target:** Optimize only calls whose result is returned unchanged from the active script frame.

**Tasks:**

- Define tail position recursively for function/closure bodies, block tail expressions, `if` branches, `match` arms, and explicit return expressions when the grammar supports them.
- Exclude calls followed by arithmetic, conversion, container construction, assignment, defer/finalization work, or any operation that observes the current frame after the call.
- Optimize script-function-item and closure targets, including sibling and mutual tail calls. Preserve the current behavior for host/builtin targets until asynchronous host-tail continuations have a separate proven design.
- Keep the root frame explicit: a root-to-script call enters one script frame; only an already-active script frame may be replaced. This preserves `Halt`, host invocation, debugger root semantics, and the configured depth limit.
- Specify that one call-boundary interrupt/fuel charge is still consumed per logical tail call, so infinite tail recursion remains interruptible.
- Preserve callable arity/schema checks, capture ownership, map-iterator cleanup, drop events, stack traces, and error attribution at every logical call boundary.

**Acceptance:** Characterization tests distinguish tail recursion from non-tail recursion and prove that only valid tail positions reuse a frame.

## 2. Add an explicit tail-call bytecode contract

**Target:** Encode compiler-proven tail position directly instead of relying on adjacency peepholes.

**Files:**

- `src/bytecode.rs`
- `src/assembler.rs`
- `src/vmbc.rs`
- `pd-vm-nostd/src/bytecode.rs`
- `pd-vm-nostd/src/vmbc.rs`
- `tests/wire/assembler_vmbc_edge_tests.rs`
- `tests/wire/wire_tests.rs`

**Tasks:**

- Add `TailCallValue(argc)` with the same operand layout as `CallValue(argc)`.
- Bump `BYTECODE_ABI_VERSION` and VMBC to v11 as a hard internal-format break; update no-std decoding and reject all earlier versions.
- Update opcode parsing, mnemonic rendering, assembler APIs, disassembly, validation, stack-effect analysis, function-region checks, and typed operand metadata.
- Require `TailCallValue` to occur inside a script function region. Reject it in the root region and reject malformed arity/stack shapes.
- Keep `Call` host-only and retain existing `CallValue` for non-tail script calls and Rust-host invocation boundaries.

**Tests:**

- VMBC v11 round-trip and old-version rejection;
- assembler/disassembler round-trip for `tailcallvalue`;
- validator rejection in the root region and across malformed function regions;
- no-std decoder parity.

**Acceptance:** Tail intent is explicit, validated, serialized, and backend-independent.

## 3. Lower RSS tail positions precisely

**Target:** Emit `TailCallValue` only where the source result is returned unchanged.

**Files:**

- `src/compiler/codegen.rs`
- `src/compiler/ast.rs` and parser files only if explicit `return` requires new tail-context plumbing
- `src/compiler/typing.rs`
- `tests/compiler/compiler_rustscript_tests.rs`
- `tests/compiler/compiler_tests.rs`

**Tasks:**

- Add a tail-context codegen path, such as `compile_tail_expr`, rather than inferring tail position from emitted bytecode.
- Propagate tail context through block tails, both sides of `if`, all `match` arms, grouped expressions, and explicit return expressions.
- Emit callable plus arguments exactly once, then `TailCallValue(argc)` without a following `Ret` on that control-flow path.
- Preserve ordinary `CallValue + remaining expression + Ret` for non-tail calls.
- Support named functions, closures, aliases, branch-merged callable values, explicit generic function values, and inferred generic callables through the same opcode.
- Keep unresolved-generic, arity, move, borrow, and result-schema diagnostics unchanged.

**Tests:**

- direct accumulator recursion and mutual recursion compile to `TailCallValue`;
- closure self-recursion and aliased callable tail calls compile to `TailCallValue`;
- tail calls in both `if` branches and every `match` arm;
- `recurse(x) + 1`, `let y = recurse(x); y`, and calls followed by cleanup remain `CallValue`;
- generic tail-recursive functions retain substituted callable schemas.

**Acceptance:** Disassembly proves precise tail marking with no adjacency-only heuristic.

## 4. Reuse script frames in interpreter and `no_std`

**Target:** Replace the active script frame atomically without increasing active script depth.

**Files:**

- `src/vm/mod.rs`
- `src/vm/value.rs`
- `src/vm/map_iter.rs`
- `src/vm/tests.rs`
- `pd-vm-nostd/src/vm.rs`
- `pd-vm-nostd/tests/embedded_host.rs`
- `tests/compiler/compiler_rustscript_tests.rs`

**Tasks:**

- Split callable validation/operand extraction from frame entry so `CallValue` and `TailCallValue` share target, arity, schema, and capture checks.
- Add one frame-replacement operation that:
  - requires an active script frame;
  - preserves the current frame's typed continuation and caller operand-stack base;
  - releases current frame locals, capture-cell bindings, iterator state, and owned values exactly once;
  - resizes/reinitializes the local window for the callee;
  - installs parameters, captures, self binding, prototype id, and callee entry IP;
  - leaves `call_depth` and `execution_frames.len()` unchanged.
- Perform all fallible validation before mutating the active frame. On failure, preserve deterministic unwind/error state.
- Charge fuel/epoch at every logical tail-call boundary and keep infinite tail recursion resumable/interruptible.
- Mirror the same semantics in `pd-vm-nostd` without allocation beyond the replacement frame/local window already required by the callee.

**Tests:**

- millions of accumulator tail calls complete with `max_script_call_depth = 1` after the initial root-to-script entry;
- mutual recursion and recursive closures keep a constant script-frame depth;
- non-tail recursion still reports the configured `CallStackOverflow` limit;
- tail-call arity/type errors report the callee and do not corrupt caller state;
- captures, moved values, mutable borrowed cells, map iterators, and drop-contract counters are released exactly once;
- fuel and epoch interruption stop infinite tail recursion and resume correctly;
- no-std parity for finite and interrupted tail recursion.

**Acceptance:** Interpreter and no-std execute valid tail recursion in constant script-frame space.

## 5. Implement native Trace JIT and AOT parity

**Target:** Keep tail calls inside native execution with frame replacement rather than interpreter fallback.

**Files:**

- `src/vm/native/bridge.rs`
- `src/vm/native/codegen.rs`
- `src/vm/native/mod.rs`
- `src/vm/jit/trace.rs`
- `src/vm/jit/ir.rs`
- `src/vm/jit/recorder.rs`
- `src/vm/jit/native/lower.rs`
- `src/vm/aot/cfg.rs`
- `src/vm/aot/ir.rs`
- `src/vm/aot/ssa.rs`
- `src/vm/aot/compile.rs`
- `tests/jit/jit_tests.rs`

**Tasks:**

- Add a native frame-replacement helper/ABI operation distinct from frame entry. Bump `NATIVE_CALLABLE_ABI_VERSION`, AOT artifact version/ABI, and native trace cache revision.
- Model `TailCallValue` explicitly in Trace JIT and AOT IR/SSA terminators.
- Resolve the dynamic callable target, replace the active frame, and link/jump to the callee's native entry without increasing native link-dispatch depth or handing execution to the interpreter.
- Preserve return-to-caller continuation: when the eventual callee returns, it resumes the original caller of the replaced frame.
- Preserve interrupt/fuel checks at logical call boundaries and pending/error propagation.
- Reject unsupported native shapes during compile/install with a deterministic feature error; do not fail halfway through a frame replacement.

**Tests:**

- direct, mutual, closure, capture-bearing, and generic tail recursion under Trace JIT and AOT;
- constant frame depth and zero interpreter fallback;
- fuel yield/resume and epoch interruption inside infinite native tail recursion;
- artifact/cache rejection after ABI revision changes;
- result/error parity with interpreter and no-std.

**Acceptance:** At least one dynamic closure tail-call path and one named mutual-recursion path execute natively in each backend with constant frame depth.

## 6. Debugger, recording, diagnostics, docs, and final verification

**Target:** Preserve logical observability despite physical frame reuse.

**Files:**

- `src/debugger/mod.rs`
- `src/debugger/recording.rs`
- `src/debug_info.rs`
- `src/vm/diagnostics.rs`
- `README.md`
- relevant examples and changelog files

**Tasks:**

- Decide and document debugger behavior: expose the current physical stack by default and record a tail-call transition event containing caller/callee prototype and source location.
- Ensure `next`, `out`, breakpoints, stack rendering, and recording/replay remain deterministic across a tail-call transition.
- Bump debugger recording format and reject earlier recordings.
- Render tail-call errors at the logical call site and include a bounded tail-call transition history if stack traces need recursion context without unbounded memory.
- Document `TailCallValue`, supported source tail positions, interaction with `--max-call-depth`, and the absence of optimization for host/builtin tail calls in the first release.

**Final verification:**

```bash
cargo fmt --all -- --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace --all-features --release
git diff --check
```

Also run focused interpreter, no-std, Trace JIT, AOT, VMBC, debugger recording, fuel, epoch, capture-lifecycle, and configurable-depth matrices.

**Acceptance:** Valid script tail recursion is constant-space in every runtime backend; non-tail recursion remains bounded by the configured call-depth limit; debugging and interruption remain usable.