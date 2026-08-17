//! Focused tests for the host-agnostic typed generational `ResourceTable`.
//!
//! These exercise the generic resource layer in isolation: handle encoding,
//! arena/scope identity, slot generation, typed access, type erasure,
//! parent/child links, stale-handle rejection, child-first close, and the
//! close state/progress contracts. No concrete VM builtin resource is involved.

use vm::resource::{
    CloseProgress, HostResource, Resource, ResourceCloseReason, ResourceError, ResourceErrorCode,
    ResourceResult, ResourceTable,
};
use vm::{ResourceHandle, Value};

use std::sync::atomic::{AtomicUsize, Ordering};
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

    fn poll_close(&mut self, _cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
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

// ---- helpers ------------------------------------------------------------------------

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn noop_waker() -> Waker {
    Waker::from(Arc::new(NoopWake))
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

    drop(table);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "dropping the table reclaims its resources"
    );
}

#[test]
fn handle_round_trips_through_value() {
    let mut table = ResourceTable::new();
    let (res, _, _) = CountingResource::new("f");
    let token = table.push(res).expect("push");
    let raw = token.handle();

    let as_value = raw.as_value();
    let back = ResourceHandle::from_value(&as_value).expect("round trip");

    // The decoded handle is usable through the table.
    let token2: Resource<CountingResource> = Resource::from_handle(back);
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
fn typed_access_rejects_wrong_concrete_type() {
    let mut table = ResourceTable::new();
    let (res, closes, drops) = CountingResource::new("a");
    let token = table.push(res).expect("push");

    // Same raw handle, wrong type marker.
    let wrong: Resource<RecordMarker> = Resource::from_handle(token.handle());
    assert_eq!(
        table.get(&wrong).unwrap_err().code(),
        ResourceErrorCode::ResourceTypeMismatch
    );
    assert_eq!(
        table.get_mut(&wrong).unwrap_err().code(),
        ResourceErrorCode::ResourceTypeMismatch
    );
    // Wrong-type begin_close must also be rejected without touching the
    // resource's state transitions.
    assert_eq!(
        table
            .begin_close(wrong, ResourceCloseReason::VmReset)
            .unwrap_err()
            .code(),
        ResourceErrorCode::ResourceTypeMismatch
    );

    // The wrong-type probes left the real resource Open and untouched: the
    // original token is still readable, unmutated, never begun-closing, and
    // still closable.
    assert_eq!(table.get(&token).unwrap().label, "a");
    assert_eq!(
        table.len(),
        1,
        "resource must remain open after wrong-type access"
    );
    assert_eq!(
        closes.load(Ordering::SeqCst),
        0,
        "wrong-type close must not fire the real close"
    );
    drive_close(&mut table, token, ResourceCloseReason::ResourceClosed);
    assert_eq!(closes.load(Ordering::SeqCst), 1);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "only the real close drops the value"
    );
}

#[test]
fn cross_table_handle_is_rejected() {
    let mut table_a = ResourceTable::new();
    let token = table_a.push(CountingResource::new("a").0).expect("push");
    let stale_token = token;

    let table_b = ResourceTable::new();
    assert_eq!(
        table_b.get(&stale_token).unwrap_err().code(),
        ResourceErrorCode::ResourceHandleWrongTable
    );
}

#[test]
fn stale_generation_rejects_handle_after_slot_reuse() {
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

    let stale: Resource<CountingResource> = Resource::from_handle(first_handle);
    assert_eq!(
        table.get(&stale).unwrap_err().code(),
        ResourceErrorCode::ResourceStale
    );
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
    let handle = token.handle();

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

    // The closed handle is rejected: the slot is vacant (and, on a reuse, the
    // generation would additionally make it stale — covered elsewhere).
    let closed: Resource<TwoPollResource> = Resource::from_handle(handle);
    assert_eq!(
        table.get(&closed).unwrap_err().code(),
        ResourceErrorCode::ResourceAlreadyClosed
    );
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
fn child_insert_validates_parent_type_and_liveness() {
    let mut table = ResourceTable::new();
    let parent: Resource<CountingResource> =
        table.push(CountingResource::new("p").0).expect("parent");
    let _child = table
        .push_child(CountingResource::new("c").0, &parent)
        .expect("child");

    // Wrong parent type marker is rejected.
    let wrong_parent: Resource<RecordMarker> = Resource::from_handle(parent.handle());
    assert_eq!(
        table
            .push_child(CountingResource::new("orphan").0, &wrong_parent)
            .unwrap_err()
            .code(),
        ResourceErrorCode::ResourceTypeMismatch
    );

    // A closed (but not yet vacated-slot-reused) parent cannot accept children.
    let mut table2 = ResourceTable::new();
    let gone = table2.push(CountingResource::new("gone").0).expect("push");
    drive_close(&mut table2, gone, ResourceCloseReason::ResourceClosed);
    let gone: Resource<CountingResource> = Resource::from_handle(gone.handle());
    let err = table2
        .push_child(CountingResource::new("late").0, &gone)
        .unwrap_err();
    assert_eq!(err.code(), ResourceErrorCode::ResourceAlreadyClosed);
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
