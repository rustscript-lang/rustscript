//! Focused tests for the host-agnostic typed generational `ResourceTable`.
//!
//! These exercise the generic resource layer in isolation: handle encoding,
//! arena/scope identity, slot generation, typed access, type erasure,
//! validated recovery, parent/child links, stale-handle rejection, child-first
//! close, the poll-based close-all contract, and the close state/progress
//! errors. No concrete VM builtin resource is involved.
//!
//! Public host recovery from a raw handle always goes through the validated
//! [`ResourceTable::typed`]; the unchecked `Resource::from_handle` constructor
//! is crate-private and exercised only from unit tests inside the crate.

use vm::resource::{
    CloseProgress, HostResource, Resource, ResourceCloseReason, ResourceError, ResourceErrorCode,
    ResourceResult, ResourceTable,
};
use vm::{ResourceHandle, Value};

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

// ---- test resource types -------------------------------------------------------------

/// Trivial resource that counts synchronous closes.
#[derive(Default)]
struct CountingResource {
    closes: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    label: &'static str,
}

impl CountingResource {
    fn new(label: &'static str) -> (Self, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let closes = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        (
            Self {
                closes: closes.clone(),
                drops: drops.clone(),
                label,
            },
            closes,
            drops,
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

/// A resource that needs a second poll to finish closing.
struct TwoPollResource(pub Arc<AtomicUsize>);

impl TwoPollResource {
    fn new() -> (Self, Arc<AtomicUsize>) {
        let polls = Arc::new(AtomicUsize::new(0));
        (Self(polls.clone()), polls)
    }
}

impl HostResource for TwoPollResource {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        Ok(CloseProgress::Pending)
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            let _ = cx;
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

/// A distinct, inert type used to exercise type-mismatch rejection.
#[derive(Default)]
struct RecordMarker;

impl HostResource for RecordMarker {}

/// Reports cleanup failure through `poll_close`.
struct PollFailingResource;

impl HostResource for PollFailingResource {
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

/// Records its begin_close order for child-first traversal assertions.
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

/// Shared gate driving genuinely-async closes. While `released` is false a
/// poll stays Pending and remembers the caller's waker, so an external event
/// can wake/drive it; once released the close completes.
struct GateState {
    released: AtomicBool,
    wakes_registered: AtomicUsize,
    last_waker: Mutex<Option<Waker>>,
}

impl Default for GateState {
    fn default() -> Self {
        Self {
            released: AtomicBool::new(false),
            wakes_registered: AtomicUsize::new(0),
            last_waker: Mutex::new(None),
        }
    }
}

/// A resource that is genuinely `Pending` until a shared gate is released.
struct GateResource {
    state: Arc<GateState>,
}

impl GateResource {
    fn new() -> (Self, Arc<GateState>) {
        let state = Arc::new(GateState::default());
        (
            Self {
                state: state.clone(),
            },
            state,
        )
    }
}

impl HostResource for GateResource {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        Ok(CloseProgress::Pending)
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        if self.state.released.load(Ordering::SeqCst) {
            Poll::Ready(Ok(()))
        } else {
            self.state.wakes_registered.fetch_add(1, Ordering::SeqCst);
            *self.state.last_waker.lock().unwrap() = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// Child-first close order with an async (gated) child mixed in.
struct GateRecorder {
    state: Arc<GateState>,
    order: Arc<Mutex<Vec<&'static str>>>,
    name: &'static str,
}

impl HostResource for GateRecorder {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.order.lock().unwrap().push(self.name);
        Ok(CloseProgress::Pending)
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        if self.state.released.load(Ordering::SeqCst) {
            Poll::Ready(Ok(()))
        } else {
            *self.state.last_waker.lock().unwrap() = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

// ---- helpers ------------------------------------------------------------------------

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn noop_waker() -> Waker {
    Waker::from(Arc::new(NoopWake))
}

/// A waker that counts every wake call (for testing caller-waker progress).
struct LatchWake(Arc<AtomicUsize>);

impl Wake for LatchWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn tracking_waker() -> (Waker, Arc<AtomicUsize>) {
    let latch = Arc::new(AtomicUsize::new(0));
    (Waker::from(Arc::new(LatchWake(latch.clone()))), latch)
}

fn require_send<T: Send>() {}

/// Runs a close through begin + poll to completion when it is pending.
fn drive_close<T: HostResource>(
    table: &mut ResourceTable,
    token: Resource<T>,
    reason: ResourceCloseReason,
) {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    if table.begin_close(token, reason).expect("begin close") == CloseProgress::Pending {
        while table.poll_close(token, &mut cx) == Poll::Pending {
            // Keep polling with a no-op waker; resources in these tests finish.
        }
    }
}

// ---- tests --------------------------------------------------------------------------

#[test]
fn typed_push_get_and_mut_round_trip() {
    require_send::<ResourceTable>();
    let mut table = ResourceTable::new();

    let (res, closes, drops) = CountingResource::new("conn");
    let token = table.push(res).expect("push should succeed");

    let borrow = table.get(&token).expect("get");
    assert_eq!(
        borrow.label, "conn",
        "shared borrow sees the concrete value"
    );
    assert_eq!(closes.load(Ordering::SeqCst), 0);
    drop(borrow);

    {
        let mut mut_borrow = table.get_mut(&token).expect("get_mut");
        mut_borrow.label = "mutated";
    }
    let borrow = table.get(&token).expect("get after mutation");
    assert_eq!(borrow.label, "mutated");
    assert_eq!(
        drops.load(Ordering::SeqCst),
        0,
        "borrowing must not drop the resource"
    );
    drop(borrow);

    drop(table);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "dropping the table reclaims its resources"
    );
}

#[test]
fn handle_round_trips_through_value_and_recovers_through_typed() {
    let mut table = ResourceTable::new();
    let (res, _, _) = CountingResource::new("f");
    let token = table.push(res).expect("push");
    let raw = token.handle();

    let as_value = raw.as_value();
    let back = ResourceHandle::from_value(&as_value).expect("round trip");

    // Public recovery of a raw handle is validated by `typed`.
    let token2: Resource<CountingResource> =
        table.typed::<CountingResource>(back).expect("recover");
    let _ = table.get(&token2).expect("reclaimed handle works");

    // Zero and negative tokens are invalid encodings.
    assert_eq!(
        ResourceHandle::from_value(&Value::Int(0))
            .unwrap_err()
            .code(),
        ResourceErrorCode::InvalidResourceHandle
    );
    assert_eq!(
        ResourceHandle::from_value(&Value::Int(-1))
            .unwrap_err()
            .code(),
        ResourceErrorCode::InvalidResourceHandle
    );
}

#[test]
fn typed_recovery_rejects_wrong_type_and_leaves_state_unchanged() {
    let mut table = ResourceTable::new();
    let (res, closes, drops) = CountingResource::new("a");
    let token = table.push(res).expect("push");
    let handle = token.handle();

    // Wrong-type validated recovery is rejected with ResourceTypeMismatch.
    assert_eq!(
        table.typed::<RecordMarker>(handle).unwrap_err().code(),
        ResourceErrorCode::ResourceTypeMismatch
    );
    // The rejected recovery left the real resource fully untouched.
    assert_eq!(table.len(), 1);
    assert_eq!(closes.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    // The correct type still recovers and borrows.
    let recovered: Resource<CountingResource> = table
        .typed::<CountingResource>(handle)
        .expect("correct type recovers");
    assert_eq!(table.get(&recovered).unwrap().label, "a");
    assert_eq!(
        table.len(),
        1,
        "a successful typed recovery must not mutate the table either"
    );

    drive_close(&mut table, token, ResourceCloseReason::ResourceClosed);
    assert_eq!(closes.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn typed_recovery_rejects_foreign_arena_and_leaves_state_unchanged() {
    let mut table_a = ResourceTable::new();
    let token_a = table_a.push(CountingResource::new("a").0).expect("push");
    let handle_a = token_a.handle();

    let table_b = ResourceTable::new();
    assert_eq!(
        table_b
            .typed::<CountingResource>(handle_a)
            .unwrap_err()
            .code(),
        ResourceErrorCode::ResourceHandleWrongTable
    );
    // Neither table is mutated by a rejected foreign recovery.
    assert_eq!(table_a.len(), 1);
    assert!(table_b.is_empty());
}

#[test]
fn typed_recovery_rejects_stale_generation_after_slot_reuse() {
    let mut table = ResourceTable::with_limit(1).expect("table");
    let first = table.push(CountingResource::new("one").0).expect("push");
    let first_handle = first.handle();

    drive_close(&mut table, first, ResourceCloseReason::ResourceClosed);
    assert_eq!(table.len(), 0);

    let second = table.push(CountingResource::new("two").0).expect("reuse");
    assert_eq!(
        first_handle.slot_index().unwrap(),
        second.handle().slot_index().unwrap(),
        "slot is reused"
    );
    assert_ne!(
        first_handle.generation(),
        second.handle().generation(),
        "generation must advance on reuse"
    );

    // The stale old handle is rejected by validated recovery...
    assert_eq!(
        table
            .typed::<CountingResource>(first_handle)
            .unwrap_err()
            .code(),
        ResourceErrorCode::ResourceStale
    );
    // ...leaving the reused resource open and untouched.
    assert_eq!(table.get(&second).unwrap().label, "two");
}

#[test]
fn close_moves_slot_to_closed_and_rejects_double_close() {
    let mut table = ResourceTable::new();
    let (res, closes, drops) = CountingResource::new("x");
    let token = table.push(res).expect("push");

    let progress = table
        .begin_close(token, ResourceCloseReason::ResourceClosed)
        .expect("begin close");
    assert_eq!(progress, CloseProgress::Ready);
    assert_eq!(closes.load(Ordering::SeqCst), 1);

    assert_eq!(table.len(), 0);
    assert_eq!(
        table
            .begin_close(token, ResourceCloseReason::ResourceClosed)
            .unwrap_err()
            .code(),
        ResourceErrorCode::ResourceAlreadyClosed
    );
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "Ready close drops the value"
    );
}

#[test]
fn pending_close_holds_generation_until_poll_finishes() {
    let mut table = ResourceTable::new();
    let (res, polls) = TwoPollResource::new();
    let token = table.push(res).expect("push");

    assert_eq!(
        table
            .begin_close(token, ResourceCloseReason::ResourceClosed)
            .expect("begin"),
        CloseProgress::Pending
    );
    assert_eq!(table.len(), 1, "still present while closing");
    assert_eq!(
        polls.load(Ordering::SeqCst),
        0,
        "no polling before begin_close returned Pending"
    );

    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert_eq!(table.poll_close(token, &mut cx), Poll::Pending);
    assert_eq!(table.len(), 1);
    assert_eq!(
        table.get(&token).unwrap_err().code(),
        ResourceErrorCode::ResourceAlreadyClosed,
        "get while closing is rejected"
    );

    assert!(table.poll_close(token, &mut cx).is_ready());
    assert_eq!(table.len(), 0);

    // A closed (vacant, same generation) slot is rejected by validated
    // recovery as AlreadyClosed.
    let closed = table
        .typed::<TwoPollResource>(token.handle())
        .expect_err("closed slot must not recover");
    assert_eq!(closed.code(), ResourceErrorCode::ResourceAlreadyClosed);
}

#[test]
fn parent_cannot_close_while_live_children_exist() {
    let mut table = ResourceTable::new();
    let parent = table
        .push(CountingResource::new("parent").0)
        .expect("parent");
    let child = table
        .push_child(CountingResource::new("child").0, &parent)
        .expect("child");

    assert_eq!(
        table
            .begin_close(parent, ResourceCloseReason::ResourceClosed)
            .unwrap_err()
            .code(),
        ResourceErrorCode::ResourceHasChildren
    );

    drive_close(&mut table, child, ResourceCloseReason::ResourceClosed);
    drive_close(&mut table, parent, ResourceCloseReason::ResourceClosed);
    assert!(table.is_empty());
}

#[test]
fn child_insert_validates_parent_handle_and_liveness() {
    let mut table = ResourceTable::new();
    let parent = table.push(CountingResource::new("p").0).expect("parent");
    let _child = table
        .push_child(CountingResource::new("c").0, &parent)
        .expect("child");

    // Wrong parent type is rejected at validated recovery.
    assert_eq!(
        table
            .typed::<RecordMarker>(parent.handle())
            .unwrap_err()
            .code(),
        ResourceErrorCode::ResourceTypeMismatch
    );

    // A closed (but not yet slot-reused) parent cannot accept new children.
    let mut table2 = ResourceTable::new();
    let gone = table2.push(CountingResource::new("gone").0).expect("push");
    drive_close(&mut table2, gone, ResourceCloseReason::ResourceClosed);
    assert_eq!(
        table2
            .typed::<CountingResource>(gone.handle())
            .expect_err("closed parent")
            .code(),
        ResourceErrorCode::ResourceAlreadyClosed
    );
}

#[test]
fn close_all_is_child_first() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut table = ResourceTable::new();

    let root = table
        .push(CloseRecorder {
            order: order.clone(),
            name: "root",
        })
        .expect("root");
    let mid = table
        .push_child(
            CloseRecorder {
                order: order.clone(),
                name: "mid",
            },
            &root,
        )
        .expect("mid");
    let _leaf = table
        .push_child(
            CloseRecorder {
                order: order.clone(),
                name: "leaf",
            },
            &mid,
        )
        .expect("leaf");
    let _sib = table
        .push_child(
            CloseRecorder {
                order: order.clone(),
                name: "sib",
            },
            &root,
        )
        .expect("sib");

    table
        .close_all(ResourceCloseReason::VmReset)
        .expect("close_all ok");

    let order = order.lock().unwrap().clone();
    let position = |name: &str| order.iter().position(|e| *e == name).unwrap();
    assert!(
        position("leaf") < position("mid"),
        "leaf closes before its parent"
    );
    assert!(
        position("mid") < position("root") && position("sib") < position("root"),
        "all children close before the root parent"
    );
    assert!(table.is_empty());
}

#[test]
fn close_all_continues_past_failures_and_reports_first() {
    let mut table = ResourceTable::new();
    let _ok = table.push(CountingResource::new("ok").0).expect("ok");
    table.push(PollFailingResource).expect("failing");

    let result = table.close_all(ResourceCloseReason::VmReset);
    assert_eq!(
        result.unwrap_err().code(),
        ResourceErrorCode::ResourceCleanupFailed
    );
    assert!(
        table.is_empty(),
        "every resource was attempted despite a failure"
    );
}

#[test]
fn sync_close_all_never_succeeds_while_resources_remain_pending() {
    let mut table = ResourceTable::new();
    table.push(GateResource::new().0).expect("gated");

    // A genuinely pending resource cannot be synchronously driven with a
    // no-op waker; close_all must not claim success.
    let err = table.close_all(ResourceCloseReason::VmReset).unwrap_err();
    assert_eq!(err.code(), ResourceErrorCode::ResourceClosePending);
    assert_eq!(
        table.len(),
        1,
        "the pending resource is still present (close_all did NOT succeed)"
    );
}

#[test]
fn poll_close_open_resource_reports_not_closing() {
    let mut table = ResourceTable::new();
    let token = table.push(CountingResource::new("o").0).expect("push");
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    // poll_close on an Open resource must be ResourceNotClosing, never a
    // confusing InvalidResourceHandle.
    let poll = table.poll_close(token, &mut cx);
    let Poll::Ready(Err(error)) = poll else {
        panic!("expected Ready(Err) for poll_close on an open resource");
    };
    assert_eq!(error.code(), ResourceErrorCode::ResourceNotClosing);

    // The naive poll must leave the resource open and fully usable.
    assert_eq!(table.len(), 1);
    table
        .get(&token)
        .expect("still open after stray poll_close");
}

#[test]
fn poll_close_all_pending_across_calls_uses_caller_waker_and_progresses() {
    let mut table = ResourceTable::new();
    let (gate, state) = GateResource::new();
    let token = table.push(gate).expect("push");

    let (waker, wake_latch) = tracking_waker();
    let mut cx = Context::from_waker(&waker);

    // First call: the gate holds; everything stays live and NOT done.
    let result = table.poll_close_all(ResourceCloseReason::VmReset, &mut cx);
    assert!(
        matches!(result, Poll::Pending),
        "must not prematurely complete"
    );
    assert_eq!(table.len(), 1, "no premature Ok while resource remains");

    // The resource captured the caller-supplied waker, so a real external
    // event can drive it: waking that waker uses the caller's context.
    let captured = state.last_waker.lock().unwrap().clone();
    let captured = captured.expect("resource must register the caller waker");
    assert!(
        waker.will_wake(&captured),
        "resource used the caller's waker"
    );
    captured.wake();
    assert_eq!(
        wake_latch.load(Ordering::SeqCst),
        1,
        "waking the captured caller waker drives progress notification"
    );

    // Release the gate; the next poll drives close to quiescence.
    state.released.store(true, Ordering::SeqCst);
    let Poll::Ready(result) = table.poll_close_all(ResourceCloseReason::VmReset, &mut cx) else {
        panic!("a released gate must complete");
    };
    assert_eq!(result.expect("clean close"), 1, "cumulative closed count");
    assert!(table.is_empty());
    let _ = token;
}

#[test]
fn poll_close_all_is_child_first_across_pending_polls() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let state = Arc::new(GateState::default());
    let mut table = ResourceTable::new();

    let root = table
        .push(GateRecorder {
            state: state.clone(),
            order: order.clone(),
            name: "root",
        })
        .expect("root");
    let mid = table
        .push_child(
            GateRecorder {
                state: state.clone(),
                order: order.clone(),
                name: "mid",
            },
            &root,
        )
        .expect("mid");
    let _leaf = table
        .push_child(
            GateRecorder {
                state: state.clone(),
                order: order.clone(),
                name: "leaf",
            },
            &mid,
        )
        .expect("leaf");

    let (waker, _latch) = tracking_waker();
    let mut cx = Context::from_waker(&waker);

    // First poll: only the leaf can begin (mid/root have live children), and
    // it is genuinely pending. The parent and child remain.
    assert!(matches!(
        table.poll_close_all(ResourceCloseReason::VmReset, &mut cx),
        Poll::Pending
    ));
    assert_eq!(table.len(), 3);
    // Only leaf's begin_close has fired so far; parents are still waiting.
    let order0 = order.lock().unwrap().clone();
    assert_eq!(order0, vec!["leaf"], "leaf begins first");

    // Release the gate: the leaf finishes, then mid and root close in order.
    state.released.store(true, Ordering::SeqCst);
    let Poll::Ready(result) = table.poll_close_all(ResourceCloseReason::VmReset, &mut cx) else {
        panic!("released gate must complete the sweep");
    };
    assert_eq!(result.expect("clean close"), 3);

    let order = order.lock().unwrap().clone();
    let position = |name: &str| order.iter().position(|e| *e == name).unwrap();
    assert!(
        position("leaf") < position("mid") && position("mid") < position("root"),
        "child-first order held across pending polls: {order:?}"
    );
    assert!(table.is_empty());
}

#[test]
fn poll_close_all_retains_first_cleanup_error_until_all_resources_finish() {
    let mut table = ResourceTable::new();
    // A resource that fails synchronously on its first poll.
    table.push(PollFailingResource).expect("failing");
    // A genuinely pending resource behind it.
    let (gate, state) = GateResource::new();
    table.push(gate).expect("gated");

    let (waker, _latch) = tracking_waker();
    let mut cx = Context::from_waker(&waker);

    // First poll: the failure was recorded, but the gate is still pending, so
    // we must NOT surface the error prematurely.
    assert!(matches!(
        table.poll_close_all(ResourceCloseReason::VmReset, &mut cx),
        Poll::Pending
    ));
    assert_eq!(table.len(), 1, "only the pending gate remains");

    // Release the gate; only now, at quiescence, is the retained error reported.
    state.released.store(true, Ordering::SeqCst);
    let Poll::Ready(result) = table.poll_close_all(ResourceCloseReason::VmReset, &mut cx) else {
        panic!("released gate must finish the sweep");
    };
    let err = result.expect_err("first cleanup error retained until quiescence");
    assert_eq!(err.code(), ResourceErrorCode::ResourceCleanupFailed);
    assert!(
        table.is_empty(),
        "error reported exactly once all resources finished"
    );
    let _ = waker;
}

#[test]
fn poll_close_all_rejects_conflicting_reason_deterministically() {
    let mut table = ResourceTable::new();
    let (gate, state) = GateResource::new();
    let _token = table.push(gate).expect("push");
    let (waker, _latch) = tracking_waker();
    let mut cx = Context::from_waker(&waker);

    // Begin a sweep with VmReset.
    assert!(matches!(
        table.poll_close_all(ResourceCloseReason::VmReset, &mut cx),
        Poll::Pending
    ));

    // A conflicting reason is rejected deterministically and leaves the
    // in-flight sweep (and its original reason) untouched.
    let Poll::Ready(Err(conflict)) = table.poll_close_all(ResourceCloseReason::Deadline, &mut cx)
    else {
        panic!("conflicting reason must be rejected");
    };
    assert_eq!(conflict.code(), ResourceErrorCode::ResourceCloseInProgress);
    assert_eq!(table.len(), 1, "the in-flight sweep is untouched");

    // The original reason still completes it.
    state.released.store(true, Ordering::SeqCst);
    let Poll::Ready(result) = table.poll_close_all(ResourceCloseReason::VmReset, &mut cx) else {
        panic!("original reason must complete the sweep");
    };
    assert!(result.is_ok());
    assert!(table.is_empty());
}

#[test]
fn capacity_limit_is_enforced() {
    let mut table = ResourceTable::with_limit(1).expect("valid");
    let _first = table.push(CountingResource::new("only").0).expect("push");
    let err = table.push(CountingResource::new("overflow").0).unwrap_err();
    assert_eq!(err.code(), ResourceErrorCode::ResourceLimitExceeded);
    assert_eq!(table.len(), 1);
}

#[test]
fn arena_identities_are_distinct_across_tables() {
    let a = ResourceTable::new();
    let b = ResourceTable::new();
    assert_ne!(a.arena_id(), b.arena_id());
}

#[test]
fn table_is_send() {
    // The table and its owned resources move between owners but are never
    // shared; requiring `Send` (and not `Sync`) is part of the contract.
    require_send::<ResourceTable>();
    require_send::<Resource<CountingResource>>();
}

#[test]
fn begin_close_is_idempotent_for_closing_resource() {
    let mut table = ResourceTable::new();
    let (res, _) = TwoPollResource::new();
    let token = table.push(res).expect("push");

    assert_eq!(
        table
            .begin_close(token, ResourceCloseReason::ResourceClosed)
            .expect("first begin"),
        CloseProgress::Pending
    );
    // Repeated begin_close on a closing resource is accepted and still Pending.
    assert_eq!(
        table
            .begin_close(token, ResourceCloseReason::ResourceClosed)
            .expect("second begin"),
        CloseProgress::Pending
    );
    assert_eq!(table.len(), 1);
}
