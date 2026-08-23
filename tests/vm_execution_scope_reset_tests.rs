//! Focused tests for the VM two-phase execution-scope reset contract.
//!
//! These exercise the public reset surface through [`Vm`]:
//!
//! - a fresh VM starts `Ready`/reusable and runs; a `Resetting` or
//!   `Poisoned` VM rejects `run`/`resume` and pool reuse;
//! - [`Vm::begin_reset_for_reuse`] is first-reason/deadline-wins and
//!   idempotent; [`Vm::poll_reset_for_reuse`] drives the scope close with a
//!   testable passed-in `now` (no sleeping);
//! - a genuinely pending scope resource blocks reset *and* pool reuse until
//!   it is released; a sync (non-pending) reset recycles the scope so an old
//!   handle is rejected with `ResourceHandleWrongTable`;
//! - cleanup errors and deadline timeouts poison the VM; the old scope and
//!   the recorded error are preserved (never replaced, never claimed clean);
//! - the module store survives a successful reset;
//! - interpreter state (stack/ip/frames) is only rewound at the successful
//!   completion endpoint, never while pending;
//! - the compat [`Vm::reset_for_reuse`] stays synchronous when it can, turns
//!   into a structured `ResetPending` (observable via [`Vm::reset_error`])
//!   when a pending resource blocks it, and is completed through the poll
//!   API — it never busy-loops.
//!
//! Only fake generic [`HostResource`] / [`HostOperation`] types are used (no
//! sql/io/http/SSE/rusqlite, no concrete builtin).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use vm::execution_scope::{
    ExecutionScopeError, ScopeCloseError, ScopeCloseFailure, ScopeCloseOutcome, ScopeState,
};
use vm::operation::{HostOperation, OperationCancelReason, OperationResult, OperationSpec};
use vm::resource::{
    CloseProgress, HostResource, Resource, ResourceCloseReason, ResourceError, ResourceErrorCode,
    ResourceResult,
};
use vm::{
    BeginResetOutcome, HostContextErrorKind, Program, Value, Vm, VmError, VmResetError,
    VmResetState, VmStatus, compile_source,
};

// ---- fake generic resources / operations ------------------------------------

/// Synchronous close (no pending phase).
#[derive(Default)]
struct SyncResource;

impl HostResource for SyncResource {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        Ok(CloseProgress::Ready)
    }
}

/// A resource whose close stays `Pending` until a shared gate is released.
struct GatedResource {
    released: Arc<AtomicBool>,
    polls: Arc<AtomicUsize>,
}

impl GatedResource {
    fn new() -> (Self, Arc<AtomicBool>, Arc<AtomicUsize>) {
        let released = Arc::new(AtomicBool::new(false));
        let polls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                released: released.clone(),
                polls: polls.clone(),
            },
            released,
            polls,
        )
    }
}

impl HostResource for GatedResource {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        Ok(CloseProgress::Pending)
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        if self.released.load(Ordering::SeqCst) {
            Poll::Ready(Ok(()))
        } else {
            self.polls.fetch_add(1, Ordering::SeqCst);
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// A resource whose close poll reports a cleanup failure.
struct FailingResource;

impl HostResource for FailingResource {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        Ok(CloseProgress::Pending)
    }

    fn poll_close(&mut self, _cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        Poll::Ready(Err(ResourceError::new(
            ResourceErrorCode::ResourceCleanupFailed,
            "test",
            "scope cleanup failed",
        )))
    }
}

/// A weakly-driven operation that stays pending until the scope cancels it.
struct TrackedOperation;

impl HostOperation for TrackedOperation {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
        Poll::Pending
    }

    fn cancel(&mut self, _reason: OperationCancelReason) -> OperationResult<()> {
        Ok(())
    }
}

// ---- helpers -----------------------------------------------------------------

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn noop_waker() -> Waker {
    Waker::from(Arc::new(NoopWake))
}

/// A program that pushes `7` and returns (leaves a non-empty stack).
fn seven_program() -> vm::CompiledProgram {
    compile_source("7;").expect("seven program should compile")
}

/// Pushes a `GatedResource` into the VM's scope and returns the release gate.
fn push_gated(vm: &mut Vm) -> Arc<AtomicBool> {
    let mut cx = vm.host_context();
    let (resource, released, _polls) = GatedResource::new();
    cx.push_resource(resource).expect("push gated resource");
    released
}

/// Polls the in-progress reset to completion, panicking on the error path
/// (used by tests that expect a successful reset).
fn drive_reset_to_success(vm: &mut Vm, now: Instant) {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    loop {
        match vm.poll_reset_for_reuse(&mut cx, now) {
            Poll::Pending => continue,
            Poll::Ready(result) => {
                result.expect("reset should complete successfully");
                break;
            }
        }
    }
}

/// Polls the in-progress reset until it terminates (Pending or error),
/// returning the final result as `Ok(())` or the structured reset error.
fn poll_reset_until_terminal(vm: &mut Vm, now: Instant) -> Result<(), VmResetError> {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    loop {
        match vm.poll_reset_for_reuse(&mut cx, now) {
            Poll::Pending => continue,
            Poll::Ready(Ok(())) => return Ok(()),
            Poll::Ready(Err(error)) => {
                let VmError::Reset(reset_error) = error else {
                    panic!("expected a structured vm reset error, got {error:?}");
                };
                return Err(reset_error);
            }
        }
    }
}

// ---- fresh VM: Ready / reusable / runnable -----------------------------------

#[test]
fn fresh_vm_is_ready_reusable_and_runnable() {
    let mut vm = Vm::try_new(seven_program().program).expect("test VM construction must not fail");

    // A brand-new VM is Ready and may be lent out of a pool.
    assert_eq!(vm.reset_state(), VmResetState::Ready);
    assert!(vm.is_reusable());

    // A normal new VM runs without any reset gating.
    assert_eq!(vm.run().expect("fresh vm should run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7)]);
    // Running does not change the reuse state.
    assert!(vm.is_reusable());
    assert_eq!(vm.reset_state(), VmResetState::Ready);
}

// ---- sync reset: fresh scope / old handle WrongTable --------------------------

#[test]
fn sync_reset_recycles_scope_and_rejects_old_handle() {
    let mut vm = Vm::try_new(seven_program().program).expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("run should halt"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7)]);

    // Push a synchronously-closing resource so we hold an old-scope handle.
    let old_handle = {
        let mut cx = vm.host_context();
        cx.push_resource(SyncResource)
            .expect("push into active scope")
    };
    let old_handle: Resource<SyncResource> = old_handle;

    // Compat path: no pending resource -> the reset completes synchronously.
    vm.reset_for_reuse();
    assert_eq!(vm.reset_state(), VmResetState::Ready);
    assert!(vm.is_reusable());
    assert_eq!(vm.reset_error(), None);

    // The scope was recycled: the installed scope is a fresh Active one.
    assert_eq!(
        vm.host_context().scope_state(),
        ScopeState::Active,
        "reset must install a fresh active scope"
    );

    // Interpreter state was rewound at the successful endpoint.
    assert!(vm.stack().is_empty());
    assert_eq!(vm.ip(), 0);
    assert_eq!(
        vm.execution_frames().len(),
        1,
        "reset must reinstall the root frame"
    );

    // The old-scope handle is rejected by the new scope's table.
    let error = vm
        .host_context()
        .execution_scope()
        .resources()
        .get(&old_handle)
        .expect_err("an old-scope handle must be rejected by the fresh scope");
    assert_eq!(error.code(), ResourceErrorCode::ResourceHandleWrongTable);

    // The fresh scope is live: a new handle resolves.
    let new_handle = vm
        .host_context()
        .push_resource(SyncResource)
        .expect("fresh scope accepts a new resource");
    vm.host_context()
        .execution_scope()
        .resources()
        .get(&new_handle)
        .expect("a new-scope handle must resolve in the fresh scope");

    // And the VM runs again.
    assert_eq!(vm.run().expect("vm should run again"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7)]);
}

// ---- pending scope resource blocks reset + pool, then completes --------------

#[test]
fn pending_scope_resource_blocks_reset_and_pool_then_completes() {
    let mut vm = Vm::try_new(seven_program().program).expect("test VM construction must not fail");
    let released = push_gated(&mut vm);

    assert_eq!(
        vm.begin_reset_for_reuse(ResourceCloseReason::VmReset, None)
            .expect("begin reset"),
        BeginResetOutcome::Started
    );
    // While the close is driven but the resource is still pending, the VM is
    // Resetting and never reusable (pool gate closed).
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let now = Instant::now();
    assert!(
        matches!(vm.poll_reset_for_reuse(&mut cx, now), Poll::Pending),
        "the reset must stay pending while the gated resource is unreleased"
    );
    assert_eq!(vm.reset_state(), VmResetState::Resetting);
    assert!(!vm.is_reusable(), "resetting vm must not be lent out");
    assert!(
        matches!(vm.reset_error(), Some(VmResetError::ResetPending { .. })),
        "pending state must carry the structured ResetPending diagnostic"
    );

    // Release the resource: polling (with a fresh `now`) completes.
    released.store(true, Ordering::SeqCst);
    drive_reset_to_success(&mut vm, Instant::now());
    assert_eq!(vm.reset_state(), VmResetState::Ready);
    assert!(vm.is_reusable());
    assert_eq!(vm.reset_error(), None);
}

// ---- pending OPERATION is cancelled by the reset and does not block ----------

#[test]
fn pending_operation_is_cancelled_and_drained_by_the_reset() {
    let mut vm = Vm::try_new(Program::new(Vec::new(), Vec::new()))
        .expect("test VM construction must not fail");
    {
        let mut cx = vm.host_context();
        cx.start_operation(OperationSpec::new(TrackedOperation))
            .expect("start operation in active scope");
        assert_eq!(cx.execution_scope().operations().len(), 1);
    }

    assert_eq!(
        vm.begin_reset_for_reuse(ResourceCloseReason::VmReset, None)
            .expect("begin reset"),
        BeginResetOutcome::Started
    );
    // The scope drains the pending operation (cancel) in its first phase, so
    // the scope quiesces and the reset completes on the first poll.
    drive_reset_to_success(&mut vm, Instant::now());
    assert_eq!(vm.reset_state(), VmResetState::Ready);
    assert_eq!(
        vm.host_context().execution_scope().operations().len(),
        0,
        "the fresh scope starts with no operations"
    );
}

// ---- cleanup error -> Poisoned (scope preserved, never replaced) --------------

#[test]
fn cleanup_error_poisons_without_replacing_the_scope() {
    let mut vm = Vm::try_new(Program::new(Vec::new(), Vec::new()))
        .expect("test VM construction must not fail");
    {
        let mut cx = vm.host_context();
        cx.push_resource(FailingResource)
            .expect("push failing resource into active scope");
    }

    assert_eq!(
        vm.begin_reset_for_reuse(ResourceCloseReason::VmReset, None)
            .expect("begin reset"),
        BeginResetOutcome::Started
    );
    let reset_error = poll_reset_until_terminal(&mut vm, Instant::now())
        .expect_err("a cleanup error must terminate the reset with a structured error");
    let VmResetError::ScopeCleanup(ScopeCloseFailure {
        first: ScopeCloseError::Resource(error),
        ..
    }) = &reset_error
    else {
        panic!("expected a preserved scope cleanup error, got {reset_error:?}");
    };
    assert_eq!(error.code(), ResourceErrorCode::ResourceCleanupFailed);

    // Poisoned: the pooled VM must never be lent out again.
    assert_eq!(vm.reset_state(), VmResetState::Poisoned);
    assert!(!vm.is_reusable());

    // The old scope is preserved and was NOT replaced by a fresh scope.
    assert_eq!(
        vm.host_context().scope_state(),
        ScopeState::Quiescent,
        "the poisoned scope stays in place (quiescent, not replaced)"
    );
    assert!(
        matches!(
            vm.host_context().execution_scope().terminal(),
            Some(ScopeCloseOutcome::SuccessWithErrors(_))
        ),
        "the preserved scope must keep its non-clean terminal outcome"
    );

    // run/resume are rejected on a poisoned VM.
    assert!(matches!(
        vm.run(),
        Err(VmError::Reset(VmResetError::NotReusable {
            state: VmResetState::Poisoned,
            stage: "run",
        }))
    ));

    // Repeated polls keep returning the same structured poison error.
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    match vm.poll_reset_for_reuse(&mut cx, Instant::now()) {
        Poll::Ready(Err(VmError::Reset(VmResetError::ScopeCleanup(_)))) => {}
        other => panic!("repeated poll after poisoning must stay stable, got {other:?}"),
    }
}

// ---- deadline -> Poisoned (no fake cleanup claim) ------------------------------

#[test]
fn deadline_poisons_without_claiming_cleanup() {
    let mut vm = Vm::try_new(Program::new(Vec::new(), Vec::new()))
        .expect("test VM construction must not fail");
    let _released = push_gated(&mut vm);

    let deadline = Instant::now() + Duration::from_secs(3600);
    assert_eq!(
        vm.begin_reset_for_reuse(ResourceCloseReason::Deadline, Some(deadline))
            .expect("begin reset"),
        BeginResetOutcome::Started
    );

    // Before the deadline the reset is still in progress (no sleeping needed:
    // `now` is a crafted instant strictly before `deadline`).
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert!(
        matches!(
            vm.poll_reset_for_reuse(&mut cx, deadline - Duration::from_millis(1)),
            Poll::Pending
        ),
        "the reset must still be pending before the deadline"
    );

    // Even though the resource is still unreleased (never cleaned), passing a
    // `now` past the deadline must poison — resources are NOT claimed clean.
    // The typed pool-contract error is ScopeCleanupDeadline (recycle
    // deadline; the VM is permanently discarded).
    let past = deadline + Duration::from_millis(1);
    match vm.poll_reset_for_reuse(&mut cx, past) {
        Poll::Ready(Err(VmError::Reset(VmResetError::ScopeCleanupDeadline { .. }))) => {}
        other => panic!("expected a scope cleanup deadline poison, got {other:?}"),
    }
    assert_eq!(vm.reset_state(), VmResetState::Poisoned);
    assert!(!vm.is_reusable());

    // The old scope is still there, still Closing (the resource was never
    // force-cleared), i.e. cleanup was not faked.
    assert_eq!(vm.host_context().scope_state(), ScopeState::Closing);
    assert!(
        !vm.host_context().execution_scope().resources().is_empty(),
        "the pending resource must still be registered (cleanup was not faked)"
    );

    // `reset_error` keeps the recycle deadline for diagnostics.
    assert!(matches!(
        vm.reset_error(),
        Some(VmResetError::ScopeCleanupDeadline { .. })
    ));
}

// ---- module state survives a successful reset ----------------------------------

#[test]
fn module_state_survives_reset() {
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ModuleState {
        count: u32,
    }

    let mut vm = Vm::try_new(seven_program().program).expect("test VM construction must not fail");
    {
        let mut cx = vm.host_context();
        assert!(!cx.set_module_state(ModuleState { count: 42 }));
    }
    assert_eq!(vm.run().expect("run"), VmStatus::Halted);

    // Full two-phase reset through the poll API.
    assert_eq!(
        vm.begin_reset_for_reuse(ResourceCloseReason::VmReset, None)
            .expect("begin reset"),
        BeginResetOutcome::Started
    );
    drive_reset_to_success(&mut vm, Instant::now());
    assert_eq!(vm.reset_state(), VmResetState::Ready);

    // The module store must survive scope cleanup + legacy reset + recycle.
    assert_eq!(
        vm.host_context().module_state::<ModuleState>(),
        Some(&ModuleState { count: 42 })
    );
}

// ---- first reason / deadline, idempotence, stable repeated polls ----------------

#[test]
fn begin_is_first_reason_deadline_wins_and_repeat_begin_is_idempotent() {
    let mut vm = Vm::try_new(Program::new(Vec::new(), Vec::new()))
        .expect("test VM construction must not fail");
    let first_deadline = Instant::now() + Duration::from_secs(1);

    assert_eq!(
        vm.begin_reset_for_reuse(ResourceCloseReason::VmReset, Some(first_deadline))
            .expect("first begin"),
        BeginResetOutcome::Started
    );
    // The first reason/deadline are bound.
    assert_eq!(vm.reset_reason(), Some(ResourceCloseReason::VmReset));
    assert_eq!(vm.reset_deadline(), Some(first_deadline));
    assert_eq!(vm.reset_state(), VmResetState::Resetting);

    // A repeat begin with a different reason/deadline is an idempotent no-op:
    // still `AlreadyStarted`, and the first values are retained.
    let later_deadline = first_deadline + Duration::from_secs(1);
    assert_eq!(
        vm.begin_reset_for_reuse(ResourceCloseReason::Requested, Some(later_deadline))
            .expect("repeat begin"),
        BeginResetOutcome::AlreadyStarted
    );
    assert_eq!(
        vm.reset_reason(),
        Some(ResourceCloseReason::VmReset),
        "first reason wins"
    );
    assert_eq!(
        vm.reset_deadline(),
        Some(first_deadline),
        "first deadline wins"
    );

    // Complete the reset; a repeat begin afterwards starts a fresh cycle.
    drive_reset_to_success(&mut vm, Instant::now());
    assert_eq!(vm.reset_state(), VmResetState::Ready);
    assert_eq!(vm.reset_reason(), None, "completed reset clears the reason");

    // Repeated successful polls are stable.
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    for _ in 0..2 {
        match vm.poll_reset_for_reuse(&mut cx, Instant::now()) {
            Poll::Ready(Ok(())) => {}
            other => panic!("repeated successful poll must stay stable, got {other:?}"),
        }
    }
}

// ---- run/resume rejected while Resetting and Poisoned --------------------------

#[test]
fn run_and_resume_are_rejected_while_resetting_and_poisoned() {
    // Resetting: a gated resource keeps the reset in progress.
    let mut vm = Vm::try_new(seven_program().program).expect("test VM construction must not fail");
    let _released = push_gated(&mut vm);
    assert_eq!(
        vm.begin_reset_for_reuse(ResourceCloseReason::VmReset, None)
            .expect("begin reset"),
        BeginResetOutcome::Started
    );
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(
        vm.poll_reset_for_reuse(&mut cx, Instant::now()),
        Poll::Pending
    ));
    assert_eq!(vm.reset_state(), VmResetState::Resetting);

    // run() is rejected with the structured NotReusable error.
    assert!(matches!(
        vm.run(),
        Err(VmError::Reset(VmResetError::NotReusable {
            state: VmResetState::Resetting,
            stage: "run",
        }))
    ));
    // resume() is rejected too.
    assert!(matches!(
        vm.resume(),
        Err(VmError::Reset(VmResetError::NotReusable {
            state: VmResetState::Resetting,
            stage: "resume",
        }))
    ));

    // Poisoned: the deadline path poisons a separate VM.
    let deadline = Instant::now() + Duration::from_millis(10);
    let mut vm2 = Vm::try_new(seven_program().program).expect("test VM construction must not fail");
    let _released2 = push_gated(&mut vm2);
    assert_eq!(
        vm2.begin_reset_for_reuse(ResourceCloseReason::VmReset, Some(deadline))
            .expect("begin reset"),
        BeginResetOutcome::Started
    );
    let waker2 = noop_waker();
    let mut cx2 = Context::from_waker(&waker2);
    assert!(matches!(
        vm2.poll_reset_for_reuse(&mut cx2, deadline + Duration::from_millis(1)),
        Poll::Ready(Err(VmError::Reset(
            VmResetError::ScopeCleanupDeadline { .. }
        )))
    ));
    assert!(matches!(
        vm2.run(),
        Err(VmError::Reset(VmResetError::NotReusable {
            state: VmResetState::Poisoned,
            ..
        }))
    ));
}

// ---- stack/ip/frames cleared only at the successful endpoint --------------------

#[test]
fn stack_ip_and_frames_are_cleared_only_at_the_successful_endpoint() {
    let mut vm = Vm::try_new(seven_program().program).expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7)]);
    let ip_before = vm.ip();
    assert!(
        ip_before > 0,
        "a halted run must have advanced the instruction pointer"
    );

    let released = push_gated(&mut vm);

    assert_eq!(
        vm.begin_reset_for_reuse(ResourceCloseReason::VmReset, None)
            .expect("begin reset"),
        BeginResetOutcome::Started
    );
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(
        vm.poll_reset_for_reuse(&mut cx, Instant::now()),
        Poll::Pending
    ));

    // While pending, the interpreter state is NOT cleared and the VM is not
    // reusable (no new scope created yet).
    assert_eq!(
        vm.stack(),
        &[Value::Int(7)],
        "pending reset must not clear interpreter state"
    );
    assert_eq!(vm.ip(), ip_before, "pending reset must not rewind the ip");
    assert_eq!(vm.reset_state(), VmResetState::Resetting);
    assert!(!vm.is_reusable());

    // Only after the reset completes successfully is the state rewound.
    released.store(true, Ordering::SeqCst);
    drive_reset_to_success(&mut vm, Instant::now());
    assert_eq!(vm.reset_state(), VmResetState::Ready);
    assert!(
        vm.stack().is_empty(),
        "stack cleared at the successful endpoint"
    );
    assert_eq!(vm.ip(), 0, "ip rewound at the successful endpoint");
    assert_eq!(
        vm.execution_frames().len(),
        1,
        "root frame reinstalled at the successful endpoint"
    );
}

// ---- compat reset_for_reuse never busy-loops; pending turns into a structured
//      ResetPending and is completed through poll ---------------------------------

#[test]
fn compat_reset_with_pending_resource_stays_resetting_and_completes_via_poll() {
    let mut vm = Vm::try_new(seven_program().program).expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("run"), VmStatus::Halted);
    let released = push_gated(&mut vm);

    // The compat entry does NOT busy-loop: with a pending resource it issues
    // the close, drives exactly one poll, and keeps the VM Resetting.
    vm.reset_for_reuse();
    assert_eq!(
        vm.reset_state(),
        VmResetState::Resetting,
        "compat reset must stay Resetting when cleanup is pending"
    );
    assert!(!vm.is_reusable());
    assert!(
        matches!(
            vm.reset_error(),
            Some(VmResetError::ResetPending {
                resource_count: 1,
                operation_count: 0,
            })
        ),
        "the compat entry must surface a structured ResetPending diagnostic"
    );

    // The reset is then completed through the poll API after release.
    released.store(true, Ordering::SeqCst);
    drive_reset_to_success(&mut vm, Instant::now());
    assert_eq!(vm.reset_state(), VmResetState::Ready);
    assert!(vm.is_reusable());
    assert_eq!(vm.reset_error(), None);

    // Compat on an already-Ready empty VM stays synchronous and reusable.
    assert_eq!(vm.run().expect("run"), VmStatus::Halted);
    vm.reset_for_reuse();
    assert_eq!(vm.reset_state(), VmResetState::Ready);
    assert!(vm.is_reusable());
}

// ---- `Vm::shutdown` drives the legacy HostRuntime sweep; a following clean
//      reset must stay Ready with a fresh scope (no stale legacy latch) --------

#[test]
fn shutdown_then_clean_reset_stays_ready_with_a_fresh_scope() {
    let mut vm = Vm::try_new(seven_program().program).expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("run"), VmStatus::Halted);

    // Public shutdown runs the legacy HostRuntime reset through
    // `close_all_handles`; it never claims, poisons, or consumes anything
    // (the migration-period builtin caller only returns `()`).
    vm.shutdown();

    // A clean two-phase reset afterwards must not trip over that legacy
    // sweep: the VM returns to Ready with a fresh Active scope and no error.
    assert_eq!(
        vm.begin_reset_for_reuse(ResourceCloseReason::VmReset, None)
            .expect("begin reset after shutdown"),
        BeginResetOutcome::Started
    );
    drive_reset_to_success(&mut vm, Instant::now());

    assert_eq!(vm.reset_state(), VmResetState::Ready);
    assert!(vm.is_reusable());
    assert_eq!(vm.reset_error(), None);
    assert_eq!(
        vm.host_context().scope_state(),
        ScopeState::Active,
        "a clean reset after shutdown must install a fresh active scope"
    );
}

// ---- typed recycle deadline: never-completing close permanently discards ----

/// A resource whose close never completes: `begin_close` returns Pending and
/// every `poll_close` stays Pending forever. Only the recycle deadline can
/// stop the drain.
struct NeverCompletingResource {
    polls: Arc<AtomicUsize>,
}

impl NeverCompletingResource {
    fn new() -> (Self, Arc<AtomicUsize>) {
        let polls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                polls: polls.clone(),
            },
            polls,
        )
    }
}

impl HostResource for NeverCompletingResource {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        Ok(CloseProgress::Pending)
    }

    fn poll_close(&mut self, _cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        Poll::Pending
    }
}

#[test]
fn never_completing_close_hits_typed_recycle_deadline_and_discards() {
    let mut vm = Vm::try_new(Program::new(Vec::new(), Vec::new()))
        .expect("test VM construction must not fail");
    let (resource, polls) = NeverCompletingResource::new();
    {
        let mut cx = vm.host_context();
        cx.push_resource(resource).expect("push never-completing");
    }

    let deadline = Instant::now() + Duration::from_secs(3600);
    assert_eq!(
        vm.begin_reset_for_reuse(ResourceCloseReason::VmReset, Some(deadline))
            .expect("begin reset"),
        BeginResetOutcome::Started
    );

    // Before the deadline, the drain stays pending and the resource is polled
    // (each poll attempts the close; it never completes).
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(
        vm.poll_reset_for_reuse(&mut cx, deadline - Duration::from_millis(1)),
        Poll::Pending
    ));
    let polls_before = polls.load(Ordering::SeqCst);
    assert!(polls_before >= 1, "the close was polled at least once");

    // Passing the recycle deadline returns the typed ScopeCleanupDeadline
    // error and permanently poisons the VM (discarded, never reused).
    let past = deadline + Duration::from_millis(1);
    match vm.poll_reset_for_reuse(&mut cx, past) {
        Poll::Ready(Err(VmError::Reset(VmResetError::ScopeCleanupDeadline {
            deadline: d,
            now: n,
        }))) => {
            assert_eq!(d, deadline);
            assert_eq!(n, past);
        }
        other => panic!("expected typed ScopeCleanupDeadline, got {other:?}"),
    }
    assert_eq!(vm.reset_state(), VmResetState::Poisoned);
    assert!(!vm.is_reusable(), "discarded VM is never reusable");
    assert!(matches!(
        vm.reset_error(),
        Some(VmResetError::ScopeCleanupDeadline { .. })
    ));

    // The old scope stays in place (Closing), resources were NOT force-clean.
    assert_eq!(vm.host_context().scope_state(), ScopeState::Closing);
    assert!(
        !vm.host_context().execution_scope().resources().is_empty(),
        "the never-completing resource is still registered (no fake cleanup)"
    );

    // A poisoned VM remains safe to Drop (no panic, no further reuse).
    drop(vm);
}

// ---- explicit single-resource close failure is local; shutdown retries --------

/// A resource whose first explicit `begin_close` fails and whose retry
/// succeeds (models a transient explicit-close failure the shutdown retry
/// overcomes).
struct FailOnceThenCloseResource {
    began: Arc<AtomicUsize>,
}

impl FailOnceThenCloseResource {
    fn new() -> (Self, Arc<AtomicUsize>) {
        let began = Arc::new(AtomicUsize::new(0));
        (
            Self {
                began: began.clone(),
            },
            began,
        )
    }
}

impl HostResource for FailOnceThenCloseResource {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        if self.began.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(ResourceError::new(
                ResourceErrorCode::ResourceCleanupFailed,
                "test::FailOnceThenCloseResource",
                "first explicit close fails; shutdown retry succeeds",
            ))
        } else {
            Ok(CloseProgress::Ready)
        }
    }
}

#[test]
fn explicit_close_failure_stays_local_and_shutdown_retries_idempotent_close() {
    let mut vm = Vm::try_new(Program::new(Vec::new(), Vec::new()))
        .expect("test VM construction must not fail");
    let (resource, began) = FailOnceThenCloseResource::new();
    let handle = {
        let mut cx = vm.host_context();
        let token = cx.push_resource(resource).expect("push fail-once resource");
        // Mark guest-owned so an explicit release fires a close.
        cx.mark_resource_guest_owned(token.handle())
            .expect("mark guest owned");
        token.handle()
    };

    // Explicit single-resource close fails: the error is returned to the
    // caller (local failure) and the resource stays open for a later retry.
    let error = vm
        .host_context()
        .close_resource::<FailOnceThenCloseResource>(handle, ResourceCloseReason::Requested)
        .expect_err("the first explicit close fails locally");
    let HostContextErrorKind::Scope(ExecutionScopeError::Resource(resource_error)) = error.kind()
    else {
        panic!(
            "expected a structured resource close failure, got {:?}",
            error.kind()
        );
    };
    assert_eq!(
        resource_error.code(),
        ResourceErrorCode::ResourceCleanupFailed
    );
    assert_eq!(began.load(Ordering::SeqCst), 1, "explicit close fired once");
    assert_eq!(
        vm.host_context().resource_count(),
        1,
        "the resource stays open (local failure does not drop it)"
    );

    // The explicit failure is local: nothing was latched in the scope, no
    // terminal outcome was produced, and the VM keeps running (not poisoned).
    assert_eq!(
        vm.host_context().execution_scope().first_error(),
        None,
        "an explicit close failure returned to the caller is not latched"
    );
    assert_eq!(vm.reset_state(), VmResetState::Ready);
    assert!(vm.is_reusable());

    // Shutdown retries the idempotent close: the retry succeeds, the scope
    // quiesces cleanly, and the VM returns Ready — the transient explicit
    // failure never poisoned anything.
    assert_eq!(
        vm.begin_reset_for_reuse(ResourceCloseReason::VmReset, None)
            .expect("begin reset"),
        BeginResetOutcome::Started
    );
    drive_reset_to_success(&mut vm, Instant::now());

    // The shutdown retried begin_close (idempotent) — the second attempt
    // succeeded, so the scope closed cleanly and the VM is reusable again.
    assert_eq!(
        began.load(Ordering::SeqCst),
        2,
        "shutdown retried the close"
    );
    assert_eq!(vm.host_context().resource_count(), 0);
    assert_eq!(
        vm.host_context().scope_state(),
        ScopeState::Active,
        "a clean retry installs a fresh active scope"
    );
    assert_eq!(vm.reset_state(), VmResetState::Ready);
    assert!(vm.is_reusable());
    assert_eq!(vm.reset_error(), None);
}
