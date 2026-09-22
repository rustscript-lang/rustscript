# Standard timer host module

The standard `timer` module provides four host functions:

- `timer::at(delay_ms, callback)` — register one callback.
- `timer::every(interval_ms, callback)` — register a repeating callback.
- `timer::pending_count()` — ask the backend how many registrations are waiting
  to begin or waiting for a running slot.
- `timer::running_count()` — ask the backend how many callback executions are
  live. A callback paused on an async host operation remains running.

`delay_ms` must be non-negative. `interval_ms` must be positive. The defaults
are `DEFAULT_MAX_PENDING_TIMERS` (1024) pending registrations and
`DEFAULT_MAX_RUNNING_TIMERS` (256) concurrently running callbacks; both are
re-exported at the crate root, and an embedding can install different
`TimerConfig` limits.

The callback parameter is typed `fn(bool) -> unknown`: the callback observes the
`premature` flag and its **return value is discarded**, so the declared result
is `unknown` — any `int`/`string`/`map`/`bool`/`null` body compiles and runs,
and the value is dropped instead of accumulating. This is the one deliberate
dynamic occurrence in the public timer surface, and the typed-catalog guard
(`tests/typed_host_no_dynamic_contract_tests.rs`) records it as its narrow
discarded-callable-result policy exception. A callback body may therefore end
with any expression — for example `timer::at(25, |premature| null)`,
`timer::at(25, |premature| 1)`, or
`timer::at(25, |premature| if true => { work(); null } else => { null })`.

## Installation

A VM resolves the timer imports as soon as it is bound to a registry that
composes the standard catalog, but a call fails until backend state is
installed. Two installation shapes exist:

- Compose the extension —
  `vm.install_extension(&TimerExtension::new(backend, config))?` registers the
  four timer host functions from the standard catalog and installs the
  `TimerBackend` state in one call.
- Install state directly — `vm.install_timer_runtime(backend, config)` (the
  `TimerHostExt` trait) installs backend state for a VM whose timer functions
  were registered through `register_timer_builtin_module` /
  `register_timer_builtin_module_from_catalog`.
  `vm.clear_timer_runtime()` removes that state again.

Because the standard catalog always exposes the timer imports, a compiled
program that only registers the functions still resolves them; each call then
fails at the runtime boundary with the installation error:

```text
timer runtime is not installed
```

`clear_timer_runtime` restores exactly that state: the registered functions
stay bound, and later calls fail with the same error until a backend is
installed again. Callback VMs hold only a weak reference to their backend, so
after the owning backend is dropped an existing callback reports
`timer runtime has shut down` instead of reusing released state.

## Generic backend boundary

`TimerBackend::register(&self, TimerRegistration)` remains the legacy
all-or-nothing entry point, so an existing backend can implement the trait
without a source change. The default `prepare_registration` creates a small
transaction that calls `register` only during commit. Backends that need strong
rollback override `prepare_registration` and return their own
`TimerRegistrationTransaction`.

Preparation may validate metadata or reserve capacity, but it cannot publish
the callback. The transaction's `commit` receives the complete
`TimerRegistration`; only a successful commit transfers ownership to the
backend. A returned commit error invokes rollback once. A commit panic leaves
the guard armed so the caller invokes rollback once. The rollback operation
must be **nonblocking and non-panicking**: it may cancel a reservation or remove
a partial publication, but it must not enter a VM/runtime, wait for worker
progress, poll callback work, or spawn a thread. If it removes a registration,
it must return that registration after releasing backend locks; the generic
guard then drops the callback VM after rollback returns.

The legacy path retains its existing limitation: a backend that panics after
publishing through `register` cannot be generically reclaimed because it did
not provide a transactional rollback operation. The helper restores the source
callback and rethrows that panic. Strong transactional backends provide the
rollback contract above.

The generic module has no request, connection, worker-phase, or `ngx.timer`
semantics. It does not create request objects, inherit request-local state, or
implement OpenResty scheduling rules. The `premature` boolean is the only
lifecycle signal supplied to a callback; the embedding defines deadlines,
worker ownership, shutdown timing, and any surrounding request policy.

## Callback ownership and rollback

The callback parameter is declared `TakeOwned`. Registration follows this
transactional order:

1. Validate the duration, before taking any argument.
2. Prepare the backend transaction from the registration metadata; preparation
   publishes no callback. The compatibility default defers legacy `register`
   until commit.
3. Clone the callback value for rollback, then take the original callback from
   the owned host call.
4. Validate the callable's complete program-local graph. Cycles are visited once;
   nested foreign callables and resource-bearing captures are rejected. Resource
   captures remain unsupported because this boundary has no resource-table
   transfer operation.
5. Create a fresh private callback VM, bind the host registry, install the
   timer module state, and adopt the validated callable graph.
6. Build the registration and commit the backend transaction.
7. Only a successful backend commit disarms rollback and transfers the callback.

A preflight, VM-spawn, or backend error restores the cloned callback into its
original host-call slot and marks that slot untaken. A returned commit error
has already run rollback once; a commit panic runs rollback once at the panic
boundary. Any registration returned by rollback is dropped only after rollback
returns and backend locks are released. The ordinary owned-dispatch failure path
then restores all untaken arguments to the guest stack exactly once. A backend
panic resumes the original panic after rollback and source restoration. A
legacy backend panic keeps the legacy limitation described above.

The fresh callback VM starts halted with no execution frames, stack, or host
return. Its callable graph remains owned by that VM for the lifetime of the
registration. Module state, host bindings, and immutable program configuration
survive a callback reset; source-VM frames, stack, locals, resources, and
waiting operations are never copied into it.

## Driving callbacks

Backends drive each accepted `OwnedTimerCallback` on the designated VM thread.
`start(premature)` begins one serialized round and passes the boolean to the
callback. A callback return value is discarded. `start` rejects overlapping
`Running`, `Waiting`, or `Yielded` rounds.

Rounds are strictly serialized: an `every` registration schedules its next
round only after the previous round reaches a terminal state, so a callback
slower than its interval never overlaps itself. Because every round reuses the
same callable value, mutable capture cells persist from one round to the next.

For synchronous error handling, inspect the `VmResult` returned by `start` and
`poll`. For a backend-owned reporting path, use `start_reporting` and
`poll_reporting`:

- `start_reporting(premature, backend)` starts a round and sends a start error
  to `report_callback_error`.
- `poll_reporting(cx, backend)` polls one waiting/resumable step and sends an
  async poll or resume error to the same sink. It returns `Pending` while the
  callback is still waiting and returns the callback's terminal/live state when
  ready.

Raw `poll` remains available when the embedding wants to handle errors itself.
Every returned poll/resume error has already moved the callback to `Complete`
and recovered its private VM before the error is returned. `poll_reporting`
therefore reports one failure for that round; a second poll of the completed
callback does not report the same failure again.

### Async host calls and the per-callback bridge

Every accepted callback owns a private VM, so async host work inside a callback
never runs on the creating request's bridge. Whenever a callback body can enter
an async host operation, the backend must install a fresh bridge for that
callback VM with `OwnedTimerCallback::set_async_bridge` **before** the first
`start` call — one bridge per callback, never shared with the source VM or with
another callback. A callback VM without its own bridge cannot suspend on async
host work.

While the callback waits, `start` and `poll` report `Waiting(op_id)`. The
backend then drives `poll_reporting(cx, backend)`: it polls one
waiting/resumable step, returns `Pending` while the operation is still
outstanding (re-poll when the bridge wakes the task), and returns the
callback's terminal or live state once the step is ready. Use `poll_reporting`
for callback rounds that can wait; it routes async poll/resume failures to
`report_callback_error` without touching the creating request VM.

Callback start, waiting poll, and resume each have a panic boundary. A callback
panic is converted to a structured `VmError::HostError`, reported through the
reporting helper when used, and followed by the same graph-preserving reset.
The callback becomes `Complete`, so a repeating registration can attempt its
next round. Backend registration failures use the registration transaction's
rollback guard: the backend must remove any publication before the source
argument is restored, and the backend panic is preserved for the embedding to
handle.

For `at`, the backend should remove the registration after the callback reaches
`Complete` or `Cancelled`. For `every`, a callback error is reported for that
round, the callback is reset for reuse, and the registration remains eligible
for later rounds. A later successful round uses the same callable capture cells.
Shutdown should stop accepting registrations, pass `premature=true` to pending
callbacks, run each of them exactly once while the backend can still execute
them, cancel active waiting operations, release every callback VM, and never
reschedule another `every` round.

## Downstream adapters: the same path under another name

An embedding that exposes this contract under its own exact host name and
schema — a seconds-based, `ngx.timer`-shaped host, for example — does not
re-implement the callback handoff. Three public items cover it:

- `HostOwnedFunction`, `OwnedHostCall`, `OwnedHostContext` (re-exported at the
  crate root) — the adapter implements `HostOwnedFunction` and receives the
  drained call;
- `OwnedHostContext::bound_program()` — the factory must retain this exact
  `Arc<BoundHostProgram>` for a production VM created with `Vm::new_bound`;
- `timer::register_owned_timer_with_bound_program(call, bound_program,
  callback_arg, delay, interval)` — takes the exact bound artifact and never
  prepares or performs a full registry bind. If `bound_program()` is `None`, a
  production bound adapter must fail closed with a host error. This prevents a
  missing binding artifact from silently changing the callback path;
- `timer::register_owned_timer(call, registry, callback_arg, delay, interval)`
  remains the explicitly labeled **registry-only fallback** for generic
  embeddings that intentionally use `bind_vm_cached`. It takes a checked
  `Duration` plus an optional repeating `Duration`, performs the same callback
  provenance and isolation checks, and restores the callback on every returned
  error or panic;
- `timer::installed_timer_counts(vm)` — returns `TimerCounts { pending, running }`
  read synchronously from the installed backend, so count functions never need
  `TimerBackend` or `TimerHostState`.

```rust
struct MySecondsTimer {
    registry: HostFunctionRegistry,
    bound_program: Option<Arc<BoundHostProgram>>,
}

impl HostOwnedAdapterFactory for MySecondsTimerFactory {
    fn create(&self, context: OwnedHostContext<'_>) -> Box<dyn HostOwnedFunction> {
        Box::new(MySecondsTimer {
            registry: context.registry().clone(),
            bound_program: context.bound_program(),
        })
    }
}

impl HostOwnedFunction for MySecondsTimer {
    fn call(&mut self, call: &mut OwnedHostCall<'_>) -> VmResult<CallOutcome> {
        let seconds = match call.arg(0) {
            Some(Value::Int(seconds)) => *seconds,
            _ => return Err(VmError::TypeMismatch("timer seconds")),
        };
        if seconds < 0 {
            return Err(VmError::HostError("timer seconds must be non-negative".into()));
        }
        let Some(bound_program) = self.bound_program.clone() else {
            return Err(VmError::HostError(
                "production timer adapter requires a bound host program".into(),
            ));
        };
        register_owned_timer_with_bound_program(
            call,
            bound_program,
            TIMER_CALLBACK_ARG,
            Duration::from_secs(seconds as u64),
            None,
        )
    }
}
```

A generic embedding that deliberately has no bound artifact may use the
registry-only fallback explicitly:

```rust
register_owned_timer(
    call,
    &self.registry,
    TIMER_CALLBACK_ARG,
    Duration::from_secs(seconds as u64),
    None,
)
```

`OwnedTimerCallback` values are only ever created inside the two registration
helpers and the standard millisecond adapters, so a downstream adapter can
never construct one directly and bypass callback provenance or VM isolation.

## Running-limit policy

`TimerRegistration.max_running` is a runtime-wide concurrent cap. A callback in
`Waiting` counts as running. When the cap is full, due callbacks remain pending;
they are not discarded and they are not started concurrently. Once a running
callback reaches a terminal state or is cancelled, the backend may admit the
next pending callback. This leave-pending policy applies to both one-shot and
repeating registrations and is part of the standard backend contract.

## Capacity: who enforces the limits

The generic module performs **no admission and keeps no counters**. Each
`TimerRegistration` carries the installed `TimerConfig` limits (`max_pending`,
`max_running`), and enforcing them is a documented **MUST** on the backend:

- the backend checks a limit and performs its own registration/scheduling under
  the same synchronization, so `max_pending` / `max_running` are hard caps
  rather than post-hoc observations;
- a rejected registration returns an error and retains nothing, and the generic
  module restores the callback to the caller (see the rollback contract above);
- `TimerBackend::prepare_registration(metadata)` may reserve capacity without
  publishing a callback. The default guard defers legacy `register` to commit;
  a strong transactional backend supplies a rollback operation that removes a
  publication after either a returned error or a panic. That rollback/cancel
  path is **NONBLOCKING and NON-PANICKING**, does not enter a VM/runtime, wait
  for worker progress, poll callback work, or spawn a thread. Any registration
  it returns is released after rollback returns and backend locks are gone;
  `Drop` invokes the same rollback once when a guard remains prepared;
- `timer::pending_count()`, `timer::running_count()`, and
  `installed_timer_counts` are pure backend queries: they return exactly
  `TimerBackend::pending_count()` / `TimerBackend::running_count()` for the
  installed runtime. The module never substitutes a module-local estimate, and
  it exposes no global or process-wide timer state.

Embeddings therefore select their own capacity by installing a `TimerConfig`
(with the documented defaults `DEFAULT_MAX_PENDING_TIMERS` = 1024 and
`DEFAULT_MAX_RUNNING_TIMERS` = 256); a backend that needs a stricter or
dynamically shared budget enforces it inside its own
`prepare_registration` / transaction scheduling path.

## VM-core boundary

The timer surface is composed from `src/builtins/runtime/mod.rs` and
re-exported from `src/lib.rs`; `tests/timer_host_arch_tests.rs` guards the
boundary. The only VM-core seam is the *generic* owned-value dispatch in
`src/vm/host.rs` (`HostOwnedFunction`, `OwnedHostCall`, `register_exact_owned`,
`OwnedHostContext`, `OwnedHostCall::spawn_owned_callable_vm`, and
`Vm::recover_owned_callable`), which carries no timer domain term. Owned
dispatch restores every argument the handler did not take exactly once on every
failure path — a host error, a panic, a rejected `Yield`, and a rejected return
alike — keeps taken arguments consumed, and never rewinds the instruction
pointer for a retry. Because the dispatch drains the operands before the
handler runs, an owned handler returning `Yield` is rejected as a structured
host error.

One adaptation note for this revision: the public owned-dispatch view types
(`HostOwnedFunction`, `OwnedHostCall`, `OwnedHostContext`) are defined in the
`host_api` vocabulary module and re-exported at the crate root, because
`src/vm`'s module re-export surface is frozen here; the dispatch and every
VM-coupled operation remain in `src/vm/host.rs`.
