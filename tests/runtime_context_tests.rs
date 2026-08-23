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

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use cancellation::CancellationReason;
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
        .get::<u32>(handle, ResourceTypeId::CALLBACK)
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
fn run_cancellation_token_reports_the_first_reason_only() {
    // The run-level cancellation flag is a plain first-reason-wins marker
    // (no parent/child propagation tree): the first cancel binds the reason,
    // later cancels with any reason are no-ops, and the reason is preserved.
    let token = cancellation::CancellationToken::root();
    assert!(!token.is_cancelled());
    assert_eq!(token.reason(), None);

    assert!(token.cancel(CancellationReason::Deadline));
    assert!(!token.cancel(CancellationReason::Requested));
    assert!(!token.cancel(CancellationReason::VmReset));
    assert_eq!(token.reason(), Some(CancellationReason::Deadline));
    assert!(token.is_cancelled());

    let cancelled = token
        .check()
        .expect_err("a cancelled token must report the cancellation");
    assert_eq!(cancelled.code(), RuntimeErrorCode::OperationCancelled);
}
