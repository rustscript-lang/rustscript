# Run Outcome, Event Stream, and Runtime Error Contract Plan

**Goal:** Define one structured execution result that keeps return values, events, usage, cancellation, and errors separate and machine-readable.

**Architecture:** A run produces a terminal `RunOutcome`; events flow during execution through a bounded sink/channel and never replace the function return value. Runtime and host failures retain structured codes and context through the VM embedding boundary.

**Tech Stack:** Rust 2024, VM embedding API, runtime context/events, host errors, agent runner integration tests.

---

## Independence and dependency

- Contract design can start independently.
- Implementation depends on RunContext ownership from the VM decomposition plan.
- Operation cancellation details depend on the unified host-lifecycle plan.
- The agent run-lifecycle plan consumes this API.

## Scope boundary

### In scope

- `RunOutcome`, terminal reason, usage, return value, and structured error.
- Bounded event emission during execution.
- Event receipt/sequence semantics at the VM boundary.
- Structured runtime/host error propagation.
- Removal of stack-top/event-last inference in embedding code.

### Out of scope

- Agent event names, provider protocols, SSE framing, or Telegram rendering.
- Durable event persistence.
- Source-language concurrency syntax.
- Compatibility wrappers for ambiguous prior return behavior.

## Target contracts

```text
RunOutcome
  return_value: optional Value
  termination: completed | cancelled | failed | budget_exhausted
  error: optional RuntimeError
  usage: RunUsage
  last_event_sequence

RuntimeEvent
  sequence
  value
  payload_bytes

RuntimeError
  code
  message
  subsystem
  operation/resource context
  retryability where meaningful
  source error where meaningful
```

## Implementation route

### Milestone 1: Add contract tests

Add tests proving:

- a script may emit events and return a different value;
- zero events does not alter the return value;
- event order is monotonic;
- sink rejection/backpressure has a documented terminal behavior;
- cancellation reason survives the public VM API;
- host/runtime codes survive without string equality checks;
- usage is finalized for success, error, cancellation, and budget exhaustion.

### Milestone 2: Define terminal and usage types

**Files:**
- Modify: `src/lib.rs`
- Create: `src/vm/outcome.rs`
- Modify runtime error modules

1. Define `RunOutcome`, `RunTermination`, and `RunUsage`.
2. Make halt/failure/cancellation paths produce exactly one terminal outcome.
3. Stop requiring embedders to inspect stack top, yield reason, and side channels to infer completion.

### Milestone 3: Make events live and bounded

**Files:**
- Modify: `src/builtins/runtime/context.rs`
- Modify: `src/builtins/runtime/event.rs`
- Modify: `src/builtins/runtime/context_host.rs`
- Modify: RunContext

1. Define a bounded event sink contract.
2. Emit each accepted event during execution.
3. Allocate sequence numbers once at the run boundary.
4. Define overflow policy explicitly: block/yield, return a typed limit error, or drop only where configured with a receipt. Silent loss is prohibited.
5. Keep event values independent from function return storage.

### Milestone 4: Preserve structured errors

**Files:**
- Modify: runtime error types
- Modify: `src/vm/host.rs`
- Modify: public VM error surface

1. Carry `RuntimeErrorCode` through host completion and `RunOutcome`.
2. Include structured cancellation/deadline/resource/operation context.
3. Remove embedding logic that compares error strings such as `"cancelled"`.
4. Define rendering separately from machine-readable fields.

### Milestone 5: Migrate embedders and remove ambiguous APIs

**Files:**
- Modify examples and tests in `rustscript`
- Coordinate later changes in `rustscript-agent/src/lib.rs`

1. Consume `RunOutcome.return_value` directly.
2. Subscribe to events through the sink/channel.
3. Remove event-last and stack-last fallback behavior.
4. Remove superseded internal return APIs after migration; no dual long-term contract.

### Milestone 6: Verification

```bash
cargo fmt --all -- --check
cargo test --locked --test runtime_context_tests
cargo test --locked --test runtime_host_tests
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
git diff --check
```

## Target criteria

- Emitting an event never changes the function return value.
- Events are observable before run completion through a bounded contract.
- Every run produces one structured terminal outcome.
- Cancellation, deadline, resource, and host errors retain machine-readable codes.
- Embedders do not infer results from stack/event ordering.
- String equality is absent from cancellation/error control flow.
- Usage and event sequence metadata are finalized for every terminal path.
