//! Structured runtime error types shared by the invocation stream.
//!
//! A [`RuntimeError`] carries a stable machine-readable [`RuntimeErrorCode`],
//! the offending builtin operation name, and optional numeric limit/value
//! fields. The invocation stream preserves these instead of flattening them
//! to a string, so an embedding can branch on the code and inspect the
//! numeric state (payload bytes, depth) without string matching.

use std::fmt;

/// Result alias used by runtime builtin surfaces.
pub type RuntimeResult<T> = Result<T, RuntimeError>;

/// Stable machine-readable runtime error codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeErrorCode {
    InvalidConfiguration,
    EventPayloadTooLarge,
    EventDepthExceeded,
    ResourceLimitExceeded,
    InvalidResourceHandle,
    ResourceHandleWrongTable,
    OperationFailed,
    OperationAlreadyTerminal,
    OperationCancelled,
    SyncResourceUnavailable,
    CloseFailed,
}

impl RuntimeErrorCode {
    /// Stable snake_case string form, used for transport and tests.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidConfiguration => "invalid_configuration",
            Self::EventPayloadTooLarge => "event_payload_too_large",
            Self::EventDepthExceeded => "event_depth_exceeded",
            Self::ResourceLimitExceeded => "resource_limit_exceeded",
            Self::InvalidResourceHandle => "invalid_resource_handle",
            Self::ResourceHandleWrongTable => "resource_handle_wrong_table",
            Self::OperationFailed => "operation_failed",
            Self::OperationAlreadyTerminal => "operation_already_terminal",
            Self::OperationCancelled => "operation_cancelled",
            Self::SyncResourceUnavailable => "sync_resource_unavailable",
            Self::CloseFailed => "close_failed",
        }
    }
}

/// A structured runtime error with a stable code and optional numeric state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeError {
    code: RuntimeErrorCode,
    operation: String,
    message: String,
    limit: Option<u64>,
    value: Option<u64>,
}

impl RuntimeError {
    pub fn new(code: RuntimeErrorCode, operation: &str, message: impl Into<String>) -> Self {
        Self {
            code,
            operation: operation.to_string(),
            message: message.into(),
            limit: None,
            value: None,
        }
    }

    /// Attaches the configured bound that was violated.
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit as u64);
        self
    }

    /// Attaches the offending value (for example the measured payload size).
    pub fn with_value(mut self, value: usize) -> Self {
        self.value = Some(value as u64);
        self
    }

    pub fn code(&self) -> RuntimeErrorCode {
        self.code
    }

    pub fn operation(&self) -> &str {
        &self.operation
    }

    pub fn limit(&self) -> Option<u64> {
        self.limit
    }

    pub fn value(&self) -> Option<u64> {
        self.value
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)?;
        if let Some(limit) = self.limit {
            write!(f, " (limit {limit})")?;
        }
        if let Some(value) = self.value {
            write!(f, " (value {value})")?;
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
