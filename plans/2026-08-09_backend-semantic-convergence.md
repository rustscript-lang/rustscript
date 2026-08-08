# Backend Semantic Convergence Plan

**Goal:** Reduce semantic duplication across interpreter, Trace JIT, AOT, native bridge, and no-std execution by introducing one canonical instruction/operation contract and differential verification.

**Architecture:** Bytecode semantics, builtin signatures, ownership rules, traps, and deoptimization outcomes are defined once. Each backend lowers or interprets the same contract. Generated coverage tables and differential fixtures detect missing or divergent implementations.

**Tech Stack:** Rust 2024, interpreter, Trace JIT, Cranelift AOT, native bridge, `pd-vm-nostd`, property/differential tests.

---

## Independence and dependency

- Independent of agent framework and module loading.
- Static builtin IDs should land first.
- VM decomposition should define Engine/Program ownership before large backend file moves.
- This plan does not block immediate correctness plans.

## Scope boundary

### In scope

- One canonical semantic description for opcodes and builtins.
- Generated backend coverage checks.
- Shared ownership/trap/helper contracts.
- Differential interpreter/JIT/AOT/no-std tests.
- Incremental removal of duplicated lowering logic.

### Out of scope

- New optimization targets or benchmark promises.
- New bytecode opcodes solely to simplify one backend.
- A complete JIT rewrite in one milestone.
- Agent, HTTP, SQLite, or gateway behavior.

## Implementation route

### Milestone 1: Build a backend coverage inventory

**Files:**
- Create backend coverage tests/tools under `tests/` or `src/backend/`
- Read interpreter, JIT recorder/lowerer, AOT IR/lowerer, no-std dispatch

Generate a matrix for every opcode/builtin:

```text
semantic definition
interpreter
trace recorder
JIT lowering
AOT lowering
no-std
fallback/deopt rule
```

Fail CI when a newly added operation lacks an explicit backend disposition.

### Milestone 2: Define canonical operation semantics

**Files:**
- Create: `src/semantics/` or equivalent
- Modify opcode/builtin metadata generation

Represent:

- operand/result types and stack effect;
- ownership/borrow/clone/drop behavior;
- trap/error conditions;
- side-effect and suspension classification;
- interpreter helper and native helper ABI;
- deopt/fallback permission.

Keep explicit Rust implementation hooks where declarative metadata is insufficient.

### Milestone 3: Generate shared dispatch metadata

1. Generate interpreter validation/stack-effect tables.
2. Generate JIT/AOT eligibility and helper IDs.
3. Generate no-std support/fallback declarations.
4. Key builtins by static ID.
5. Reject mismatched arity/type/ownership metadata at build time.

### Milestone 4: Consolidate native helper contracts

**Files:**
- Modify native bridge/helper modules
- Modify JIT/AOT lowerers

1. Define one helper ABI for tagged/scalar/heap operands.
2. Centralize owned temporary and Arc/raw-pointer rules.
3. Centralize trap/status routing.
4. Remove backend-specific reinterpretation of the same helper payload.

### Milestone 5: Add differential execution harness

For generated and curated programs, compare:

- return value and structured error;
- side-effect/event sequence;
- ownership/drop counters where observable;
- fuel/deadline behavior;
- interpreter, JIT, AOT, and no-std supported subsets.

Include arrays/maps/bytes, calls/closures, branches/loops, host-call boundaries, traps, and deopt cases.

### Milestone 6: Migrate one semantic family at a time

Recommended order:

1. scalar arithmetic/comparison;
2. stack/local/frame operations;
3. collection access/mutation;
4. calls/closures;
5. builtin/native helper calls;
6. suspension/deopt/terminal outcomes.

Each family removes superseded duplicate tables after differential parity passes.

### Milestone 7: Verification

```bash
cargo fmt --all -- --check
cargo test --locked --workspace --all-features
cargo test --locked -p pd-vm-nostd
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
git diff --check
```

## Target criteria

- Every opcode and builtin has one canonical semantic entry.
- Every backend declares implement/fallback/unsupported explicitly.
- New operations cannot compile without a complete backend disposition.
- Interpreter/JIT/AOT/no-std differential fixtures agree on the supported subset.
- Native ownership and trap ABI is shared by JIT and AOT.
- Backend-specific large files lose duplicated semantic policy over incremental milestones.
- Performance changes are measured separately from semantic convergence.
