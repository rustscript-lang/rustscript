# pd-vm JIT Native String And Bytes Builtins Plan

## Summary

- Remove generic helper fallback for the string/bytes builtin surface in the trace JIT.
- Fold the existing `SConcat` string-special case into one broader typed string/bytes lowering
  plan instead of treating concat as the only operation worth specializing.
- Make the core sequence surface true inline native:
  - strings: `len`, `slice`, `concat` / `+`, `get`
  - bytes: `len`, `slice`, `concat` / `+`, `get`, `has`
- For the remaining `bytes::*` codec/conversion surface, require native lowering that does not
  route through generic `TraceStep::BuiltinCall`, `OP_BUILTIN_CALL`, or concat-specific helper
  bridges.
- Keep the scope tight: no broader language redesign and no AOT work.

Primary touch points:

- [`trace.rs`](../src/vm/jit/trace.rs)
- [`runtime.rs`](../src/vm/jit/runtime.rs)
- [`codegen.rs`](../src/vm/jit/native/codegen.rs)
- [`cranelift.rs`](../src/vm/jit/native/cranelift.rs)
- [`bridge.rs`](../src/vm/jit/native/bridge.rs)
- [`core.rs`](../src/builtins/runtime/core.rs)
- [`bytes.rs`](../src/builtins/runtime/bytes.rs)
- [`layout.rs`](../src/vm/native/layout.rs)
- [`bytecode.rs`](../src/bytecode.rs)
- [`mod.rs`](../src/vm/mod.rs)
- [`jit_tests.rs`](../tests/jit/jit_tests.rs)

## Builtin Surface In Scope

String helpers in scope:

- `len(string)`
- `slice(string, start, length)`
- `concat(string, string)` and `string + string`
- `get(string, index)`

Bytes helpers in scope:

- `len(bytes)`
- `slice(bytes, start, length)`
- `concat(bytes, bytes)` and `bytes + bytes`
- `get(bytes, index)`
- `has(bytes, index)`
- `bytes::from_utf8`
- `bytes::to_utf8`
- `bytes::to_utf8_lossy`
- `bytes::from_hex`
- `bytes::to_hex`
- `bytes::from_base64`
- `bytes::to_base64`
- `bytes::from_array_u8`
- `bytes::to_array_u8`

## Current State

- The VM already has `string_concat_op()` and `bytes_concat_op()` in
  [`mod.rs`](../src/vm/mod.rs), so interpreter semantics exist for both concat cases.
- The trace recorder only gives `string + string` a dedicated trace step today. In
  [`trace.rs`](../src/vm/jit/trace.rs), `OpCode::Add` lowers to `TraceStep::SConcat` for
  `ValueType::String`, while `bytes + bytes` still falls back to generic `TraceStep::Add`.
- Native JIT lowering still uses a dedicated `sconcat` helper path for string concat, which means
  it is not yet truly inline native.
- The rest of the string/bytes builtin surface, including `len`, `slice`, `get`, `has`, and all
  `bytes::*` codec/conversion builtins, is emitted as `TraceStep::BuiltinCall` and therefore
  always routes through generic helper dispatch today.
- Native codegen explicitly declines to inline `TraceStep::BuiltinCall` in
  [`codegen.rs`](../src/vm/jit/native/codegen.rs), so these operations never get dedicated
  native lowering.

## What "Native" Means Here

For this plan, "native" means:

- no generic `OP_ADD` fallback for hot string/bytes concat
- no generic `OP_BUILTIN_CALL` fallback for hot string/bytes builtins in scope
- generated native traces use dedicated string/bytes-native lowering paths

Important constraints:

- strings are backed by `Arc<String>`
- bytes are backed by `Arc<Vec<u8>>`
- string `len`, `slice`, and `get` are Unicode-character based, not byte-index based
- several `bytes::*` builtins are codec boundaries rather than simple scalar operations

Because of that, the required target is:

- the core sequence surface is inline-native:
  - `len(string)`
  - `slice(string, start, length)`
  - `concat(string, string)` and `string + string`
  - `get(string, index)`
  - `len(bytes)`
  - `slice(bytes, start, length)`
  - `concat(bytes, bytes)` and `bytes + bytes`
  - `get(bytes, index)`
  - `has(bytes, index)`
- the `bytes::*` codec/conversion builtins may use dedicated narrow native-runtime intrinsics, but
  they must not continue to rely on the generic builtin helper dispatcher

This plan explicitly rejects the current state where string/bytes builtins remain hidden behind
generic builtin-call fallback.

## Recommended Shape

Use typed string/bytes trace steps rather than leaving these paths as generic add or builtin calls.

Recommended trace IR additions:

- replace `TraceStep::SConcat` with `TraceStep::Concat(TraceConcatKind)`
- add `TraceConcatKind::{String, Bytes}`
- add `TraceTextBytesKind::{String, Bytes}`
- add `TraceStep::Len(TraceTextBytesKind)`
- add `TraceStep::Slice(TraceTextBytesKind)`
- add `TraceStep::Get(TraceTextBytesKind)`
- add `TraceStep::HasBytes`
- add `TraceStep::BytesCodec(TraceBytesCodecKind)`

Recommended codec kinds:

- `FromUtf8`
- `ToUtf8`
- `ToUtf8Lossy`
- `FromHex`
- `ToHex`
- `FromBase64`
- `ToBase64`
- `FromArrayU8`
- `ToArrayU8`

Why this shape:

- it retires `SConcat` as a string-only one-off
- it makes generic string/bytes sequence builtins first-class in traces
- it gives the `bytes::*` namespace explicit lowering points instead of hiding them in builtin call
  metadata
- it keeps the trace dump and tests honest about which operations are still generic fallback

## Semantic Requirements

String requirements:

- `len(string)` preserves current character-count semantics
- `slice(string, start, length)` preserves current character-slice semantics
- `concat(string, string)` and `string + string` preserve current result semantics
- `get(string, index)` preserves current character extraction semantics
- all four operations above must be true inline native in the JIT path

Bytes requirements:

- `len(bytes)` returns byte length
- `slice(bytes, start, length)` preserves current byte-slice semantics
- `concat(bytes, bytes)` and `bytes + bytes` preserve current result semantics
- `get(bytes, index)` returns an `int` in `0..=255`
- `has(bytes, index)` preserves current bounds semantics
- all five operations above must be true inline native in the JIT path
- UTF-8, hex, base64, and array-u8 conversion builtins preserve current validation and error
  behavior

Cross-plan note:

- `bytes::from_array_u8` and `bytes::to_array_u8` interact directly with the array-native plan in
  [pd_vm_jit_native_collection_ops_plan.md](../plans/pd_vm_jit_native_collection_ops_plan.md).
  They should share low-level array cloning/building machinery rather than inventing a separate
  path.

## Implementation Plan

1. Refactor typed string/bytes representation in [`trace.rs`](../src/vm/jit/trace.rs):
   - add `TraceTextBytesKind::{String, Bytes}`
   - replace `TraceStep::SConcat` with `TraceStep::Concat(TraceConcatKind)`
   - add `TraceStep::Len(TraceTextBytesKind)`
   - add `TraceStep::Slice(TraceTextBytesKind)`
   - add `TraceStep::Get(TraceTextBytesKind)`
   - add `TraceStep::HasBytes`
   - add `TraceBytesCodecKind`
   - add `TraceStep::BytesCodec(TraceBytesCodecKind)`
   - update dump names and any debug helpers accordingly

2. Teach the recorder to recognize string/bytes ops:
   - lower `Add` with `(ValueType::String, ValueType::String)` to
     `TraceStep::Concat(TraceConcatKind::String)`
   - lower `Add` with `(ValueType::Bytes, ValueType::Bytes)` to
     `TraceStep::Concat(TraceConcatKind::Bytes)`
   - recognize builtin-call indices for `len`, `slice`, `get`, `has`, and the `bytes::*` namespace
     builtins in scope
   - when operand types match string/bytes forms, emit typed string/bytes steps instead of
     `TraceStep::BuiltinCall`
   - leave unsupported or untyped cases on the existing builtin-call path

3. Add explicit VM-side string/bytes op methods in [`mod.rs`](../src/vm/mod.rs):
   - `string_len_op()`
   - `bytes_len_op()`
   - `string_slice_op()`
   - `bytes_slice_op()`
   - `string_concat_op()`
   - `bytes_concat_op()`
   - `string_get_op()`
   - `bytes_get_op()`
   - `bytes_has_op()`
   - `bytes_from_utf8_op()`
   - `bytes_to_utf8_op()`
   - `bytes_to_utf8_lossy_op()`
   - `bytes_from_hex_op()`
   - `bytes_to_hex_op()`
   - `bytes_from_base64_op()`
   - `bytes_to_base64_op()`
   - `bytes_from_array_u8_op()`
   - `bytes_to_array_u8_op()`
   - keep them aligned with the current builtin semantics in
     [`core.rs`](../src/builtins/runtime/core.rs) and
     [`bytes.rs`](../src/builtins/runtime/bytes.rs)

4. Add the native data-path support needed by [`codegen.rs`](../src/vm/jit/native/codegen.rs):
   - use the existing probed value layout from [`layout.rs`](../src/vm/native/layout.rs)
   - make the plan explicit about the heap-backed representations involved:
     - `Value::String(Arc<String>)`
     - `Value::Bytes(Arc<Vec<u8>>)`
   - define the minimum native-accessible layout information or native-runtime intrinsics needed to
     build, validate, and clone those result values correctly

5. Replace helper-based concat lowering in [`codegen.rs`](../src/vm/jit/native/codegen.rs)
   with real inline native lowering for `TraceStep::Concat(kind)`:
   - verify stack length and operand tags
   - load lhs and rhs payload pointers and lengths
   - compute total output length with overflow guards
   - allocate output backing storage
   - copy lhs and rhs payload contents with emitted native memory operations
   - construct the resulting `Value::String` or `Value::Bytes` directly into the stack slot
   - shrink the VM stack from two operands to one result without routing through a concat helper

6. Lower the core string/bytes sequence surface as true inline native:
   - inline-native string operations:
     - `len(string)`
     - `slice(string, start, length)`
     - `get(string, index)`
   - inline-native bytes operations:
     - `len(bytes)`
     - `slice(bytes, start, length)`
     - `get(bytes, index)`
     - `has(bytes, index)`
   - these operations must not go through either:
     - generic builtin helper dispatch
     - dedicated narrow runtime intrinsics
   - the generated trace code itself must perform the necessary validation, indexing, character
     counting, slicing, allocation, and result-value construction

7. Lower the remaining `bytes::*` codec/conversion steps natively:
   - dedicated narrow native-runtime intrinsics are acceptable for:
     - `bytes::from_utf8`
     - `bytes::to_utf8`
     - `bytes::to_utf8_lossy`
     - `bytes::from_hex`
     - `bytes::to_hex`
     - `bytes::from_base64`
     - `bytes::to_base64`
     - `bytes::from_array_u8`
     - `bytes::to_array_u8`
   - these must not go through generic builtin helper dispatch

8. Update [`cranelift.rs`](../src/vm/jit/native/cranelift.rs):
   - remove concat-helper address plumbing from trace compilation
   - plumb any new dedicated string/bytes intrinsic entry points required by native codegen
   - keep generic builtin helper plumbing only for operations not yet native-lowered

9. Clean up obsolete concat-helper code in [`bridge.rs`](../src/vm/jit/native/bridge.rs):
   - remove the dedicated `sconcat` bridge entry once string concat is fully inline
   - do not add a new dedicated `bconcat` bridge op
   - leave the generic helper bridge for non-string/bytes operations unchanged

10. Keep the trace interpreter aligned in [`runtime.rs`](../src/vm/jit/runtime.rs):
   - dispatch the new typed string/bytes steps directly to the new VM op methods
   - avoid leaving string/bytes behavior split between typed steps and builtin-call emulation

11. Keep the scope intentionally narrow:
   - do not redesign generic `Add`
   - do not redesign the string model or introduce implicit coercions
   - do not add AOT support
   - do not accept a plan where string/bytes builtins in scope remain primarily implemented through
     generic builtin helper dispatch

## Tests And Acceptance Criteria

Add or update tests in [`jit_tests.rs`](../tests/jit/jit_tests.rs).

Required coverage:

- a trace-recording test proving hot `string + string` lowers to
  `TraceStep::Concat(TraceConcatKind::String)` and no longer refers to `SConcat`
- a trace-recording test proving hot `bytes + bytes` lowers to
  `TraceStep::Concat(TraceConcatKind::Bytes)` instead of generic `Add`
- a trace-recording test proving hot `len`, `slice`, `get`, `has`, and `bytes::*` builtins in
  scope lower to typed string/bytes steps instead of `BuiltinCall`
- a native-execution test for hot string concat proving:
  - correct final result
  - no `"add"` helper bridge hits
  - no `"sconcat"` helper bridge hits
- a native-execution test for hot bytes concat proving:
  - correct final bytes result
  - no `"add"` helper bridge hits
  - no concat-related helper bridge hits
- a native-execution test for hot bytes len/get/has/slice proving:
  - correct final result
  - no `"len"`, `"get"`, `"has"`, or `"slice"` builtin helper bridge hits
  - no string/bytes-specific runtime intrinsic hits for those operations
- a native-execution test for hot string len/get/slice proving:
  - correct final result under current Unicode semantics
  - no `"len"`, `"get"`, or `"slice"` builtin helper bridge hits
  - no string/bytes-specific runtime intrinsic hits for those operations
- a native-execution test for `bytes::*` codec/conversion helpers in scope proving:
  - correct results and error behavior
  - no generic builtin helper bridge hits for `bytes::from_utf8`, `bytes::to_utf8`,
    `bytes::to_utf8_lossy`, `bytes::from_hex`, `bytes::to_hex`, `bytes::from_base64`,
    `bytes::to_base64`, `bytes::from_array_u8`, and `bytes::to_array_u8`

Recommended example test shape:

- compile hot loops or repeated calls around `b"x"` and `"x"` sequence operations
- enable native bridge stats
- assert the trace snapshot contains the expected typed string/bytes steps
- assert bridge stats do not contain `"add"`, `"sconcat"`, or the relevant builtin names
- optionally inspect the native trace dump to confirm concat stayed inline

## Non-Goals

- no attempt to specialize `bytes + string`, `string + bytes`, or any implicit coercion
- no AOT work
- no broader string or bytes language redesign outside the builtins listed above
- no requirement that every `bytes::*` codec builtin be instruction-only if a narrow dedicated
  native intrinsic is the safer implementation

## Acceptance Bar

This plan is complete when:

- hot `string + string` traces stop using `SConcat` and instead use the shared concat step
- hot `bytes + bytes` traces stop lowering to generic `Add`
- string/bytes builtins in scope stop routing through generic builtin helper dispatch
- the core sequence surface:
  - strings: `len`, `slice`, `concat` / `+`, `get`
  - bytes: `len`, `slice`, `concat` / `+`, `get`, `has`
  executes as true inline native code
- native traces preserve current string Unicode semantics and bytes codec semantics
- helper bridge stats stop reporting `add`, `sconcat`, `len`, `slice`, `get`, `has`, and the
  `bytes::*` builtin names for the native-lowered string/bytes cases
