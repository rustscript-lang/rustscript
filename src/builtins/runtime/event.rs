//! Per-item event bounds for the invocation item stream.
//!
//! [`EventLimits`] configures the per-item bound applied to one
//! `stream::emit(value)` call: a maximum payload byte estimate and a maximum
//! nesting depth. The core validates only this per-item value bound before
//! placing the value in the active invocation's single pending-event slot.
//! Sequence assignment, cumulative byte accounting, event receipts, and
//! embedding-owned sinks are not part of the core contract; delivery policy
//! belongs to the embedding.

use crate::vm::Value;

use super::error::{RuntimeError, RuntimeErrorCode, RuntimeResult};

pub const DEFAULT_MAX_EVENT_PAYLOAD_BYTES: usize = 64 * 1024;
pub const DEFAULT_MAX_EVENT_DEPTH: usize = 64;

/// Per-item bounds applied to one `stream::emit(value)` call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventLimits {
    max_payload_bytes: usize,
    max_depth: usize,
}

#[allow(dead_code)]
impl EventLimits {
    pub fn new(max_payload_bytes: usize, max_depth: usize) -> RuntimeResult<Self> {
        if max_payload_bytes == 0 || max_depth == 0 {
            return Err(RuntimeError::new(
                RuntimeErrorCode::InvalidConfiguration,
                "stream::emit",
                "event payload and depth limits must be positive",
            ));
        }
        Ok(Self {
            max_payload_bytes,
            max_depth,
        })
    }

    pub const fn max_payload_bytes(self) -> usize {
        self.max_payload_bytes
    }

    pub const fn max_depth(self) -> usize {
        self.max_depth
    }
}

impl Default for EventLimits {
    fn default() -> Self {
        Self {
            max_payload_bytes: DEFAULT_MAX_EVENT_PAYLOAD_BYTES,
            max_depth: DEFAULT_MAX_EVENT_DEPTH,
        }
    }
}

/// An event value whose per-item bound has been validated.
#[derive(Clone, Debug, PartialEq)]
pub struct EventPayload {
    value: Value,
    size_bytes: usize,
}

impl EventPayload {
    /// Validates a value against the per-item bound and preserves the
    /// validated value plus its bounded size estimate.
    pub fn try_new(value: Value, limits: EventLimits) -> RuntimeResult<Self> {
        let size_bytes = measure_value(&value, 0, limits)?;
        Ok(Self { value, size_bytes })
    }

    pub fn into_value(self) -> Value {
        self.value
    }

    #[allow(dead_code)]
    pub fn size_bytes(&self) -> usize {
        self.size_bytes
    }
}

/// Estimates the bounded representation size of a value.
///
/// The estimate is deliberately independent of serialization formats. It counts scalar tags,
/// container headers, string/byte contents, and recursively contained values. The host transport
/// may produce larger or smaller blobs; this bound is a conservative per-item budget used to
/// reject oversized event payloads before they enter the single pending-event slot.
fn measure_value(value: &Value, depth: usize, limits: EventLimits) -> RuntimeResult<usize> {
    if depth > limits.max_depth {
        return Err(RuntimeError::new(
            RuntimeErrorCode::EventDepthExceeded,
            "stream::emit",
            "event payload nesting exceeds the configured bound",
        )
        .with_limit(limits.max_depth)
        .with_value(depth));
    }

    let size = match value {
        Value::Null => 1,
        Value::Bool(_) => 1,
        Value::Int(_) => 8,
        Value::Float(_) => 8,
        Value::String(text) => 2 * text.len() + 1,
        Value::Bytes(bytes) => bytes.len() + 2,
        Value::Array(items) => {
            let mut size = 2usize;
            for item in items.iter() {
                size = checked_payload_add(size, measure_value(item, depth + 1, limits)?, limits)?;
            }
            size
        }
        Value::Map(entries) => {
            let mut size = 2usize;
            for (key, value) in entries.iter() {
                size = checked_payload_add(size, measure_value(key, depth + 1, limits)?, limits)?;
                size = checked_payload_add(size, measure_value(value, depth + 1, limits)?, limits)?;
            }
            size
        }
        Value::Callable(_) => 8,
    };
    if size > limits.max_payload_bytes {
        return Err(RuntimeError::new(
            RuntimeErrorCode::EventPayloadTooLarge,
            "stream::emit",
            "event payload exceeds the configured byte bound",
        )
        .with_limit(limits.max_payload_bytes)
        .with_value(size));
    }
    Ok(size)
}

fn checked_payload_add(
    current: usize,
    additional: usize,
    limits: EventLimits,
) -> RuntimeResult<usize> {
    let total = current.checked_add(additional).ok_or_else(|| {
        RuntimeError::new(
            RuntimeErrorCode::EventPayloadTooLarge,
            "stream::emit",
            "event payload size overflowed",
        )
        .with_limit(limits.max_payload_bytes)
    })?;
    if total > limits.max_payload_bytes {
        return Err(RuntimeError::new(
            RuntimeErrorCode::EventPayloadTooLarge,
            "stream::emit",
            "event payload exceeds the configured byte bound",
        )
        .with_limit(limits.max_payload_bytes)
        .with_value(total));
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::{EventLimits, EventPayload};
    use crate::vm::Value;

    #[test]
    fn per_item_limits_validate_payload_and_depth() {
        let limits = EventLimits::new(32, 4).expect("limits should be valid");
        let payload =
            EventPayload::try_new(Value::string("event"), limits).expect("payload should fit");
        assert!(payload.size_bytes() >= 5);
        assert_eq!(payload.into_value(), Value::string("event"));
    }

    #[test]
    fn oversized_or_too_deep_values_are_rejected_before_placement() {
        let limits = EventLimits::new(8, 2).expect("limits should be valid");
        let too_large = EventPayload::try_new(Value::string("payload-too-large"), limits)
            .expect_err("oversized event should be rejected");
        assert_eq!(
            too_large.code(),
            super::super::error::RuntimeErrorCode::EventPayloadTooLarge
        );
        let too_deep = EventPayload::try_new(
            Value::array(vec![Value::array(vec![Value::array(vec![Value::Int(1)])])]),
            limits,
        )
        .expect_err("too-deep event should be rejected");
        assert_eq!(
            too_deep.code(),
            super::super::error::RuntimeErrorCode::EventDepthExceeded
        );
    }
}
