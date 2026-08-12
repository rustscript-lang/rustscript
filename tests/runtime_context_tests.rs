mod vm {
    pub use ::vm::Value;
}

#[allow(dead_code)]
#[path = "../src/builtins/runtime/cancellation.rs"]
mod cancellation;
#[allow(dead_code)]
#[path = "../src/builtins/runtime/context.rs"]
mod context;
#[allow(dead_code)]
#[path = "../src/builtins/runtime/error.rs"]
mod error;
#[allow(dead_code)]
#[path = "../src/builtins/runtime/event.rs"]
mod event;
#[allow(dead_code)]
#[path = "../src/builtins/runtime/resource.rs"]
mod resource;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use cancellation::{CancellationReason, OperationRegistry, OperationStatus};
use context::{RuntimeContext, RuntimeContextConfig};
use error::RuntimeErrorCode;
use event::{EventLimits, EventPayload};
use resource::{CloseStatus, ResourceArena, ResourceHandle, ResourceTypeId};
use vm::Value;

#[test]
fn per_item_event_limits_are_run_scoped_configuration() {
    let context = RuntimeContext::default();
    assert_eq!(context.event_limits(), EventLimits::default());
    assert_eq!(context.config().event_limits(), EventLimits::default());

    let configured = RuntimeContext::with_config(RuntimeContextConfig::new(
        EventLimits::new(8, 4).expect("test limits should be valid"),
    ))
    .expect("context should be constructible");
    assert_eq!(configured.event_limits().max_payload_bytes(), 8);
    assert_eq!(configured.event_limits().max_depth(), 4);
}

#[test]
fn event_payload_validates_the_per_item_bound_before_placement() {
    let limits = EventLimits::new(8, 4).expect("test limits should be valid");

    let payload =
        EventPayload::try_new(Value::string("ok"), limits).expect("bounded event should validate");
    assert_eq!(payload.into_value(), Value::string("ok"));

    let too_large = EventPayload::try_new(Value::string("payload-too-large"), limits)
        .expect_err("oversized event should be rejected");
    assert_eq!(too_large.code(), RuntimeErrorCode::EventPayloadTooLarge);

    let too_deep = EventPayload::try_new(
        Value::array(vec![Value::array(vec![Value::array(vec![Value::Int(1)])])]),
        EventLimits::new(1024, 2).expect("depth test limits should be valid"),
    )
    .expect_err("too-deep event should be rejected");
    assert_eq!(too_deep.code(), RuntimeErrorCode::EventDepthExceeded);
}

#[test]
fn resource_handles_are_opaque_bounded_typed_and_cleanup_is_idempotent() {
    let cleanup_count = Arc::new(AtomicUsize::new(0));
    let count_for_cleanup = Arc::clone(&cleanup_count);
    let mut arena = ResourceArena::with_limit(1).expect("resource limit should be valid");
    let handle = arena
        .insert_with_cleanup(ResourceTypeId::IO_FILE, 7_u32, move |resource, reason| {
            assert_eq!(resource, 7);
            assert_eq!(reason, CancellationReason::ResourceClosed);
            count_for_cleanup.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .expect("first resource should be allocated");

    assert_eq!(
        arena
            .get::<u32>(handle, ResourceTypeId::IO_FILE)
            .expect("handle should resolve"),
        &7
    );
    assert_eq!(
        ResourceHandle::from_value(&handle.as_value()).expect("VM value should decode"),
        handle
    );
    let Value::Int(encoded) = handle.as_value() else {
        unreachable!("resource handle should encode as an integer");
    };
    let forged_generation = ResourceHandle::from_value(&Value::Int(encoded + (1 << 8)))
        .expect("the altered token remains structurally valid");
    let forged = arena
        .get::<u32>(forged_generation, ResourceTypeId::IO_FILE)
        .expect_err("an altered generation must not resolve");
    assert_eq!(forged.code(), RuntimeErrorCode::ResourceStale);
    let wrong_type = arena
        .get::<u32>(handle, ResourceTypeId::SQLITE_CONNECTION)
        .expect_err("wrong resource type should be rejected");
    assert_eq!(wrong_type.code(), RuntimeErrorCode::ResourceTypeMismatch);
    let limit_error = arena
        .insert(ResourceTypeId::IO_FILE, 8_u32)
        .expect_err("the bounded arena should reject a second resource");
    assert_eq!(limit_error.code(), RuntimeErrorCode::ResourceLimitExceeded);

    assert_eq!(
        arena
            .close(handle, CancellationReason::ResourceClosed)
            .expect("close should succeed"),
        CloseStatus::Closed
    );
    assert_eq!(
        arena
            .close(handle, CancellationReason::ResourceClosed)
            .expect("repeated close should be harmless"),
        CloseStatus::AlreadyClosed
    );
    assert_eq!(cleanup_count.load(Ordering::SeqCst), 1);

    let replacement = arena
        .insert(ResourceTypeId::IO_FILE, 9_u32)
        .expect("capacity should be reusable after close");
    assert_ne!(
        replacement, handle,
        "reusing a slot must change its generation"
    );
    let closed = arena
        .get::<u32>(handle, ResourceTypeId::IO_FILE)
        .expect_err("the prior generation must not resolve after slot reuse");
    assert_eq!(closed.code(), RuntimeErrorCode::ResourceStale);
    assert_eq!(
        arena
            .get::<u32>(replacement, ResourceTypeId::IO_FILE)
            .expect("the replacement generation should resolve"),
        &9
    );
}

#[test]
fn resource_handles_cannot_cross_resource_arenas() {
    let mut first = ResourceArena::with_limit(1).expect("resource limit should be valid");
    let second = ResourceArena::with_limit(1).expect("resource limit should be valid");
    let handle = first
        .insert(ResourceTypeId::IO_FILE, 1_u32)
        .expect("resource should be allocated");

    let error = second
        .get::<u32>(handle, ResourceTypeId::IO_FILE)
        .expect_err("a handle from another arena must be rejected");
    assert_eq!(error.code(), RuntimeErrorCode::ResourceHandleWrongTable);
}

#[test]
fn cancellation_transitions_once_and_runs_cleanup_once() {
    let cleanup_count = Arc::new(AtomicUsize::new(0));
    let count_for_cleanup = Arc::clone(&cleanup_count);
    let mut registry = OperationRegistry::with_limit(2).expect("operation limit should be valid");
    let operation = registry
        .start_owned(
            cancellation::OperationOwner::Io,
            None,
            None,
            Some(Box::new(move |end| {
                assert_eq!(
                    end,
                    cancellation::OperationEnd::Cancelled(CancellationReason::Requested)
                );
                count_for_cleanup.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })),
        )
        .expect("operation should start");
    let token = operation.token();

    assert_eq!(operation.status(), OperationStatus::Pending);
    assert!(
        operation
            .cancel(CancellationReason::Requested)
            .expect("cancel should succeed")
    );
    assert!(
        !operation
            .cancel(CancellationReason::Requested)
            .expect("cancel is idempotent")
    );
    assert_eq!(
        operation.status(),
        OperationStatus::Cancelled(CancellationReason::Requested)
    );
    assert_eq!(cleanup_count.load(Ordering::SeqCst), 1);
    let cancelled = token
        .check()
        .expect_err("the cancellation token should stop the operation");
    assert_eq!(cancelled.code(), RuntimeErrorCode::OperationCancelled);
    assert!(
        !operation
            .complete()
            .expect("terminal operation should remain terminal")
    );
}

#[test]
fn cancellation_after_completion_does_not_reopen_or_relabel_operation() {
    let mut registry = OperationRegistry::with_limit(2).expect("operation limit should be valid");
    let operation = registry
        .start_owned(cancellation::OperationOwner::Io, None, None, None)
        .expect("operation should start");
    assert!(operation.complete().expect("operation should complete"));
    assert!(
        !operation
            .cancel(CancellationReason::Requested)
            .expect("cancel is idempotent")
    );
    assert_eq!(operation.status(), OperationStatus::Completed);
    assert!(!operation.token().is_cancelled());
}

#[test]
fn operation_registry_bounds_active_operations_and_releases_cancelled_state() {
    let mut registry = OperationRegistry::with_limit(1).expect("operation limit should be valid");
    let operation = registry
        .start_owned(cancellation::OperationOwner::Io, None, None, None)
        .expect("first operation should start");
    let limit_error = registry
        .start_owned(cancellation::OperationOwner::Io, None, None, None)
        .expect_err("active operation limit should be enforced");
    assert_eq!(limit_error.code(), RuntimeErrorCode::OperationLimitExceeded);

    assert!(
        registry
            .cancel(operation.id(), CancellationReason::VmReset)
            .expect("registry cancellation should succeed")
    );
    assert_eq!(registry.active_count(), 0);
    assert!(matches!(
        operation.status(),
        OperationStatus::Cancelled(CancellationReason::VmReset)
    ));
}

#[test]
fn registry_retains_terminal_result_until_it_is_consumed() {
    let mut registry = OperationRegistry::with_limit(1).expect("operation limit should be valid");
    let operation = registry
        .start_owned(cancellation::OperationOwner::Io, None, None, None)
        .expect("operation should start");
    assert!(operation.complete().expect("completion should succeed"));

    let limit_error = registry
        .start_owned(cancellation::OperationOwner::Io, None, None, None)
        .expect_err("unconsumed terminal result should retain its registry slot");
    assert_eq!(limit_error.code(), RuntimeErrorCode::OperationLimitExceeded);
    assert!(registry.get(operation.id()).is_ok());

    assert!(
        !registry
            .complete(operation.id())
            .expect("consuming an already completed operation should succeed")
    );
    assert!(registry.get(operation.id()).is_err());
    registry
        .start_owned(cancellation::OperationOwner::Io, None, None, None)
        .expect("consuming the terminal result should release capacity");
}

#[test]
fn concurrent_completion_and_cancellation_choose_one_terminal_state() {
    let cleanup_count = Arc::new(AtomicUsize::new(0));
    let cleanup_for_operation = Arc::clone(&cleanup_count);
    let mut registry = OperationRegistry::with_limit(2).expect("operation limit should be valid");
    let operation = registry
        .start_owned(
            cancellation::OperationOwner::Io,
            None,
            None,
            Some(Box::new(move |_| {
                cleanup_for_operation.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })),
        )
        .expect("operation should start");
    let barrier = Arc::new(Barrier::new(3));

    let complete_operation = operation.clone();
    let complete_barrier = Arc::clone(&barrier);
    let complete = std::thread::spawn(move || {
        complete_barrier.wait();
        complete_operation
            .complete()
            .expect("completion should run")
    });

    let cancel_operation = operation.clone();
    let cancel_barrier = Arc::clone(&barrier);
    let cancel = std::thread::spawn(move || {
        cancel_barrier.wait();
        cancel_operation
            .cancel(CancellationReason::Requested)
            .expect("cancellation should run")
    });

    barrier.wait();
    let terminal_wins = usize::from(complete.join().expect("completion thread"))
        + usize::from(cancel.join().expect("cancellation thread"));
    assert_eq!(terminal_wins, 1);
    assert_eq!(cleanup_count.load(Ordering::SeqCst), 1);
    match operation.status() {
        OperationStatus::Completed => assert_eq!(operation.token().reason(), None),
        OperationStatus::Cancelled(reason) => {
            assert_eq!(reason, CancellationReason::Requested);
            assert_eq!(operation.token().reason(), Some(reason));
        }
        status => panic!("unexpected terminal state: {status:?}"),
    }
}

#[test]
fn completed_child_ignores_later_parent_cancellation() {
    let mut registry = OperationRegistry::with_limit(4).expect("operation limit should be valid");
    let parent = registry
        .start_owned(cancellation::OperationOwner::Http, None, None, None)
        .expect("parent should start");
    let child = registry
        .start_owned(
            cancellation::OperationOwner::Io,
            Some(&parent.token()),
            None,
            None,
        )
        .expect("child should start");

    assert!(child.complete().expect("child should complete"));
    assert!(
        parent
            .cancel(CancellationReason::Requested)
            .expect("parent should cancel")
    );
    assert_eq!(child.status(), OperationStatus::Completed);
    assert_eq!(child.token().reason(), None);
}

#[test]
fn expired_deadline_is_the_status_token_and_cleanup_reason() {
    let cleanup_end = Arc::new(Mutex::new(None));
    let cleanup_end_for_operation = Arc::clone(&cleanup_end);
    let mut registry = OperationRegistry::with_limit(4).expect("operation limit should be valid");
    let parent = registry
        .start_owned(cancellation::OperationOwner::Io, None, None, None)
        .expect("parent should start");
    let operation = registry
        .start_owned(
            cancellation::OperationOwner::Io,
            Some(&parent.token()),
            Some(Instant::now() - Duration::from_millis(1)),
            Some(Box::new(move |end| {
                *cleanup_end_for_operation.lock().expect("cleanup lock") = Some(end);
                Ok(())
            })),
        )
        .expect("deadline child should start");

    assert!(
        operation
            .cancel(CancellationReason::Requested)
            .expect("deadline cancellation should run")
    );
    assert_eq!(
        operation.token().reason(),
        Some(CancellationReason::Deadline)
    );
    assert_eq!(
        operation.status(),
        OperationStatus::Cancelled(CancellationReason::Deadline)
    );
    assert_eq!(
        *cleanup_end.lock().expect("cleanup lock"),
        Some(cancellation::OperationEnd::Cancelled(
            CancellationReason::Deadline
        ))
    );
}
