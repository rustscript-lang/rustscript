# Semantic Module System Implementation Plan

**Goal:** Replace textual import rewriting and synthetic declarations with a semantic module graph and symbol resolution model.

**Architecture:** Parse imports as syntax, assign every source a `ModuleId` and `SourceId`, resolve declarations to `SymbolId`, and link by resolved identity. Module namespaces, visibility, private helpers, and diagnostics become first-class compiler data instead of rewritten text and parallel metadata arrays.

**Tech Stack:** Rust 2024, RustScript parser, frontend IR, compiler pipeline, linker, `SourceMap`.

---

## Independence and dependency

- Depends on completion of `2026-08-09_nested-module-correctness.md` so the current branch has a verified baseline.
- Independent of VM execution, host capabilities, HTTP, agent gateway, and backend optimization.
- May proceed in compiler-only milestones with bytecode output held behaviorally constant.

## Scope boundary

### In scope

- `ModuleId`, `SourceId`, `SymbolId`, import edges, export tables, and visibility.
- Semantic namespace and named-import resolution.
- Private function identity across modules.
- Source-owned diagnostics after graph merge.
- Removal of synthetic function preludes and call-site text rewriting for file modules.

### Out of scope

- Package manager, remote modules, registry resolution, or dependency downloads.
- Dynamic module loading at VM runtime.
- New bytecode opcodes solely for module names.
- Host namespace redesign.
- Agent-specific storage or provider modules.

## Implementation route

### Milestone 1: Define compiler-owned identities

**Files:**
- Create: `src/compiler/modules.rs`
- Modify: `src/compiler/source_loader.rs`
- Modify: `src/compiler/pipeline.rs`
- Test: compiler module tests

Define:

```text
ModuleId
SourceId
SymbolId
ModuleGraph
ModuleNode { source, imports, declarations, exports }
ResolvedImport
```

IDs are deterministic within one compilation and never derived only from a file stem.

### Milestone 2: Parse import syntax into AST/IR

**Files:**
- Modify RustScript parser/frontend import nodes
- Modify source-loader import discovery
- Test parser and module fixtures

1. Stop using line-prefix stripping as the authoritative import parser.
2. Preserve import spans and clauses in the parsed unit.
3. Resolve `self::`, `super::`, namespace aliases, and named imports from structured nodes.
4. Keep host namespace imports on their existing dedicated resolution path.

### Milestone 3: Build declarations and export tables

**Files:**
- Modify frontend IR declaration metadata
- Modify `src/compiler/source_loader/graph.rs`
- Modify linker symbol collection

1. Assign each declaration a symbol owned by its module.
2. Mark public exports explicitly.
3. Keep imported symbols separate from local declarations.
4. Prevent implicit transitive re-export.
5. Permit different modules to have private or public functions with the same source name.

### Milestone 4: Resolve calls by symbol identity

**Files:**
- Modify expression/call IR
- Modify `src/compiler/linker.rs`
- Modify lowering consumers

1. Resolve local, named-import, and namespace calls to `SymbolId` before merge.
2. Replace string-based global function matching with symbol lookup.
3. Use deterministic internal mangling only at the final flat bytecode boundary if required.
4. Remove basename-only scope prefixes.

### Milestone 5: Preserve source ownership through merge

**Files:**
- Modify `src/compiler/pipeline.rs`
- Modify diagnostic/source-map structures
- Test rendered diagnostics

1. Every span retains its source identity.
2. Merging units cannot reinterpret one module's offset in another source.
3. Parse, typing, duplicate symbol, visibility, and unresolved import errors render from the owning source.
4. Remove parallel `stmt_sources`, `function_sources`, and ad hoc prelude line remapping where replaced by source-owned IR.

### Milestone 6: Remove textual compatibility machinery

**Files:**
- Remove obsolete paths in `src/compiler/source_loader/rewrite.rs`
- Remove synthetic prelude generation and related line maps
- Update tests and compiler docs

Do not retain a second module pipeline after semantic resolution reaches parity.

### Milestone 7: Verification

Required cases:

- two directories containing modules with the same stem;
- two namespaces exporting the same function name;
- same-named private helpers in multiple modules;
- visibility errors and no transitive re-export;
- cycles through path aliases;
- in-memory overrides mixed with disk modules;
- deterministic output independent of import discovery order;
- source-correct diagnostics for every module.

```bash
cargo fmt --all -- --check
cargo test --locked --test compiler_tests
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
git diff --check
```

## Target criteria

- File-module calls are resolved by `SymbolId`, not rewritten source text.
- Module identity never depends only on a basename.
- Same-named declarations in independent modules coexist.
- Public/private and re-export rules are represented in compiler data.
- Synthetic imported-function preludes are removed from the module path.
- Every diagnostic span retains its owning source after linking.
- No agent-specific compiler rule is introduced.
