use crate::vm::Value;

use super::error::{RuntimeError, RuntimeErrorCode, RuntimeResult};

pub const DEFAULT_MAX_EVENT_PAYLOAD_BYTES: usize = 64 * 1024;
pub const DEFAULT_MAX_EVENT_DEPTH: usize = 64;
pub const DEFAULT_MAX_EVENTS: u64 = 1_024;
pub const DEFAULT_MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;

/// Bounds applied before an event is handed to an embedding-owned sink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventLimits {
    max_payload_bytes: usize,
    max_depth: usize,
    max_events: u64,
    max_total_bytes: usize,
}

impl EventLimits {
    pub fn new(max_payload_bytes: usize, max_depth: usize) -> RuntimeResult<Self> {
        Self::with_budget(
            max_payload_bytes,
            max_depth,
            DEFAULT_MAX_EVENTS,
            DEFAULT_MAX_EVENT_BYTES,
        )
    }

    pub fn with_budget(
        max_payload_bytes: usize,
        max_depth: usize,
        max_events: u64,
        max_total_bytes: usize,
    ) -> RuntimeResult<Self> {
        if max_payload_bytes == 0 || max_depth == 0 || max_events == 0 || max_total_bytes == 0 {
            return Err(RuntimeError::new(
                RuntimeErrorCode::InvalidConfiguration,
                "runtime::emit",
                "event payload and depth limits must be positive",
            ));
        }
        Ok(Self {
            max_payload_bytes,
            max_depth,
            max_events,
            max_total_bytes,
        })
    }

    pub const fn max_payload_bytes(self) -> usize {
        self.max_payload_bytes
    }

    pub const fn max_depth(self) -> usize {
        self.max_depth
    }

    pub const fn max_events(self) -> u64 {
        self.max_events
    }

    pub const fn max_total_bytes(self) -> usize {
        self.max_total_bytes
    }
}

impl Default for EventLimits {
    fn default() -> Self {
        Self {
            max_payload_bytes: DEFAULT_MAX_EVENT_PAYLOAD_BYTES,
            max_depth: DEFAULT_MAX_EVENT_DEPTH,
            max_events: DEFAULT_MAX_EVENTS,
            max_total_bytes: DEFAULT_MAX_EVENT_BYTES,
        }
    }
}

/// An event value whose size and nesting have already been checked.
#[derive(Clone, Debug, PartialEq)]
pub struct EventPayload {
    value: Value,
    size_bytes: usize,
}

impl EventPayload {
    pub fn try_new(value: Value, limits: EventLimits) -> RuntimeResult<Self> {
        let size_bytes = estimate_value_size(&value, limits)?;
        Ok(Self { value, size_bytes })
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn size_bytes(&self) -> usize {
        self.size_bytes
    }

    pub fn into_value(self) -> Value {
        self.value
    }
}

/// Embedding-owned transport hook for bounded runtime events.
pub trait EventSink: Send {
    fn emit(&mut self, payload: EventPayload) -> RuntimeResult<()>;
}

impl<F> EventSink for F
where
    F: FnMut(EventPayload) -> RuntimeResult<()> + Send + 'static,
{
    fn emit(&mut self, payload: EventPayload) -> RuntimeResult<()> {
        self(payload)
    }
}

/// Validates and forwards generic values without attaching agent or platform semantics.
pub struct EventEmitter {
    limits: EventLimits,
    sink: Option<Box<dyn EventSink>>,
    emitted_events: u64,
    emitted_bytes: usize,
}

#[allow(dead_code)]
impl EventEmitter {
    pub fn new(limits: EventLimits) -> Self {
        Self {
            limits,
            sink: None,
            emitted_events: 0,
            emitted_bytes: 0,
        }
    }

    pub fn limits(&self) -> EventLimits {
        self.limits
    }

    pub fn set_sink<S>(&mut self, sink: S)
    where
        S: EventSink + 'static,
    {
        self.sink = Some(Box::new(sink));
    }

    pub fn clear_sink(&mut self) {
        self.sink = None;
    }

    pub fn reset_for_reuse(&mut self) {
        self.sink = None;
        self.emitted_events = 0;
        self.emitted_bytes = 0;
    }

    pub fn emitted_events(&self) -> u64 {
        self.emitted_events
    }

    pub fn emit(&mut self, value: Value) -> RuntimeResult<EventReceipt> {
        let payload = EventPayload::try_new(value, self.limits)?;
        if self.emitted_events >= self.limits.max_events {
            return Err(RuntimeError::new(
                RuntimeErrorCode::EventSequenceExhausted,
                "runtime::emit",
                "event count exceeds the configured bound",
            )
            .with_limit(self.limits.max_events.min(usize::MAX as u64) as usize)
            .with_value(self.emitted_events));
        }
        let total_bytes = self
            .emitted_bytes
            .checked_add(payload.size_bytes())
            .ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::EventPayloadTooLarge,
                    "runtime::emit",
                    "cumulative event bytes overflowed",
                )
            })?;
        if total_bytes > self.limits.max_total_bytes {
            return Err(RuntimeError::new(
                RuntimeErrorCode::EventPayloadTooLarge,
                "runtime::emit",
                "cumulative event bytes exceed the configured bound",
            )
            .with_limit(self.limits.max_total_bytes)
            .with_value(total_bytes as u64));
        }
        let sequence = self.emitted_events.checked_add(1).ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::EventSequenceExhausted,
                "runtime::emit",
                "event sequence exhausted",
            )
        })?;
        let sink = self.sink.as_mut().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::EventSinkUnavailable,
                "runtime::emit",
                "an event sink has not been configured",
            )
        })?;
        sink.emit(payload.clone()).map_err(|error| {
            RuntimeError::new(
                RuntimeErrorCode::EventSinkRejected,
                "runtime::emit",
                error.to_string(),
            )
        })?;
        self.emitted_events = sequence;
        self.emitted_bytes = total_bytes;
        Ok(EventReceipt {
            sequence,
            payload_bytes: payload.size_bytes(),
        })
    }
}

impl Default for EventEmitter {
    fn default() -> Self {
        Self::new(EventLimits::default())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventReceipt {
    sequence: u64,
    payload_bytes: usize,
}

#[allow(dead_code)]
impl EventReceipt {
    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    pub const fn payload_bytes(self) -> usize {
        self.payload_bytes
    }
}

/// Estimates the bounded representation size used by [`EventPayload`].
///
/// The estimate is deliberately independent of serialization formats. It counts scalar tags,
/// container headers, string/byte contents, and recursively contained values. The host transport
/// can apply a stricter byte limit when it serializes the validated value.
pub fn estimate_value_size(value: &Value, limits: EventLimits) -> RuntimeResult<usize> {
    measure_value(value, 0, limits)
}

fn measure_value(value: &Value, depth: usize, limits: EventLimits) -> RuntimeResult<usize> {
    if depth > limits.max_depth {
        return Err(RuntimeError::new(
            RuntimeErrorCode::EventDepthExceeded,
            "runtime::emit",
            "event payload nesting exceeds the configured bound",
        )
        .with_limit(limits.max_depth)
        .with_value(depth as u64));
    }

    let size = match value {
        Value::Null | Value::Bool(_) => 1,
        Value::Int(_) | Value::Float(_) => 9,
        Value::String(text) => 1usize.saturating_add(text.len()),
        Value::Bytes(bytes) => 1usize.saturating_add(bytes.len()),
        Value::Callable(_) => 17,
        Value::Array(values) => {
            let mut size = 5usize;
            for child in values.iter() {
                size = checked_payload_add(size, measure_value(child, depth + 1, limits)?, limits)?;
            }
            size
        }
        Value::Map(entries) => {
            let mut size = 5usize;
            for (key, child) in entries.iter() {
                size = checked_payload_add(size, measure_value(key, depth + 1, limits)?, limits)?;
                size = checked_payload_add(size, measure_value(child, depth + 1, limits)?, limits)?;
            }
            size
        }
    };

    if size > limits.max_payload_bytes {
        return Err(RuntimeError::new(
            RuntimeErrorCode::EventPayloadTooLarge,
            "runtime::emit",
            "event payload exceeds the configured byte bound",
        )
        .with_limit(limits.max_payload_bytes)
        .with_value(size as u64));
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
            "runtime::emit",
            "event payload size overflowed",
        )
        .with_limit(limits.max_payload_bytes)
    })?;
    if total > limits.max_payload_bytes {
        return Err(RuntimeError::new(
            RuntimeErrorCode::EventPayloadTooLarge,
            "runtime::emit",
            "event payload exceeds the configured byte bound",
        )
        .with_limit(limits.max_payload_bytes)
        .with_value(total as u64));
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::{EventEmitter, EventLimits, EventPayload};
    use crate::vm::Value;

    #[test]
    fn payload_size_and_sequence_are_exposed_after_validation() {
        let limits = EventLimits::new(128, 4).expect("limits should be valid");
        let payload =
            EventPayload::try_new(Value::string("event"), limits).expect("payload should fit");
        assert!(payload.size_bytes() >= 5);

        let mut emitter = EventEmitter::new(limits);
        emitter.set_sink(|_| Ok(()));
        let receipt = emitter
            .emit(Value::string("event"))
            .expect("event should be emitted");
        assert_eq!(receipt.sequence(), 1);
        assert_eq!(emitter.emitted_events(), 1);
    }
}
