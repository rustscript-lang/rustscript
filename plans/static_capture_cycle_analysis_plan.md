# Static Capture Ownership-Cycle Analysis Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `codex-superpowers-subagent-driven-development` or `codex-superpowers-executing-plans` to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** Reject every callable capture ownership cycle during compilation or bytecode validation, first with early source diagnostics and then with a sound capture-region/provenance analysis, so desktop and no-std runtimes can remove recursive cycle checks from captured-local stores.

**Architecture:** Add a flow-sensitive ownership-provenance pass after lifetime rewriting and type inference. The pass models capture cells, callable environments, arrays, maps, branches, loops, calls, and host boundaries as a finite may-reference graph. A write into capture cell `C` is accepted only when the graph proves that the written value cannot reach `C`; unknown provenance is a compile-time error. The same invariant is encoded in Program metadata and revalidated for VMBC/AOT/untrusted assembled Programs before runtime cycle checks are removed.

**Tech Stack:** Rust, RustScript frontend IR, availability/lifetime analysis, interprocedural fixed-point dataflow, VMBC validator, desktop `Arc<Mutex<Value>>`, no-std `Rc<RefCell<Value>>`, Trace JIT, AOT.

---

## Semantic boundary

Static rejection cannot accept every dynamically cycle-free program in a higher-order language while also proving every execution cycle-free. Unbounded closure allocation, dynamic container indexing, recursion, host-returned `Value`, and runtime aliases prevent a complete no-false-positive decision procedure.

For this plan, **static precise determination** means:

1. exact reachability over the compiler's finite symbolic capture-region/provenance graph;
2. sound joins at branches, loops, recursive SCCs, containers, and higher-order calls;
3. rejection of `Unknown` provenance at a captured-cell write;
4. validation of the same proof contract for every executable Program, including VMBC and manually assembled bytecode.

This boundary guarantees that removal of the runtime graph walk does not permit an RC cycle. It intentionally rejects programs whose safety cannot be proven.

## File map

**Create:**

- `src/compiler/lifetime/capture_cycles.rs` — stage-1 source diagnostics and stage-2 ownership analysis.
- `src/compiler/lifetime/capture_cycles/domain.rs` — capture regions, abstract values, heap nodes, reachability, and joins.
- `src/compiler/lifetime/capture_cycles/interprocedural.rs` — function/callable summaries and SCC fixed point.
- `src/compiler/lifetime/capture_cycles/tests.rs` — unit tests for graph transfer and join rules.

**Modify:**

- `src/compiler/lifetime.rs` — expose the capture-cycle pass.
- `src/compiler/pipeline.rs` — run the pass after availability rewriting and type inference, before codegen.
- `src/compiler/mod.rs` — add focused compile diagnostics.
- `src/compiler/ir.rs` — carry source/callable identity required by diagnostics and summaries.
- `src/compiler/typing.rs` and `src/compiler/typing/*` — expose callable/container schemas to provenance classification.
- `src/bytecode.rs` — store capture-write proof metadata and bump internal ABI revisions.
- `src/vmbc.rs` — serialize and validate proof metadata.
- `src/assembler.rs` — require explicit proof metadata for assembled capture-writing Programs.
- `src/vm/mod.rs` — remove the desktop runtime recursive graph walk only after proof validation is mandatory.
- `pd-vm-nostd/src/bytecode.rs` — decode proof metadata.
- `pd-vm-nostd/src/vmbc.rs` — enforce the revised VMBC contract.
- `pd-vm-nostd/src/vm.rs` — remove the no-std runtime graph walk only after validation is mandatory.
- `src/vm/jit/*`, `src/vm/aot/*`, and debugger recording files — bump revisions and preserve the verified invariant.
- `tests/compiler/compiler_rustscript_tests.rs` — source-level positive and negative cases.
- `tests/wire/assembler_vmbc_edge_tests.rs` and `tests/wire/wire_tests.rs` — untrusted Program and VMBC proof validation.
- `README.md` — document the ownership-cycle rule and conservative unknown-provenance rejection.

---

## Stage 1: Static early diagnostics for obvious cycles

### Task 1: Characterize current runtime-only behavior

**Files:**

- Modify: `tests/compiler/compiler_rustscript_tests.rs`
- Modify: `src/vm/tests.rs`
- Modify: `pd-vm-nostd/tests/embedded_host.rs`

- [ ] Add source cases for direct self-capture assignment, indirect assignment through an alias, container-wrapped callable assignment, branch-dependent cycles, safe scalar captured mutation, and safe assignment of a callable that does not capture the target cell.

Use cases shaped like:

```rust
let mut callback = null;
let closure = || callback;
callback = closure;
```

and:

```rust
let mut count = 0;
let increment = || {
    count = count + 1;
    count
};
```

- [ ] Run the focused compiler/runtime tests and record that direct cycles currently compile and fail only in `store_local_with_drop_contract` or no-std `Stloc`.

```bash
cargo test -p pd-vm --all-features --test compiler_tests capture_cycle -- --nocapture
cargo test -p pd-vm --all-features --lib shared_capture_cell_rejects_callable_ownership_cycle -- --nocapture
cargo test -p pd-vm-nostd capture_cycle -- --nocapture
```

- [ ] Keep these tests as the semantic migration matrix; change expected phases only in the tasks that introduce each diagnostic.

### Task 2: Add focused compile diagnostics

**Files:**

- Modify: `src/compiler/mod.rs`
- Create: `src/compiler/lifetime/capture_cycles.rs`
- Modify: `src/compiler/lifetime.rs`
- Modify: `src/compiler/pipeline.rs`

- [ ] Add two compile errors with source line and local name:

```rust
CompileError::CaptureOwnershipCycle {
    line: Option<u32>,
    source_name: Option<String>,
    captured_local: String,
    path: Vec<String>,
}

CompileError::UnprovenCaptureWrite {
    line: Option<u32>,
    source_name: Option<String>,
    captured_local: String,
    reason: String,
}
```

- [ ] Add `capture_cycles::reject_obvious_cycles(&FrontendIr) -> Result<(), CompileError>` after lifetime capture modes are known.

- [ ] In stage 1, detect only syntactically certain back-edges:

  - assigning a closure/function value directly to a cell captured by that callable;
  - assigning a local whose exact callable binding captures the target cell;
  - assigning an array/map literal that directly contains such a callable;
  - direct aliases whose callable identity is still exact in `callable_bindings`.

- [ ] Build diagnostic paths such as:

```text
assignment to 'callback' would create a callable capture ownership cycle:
callback -> closure environment -> callback
```

- [ ] Do not reject scalar mutation or callable values proven to capture different cells.

- [ ] Run focused tests. Direct cases must now fail during compilation; dynamic and indirect cases must continue reaching the runtime guard until stage 2.

```bash
cargo test -p pd-vm --all-features --test compiler_tests obvious_capture_cycle -- --nocapture
```

- [ ] Commit stage 1 independently.

```bash
git add src/compiler tests/compiler/compiler_rustscript_tests.rs
git commit -m "Reject obvious callable capture cycles statically"
```

---

## Stage 2: Static precise determination over ownership provenance

### Task 3: Define the ownership-provenance domain

**Files:**

- Create: `src/compiler/lifetime/capture_cycles/domain.rs`
- Create: `src/compiler/lifetime/capture_cycles/tests.rs`
- Modify: `src/compiler/lifetime/capture_cycles.rs`

- [ ] Define symbolic region and heap identities:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct CaptureRegionId {
    owner: CallableOwnerId,
    source_slot: LocalSlot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct HeapNodeId(u32);

#[derive(Clone, Debug, PartialEq, Eq)]
enum OwnershipProvenance {
    AcyclicScalar,
    References(BTreeSet<CaptureRegionId>),
    Heap(HeapNodeId),
    Unknown(UnknownProvenanceReason),
}
```

- [ ] Model callable environments and aggregate values:

```rust
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct OwnershipGraph {
    cell_contents: BTreeMap<CaptureRegionId, OwnershipProvenance>,
    heap_edges: BTreeMap<HeapNodeId, BTreeSet<OwnershipTarget>>,
}

enum OwnershipTarget {
    Capture(CaptureRegionId),
    Heap(HeapNodeId),
}
```

- [ ] Implement deterministic `join`, `references`, `reachable`, and `write_capture` operations.

`write_capture(target, value)` must:

1. reject `Unknown`;
2. reject when `value` reaches `target` through callable, cell, array, or map edges;
3. update the cell edge only after checks pass.

- [ ] Unit-test direct, transitive, aggregate, mutually recursive, branch-joined, and non-cyclic graphs.

```bash
cargo test -p pd-vm --all-features --lib compiler::lifetime::capture_cycles -- --nocapture
```

### Task 4: Add flow-sensitive intraprocedural transfer

**Files:**

- Modify: `src/compiler/lifetime/capture_cycles.rs`
- Modify: `src/compiler/ir.rs`
- Modify: `src/compiler/typing.rs`
- Modify: `src/compiler/typing/*`
- Modify: `tests/compiler/compiler_rustscript_tests.rs`

- [ ] Track one `OwnershipProvenance` per local alongside the capture-cell graph.

- [ ] Implement expression transfer rules:

  - primitive/string/bytes values: `AcyclicScalar`;
  - callable creation: references exactly its borrowed/mutably borrowed capture regions; moved/copy captures contribute the provenance of captured values;
  - local loads: copy the local provenance;
  - array/map literals: allocate a symbolic heap node and union member/key/value edges;
  - array/map concatenation and mutation: update or conservatively join heap edges;
  - dynamic indexing: return the aggregate member provenance;
  - `if`/`match`: join local and graph states from all reachable branches;
  - loops: iterate to a monotone fixed point;
  - values whose schema excludes callable/array/map: `AcyclicScalar` even when their origin is a host call;
  - dynamic `Value`, unknown callable result, or erased aggregate element: `Unknown`.

- [ ] At every assignment to a `Borrow`/`BorrowMut` capture region, call `write_capture` and emit `CaptureOwnershipCycle` or `UnprovenCaptureWrite`.

- [ ] Preserve safe stateful closures and assignments between independently proven regions.

- [ ] Add tests for container nesting, branch merges, loops, separate factory evaluations, aliases sharing one environment, and dynamic index reads.

### Task 5: Add interprocedural and higher-order summaries

**Files:**

- Create: `src/compiler/lifetime/capture_cycles/interprocedural.rs`
- Modify: `src/compiler/lifetime/capture_cycles.rs`
- Modify: `src/compiler/ir.rs`
- Modify: `tests/compiler/compiler_rustscript_tests.rs`

- [ ] Define parameterized summaries:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
struct CallableOwnershipSummary {
    return_provenance: OwnershipExpr,
    captured_writes: Vec<CapturedWriteEffect>,
    parameter_escape: Vec<ParameterEscapeEffect>,
}
```

- [ ] Express summary provenance in terms of parameter indices and captured regions so generic and higher-order functions do not collapse immediately to `Unknown`.

- [ ] Build the callable call graph, compute strongly connected components, and iterate summaries inside each SCC until no summary changes.

- [ ] At a call site, substitute argument provenance into the callee summary and apply captured-write effects to the caller graph.

- [ ] For dynamic callable dispatch, join every target retained by callable type/control-flow analysis. If the target set is open or erased, return `Unknown` and reject only when it reaches a captured-cell write.

- [ ] Treat host functions according to declared schemas and effects:

  - primitive-only return schemas are `AcyclicScalar`;
  - callable/array/map/unknown returns are `Unknown` unless the host declaration carries a verified `capture_free_return` effect;
  - a host function accepting mutable VM access cannot provide that effect automatically.

- [ ] Add compile tests for higher-order returns, generic identity/apply, recursive callable SCCs, host-returned values, callback parameters, and REPL entry locals.

### Task 6: Make unknown-provenance rejection explicit language behavior

**Files:**

- Modify: `README.md`
- Modify: `src/compiler/mod.rs`
- Modify: `tests/compiler/compiler_rustscript_tests.rs`

- [ ] Document that captured-cell writes require statically proven non-reachability.

- [ ] Add diagnostics that identify the first provenance loss, for example:

```text
cannot prove assignment to captured local 'callback' is ownership-cycle free:
value comes from dynamic callable return with an open target set
```

- [ ] Add source-level escape hatches only for declarations that can be verified mechanically. Do not add an unchecked annotation that bypasses the invariant.

- [ ] Run all compiler suites in default, strict, dynamic, REPL, source-file, and linked-module modes.

- [ ] Commit the precise analysis before changing runtime behavior.

```bash
git add src/compiler README.md tests/compiler
git commit -m "Prove capture writes cycle free at compile time"
```

---

## Stage 3: Make the proof mandatory and remove runtime checks

### Task 7: Encode and validate the Program proof contract

**Files:**

- Modify: `src/bytecode.rs`
- Modify: `src/assembler.rs`
- Modify: `src/vmbc.rs`
- Modify: `pd-vm-nostd/src/bytecode.rs`
- Modify: `pd-vm-nostd/src/vmbc.rs`
- Modify: `tests/wire/assembler_vmbc_edge_tests.rs`
- Modify: `tests/wire/wire_tests.rs`

- [ ] Add Program metadata that identifies capture-backed local slots and records each compiler-verified captured write site:

```rust
pub struct CaptureWriteProof {
    pub instruction_ip: u32,
    pub target_slot: u16,
    pub provenance_hash: u64,
}
```

- [ ] Compute `provenance_hash` from the normalized ownership summary, callable layouts, capture layouts, local slot layout, and instruction operands.

- [ ] Bump bytecode ABI and VMBC as a hard internal-format break. Reject earlier VMBC versions.

- [ ] Extend `validate_program` so every `Stloc` that can target a capture-backed slot has a matching proof record and every proof record matches the current Program metadata/code hash.

- [ ] Require manually assembled Programs to supply proof metadata. Programs without proof may execute only when validation proves that no local is capture-backed or no captured write exists.

- [ ] Make `Vm::new`, VMBC decode, AOT artifact load, no-std `Vm::new`, and public Program installation paths validate the proof before execution.

- [ ] Add tampering tests: changed opcode, target slot, callable layout, capture layout, local count, or proof hash must reject the Program before running.

- [ ] Verify that malformed/untrusted bytecode cannot bypass the source compiler invariant.

### Task 8: Remove desktop and no-std runtime graph walks

**Files:**

- Modify: `src/vm/mod.rs`
- Modify: `src/vm/tests.rs`
- Modify: `pd-vm-nostd/src/vm.rs`
- Modify: `pd-vm-nostd/tests/embedded_host.rs`

- [ ] Replace desktop captured-local store logic with direct cell replacement after Program validation:

```rust
if let Some(cell) = self.capture_cells.get(&absolute).cloned() {
    let previous = {
        let mut captured = cell
            .lock()
            .map_err(|_| VmError::InvalidFrameState("capture cell lock is poisoned"))?;
        core::mem::replace(&mut *captured, value.clone())
    };
    self.locals[absolute] = value;
    self.drop_value_with_contract(previous);
    return Ok(());
}
```

- [ ] Delete desktop `value_references_capture_cell` and its `HashSet` dependency.

- [ ] Remove the equivalent no-std recursive `Rc<RefCell<Value>>` graph walk and its visited-vector allocation.

- [ ] Replace runtime-cycle tests with compile rejection and Program-validation tests. Keep direct VM tests proving that validated captured scalar/callable writes update aliases and release previous values exactly once.

- [ ] Add an assertion/test hook proving the recursive runtime check is absent from interpreter and no-std execution.

### Task 9: Update native backends, recordings, and cache revisions

**Files:**

- Modify: `src/vm/native/mod.rs`
- Modify: `src/vm/native/bridge.rs`
- Modify: `src/vm/jit/*`
- Modify: `src/vm/aot/*`
- Modify: `src/debugger/recording.rs`
- Modify: relevant backend and artifact tests

- [ ] Confirm Trace JIT and AOT stores already route through verified local-slot metadata or helper contracts. Remove any duplicate cycle walk if one exists.

- [ ] Include the capture-write proof hash in native trace and AOT cache keys.

- [ ] Bump native callable ABI, AOT artifact, and debugger recording revisions because old artifacts lack the mandatory proof.

- [ ] Reject old recordings/artifacts and add tampering tests for proof mismatch.

- [ ] Run native tests showing safe captured mutation remains inside compiled execution with no interpreter handoff.

### Task 10: Final soundness gate

**Files:**

- Modify: `README.md`
- Modify: all capture-cycle test files from previous tasks

- [ ] Audit every way a `Value` can reach `Stloc`:

  - source expressions;
  - callable return values;
  - host return values;
  - callback arguments;
  - REPL persisted locals;
  - arrays/maps and dynamic indexing;
  - VMBC/AOT load;
  - public assembler and Program constructors;
  - debugger replay;
  - no-std host dispatch.

- [ ] Require each path to produce proven provenance or reject before execution.

- [ ] Run the final matrix:

```bash
cargo fmt --all -- --check
cargo check --workspace
cargo check --workspace --no-default-features
cargo check --workspace --all-features
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace --all-features --release
git diff --check
```

- [ ] Run focused compiler, REPL, linked module, VMBC, assembler, no-std, Trace JIT, AOT, debugger recording, host callback, capture aliasing, drop-contract, and artifact-tampering suites.

- [ ] Search the tree and confirm that both recursive runtime functions and their cycle-error strings are gone:

```bash
git grep -n -E "value_references_capture_cell|callable capture ownership cycle is unsupported" -- src pd-vm-nostd
```

Expected: no runtime implementation matches.

- [ ] Commit runtime removal only after every proof-entry path passes.

```bash
git add src pd-vm-nostd tests README.md
git commit -m "Remove runtime capture cycle checks"
```

## Completion criteria

- Obvious ownership cycles fail early with source-focused diagnostics.
- All remaining captured-cell writes are accepted only when static provenance proves no path back to the target cell.
- Unknown/open provenance at a captured-cell write is rejected at compile time.
- VMBC, AOT, debugger replay, assembler, and direct Program construction cannot bypass proof validation.
- Desktop and no-std no longer recursively inspect runtime `Value` graphs during captured-local stores.
- Safe mutable captures, independent closure instances, shared aliases, higher-order callables, generics, containers, callbacks, JIT, and AOT retain their behavior.
- RC ownership remains cycle-free without introducing tracing GC.