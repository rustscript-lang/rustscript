# Static Builtin ID Implementation Plan

**Goal:** Replace count-derived builtin call indices with explicit static IDs that remain fixed after assignment.

**Architecture:** Every VM-visible builtin receives an explicit `u16` ID in one authoritative catalog. `build.rs`, the interpreter, compiler, wire encoder/decoder, and `pd-vm-nostd` consume generated tables from that catalog. This migration may break existing VMBC once; the implementation will bump the bytecode ABI and reject the previous format instead of carrying an old-ID decoder.

**Tech Stack:** Rust 2024, `build.rs` code generation, VMBC wire format, `pd-vm`, `pd-vm-nostd`.

---

## Independence and dependency

- Independent of agent framework, module loading, HTTP behavior, and JIT refactoring.
- Must land before more builtins are added.
- Later capability plans may key permissions by the static builtin ID.

## Scope boundary

### In scope

- Explicit IDs for ordinary, internal, and special-call builtins.
- One authoritative catalog and generated forward/reverse lookup.
- A one-time VMBC ABI version bump.
- Compile-time duplicate/range validation.
- Shared std/no-std ID generation.

### Out of scope

- Compatibility decoding for prior VMBC versions.
- Aliases from old IDs to new IDs.
- New builtin behavior or host capabilities.
- Changes to source-language names.

## Implementation route

### Milestone 1: Freeze the ID contract with failing tests

**Files:**
- Modify: `tests/wire/wire_tests.rs`
- Modify: `src/bytecode.rs`
- Add fixture/catalog tests under `tests/wire/`

1. Add assertions for explicit IDs of representative ordinary, internal, and special builtins.
2. Add a uniqueness test over the complete catalog.
3. Add range tests proving static IDs do not overlap opcodes or reserved sentinels.
4. Add a test that appending a synthetic catalog entry does not change existing IDs.

**RED command:**

```bash
cargo test --locked --test wire_tests builtin
```

### Milestone 2: Introduce the authoritative catalog

**Files:**
- Modify: `build.rs`
- Modify: `src/builtins/mod.rs` or create `src/builtins/catalog.rs`
- Modify generated builtin metadata consumers

1. Define each entry as `{ id, source_name, Rust variant, class, feature gate }`.
2. Remove `BUILTIN_CALL_BASE` arithmetic from ID assignment.
3. Generate `BuiltinFunction::call_index`, reverse lookup, dispatch tables, and catalog iteration from explicit IDs.
4. Fail the build on duplicate IDs, duplicate names, out-of-range IDs, or a missing explicit ID.
5. Reserve documented ID blocks for ordinary, internal, and future extension entries without deriving IDs from catalog length.

### Milestone 3: Share IDs with no-std

**Files:**
- Modify: `pd-vm-nostd/src/vm.rs`
- Modify: `pd-vm-nostd/build.rs` or generate a shared checked-in artifact
- Modify: no-std wire tests

1. Remove the duplicated `BUILTIN_BASE` constant.
2. Generate or import the same explicit ID table without requiring std-only dependencies.
3. Verify std compiler output executes under `pd-vm-nostd` with identical builtin dispatch.

### Milestone 4: Declare the format break

**Files:**
- Modify: `src/bytecode.rs`
- Modify: VMBC format tests and documentation

1. Increment `BYTECODE_ABI_VERSION` once.
2. Reject the previous version with a deterministic unsupported-version error.
3. Do not add migration, dual decoding, or legacy aliases.
4. Regenerate only current-version fixtures.

### Milestone 5: Full verification

```bash
cargo fmt --all -- --check
cargo test --locked --test wire_tests
cargo test --locked -p pd-vm-nostd
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
git diff --check
```

## Target criteria

- Every VM-visible builtin has one explicit static ID.
- Adding or reordering catalog entries leaves all prior explicit IDs unchanged.
- Duplicate or missing IDs fail during generation.
- std and no-std use the same IDs without manually mirrored base arithmetic.
- VMBC declares the one-time incompatible format change and rejects the old version.
- No compatibility decoder or old-ID alias remains.
