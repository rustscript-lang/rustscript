# VM Runtime Decomposition Implementation Plan

**Goal:** Split the current monolithic `Vm` state into explicit engine, program, instance, run-context, and host-runtime ownership layers.

**Architecture:** Immutable compiled artifacts and backend caches live outside per-run execution state. An `Instance` owns interpreter state, a `RunContext` owns one execution's input/budgets/events/cancellation, and `HostRuntime` owns capabilities/resources/operations. The migration preserves observable execution behavior while removing subsystem-specific fields from the central VM object.

**Tech Stack:** Rust 2024, `pd-vm` interpreter/JIT/AOT integration, existing compiler and runtime tests.

---

## Independence and dependency

- Static builtin IDs should land first so decomposition does not move an unstable wire catalog.
- Defines ownership required by the unified host-lifecycle and RunOutcome plans.
- Independent of agent providers, gateway routes, module semantics, and new host functions.

## Scope boundary

### In scope

- Ownership split for program, backend cache, interpreter instance, run-scoped context, and host runtime.
- Explicit reset/drop semantics for each layer.
- Removal of subsystem fields from the top-level VM facade.
- Migration of embedding entry points to the new ownership model.

### Out of scope

- New language syntax or bytecode operations.
- New host capabilities.
- Compatibility adapters for every prior internal API.
- JIT/AOT optimization redesign.
- Agent-specific execution policy.

## Target model

```text
Engine
  backend configuration
  decoded/JIT/AOT caches
  code-generation telemetry

Program
  immutable bytecode
  constants and metadata
  import requirements

Instance
  instruction pointer
  stack, locals, frames, captures
  yield/wait state

RunContext
  input
  event channel
  fuel/deadline/cancellation
  usage accounting

HostRuntime
  capability profile
  resources
  operations
  executor
```

The public facade may be renamed or retained, but ownership must follow this model.

## Implementation route

### Milestone 1: Add ownership tests

**Files:**
- Add focused tests under `tests/vm/`
- Modify reset/reuse tests

Prove:

- one immutable program can create multiple isolated instances;
- run input/events/budgets never leak between runs;
- backend cache may be shared without sharing stacks/resources;
- reset closes run-scoped state and retains only documented reusable state.

### Milestone 2: Extract immutable Program and Engine state

**Files:**
- Modify: `src/vm/mod.rs`
- Create: `src/vm/engine.rs`
- Create or refine: `src/vm/program.rs`
- Move backend cache ownership from VM fields

1. Remove raw program pointer/cache duplication from per-run state.
2. Give Engine explicit cache keys and invalidation rules.
3. Keep Program immutable after validation/binding metadata construction.
4. Test program sharing across interpreter-only, JIT, and AOT configurations.

### Milestone 3: Extract Instance state

**Files:**
- Create: `src/vm/instance.rs`
- Modify interpreter dispatch and frame helpers

Move IP, stack, locals, frames, captures, callbacks, waiting/yield state, and instance-only counters. Define one lifecycle from new to halted/failed/cancelled.

### Milestone 4: Introduce RunContext

**Files:**
- Create: `src/vm/run_context.rs`
- Move runtime input, event sink, fuel, epoch/deadline, cancellation, and usage state

1. Create a fresh RunContext per execution.
2. Make cancellation and deadline mandatory run-owned data, with explicit unlimited settings where allowed.
3. Remove source injection and embedding-global event ownership from execution paths.
4. Make run completion consume/finalize the context.

### Milestone 5: Extract HostRuntime shell

**Files:**
- Create: `src/vm/host_runtime.rs`
- Modify: `src/vm/host.rs`
- Modify: `src/builtins/runtime/mod.rs`

Move capability profile, host bindings, resource tables, operation registry, and executor references behind HostRuntime. Subsystem migration proceeds in the separate host-lifecycle plan.

### Milestone 6: Remove duplicate lifecycle paths

1. Replace central constructor/reset/drop field lists with component lifecycle methods.
2. Remove fields that exist only as transitional mirrors.
3. Remove old internal APIs once all callers move; no long-lived compatibility layer.
4. Document thread-safety and clone semantics for Engine, Program, Instance, RunContext, and HostRuntime.

### Milestone 7: Verification

```bash
cargo fmt --all -- --check
cargo test --locked --workspace --all-features
cargo test --locked -p pd-vm-nostd
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
git diff --check
```

Add behavioral comparison fixtures that run the same program before and after each extraction milestone.

## Target criteria

- Immutable Program data and backend caches are not owned by per-run state.
- Stack/frame/wait state is isolated in Instance.
- Input/events/budget/cancellation are isolated in RunContext.
- Capabilities/resources/operations are isolated in HostRuntime.
- Reset and drop no longer enumerate every runtime subsystem in one central method.
- Multiple instances from one program cannot share mutable run or host resources.
- Existing interpreter/JIT/AOT/no-std behavior tests remain passing.
