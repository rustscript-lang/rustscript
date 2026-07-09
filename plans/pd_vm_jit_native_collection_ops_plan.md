# pd-vm JIT Native Collection Builtins Plan

## Summary

- Remove generic helper fallback for the array/map builtin surface in the trace JIT.
- Treat this as a dedicated collection-native effort, separate from the typed string/bytes concat
  plan.
- Under the current runtime representation, "full native" means these operations no longer go
  through generic `TraceStep::BuiltinCall`, `OP_BUILTIN_CALL`, or generic `OP_ADD` helper fallback.
  It does not imply reimplementing Rust `HashMap` internals directly in Cranelift.

Primary touch points:

- [`trace.rs`](../src/vm/jit/trace.rs)
- [`runtime.rs`](../src/vm/jit/runtime.rs)
- [`codegen.rs`](../src/vm/jit/native/codegen.rs)
- [`cranelift.rs`](../src/vm/jit/native/cranelift.rs)
- [`bridge.rs`](../src/vm/native/bridge.rs)
- [`core.rs`](../src/builtins/runtime/core.rs)
- [`bytecode.rs`](../src/bytecode.rs)
- [`jit_tests.rs`](../tests/jit/jit_tests.rs)

## Builtin Surface In Scope

Array helpers in scope:

- `len(array)`
- `count(array)`
- `slice(array, start, length)`
- `concat(array, array)` and `array + array`
- `array_new()`
- `array_push(array, value)`
- `get(array, index)`
- `has(array, index)`
- `set(array, index, value)`
- `keys(array)`

Map helpers in scope:

- `len(map)`
- `count(map)`
- `map_new()`
- `get(map, key)`
- `has(map, key)`
- `set(map, key, value)`
- `keys(map)`

## Current State

- `array + array` is not a typed collection op in the trace recorder today. It stays as generic
  `TraceStep::Add` in [`trace.rs`](../src/vm/jit/trace.rs).
- In native JIT, generic `TraceStep::Add` enters the int fast path in
  [`codegen.rs`](../src/vm/jit/native/codegen.rs), and non-int operands fall back to the
  generic native helper bridge. That means hot array concat still routes through helper handling.
- Collection builtins such as `len`, `count`, `slice(array, ...)`, `concat(array, array)`,
  `array_new`, `map_new`, `array_push`, `get`, `has`, `set`, and `keys` are emitted as
  `TraceStep::BuiltinCall` and always go through helper dispatch today.
- Native codegen explicitly declines to inline `TraceStep::BuiltinCall` in
  [`codegen.rs`](../src/vm/jit/native/codegen.rs), so these operations never get a dedicated
  native lowering.

## What "Full Native" Means Here

For this plan, "full native" means:

- no generic `OP_ADD` fallback for hot array concat
- no generic `OP_BUILTIN_CALL` fallback for hot collection builtins in scope
- generated native traces use dedicated collection-native lowering paths

Important constraint:

- arrays are backed by `Arc<Vec<Value>>`
- maps are backed by `Arc<VmMap>`
- `VmMap` itself wraps Rust `HashMap`

Because of that, a realistic first-class native plan uses dedicated native-runtime collection
intrinsics for clone, detach, lookup, insert, remove, and concat semantics. It should not continue
to rely on the generic VM helper dispatcher, but it also should not pretend that raw Cranelift
instructions alone can safely reproduce Rust `HashMap` behavior under the current storage model.

If the runtime storage model changes later to a VM-owned collection layout, a stricter
instruction-only plan can follow.

## Recommended Shape

Use typed collection trace steps rather than leaving these paths as generic add or builtin calls.

Recommended trace IR additions:

- extend the shared concat shape to include `TraceConcatKind::Array`
- add `TraceStep::ArrayNew`
- add `TraceStep::MapNew`
- add `TraceStep::ArrayPush`
- add `TraceStep::Len(TraceCollectionKind::{Array, Map})`
- add `TraceStep::Count(TraceCollectionKind::{Array, Map})`
- add `TraceStep::Get(TraceGetKind::{Array, Map})`
- add `TraceStep::Has(TraceHasKind::{Array, Map})`
- add `TraceStep::Set(TraceSetKind::{Array, Map})`
- add `TraceStep::Keys(TraceCollectionKind::{Array, Map})`
- add `TraceStep::Slice(TraceSliceKind::Array)`

Why this shape:

- array concat should share the same typed-concat family as string and bytes
- collection builtins should become first-class trace operations instead of hidden builtin calls
- it keeps the collection lowering explicit in traces, dumps, and tests

## Semantic Requirements

The native collection path must preserve current VM behavior.

Array requirements:

- `len(array)` and `count(array)` return the current element count
- `slice(array, start, length)` matches existing array slice semantics
- `array + array` and `concat(array, array)` produce a new array value
- `array_new()` returns an empty array
- `array_push(array, value)` returns an updated array
- `get(array, index)` returns a cloned element
- `has(array, index)` preserves current bounds semantics
- `set(array, index, value)` preserves current append-at-len behavior
- `keys(array)` returns an array of integer indices
- mutating a shared array must detach backing storage before write

Map requirements:

- `len(map)` and `count(map)` return the current entry count
- `map_new()` returns an empty map
- `get(map, key)` returns a cloned value
- `has(map, key)` preserves current key-presence semantics
- `set(map, key, value)` updates or inserts
- `set(map, key, null)` removes the entry
- `keys(map)` returns an array of keys
- mutating a shared map must detach backing storage before write
- heap-key identity semantics must remain unchanged for array/map keys stored in maps

## Implementation Plan

1. Add typed collection steps in [`trace.rs`](../src/vm/jit/trace.rs):
   - add `TraceCollectionKind::{Array, Map}`
   - extend `TraceConcatKind` with `Array`
   - add `TraceStep::ArrayNew`
   - add `TraceStep::MapNew`
   - add `TraceStep::ArrayPush`
   - add `TraceStep::Len(TraceCollectionKind)`
   - add `TraceStep::Count(TraceCollectionKind)`
   - add `TraceGetKind::{Array, Map}`
   - add `TraceHasKind::{Array, Map}`
   - add `TraceSetKind::{Array, Map}`
   - add `TraceStep::Keys(TraceCollectionKind)`
   - add `TraceSliceKind::Array`
   - add corresponding `TraceStep` variants and dump names

2. Teach the recorder to recognize collection ops:
   - lower `Add` with `(ValueType::Array, ValueType::Array)` to
     `TraceStep::Concat(TraceConcatKind::Array)`
   - recognize builtin-call indices for `len`, `count`, `slice`, `concat`, `array_new`, `map_new`,
     `array_push`, `get`, `has`, `set`, and `keys`
   - when operand types match array/map forms, emit typed collection steps instead of
     `TraceStep::BuiltinCall`
   - leave unsupported or untyped cases on the existing builtin-call path

3. Add explicit VM-side collection op methods in [`mod.rs`](../src/vm/mod.rs):
   - `array_new_op()`
   - `map_new_op()`
   - `array_push_op()`
   - `array_len_op()` / `map_len_op()`
   - `array_count_op()` / `map_count_op()`
   - `array_slice_op()`
   - `array_concat_op()`
   - `array_get_op()`
   - `array_has_op()`
   - `map_get_op()`
   - `map_has_op()`
   - `array_set_op()`
   - `map_set_op()`
   - `array_keys_op()`
   - `map_keys_op()`
   - keep these aligned with the current builtin semantics in
     [`core.rs`](../src/builtins/runtime/core.rs)

4. Add dedicated native collection intrinsics outside the generic helper dispatcher:
   - create narrow native-runtime entry points for collection operations, such as:
     - array new
     - map new
     - array len/count
     - map len/count
     - array slice
     - array concat
     - array push
     - array get clone
     - array has
     - array set with copy-on-write detach
     - map get clone
     - map has
     - map set/remove with copy-on-write detach
     - array keys
     - map keys
   - these intrinsics may live beside other low-level native runtime glue, but they must not be
     routed through generic `pd_vm_native_step(...)` opcode dispatch
   - they must preserve `Value` clone/drop semantics and `Arc` ownership rules

5. Lower typed collection steps in [`codegen.rs`](../src/vm/jit/native/codegen.rs):
   - replace generic-add fallback for array concat with dedicated array concat lowering
   - replace `BuiltinCall` helper fallback for typed collection builtins in scope with dedicated
     lowering
   - call only the narrow collection intrinsics needed for the operation, not the generic helper
     path

6. Update [`cranelift.rs`](../src/vm/jit/native/cranelift.rs):
   - plumb any new collection-intrinsic entry points required by native codegen
   - keep generic builtin helper plumbing only for operations not yet native-lowered

7. Keep the trace interpreter aligned in [`runtime.rs`](../src/vm/jit/runtime.rs):
   - dispatch new typed collection steps directly to the new VM op methods
   - avoid leaving collection behavior split between typed steps and builtin-call emulation

8. Clean up collection helper usage reporting:
   - hot array/map builtins in scope should stop appearing as helper bridge hits
   - generic helper stats should remain available for other unsupported builtins

## Tests And Acceptance Criteria

Add or update tests in [`jit_tests.rs`](../tests/jit/jit_tests.rs).

Required coverage:

- a trace-recording test proving hot `array + array` lowers to typed array concat instead of
  generic `Add`
- a trace-recording test proving hot builtin `len`, `count`, `slice(array, ...)`, `array_new`,
  `map_new`, `array_push`, `get`, `has`, `set`, and `keys` on arrays/maps lower to typed
  collection steps instead of `BuiltinCall`
- a native-execution test for hot array len/count/slice/new/push proving:
  - correct final result
  - no `"len"`, `"count"`, `"slice"`, `"array_new"`, or `"array_push"` builtin helper bridge hits
- a native-execution test for hot array concat proving:
  - correct final result
  - no `"add"` helper bridge hits
  - no `"concat"` builtin helper bridge hits
- a native-execution test for hot array get/has/set/keys proving:
  - correct final result
  - no `"get"`, `"has"`, `"set"`, or `"keys"` builtin helper bridge hits
  - copy-on-write detachment still holds for aliased arrays
- a native-execution test for hot map len/count/new/get/has/set/keys proving:
  - correct final result
  - no `"len"`, `"count"`, `"map_new"`, `"get"`, `"has"`, `"set"`, or `"keys"` builtin helper
    bridge hits
  - map key identity semantics still hold for heap keys
  - `set(map, key, null)` still removes entries

## Non-Goals

- no AOT work
- no storage-model rewrite replacing `Arc<Vec<Value>>` or `Arc<VmMap>`
- no attempt to make collection ops instruction-only if that would require duplicating Rust
  `HashMap` internals in Cranelift

## Acceptance Bar

This plan is complete when:

- hot array concat no longer routes through generic add helper fallback
- hot array/map builtins in scope no longer route through generic builtin helper dispatch
- native traces preserve current collection semantics, including alias-detach and map-key identity
- helper bridge stats stop reporting `add`, `len`, `count`, `slice`, `concat`, `array_new`,
  `map_new`, `array_push`, `get`, `has`, `set`, and `keys` for the native-lowered collection cases
