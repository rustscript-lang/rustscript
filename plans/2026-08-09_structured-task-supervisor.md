# Structured Task Supervisor Implementation Plan

**Goal:** Add implementation-independent structured concurrency for multiple pending operations and isolated child program runs without exposing Rust futures, threads, or executor handles to scripts.

**Architecture:** A run-scoped `TaskSupervisor` owns child operation/program tasks, concurrency permits, cancellation tree, result ordering, and cleanup. Tasks are descriptors validated against delegated capability profiles. Parent completion cannot leave active descendants.

**Tech Stack:** Rust 2024, RunContext, HostRuntime operation registry, isolated VM instances, bounded executor.

---

## Independence and dependency

- Depends on VM decomposition and unified host lifecycle.
- Consumes static capability identity/profile delegation.
- Independent of agent tool/subagent policy; agent RSS may wrap it later.

## Scope boundary

### In scope

- Multiple active operation/task records per run.
- Bounded `all`, `pool`, `race`, fail-fast, and isolated program fanout semantics.
- Parent/child cancellation and resource budgets.
- Ordered result collection and event association.
- Removal of the one-waiting-slot architectural limitation for structured tasks.

### Out of scope

- Source-language `async`, `await`, arbitrary futures, or shared-memory threads.
- Agent-specific tool, provider, or subagent descriptors.
- Mutable resource sharing between child VMs.
- Distributed execution or durable background jobs.

## Target contracts

```text
TaskSupervisor
  spawn(descriptor, delegated_profile)
  all(task_ids)
  pool(descriptors, max_concurrency, fail_fast)
  race(task_ids)
  cancel(task_id/reason)
  cancel_all(reason)

TaskDescriptor
  host operation
  isolated program + input

TaskResult
  index
  terminal status
  return value or structured error
  usage
```

## Implementation route

### Milestone 1: Freeze structured semantics with tests

Cover:

- ordered all/pool results despite completion order;
- race returns first success and cancels remaining tasks;
- fail-fast cancellation;
- collect-all partial failures;
- parent cancellation reaches every descendant;
- child cancellation does not affect siblings by default;
- depth/fanout/active/time/fuel/operation limits;
- no child result/event after parent terminal state;
- isolated stacks/resources/capability profiles.

### Milestone 2: Add TaskSupervisor to RunContext/HostRuntime

**Files:**
- Create: `src/builtins/runtime/task.rs`
- Modify: RunContext and HostRuntime component files
- Modify: operation registry integration

1. Store task state outside Instance's single wait marker.
2. Register every task/child operation in the shared operation registry.
3. Allocate permits before spawn.
4. Create child cancellation tokens under the run token.
5. Associate result/event/usage with task and parent run identity.

### Milestone 3: Support isolated child programs

1. Spawn a fresh Instance and RunContext from immutable Program/Engine references.
2. Delegate only a subset of the parent capability profile.
3. Prohibit mutable resource-handle transfer.
4. Bound child input/output/event bytes.
5. Finalize child outcome before collection.

### Milestone 4: Add generic task host surface

Expose implementation-independent operations such as:

```text
task::all
task::pool
task::race
task::run_program
task::run_program_many
```

Descriptors contain only generic host-operation or program references. They cannot contain agent tool/provider names.

### Milestone 5: Integrate scheduler wake/resume

1. Permit multiple active task operations while Instance waits on one structured join/select result.
2. Wake the instance when the requested aggregate condition is met.
3. Keep remaining task state under supervisor ownership.
4. Cancel and clean all descendants before parent terminal completion.

### Milestone 6: Verification

```bash
cargo fmt --all -- --check
cargo test --locked --test runtime_task_tests
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
git diff --check
```

Include stress tests for permit exhaustion, cancellation races, deterministic ordering, nested depth, and cleanup counters.

## Target criteria

- One run can own multiple active tasks under explicit bounds.
- Structured joins/races have deterministic documented semantics.
- Parent terminal state implies zero active descendants.
- Child VMs share immutable program/engine data only.
- Capability delegation can only narrow access.
- Mutable resource handles never cross child boundaries.
- Agent/provider concepts do not appear in the core task descriptors.
