//! Adapter-independent runtime values used by the VM execution boundary.
//!
//! This module owns invocation-stream limits, event payload validation, and
//! structured runtime errors. Concrete builtin adapters may re-export these
//! types, but the VM does not depend on any adapter module for them.

use std::fmt;

use crate::vm::Value;

/// Result type used by generic runtime support modules.
pub type RuntimeResult<T> = Result<T, RuntimeError>;

/// Stable machine-readable categories for runtime capability failures.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeErrorCode {
    InvalidConfiguration,
    EventPayloadTooLarge,
    EventDepthExceeded,
    ResourceLimitExceeded,
    InvalidResourceHandle,
    ResourceHandleWrongTable,
    ResourceTypeMismatch,
    ResourceStale,
    ResourceAlreadyClosed,
    ResourceIdExhausted,
    ResourceCleanupFailed,
    OperationLimitExceeded,
    OperationNotFound,
    OperationAlreadyFinished,
    OperationCancelled,
    OperationFailed,
    OperationIdExhausted,
    OperationCleanupFailed,
}

impl RuntimeErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidConfiguration => "invalid_configuration",
            Self::EventPayloadTooLarge => "event_payload_too_large",
            Self::EventDepthExceeded => "event_depth_exceeded",
            Self::ResourceLimitExceeded => "resource_limit_exceeded",
            Self::InvalidResourceHandle => "invalid_resource_handle",
            Self::ResourceHandleWrongTable => "resource_handle_wrong_table",
            Self::ResourceTypeMismatch => "resource_type_mismatch",
            Self::ResourceStale => "resource_stale",
            Self::ResourceAlreadyClosed => "resource_already_closed",
            Self::ResourceIdExhausted => "resource_id_exhausted",
            Self::ResourceCleanupFailed => "resource_cleanup_failed",
            Self::OperationLimitExceeded => "operation_limit_exceeded",
            Self::OperationNotFound => "operation_not_found",
            Self::OperationAlreadyFinished => "operation_already_finished",
            Self::OperationCancelled => "operation_cancelled",
            Self::OperationFailed => "operation_failed",
            Self::OperationIdExhausted => "operation_id_exhausted",
            Self::OperationCleanupFailed => "operation_cleanup_failed",
        }
    }
}

/// Structured error returned by generic runtime support modules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeError {
    code: RuntimeErrorCode,
    operation: &'static str,
    message: String,
    limit: Option<usize>,
    value: Option<u64>,
}

#[allow(dead_code)]
impl RuntimeError {
    pub fn new(
        code: RuntimeErrorCode,
        operation: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            operation,
            message: message.into(),
            limit: None,
            value: None,
        }
    }

    pub fn code(&self) -> RuntimeErrorCode {
        self.code
    }

    pub fn operation(&self) -> &'static str {
        self.operation
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn limit(&self) -> Option<usize> {
        self.limit
    }

    pub fn value(&self) -> Option<u64> {
        self.value
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    pub fn with_value(mut self, value: u64) -> Self {
        self.value = Some(value);
        self
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "runtime error [{}] in {}: {}",
            self.code.as_str(),
            self.operation,
            self.message
        )?;
        if let Some(limit) = self.limit {
            write!(formatter, " (limit: {limit})")?;
        }
        if let Some(value) = self.value {
            write!(formatter, " (value: {value})")?;
        }
        Ok(())
    }
}

impl std::error::Error for RuntimeError {}

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
    pub fn try_new(value: Value, limits: EventLimits) -> RuntimeResult<Self> {
        let size_bytes = estimate_value_size(&value, limits)?;
        Ok(Self { value, size_bytes })
    }

    #[allow(dead_code)]
    pub fn size_bytes(&self) -> usize {
        self.size_bytes
    }

    pub fn into_value(self) -> Value {
        self.value
    }
}

/// Estimates the bounded representation size of a value.
pub fn estimate_value_size(value: &Value, limits: EventLimits) -> RuntimeResult<usize> {
    measure_value(value, 0, limits)
}

fn measure_value(value: &Value, depth: usize, limits: EventLimits) -> RuntimeResult<usize> {
    if depth > limits.max_depth {
        return Err(RuntimeError::new(
            RuntimeErrorCode::EventDepthExceeded,
            "stream::emit",
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
            "stream::emit",
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
        .with_value(total as u64));
    }
    Ok(total)
}

/// Stable source name used by the stream event host function.
pub const STREAM_EMIT_NAME: &str = "stream::emit";

/// Configuration for one VM/run-scoped invocation stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeContextConfig {
    event_limits: EventLimits,
}

impl RuntimeContextConfig {
    pub const fn new(event_limits: EventLimits) -> Self {
        Self { event_limits }
    }

    pub const fn event_limits(self) -> EventLimits {
        self.event_limits
    }
}

impl Default for RuntimeContextConfig {
    fn default() -> Self {
        Self::new(EventLimits::default())
    }
}

/// Run-scoped invocation stream configuration.
pub struct RuntimeContext {
    event_limits: EventLimits,
}

#[allow(dead_code)]
impl RuntimeContext {
    pub fn with_config(config: RuntimeContextConfig) -> RuntimeResult<Self> {
        Ok(Self {
            event_limits: config.event_limits(),
        })
    }

    pub fn config(&self) -> RuntimeContextConfig {
        RuntimeContextConfig::new(self.event_limits)
    }

    pub fn event_limits(&self) -> EventLimits {
        self.event_limits
    }
}

impl Default for RuntimeContext {
    fn default() -> Self {
        Self::with_config(RuntimeContextConfig::default())
            .expect("default runtime context configuration should be valid")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EventLimits, EventPayload, RuntimeContext, RuntimeContextConfig, RuntimeError,
        RuntimeErrorCode, STREAM_EMIT_NAME,
    };
    use crate::vm::Value;

    #[test]
    fn runtime_context_and_error_api_are_neutral() {
        assert_eq!(STREAM_EMIT_NAME, "stream::emit");
        assert!(std::mem::size_of::<RuntimeContext>() > 0);
        let error = RuntimeError::new(
            RuntimeErrorCode::EventPayloadTooLarge,
            STREAM_EMIT_NAME,
            "payload exceeds limit",
        )
        .with_limit(32)
        .with_value(64);
        assert_eq!(error.code(), RuntimeErrorCode::EventPayloadTooLarge);
        assert_eq!(error.limit(), Some(32));
        assert_eq!(error.value(), Some(64));
        assert!(error.to_string().contains("event_payload_too_large"));
    }

    #[test]
    fn per_item_event_limits_validate_payload_and_depth() {
        let limits = EventLimits::new(32, 4).expect("limits should be valid");
        let context = RuntimeContext::with_config(RuntimeContextConfig::new(limits))
            .expect("context should be constructible");
        assert_eq!(context.event_limits(), limits);
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
        assert_eq!(too_large.code(), RuntimeErrorCode::EventPayloadTooLarge);
        let too_deep = EventPayload::try_new(
            Value::array(vec![Value::array(vec![Value::array(vec![Value::Int(1)])])]),
            limits,
        )
        .expect_err("too-deep event should be rejected");
        assert_eq!(too_deep.code(), RuntimeErrorCode::EventDepthExceeded);
    }
}
