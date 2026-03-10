# Implementation Plan (Reconstructed)

This is a best-effort reconstruction of the lost `implementation_plan.md`, rebuilt from:

- the surviving notes in [plans/type_infering_phase2](plans/type_infering_phase2)
- the landed compiler/VM/JIT changes
- the test suite and follow-up fixes completed through 2026-03-09

Where the original wording is ambiguous, this reconstruction prefers the final implemented state.

## Goal

Reduce dynamic dispatch and type ambiguity across the compiler, interpreter, trace JIT, wire formats, and frontend diagnostics, while adding enough verification to trust the inferred metadata.

## Phase 1: Emit and Preserve Type Metadata End-to-End

### Scope

- Add `TypeMap` to [pd-vm/src/bytecode.rs](pd-vm/src/bytecode.rs).
- Record inferred operand types at emitted bytecode offsets in [pd-vm/src/compiler/mod.rs](pd-vm/src/compiler/mod.rs).
- Keep local-slot type summaries for debug/runtime consumers, but treat them as lossy slot history rather than source binding truth.
- Persist `Program.type_map` through VMBC in [pd-vm/src/vmbc.rs](pd-vm/src/vmbc.rs).
- Persist the richer trace/AOT shape through [pd-vm/src/vm/jit/aot.rs](pd-vm/src/vm/jit/aot.rs).
- Extend host import metadata to carry explicit return types in [pd-vm/src/bytecode.rs](pd-vm/src/bytecode.rs).

### Deliverables

- `Program` carries `type_map: Option<TypeMap>`.
- `TypeMap` contains:
  - `local_types: Vec<ValueType>`
  - `operand_types: HashMap<usize, (ValueType, ValueType)>`
- Bytecode encode/decode preserves type metadata.
- Host import signatures can express known return types instead of defaulting everything to `Unknown`.

### Acceptance Criteria

- Compiling simple int/float/string arithmetic emits operand metadata for each binary op.
- Type metadata survives VMBC round-trips.
- Runtime consumers can access type metadata without recomputing it.

## Verification 1

- [pd-vm/tests/wire_tests.rs](pd-vm/tests/wire_tests.rs): round-trip `Program.type_map`
- [pd-vm/tests/compiler/type_inference_tests.rs](pd-vm/tests/compiler/type_inference_tests.rs): compiler attaches known operand types to emitted bytecode
- [pd-vm/tests/compiler/compiler_common_tests.rs](pd-vm/tests/compiler/compiler_common_tests.rs): compile/runtime sanity remains intact with the new metadata attached

## Phase 2: Use Metadata in the Interpreter and Trace JIT

### Scope

- Teach the interpreter in [pd-vm/src/vm/mod.rs](pd-vm/src/vm/mod.rs) to consume operand metadata for monomorphic fast paths.
- Avoid hot-path `HashMap` lookups by materializing a dense packed hint table once per VM.
- Teach the trace recorder in [pd-vm/src/vm/jit/trace.rs](pd-vm/src/vm/jit/trace.rs) to emit typed trace steps instead of falling back to generic arithmetic where types are known.
- Execute the typed steps in [pd-vm/src/vm/jit/runtime.rs](pd-vm/src/vm/jit/runtime.rs).
- Inline typed native lowering in [pd-vm/src/vm/jit/native/codegen.rs](pd-vm/src/vm/jit/native/codegen.rs), including float arithmetic and float comparisons/equality.

### Deliverables

- Typed arithmetic trace steps:
  - `IAdd`, `ISub`, `IMul`, `IDiv`, `IMod`, `INeg`
  - `FAdd`, `FSub`, `FMul`, `FDiv`, `FMod`, `FNeg`
  - `SConcat`
- Typed float comparison trace steps:
  - `FCeq`, `FClt`, `FCgt`
- Interpreter fast paths keyed by packed operand hints instead of repeated map probes.
- Native JIT/AOT lowering for typed float math and typed float compare/equality without helper-bridge fallback on the hot path.

### Acceptance Criteria

- Hot loops improve in interpreter mode relative to the pre-hint-table regression.
- Trace dumps show typed steps for known monomorphic operations.
- Float arithmetic and float compare/equality no longer hit the generic native helper bridge when recorded as typed steps.

## Verification 2

- [pd-vm/tests/jit_tests.rs](pd-vm/tests/jit_tests.rs): typed int/float/string trace-step coverage
- [pd-vm/tests/jit_tests.rs](pd-vm/tests/jit_tests.rs): typed float math lowers natively without helper-bridge hits
- [pd-vm/tests/jit_tests.rs](pd-vm/tests/jit_tests.rs): typed float compare/equality lowers natively without helper-bridge hits
- [pd-vm/tests/jit/perf_tests.rs](pd-vm/tests/jit/perf_tests.rs): `perf_jit_native_reduces_tight_loop_latency`

## Phase 3: Improve Inference Quality and Reject Known-Bad Programs Earlier

### Scope

- Propagate callable return types through:
  - direct function calls
  - function-valued locals
  - closure-valued locals
  - callable parameters
- Infer string-concat paths such as `"text" + 123` as string-producing operations.
- Preserve stable variable types across `for`/`while` loops instead of clearing all knowledge after loop analysis.
- Expand known return-type inference for builtins and generated namespace members.
- Honor explicit host return type signatures during inference.
- Infer `get`/index results from homogeneous arrays/maps when the element type is known.
- Reject incompatible known `if/else` branch result types and incompatible local merges in the compiler.
- Run `if/else` validation on legalized frontend IR so mismatches are caught before lifetime/drop rewriting introduces noise.

### Deliverables

- Operand metadata remains precise after:
  - function/closure propagation
  - loop-carried stable types
  - string-plus-number concat paths
  - typed container `get` operations
  - typed host/builtin return flows
- Incompatible `if/else` merges fail compilation with concrete type names.
- Loop-carried stable types survive nested loops and shadowing cases well enough for later validation to still work.

### Acceptance Criteria

- Tests validate inferred operand metadata rather than relying on lossy `local_types` summaries.
- Known-invalid `if/else` cases fail at compile time with readable diagnostics.
- More programs reach typed interpreter/JIT fast paths because inference survives common control-flow and call patterns.

## Verification 3

- [pd-vm/tests/compiler/type_inference_tests.rs](pd-vm/tests/compiler/type_inference_tests.rs):
  - callable return propagation through functions and closures
  - `"text" + 123` string-concat inference
  - loop stability for `for`/`while`
  - unstable loop demotion back to dynamic
  - nested-loop stability
  - homogeneous container `get` inference
  - explicit host return-type propagation
  - generated builtin/namespace return-type propagation
- [pd-vm/tests/compiler/compiler_common_tests.rs](pd-vm/tests/compiler/compiler_common_tests.rs):
  - data-driven `if/else` mismatch rejection cases
  - shadowed outer-local mismatch cases
  - loop-carried shadowed mismatch cases
- [pd-vm/tests/compiler/diagnostics_tests.rs](pd-vm/tests/compiler/diagnostics_tests.rs): rendered compile diagnostics for `if/else` mismatches
- [pd-vm/pd-vm-lint-wasm/src/lib.rs](pd-vm/pd-vm-lint-wasm/src/lib.rs): structured wasm diagnostic coverage
- [pd-vm/pd-vm-runtime-wasm/src/lib.rs](pd-vm/pd-vm-runtime-wasm/src/lib.rs): structured wasm runtime-analyzer coverage

## Frontend / Tooling Follow-Through

If frontend/editor changes are required to surface the new compiler behavior:

- Monaco markers should show line-aware compile diagnostics for type mismatches.
- The same marker rendering should be used in the main editor and the pd-controller debug-session source view.
- CLI compile errors should render through the same human-readable diagnostic path.

Implementation landed in:

- [pd-vm/src/compiler/diagnostics.rs](pd-vm/src/compiler/diagnostics.rs)
- [pd-vm/pd-vm-lint-wasm/src/analyzer.rs](pd-vm/pd-vm-lint-wasm/src/analyzer.rs)
- [pd-vm/pd-vm-runtime-wasm/src/analyzer.rs](pd-vm/pd-vm-runtime-wasm/src/analyzer.rs)
- [pd-controller/webui/src/app/monaco/lintMarkers.ts](pd-controller/webui/src/app/monaco/lintMarkers.ts)
- [pd-controller/webui/src/app/components/HighlightedCode.tsx](pd-controller/webui/src/app/components/HighlightedCode.tsx)
- [pd-controller/webui/src/app/hooks/useDebugSessions.ts](pd-controller/webui/src/app/hooks/useDebugSessions.ts)

## Notes and Constraints

- `local_types` is intentionally lossy. It summarizes slot history and compiler-inserted clears, not the stable semantic type of a source binding.
- Operand metadata is the primary signal for validating inference quality.
- Untyped trace steps such as generic `Add` still remain necessary for genuinely dynamic or mixed-type paths that the compiler cannot prove monomorphic.
- Backward compatibility was intentionally not preserved for wire/AOT versions when new metadata or typed trace-step encodings required a version bump.

## Deferred / Follow-Up Items

These remained outside the core recovered phases or were discussed as later cleanup:

- Reduce remaining reliance on generic untyped trace steps for mixed numeric paths.
- Decide whether `local_types` should grow a parallel non-lossy binding view.
- Continue expanding builtin/namespace signature coverage where return types are still unknown.
- Revisit whether additional typed compare/equality variants are worth adding for non-float monomorphic cases.
