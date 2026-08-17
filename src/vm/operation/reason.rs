//! VM-owned operation cancellation reason.
//!
//! Describes the generic lifecycle of an operation on the VM and the
//! reasons a running operation may be cancelled. This module only
//! covers the *reason* values themselves — the cancellation flow is
//! implemented by the operation executor.

use core::cmp::Ordering;
use core::fmt;
use core::hash;
use core::ops;

/// Reason why a VM-owned operation was cancelled.
///
/// Values are intentionally small and stable — they are persisted as
/// raw bytes in some contexts, so reordering or renumbering is a breaking
/// change.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OperationCancelReason {
    /// The operation was explicitly requested by the caller.
    Requested = 1,
    /// The VM was reset while the operation was in flight.
    Deadline = 2,
    /// The VM was reset while the operation was still pending.
    VmReset = 3,
    /// The parent operation was cancelled/closed first.
    Parent = 4,
    /// A resource the operation depended on was closed.
    ResourceClosed = 5,
}

impl OperationCancelReason {
    /// Raw byte value of this reason.
    #[inline]
    pub const fn raw(self) -> u8 {
        self as u8
    }

    /// Decode from a raw byte.
    ///
    /// Returns `None` for invalid / reserved values (0 and 255 are
    /// explicitly rejected; other unknown values are also rejected).
    pub const fn from_raw(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Requested),
            2 => Some(Self::Deadline),
            3 => Some(Self::VmReset),
            4 => Some(Self::Parent),
            5 => Some(Self::ResourceClosed),
            _ => None,
        }
    }

    /// Stable string form of this reason.
    ///
    /// The returned string is a `'static` str and matches the
    /// variant name in kebab-case exactly.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Deadline => "deadline",
            Self::VmReset => "vm_reset",
            Self::Parent => "parent",
            Self::ResourceClosed => "resource_closed",
        }
    }
}

impl fmt::Display for OperationCancelReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_values_are_stable() {
        assert_eq!(OperationCancelReason::Requested.raw(), 1);
        assert_eq!(OperationCancelReason::Deadline.raw(), 2);
        assert_eq!(OperationCancelReason::VmReset.raw(), 3);
        assert_eq!(OperationCancelReason::Parent.raw(), 4);
        assert_eq!(OperationCancelReason::ResourceClosed.raw(), 5);
    }

    #[test]
    fn from_raw_accepts_valid_values() {
        assert_eq!(
            OperationCancelReason::from_raw(1),
            Some(OperationCancelReason::Requested)
        );
        assert_eq!(
            OperationCancelReason::from_raw(2),
            Some(OperationCancelReason::Deadline)
        );
        assert_eq!(
            OperationCancelReason::from_raw(3),
            Some(OperationCancelReason::VmReset)
        );
        assert_eq!(
            OperationCancelReason::from_raw(4),
            Some(OperationCancelReason::Parent)
        );
        assert_eq!(
            OperationCancelReason::from_raw(5),
            Some(OperationCancelReason::ResourceClosed)
        );
    }

    #[test]
    fn from_raw_rejects_invalid_values() {
        // 0 and 255 are explicitly reserved; anything else unknown
        // is rejected too.
        assert_eq!(OperationCancelReason::from_raw(0), None);
        assert_eq!(OperationCancelReason::from_raw(255), None);
        assert_eq!(OperationCancelReason::from_raw(6), None);
        assert_eq!(OperationCancelReason::from_raw(u8::MAX), None);
    }

    #[test]
    fn as_str_matches_exact_kebab_case() {
        assert_eq!(OperationCancelReason::Requested.as_str(), "requested");
        assert_eq!(OperationCancelReason::Deadline.as_str(), "deadline");
        assert_eq!(OperationCancelReason::VmReset.as_str(), "vm_reset");
        assert_eq!(OperationCancelReason::Parent.as_str(), "parent");
        assert_eq!(
            OperationCancelReason::ResourceClosed.as_str(),
            "resource_closed"
        );
    }

    #[test]
    fn display_matches_as_str() {
        let cases = [
            (OperationCancelReason::Requested, "requested"),
            (OperationCancelReason::Deadline, "deadline"),
            (OperationCancelReason::VmReset, "vm_reset"),
            (OperationCancelReason::Parent, "parent"),
            (OperationCancelReason::ResourceClosed, "resource_closed"),
        ];
        for (reason, expected) in cases {
            assert_eq!(reason.to_string(), expected);
        }
    }
}
