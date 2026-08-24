//! Host-agnostic operation errors.
//!
//! Carries a stable machine-readable category, the operation scope
//! name, and optional limit/value payloads (e.g. the pending
//! capacity reached and the offending operation id).

use std::fmt;

/// Result alias used by the generic operation modules.
pub type OperationResult<T> = Result<T, OperationError>;

/// Stable, machine-readable categories for operation capability failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationErrorCode {
    /// The operation configuration was invalid (zero capacity, bad class, etc).
    InvalidConfiguration,
    /// The configured pending-operation ceiling was reached.
    OperationLimitExceeded,
    /// A raw operation id did not parse into a valid operation handle.
    InvalidOperationId,
    /// A handle was valid but referred to a different operation registry.
    OperationWrongRegistry,
    /// The operation id referred to a generation that had moved on.
    OperationStale,
    /// The requested operation does not exist in this registry.
    OperationNotFound,
    /// The operation is currently pending.
    OperationPending,
    /// The operation exists, but has already reached a terminal status.
    OperationNotPending,
    /// The operation id space was exhausted.
    OperationIdExhausted,
    /// The process-unique operation-registry tag space was exhausted.
    OperationRegistryTagExhausted,
    /// A cleanup hook failed after the operation's terminal transition.
    OperationCleanupFailed,
    /// The registry is sealed and rejects the start of new operations.
    OperationRegistrySealed,
    /// A driver poll or cancellation action failed.
    OperationDriverFailed,
}

impl OperationErrorCode {
    /// Stable snake_case string for logs and machine use.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidConfiguration => "invalid_configuration",
            Self::OperationLimitExceeded => "operation_limit_exceeded",
            Self::InvalidOperationId => "invalid_operation_id",
            Self::OperationWrongRegistry => "operation_wrong_registry",
            Self::OperationStale => "operation_stale",
            Self::OperationNotFound => "operation_not_found",
            Self::OperationPending => "operation_pending",
            Self::OperationNotPending => "operation_not_pending",
            Self::OperationIdExhausted => "operation_id_exhausted",
            Self::OperationRegistryTagExhausted => "operation_registry_tag_exhausted",
            Self::OperationCleanupFailed => "operation_cleanup_failed",
            Self::OperationRegistrySealed => "operation_registry_sealed",
            Self::OperationDriverFailed => "operation_driver_failed",
        }
    }
}

/// A structured, human- and machine-readable operation error.
///
/// `code` is the stable category, `operation` is the VM scope the failure
/// occurred in, and `limit`/`value` carry optional numeric payloads (e.g.
/// the pending ceiling and the offending raw operation id).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationError {
    code: OperationErrorCode,
    operation: &'static str,
    message: String,
    limit: Option<u64>,
    value: Option<u64>,
}

impl OperationError {
    /// Builds an operation error without an optional payload.
    pub fn new(
        code: OperationErrorCode,
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

    /// The stable machine-readable category.
    pub fn code(&self) -> OperationErrorCode {
        self.code
    }

    /// The operation scope this error occurred in.
    pub fn operation(&self) -> &'static str {
        self.operation
    }

    /// The human-readable detail message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The optional capacity/limit payload, when one is attached.
    pub fn limit(&self) -> Option<u64> {
        self.limit
    }

    /// The optional numeric value payload, when set.
    pub fn value(&self) -> Option<u64> {
        self.value
    }

    /// Attaches a numeric limit payload.
    pub fn with_limit(mut self, limit: u64) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Attaches a numeric value payload.
    pub fn with_value(mut self, value: u64) -> Self {
        self.value = Some(value);
        self
    }
}

impl fmt::Display for OperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "operation error [{}] in {}: {}",
            self.code.as_str(),
            self.operation,
            self.message
        )?;
        if let Some(limit) = self.limit {
            write!(f, " (limit: {limit})")?;
        }
        if let Some(value) = self.value {
            write!(f, " (value: {value})")?;
        }
        Ok(())
    }
}

impl std::error::Error for OperationError {}

#[cfg(test)]
mod tests {
    use super::{OperationError, OperationErrorCode};

    #[test]
    fn every_code_has_a_stable_unique_snake_case_name() {
        let expected = [
            (
                OperationErrorCode::InvalidConfiguration,
                "invalid_configuration",
            ),
            (
                OperationErrorCode::OperationLimitExceeded,
                "operation_limit_exceeded",
            ),
            (
                OperationErrorCode::InvalidOperationId,
                "invalid_operation_id",
            ),
            (
                OperationErrorCode::OperationWrongRegistry,
                "operation_wrong_registry",
            ),
            (OperationErrorCode::OperationStale, "operation_stale"),
            (OperationErrorCode::OperationNotFound, "operation_not_found"),
            (OperationErrorCode::OperationPending, "operation_pending"),
            (
                OperationErrorCode::OperationNotPending,
                "operation_not_pending",
            ),
            (
                OperationErrorCode::OperationIdExhausted,
                "operation_id_exhausted",
            ),
            (
                OperationErrorCode::OperationRegistryTagExhausted,
                "operation_registry_tag_exhausted",
            ),
            (
                OperationErrorCode::OperationCleanupFailed,
                "operation_cleanup_failed",
            ),
            (
                OperationErrorCode::OperationRegistrySealed,
                "operation_registry_sealed",
            ),
            (
                OperationErrorCode::OperationDriverFailed,
                "operation_driver_failed",
            ),
        ];

        let mut seen: Vec<&str> = expected.iter().map(|(_, s)| *s).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            expected.len(),
            "every code must be unique and non-empty"
        );
        assert!(seen.iter().all(|s| !s.is_empty()));
        for (code, expected_str) in expected {
            assert_eq!(code.as_str(), expected_str, "stable string for {code:?}");
        }
    }

    #[test]
    fn limit_and_value_payloads_are_optional() {
        let base = OperationError::new(
            OperationErrorCode::OperationLimitExceeded,
            "vm::operation",
            "pending ceiling reached",
        );
        assert_eq!(base.limit(), None);
        assert_eq!(base.value(), None);
        let attached = base.with_limit(32).with_value(64);
        assert_eq!(attached.limit(), Some(32));
        assert_eq!(attached.value(), Some(64));
    }

    #[test]
    fn display_only_renders_attached_payloads() {
        let full = OperationError::new(
            OperationErrorCode::OperationDriverFailed,
            "operation::driver",
            "driver reported a failure",
        )
        .with_limit(32)
        .with_value(64);
        let text = full.to_string();
        assert!(text.contains("operation_driver_failed"));
        assert!(text.contains("operation::driver"));
        assert!(text.contains("driver reported a failure"));
        assert!(text.contains("limit: 32"));
        assert!(text.contains("value: 64"));

        let plain = OperationError::new(
            OperationErrorCode::OperationStale,
            "operation::table",
            "stale slot",
        );
        let plain_text = plain.to_string();
        assert!(!plain_text.contains("limit:"));
        assert!(!plain_text.contains("value:"));
    }

    #[test]
    fn error_trait_is_implemented() {
        let error = OperationError::new(
            OperationErrorCode::OperationCleanupFailed,
            "operation::table",
            "cleanup reported a failure",
        );
        assert!(std::error::Error::source(&error).is_none());
        let boxed: Box<dyn std::error::Error> = Box::new(error.clone());
        assert!(boxed.to_string().contains("operation_cleanup_failed"));
        let restored = boxed.downcast::<OperationError>().expect("downcast");
        assert_eq!(*restored, error);
    }
}
