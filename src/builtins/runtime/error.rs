use std::fmt;

/// Result type used by the generic runtime support modules.
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

/// Structured error returned by the generic runtime support modules.
///
/// The core VM currently exposes `VmError::HostError` as the extension point for host failures.
/// Runtime code keeps the stable category and fields until the parent wiring maps it into that
/// existing VM error variant.
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

#[cfg(test)]
mod tests {
    use super::{RuntimeError, RuntimeErrorCode};

    #[test]
    fn structured_error_preserves_code_and_fields() {
        let error = RuntimeError::new(
            RuntimeErrorCode::EventPayloadTooLarge,
            "stream::emit",
            "event payload exceeds the configured bound",
        )
        .with_limit(32)
        .with_value(64);

        assert_eq!(error.code(), RuntimeErrorCode::EventPayloadTooLarge);
        assert_eq!(error.operation(), "stream::emit");
        assert_eq!(error.limit(), Some(32));
        assert_eq!(error.value(), Some(64));
        assert!(error.to_string().contains("event_payload_too_large"));
    }
}
