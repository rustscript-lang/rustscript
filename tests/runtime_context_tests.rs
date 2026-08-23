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

use cancellation::CancellationReason;
use context::{RuntimeContext, RuntimeContextConfig};
use error::RuntimeErrorCode;
use event::{EventLimits, EventPayload};
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
