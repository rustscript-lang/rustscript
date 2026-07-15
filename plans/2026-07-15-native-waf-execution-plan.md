# Native WAF Execution Plan — pd-vm

> **For Hermes:** Start this repository only after the pd-edge-waf Stage 1 spike proves the plan schema and semantics.

**Goal:** Add a validated native rule-plan execution facility to pd-vm without coupling the VM core to OWASP CRS source syntax.

**Architecture:** pd-vm owns generic plan validation, immutable operator assets, fixed-layout transaction storage, and native execution. pd-edge-waf owns CRS translation. The RSS VM remains available for orchestration and fallback.

**Tech Stack:** Rust, serde, regex, pd-vm runtime feature, existing VM interruption and memory-accounting infrastructure.

---

## Stage 1: Promote the proven plan model

### Task 1: Define generic plan types

**Files:**
- Create: `src/rule_plan/mod.rs`
- Create: `src/rule_plan/schema.rs`
- Modify: `src/lib.rs`
- Test: `tests/rule_plan_tests.rs`

**Requirements:**
- Versioned schema and explicit capability set.
- Phase indexes, rule order, targets, transforms, operators, actions, chains, and markers.
- Load-time rejection for unsupported operators, invalid indexes, malformed regex, and invalid chain topology.

### Task 2: Add immutable compiled assets

**Files:**
- Create: `src/rule_plan/assets.rs`
- Test: `tests/rule_plan_tests.rs`

**Requirements:**
- Compile regex and phrase assets once during plan load.
- Store assets behind immutable `Arc` values.
- Use operator IDs during request execution.
- Keep dynamic regex support in the existing VM-local cache.

### Task 3: Add fixed-layout transaction storage

**Files:**
- Create: `src/rule_plan/transaction.rs`
- Test: `tests/rule_plan_tests.rs`

**Requirements:**
- Typed score, block status, HTTP status, phase, paranoia, chain state, matched IDs, and indexed TX values.
- Indexed request collections with exact selector/exclusion semantics.
- Explicit conversion at the RSS/host boundary only.

### Task 4: Implement the executor

**Files:**
- Create: `src/rule_plan/executor.rs`
- Test: `tests/rule_plan_tests.rs`

**Requirements:**
- Preserve plan order and all control-flow semantics.
- Honor VM interruption budgets for long plans.
- Return typed errors carrying plan entry and source rule identity.
- Avoid per-rule string-key lookups and descriptor decoding.

## Stage 2: RSS integration

1. Expose plan loading and execution through a builtin or opaque handle API.
2. Keep builtin ABI versioned and deterministic.
3. Let RSS orchestrate request/response flow while the plan executor owns rule evaluation.
4. Add VMBC metadata only after the standalone artifact path passes compatibility review.

## Stage 3: General VM JIT work

1. Add a profitability model for call-boundary traces.
2. Reject traces whose materialization cost exceeds their native work.
3. Add continuation or trace stitching for supported builtin calls.
4. Keep WAF plan execution independent from trace-JIT success.

## Verification

```bash
cargo fmt --check
cargo test --release --lib
cargo test --release --test rule_plan_tests
cargo test --release --test jit_tests
cargo clippy --all-targets --all-features -- -D warnings
```

## Acceptance criteria

- No OWASP-specific source parser enters pd-vm.
- Plan load compiles static regex assets once.
- Request execution uses fixed-layout state.
- VM interruption and memory limits remain enforceable.
- Existing VMBC and host ABI tests remain green.
