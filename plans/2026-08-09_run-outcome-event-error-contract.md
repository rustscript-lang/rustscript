# Invocation Item Stream and Runtime Error Contract Plan

**Goal:** Expose one small, Rust-like pull stream for each exported RSS invocation so structured arguments, emitted items, the final return value, cancellation, and errors have unambiguous semantics.

**Architecture:** The host initializes a VM, resolves an exported callable, and starts an invocation with ordinary `Value` arguments. The invocation behaves like `Stream<Item = Result<InvocationItem, RuntimeError>>`: `Event(Value)` items may arrive during execution, one `Complete(Value)` item carries the function return value, and the stream is fused after that terminal item or one error. `Vm::run()` remains the low-level execution pump; core does not add generator syntax, a second completion future, an embedding callback sink, or event persistence policy.

**Tech Stack:** Rust 2024, existing VM callable APIs, `VmStatus`, generic runtime errors, existing async host bridge.

---

## 1. Contract and non-goals

### Public semantics

```text
InvocationItem
  Event(Value)
  Complete(Value)

Invocation stream item
  Result<InvocationItem, RuntimeError>

poll_next
  Pending(wait reason)
  Ready(Some(Ok(Event(value))))
  Ready(Some(Ok(Complete(return_value))))
  Ready(Some(Err(runtime_error)))
  Ready(None)  // only after Complete or Err
```

The concrete API may use a small VM-specific poll enum instead of implementing `futures::Stream`; core must not create an executor or require a Tokio runtime. Its observable behavior must match a fused Rust stream.

### Required rules

- Input is passed as ordinary arguments to an exported callable such as `run(input)`.
- `stream::emit(value)` produces one `Event(value)` item and never changes the callable return value.
- A normal callable return produces exactly one `Complete(value)` item.
- Cancellation or failure produces exactly one typed error item and no `Complete` item.
- The next poll after `Complete` or `Err` returns end-of-stream.
- Polling drives execution. When the consumer stops polling, the VM does not continue producing items.
- At most one event item is buffered between polls; this provides natural backpressure without an event queue.
- Core validates only the configured per-item value bound. Run IDs, event names, sequence numbers, durable cursors, retention, replay, and platform delivery belong to the embedding.

### Explicit non-goals

- No source-language `yield`, generator object, `next(value)`, or resume-value semantics.
- No `RunOutcome`, `RunTermination`, `RunUsage`, terminal future, or event receipt type.
- No ambient-input builtin or JSON-specific input/output wrapper.
- No stack-top or event-last result inference.
- No agent/provider/platform event schema in core.

## 2. Dependency boundary

- Reuse exported callable identity and `Vm::resolve_exported_callable`.
- Reuse callable execution state from `start_callable`, `run`, and `take_callable_result`.
- Reuse `HostAsyncBridge`; an outstanding host operation maps to `Pending` and is resumed by the embedding-owned driver.
- Reuse unified cancellation tokens, but expose typed invocation cancellation through the public invocation API.
- Preserve the capability-profile and host-binding contracts unchanged.

## 3. Implementation route

### Milestone 1: Freeze stream behavior with failing tests

**Files:**
- Create: `tests/invocation_stream_tests.rs`
- Modify: `tests/runtime_context_tests.rs`
- Modify: `tests/runtime_host_tests.rs`

Add tests proving:

1. `run(input)` receives the exact structured `Value` argument.
2. A script that emits `a`, emits `b`, and returns `c` produces `Event(a)`, `Event(b)`, `Complete(c)`, then end-of-stream.
3. A script with no events produces `Complete(value)`, then end-of-stream.
4. An event value never replaces or mutates the return value.
5. One poll exposes at most one event and execution does not advance while polling is paused.
6. A waiting host operation returns `Pending`, resumes through the existing async host driver, and preserves item order.
7. Cancellation produces one typed error item with its reason, then end-of-stream.
8. Fuel exhaustion, deadline expiry, and host failure each produce one typed error item, then end-of-stream.
9. Starting a second invocation on the same VM while one is active is rejected.
10. No public embedder needs to inspect the operand stack or compare error strings.

### Milestone 2: Add the minimal invocation state machine

**Files:**
- Create: `src/vm/invocation.rs`
- Modify: `src/vm/mod.rs`
- Modify: `src/vm/instance.rs`
- Modify: `src/lib.rs`

1. Define `InvocationItem::{Event(Value), Complete(Value)}`.
2. Define one public invocation handle/state with `poll_next` and typed cancellation.
3. Start only from an initialized exported callable plus ordinary arguments.
4. Reuse `start_callable`, `run`, and `take_callable_result`; do not duplicate interpreter or async-host loops.
5. Enforce one active invocation per VM and fused termination.
6. Keep `Vm::run() -> VmResult<VmStatus>` unchanged as the low-level pump.

### Milestone 3: Turn runtime emit into one stream item

**Files:**
- Modify: `src/builtins/runtime/context.rs`
- Modify: `src/builtins/runtime/event.rs`
- Modify: `src/builtins/runtime/context_host.rs`
- Modify: `src/vm/run_context.rs`

1. Remove run-scoped ambient input storage and its script-visible builtins.
2. Replace the embedding-owned `EventSink` and cumulative event counters with one pending event slot owned by the active invocation.
3. Add the single script-visible `stream::emit(value)` builtin; it validates the per-item bound, places one pending event, and yields control to the invocation poller.
4. Resume the script after the caller consumes that item; `stream::emit` still evaluates to `()` inside RSS.
5. Remove core sequence assignment, event receipts, cumulative event-byte accounting, and sink rejection wrapping.
6. Do not add a JSON-specific emit builtin; adapters encode or decode JSON outside the VM contract.

### Milestone 4: Capture the callable return explicitly

**Files:**
- Modify: `src/vm/mod.rs`
- Modify: `src/vm/instance.rs`
- Test: `tests/invocation_stream_tests.rs`

1. On normal callable completion, take the existing host callable result and emit `Complete(value)` once.
2. Never read the operand stack to infer the result.
3. Reject termination that lacks an explicit callable result as an internal frame-state error.
4. After `Complete`, release invocation state and return end-of-stream on every later poll.

### Milestone 5: Preserve typed terminal errors and cancellation

**Files:**
- Modify: `src/builtins/runtime/error.rs`
- Modify: `src/builtins/runtime/cancellation.rs`
- Modify: `src/vm/run_context.rs`
- Modify: `src/vm/invocation.rs`
- Modify: public VM error conversion paths

1. Expose typed invocation cancellation without making `RunContext` public.
2. Preserve cancellation reason, deadline, fuel, resource, operation, and host error codes through the stream item.
3. Remove `HostError(String)` flattening from runtime capability paths consumed by the invocation API.
4. Emit one error item, cancel outstanding owned operations, release invocation state, and fuse the stream.
5. Do not add string parsing or dual legacy error contracts.

### Milestone 6: Migrate embedders and remove superseded APIs

**Files:**
- Modify: RustScript examples and embedding tests
- Coordinate: `rustscript-agent/src/lib.rs`

1. Resolve an exported `run` callable and pass structured input as its argument.
2. Consume `Event` and `Complete` items in order.
3. Remove `events.last()`, `stack.last()`, ambient runtime input, and event sink setup.
4. Remove superseded runtime input/event exports after all in-repository consumers migrate.

### Milestone 7: Verification

```bash
cargo fmt --all -- --check
cargo test --locked --test invocation_stream_tests
cargo test --locked --test runtime_context_tests
cargo test --locked --test runtime_host_tests
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
git diff --check
```

## 4. Target criteria

- The public invocation surface has one input path: exported callable arguments.
- The invocation yields zero or more `Event` items, then exactly one `Complete` item or one typed error, then ends.
- Events never replace the callable return value.
- Backpressure follows polling; no unbounded or embedding-callback event queue exists in core.
- `Vm::run` remains a low-level pump and no executor is introduced.
- Cancellation and runtime failures remain machine-readable.
- Core carries no event sequence, persistence, replay, or platform policy.
- No generator syntax or compatibility wrapper is introduced.
