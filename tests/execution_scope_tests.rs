//! Focused tests for the host-agnostic `ExecutionScope` core state machine.
//!
//! These exercise the scope lifecycle in isolation: **Active → Closing →
//! Quiescent**, first-reason-wins close, operation drain followed by
//! child-first resource close, operation/resource Pending both blocking
//! quiescence, best-effort cleanup with the first error preserved, rejection
//! of new inserts after closing, idempotent repeat close/poll, and isolation
//! of a fresh scope's arena/generation. Only fake [`HostResource`] and
//! [`HostOperation`] types are used — no concrete VM domain/resource, no host
//! function names, no sql/io/http/SSE/tokio/rusqlite dispatch.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use vm::execution_scope::{
    ExecutionScope, ExecutionScopeError, ScopeCloseError, ScopeCloseOutcome, ScopeState,
};
use vm::operation::{
    HostOperation, OperationCancelReason, OperationError, OperationErrorCode, OperationSpec,
};
use vm::resource::{
    CloseProgress, HostResource, ResourceCloseReason, ResourceError, ResourceErrorCode,
    ResourceResult,
};

// ---- fake resources -------------------------------------------------------

/// Synchronous close that counts begin_close calls and drops.
#[derive(Default)]
struct CountingResource {
    closes: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl CountingResource {
    fn new() -> (Self, Arc<AtomicUsize>) {
        let closes = Arc::new(AtomicUsize::new(0));
        (
            Self {
                closes: closes.clone(),
                drops: Arc::new(AtomicUsize::new(0)),
            },
            closes,
        )
    }
}

impl HostResource for CountingResource {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        Ok(CloseProgress::Ready)
    }
}

impl Drop for CountingResource {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
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

    fn poll_close(&mut self, _cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        if self.released.load(Ordering::SeqCst) {
            Poll::Ready(Ok(()))
        } else {
            self.polls.fetch_add(1, Ordering::SeqCst);
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
            "poll cleanup failed",
        )))
    }
}

/// Records the order in which `begin_close` was invoked on each resource.
struct CloseRecorder {
    order: Arc<Mutex<Vec<&'static str>>>,
    name: &'static str,
}

impl HostResource for CloseRecorder {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.order.lock().unwrap().push(self.name);
        Ok(CloseProgress::Ready)
    }
}

// ---- fake operations ---------------------------------------------------------

/// In-flight operation that stays pending until the scope hard-cancels it;
/// counts every cancel delivery and can be made to fail cancellation.
struct PendingOperation {
    cancels: Arc<AtomicUsize>,
    fail_cancel: bool,
}

impl PendingOperation {
    fn new() -> (Self, Arc<AtomicUsize>) {
        let cancels = Arc::new(AtomicUsize::new(0));
        (
            Self {
                cancels: cancels.clone(),
                fail_cancel: false,
            },
            cancels,
        )
    }

    fn new_failing_cancel() -> Self {
        Self {
            cancels: Arc::new(AtomicUsize::new(0)),
            fail_cancel: true,
        }
    }
}

impl HostOperation for PendingOperation {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<vm::operation::OperationResult<()>> {
        // In-flight indefinitely; the scope drives cancellation.
        Poll::Pending
    }

    fn cancel(&mut self, _reason: OperationCancelReason) -> vm::operation::OperationResult<()> {
        self.cancels.fetch_add(1, Ordering::SeqCst);
        if self.fail_cancel {
            Err(OperationError::new(
                OperationErrorCode::OperationDriverFailed,
                "test",
                "driver refused to cancel",
            ))
        } else {
            Ok(())
        }
    }
}

// ---- helpers -------------------------------------------------------------

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn noop_waker() -> Waker {
    Waker::from(Arc::new(NoopWake))
}

fn require_send<T: Send>() {}

/// Fully drives a scope that has been `begin_close`d to quiescence.
fn drive_to_quiescence(scope: &mut ExecutionScope) -> ScopeCloseOutcome {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let mut outcome = None;
    while outcome.is_none() {
        match scope.poll_close(&mut cx) {
            Poll::Pending => continue,
            Poll::Ready(Ok(terminal)) => outcome = Some(terminal),
            Poll::Ready(Err(error)) => panic!("poll_close failed: {error:?}"),
        }
    }
    outcome.expect("outcome set")
}

// ---- Active state / generic API -------------------------------------------

#[test]
fn execution_scope_is_send_and_starts_active() {
    require_send::<ExecutionScope>();
    let scope = ExecutionScope::new();
    assert_eq!(scope.state(), ScopeState::Active);
    assert!(scope.is_active());
    assert!(!scope.is_closing());
    assert!(!scope.is_quiescent());
    assert_eq!(scope.close_reason(), None);
}

#[test]
fn active_scope_accepts_generic_resource_and_operation_api() {
    let mut scope = ExecutionScope::new();
    let (resource, closes) = CountingResource::new();
    let _token = scope
        .push_resource(resource)
        .expect("push resource in active");
    assert_eq!(scope.resources().len(), 1);

    let (op, cancels) = PendingOperation::new();
    let _id = scope
        .start_operation(OperationSpec::new(op))
        .expect("start operation in active");
    assert_eq!(scope.operations().len(), 1);

    assert!(matches!(
        scope.begin_close(ResourceCloseReason::Requested),
        Ok(true)
    ));
    assert_eq!(drive_to_quiescence(&mut scope), ScopeCloseOutcome::Success);
    assert!(scope.is_quiescent());
    assert_eq!(scope.resources().len(), 0);
    assert_eq!(scope.operations().len(), 0);
    assert_eq!(
        closes.load(Ordering::SeqCst),
        1,
        "resource close issued once"
    );
    assert_eq!(
        cancels.load(Ordering::SeqCst),
        1,
        "pending operation cancelled once"
    );
    assert_eq!(scope.state(), ScopeState::Quiescent);
}

// ---- quiescence blocked by Pending ----------------------------------------

#[test]
fn pending_operation_blocks_quiescence_until_drained() {
    let mut scope = ExecutionScope::new();
    let (op, cancels) = PendingOperation::new();
    let _id = scope
        .start_operation(OperationSpec::new(op))
        .expect("start operation");
    // No resources, but the pending operation keeps the scope from quiescing.
    assert_eq!(scope.resources().len(), 0);
    assert_eq!(scope.operations().active_count(), 1);
    assert!(
        !scope.is_quiescent(),
        "pending operation prevents quiescence"
    );

    assert!(matches!(
        scope.begin_close(ResourceCloseReason::Requested),
        Ok(true)
    ));
    // Operations are sealed+cancelled+drained before resources close.
    assert_eq!(drive_to_quiescence(&mut scope), ScopeCloseOutcome::Success);
    assert!(scope.is_quiescent());
    assert_eq!(scope.operations().len(), 0);
    assert_eq!(cancels.load(Ordering::SeqCst), 1);
}

#[test]
fn pending_resource_blocks_quiescence_until_gate_released() {
    let mut scope = ExecutionScope::new();
    let (resource, released, _polls) = GatedResource::new();
    let _token = scope.push_resource(resource).expect("push gated resource");

    assert!(matches!(
        scope.begin_close(ResourceCloseReason::ResourceClosed),
        Ok(true)
    ));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    // Genuinely-pending resource close keeps poll_close pending.
    assert_eq!(scope.poll_close(&mut cx), Poll::Pending);
    assert!(!scope.is_quiescent());
    assert_eq!(scope.resources().len(), 1);

    // Release the gate; the next poll drives to quiescence.
    released.store(true, Ordering::SeqCst);
    assert_eq!(drive_to_quiescence(&mut scope), ScopeCloseOutcome::Success);
    assert!(scope.is_quiescent());
    assert_eq!(scope.resources().len(), 0);
    assert_eq!(scope.operations().len(), 0);
}

// ---- child-first close ----------------------------------------------------

#[test]
fn resources_close_child_first_during_poll_close() {
    let mut scope = ExecutionScope::new();
    let order = Arc::new(Mutex::new(Vec::new()));
    let parent = CloseRecorder {
        order: order.clone(),
        name: "parent",
    };
    let parent_token = scope.push_resource(parent).expect("push parent");
    for name in ["child1", "child2"] {
        let child = CloseRecorder {
            order: order.clone(),
            name,
        };
        scope
            .push_child_resource(child, &parent_token)
            .expect("push child");
    }
    let root = CloseRecorder {
        order: order.clone(),
        name: "root",
    };
    let _root_token = scope.push_resource(root).expect("push root");

    assert!(matches!(
        scope.begin_close(ResourceCloseReason::Requested),
        Ok(true)
    ));
    assert_eq!(drive_to_quiescence(&mut scope), ScopeCloseOutcome::Success);
    assert!(scope.is_quiescent());

    let recorded = order.lock().unwrap().clone();
    assert_eq!(recorded.len(), 4, "every resource was begun");
    let parent_at = recorded
        .iter()
        .position(|n| *n == "parent")
        .expect("parent recorded");
    let child1_at = recorded
        .iter()
        .position(|n| *n == "child1")
        .expect("child1 recorded");
    let child2_at = recorded
        .iter()
        .position(|n| *n == "child2")
        .expect("child2 recorded");
    assert!(
        child1_at < parent_at && child2_at < parent_at,
        "children must begin closing before their parent: {recorded:?}"
    );
}

// ---- first-reason-wins ----------------------------------------------------

#[test]
fn begin_close_is_idempotent_and_first_reason_wins() {
    let mut scope = ExecutionScope::new();
    assert!(matches!(
        scope.begin_close(ResourceCloseReason::Requested),
        Ok(true)
    ));
    assert_eq!(scope.close_reason(), Some(ResourceCloseReason::Requested));
    assert!(scope.is_closing());

    // Same reason again: idempotent no-op.
    assert!(matches!(
        scope.begin_close(ResourceCloseReason::Requested),
        Ok(false)
    ));
    // A different reason is rejected and the first is preserved.
    let error = scope
        .begin_close(ResourceCloseReason::Deadline)
        .expect_err("conflicting begin_close must be rejected");
    assert!(matches!(
        error,
        ExecutionScopeError::CloseAlreadyInProgress {
            current: Some(ResourceCloseReason::Requested),
            requested: ResourceCloseReason::Deadline
        }
    ));
    assert_eq!(scope.close_reason(), Some(ResourceCloseReason::Requested));

    // Sealing is observable on the operation registry too.
    assert!(scope.operations().is_sealed());
}

// ---- failure + best-effort ------------------------------------------------

#[test]
fn first_cleanup_error_preserved_and_best_effort_continues() {
    let mut scope = ExecutionScope::new();

    // One pending operation whose cancellation fails (first error).
    let failing_op = PendingOperation::new_failing_cancel();
    scope
        .start_operation(OperationSpec::new(failing_op))
        .expect("start failing op");

    // One resource whose close poll fails, and one that closes cleanly.
    let _failing = scope
        .push_resource(FailingResource)
        .expect("push failing resource");
    let (clean, closes) = CountingResource::new();
    let _clean = scope.push_resource(clean).expect("push clean resource");

    assert!(matches!(
        scope.begin_close(ResourceCloseReason::VmReset),
        Ok(true)
    ));
    // Terminal expresses error with the preserved first failure.
    let outcome = drive_to_quiescence(&mut scope);
    assert!(matches!(
        outcome,
        ScopeCloseOutcome::SuccessWithErrors(ScopeCloseError::Operation(_))
    ));
    assert!(scope.is_quiescent());
    // Best-effort: the clean resource still closed and everything drained.
    assert_eq!(closes.load(Ordering::SeqCst), 1);
    assert_eq!(scope.resources().len(), 0);
    assert_eq!(scope.operations().len(), 0);

    // The preserved first error is stable across repeat polls.
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert_eq!(scope.poll_close(&mut cx), Poll::Ready(Ok(outcome)));
}

// ---- closing rejects new inserts ------------------------------------------

#[test]
fn closing_rejects_new_resources_and_operations() {
    let mut scope = ExecutionScope::new();
    assert!(matches!(
        scope.begin_close(ResourceCloseReason::Requested),
        Ok(true)
    ));

    let (resource, _) = CountingResource::new();
    assert!(matches!(
        scope.push_resource(resource),
        Err(ExecutionScopeError::ScopeClosing)
    ));
    let (op, _) = PendingOperation::new();
    assert!(matches!(
        scope.start_operation(OperationSpec::new(op)),
        Err(ExecutionScopeError::ScopeClosing)
    ));

    // A scope that never began closing rejects a premature poll.
    let mut fresh = ExecutionScope::new();
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(
        fresh.poll_close(&mut cx),
        Poll::Ready(Err(ExecutionScopeError::ScopeNotClosing))
    ));
}

// ---- repeat close / poll ---------------------------------------------------

#[test]
fn repeat_begin_close_and_poll_are_idempotent_after_quiescence() {
    let mut scope = ExecutionScope::new();
    let (resource, _) = CountingResource::new();
    let _token = scope.push_resource(resource).expect("push");
    assert!(matches!(
        scope.begin_close(ResourceCloseReason::Requested),
        Ok(true)
    ));
    assert_eq!(drive_to_quiescence(&mut scope), ScopeCloseOutcome::Success);

    // Repeat begins after terminal are safe no-ops preserving the reason.
    assert!(matches!(
        scope.begin_close(ResourceCloseReason::Requested),
        Ok(false)
    ));
    assert!(matches!(
        scope.begin_close(ResourceCloseReason::Requested),
        Ok(false)
    ));
    assert_eq!(scope.close_reason(), Some(ResourceCloseReason::Requested));

    // Repeat polls return the same terminal outcome, never a fake success.
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert_eq!(
        scope.poll_close(&mut cx),
        Poll::Ready(Ok(ScopeCloseOutcome::Success))
    );
    assert_eq!(
        scope.poll_close(&mut cx),
        Poll::Ready(Ok(ScopeCloseOutcome::Success))
    );
    assert!(scope.is_quiescent());
    assert_eq!(scope.resources().len(), 0);
    assert_eq!(scope.operations().len(), 0);
}

// ---- fresh-scope isolation --------------------------------------------------

#[test]
fn fresh_scope_arena_and_registry_are_isolated() {
    let mut scope_a = ExecutionScope::new();
    let mut scope_b = ExecutionScope::new();

    // Distinct generational arenas: a token from A is rejected by B.
    let (ra, _) = CountingResource::new();
    let token_a = scope_a.push_resource(ra).expect("push in A");
    let (rb, _) = CountingResource::new();
    let token_b = scope_b.push_resource(rb).expect("push in B");
    assert!(
        token_a.handle().raw() != token_b.handle().raw(),
        "independent tables must produce independent handles"
    );
    let cross_a = scope_b
        .resources()
        .get(&token_a)
        .expect_err("A's token must be rejected by B's table");
    assert_eq!(
        cross_a.code(),
        ResourceErrorCode::ResourceHandleWrongTable,
        "A's token belongs to a different arena, not a wrong type"
    );
    let cross_b = scope_a
        .resources()
        .get(&token_b)
        .expect_err("B's token must be rejected by A's table");
    assert_eq!(cross_b.code(), ResourceErrorCode::ResourceHandleWrongTable);

    // Distinct operation registries: a pending id from A is rejected by B.
    let (oa, _) = PendingOperation::new();
    let id_a = scope_a
        .start_operation(OperationSpec::new(oa))
        .expect("start op in A");
    let (ob, _) = PendingOperation::new();
    let id_b = scope_b
        .start_operation(OperationSpec::new(ob))
        .expect("start op in B");
    let op_cross_a = scope_b
        .operations()
        .status(id_a)
        .expect_err("A's operation id must be rejected by B's registry");
    assert_eq!(
        op_cross_a.code(),
        OperationErrorCode::OperationWrongRegistry,
        "A's operation belongs to a different tagged registry"
    );
    let op_cross_b = scope_a
        .operations()
        .status(id_b)
        .expect_err("B's operation id must be rejected by A's registry");
    assert_eq!(
        op_cross_b.code(),
        OperationErrorCode::OperationWrongRegistry
    );

    // A fresh scope after A quiesces still refuses A's stale handles.
    assert!(matches!(
        scope_a.begin_close(ResourceCloseReason::Requested),
        Ok(true)
    ));
    assert_eq!(
        drive_to_quiescence(&mut scope_a),
        ScopeCloseOutcome::Success
    );
    let stale_res = scope_a
        .resources()
        .get(&token_a)
        .expect_err("A's closed token must be rejected in A itself");
    // The slot is already vacant and closed (generation advances only on reuse).
    assert_eq!(stale_res.code(), ResourceErrorCode::ResourceAlreadyClosed);
    let stale_op = scope_a
        .operations()
        .status(id_a)
        .expect_err("A's drained operation id must be stale in A itself");
    assert_eq!(stale_op.code(), OperationErrorCode::OperationStale);
    assert!(scope_b.is_active(), "an untouched fresh scope stays active");
}
