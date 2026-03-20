# pd-vm Call Overhead Reduction Plan

## Summary

This plan reduces the current builtin and host-call overhead in `pd-vm` by attacking four
separate costs in dependency order:

1. replace clone-heavy owned heap extraction with explicit borrowed or taken argument ABI
2. remove `Vec<Value>` allocation from the common 0/1-result return path
3. remove `Vec<Value>` argument marshalling from VM-aware host-call dispatch
4. lower the hottest builtin projection calls into direct interpreter fast paths instead of paying
   full generic `Call` dispatch

The intent is not to redesign ordinary RustScript function calls. Those are already compiled
inline today in [`pd-vm/src/compiler/codegen.rs`](../pd-vm/src/compiler/codegen.rs), so the main
runtime tax is on builtin and host dispatch.

## Current Snapshot (2026-03-20)

Observed behavior in the current workspace:

- heap VM values are shared through `Arc`:
  [`SharedString`, `SharedBytes`, `SharedArray`, `SharedMap`](../pd-vm/src/bytecode.rs)
- core hot builtins such as `len`, `slice`, `concat`, `get`, `has`, `set`, `keys`, and `count`
  already have manual borrowed or shared-handle fast paths in
  [`pd-vm/src/builtins/runtime/core.rs`](../pd-vm/src/builtins/runtime/core.rs)
- proc-macro-generated builtin wrappers still extract arguments with
  [`arg::<T>(args, index, label)`](../pd-vm/src/builtins/runtime/typed.rs), and
  `VmArray`/`VmBytes`/`VmMap` extraction currently does `values.clone()` followed by
  [`unwrap_or_clone_shared(...)`](../pd-vm/src/bytecode.rs), which guarantees a full payload clone
  for read-only proc-macro builtin parameters
- builtin calls already borrow the VM stack tail directly in
  [`execute_builtin_call_from_stack`](../pd-vm/src/vm/host.rs)
- VM-aware host calls still allocate a temporary `Vec<Value>` through
  [`pop_call_args`](../pd-vm/src/vm/host.rs) before dispatching to
  [`HostFunction::call`](../pd-vm/src/vm/host.rs)
- builtin and host return paths still use `Vec<Value>` through
  [`BuiltinCallOutcome::Return(Vec<Value>)`](../pd-vm/src/builtins/runtime/mod.rs) and
  [`CallOutcome::Return(Vec<Value>)`](../pd-vm/src/vm/host.rs)
- JIT already recognizes specialized projection operations such as `string_len`, `bytes_len`,
  `string_get`, `bytes_get`, `array_len`, `array_get`, `array_has`, `map_len`, `map_get`, and
  `map_has`, but the interpreter still executes those through generic builtin `Call`

Current measured baselines from this workspace:

- `perf_jit_native_reduces_array_builtin_loop_latency`
  - interpreter: `29129 us`
  - JIT: `15732 us`
  - workload executes about `303104` array builtin operations in the timed loop
- `perf_jit_native_characterizes_map_builtin_loop_latency`
  - interpreter: `31986 us`
  - JIT: `17912 us`
  - workload executes about `294912` map builtin operations in the timed loop
- `perf_vm_creation_cleanup_speed_and_ram_usage`
  - cached host bind/load overhead: about `173 ns/import`
  - cached static host bind/load overhead: about `163 ns/import`

Important gap:

- the repo does not yet have a dedicated steady-state host call latency benchmark that isolates
  per-call runtime cost from bind/load/setup cost

## Goals

- eliminate accidental deep clones for read-only heap-backed builtin parameters
- keep mutation-capable builtin paths on shared `Arc` handles until they actually detach on write
- remove common per-call small allocations on both the argument and return sides
- let the interpreter hit direct collection/string/bytes projection fast paths without going
  through the generic builtin dispatch boundary
- keep rollout incremental so each stage has a measurable before/after perf gate

## Non-Goals

- redesigning RustScript source-level function semantics
- introducing general closure values or call frames as part of this work
- removing `Arc` from the VM value model
- broad AOT architecture work beyond the minimal compatibility needed for the hot builtin lowering
- changing user-visible RSS syntax as part of this plan

## Cross-Stage Benchmark Gate

Before or alongside Stage 1, add a new ignored perf harness focused on steady-state call overhead.

Required new measurements:

- no-op host import call loop
  - dynamic host function
  - static host function
  - 0-arg and 1-arg variants
  - interpreter first; JIT optional second
- clone-heavy proc-macro builtin loop
  - `bytes::from_array_u8`
  - `__format_template`
  - one borrowed bytes/string path as control
- existing array and map builtin loop benchmarks remain as regression guards

Suggested harness location:

- [`pd-vm/tests/jit/perf_tests.rs`](../pd-vm/tests/jit/perf_tests.rs)

Suggested benchmark names:

- `perf_host_call_steady_state_latency`
- `perf_proc_macro_builtin_heap_arg_latency`

## ABI Inventory

This section names the ABI surfaces expected to change or stay intentionally stable.

### Stage 1 ABI Surfaces: Borrowed/Taken Builtin Arguments

Current surfaces:

- proc-macro-generated builtin wrapper
  - `fn wrapper(args: &[Value]) -> VmResult<T>`
  - arity: `1`
- proc-macro-generated VM-aware builtin wrapper
  - `fn wrapper(vm: &mut Vm, args: &[Value]) -> VmResult<T>`
  - arity: `2`
- [`FromVmValue::from_vm_value(value, label)`](../pd-vm/src/builtins/runtime/typed.rs)
  - arity: `2`
- [`arg(args, index, label)`](../pd-vm/src/builtins/runtime/typed.rs)
  - arity: `3`

Planned target surfaces:

- proc-macro-generated builtin wrapper
  - `fn wrapper(args: &mut [Value]) -> VmResult<T>`
  - arity: `1`
- proc-macro-generated VM-aware builtin wrapper
  - `fn wrapper(vm: &mut Vm, args: &mut [Value]) -> VmResult<T>`
  - arity: `2`
- new borrowed extractor trait
  - `BorrowVmValue::borrow_vm_value(value: &Value, label: &str) -> VmResult<Self>`
  - arity: `2`
- new taken extractor trait
  - `TakeVmValue::take_vm_value(slot: &mut Value, label: &str) -> VmResult<Self>`
  - arity: `2`
- new borrowed helper
  - `borrow_arg(args: &[Value], index: usize, label: &str) -> VmResult<T>`
  - arity: `3`
- new taken helper
  - `take_arg(args: &mut [Value], index: usize, label: &str) -> VmResult<T>`
  - arity: `3`

Planned parameter families:

- borrowed read-only views
  - `VmValueRef<'a>`
  - `VmStringRef<'a>`
  - `VmBytesRef<'a>`
  - `VmArrayRef<'a>`
  - `VmMapRef<'a>`
- taken/shared handles for mutation-capable paths
  - `VmValueOwned`
  - `VmBytesHandle`
  - `VmArrayHandle`
  - `VmMapHandle`

Intent:

- read-only proc-macro builtin inputs stop using `VmArray`/`VmBytes`/`VmMap` by value
- mutation-capable proc-macro builtins take shared handles, not cloned owned payloads

### Stage 2 ABI Surfaces: Return Path

Current type surfaces:

- [`CallOutcome::Return(Vec<Value>)`](../pd-vm/src/vm/host.rs)
- [`BuiltinCallOutcome::Return(Vec<Value>)`](../pd-vm/src/builtins/runtime/mod.rs)

Current function surfaces:

- [`HostAsyncBridge::poll_op(op_id, cx)`](../pd-vm/src/vm/host.rs)
  - arity: `2`
- [`Vm::complete_host_op(op_id, values)`](../pd-vm/src/vm/host.rs)
  - arity: `2`
- [`Vm::complete_waiting_host_op(op_id, values)`](../pd-vm/src/vm/host.rs)
  - arity: `2`
- [`return_values(value)`](../pd-vm/src/builtins/runtime/typed.rs)
  - arity: `1`
- [`IntoBuiltinCallOutcome::into_builtin_call_outcome(self)`](../pd-vm/src/builtins/runtime/typed.rs)
  - arity: `0`
- [`IntoHostCallOutcome::into_host_call_outcome(self)`](../pd-vm/src/builtins/runtime/typed.rs)
  - arity: `0`

Planned target surfaces:

- new small-result carrier
  - `CallReturn::{None, One(Value)}`
- `CallOutcome::Return(CallReturn)`
- `BuiltinCallOutcome::Return(CallReturn)`
- `HostAsyncBridge::poll_op(op_id, cx) -> Poll<VmResult<CallReturn>>`
  - arity: `2`
- `Vm::complete_host_op(op_id, result: CallReturn) -> VmResult<()>`
  - arity: `2`
- `Vm::complete_waiting_host_op(op_id, result: CallReturn) -> VmResult<()>`
  - arity: `2`
- helper split
  - `return_none() -> CallReturn`
  - arity: `0`
  - `return_one(value) -> CallReturn`
  - arity: `1`
- legacy compatibility
  - `From<Vec<Value>> for CallReturn`
  - arity: `1`
  - many legacy values are packed into one array `Value`, which matches the current Lua-style pack
    and unpack model

Intent:

- common 0-result and 1-result call paths become allocation-free
- legacy many-result compatibility remains available through packed array values without forcing the
  common case to pay for them

### Stage 3 ABI Surfaces: VM-Aware Host Dispatch

Current public host ABI already has the right read-only argument shape:

- [`HostFunction::call(vm, args)`](../pd-vm/src/vm/host.rs)
  - signature: `fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome>`
  - arity: `2`
- [`StaticHostFunction`](../pd-vm/src/vm/host.rs)
  - signature: `fn(&mut Vm, &[Value]) -> VmResult<CallOutcome>`
  - arity: `2`

Current args-only ABI:

- [`HostArgsFunction::call(args)`](../pd-vm/src/vm/host.rs)
  - arity: `1`
- [`StaticHostArgsFunction`](../pd-vm/src/vm/host.rs)
  - arity: `1`

Current internal execution ABI that still allocates:

- [`Vm::execute_bound_host_function(resolved_index, args, call_ip)`](../pd-vm/src/vm/host.rs)
  - arity: `3`
- [`Vm::pop_call_args(argc)`](../pd-vm/src/vm/host.rs)
  - arity: `1`

Planned target:

- keep public read-only host-call signature stable for Stage 3
  - `HostFunction::call(vm, args)` stays arity `2`
  - `StaticHostFunction` stays arity `2`
- replace the allocating internal execution path with borrowed stack-tail dispatch
  - `Vm::execute_bound_host_function_from_stack(resolved_index, argc, call_ip)`
  - arity: `3`
- `Vm::pop_call_args(argc)` leaves the steady-state path for ordinary VM-aware host calls

Optional follow-up, explicitly not required in first Stage 3 pass:

- mutable/taking host ABI if a real host use case needs move-out semantics
  - `HostMutStackFunction::call(vm, args: &mut [Value])`
  - arity: `2`

Intent:

- `print`, `println`, `runtime::sleep`, and similar read-only VM-aware host calls stop allocating
  an argument `Vec<Value>` on every invocation
- Stage 3 remains semantically conservative by not changing host trait shapes unless required

### Stage 4 ABI Surfaces: Hot Builtin Lowering

No host trait ABI change is required for Stage 4.

Stage 4 may change one of these internal execution interfaces:

- interpreter fast path under [`OpCode::Call`](../pd-vm/src/vm/mod.rs)
- bytecode opcode set in [`pd-vm/src/bytecode.rs`](../pd-vm/src/bytecode.rs), if dedicated
  opcodes are chosen instead of interpreter superinstructions
- compiler lowering under [`pd-vm/src/compiler/codegen.rs`](../pd-vm/src/compiler/codegen.rs)
- VMBC encode/decode if new opcodes are introduced

Preferred first pass:

- no bytecode wire-format change
- implement interpreter-side direct fast paths or superinstructions first
- introduce new opcodes only if the post-fast-path profile still shows decode or dispatch overhead
  dominating

## Stage 1: Borrowed/Taken Builtin Argument ABI

## Goal

Stop the generic proc-macro builtin path from materializing owned heap payloads when the callee only
needs borrowed read access, and stop using cloned `Vec`/`VmMap` payloads for mutation-capable
paths.

## Work Items

1. Extend [`pd-vm/src/builtins/runtime/typed.rs`](../pd-vm/src/builtins/runtime/typed.rs) with
   borrowed and taken extractor traits.
2. Teach [`pd-vm/pd-host-function/src/lib.rs`](../pd-vm/pd-host-function/src/lib.rs) to generate
   wrappers over `&mut [Value]` instead of `&[Value]`.
3. Add borrowed argument wrapper types for strings, bytes, arrays, maps, and generic `Value`.
4. Add taken/shared handle types for mutation-capable array/bytes/map inputs.
5. Migrate read-only proc-macro builtins away from by-value heap extraction.
6. Leave existing hand-written fast paths in
   [`pd-vm/src/builtins/runtime/core.rs`](../pd-vm/src/builtins/runtime/core.rs) intact until the
   new taken/shared-handle ABI can express them cleanly.

## First-Wave Migration Targets

- [`bytes::from_array_u8`](../pd-vm/src/builtins/runtime/bytes.rs)
  - switch `values: VmArray` to borrowed array view
- [`__format_template`](../pd-vm/src/builtins/runtime/core.rs)
  - switch `values: VmArray` to borrowed array view
- any future proc-macro builtin that only reads bytes/array/map/string inputs

## Acceptance Criteria

- no read-only proc-macro builtin path uses `unwrap_or_clone_shared(values.clone())`
- heap-backed proc-macro parameter extraction is either:
  - borrowed, or
  - taken as a shared handle
- array/map/bytes mutation still detaches only at `Arc::make_mut` write points

## Stage 2: Small Return ABI

## Goal

Remove the mandatory `Vec<Value>` allocation from the common 0-result and 1-result call path.

## Work Items

1. Introduce `CallReturn` or equivalent local small-result carrier.
2. Update builtin and host outcomes to return `CallReturn`.
3. Update async host completion surfaces to return and complete `CallReturn`.
4. Replace `return_values(...)` with `return_none()` and `return_one(...)`.
5. Update the VM stack push sites in [`pd-vm/src/vm/host.rs`](../pd-vm/src/vm/host.rs) to consume
   `CallReturn` without heap allocation in the common case.

## Acceptance Criteria

- returning zero values does not allocate
- returning one value does not allocate
- legacy multi-value compatibility stays intact through packed array values for debugger, async
  completion, and Lua-style pack/unpack behavior

## Stage 3: Zero-Alloc VM-Aware Host Dispatch

## Goal

Stop allocating and reversing a temporary `Vec<Value>` for VM-aware host calls that only need a
borrowed argument slice.

## Work Items

1. Change host dispatch in [`pd-vm/src/vm/host.rs`](../pd-vm/src/vm/host.rs) to borrow the stack
   tail directly for ordinary `HostFunction` and `StaticHostFunction` calls.
2. Preserve the current read-only host ABI so existing host implementations keep compiling.
3. Keep yield and pending semantics correct when dispatch uses borrowed stack-tail slices.
4. Migrate the default runtime host functions in
   [`pd-vm/src/builtins/runtime/host.rs`](../pd-vm/src/builtins/runtime/host.rs) to the zero-alloc
   dispatch path.

## Notes

- Stage 3 should first reuse the already-correct public host signature
  `(&mut Vm, &[Value])`.
- A mutable host-args ABI should only be added if a concrete host import needs move-out semantics
  and its yield rules are clearly specified.

## Acceptance Criteria

- `print`, `println`, and `runtime::sleep` no longer allocate a per-call `Vec<Value>`
- the new steady-state host call benchmark shows a measurable improvement
- bind/load behavior remains unchanged except for any secondary effect from Stage 2 return changes

## Stage 4: Hot Builtin Projection Lowering

## Goal

Stop paying the full generic builtin `Call` boundary in the interpreter for projection-like
operations that are already recognized as specialized operations elsewhere.

## First-Wave Projection Set

Match the current specialized JIT/AOT understanding before widening scope:

- `string_len`
- `bytes_len`
- `string_get`
- `bytes_get`
- `array_len`
- `array_get`
- `array_has`
- `map_len`
- `map_get`
- `map_has`

Second-wave candidates after the first pass lands:

- `bytes_has`
- any additional projection builtin that shows up hot in the new benchmarks

## Work Items

1. Add interpreter-side direct lowering or superinstructions for the first-wave projection set.
2. Reuse existing operand-type metadata where possible instead of adding new user-visible syntax.
3. Keep JIT and AOT recognition aligned with the interpreter fast path.
4. Only introduce new bytecode opcodes if interpreter-side fast paths still leave too much
   dispatch overhead on the table.

## Acceptance Criteria

- interpreter-side array/map/string/bytes projection loops show a measurable latency reduction
- the JIT continues to see specialized operations instead of regressing back to generic call
  boundaries
- if new opcodes are added, VMBC encode/decode and disassembly are updated in the same stage

## Rollout Order

1. Add the missing steady-state host-call benchmark and the clone-heavy proc-macro builtin
   benchmark.
2. Land Stage 1.
3. Land Stage 2.
4. Land Stage 3.
5. Land Stage 4.

Reason for this order:

- Stage 1 removes the most obviously wrong heap behavior first.
- Stage 2 simplifies the common call result path before Stage 3 broadens zero-alloc dispatch.
- Stage 3 can then measure a cleaner host-call boundary without the return `Vec<Value>` noise.
- Stage 4 is best done last because it depends on the stabilized builtin behavior and perf data
  from the first three stages.

## Files Likely To Change

- [`pd-vm/pd-host-function/src/lib.rs`](../pd-vm/pd-host-function/src/lib.rs)
- [`pd-vm/src/builtins/runtime/typed.rs`](../pd-vm/src/builtins/runtime/typed.rs)
- [`pd-vm/src/builtins/runtime/mod.rs`](../pd-vm/src/builtins/runtime/mod.rs)
- [`pd-vm/src/builtins/runtime/core.rs`](../pd-vm/src/builtins/runtime/core.rs)
- [`pd-vm/src/builtins/runtime/bytes.rs`](../pd-vm/src/builtins/runtime/bytes.rs)
- [`pd-vm/src/builtins/runtime/host.rs`](../pd-vm/src/builtins/runtime/host.rs)
- [`pd-vm/src/vm/host.rs`](../pd-vm/src/vm/host.rs)
- [`pd-vm/src/vm/mod.rs`](../pd-vm/src/vm/mod.rs)
- [`pd-vm/src/compiler/codegen.rs`](../pd-vm/src/compiler/codegen.rs)
- [`pd-vm/src/assembler.rs`](../pd-vm/src/assembler.rs)
- [`pd-vm/src/bytecode.rs`](../pd-vm/src/bytecode.rs)
- [`pd-vm/src/vmbc.rs`](../pd-vm/src/vmbc.rs), if new opcodes are introduced
- [`pd-vm/src/vm/jit/recorder.rs`](../pd-vm/src/vm/jit/recorder.rs)
- [`pd-vm/src/vm/aot/ir.rs`](../pd-vm/src/vm/aot/ir.rs)
- [`pd-vm/src/vm/aot/ssa.rs`](../pd-vm/src/vm/aot/ssa.rs)
- [`pd-vm/src/vm/aot/compile.rs`](../pd-vm/src/vm/aot/compile.rs), only if Stage 4 needs parity
- [`pd-vm/tests/jit/perf_tests.rs`](../pd-vm/tests/jit/perf_tests.rs)
