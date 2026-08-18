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
    /// The guest released its ownership of the resource; the release launches
    /// the close exactly once.
    OwnershipRelease = 6,
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
            Self::OwnershipRelease => "ownership_release",
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
            6 => Some(Self::OwnershipRelease),
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
            (
                ResourceCloseReason::OwnershipRelease,
                6,
                "ownership_release",
            ),
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
        assert!(ResourceCloseReason::from_raw(7).is_none());
        assert!(ResourceCloseReason::from_raw(u8::MAX).is_none());
    }
}

/// Architecture guard: the resource support modules must stay free of
/// `crate::builtins` (and comment-only noise) so they can be reused without
/// pulling in the core crate's builtin registry. The scan is dynamic: every
/// production `.rs` file directly under `src/vm/resource/` is enumerated at
/// test time, so any future module is covered automatically without editing
/// this test.
#[cfg(test)]
mod architecture_tests {
    use std::fs;
    use std::path::PathBuf;

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
    fn forbidden_builtins() -> String {
        ["crate", "::builtins"].join("")
    }

    /// Any remaining direct reference to a builtin registry entry.
    fn forbidden_builtins_path() -> String {
        ["::", "builtins", "::"].join("")
    }

    /// Every production `.rs` file directly under `src/vm/resource`.
    fn production_sources() -> Vec<PathBuf> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/vm/resource");
        let mut files: Vec<PathBuf> = fs::read_dir(&dir)
            .expect("src/vm/resource must exist")
            .map(|entry| entry.expect("readable directory entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
            .collect();
        files.sort();
        files
    }

    #[test]
    fn resource_production_sources_reject_core_and_domain_imports() {
        let sources = production_sources();
        assert!(
            !sources.is_empty(),
            "dynamic enumeration must find production sources under src/vm/resource"
        );
        let forbidden = [forbidden_builtins(), forbidden_builtins_path()];
        for path in &sources {
            let source = fs::read_to_string(path).expect("read production source");
            let code = strip_comments(&source);
            for needle in &forbidden {
                assert!(
                    !code.contains(needle),
                    "{} must stay decoupled from the core crate builtin registry / domain modules: found `{needle}`",
                    path.display(),
                );
            }
            // Explicit rusqlite (or any external domain resource) coupling is
            // forbidden; this module family must stay host- and domain-agnostic.
            // Built via join so the guarded token cannot accidentally appear in
            // this very test's source.
            let external_domain = ["rus", "qlite"].join("");
            assert!(
                !code.contains(&external_domain),
                "{} must not import an external domain dependency",
                path.display(),
            );
        }
    }

    #[test]
    fn resource_production_sources_never_make_unchecked_typed_construction_public() {
        // `Resource::from_handle` is a safe, unchecked typed constructor. It
        // must stay crate-private: public host recovery goes through the
        // validated `ResourceTable::typed`. This asserts the public-surface
        // boundary at the source level.
        let handle_src = {
            let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/vm/resource");
            fs::read_to_string(dir.join("handle.rs")).expect("read handle.rs")
        };
        let code = strip_comments(&handle_src);
        assert!(
            !code.contains("pub fn from_handle"),
            "Resource::from_handle must not be public safe arbitrary-type construction"
        );
        assert!(
            code.contains("pub(crate) fn from_handle"),
            "Resource::from_handle must be crate-private"
        );
    }
}
