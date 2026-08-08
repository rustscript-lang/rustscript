# Nested Module Correctness Implementation Plan

**Goal:** Correct the current nested RSS module loader without expanding it into a new module architecture.

**Architecture:** Keep the existing source-loader, import rewrite, and public `SourcePathError` shape, while repairing UTF-8 preservation, lexical path normalization, source ownership in diagnostics, and parity across public compile entry points. This is the short corrective plan; semantic module replacement is covered separately.

**Tech Stack:** Rust 2024, RustScript compiler/source loader, `SourceMap`, CLI diagnostics, Cargo integration tests.

---

## Independence and dependency

- Independent of VM runtime, builtin IDs, agent gateway, HTTP, and persistence.
- Must complete before the semantic module-system plan begins.
- The agent repository only supplies a composition fixture; compiler behavior remains owned by `rustscript`.

## Scope boundary

### In scope

- UTF-8-safe call-site rewrite.
- Correct preservation of unmatched relative `..` components.
- Canonical identity for equivalent disk module paths.
- Nested parse and strict-type diagnostics rendered from the real module source.
- Equivalent behavior for file, source-with-options, and source-at-path public entry points.
- Regression coverage for import/export/cycle behavior already changed in the worktree.

### Out of scope

- New public error enum variants.
- A compatibility wrapper for the removed `SourceAt` experiment.
- Semantic symbol resolution or a new IR.
- Agent storage migration or gateway changes.
- VM-visible opcodes or host capabilities.

## Implementation route

### Milestone 1: Add failing UTF-8 rewrite tests

**Files:**
- Modify: `tests/compiler/module_import_tests.rs`
- Modify: unit tests in `src/compiler/source_loader/rewrite.rs`

Add nested modules that trigger namespace and named-import rewriting while containing:

- non-ASCII string literals;
- line and block comments;
- non-ASCII source outside rewritten spans where syntax permits it.

Assert byte-for-byte preservation of untouched source and runtime preservation of values such as `"猫"`.

### Milestone 2: Make scanners copy source slices

**Files:**
- Modify: `src/compiler/source_loader/rewrite.rs`

1. Stop appending UTF-8 bytes with `byte as char`.
2. Advance by valid UTF-8 scalar boundaries or copy untouched ranges as source slices.
3. Keep token recognition ASCII-specific where the grammar requires ASCII identifiers and separators.
4. Preserve comments, strings, escapes, and line counts exactly.

### Milestone 3: Correct path normalization and identity

**Files:**
- Modify: `src/compiler/source_loader/imports.rs`
- Modify: `src/compiler/source_loader/graph.rs`
- Test: `tests/compiler/module_import_tests.rs`

1. Pop `ParentDir` only when the previous normalized component is `Normal`.
2. Never cancel an unmatched `ParentDir` with a later `ParentDir`.
3. Preserve root semantics for absolute paths.
4. Use canonical disk identity for files that exist; use a normalized explicit virtual identity for source overrides.
5. Key `seen`, `visiting`, exports, and overrides with the same module identity.
6. Test consecutive `super::`, absolute above-root input rejection/normalization policy, path aliases, cycle aliases, and duplicate import aliases.

### Milestone 4: Carry nested source context to diagnostics

**Files:**
- Modify: `src/compiler/source_loader.rs`
- Modify: `src/compiler/source_loader/graph.rs`
- Modify: `src/compiler/pipeline.rs`
- Modify: `src/cli.rs`
- Test: `tests/compiler/module_import_tests.rs`
- Add or modify CLI diagnostic integration tests

1. Keep `SourcePathError` public enum shape unchanged.
2. Carry internal `{ path, source text, SourceId/span }` context through compilation.
3. Render nested parse and strict-type errors against the nested source, not the root source map.
4. Apply the same path/source enrichment to:
   - `compile_source_file`;
   - `compile_source_with_flavor_and_options`;
   - `compile_source_at_path_with_flavor_and_options`.
5. Test path, line, code frame, underline, and source override content, not message text alone.

### Milestone 5: Preserve import/export behavior

Add regression cases for:

- nested namespace aliases;
- nested named imports;
- public-only exports;
- no transitive re-export;
- same-directory `self::` and parent-directory `super::`;
- missing modules and normalized cycles;
- root and nested host namespace imports.

### Milestone 6: Verification

```bash
cargo fmt --all -- --check
cargo test --locked --test compiler_tests module_import
cargo test --locked --test compiler_tests
cargo test --locked --workspace --all-features
git diff --check
```

Run the CLI diagnostic fixture and assert that the rendered path and highlighted line both belong to the nested source.

## Target criteria

- Rewriting never changes untouched UTF-8 bytes.
- Consecutive unmatched parent components retain their lexical meaning.
- Equivalent disk paths resolve to one module identity.
- Nested diagnostics display the actual nested source line and underline.
- All public module-capable compile entry points identify the failing module.
- Existing public error enum shape remains unchanged.
- Import, export, and cycle tests pass without agent-specific compiler behavior.
