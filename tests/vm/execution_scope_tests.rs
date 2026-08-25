//! Focused TDD tests for the generic, host-agnostic execution-scope lifecycle.
//!
//! These exercise the *feature-neutral* surface added by PR16 commit 2: one
//! [`ExecutionScope`] owning one resource registry and one operation registry,
//! typed generational handles, exact-once close, bounded admission, direct
//! typed cancellation, reset/drop cleanup and the slim first-reason run flag.
//!
//! Only the public, host-agnostic API is used here; constructor-dependent
//! internals (handle encoding, type mismatch through a crate-private
//! constructor) are covered by unit tests inside the crate modules.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use vm::execution_scope::{ExecutionScope, ExecutionScopeError, ScopeCloseOutcome, ScopeState};
use vm::operation::driver::{HostOperation, OperationOutcome, OperationSpec};
use vm::operation::error::{OperationErrorCode, OperationResult};
use vm::operation::{OperationCancelReason, OperationRegistry};
use vm::resource::ResourceCloseReason;
use vm::resource::ResourceTable;
use vm::resource::close::{CloseProgress, HostResource};
use vm::resource::error::{ResourceErrorCode, ResourceResult};

// ---------------------------------------------------------------- helpers

fn cx() -> Context<'static> {
    Context::from_waker(Waker::noop())
}

/// Minimal sync resource that counts close cycles.
#[derive(Debug)]
struct Counted(Arc<AtomicUsize>);
impl Counted {
    fn new() -> (Self, Arc<AtomicUsize>) {
        let closes = Arc::new(AtomicUsize::new(0));
        (Self(closes.clone()), closes)
    }
}
impl HostResource for Counted {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(CloseProgress::Ready)
    }
}

/// Driver that completes immediately.
struct DoneDriver;
impl HostOperation for DoneDriver {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
        Poll::Ready(Ok(()))
    }
    fn cancel(&mut self, _reason: OperationCancelReason) -> OperationResult<()> {
        Ok(())
    }

    fn is_quiescent(&self) -> bool {
        true
    }
}

/// Driver that stays pending until released, recording every cancel.
struct PendingDriver {
    release: Arc<Mutex<bool>>,
    cancels: Arc<Mutex<Vec<OperationCancelReason>>>,
}
impl HostOperation for PendingDriver {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
        if *self.release.lock().unwrap() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
    fn cancel(&mut self, reason: OperationCancelReason) -> OperationResult<()> {
        self.cancels.lock().unwrap().push(reason);
        Ok(())
    }

    fn is_quiescent(&self) -> bool {
        *self.release.lock().unwrap()
    }
}

/// Driver that reports a terminal cancellation before its background worker
/// has finished. The registry must wait for `done` before releasing the slot.
struct CancelAwareWorker {
    cancelled: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    quiescence_waker: Arc<Mutex<Option<Waker>>>,
}

impl HostOperation for CancelAwareWorker {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
        if self.cancelled.load(Ordering::SeqCst) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn cancel(&mut self, _reason: OperationCancelReason) -> OperationResult<()> {
        self.cancelled.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn is_quiescent(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }

    fn register_quiescence_waker(&mut self, cx: &Context<'_>) {
        *self.quiescence_waker.lock().unwrap() = Some(cx.waker().clone());
    }
}

// ------------------------------------------------------------------ scope

#[test]
fn scope_begins_active_and_exposes_registries() {
    let scope = ExecutionScope::new().expect("scope");
    assert!(scope.is_active());
    assert!(!scope.is_closing());
    assert!(!scope.is_quiescent());
    assert_eq!(scope.state(), ScopeState::Active);
    assert_eq!(scope.resources().len(), 0);
    assert!(scope.resources().is_empty());
    assert!(scope.operations().is_empty());
    assert!(scope.terminal().is_none());
    assert!(scope.close_reason().is_none());
}

#[test]
fn close_is_first_reason_wins_and_rejects_conflict() {
    let mut scope = ExecutionScope::new().expect("scope");
    // First transition succeeds.
    assert!(
        scope
            .begin_close(ResourceCloseReason::Requested)
            .expect("first close must begin")
    );
    assert!(scope.is_closing());
    assert_eq!(scope.close_reason(), Some(ResourceCloseReason::Requested));
    // Repeat with the bound reason is a no-op.
    assert!(
        !scope
            .begin_close(ResourceCloseReason::Requested)
            .expect("repeat with same reason is idempotent")
    );
    // A conflicting reason is rejected and the first reason preserved.
    let error = scope
        .begin_close(ResourceCloseReason::Deadline)
        .expect_err("conflicting reason must be rejected");
    let ExecutionScopeError::CloseAlreadyInProgress { current, requested } = error else {
        panic!("expected CloseAlreadyInProgress, got {error:?}");
    };
    assert_eq!(current, Some(ResourceCloseReason::Requested));
    assert_eq!(requested, ResourceCloseReason::Deadline);
    assert_eq!(scope.close_reason(), Some(ResourceCloseReason::Requested));
}

#[test]
fn closed_scope_rejects_new_inserts() {
    let mut scope = ExecutionScope::new().expect("scope");
    scope
        .begin_close(ResourceCloseReason::Requested)
        .expect("close");
    let (res, _) = Counted::new();
    let error = scope
        .push_resource(res)
        .expect_err("closed scope rejects push");
    assert_eq!(error, ExecutionScopeError::ScopeClosing);
    assert!(
        scope
            .start_operation(OperationSpec::new(DoneDriver))
            .is_err()
    );
}

#[test]
fn empty_scope_quiesces_cleanly() {
    let mut scope = ExecutionScope::new().expect("scope");
    scope
        .begin_close(ResourceCloseReason::Requested)
        .expect("close");
    match scope.poll_close(&mut cx()) {
        Poll::Ready(Ok(ScopeCloseOutcome::Success)) => {}
        other => panic!("expected clean quiescence, got {other:?}"),
    }
    assert!(scope.is_quiescent());
    assert_eq!(scope.state(), ScopeState::Quiescent);
    // Idempotent terminal read.
    match scope.poll_close(&mut cx()) {
        Poll::Ready(Ok(ScopeCloseOutcome::Success)) => {}
        other => panic!("terminal poll must be idempotent, got {other:?}"),
    }
}

#[test]
fn poll_close_stays_pending_until_operation_worker_quiesces() {
    let mut scope = ExecutionScope::new().expect("scope");
    let cancels = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(Mutex::new(false));
    scope
        .start_operation(OperationSpec::new(PendingDriver {
            release: Arc::clone(&release),
            cancels: Arc::clone(&cancels),
        }))
        .expect("start");
    scope
        .begin_close(ResourceCloseReason::Deadline)
        .expect("close");

    // The pending operation blocks quiescence; poll_close must keep returning
    // Pending (the cancel is recorded but the worker has not quiesced).
    assert!(matches!(scope.poll_close(&mut cx()), Poll::Pending));
    assert_eq!(
        cancels.lock().unwrap()[..],
        [OperationCancelReason::Deadline]
    );
    assert!(scope.is_closing());

    // Release the worker; the next poll drives the terminal slot and quiesces.
    *release.lock().unwrap() = true;
    let mut quiesced = false;
    for _ in 0..4 {
        if let Poll::Ready(Ok(outcome)) = scope.poll_close(&mut cx()) {
            match outcome {
                ScopeCloseOutcome::Success => {
                    quiesced = true;
                    break;
                }
                other => panic!("expected clean quiescence, got {other:?}"),
            }
        }
    }
    assert!(quiesced, "scope must quiesce after the worker releases");
    assert!(scope.is_quiescent());
}

#[test]
fn canceled_terminal_operation_waits_for_worker_before_cleanup() {
    let mut scope = ExecutionScope::new().expect("scope");
    let (resource, closes) = Counted::new();
    scope.push_resource(resource).expect("resource");
    let cancelled = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let quiescence_waker = Arc::new(Mutex::new(None));
    let operation = scope
        .start_operation(OperationSpec::new(CancelAwareWorker {
            cancelled: Arc::clone(&cancelled),
            done: Arc::clone(&done),
            quiescence_waker: Arc::clone(&quiescence_waker),
        }))
        .expect("start");
    scope
        .begin_close(ResourceCloseReason::VmReset)
        .expect("close");

    assert!(matches!(scope.poll_close(&mut cx()), Poll::Pending));
    assert!(!scope.is_quiescent());
    assert_eq!(closes.load(Ordering::SeqCst), 0);
    assert!(scope.operations().status(operation).is_ok());

    done.store(true, Ordering::SeqCst);
    if let Some(waker) = quiescence_waker.lock().unwrap().take() {
        waker.wake();
    }
    assert!(matches!(
        scope.poll_close(&mut cx()),
        Poll::Ready(Ok(ScopeCloseOutcome::Success))
    ));
    assert_eq!(closes.load(Ordering::SeqCst), 1);
    assert!(scope.operations().status(operation).is_err());
}

// ------------------------------------------------------------------ resources

#[test]
fn push_and_close_round_trip_a_typed_resource() {
    let mut table = ResourceTable::new().expect("table");
    let (res, closes) = Counted::new();
    let token = table.push(res).expect("push");
    assert_eq!(table.len(), 1);
    table
        .begin_close(token, ResourceCloseReason::Requested)
        .expect("begin_close");
    assert_eq!(closes.load(Ordering::SeqCst), 1);
    assert_eq!(table.len(), 0);
    // Re-close of the same token is an exact-once no-op (already closed).
    let error = table
        .begin_close(token, ResourceCloseReason::Requested)
        .expect_err("already closed");
    assert_eq!(error.code(), ResourceErrorCode::ResourceAlreadyClosed);
    assert_eq!(closes.load(Ordering::SeqCst), 1);
}

#[test]
fn resource_close_is_exact_once_through_scope_close() {
    let mut scope = ExecutionScope::new().expect("scope");
    let (res, closes) = Counted::new();
    let token = scope.push_resource(res).expect("push");
    scope
        .begin_close(ResourceCloseReason::VmReset)
        .expect("close");
    match scope.poll_close(&mut cx()) {
        Poll::Ready(Ok(ScopeCloseOutcome::Success)) => {}
        other => panic!("expected clean quiescence, got {other:?}"),
    }
    assert_eq!(closes.load(Ordering::SeqCst), 1);
    // The handle is now closed: closing it again is rejected with the precise
    // closed-state error (distinct from a stale handle after slot reuse).
    let error = scope
        .close_resource::<Counted>(token.handle(), ResourceCloseReason::Requested)
        .expect_err("closed handle rejected");
    assert!(matches!(
        error,
        ExecutionScopeError::Resource(ref resource_error)
            if resource_error.code() == ResourceErrorCode::ResourceAlreadyClosed
    ));
}

#[test]
fn handle_from_other_scope_is_rejected_cross_vm() {
    let mut scope_a = ExecutionScope::new().expect("scope a");
    let mut scope_b = ExecutionScope::new().expect("scope b");
    let (res, _) = Counted::new();
    let token = scope_a.push_resource(res).expect("push into a");
    let foreign = token.handle();
    let error = scope_b
        .close_resource::<Counted>(foreign, ResourceCloseReason::Requested)
        .expect_err("foreign handle must be rejected");
    match error {
        ExecutionScopeError::Resource(resource_error) => {
            assert_eq!(
                resource_error.code(),
                ResourceErrorCode::ResourceHandleWrongTable
            );
        }
        other => panic!("expected resource wrong-table error, got {other:?}"),
    }
}

// ------------------------------------------------------------------ operations

#[test]
fn operation_direct_cancellation_is_typed_and_once() {
    let mut scope = ExecutionScope::new().expect("scope");
    let cancels = Arc::new(Mutex::new(Vec::new()));
    let id = scope
        .start_operation(OperationSpec::new(PendingDriver {
            release: Arc::new(Mutex::new(false)),
            cancels: Arc::clone(&cancels),
        }))
        .expect("start");

    assert!(
        scope
            .cancel_operation(id, OperationCancelReason::Requested)
            .expect("cancel must succeed")
    );
    assert_eq!(
        cancels.lock().unwrap()[..],
        [OperationCancelReason::Requested]
    );
    // Second cancel on the now-terminal operation is a no-op (false).
    assert!(
        !scope
            .cancel_operation(id, OperationCancelReason::Requested)
            .expect("terminal cancel returns false")
    );
    assert_eq!(cancels.lock().unwrap().len(), 1);
}

#[test]
fn abort_releases_slot_and_stales_id() {
    let mut scope = ExecutionScope::new().expect("scope");
    let id = scope
        .start_operation(OperationSpec::new(DoneDriver))
        .expect("start");
    assert!(
        scope
            .abort_operation(id, OperationCancelReason::VmReset)
            .expect("abort")
    );
    assert_eq!(
        scope.operations().status(id).expect_err("stale").code(),
        OperationErrorCode::OperationStale
    );
}

#[test]
fn take_outcome_delivers_terminal_exactly_once() {
    let mut scope = ExecutionScope::new().expect("scope");
    // Pending operation has no terminal outcome yet.
    let id = scope
        .start_operation(OperationSpec::new(DoneDriver))
        .expect("start");
    assert_eq!(
        scope
            .take_operation_outcome(id)
            .expect_err("pending has no outcome")
            .into_operation_error()
            .expect("pending maps to an operation error")
            .code(),
        OperationErrorCode::OperationPending
    );
    // Complete out-of-band then consume exactly once.
    assert!(scope.complete_operation(id).expect("complete"));
    assert_eq!(
        scope.take_operation_outcome(id).expect("terminal outcome"),
        OperationOutcome::Completed
    );
    // Consumed: id is stale now.
    assert_eq!(
        scope
            .operations()
            .status(id)
            .expect_err("stale after take")
            .code(),
        OperationErrorCode::OperationStale
    );
}

#[test]
fn bounded_admission_rejects_over_capacity() {
    let mut registry = OperationRegistry::with_limit(2).expect("registry");
    let _a = registry
        .start(OperationSpec::new(DoneDriver))
        .expect("first");
    let _b = registry
        .start(OperationSpec::new(DoneDriver))
        .expect("second");
    let error = registry
        .start(OperationSpec::new(DoneDriver))
        .expect_err("capacity reached");
    assert_eq!(error.code(), OperationErrorCode::OperationLimitExceeded);
    // Consuming a terminal restores capacity.
    let _ = registry.poll(_a, &mut cx());
    let _c = registry
        .start(OperationSpec::new(DoneDriver))
        .expect("capacity restored");
    assert_eq!(registry.active_count(), 2);
}

#[test]
fn resource_bounded_admission_rejects_over_capacity() {
    let mut table = ResourceTable::with_limit(2).expect("table");
    let (a, _) = Counted::new();
    let (b, _) = Counted::new();
    let a_token = table.push(a).expect("first");
    let b_token = table.push(b).expect("second");
    let (c, _) = Counted::new();
    let error = table.push(c).expect_err("capacity reached");
    assert_eq!(error.code(), ResourceErrorCode::ResourceLimitExceeded);

    // Closing restores capacity: both slots return to the vacant pool.
    let _ = table.begin_close(a_token, ResourceCloseReason::Requested);
    let _ = table.begin_close(b_token, ResourceCloseReason::Requested);
    assert_eq!(table.len(), 0);

    // Reuse stays bounded: many close/re-push cycles never exceed the
    // configured capacity (the same physical slots are recycled).
    for _ in 0..4 {
        let (res, _) = Counted::new();
        let token = table.push(res).expect("re-push after close");
        let _ = table.begin_close(token, ResourceCloseReason::Requested);
    }
    assert!(table.len() <= 2);
}
