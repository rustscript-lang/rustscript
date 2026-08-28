//! Compatibility re-exports for invocation event validation.

#[allow(unused_imports)]
pub use crate::vm::runtime::{
    DEFAULT_MAX_EVENT_DEPTH, DEFAULT_MAX_EVENT_PAYLOAD_BYTES, EventLimits, EventPayload,
    estimate_value_size,
};
