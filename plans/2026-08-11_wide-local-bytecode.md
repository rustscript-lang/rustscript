# Wide Local Bytecode Implementation Plan

**Goal:** Support frames with up to 65,536 addressable local slots by adding wide local load/store instructions while preserving the compact one-byte encoding for slots 0 through 255.

**Architecture:** Keep `LocalSlot = u16` as the compiler/runtime semantic index. Preserve existing short `Ldloc` and `Stloc` opcode values and add `LdlocWide`/`StlocWide` with little-endian `u16` operands. Decode both forms immediately into one normalized `u16` slot type, then carry that type through debug metadata, VMBC, interpreter, JIT, AOT, native bridges, CLI tooling, wasm analysis, and `pd-vm-nostd`.

**Tech Stack:** Rust 2024, RustScript compiler and allocator, bytecode assembler/decoder, VMBC, interpreter, Trace JIT/Cranelift native backend, AOT artifacts, debugger/REPL, wasm analyzer, and `pd-vm-nostd`.

---

## Status and dependency

- Status: proposed.
- Depends on `2026-08-11_frame-aware-local-allocation.md`.
- Begin only after named-call liveness no longer counts separate callee frames as simultaneous local pressure.
- Wide encoding addresses genuine same-frame pressure. It must not become a workaround for stale liveness or unconditional hidden callable allocation.
- The dependency plan reserves `CallScript = 0x1A` and VMBC V12. This plan therefore reserves `LdlocWide = 0x1B`, `StlocWide = 0x1C`, and VMBC V13. Recheck the opcode table and current wire constants at implementation start; if another accepted plan has consumed those values, update this plan and all fixtures before writing code.

## Current baseline

The current tree has mixed local-index widths:

| Surface | Current representation |
| --- | --- |
| Frontend/compiler slot | `LocalSlot = u16` |
| Program/frame local count | vector length / `usize` |
| Short bytecode local operand | `u8` |
| `Assembler::ldloc/stloc` | `u8` |
| `DebugInfo::LocalInfo.index` | `u8` |
| Interpreter local helpers | primarily `u8` |
| Trace JIT local/source metadata | primarily `u8` |
| AOT IR/provenance | primarily `u8` |
| `pd-vm-nostd` local helpers | `u8` |
| VMBC baseline after dependency plan | V12 |

Current codegen rejects an aggregate frame above 256 before debug registration or bytecode emission. The allocator also uses a 256-entry used-color set. Merely widening one emission helper would therefore leave earlier compiler gates and downstream decoders inconsistent.

## Encoding contract

After the dependency plan, the opcode table is expected to contain:

```text
CallValue = 0x19
CallScript = 0x1A
LdlocWide = 0x1B
StlocWide = 0x1C
```

Local access encoding:

```text
slot 0..=255
  Ldloc  <u8>
  Stloc  <u8>

slot 256..=65535
  LdlocWide <u16 little-endian>
  StlocWide <u16 little-endian>
```

Rules:

- Existing `Ldloc`/`Stloc` opcode values and operand widths never change.
- Wide instructions always carry exactly two operand bytes.
- Compiler emission always chooses the shortest valid encoding.
- Decoders normalize either form to `u16` before semantic processing.
- Slot 255 uses short form; slot 256 uses wide form.
- Local count is one greater than the largest referenced slot, subject to frame metadata and parameter/capture slots.
- The maximum valid slot index is 65,535; the maximum valid local count is 65,536.
- Truncated wide operands return typed bounds/validation errors and never read a partial index.
- Branch offsets remain byte offsets and account for the wider instruction length.

## Compatibility contract

### Raw bytecode

- Existing short instruction bytes remain unchanged.
- Old runtimes reject unknown wide opcodes; they must not reinterpret operands as independent instructions.
- New runtimes decode both short and wide forms.

### VMBC

- V12 remains the encoding for programs whose bytecode and debug metadata contain only short local indices.
- V13 is required when bytecode contains `LdlocWide`/`StlocWide` or any debug local index exceeds 255.
- The V13 decoder accepts both short and wide instructions.
- The new decoder accepts both V12 and V13.
- V12 debug local indices remain one byte; V13 debug local indices are little-endian `u16`.
- The encoder selects the minimum compatible version from validated program contents; callers do not choose it manually.
- Short-only V12 output remains byte-for-byte identical to the baseline after the dependency plan.

### AOT/native artifacts

- Increment AOT artifact format and callable/native ABI revisions once.
- Mark wide-local artifacts with an explicit supported flag or revision so an older loader rejects them before execution.
- Do not make artifact consumers infer wide-local support from frame length alone.
- Regenerate checked-in artifact fixtures under the new declared version policy.

## Semantic invariants

- Move-by-default `Ldloc`, copy/borrow behavior, and capture-cell routing remain unchanged.
- `LdlocWide` has exactly the same ownership semantics as `Ldloc`.
- `StlocWide` has exactly the same replacement/drop semantics as `Stloc`.
- Frame-relative addressing and `local_base` remain the only way to obtain an absolute local index.
- Parameters, captures, hidden materialized callables, debugger locals, deopt restoration, and AOT exits can all address slots above 255.
- Short programs do not pay an extra byte per local access.
- Optimizers may initially decline wide-local frames using an explicit reason, but every scanner must decode instruction boundaries correctly.
- No backend may silently truncate `u16` to `u8`.

## Scope boundary

### In scope

- Dynamic graph-color bookkeeping up to `LocalSlot::MAX`.
- Additive wide load/store opcodes.
- Slot-aware assembler/compiler APIs.
- Debug metadata and lookup widening.
- VMBC V12/V13 dual decoding and version selection.
- Interpreter and no-std wide execution.
- JIT scanner/recorder/native index normalization and explicit admission behavior.
- AOT IR, SSA, lowering, runtime restoration, and artifact compatibility.
- REPL/debugger/wasm bytecode scanning and local synchronization.
- Tests using roughly 300 genuinely simultaneous locals in one frame.

### Out of scope

- Local indices wider than `u16`.
- Wide constant, host call, prototype, argument-count, branch, stack, or capture-ID operands.
- Replacing the bytecode ISA with LEB128 or a generic prefix encoding.
- Changing short opcode values or widening their operands in place.
- Changing frame-aware liveness or named callable materialization; those belong to the dependency plan.
- Removing existing JIT profitability limits solely to make a 300-local trace compile natively.
- Agent-specific source changes or compiler exceptions.
- A new VMBC migration service or automatic rewriting of external stored artifacts.

## Shared implementation type

Introduce or standardize one normalized runtime-facing type:

```rust
pub type RuntimeLocalIndex = u16;
```

A newtype is acceptable if it reduces accidental narrowing:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RuntimeLocalIndex(u16);
```

Selection criteria:

- Prefer a newtype if it can be introduced without large public API churn.
- Provide `as_usize()` and checked constructors from `usize`.
- Do not expose unchecked `as u8` conversions.
- `LocalSlot` may remain the compiler IR type; define an explicit conversion at the codegen boundary if compiler and runtime types stay separate.

## Milestone 1: Add boundary and compatibility tests

**Objective:** Establish RED coverage for slots 255, 256, 299, and 65,535 before changing emission.

**Files:**

- Modify: `tests/compiler/compiler_common_tests.rs`
- Modify: `tests/compiler/compiler_rustscript_tests.rs`
- Modify: `tests/vm_tests.rs` or focused files under `tests/vm/`
- Modify: `tests/wire_tests.rs` or focused files under `tests/wire/`
- Modify: `tests/compiler/compiler_common_tests.rs`
- Modify: `tests/wire/assembler_vmbc_edge_tests.rs`
- Modify: `tests/vm/runtime_state_edge_tests.rs`
- Modify: `pd-vm-nostd/tests/`
- Create: generated-source helpers under `tests/common/` if no suitable helper exists

**Test source design:**

Generate one function with approximately 300 locals that remain simultaneously live until the final expression. Sequential declarations followed by a final reduction or tuple/array construction are valid only if liveness proves every value remains required. Add an assertion on resulting frame local count so an optimizer cannot accidentally turn the fixture into a low-pressure test.

**Steps:**

1. Add a 256-local boundary fixture whose highest referenced slot is 255. Assert short `Ldloc`/`Stloc` emission.
2. Add a 257-local fixture whose highest referenced slot is 256. Current behavior must be RED with the real frame-limit diagnostic from the dependency plan.
3. Add a roughly 300-local fixture that returns a value sourced from a slot above 255.
4. Add a hand-assembled boundary test that emits slot 255 and slot 256 through the future slot-aware assembler API.
5. Add expected decode failures for one-byte and zero-byte truncated wide operands.
6. Add a small-program VMBC golden fixture and record its exact V12 bytes before implementation.
7. Add debugger metadata coverage for a named local at slot 299.
8. Add a true maximum-index structural test for slot 65,535 without allocating an excessive source AST. Use hand assembly/program metadata and a locals vector sized 65,536 only in a bounded test.
9. Add an overflow test for requested local count 65,537 or source slot 65,536, expecting a typed compiler/assembler error.

**Focused RED command:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/wide-local-target \
  cargo test --locked --test compiler_tests wide_local
```

Expected before implementation: slot-256/high-slot success cases fail at the compiler or assembler boundary; slot-255 and short VMBC baselines pass.

Do not commit failing tests alone. Commit the tests with the first minimal implementation milestone that makes their scoped subset pass.

## Milestone 2: Widen allocator bookkeeping without weakening pressure checks

**Objective:** Allow graph coloring to select physical colors above 255 while retaining the `u16` semantic ceiling.

**Files:**

- Modify: `src/compiler/lifetime/liveness.rs`
- Modify: `src/compiler/lifetime/availability.rs`
- Modify: `src/compiler/mod.rs`
- Test: compiler local-compaction and true-live-pressure tests

**Steps:**

1. Replace fixed `[bool; 256]` or equivalent used-color storage with a dynamically sized bitset/vector bounded by the compilation unit's candidate local count and 65,536.
2. Keep deterministic first-fit color selection so existing short programs retain their current slot assignment where graph order is unchanged.
3. Convert candidate colors to `LocalSlot` with checked conversion; never wrap an oversized `usize`.
4. Distinguish:
   - graph cannot allocate because more than 65,536 colors are genuinely required;
   - codegen needs wide bytecode because color exceeds 255;
   - aggregate count arithmetic overflow.
5. Remove the 256-color allocator error path only after wide codegen exists in the same passing commit series.
6. Add tests proving:
   - 300 overlapping values produce a highest slot at or above 299;
   - 300 non-overlapping values still compact into a small short range;
   - deterministic source recompilation yields identical slot assignment and bytecode;
   - 65,537 overlapping colors produce a typed upper-bound error.
7. Check memory behavior of the allocator with a 65,536-slot synthetic graph. Avoid an unconditional dense 65,536-by-65,536 matrix; adjacency remains sparse or uses existing graph sets.

**Focused command:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/wide-local-target \
  cargo test --locked --test compiler_tests local_slot_allocator
```

**Commit:**

```bash
git add src/compiler/lifetime/liveness.rs \
        src/compiler/lifetime/availability.rs src/compiler/mod.rs \
        tests/compiler/compiler_common_tests.rs
git commit -m "feat(compiler): allocate u16 local slots"
```

## Milestone 3: Add wide opcodes and slot-aware assembler emission

**Objective:** Define the additive ISA and preserve existing short assembler APIs.

**Files:**

- Modify: `src/bytecode.rs`
- Modify: `src/assembler.rs`
- Modify: assembler/disassembler tests

**Steps:**

1. Add `OpCode::LdlocWide = 0x1B` and `OpCode::StlocWide = 0x1C` after confirming `CallScript = 0x1A` from the dependency plan.
2. Return operand length 2 for both wide opcodes.
3. Add little-endian `emit_u16` and checked `read_u16` helpers where a shared helper does not already exist.
4. Preserve:

```rust
pub fn ldloc(&mut self, index: u8)
pub fn stloc(&mut self, index: u8)
```

5. Add slot-oriented APIs:

```rust
pub fn ldloc_slot(&mut self, index: u16)
pub fn stloc_slot(&mut self, index: u16)
```

6. Have slot-oriented APIs emit short form for `index <= 255`, wide form otherwise.
7. Update text assembly and numeric parsing to accept local indices through 65,535 and reject larger/negative values with source-aware errors.
8. Update disassembly and local-count inference to normalize both forms to `u16`.
9. Audit every handwritten bytecode walker. Generic `operand_len()` loops should adapt automatically; fixed `ip += 2` assumptions require explicit changes.
10. Add exact byte tests:

```text
ldloc_slot(255) -> [Ldloc, 0xff]
ldloc_slot(256) -> [LdlocWide, 0x00, 0x01]
stloc_slot(65535) -> [StlocWide, 0xff, 0xff]
```

11. Confirm old `ldloc(u8)` callers emit unchanged bytes.

**Focused commands:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/wide-local-target \
  cargo test --locked assembler wide_local
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/wide-local-target \
  cargo test --locked bytecode wide_local
```

Each filter must select at least one test.

**Commit:**

```bash
git add src/bytecode.rs src/assembler.rs
git commit -m "feat(bytecode): add wide local access opcodes"
```

## Milestone 4: Emit wide locals from codegen and widen debug metadata

**Objective:** Remove all compiler/debug narrowing before bytecode emission.

**Files:**

- Modify: `src/compiler/codegen.rs`
- Modify: `src/debug_info.rs`
- Modify: `src/compiler/diagnostics.rs`
- Modify: debug metadata consumers in `src/vmbc.rs`, `src/cli.rs`, and `pd-vm-wasm/src/analyzer.rs`
- Modify: compiler and debugger tests

**Steps:**

1. Replace `u8::try_from(LocalSlot)` gates in local load/store emission with `Assembler::ldloc_slot` and `stloc_slot`.
2. Keep aggregate frame validation at `<= 65,536`; update the diagnostic maximum accordingly.
3. Change `LocalInfo.index` to `u16` or the normalized local-index newtype.
4. Change debug builder methods and lookups to accept the widened type.
5. Change `DebugInfo::local_index` to return `Option<u16>`. If public source compatibility must be preserved, add a clearly named checked short helper rather than truncating:

```rust
pub fn local_index_u8(&self, name: &str) -> Option<u8>
```

6. Update compiler debug registration before emission so a local above 255 never fails in metadata first.
7. Update local range/lifetime metadata and debugger rendering for high slots.
8. Add tests for source name lookup, declared/last line ranges, breakpoints, and local display at slot 299.
9. Verify slot 255 still uses short bytecode even though debug metadata is now `u16`.
10. Search the compiler and debugger trees for every `local as u8`, `index: u8`, and `Option<u8>` tied to locals. Replace or justify each occurrence in the plan execution notes.

**Focused commands:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/wide-local-target \
  cargo test --locked --test compiler_tests wide_local
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/wide-local-target \
  cargo test --locked --test compiler_tests debug_info
```

**Commit:**

```bash
git add src/compiler/codegen.rs src/compiler/diagnostics.rs \
        src/debug_info.rs src/vmbc.rs src/cli.rs pd-vm-wasm/src/analyzer.rs \
        tests/compiler/compiler_common_tests.rs \
        tests/wire/assembler_vmbc_edge_tests.rs \
        tests/vm/runtime_state_edge_tests.rs
git commit -m "feat(compiler): emit and describe wide locals"
```

Stage exact files during implementation.

## Milestone 5: Execute wide locals in the interpreter

**Objective:** Make the primary VM execute short and wide local operations through one semantic path.

**Files:**

- Modify: `src/vm/mod.rs`
- Modify: `src/vm/instance.rs` if local helpers are owned there
- Modify: `src/vm/superinstructions.rs`
- Test: VM, capture, drop-contract, suspension, and recursion tests

**Steps:**

1. Normalize decoded local indices to `u16` immediately:

```rust
OpCode::Ldloc => read_u8().map(u16::from)
OpCode::LdlocWide => read_u16_le()
```

2. Change internal `absolute_local`, load, store, capture-cell lookup, and error constructors to accept `u16` or the normalized newtype.
3. Retain public `set_local(u8, ...)` where useful and add `set_local_slot(u16, ...)`; route both through one implementation.
4. Keep frame-base addition checked in `usize` and verify the result lies within the active frame, not merely within the whole locals vector.
5. Route short and wide load through the same ownership/capture-cell helper.
6. Route short and wide store through the same replacement/drop/capture-cell helper.
7. Let wide operations bypass existing short-only superinstructions initially. Update fusion scanners so they skip the correct width and never fuse across a wide operand.
8. Add interpreter tests for:
   - moved and copied high-slot values;
   - store replacement/drop at slot 299;
   - borrowed capture cell at a high slot;
   - nested frames where caller and callee both use high slots;
   - yield/wait/resume with a high slot live;
   - invalid high slot relative to active frame;
   - truncated operand errors.
9. Execute the generated 300-live-local program and assert its final value.

**Focused command:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/wide-local-target \
  cargo test --locked --test vm_tests wide_local
```

**Commit:**

```bash
git add src/vm/mod.rs src/vm/instance.rs \
        src/vm/superinstructions.rs tests/vm/
git commit -m "feat(vm): execute wide local accesses"
```

## Milestone 6: Add VMBC V13 with dual-version decoding

**Objective:** Serialize wide bytecode/debug metadata without changing short-only V12 artifacts.

**Files:**

- Modify: `src/vmbc.rs`
- Modify: VMBC validators/disassemblers in `src/`
- Modify: `tests/wire_tests.rs` and wire fixtures
- Modify later in parity: `pd-vm-nostd/src/vmbc.rs`

**Steps:**

1. Define `VERSION_V12` and `VERSION_V13`; stop using a single accepted-version equality check.
2. Add a validated program scan that determines whether wide bytecode or wide debug indices exist.
3. Encode V12 when all local accesses/debug entries fit short form; encode V13 otherwise.
4. Decode V12 using one-byte debug local indices and reject wide opcodes.
5. Decode V13 using `u16` debug local indices and accept both short and wide local opcodes.
6. Validate instruction boundaries before deriving local count.
7. Infer local count using normalized `u16`; do not cap inferred count at 256.
8. Ensure frame/prototype local counts can represent 65,536 even though the highest index is `u16::MAX`.
9. Add tests for:
   - byte-identical short-only V12 golden artifact;
   - V13 round trip with slot 256 and debug slot 299;
   - V13 containing only short access instructions but a wide debug index;
   - V12 rejection of wide opcodes;
   - old/unknown version rejection;
   - truncated wide bytecode;
   - count overflow/inconsistent frame metadata;
   - disassembly showing 256/299 exactly.
10. Document the version decision near constants and wire format docs.

**Focused command:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/wide-local-target \
  cargo test --locked --test wire_tests wide_local
```

**Commit:**

```bash
git add src/vmbc.rs tests/wire_tests.rs tests/fixtures/
git commit -m "feat(vmbc): encode wide local metadata in v13"
```

Stage exact fixture paths during implementation.

## Milestone 7: Mirror wide locals in `pd-vm-nostd`

**Objective:** Decode and execute the same V12/V13 contract without allocation-dependent shortcuts.

**Files:**

- Modify: `pd-vm-nostd/src/program.rs`
- Modify: `pd-vm-nostd/src/vm.rs`
- Modify: `pd-vm-nostd/src/error.rs`
- Modify: `pd-vm-nostd/src/vmbc.rs`
- Modify: `pd-vm-nostd/tests/`

**Steps:**

1. Mirror opcode values and operand lengths exactly.
2. Widen local-count inference to `usize` with normalized `u16` indices.
3. Add `read_u16_le` without unaligned pointer reads.
4. Change internal absolute/store/load helpers to accept `u16`.
5. Preserve public short APIs and add slot-aware alternatives where needed.
6. Decode both V12 and V13 under the same rules as std VMBC.
7. Execute a V13 fixture generated by the std encoder; do not duplicate a hand-maintained fixture if the test harness can share bytes.
8. Add no-std tests for slot 255, 256, 299, invalid frame-relative access, and truncated operands.
9. Verify feature-minimal/no-default-feature builds if supported by the crate.

**Focused commands:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/wide-local-target \
  cargo test --locked -p pd-vm-nostd wide_local
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/wide-local-target \
  cargo check --locked -p pd-vm-nostd --no-default-features
```

**Commit:**

```bash
git add pd-vm-nostd/src/ pd-vm-nostd/tests/
git commit -m "feat(nostd): execute v13 wide locals"
```

Stage exact files during implementation.

## Milestone 8: Make Trace JIT and native scanners width-safe

**Objective:** Prevent trace/JIT corruption and carry normalized indices even when policy declines a wide frame.

**Files:**

- Modify: `src/vm/jit/trace.rs`
- Modify: `src/vm/jit/recorder.rs`
- Modify: `src/vm/jit/inline.rs`
- Modify: `src/vm/jit/deopt.rs`
- Modify: `src/vm/jit/native/`
- Modify: `src/vm/native/bridge.rs`
- Modify: `src/vm/native/mod.rs`
- Test: JIT trace scanner, recorder, native bridge, and deopt tests

**Steps:**

1. Update all trace/header/inlining scanners to recognize two-byte wide operands and preserve instruction boundaries.
2. Change `source_local`, recorded `Ldloc/Stloc` indices, symbolic local maps, invalid-local errors, and native helper parameters from `u8` to `u16` or the normalized type.
3. Remove `enumerate() as u8` and equivalent narrowing in restoration/bridge code.
4. Keep existing profitability/admission limits such as `MAX_PROFITABLE_FRAME_LOCALS` unless an independent benchmark justifies changing them.
5. If a frame above that limit is declined, return an explicit admission reason after valid decoding. Do not report malformed bytecode or terminate scanning at `LdlocWide`.
6. For test-only traces that contain a wide instruction within an otherwise accepted shape, either:
   - lower the wide index end to end; or
   - return a typed unsupported/admission reason before native execution.

   Silent interpreter fallback from an unknown opcode is prohibited.
7. If wide traces are accepted, widen native local address calculations and deopt state maps; verify slot 299 is restored exactly.
8. Increment `NATIVE_CALLABLE_ABI_VERSION` once if helper signatures or persisted native metadata change.
9. Add tests for:
   - scanner skips 2-byte operand;
   - branch target after a wide instruction;
   - inline classifier reports an intentional reason;
   - recorder preserves index 299;
   - deopt/native exit restoration does not truncate;
   - short trace behavior remains unchanged.

**Focused commands:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/wide-local-target \
  cargo test --locked --test jit_tests wide_local --features cranelift-jit
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/wide-local-target \
  cargo test --locked native wide_local --features cranelift-jit
```

**Commit:**

```bash
git add src/vm/jit/ src/vm/native/ tests/jit/
git commit -m "feat(jit): decode and track wide local slots"
```

Stage exact files during implementation.

## Milestone 9: Widen AOT IR, lowering, restoration, and artifacts

**Objective:** Carry high local indices through AOT analysis and generated code without truncation.

**Files:**

- Modify: `src/vm/aot/cfg.rs`
- Modify: `src/vm/aot/ir.rs`
- Modify: `src/vm/aot/ssa.rs`
- Modify: `src/vm/aot/compile.rs`
- Modify: `src/vm/aot/runtime.rs`
- Modify: `src/vm/aot/artifact.rs`
- Modify: AOT tests and artifact fixtures

**Steps:**

1. Decode short/wide operands into `u16` in AOT IR lowering.
2. Change `AotInstruction::{Ldloc, LdlocOwned, Stloc}`, stack provenance, delayed-move analysis, local-null maps, SSA local IDs, errors, and runtime helper arguments to `u16`/`usize` as appropriate.
3. Update optimization patterns that compare adjacent `Ldloc`/`Stloc`; they must decode semantic indices rather than assume a fixed byte offset.
4. Verify CFG block boundaries and branch targets around three-byte wide instructions.
5. Widen AOT exit/deopt/restoration metadata and never narrow an enumerated local index.
6. Emit native address calculations from normalized indices and checked frame bases.
7. Increment AOT artifact version and ABI once; add `FLAG_WIDE_LOCALS` or equivalent declared compatibility capability.
8. Reject a wide artifact in a loader that lacks the flag/revision before mapping/executing code.
9. Add tests for:
   - AOT IR contains index 299;
   - delayed move from/to high slots;
   - SSA block parameters include high locals;
   - native AOT execution returns a high-slot value;
   - exit restoration preserves slot 299;
   - artifact round trip with wide flag;
   - short artifact compatibility/golden policy;
   - malformed artifact and unsupported flags.
10. Prefix AOT execution/lowering tests with `aot_wide_local` and artifact compatibility tests with `wide_local_artifact` so focused commands select the intended unit tests.

**Focused commands:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/wide-local-target \
  cargo test --locked aot_wide_local --features cranelift-jit
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/wide-local-target \
  cargo test --locked wide_local_artifact --features cranelift-jit
```

**Commit:**

```bash
git add src/vm/aot/
git commit -m "feat(aot): lower and restore wide local slots"
```

Stage exact files during implementation.

## Milestone 10: Update CLI, debugger, REPL, wasm, and remaining consumers

**Objective:** Remove every fixed-width local assumption outside execution backends.

**Files:**

- Modify: `src/cli.rs`
- Modify: `src/debug_info.rs`
- Modify: debug consumers in `src/vmbc.rs` and `tests/vm/runtime_state_edge_tests.rs`
- Modify: `pd-vm-wasm/src/analyzer.rs`
- Modify: any disassembler/formatter/validator found by the audit
- Modify: corresponding tests

**Steps:**

1. Replace REPL move tracking keyed by `u8` with normalized local indices.
2. Remove fixed instruction-offset assumptions for local loads/stores.
3. Update debugger local lookup/display and breakpoint stepping through wide instructions.
4. Update wasm analyzer validation, instruction counts, disassembly, and local display.
5. Audit repository-wide occurrences using searches for:

```text
index: u8
local_index: u8
source_local: Option<u8>
OpCode::Ldloc | OpCode::Stloc
read_u8(...local...)
as u8
```

6. Classify every remaining occurrence:
   - argument positions may legitimately remain `u8`;
   - host call argument counts may remain `u8`;
   - local-slot occurrences must widen or use checked short compatibility APIs.
7. Add tests for REPL/debugger/wasm inspection of slot 299 and for instruction stepping after a wide local access.
8. Update bytecode and VMBC documentation with opcode values, widths, endianness, and version rules.

**Focused commands:**

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/wide-local-target \
  cargo test --locked cli wide_local
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/wide-local-target \
  cargo test --locked --test compiler_tests debug_info
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/wide-local-target \
  cargo test --locked -p pd-vm-wasm wide_local
```

**Commit:**

```bash
git add src/cli.rs src/debug_info.rs src/vmbc.rs \
        tests/vm/runtime_state_edge_tests.rs pd-vm-wasm/src/ docs/
git commit -m "feat(tooling): inspect wide local bytecode"
```

Stage exact files during implementation.

## Milestone 11: Compatibility and performance audit

**Objective:** Prove short programs remain compact and wide support does not introduce unbounded compiler/runtime cost.

**Files:**

- Modify: benchmark or size-test files under `benches/` if present
- Modify: wire golden fixtures
- Modify: allocator stress tests
- Modify: documentation

**Steps:**

1. Compile a representative short-program corpus before/after and compare raw bytecode bytes and VMBC V12 artifacts.
2. Assert every local access under 256 still emits the short opcode.
3. Measure code-size delta for the 300-live-local fixture; only accesses to slots above 255 should grow by one byte.
4. Measure allocator memory/time at 300, 4,096, and a bounded 65,536-slot synthetic case. Record values without adding a brittle timing assertion.
5. Ensure no dense quadratic allocation is introduced for the maximum slot domain.
6. Verify error formatting uses actual counts and maximum 65,536.
7. Verify old V12 fixtures decode in std and no-std.
8. Verify V13 wide fixtures reject cleanly in a V12-only reference decoder if one is retained for tests.
9. Confirm native/AOT cache keys and artifact revisions separate old and new semantics.

**Commit:**

```bash
git add benches/ tests/fixtures/ docs/
git commit -m "test(vm): lock wide-local compatibility boundaries"
```

Stage only paths actually changed.

## Verification matrix

Use one isolated target directory:

```bash
export CARGO_TARGET_DIR=/mnt/TEMP/rustscript/wide-local-target
```

### Compiler and ISA boundaries

```bash
cargo fmt --all -- --check
cargo test --locked --test compiler_tests wide_local
cargo test --locked assembler wide_local
cargo test --locked bytecode wide_local
cargo test --locked --test compiler_tests debug_info
```

Required assertions:

- slot 255 emits short form;
- slot 256 emits wide form;
- slot 299 compiles, executes, and appears in debug metadata;
- slot 65,535 can be represented structurally;
- local count 65,537 fails with a typed upper-bound error;
- 300 genuinely simultaneous locals do not compact below the expected pressure range.

### Wire compatibility

```bash
cargo test --locked --test wire_tests wide_local
cargo test --locked --test wire_tests v12
cargo test --locked --test wire_tests v13
```

Required assertions:

- short-only artifacts remain V12 and byte-identical;
- wide bytecode or debug metadata selects V13;
- new decoder accepts V12 and V13;
- V12 rejects wide opcodes;
- truncated wide operands and inconsistent local counts fail deterministically.

### Runtime ownership and frame behavior

```bash
cargo test --locked --test vm_tests wide_local
cargo test --locked --test vm_tests drop_contract
cargo test --locked --test compiler_tests closure
cargo test --locked --test compiler_tests recursion
```

Required assertions:

- move/copy/store/drop behavior matches short instructions;
- capture cells and nested frame bases handle high slots;
- yield/resume and callable return preserve high locals;
- no absolute index can escape the active frame.

### Backend parity

```bash
cargo test --locked --test jit_tests wide_local --features cranelift-jit
cargo test --locked aot_wide_local --features cranelift-jit
cargo test --locked wide_local_artifact --features cranelift-jit
cargo test --locked -p pd-vm-nostd wide_local
cargo test --locked -p pd-vm-wasm wide_local
```

Required assertions:

- all scanners preserve instruction boundaries;
- JIT either handles high indices or returns an explicit admission reason;
- AOT IR/native restoration preserves high indices;
- std/no-std decode and execute the same V13 fixture;
- wasm/debug tooling prints exact indices.

### Full gates

```bash
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
git diff --check
```

Every filtered command must select at least one test. A command reporting zero selected tests is not evidence.

## Manual verification artifact

Generate a deterministic RSS program under `/mnt/TEMP/rustscript/wide-local-verification/` with approximately 300 simultaneously live values and a final expected scalar. Verify:

```bash
CARGO_TARGET_DIR=/mnt/TEMP/rustscript/wide-local-target \
  cargo run --locked --bin pd-vm-run -- \
  /mnt/TEMP/rustscript/wide-local-verification/main.rss
```

Expected: successful execution with the known scalar result.

Then compile/encode the same program to VMBC through the repository's supported CLI/API, decode it with std and no-std tests, disassemble it, and confirm at least one `LdlocWide` or `StlocWide` references slot 256 or above. Remove the generated source and temporary target directory after final verification.

## Stop conditions

Stop and report the exact blocker before continuing if any of these occurs:

- Frame-aware allocation has not landed, or the 300-local fixture's pressure comes from cross-frame over-allocation.
- Supporting wide locals requires changing existing short opcode widths or values.
- Any decoder can desynchronize after a truncated wide operand.
- Debug metadata still narrows before bytecode emission.
- A backend silently truncates or silently falls back after encountering a wide opcode.
- VMBC V12 output changes for short-only programs without an explicit approved reason.
- The allocator requires a dense 65,536-square interference matrix.
- AOT/native restoration cannot represent high indices without an undeclared artifact or ABI change.
- Intermediate milestones cannot pass their focused default-feature gates.

## Target criteria

- The compiler supports genuinely simultaneous frame-local pressure through 65,536 slots.
- Slots 0 through 255 retain existing compact bytecode.
- Slots 256 through 65,535 use additive little-endian wide instructions.
- All semantic consumers normalize local indices to `u16` without truncation.
- Debug metadata and tooling expose exact high-slot indices.
- VMBC V12 remains byte-compatible for short-only programs; V13 carries wide code/debug metadata.
- Interpreter and no-std execute a shared wide fixture identically.
- JIT scanners handle wide instruction boundaries and expose explicit policy when declining large frames.
- AOT lowering, execution, restoration, and artifacts preserve high indices.
- Ownership, capture, recursion, suspension, drop, and frame isolation match short-local behavior.
- No compiler or runtime component treats 256 as the semantic maximum after this plan lands.
