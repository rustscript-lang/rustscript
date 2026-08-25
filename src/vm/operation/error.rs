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
    /// The operation configuration was invalid (zero capacity, etc).
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
