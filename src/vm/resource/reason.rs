//! Generic, host-agnostic lifecycle reasons for closing VM resources.
//!
//! This mirrors the runtime cancellation-reason vocabulary but stays in the
//! resource domain so no builtin or domain type leaks into this support
//! module. The variants are stable and machine-readable; later layers (e.g.
//! the operation registry) map them onto their own lifecycle semantics.

use std::fmt;

/// Numeric, stable reason a resource is being closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum ResourceCloseReason {
    Requested = 1,
    Deadline = 2,
    VmReset = 3,
    Parent = 4,
    ResourceClosed = 5,
}

impl ResourceCloseReason {
    /// Stable string form used for machine-readable messages / logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Deadline => "deadline",
            Self::VmReset => "vm_reset",
            Self::Parent => "parent",
            Self::ResourceClosed => "resource_closed",
        }
    }

    /// Decodes a raw numeric reason into a variant, returning `None` for any
    /// encoding that is not one of the stable reason values.
    pub const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::Requested),
            2 => Some(Self::Deadline),
            3 => Some(Self::VmReset),
            4 => Some(Self::Parent),
            5 => Some(Self::ResourceClosed),
            _ => None,
        }
    }

    /// The raw numeric encoding, for machine-readable payloads.
    pub const fn raw(self) -> u8 {
        self as u8
    }
}

impl fmt::Display for ResourceCloseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::ResourceCloseReason;

    #[test]
    fn reasons_cover_lifecycle_vocabulary_with_raw_and_string_round_trip() {
        for (reason, raw, text) in [
            (ResourceCloseReason::Requested, 1u8, "requested"),
            (ResourceCloseReason::Deadline, 2, "deadline"),
            (ResourceCloseReason::VmReset, 3, "vm_reset"),
            (ResourceCloseReason::Parent, 4, "parent"),
            (ResourceCloseReason::ResourceClosed, 5, "resource_closed"),
        ] {
            assert_eq!(reason.raw(), raw, "raw encoding of {reason:?}");
            assert_eq!(
                ResourceCloseReason::from_raw(raw),
                Some(reason),
                "decoding raw {raw}"
            );
            assert_eq!(
                ResourceCloseReason::from_raw(reason.raw()),
                Some(reason),
                "raw round-trip for {reason:?}"
            );
            assert_eq!(reason.as_str(), text, "string form of {reason:?}");
            assert_eq!(reason.to_string(), text, "Display matches string form");
        }
        // Unknown encodings decode to None.
        assert!(ResourceCloseReason::from_raw(0).is_none());
        assert!(ResourceCloseReason::from_raw(6).is_none());
        assert!(ResourceCloseReason::from_raw(u8::MAX).is_none());
    }
}

/// Architecture guard: the resource support modules must stay free of
/// `crate::builtins` (and comment-only noise) so they can be reused without
/// pulling in the core crate's builtin registry.
#[cfg(test)]
mod architecture_tests {
    /// Removes `//` line comments (including `//!` / `///`) and `/* ... */`
    /// block comments so the guard only inspects real code, not doc text.
    fn strip_comments(source: &str) -> String {
        let mut out = String::new();
        let bytes = source.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index..].starts_with(b"//") {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            } else if bytes[index..].starts_with(b"/*") {
                index += 2;
                while index < bytes.len() && !bytes[index..].starts_with(b"*/") {
                    index += 1;
                }
                index += 2;
            } else {
                out.push(bytes[index] as char);
                index += 1;
            }
        }
        out
    }

    /// Built via `join` so the guard never matches its own source.
    fn forbidden() -> String {
        ["crate", "::builtins"].join("")
    }

    #[test]
    fn support_modules_reject_builtins_in_both_sources() {
        let forbidden = forbidden();
        for source in [include_str!("error.rs"), include_str!("reason.rs")] {
            let code = strip_comments(source);
            assert!(
                !code.contains(&forbidden),
                "resource support modules must stay decoupled from the core crate: found `{forbidden}`"
            );
        }
    }
}
