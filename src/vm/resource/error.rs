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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResourceErrorCode {
    /// The resource configuration was invalid (e.g. a zero or oversized
    /// capacity, or an invalid resource class).
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
    /// A handle referred to a slot generation that had moved on (stale).
    ResourceStale,
    /// The resource was already closed or is in the middle of closing.
    ResourceAlreadyClosed,
    /// A resource with live children cannot be closed yet.
    ResourceHasChildren,
    /// The resource identity space (slots, generations, arenas) is exhausted.
    ResourceIdExhausted,
    /// Best-effort cleanup of a closing resource reported a failure.
    ResourceCleanupFailed,
    /// `poll_close` was called on a resource that is not in the closing state.
    ResourceNotClosing,
    /// A close-all sweep is already in progress and a conflicting reason was
    /// supplied; the in-flight sweep keeps its original reason.
    ResourceCloseInProgress,
    /// A best-effort synchronous close-all could not drive every resource to
    /// quiescence (at least one remains pending) and so must not claim success.
    ResourceClosePending,
    /// A guest-ownership operation required a guest-owned resource, but the
    /// resource is still host-owned.
    ResourceNotGuestOwned,
    /// A guest-ownership mark required a host-owned resource, but the
    /// resource was already marked guest-owned (duplicate mark).
    ResourceNotHostOwned,
    /// The resource's concrete value was already taken out of the table by an
    /// ownership take; the raw handle is stale.
    ResourceAlreadyTaken,
    /// The catalog/resource declaration key did not match the live slot.
    ResourceKeyMismatch,
    /// No key was declared for a request that requires exact resource identity.
    ResourceKeyUnavailable,
    /// Two resource parameters requested an illegal aliasing combination.
    ResourceAccessConflict,
    /// An associated operation prevents an ownership take.
    ResourceOperationActive,
    /// A non-resource Value/ToOwned mode was supplied to the resource frame.
    ResourceAccessModeUnsupported,
    /// A declared TakeOwned argument was not consumed by the callee and had
    /// to be reclaimed by the exact host-call contract.
    ResourceNotConsumed,
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
            Self::ResourceStale => "resource_stale",
            Self::ResourceAlreadyClosed => "resource_already_closed",
            Self::ResourceHasChildren => "resource_has_children",
            Self::ResourceIdExhausted => "resource_id_exhausted",
            Self::ResourceCleanupFailed => "resource_cleanup_failed",
            Self::ResourceNotClosing => "resource_not_closing",
            Self::ResourceCloseInProgress => "resource_close_in_progress",
            Self::ResourceClosePending => "resource_close_pending",
            Self::ResourceNotGuestOwned => "resource_not_guest_owned",
            Self::ResourceNotHostOwned => "resource_not_host_owned",
            Self::ResourceAlreadyTaken => "resource_already_taken",
            Self::ResourceKeyMismatch => "resource_key_mismatch",
            Self::ResourceKeyUnavailable => "resource_key_unavailable",
            Self::ResourceAccessConflict => "resource_access_conflict",
            Self::ResourceOperationActive => "resource_operation_active",
            Self::ResourceAccessModeUnsupported => "resource_access_mode_unsupported",
            Self::ResourceNotConsumed => "resource_not_consumed",
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

#[cfg(test)]
mod tests {
    use super::{ResourceError, ResourceErrorCode};

    fn all_codes() -> Vec<ResourceErrorCode> {
        vec![
            ResourceErrorCode::InvalidConfiguration,
            ResourceErrorCode::ResourceLimitExceeded,
            ResourceErrorCode::InvalidResourceHandle,
            ResourceErrorCode::ResourceHandleWrongTable,
            ResourceErrorCode::ResourceTypeMismatch,
            ResourceErrorCode::ResourceStale,
            ResourceErrorCode::ResourceAlreadyClosed,
            ResourceErrorCode::ResourceHasChildren,
            ResourceErrorCode::ResourceIdExhausted,
            ResourceErrorCode::ResourceCleanupFailed,
            ResourceErrorCode::ResourceNotClosing,
            ResourceErrorCode::ResourceCloseInProgress,
            ResourceErrorCode::ResourceClosePending,
            ResourceErrorCode::ResourceNotGuestOwned,
            ResourceErrorCode::ResourceNotHostOwned,
            ResourceErrorCode::ResourceAlreadyTaken,
            ResourceErrorCode::ResourceKeyMismatch,
            ResourceErrorCode::ResourceKeyUnavailable,
            ResourceErrorCode::ResourceAccessConflict,
            ResourceErrorCode::ResourceOperationActive,
            ResourceErrorCode::ResourceAccessModeUnsupported,
            ResourceErrorCode::ResourceNotConsumed,
        ]
    }

    #[test]
    fn stable_str_mapping_cover_every_code_without_duplicates() {
        let expected = [
            (
                ResourceErrorCode::InvalidConfiguration,
                "invalid_configuration",
            ),
            (
                ResourceErrorCode::ResourceLimitExceeded,
                "resource_limit_exceeded",
            ),
            (
                ResourceErrorCode::InvalidResourceHandle,
                "invalid_resource_handle",
            ),
            (
                ResourceErrorCode::ResourceHandleWrongTable,
                "resource_handle_wrong_table",
            ),
            (
                ResourceErrorCode::ResourceTypeMismatch,
                "resource_type_mismatch",
            ),
            (ResourceErrorCode::ResourceStale, "resource_stale"),
            (
                ResourceErrorCode::ResourceAlreadyClosed,
                "resource_already_closed",
            ),
            (
                ResourceErrorCode::ResourceHasChildren,
                "resource_has_children",
            ),
            (
                ResourceErrorCode::ResourceIdExhausted,
                "resource_id_exhausted",
            ),
            (
                ResourceErrorCode::ResourceCleanupFailed,
                "resource_cleanup_failed",
            ),
            (
                ResourceErrorCode::ResourceNotClosing,
                "resource_not_closing",
            ),
            (
                ResourceErrorCode::ResourceCloseInProgress,
                "resource_close_in_progress",
            ),
            (
                ResourceErrorCode::ResourceClosePending,
                "resource_close_pending",
            ),
            (
                ResourceErrorCode::ResourceNotGuestOwned,
                "resource_not_guest_owned",
            ),
            (
                ResourceErrorCode::ResourceNotHostOwned,
                "resource_not_host_owned",
            ),
            (
                ResourceErrorCode::ResourceAlreadyTaken,
                "resource_already_taken",
            ),
            (
                ResourceErrorCode::ResourceKeyMismatch,
                "resource_key_mismatch",
            ),
            (
                ResourceErrorCode::ResourceKeyUnavailable,
                "resource_key_unavailable",
            ),
            (
                ResourceErrorCode::ResourceAccessConflict,
                "resource_access_conflict",
            ),
            (
                ResourceErrorCode::ResourceOperationActive,
                "resource_operation_active",
            ),
            (
                ResourceErrorCode::ResourceAccessModeUnsupported,
                "resource_access_mode_unsupported",
            ),
            (
                ResourceErrorCode::ResourceNotConsumed,
                "resource_not_consumed",
            ),
        ];
        // Exhaustive: every code has exactly one stable string mapping.
        assert_eq!(
            expected.len(),
            all_codes().len(),
            "every ResourceErrorCode must have a stable string mapping"
        );
        for (code, expected_str) in expected {
            assert_eq!(code.as_str(), expected_str, "stable string for {code:?}");
        }
        // The mapping must be unique and non-empty across the whole enum.
        let mut strings: Vec<&str> = all_codes().iter().map(|code| code.as_str()).collect();
        strings.sort_unstable();
        strings.dedup();
        assert_eq!(strings.len(), all_codes().len(), "as_str must not collide");
        assert!(strings.iter().all(|s| !s.is_empty()));
    }

    #[test]
    fn limit_and_value_payloads_are_optional() {
        let base = ResourceError::new(
            ResourceErrorCode::InvalidResourceHandle,
            "resource::handle",
            "bad handle",
        );
        assert_eq!(base.limit(), None);
        assert_eq!(base.value(), None);
        let attached = base.with_limit(1024).with_value(42);
        assert_eq!(attached.limit(), Some(1024));
        assert_eq!(attached.value(), Some(42));
    }

    #[test]
    fn display_renders_only_present_payloads() {
        let full = ResourceError::new(
            ResourceErrorCode::ResourceLimitExceeded,
            "resource::push",
            "capacity reached",
        )
        .with_limit(32)
        .with_value(64);
        let shown = full.to_string();
        assert!(shown.contains("resource_limit_exceeded"));
        assert!(shown.contains("resource::push"));
        assert!(shown.contains("capacity reached"));
        assert!(shown.contains("limit: 32"));
        assert!(shown.contains("value: 64"));

        let plain = ResourceError::new(
            ResourceErrorCode::ResourceStale,
            "resource::table",
            "stale slot",
        );
        let plain_shown = plain.to_string();
        assert!(!plain_shown.contains("limit:"));
        assert!(!plain_shown.contains("value:"));
    }

    #[test]
    fn implements_error_trait_with_no_source() {
        let error = ResourceError::new(
            ResourceErrorCode::ResourceCleanupFailed,
            "resource::table",
            "cleanup reported a failure",
        );
        assert!(std::error::Error::source(&error).is_none());
        let boxed: Box<dyn std::error::Error> = Box::new(error.clone());
        assert!(boxed.to_string().contains("resource_cleanup_failed"));
        let restored = boxed.downcast::<ResourceError>().expect("downcast");
        assert_eq!(*restored, error);
    }
}
