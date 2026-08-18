//! Focused tests for C2-A guest resource ownership.
//!
//! These exercise the guest-ownership layer of the host-agnostic
//! [`ResourceTable`] and the derived [`Program::owned_local_slots`]
//! projection:
//!
//! - per-slot `contains_resource` projection cached on `Program` (non-wire),
//! - `mark_guest_owned` / `release_guest_owner` / `take_owned` validation and
//!   atomicity (failures consume nothing),
//! - exactly-once close on release, idempotent no-op releases,
//! - fallback `close_all` behavior for unreleased guest-owned resources and
//!   no double-close for released or taken ones,
//! - foreign-table isolation and first-reason-wins scope close.
//!
//! Only fake [`HostResource`] types with close counters are used — no
//! concrete VM domain/resource is involved.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use vm::compiler::TypeSchema;
use vm::execution_scope::{ExecutionScope, ExecutionScopeError, ScopeCloseOutcome};
use vm::resource::{
    CloseProgress, GuestReleaseOutcome, HostResource, OwnershipRelease, ResourceCloseReason,
    ResourceErrorCode, ResourceOwnership, ResourceResult, ResourceTable,
};
use vm::{Program, ResourceHandle, ResourceTypeKey, TypeMap, ValueType};

// ---- test resource types -------------------------------------------------------------

/// Synchronous-close resource counting `begin_close` calls and drops, and
/// recording every close reason it observed.
#[derive(Debug)]
struct CountingResource {
    begins: Arc<AtomicUsize>,
    reasons: Arc<Mutex<Vec<ResourceCloseReason>>>,
    drops: Arc<AtomicUsize>,
}

impl CountingResource {
    fn new() -> (
        Self,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<ResourceCloseReason>>>,
        Arc<AtomicUsize>,
    ) {
        let begins = Arc::new(AtomicUsize::new(0));
        let reasons = Arc::new(Mutex::new(Vec::new()));
        let drops = Arc::new(AtomicUsize::new(0));
        (
            Self {
                begins: begins.clone(),
                reasons: reasons.clone(),
                drops: drops.clone(),
            },
            begins,
            reasons,
            drops,
        )
    }
}

impl HostResource for CountingResource {
    fn begin_close(&mut self, reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.begins.fetch_add(1, Ordering::SeqCst);
        self.reasons.lock().unwrap().push(reason);
        Ok(CloseProgress::Ready)
    }
}

impl Drop for CountingResource {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// A resource whose close stays `Pending` until its shared gate is released.
#[derive(Debug)]
struct GatedResource {
    begins: Arc<AtomicUsize>,
    reasons: Arc<Mutex<Vec<ResourceCloseReason>>>,
    polls: Arc<AtomicUsize>,
    gate: Arc<AtomicBool>,
}

impl GatedResource {
    fn new() -> (
        Self,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<ResourceCloseReason>>>,
        Arc<AtomicUsize>,
        Arc<AtomicBool>,
    ) {
        let begins = Arc::new(AtomicUsize::new(0));
        let reasons = Arc::new(Mutex::new(Vec::new()));
        let polls = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(AtomicBool::new(false));
        (
            Self {
                begins: begins.clone(),
                reasons: reasons.clone(),
                polls: polls.clone(),
                gate: gate.clone(),
            },
            begins,
            reasons,
            polls,
            gate,
        )
    }
}

impl HostResource for GatedResource {
    fn begin_close(&mut self, reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.begins.fetch_add(1, Ordering::SeqCst);
        self.reasons.lock().unwrap().push(reason);
        Ok(CloseProgress::Pending)
    }

    fn poll_close(&mut self, _cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        if self.gate.load(Ordering::SeqCst) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

/// A distinct, inert type used to exercise typed-take mismatch rejection.
#[derive(Debug)]
struct WrongType;

impl HostResource for WrongType {}

// ---- helpers -------------------------------------------------------------------------

fn noop_context() -> Context<'static> {
    Context::from_waker(Waker::noop())
}

// ---- 1. Program owned_local_slots projection -----------------------------------------

#[test]
fn program_owned_local_slots_marks_direct_nested_and_plain_slots() {
    let key = ResourceTypeKey::new("io.file").expect("valid resource key");
    let direct = TypeSchema::Resource(key.clone());
    // A resource nested inside an optional array still makes the slot owned:
    // the projection uses the recursive `contains_resource` walk.
    let nested = TypeSchema::Optional(Box::new(TypeSchema::Array(Box::new(TypeSchema::Resource(
        key,
    )))));
    let program = Program::new(Vec::new(), Vec::new()).with_type_map(TypeMap {
        strict_types: false,
        local_types: vec![ValueType::Unknown; 4],
        local_schemas: vec![Some(direct), Some(nested), Some(TypeSchema::Int), None],
        callable_slots: vec![false; 4],
        optional_slots: vec![false; 4],
        operand_types: HashMap::new(),
    });

    assert_eq!(
        program.owned_local_slots(),
        &[true, true, false, false],
        "direct resource slot set, nested resource slot set, plain slots clear"
    );

    // The projection is a derived, lazily-computed cache: a clone observes the
    // same view (shared, never re-serialized into the wire type_map).
    let clone = program.clone();
    assert_eq!(clone.owned_local_slots(), &[true, true, false, false]);

    // A program without a type map owns no local slots.
    let bare = Program::new(Vec::new(), Vec::new());
    assert!(bare.owned_local_slots().is_empty());
}

// ---- 2. duplicate mark is a structured, atomic error ----------------------------------

#[test]
fn duplicate_mark_guest_owned_is_structured_error_and_atomic() {
    let mut table = ResourceTable::new();
    let (res, begins, _reasons, _drops) = CountingResource::new();
    let token = table.push(res).expect("push");
    let handle = token.handle();

    // A fresh allocation defaults to HostOwned: nothing is guest-owned
    // implicitly.
    assert_eq!(table.ownership(handle), Some(ResourceOwnership::HostOwned));
    table.mark_guest_owned(handle).expect("first mark succeeds");
    assert_eq!(table.ownership(handle), Some(ResourceOwnership::GuestOwned));

    let error = table.mark_guest_owned(handle).expect_err("duplicate mark");
    assert_eq!(error.code(), ResourceErrorCode::ResourceNotHostOwned);

    // Atomic: ownership and lifecycle are unchanged, no close fired.
    assert_eq!(table.ownership(handle), Some(ResourceOwnership::GuestOwned));
    table.get(&token).expect("still open");
    assert_eq!(begins.load(Ordering::SeqCst), 0);
}

// ---- 3. release with a synchronous close fires exactly once ---------------------------

#[test]
fn release_guest_owner_sync_close_fires_exactly_once() {
    let mut table = ResourceTable::new();
    let (res, begins, reasons, _drops) = CountingResource::new();
    let token = table.push(res).expect("push");
    let handle = token.handle();
    table.mark_guest_owned(handle).expect("mark");

    let outcome = table
        .release_guest_owner(handle, OwnershipRelease::close())
        .expect("release");
    assert_eq!(outcome, GuestReleaseOutcome::Released(CloseProgress::Ready));
    assert_eq!(begins.load(Ordering::SeqCst), 1);
    assert_eq!(
        *reasons.lock().unwrap(),
        vec![ResourceCloseReason::OwnershipRelease]
    );
    assert!(table.is_empty());

    // A repeated release on the now-stale handle is an idempotent no-op and
    // never re-fires the close.
    let again = table
        .release_guest_owner(handle, OwnershipRelease::close())
        .expect("repeat release is not an error");
    assert_eq!(again, GuestReleaseOutcome::NotGuestOwned);
    assert_eq!(begins.load(Ordering::SeqCst), 1);
}

// ---- 4. release with a pending close fires begin_close exactly once -------------------

#[test]
fn release_guest_owner_pending_fires_begin_close_exactly_once() {
    let mut table = ResourceTable::new();
    let (res, begins, _reasons, polls, gate) = GatedResource::new();
    let token = table.push(res).expect("push");
    let handle = token.handle();
    table.mark_guest_owned(handle).expect("mark");

    let outcome = table
        .release_guest_owner(handle, OwnershipRelease::close())
        .expect("release");
    assert_eq!(
        outcome,
        GuestReleaseOutcome::Released(CloseProgress::Pending)
    );
    assert_eq!(begins.load(Ordering::SeqCst), 1);

    // Repeated releases while the close is pending are idempotent no-ops:
    // no error, and begin_close is never re-fired.
    for _ in 0..3 {
        let outcome = table
            .release_guest_owner(handle, OwnershipRelease::close())
            .expect("repeat release is not an error");
        assert_eq!(outcome, GuestReleaseOutcome::NotGuestOwned);
    }
    assert_eq!(begins.load(Ordering::SeqCst), 1);

    // A close-all sweep treats the still-pending resource as pending and does
    // NOT re-begin_close it; it only drives the poll to completion.
    gate.store(true, Ordering::SeqCst);
    let closed = table
        .close_all(ResourceCloseReason::VmReset)
        .expect("close_all finishes the pending close");
    assert_eq!(closed, 1);
    assert_eq!(begins.load(Ordering::SeqCst), 1);
    assert!(polls.load(Ordering::SeqCst) >= 1);
    assert!(table.is_empty());
}

// ---- 5. close_all: fallback close once for unreleased GuestOwned; no re-fire ----------

#[test]
fn close_all_closes_pending_guest_owned_once_and_never_refires_a_released_one() {
    let mut table = ResourceTable::new();
    // Resource A: GuestOwned, never released; close_all is its fallback close.
    let (res_a, begins_a, reasons_a, _polls_a, gate_a) = GatedResource::new();
    // Resource B: GuestOwned and already released (Closing) before close_all.
    let (res_b, begins_b, reasons_b, _polls_b, gate_b) = GatedResource::new();

    let token_a = table.push(res_a).expect("push a");
    let token_b = table.push(res_b).expect("push b");
    table.mark_guest_owned(token_a.handle()).expect("mark a");
    table.mark_guest_owned(token_b.handle()).expect("mark b");

    // Release B first: begin_close fires exactly once with the release reason.
    let outcome = table
        .release_guest_owner(token_b.handle(), OwnershipRelease::close())
        .expect("release b");
    assert_eq!(
        outcome,
        GuestReleaseOutcome::Released(CloseProgress::Pending)
    );
    assert_eq!(begins_b.load(Ordering::SeqCst), 1);
    assert_eq!(
        *reasons_b.lock().unwrap(),
        vec![ResourceCloseReason::OwnershipRelease]
    );

    // A's gate is open so its fallback close completes synchronously; B's gate
    // stays shut so the first sweep leaves it pending.
    gate_a.store(true, Ordering::SeqCst);
    let first = table.close_all(ResourceCloseReason::VmReset);
    assert_eq!(
        first.expect_err("b still pending").code(),
        ResourceErrorCode::ResourceClosePending
    );
    // The unreleased GuestOwned resource was closed by the fallback exactly once.
    assert_eq!(begins_a.load(Ordering::SeqCst), 1);
    assert_eq!(
        *reasons_a.lock().unwrap(),
        vec![ResourceCloseReason::VmReset]
    );
    // The released, already-closing resource was NOT re-begun by the sweep.
    assert_eq!(begins_b.load(Ordering::SeqCst), 1);

    // Releasing B's gate lets the sweep finish without any double close.
    gate_b.store(true, Ordering::SeqCst);
    let closed = table
        .close_all(ResourceCloseReason::VmReset)
        .expect("second close_all completes");
    assert_eq!(closed, 2);
    assert_eq!(begins_a.load(Ordering::SeqCst), 1);
    assert_eq!(begins_b.load(Ordering::SeqCst), 1);
    assert!(table.is_empty());
}

// ---- 6. typed take_owned success ------------------------------------------------------

#[test]
fn take_owned_returns_the_value_and_the_handle_is_stale_afterwards() {
    let mut table = ResourceTable::new();
    let (res, begins, _reasons, drops) = CountingResource::new();
    let token = table.push(res).expect("push");
    let handle = token.handle();
    table.mark_guest_owned(handle).expect("mark");

    let owned = table
        .take_owned::<CountingResource>(handle)
        .expect("take succeeds");

    // The slot is retired as Taken and the table no longer tracks the value.
    assert_eq!(table.ownership(handle), Some(ResourceOwnership::Taken));
    assert!(table.is_empty());
    // The raw handle is stale: every validated use now fails structurally.
    assert_eq!(
        table.mark_guest_owned(handle).unwrap_err().code(),
        ResourceErrorCode::ResourceAlreadyTaken
    );
    assert_eq!(
        table
            .take_owned::<CountingResource>(handle)
            .unwrap_err()
            .code(),
        ResourceErrorCode::ResourceAlreadyTaken
    );
    assert_eq!(
        table.typed::<CountingResource>(handle).unwrap_err().code(),
        ResourceErrorCode::ResourceAlreadyClosed
    );
    // The moved-out value was never closed by the table...
    assert_eq!(begins.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    // ...and its ownership really transferred: dropping the owned value drops
    // the resource.
    drop(owned);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

// ---- 7. take_owned with the wrong type consumes nothing --------------------------------

#[test]
fn take_owned_wrong_type_is_an_error_and_consumes_nothing() {
    let mut table = ResourceTable::new();
    let (res, begins, _reasons, _drops) = CountingResource::new();
    let token = table.push(res).expect("push");
    let handle = token.handle();
    table.mark_guest_owned(handle).expect("mark");

    let error = table.take_owned::<WrongType>(handle).unwrap_err();
    assert_eq!(error.code(), ResourceErrorCode::ResourceTypeMismatch);

    // Not consumed: still open, still guest-owned, never closed.
    assert_eq!(table.ownership(handle), Some(ResourceOwnership::GuestOwned));
    table.get(&token).expect("still open");
    assert_eq!(table.len(), 1);
    assert_eq!(begins.load(Ordering::SeqCst), 0);
}

// ---- 8. take_owned with the wrong key consumes nothing ---------------------------------

#[test]
fn take_owned_wrong_key_is_an_error_and_consumes_nothing() {
    let mut table = ResourceTable::new();
    let (res, begins, _reasons, _drops) = CountingResource::new();
    let token = table.push(res).expect("push");
    let handle = token.handle();
    table.mark_guest_owned(handle).expect("mark");

    // Same table and slot key shape, but a generation no live resource has:
    // the lowest handle bits carry the generation, so flipping bit 1 yields a
    // well-formed token that names nothing live.
    let wrong_key = ResourceHandle::from_raw(handle.raw() ^ 2).expect("valid encoding");
    let error = table.take_owned::<CountingResource>(wrong_key).unwrap_err();
    assert_eq!(error.code(), ResourceErrorCode::ResourceStale);

    // Not consumed.
    assert_eq!(table.ownership(handle), Some(ResourceOwnership::GuestOwned));
    table.get(&token).expect("still open");
    assert_eq!(begins.load(Ordering::SeqCst), 0);
}

// ---- 9. take_owned with live children consumes nothing ---------------------------------

#[test]
fn take_owned_with_live_children_is_an_error_and_consumes_nothing() {
    let mut table = ResourceTable::new();
    let (parent_res, parent_begins, _parent_reasons, _parent_drops) = CountingResource::new();
    let parent = table.push(parent_res).expect("push parent");
    let (child_res, _child_begins, _child_reasons, _child_drops) = CountingResource::new();
    let child = table.push_child(child_res, &parent).expect("push child");
    table
        .mark_guest_owned(parent.handle())
        .expect("mark parent");

    // A parent cannot be taken before its children.
    let error = table
        .take_owned::<CountingResource>(parent.handle())
        .unwrap_err();
    assert_eq!(error.code(), ResourceErrorCode::ResourceHasChildren);

    // Not consumed: parent still open and guest-owned, nothing closed.
    assert_eq!(
        table.ownership(parent.handle()),
        Some(ResourceOwnership::GuestOwned)
    );
    table.get(&parent).expect("parent still open");
    assert_eq!(parent_begins.load(Ordering::SeqCst), 0);

    // Once the child is closed the take succeeds: the blocker was the live
    // child, nothing else.
    assert_eq!(
        table
            .begin_close(child, ResourceCloseReason::Requested)
            .expect("close child"),
        CloseProgress::Ready
    );
    let owned = table
        .take_owned::<CountingResource>(parent.handle())
        .expect("take after child closed");
    assert_eq!(
        table.ownership(parent.handle()),
        Some(ResourceOwnership::Taken)
    );
    drop(owned);
}

// ---- 10. take_owned with a foreign table handle consumes nothing -----------------------

#[test]
fn take_owned_foreign_table_handle_is_an_error_and_consumes_nothing() {
    let mut table = ResourceTable::new();
    let mut foreign = ResourceTable::new();
    let (res, begins, _reasons, _drops) = CountingResource::new();
    let token = table.push(res).expect("push");
    let handle = token.handle();
    table.mark_guest_owned(handle).expect("mark");

    let (foreign_res, _fb, _fr, _fd) = CountingResource::new();
    let foreign_token = foreign.push(foreign_res).expect("push foreign");

    let error = table
        .take_owned::<CountingResource>(foreign_token.handle())
        .unwrap_err();
    assert_eq!(error.code(), ResourceErrorCode::ResourceHandleWrongTable);

    // Isolation: the local resource was not consumed...
    assert_eq!(table.ownership(handle), Some(ResourceOwnership::GuestOwned));
    table.get(&token).expect("still open");
    assert_eq!(begins.load(Ordering::SeqCst), 0);
    // ...and the foreign table is equally untouched.
    assert_eq!(foreign.len(), 1);
    assert_eq!(
        foreign.ownership(foreign_token.handle()),
        Some(ResourceOwnership::HostOwned)
    );
}

// ---- 11. foreign / stale release is an idempotent no-op --------------------------------

#[test]
fn release_with_foreign_or_stale_handle_is_an_idempotent_noop() {
    let mut table = ResourceTable::new();
    let mut foreign = ResourceTable::new();
    let (res, begins, _reasons, _drops) = CountingResource::new();
    let token = table.push(res).expect("push");
    let handle = token.handle();
    table.mark_guest_owned(handle).expect("mark");

    let (foreign_res, foreign_begins, _fr, _fd) = CountingResource::new();
    let foreign_token = foreign.push(foreign_res).expect("push foreign");

    // Foreign handle: no-op, and no close is fired in either table.
    let outcome = table
        .release_guest_owner(foreign_token.handle(), OwnershipRelease::close())
        .expect("foreign release is not an error");
    assert_eq!(outcome, GuestReleaseOutcome::NotGuestOwned);
    assert_eq!(begins.load(Ordering::SeqCst), 0);
    assert_eq!(foreign_begins.load(Ordering::SeqCst), 0);
    assert_eq!(table.len(), 1);
    assert_eq!(foreign.len(), 1);

    // Stale handle (after the real release completed): no-op, and the close
    // stays fired exactly once.
    let outcome = table
        .release_guest_owner(handle, OwnershipRelease::close())
        .expect("release");
    assert_eq!(outcome, GuestReleaseOutcome::Released(CloseProgress::Ready));
    assert_eq!(begins.load(Ordering::SeqCst), 1);
    let outcome = table
        .release_guest_owner(handle, OwnershipRelease::close())
        .expect("stale release is not an error");
    assert_eq!(outcome, GuestReleaseOutcome::NotGuestOwned);
    assert_eq!(begins.load(Ordering::SeqCst), 1);
}

// ---- 12. mark on a Closing or Taken resource is a structured, atomic error -------------

#[test]
fn mark_on_closing_or_taken_is_a_structured_error_and_atomic() {
    // Closing resource (release launched, close still pending).
    let mut table = ResourceTable::new();
    let (res, begins, _reasons, _polls, _gate) = GatedResource::new();
    let token = table.push(res).expect("push");
    let handle = token.handle();
    table.mark_guest_owned(handle).expect("mark");
    let outcome = table
        .release_guest_owner(handle, OwnershipRelease::close())
        .expect("release");
    assert_eq!(
        outcome,
        GuestReleaseOutcome::Released(CloseProgress::Pending)
    );

    let error = table.mark_guest_owned(handle).unwrap_err();
    assert_eq!(error.code(), ResourceErrorCode::ResourceAlreadyClosed);
    // Atomic: the release close fired exactly once, ownership unchanged.
    assert_eq!(begins.load(Ordering::SeqCst), 1);
    assert_eq!(table.ownership(handle), Some(ResourceOwnership::GuestOwned));

    // Taken resource (concrete value moved out of the table).
    let mut table = ResourceTable::new();
    let (res, _begins, _reasons, _drops) = CountingResource::new();
    let token = table.push(res).expect("push");
    let handle = token.handle();
    table.mark_guest_owned(handle).expect("mark");
    let owned = table.take_owned::<CountingResource>(handle).expect("take");

    let error = table.mark_guest_owned(handle).unwrap_err();
    assert_eq!(error.code(), ResourceErrorCode::ResourceAlreadyTaken);
    // Atomic: still Taken, never remapped to GuestOwned.
    assert_eq!(table.ownership(handle), Some(ResourceOwnership::Taken));
    drop(owned);
}

// ---- 13. scope first-reason-wins is preserved -------------------------------------------

#[test]
fn scope_begin_close_first_reason_wins_is_preserved() {
    let mut scope = ExecutionScope::new();
    assert!(
        scope
            .begin_close(ResourceCloseReason::OwnershipRelease)
            .expect("first close")
    );
    // Repeating the bound reason is an idempotent no-op.
    assert!(
        !scope
            .begin_close(ResourceCloseReason::OwnershipRelease)
            .expect("repeat with the bound reason")
    );
    // A conflicting reason is rejected; the first reason stays bound.
    let error = scope
        .begin_close(ResourceCloseReason::Deadline)
        .unwrap_err();
    match error {
        ExecutionScopeError::CloseAlreadyInProgress { current, requested } => {
            assert_eq!(current, Some(ResourceCloseReason::OwnershipRelease));
            assert_eq!(requested, ResourceCloseReason::Deadline);
        }
        other => panic!("expected CloseAlreadyInProgress, got {other:?}"),
    }
    assert_eq!(
        scope.close_reason(),
        Some(ResourceCloseReason::OwnershipRelease)
    );

    // The new reason also drives the close pipeline (operation-reason adapter)
    // to a clean quiescence on an empty scope.
    let mut cx = noop_context();
    match scope.poll_close(&mut cx) {
        Poll::Ready(Ok(outcome)) => assert_eq!(outcome, ScopeCloseOutcome::Success),
        other => panic!("expected a clean terminal outcome, got {other:?}"),
    }
    assert!(scope.is_quiescent());
}

// ---- 14. close_all reclaims HostOwned + GuestOwned once; Taken never re-closed ----------

#[test]
fn close_all_reclaims_host_and_guest_owned_once_and_never_touches_taken() {
    let mut table = ResourceTable::new();
    let (host_res, host_begins, _host_reasons, _host_drops) = CountingResource::new();
    let (guest_res, guest_begins, _guest_reasons, _guest_drops) = CountingResource::new();
    let (taken_res, taken_begins, _taken_reasons, taken_drops) = CountingResource::new();

    // HostOwned by default: nothing marks this resource guest-owned.
    let _host = table.push(host_res).expect("push host");
    // GuestOwned but never released: close_all is its fallback close.
    let guest = table.push(guest_res).expect("push guest");
    table.mark_guest_owned(guest.handle()).expect("mark guest");
    // GuestOwned and then taken: the value moved out before the sweep.
    let taken = table.push(taken_res).expect("push taken");
    table.mark_guest_owned(taken.handle()).expect("mark taken");
    let owned = table
        .take_owned::<CountingResource>(taken.handle())
        .expect("take");

    let closed = table
        .close_all(ResourceCloseReason::VmReset)
        .expect("close_all");
    assert_eq!(closed, 2);
    // HostOwned and GuestOwned unreleased resources each closed exactly once.
    assert_eq!(host_begins.load(Ordering::SeqCst), 1);
    assert_eq!(guest_begins.load(Ordering::SeqCst), 1);
    // The Taken resource was moved out earlier: the sweep neither closes nor
    // drops it (no double close is possible).
    assert_eq!(taken_begins.load(Ordering::SeqCst), 0);
    assert_eq!(taken_drops.load(Ordering::SeqCst), 0);
    assert!(table.is_empty());

    // The taken value lives on as an ordinary owned value.
    drop(owned);
    assert_eq!(taken_drops.load(Ordering::SeqCst), 1);
}
