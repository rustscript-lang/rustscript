//! Host-agnostic generic operation layer.
//!
//! This module owns the host-agnostic operation lifecycle (status,
//! cancellation, cleanup) for the VM. The concrete driver contract lives
//! in [`driver`], the registry in [`registry`].
//!
//! Key ideas:
//!
//! * **Concrete driver owns poll/cancel** — each in-flight operation is a
//!   [`HostOperation`] that owns its own [`HostOperation::poll`] and
//!   [`HostOperation::cancel`] behaviour; the registry never dispatches on a
//!   host domain.
//! * **Registry owns per-entry reason/status** — the registry records the
//!   first cancellation reason (deadline included) and the terminal status on
//!   each operation entry, forwarding cancellation directly to the owning
//!   driver. There is no standalone cancellation-token graph and no second
//!   cancellation framework.
//! * **Packed, validated, reusable slots** — [`OperationRegistry`] stores
//!   operations in generational slots addressed by a packed registry-tag /
//!   slot-identity / generation [`OperationId`]. Caller-supplied ids are
//!   validated (foreign tag, out-of-range/future slot, or stale generation are
//!   rejected before any status/driver/cleanup mutation) and a released slot
//!   is reused under an incremented generation.
//! * **Optional resource association** — an operation can be tied to a
//!   [`ResourceHandle`](crate::vm::resource::ResourceHandle)
//!   so cancelling that resource cancels the operation.
pub mod driver;
pub mod error;
pub mod id;
pub mod reason;
pub mod registry;

pub use driver::{HostOperation, OperationCleanup, OperationOutcome, OperationSpec};
pub use error::{OperationError, OperationErrorCode, OperationResult};
pub use id::OperationId;
pub use reason::OperationCancelReason;
pub use registry::{
    DEFAULT_MAX_PENDING_OPERATIONS, OperationCancelSummary, OperationRegistry, OperationStatus,
};

#[cfg(test)]
mod architecture_gate {
    //! Recursive, dynamic architecture gate for the operation core.
    //!
    //! Keeps `src/vm/operation` host-domain-agnostic. Every `.rs` file under
    //! the directory (including future nested subdirectories) is read,
    //! comment-stripped, and scanned for identifiers and paths that would
    //! couple the operation core to a concrete host domain (builtins, a
    //! database binding, or the removed cancellation-token / owner APIs).
    //! Newly added files are picked up automatically; the gate is not a fixed
    //! allowlist.

    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Copy one string/char literal verbatim (respecting `\` escapes and the
    /// closing quote) so `//` or `/*` inside a literal are never treated as
    /// comment delimiters. Returns the index just past the closing quote.
    fn copy_string_literal(src: &[u8], mut i: usize, quote: u8, out: &mut Vec<u8>) -> usize {
        out.push(quote);
        i += 1;
        while i < src.len() {
            let c = src[i];
            out.push(c);
            i += 1;
            if c == b'\\' {
                if i < src.len() {
                    out.push(src[i]);
                    i += 1;
                }
            } else if c == quote {
                break;
            }
        }
        i
    }

    /// Strip line, block, and doc comments from `src`, tracking nested
    /// block-comment depth, while preserving comment-free string literals and
    /// newlines (so line alignment stays roughly stable). Returns valid UTF-8
    /// because comment boundaries are ASCII and body bytes are copied intact.
    fn strip_comments(src: &str) -> String {
        let b = src.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0usize;
        let mut in_line = false;
        let mut block_depth = 0usize;
        while i < b.len() {
            let c = b[i];
            let next = b.get(i + 1).copied();
            if in_line {
                if c == b'\n' {
                    out.push(b'\n');
                    in_line = false;
                }
                i += 1;
            } else if block_depth > 0 {
                match (c, next) {
                    (b'/', Some(b'*')) => {
                        block_depth += 1;
                        i += 2;
                    }
                    (b'*', Some(b'/')) => {
                        block_depth -= 1;
                        i += 2;
                    }
                    _ => {
                        if c == b'\n' {
                            out.push(b'\n');
                        }
                        i += 1;
                    }
                }
            } else {
                match (c, next) {
                    (b'/', Some(b'/')) => {
                        in_line = true;
                        i += 2;
                    }
                    (b'/', Some(b'*')) => {
                        block_depth = 1;
                        i += 2;
                    }
                    (b'"', _) => i = copy_string_literal(b, i, b'"', &mut out),
                    (b'\'', _) => i = copy_string_literal(b, i, b'\'', &mut out),
                    _ => {
                        out.push(c);
                        i += 1;
                    }
                }
            }
        }
        String::from_utf8(out).expect("operation source is valid UTF-8")
    }

    /// Recursively enumerate every `.rs` file under `dir` in a stable,
    /// deterministic order (paths sorted at each level).
    fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        for p in paths {
            if p.is_dir() {
                collect_rs_files(&p, out);
            } else if p.extension().is_some_and(|e| e == "rs") {
                out.push(p);
            }
        }
    }

    /// Root of the operation core referenced from the manifest.
    fn operation_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("vm")
            .join("operation")
    }

    /// Forbidden host-domain needles, built through string-segment joining so
    /// the gate source never contains any forbidden needle verbatim (which
    /// would make the gate match its own test file). Each entry is
    /// `(needle, identifier)`; identifier needles are matched only on token
    /// boundaries so longer legal identifiers cannot be false-flagged.
    fn forbidden_needles() -> Vec<(String, bool)> {
        vec![
            (["crate", "::", "builtins"].concat(), false),
            (["::", "builtins", "::"].concat(), false),
            (["rusq", "lite"].concat(), false),
            (["Cancel", "lation", "Token"].concat(), true),
            (["Cancel", "lation", "Reason"].concat(), true),
            (["Operation", "Owner"].concat(), true),
        ]
    }

    fn is_ident_byte(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }

    fn needle_found(code: &str, needle: &str, identifier: bool) -> bool {
        if needle.is_empty() {
            return false;
        }
        let b = code.as_bytes();
        let n = needle.as_bytes();
        if n.len() > b.len() {
            return false;
        }
        let mut i = 0usize;
        while i + n.len() <= b.len() {
            if &b[i..i + n.len()] == n {
                if identifier {
                    let before_ok = i == 0 || !is_ident_byte(b[i - 1]);
                    let after_ok = i + n.len() == b.len() || !is_ident_byte(b[i + n.len()]);
                    if !before_ok || !after_ok {
                        i += 1;
                        continue;
                    }
                }
                return true;
            }
            i += 1;
        }
        false
    }

    /// The enumerator is recursive, nonempty, includes the current core
    /// modules, and does not hard-code a fixed allowlist (the exact full set is
    /// never asserted), so future files remain subject to the gate.
    #[test]
    fn operation_core_enumeration_includes_all_core_modules() {
        let mut files = Vec::new();
        collect_rs_files(&operation_dir(), &mut files);
        assert!(
            !files.is_empty(),
            "operation directory must contain .rs files"
        );
        let names: BTreeSet<String> = files
            .iter()
            .map(|p| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        for required in [
            "id.rs",
            "error.rs",
            "reason.rs",
            "driver.rs",
            "registry.rs",
            "mod.rs",
        ] {
            assert!(
                names.contains(required),
                "recursive enumerator must include core module {required}"
            );
        }
    }

    /// The canonical reason type must never be caught by the removed
    /// `CancellationReason` identifier needle (token-boundary matching).
    #[test]
    fn canonical_cancellation_reason_is_allowed() {
        let canonical = "OperationCancelReason";
        for (needle, identifier) in forbidden_needles() {
            assert!(
                !needle_found(canonical, &needle, identifier),
                "canonical type must not be forbidden by needle {needle:?}"
            );
        }
    }

    /// Main gate: every production `.rs` file under the operation directory must
    /// stay host-domain-agnostic.
    #[test]
    fn operation_core_is_host_domain_agnostic() {
        let mut files = Vec::new();
        collect_rs_files(&operation_dir(), &mut files);
        assert!(
            files.len() >= 6,
            "expected at least the core operation modules"
        );
        for path in &files {
            let raw =
                fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let code = strip_comments(&raw);
            for (needle, identifier) in forbidden_needles() {
                assert!(
                    !needle_found(&code, &needle, identifier),
                    "operation-core {} must not reference host-domain {needle}",
                    path.display()
                );
            }
        }
    }
}
