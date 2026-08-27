//! Host-agnostic, typed resource errors.
//!
//! Carries a stable machine-readable category, the operation name, and an
//! optional limit/value payload. The raw resource handle can be stored in
//! [`ResourceError::value`] when a particular handle is implicated in a
//! failure.
//!
//! This module stays in the resource domain on purpose: no builtin or domain
//! type is referenced here, so it can be reused by the resource table, host
//! resource adapters, and later resource-facing VM layers without pulling in
//! the core crate's builtin registry.

use std::fmt;

/// Result type used by the generic resource modules.
pub type ResourceResult<T> = Result<T, ResourceError>;

/// Stable, machine-readable categories for resource capability failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ResourceErrorCode {
    /// The resource configuration was invalid (e.g. a zero or oversized
    /// capacity).
    InvalidConfiguration,
    /// The configured resource capacity for the scope was reached.
    ResourceLimitExceeded,
    /// A raw handle token did not parse into a valid resource handle.
    InvalidResourceHandle,
    /// A handle was valid but belonged to a different table (arena).
    ResourceHandleWrongTable,
    /// A resource token named a concrete type that did not match the live
    /// resource's actual type.
    ResourceTypeMismatch,
    /// A declared catalog resource key did not match the live/concrete key.
    ResourceTypeKeyMismatch,
    /// A handle referred to a slot generation that had moved on (stale).
    ResourceStale,
    /// The resource was already closed or is in the middle of closing.
    ResourceAlreadyClosed,
    /// The resource identity space (slots, generations, arenas) is exhausted.
    ResourceIdExhausted,
    /// A resource slot is already borrowed by an active guard.
    ResourceAccessConflict,
    /// The [`ResourceTable`](crate::vm::resource::table::ResourceTable)
    /// process-unique arena identity space is exhausted: no new table can be
    /// constructed because the bounded arena id space has been fully handed
    /// out.
    ///
    /// This is the typed, stable discriminator for ResourceTable arena-ID
    /// identity exhaustion and is deliberately distinct from
    /// [`ResourceIdExhausted`](Self::ResourceIdExhausted), which keeps covering
    /// ordinary resource slot/id exhaustion inside an existing table.
    ResourceTableArenaExhausted,
    /// Best-effort cleanup of a closing resource reported a failure.
    ResourceCleanupFailed,
    /// `poll_close` was called on a resource that is not in the closing state.
    ResourceNotClosing,
    /// A close-all sweep is already in progress and a conflicting reason was
    /// supplied; the in-flight sweep keeps its original reason.
    ResourceCloseInProgress,
    /// A best-effort synchronous close-all could not drive every resource to
    /// quiescence (at least one remains pending) and so must not claim
    /// success.
    ResourceClosePending,
}

impl ResourceErrorCode {
    /// Stable string form for machine-readable messages / logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidConfiguration => "invalid_configuration",
            Self::ResourceLimitExceeded => "resource_limit_exceeded",
            Self::InvalidResourceHandle => "invalid_resource_handle",
            Self::ResourceHandleWrongTable => "resource_handle_wrong_table",
            Self::ResourceTypeMismatch => "resource_type_mismatch",
            Self::ResourceTypeKeyMismatch => "resource_type_key_mismatch",
            Self::ResourceStale => "resource_stale",
            Self::ResourceAlreadyClosed => "resource_already_closed",
            Self::ResourceIdExhausted => "resource_id_exhausted",
            Self::ResourceAccessConflict => "resource_access_conflict",
            Self::ResourceTableArenaExhausted => "resource_arena_id_exhausted",
            Self::ResourceCleanupFailed => "resource_cleanup_failed",
            Self::ResourceNotClosing => "resource_not_closing",
            Self::ResourceCloseInProgress => "resource_close_in_progress",
            Self::ResourceClosePending => "resource_close_pending",
        }
    }
}

/// A structured, human- and machine-readable resource error.
///
/// `code` is the stable machine category, `operation` is the VM scope name the
/// failure occurred in, and `limit` / `value` are optional numeric payloads
/// (e.g. the capacity reached and the offending handle's raw token).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceError {
    code: ResourceErrorCode,
    operation: &'static str,
    message: String,
    limit: Option<usize>,
    value: Option<u64>,
}

impl ResourceError {
    /// Builds a resource error without an optional numeric payload.
    pub fn new(
        code: ResourceErrorCode,
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
    pub fn code(&self) -> ResourceErrorCode {
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

    /// The optional capacity/limit payload, if one was attached.
    pub fn limit(&self) -> Option<usize> {
        self.limit
    }

    /// The optional numeric payload, when a value is implicated.
    pub fn value(&self) -> Option<u64> {
        self.value
    }

    /// Attaches an optional capacity/limit payload.
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Attaches an optional numeric value payload.
    pub fn with_value(mut self, value: u64) -> Self {
        self.value = Some(value);
        self
    }
}

impl fmt::Display for ResourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "resource error [{}] in {}: {}",
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

impl std::error::Error for ResourceError {}
