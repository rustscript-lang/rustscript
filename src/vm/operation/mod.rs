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
    //! lexically sanitized (comments and the bodies of every string/char
    //! literal are blanked while code tokens and newlines are preserved), and
    //! scanned for identifiers and paths that would couple the operation core
    //! to a concrete host domain (builtins, a database binding, or the removed
    //! cancellation-token / owner APIs). Newly added files are picked up
    //! automatically; the gate is not a fixed allowlist.
    //!
    //! Enumeration is fail-closed: an unreadable directory or entry propagates
    //! as an error instead of silently shrinking the scan.

    use std::collections::BTreeSet;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};

    /// True when `i` is the first byte of a token (not preceded by an
    /// identifier byte), used to avoid reading `b`/`r`/`c` literal prefixes
    /// out of a longer identifier.
    fn at_token_start(b: &[u8], i: usize) -> bool {
        i == 0 || !is_ident_byte(b[i - 1])
    }

    /// Length in bytes of a single UTF-8 character given its leading byte.
    fn utf8_char_len(first: u8) -> usize {
        if first >= 0xF0 {
            4
        } else if first >= 0xE0 {
            3
        } else if first >= 0xC0 {
            2
        } else {
            1
        }
    }

    /// Whether a `'` at index `q` opens a char literal (as opposed to a
    /// lifetime/label). A char literal is `'` plus one char or escape plus a
    /// closing `'`. Lifetimes/labels (`'_`, `'static`, `'a`, `for<'a>`,
    /// `'a:`) have no closing quote and are treated as code so they never
    /// suspend comment stripping or swallow following text.
    fn is_char_literal_at(b: &[u8], q: usize) -> bool {
        let Some(&c) = b.get(q + 1) else {
            return false;
        };
        if c == b'\\' {
            return true; // escape char literal
        }
        if c == b'\'' {
            return false; // empty; not a literal
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let mut j = q + 1;
            while j < b.len() && is_ident_byte(b[j]) {
                j += 1;
            }
            // Closed by a quote => single-char literal; otherwise a lifetime.
            b.get(j) == Some(&b'\'')
        } else {
            let len = utf8_char_len(c);
            b.get(q + 1 + len) == Some(&b'\'')
        }
    }

    /// Detect a raw / raw-byte / C raw string prefix at `i` (`r"`, `r#"`,
    /// `br"`, `br#"`, `cr"`, `cr#"`, … with any number of `#`). Returns the
    /// number of bytes in the opening prefix (including the opening quote) and
    /// the hash count, or `None`.
    fn raw_string_prefix(b: &[u8], i: usize) -> Option<(usize, usize)> {
        if !at_token_start(b, i) {
            return None;
        }
        let base = if b[i] == b'r' {
            1
        } else if b[i] == b'b' && b.get(i + 1) == Some(&b'r') {
            2
        } else if b[i] == b'c' && b.get(i + 1) == Some(&b'r') {
            2
        } else {
            return None;
        };
        let mut j = i + base;
        let mut hashes = 0usize;
        while j < b.len() && b[j] == b'#' {
            hashes += 1;
            j += 1;
        }
        if j < b.len() && b[j] == b'"' {
            Some((j + 1 - i, hashes))
        } else {
            None
        }
    }

    /// Index of the closing `"` for a raw string opened at `start` with
    /// `hashes` trailing hashes (delimiter `"` + N `#`). For valid Rust the
    /// body never contains the delimiter, so the first match is correct.
    fn find_raw_close(b: &[u8], start: usize, hashes: usize) -> usize {
        let mut j = start;
        while j < b.len() {
            if b[j] == b'"' {
                let mut ok = true;
                for k in 0..hashes {
                    if b.get(j + 1 + k) != Some(&b'#') {
                        ok = false;
                        break;
                    }
                }
                if ok {
                    return j;
                }
            }
            j += 1;
        }
        b.len()
    }

    /// Blank (spaces, preserving newlines) a cooked string body starting just
    /// past the opening quote through and including the closing quote,
    /// honoring `\` escapes. Returns the new index.
    fn blank_cooked_string(b: &[u8], mut i: usize, out: &mut Vec<u8>) -> usize {
        while i < b.len() {
            let c = b[i];
            if c == b'\\' {
                out.push(b' ');
                i += 1;
                if i < b.len() {
                    out.push(b' ');
                    i += 1;
                }
            } else if c == b'"' {
                out.push(b' ');
                i += 1;
                break;
            } else if c == b'\n' {
                out.push(b'\n');
                i += 1;
            } else {
                out.push(b' ');
                i += 1;
            }
        }
        i
    }

    /// Blank a char literal body starting just past the opening quote through
    /// and including the closing quote, honoring `\` escapes (including the
    /// escaped quote `\'`). Returns the new index.
    fn blank_char_body(b: &[u8], mut i: usize, out: &mut Vec<u8>) -> usize {
        while i < b.len() {
            let c = b[i];
            if c == b'\\' {
                out.push(b' ');
                i += 1;
                if i < b.len() {
                    out.push(b' ');
                    i += 1;
                }
            } else if c == b'\'' {
                out.push(b' ');
                i += 1;
                break;
            } else if c == b'\n' {
                out.push(b'\n');
                i += 1;
            } else {
                out.push(b' ');
                i += 1;
            }
        }
        i
    }

    /// Lexically sanitize `src` for scanning: comments and the bodies of every
    /// string/char literal (cooked, byte, raw, raw-byte, C, C-raw, with
    /// arbitrary `#` delimiters, escapes, and multi-line) are replaced with
    /// spaces while newlines are preserved and code tokens are left intact.
    /// Lifetimes and labels remain code. Returns valid UTF-8 because sanitized
    /// boundaries are ASCII and body bytes are blanked.
    fn sanitize(src: &str) -> String {
        let b = src.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0usize;
        while i < b.len() {
            let c = b[i];
            let next = b.get(i + 1).copied();
            match (c, next) {
                (b'/', Some(b'/')) => {
                    // Line comment: blank to (not including) the newline.
                    i += 2;
                    while i < b.len() && b[i] != b'\n' {
                        out.push(b' ');
                        i += 1;
                    }
                }
                (b'/', Some(b'*')) => {
                    // Nested block comment.
                    let mut depth = 1usize;
                    i += 2;
                    while i < b.len() && depth > 0 {
                        match (b[i], b.get(i + 1).copied()) {
                            (b'/', Some(b'*')) => {
                                depth += 1;
                                out.push(b' ');
                                out.push(b' ');
                                i += 2;
                            }
                            (b'*', Some(b'/')) => {
                                depth -= 1;
                                out.push(b' ');
                                out.push(b' ');
                                i += 2;
                            }
                            (b'\n', _) => {
                                out.push(b'\n');
                                i += 1;
                            }
                            _ => {
                                out.push(b' ');
                                i += 1;
                            }
                        }
                    }
                }
                _ => {
                    if let Some((prefix_len, hashes)) = raw_string_prefix(b, i) {
                        for _ in 0..prefix_len {
                            out.push(b' ');
                        }
                        i += prefix_len;
                        let close = find_raw_close(b, i, hashes);
                        while i < close && i < b.len() {
                            if b[i] == b'\n' {
                                out.push(b'\n');
                            } else {
                                out.push(b' ');
                            }
                            i += 1;
                        }
                        // Closing quote plus trailing hashes.
                        if i < b.len() {
                            out.push(b' ');
                            i += 1;
                        }
                        for _ in 0..hashes {
                            if i < b.len() {
                                out.push(b' ');
                                i += 1;
                            }
                        }
                        continue;
                    }
                    // Cooked / byte / C string literal.
                    if c == b'"'
                        || (c == b'b' && next == Some(b'"') && at_token_start(b, i))
                        || (c == b'c' && next == Some(b'"') && at_token_start(b, i))
                    {
                        if c != b'"' {
                            out.push(b' '); // blank the b / c prefix
                            i += 1;
                        }
                        out.push(b' '); // opening quote
                        i += 1;
                        i = blank_cooked_string(b, i, &mut out);
                        continue;
                    }
                    // Byte / C char literal.
                    if (c == b'b'
                        && next == Some(b'\'')
                        && at_token_start(b, i)
                        && is_char_literal_at(b, i + 1))
                        || (c == b'c'
                            && next == Some(b'\'')
                            && at_token_start(b, i)
                            && is_char_literal_at(b, i + 1))
                    {
                        out.push(b' '); // prefix
                        i += 1;
                        out.push(b' '); // opening quote
                        i += 1;
                        i = blank_char_body(b, i, &mut out);
                        continue;
                    }
                    // Plain char literal vs lifetime/label.
                    if c == b'\'' {
                        if is_char_literal_at(b, i) {
                            out.push(b' ');
                            i += 1;
                            i = blank_char_body(b, i, &mut out);
                        } else {
                            out.push(c); // lifetime/label remains code
                            i += 1;
                        }
                        continue;
                    }
                    out.push(c);
                    i += 1;
                }
            }
        }
        String::from_utf8(out).expect("sanitized operation source is valid UTF-8")
    }

    /// Drop whitespace that hugs a `:` so spaced paths like
    /// `crate :: builtins` normalize to `crate::builtins` and stay detectable
    /// without merging unrelated identifiers.
    fn normalize_spaced_paths(code: &str) -> String {
        let b = code.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        for (idx, &c) in b.iter().enumerate() {
            if c.is_ascii_whitespace() {
                let prev = if idx > 0 { b[idx - 1] } else { 0 };
                let next = b.get(idx + 1).copied().unwrap_or(0);
                if prev == b':' || next == b':' {
                    continue;
                }
                out.push(b' ');
            } else {
                out.push(c);
            }
        }
        String::from_utf8(out).expect("sanitized operation source is valid UTF-8")
    }

    /// Sanitize and normalize, i.e. the form used for needle matching.
    fn visible_code(src: &str) -> String {
        normalize_spaced_paths(&sanitize(src))
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

    /// Recursively enumerate every `.rs` file under `dir` in a stable,
    /// deterministic order (paths sorted at each level). Fail-closed: an
    /// unreadable directory or entry propagates as an error rather than being
    /// silently dropped.
    fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
        let entries = fs::read_dir(dir)?;
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let p = entry?.path();
            paths.push(p);
        }
        paths.sort();
        for p in paths {
            let md = fs::metadata(&p)?;
            if md.is_dir() {
                collect_rs_files(&p, out)?;
            } else if p.extension().is_some_and(|e| e == "rs") {
                out.push(p);
            }
        }
        Ok(())
    }

    /// The enumerator is recursive, nonempty, includes the current core
    /// modules, and does not hard-code a fixed allowlist (the exact full set is
    /// never asserted), so future files remain subject to the gate.
    #[test]
    fn operation_core_enumeration_includes_all_core_modules() {
        let mut files = Vec::new();
        collect_rs_files(&operation_dir(), &mut files)
            .unwrap_or_else(|e| panic!("failed to enumerate {}: {e}", operation_dir().display()));
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
        collect_rs_files(&operation_dir(), &mut files)
            .unwrap_or_else(|e| panic!("failed to enumerate {}: {e}", operation_dir().display()));
        assert!(
            files.len() >= 6,
            "expected at least the core operation modules"
        );
        for path in &files {
            let raw =
                fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let code = visible_code(&raw);
            for (needle, identifier) in forbidden_needles() {
                assert!(
                    !needle_found(&code, &needle, identifier),
                    "operation-core {} must not reference host-domain {needle}",
                    path.display()
                );
            }
        }
    }

    /// A lifetime before a line comment must not suspend stripping, so a
    /// forbidden needle inside that comment is ignored.
    #[test]
    fn lifetime_does_not_suppress_comment_stripping() {
        let needle = ["crate", "::", "builtins"].concat();
        let src = format!(
            "fn f<'a>(s: &'static str) -> &'a str {{\n    // for example: {needle}::x is forbidden\n    s\n}}\n"
        );
        let code = visible_code(&src);
        assert!(
            !needle_found(&code, &needle, false),
            "needle in a line comment after a lifetime must be ignored"
        );
        // Lifetimes stay as code and never swallow following text.
        assert!(code.contains("&'static"), "lifetime must remain code");
        assert!(code.contains("'a"), "lifetime must remain code");
    }

    /// Forbidden needles hidden inside string/char literals are ignored across
    /// all literal forms (cooked, byte, raw, raw-byte, C, C-raw, `#` delimiters,
    /// escapes, multi-line) and chars are blanked without swallowing code.
    #[test]
    fn needles_in_string_and_char_literals_are_ignored() {
        let needle = ["crate", "::", "builtins"].concat();
        let cases = [
            format!("let a = \"{needle}\";"),
            format!("let b = b\"{needle}\";"),
            format!("let c = r\"{needle}\";"),
            format!("let d = r#\"{needle}\"#;"),
            format!("let e = r##\"{needle}\"##;"),
            format!("let f = br\"{needle}\";"),
            format!("let g = br#\"{needle}\"#;"),
            format!("let h = cr#\"{needle}\"#;"),
            format!("let i = c\"{needle}\";"),
            format!("let m = \"first\\n{needle}\\t\";"),
            format!("let ml = \"line1\n{needle}\nline2\";"),
        ];
        for (n, src) in cases.iter().enumerate() {
            let code = visible_code(src);
            assert!(
                !needle_found(&code, &needle, false),
                "case {n} must ignore the needle inside a literal: {src}"
            );
        }
        // Char literals (including the quote char, escapes, and non-ASCII) are
        // blanked but never swallow surrounding code.
        for src in [
            "let a = 'x';",
            "let q = '\\'';",
            "let n = '\\n';",
            "let u = '\\u{7f}';",
            "let byte = b'y';",
            "let cstr = c'z';",
        ] {
            let code = visible_code(src);
            assert!(
                code.contains("let"),
                "char literal must not swallow code: {src}"
            );
        }
        // A needle after a byte-char in a comment is still ignored.
        let with_comment = format!("let a = b'x'; // {needle} forbidden\n");
        let code = visible_code(&with_comment);
        assert!(!needle_found(&code, &needle, false));
    }

    /// Real forbidden code (contiguous, spaced/rustfmt-normal, and owner API)
    /// is still detected after sanitization.
    #[test]
    fn real_forbidden_code_is_detected() {
        let real_path = ["crate", "::", "builtins", "::", "x"].concat();
        let op_owner = ["Operation", "Owner"].concat();
        let sqlite = ["rusq", "lite"].concat();
        let spaced = ["crate", " ", "::", " ", "builtins", " ", "::", " ", "x"].concat();
        let needle = ["crate", "::", "builtins"].concat();

        let code = visible_code(&format!(
            "fn go() {{ {real_path}(); {op_owner}::poll(); {sqlite}_open(); {spaced}; }}\n"
        ));
        assert!(
            needle_found(&code, &real_path, false),
            "contiguous path must be found"
        );
        assert!(
            needle_found(&code, &op_owner, true),
            "OperationOwner identifier must be found"
        );
        assert!(
            needle_found(&code, &sqlite, false),
            "rusqlite must be found"
        );
        assert!(
            needle_found(&code, &needle, false),
            "spaced/rustfmt-normal path must be found after normalization"
        );
    }

    /// Nested block comments are fully blanked so needles inside are ignored.
    #[test]
    fn nested_block_comments_are_ignored() {
        let needle = ["crate", "::", "builtins"].concat();
        let src = format!("/* outer /* inner {needle} */ still comment */\nfn ok() {{}}\n");
        let code = visible_code(&src);
        assert!(
            !needle_found(&code, &needle, false),
            "needle in a nested block comment must be ignored"
        );
        assert!(code.contains("fn ok"), "code after the comment must remain");
    }

    /// The recursive enumerator finds a depth-2 tree of `.rs` files (census)
    /// and the sanitizer detects forbidden content inside it, then the RAII
    /// cleanup guard removes the tree even on the normal completion.
    #[test]
    fn recursive_collector_detects_depth2_tree_and_cleans_up() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let root = Path::new("/mnt/TEMP/rustscript/architecture-gate-tests").join(format!(
            "archgate-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let leaf = root.join("sub").join("leaf");

        // Small RAII cleanup guard: removes the tree on drop (normal return or
        // unwinding), so no temp files are ever left behind.
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        {
            let _guard = Cleanup(root.clone());
            fs::create_dir_all(&leaf).expect("create depth-2 test tree");

            let benign = leaf.join("benign.rs");
            let forbidden = leaf.join("forbidden.rs");
            fs::write(&benign, "fn helper() {}\n").expect("write benign source");
            let needle = ["crate", "::", "builtins"].concat();
            fs::write(&forbidden, format!("fn hostile() {{ {needle}::x() }}\n"))
                .expect("write forbidden source");

            // Census: the recursive enumerator finds exactly the two .rs files.
            let mut found = Vec::new();
            collect_rs_files(&root, &mut found).expect("collect test tree");
            let found_set: BTreeSet<PathBuf> = found.into_iter().collect();
            assert_eq!(found_set.len(), 2, "census must find both .rs files");
            assert!(found_set.contains(&benign), "census must include benign.rs");
            assert!(
                found_set.contains(&forbidden),
                "census must include forbidden.rs"
            );

            // Detection: forbidden content is caught; benign is clean.
            let forbidden_code =
                visible_code(&fs::read_to_string(&forbidden).expect("read forbidden.rs"));
            assert!(
                needle_found(&forbidden_code, &needle, false),
                "forbidden needle must be detected"
            );
            let benign_code = visible_code(&fs::read_to_string(&benign).expect("read benign.rs"));
            assert!(
                !needle_found(&benign_code, &needle, false),
                "benign source must be clean"
            );
        }
        // The guard removed the tree even on the normal completion path.
        assert!(
            !root.exists(),
            "test tree must be removed by the cleanup guard"
        );
    }

    /// A missing directory fails closed (returns Err) with no partial census.
    #[test]
    fn collect_rs_files_fails_closed_on_missing_dir() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let missing = Path::new("/mnt/TEMP/rustscript/architecture-gate-tests").join(format!(
            "missing-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let mut files = Vec::new();
        let result = collect_rs_files(&missing, &mut files);
        assert!(result.is_err(), "missing directory must fail closed");
        assert!(files.is_empty(), "no partial census on failure");
    }
}
