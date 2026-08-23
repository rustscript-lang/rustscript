//! Focused tests for wiring `HostContext` to a per-`HostRuntime` generic
//! `ExecutionScope`.
//!
//! These exercise the public generic host boundary through `Vm::host_context`:
//!
//! - every `HostRuntime`/`Vm` owns an **independent** scope created Active;
//! - `HostContext` inserts of resources and operation starts land in the *same
//!   scope*, and typed handles / operation ids are queryable back through the
//!   boundary;
//! - parent/child resources close child-first through the generic SDK;
//! - once [`HostContext::begin_close`] has been issued, every SDK *write*
//!   entry is rejected with a **structured** `ScopeClosing` error while reads
//!   still work;
//! - dispatch is strictly type-`Any` based — no domain resource class, no
//!   domain name, no feature coupling.
//!
//! Only fake generic [`HostResource`] / [`HostOperation`] types are used (no
//! sql/io/http/SSE/rusqlite, no concrete builtin).

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use vm::execution_scope::{ExecutionScopeError, ScopeCloseOutcome, ScopeState};
use vm::operation::{
    HostOperation, OperationCancelReason, OperationResult, OperationSpec, OperationStatus,
};
use vm::resource::{
    CloseProgress, HostResource, ResourceCloseReason, ResourceErrorCode, ResourceResult,
};
use vm::{HostContextErrorKind, Program, Vm};

// ---- fake generic resources ------------------------------------------------

/// A plain host resource carrying a readable value.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Counter {
    value: u64,
}

impl HostResource for Counter {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        Ok(CloseProgress::Ready)
    }
}

/// A second, unrelated generic resource type used to prove typed `TypeId`
/// dispatch (no domain class).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Named {
    name: &'static str,
}

impl HostResource for Named {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        Ok(CloseProgress::Ready)
    }
}

/// Records the order in which `begin_close` was invoked, for child-first
/// ordering assertions.
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

/// Typed per-VM module state (lives in the module store, outside the scope).
#[derive(Clone, Debug, PartialEq, Eq)]
struct CounterState {
    count: u32,
}

// ---- helpers ---------------------------------------------------------------

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn noop_waker() -> Waker {
    Waker::from(Arc::new(NoopWake))
}

/// Drives a `begin_close`d context to quiescence, returning the terminal
/// outcome.
fn drive_to_quiescence(cx: &mut vm::HostContext<'_>) -> ScopeCloseOutcome {
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    loop {
        match cx.poll_close(&mut context) {
            Poll::Pending => continue,
            Poll::Ready(Ok(outcome)) => return outcome,
            Poll::Ready(Err(error)) => panic!("poll_close failed: {error}"),
        }
    }
}

// ---- independent per-instance scopes ----------------------------------------

#[test]
fn every_host_context_owns_an_independent_active_scope() {
    let mut vm_a =
        Vm::try_new(Program::new(vec![], vec![])).expect("test VM construction must not fail");
    let mut vm_b =
        Vm::try_new(Program::new(vec![], vec![])).expect("test VM construction must not fail");

    let mut cx_a = vm_a.host_context();
    let cx_b = vm_b.host_context();

    // Both start Active before anything is pushed.
    assert_eq!(cx_a.scope_state(), ScopeState::Active);
    assert_eq!(cx_b.scope_state(), ScopeState::Active);
    assert!(cx_a.is_scope_active());
    assert!(cx_b.is_scope_active());
    assert!(cx_a.execution_scope().resources().is_empty());
    assert!(cx_b.execution_scope().resources().is_empty());
    assert!(cx_a.execution_scope().operations().is_empty());
    assert!(cx_b.execution_scope().operations().is_empty());

    // Inserting only into A must not leak into B's scope.
    let _token = cx_a
        .push_resource(Counter { value: 42 })
        .expect("push into A");
    assert_eq!(cx_a.resource_count(), 1);
    assert_eq!(cx_b.resource_count(), 0);
    assert!(cx_b.execution_scope().resources().is_empty());
}

// ---- same-scope landing and queries ----------------------------------------

#[test]
fn host_context_inserts_land_in_the_same_scope_and_are_queryable() {
    let mut vm =
        Vm::try_new(Program::new(vec![], vec![])).expect("test VM construction must not fail");
    let mut cx = vm.host_context();

    let token = cx
        .push_resource(Counter { value: 7 })
        .expect("push resource");
    assert_eq!(cx.resource_count(), 1);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let id = cx
        .start_operation(
            OperationSpec::new(TrackedOperation)
                .with_deadline(deadline)
                .with_resource(token.handle()),
        )
        .expect("start operation");

    // Typed read query resolves the pushed resource through the boundary.
    let borrow = cx.resource(&token).expect("typed get");
    assert_eq!(borrow.value, 7);

    // Operation metadata carried by the spec is observable on the same scope.
    assert_eq!(cx.operation_count(), 1);
    assert_eq!(
        cx.operation_status(id).expect("status"),
        OperationStatus::Pending
    );
    assert_eq!(
        cx.execution_scope()
            .operations()
            .operations_for_resource(token.handle()),
        vec![id]
    );

    // Typed recovery from the raw handle is validated and domain-free.
    let recovered = cx
        .typed_resource::<Counter>(token.handle())
        .expect("recover typed");
    assert_eq!(recovered, token);
}

// ---- parent/child ----------------------------------------------------------

#[test]
fn parent_and_child_resources_close_child_first_through_the_sdk() {
    let mut vm =
        Vm::try_new(Program::new(vec![], vec![])).expect("test VM construction must not fail");
    let mut cx = vm.host_context();

    let order = Arc::new(Mutex::new(Vec::new()));
    let parent = cx
        .push_resource(CloseRecorder {
            order: order.clone(),
            name: "parent",
        })
        .expect("push parent");
    for name in ["child1", "child2"] {
        cx.push_child_resource(
            CloseRecorder {
                order: order.clone(),
                name,
            },
            &parent,
        )
        .expect("push child");
    }
    assert_eq!(cx.execution_scope().resources().len(), 3);

    assert!(
        cx.begin_close(ResourceCloseReason::Requested)
            .expect("begin close")
    );
    assert_eq!(drive_to_quiescence(&mut cx), ScopeCloseOutcome::Success);
    assert_eq!(cx.execution_scope().state(), ScopeState::Quiescent);
    assert_eq!(cx.execution_scope().resources().len(), 0);

    let recorded = order.lock().unwrap().clone();
    assert_eq!(recorded.len(), 3, "every resource began closing");
    let parent_at = recorded
        .iter()
        .position(|n| *n == "parent")
        .expect("parent");
    let child1_at = recorded
        .iter()
        .position(|n| *n == "child1")
        .expect("child1");
    let child2_at = recorded
        .iter()
        .position(|n| *n == "child2")
        .expect("child2");
    assert!(
        child1_at < parent_at && child2_at < parent_at,
        "children must begin closing before their parent: {recorded:?}"
    );
}

// ---- closing rejects writes with structured ScopeClosing -------------------

#[test]
fn closing_scope_rejects_all_sdk_writes_with_structured_scope_closing() {
    let mut vm =
        Vm::try_new(Program::new(vec![], vec![])).expect("test VM construction must not fail");
    let mut cx = vm.host_context();

    // A parent kept for the (rejected) child push.
    let parent = cx.push_resource(Counter { value: 0 }).expect("push parent");
    assert!(
        cx.begin_close(ResourceCloseReason::Requested)
            .expect("begin close")
    );
    assert_eq!(cx.scope_state(), ScopeState::Closing);

    // Every write entry is rejected with the structured ScopeClosing error.
    let error = cx
        .push_resource(Counter { value: 1 })
        .expect_err("push rejected while closing");
    assert!(matches!(
        error.kind(),
        HostContextErrorKind::Scope(ExecutionScopeError::ScopeClosing)
    ));

    let error = cx
        .push_child_resource(Counter { value: 2 }, &parent)
        .expect_err("push child rejected while closing");
    assert!(matches!(
        error.kind(),
        HostContextErrorKind::Scope(ExecutionScopeError::ScopeClosing)
    ));

    let error = cx
        .start_operation(OperationSpec::new(TrackedOperation))
        .expect_err("start operation rejected while closing");
    assert!(matches!(
        error.kind(),
        HostContextErrorKind::Scope(ExecutionScopeError::ScopeClosing)
    ));

    // Read-only queries still resolve while the scope is closing.
    assert!(cx.resource(&parent).is_ok());
    assert!(!cx.is_scope_active());
    assert!(!cx.is_scope_quiescent());
    assert_eq!(cx.resource_count(), 1);
}

// ---- module state outlives the scope ----------------------------------------

#[test]
fn module_state_survives_execution_scope_close() {
    let mut vm =
        Vm::try_new(Program::new(vec![], vec![])).expect("test VM construction must not fail");
    let mut cx = vm.host_context();

    assert!(!cx.set_module_state(CounterState { count: 9 }));
    let _token = cx
        .push_resource(Counter { value: 1 })
        .expect("push resource");

    assert!(
        cx.begin_close(ResourceCloseReason::VmReset)
            .expect("begin close")
    );
    assert_eq!(drive_to_quiescence(&mut cx), ScopeCloseOutcome::Success);
    assert_eq!(cx.scope_state(), ScopeState::Quiescent);
    assert!(cx.is_scope_quiescent());
    assert_eq!(cx.resource_count(), 0);

    // Closing the scope must never clear the module store.
    assert_eq!(cx.module_state::<CounterState>().unwrap().count, 9);
}

// ---- strict typed dispatch, no domain class --------------------------------

#[test]
fn typed_recovery_is_type_checked_and_domain_free() {
    let mut vm =
        Vm::try_new(Program::new(vec![], vec![])).expect("test VM construction must not fail");
    let mut cx = vm.host_context();

    let token = cx
        .push_resource(Counter { value: 1 })
        .expect("push counter");

    // Asking for the unrelated generic type is rejected; the original stays
    // open and usable.
    match cx.typed_resource::<Named>(token.handle()) {
        Ok(_) => panic!("wrong type must not recover"),
        Err(error) => match error.kind() {
            HostContextErrorKind::Resource(inner) => {
                assert_eq!(
                    inner.code(),
                    ResourceErrorCode::ResourceTypeMismatch,
                    "type mismatch must be preserved structurally"
                );
            }
            other => panic!("expected a resource-layer error, got {other:?}"),
        },
    }

    // The original resource is untouched by the rejected recovery.
    let borrow = cx.resource(&token).expect("original still accessible");
    assert_eq!(borrow.value, 1);
}
