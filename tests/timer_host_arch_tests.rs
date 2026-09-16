//! Source-level architecture guard for the standard timer host module.
//!
//! The standard `timer::*` surface is a self-contained host-function module
//! (`src/builtins/runtime/timer.rs`). The host-agnostic VM core must never
//! learn about timers: no timer state, deadlines, intervals, counts, or
//! scheduling policy may appear under `src/vm` except for the *generic*
//! owned-call registry primitives in `src/vm/host.rs`, which must stay free of
//! every timer domain term.
//!
//! These tests are source-only (they read the manifest-adjacent production
//! sources), so they run under the default feature set without executing any
//! timer. They deliberately inspect *production* code paths rather than
//! comments, doc prose, string literals, or `#[cfg(test)]` fixtures.

use std::fs;
use std::path::{Path, PathBuf};

/// The single file that owns every timer implementation symbol.
const TIMER_HOST_MODULE: &str = "src/builtins/runtime/timer.rs";

/// Production files allowed to reference the timer module (composition and
/// public re-exports only — never timer *implementation* symbols).
///
/// `host_modules.rs` is the deterministic standard host-module aggregation: it
/// is the composition layer that selects the timer module for a build, so it
/// belongs here for the same reason `runtime/mod.rs` does.
const TIMER_COMPOSITION_FILES: &[&str] = &[
    "src/builtins/runtime/mod.rs",
    "src/builtins/runtime/host_modules.rs",
    "src/builtins/runtime/standard_composition.rs",
    "src/lib.rs",
];

/// The only `src/vm` file allowed to carry generic owned-call registry
/// primitives, and the file that must never name a timer domain term.
const HOST_REGISTRY_FILE: &str = "src/vm/host.rs";

/// VM-core files that must stay entirely timer-free.
const TIMER_FREE_VM_CORE_FILES: &[&str] = &[
    "src/vm/mod.rs",
    "src/vm/instance.rs",
    "src/vm/invocation.rs",
    "src/vm/execution_scope.rs",
    "src/vm/run_context.rs",
    "src/vm/host_runtime.rs",
    "src/vm/resource/table.rs",
];

/// Directory trees that must stay entirely timer-free.
const TIMER_FREE_VM_CORE_TREES: &[&str] = &["src/vm/async_host", "src/vm/resource"];

/// Timer *domain* terms. These must never appear in VM-core production code.
const TIMER_DOMAIN_TERMS: &[&str] = &[
    "timer",
    "Timer",
    "premature",
    "pending_count",
    "running_count",
];

/// Timer *implementation* symbols. Outside the timer host module these may
/// appear only in the composition/re-export files above.
const TIMER_IMPLEMENTATION_SYMBOLS: &[&str] = &[
    "TimerConfig",
    "TimerBackend",
    "TimerRegistration",
    "OwnedTimerCallback",
    "TimerHostState",
    "TimerTask",
    "register_timer_builtin_module",
    "timer_host_catalog",
];

/// Generic owned-dispatch primitives the registry must provide.
const OWNED_DISPATCH_SYMBOLS: &[&str] =
    &["HostOwnedFunction", "OwnedHostCall", "register_exact_owned"];

/// Review-address files that must stay outside the timer/owned-dispatch
/// boundary. Resource-table, execution-scope, host-runtime, compiler-codegen
/// and lifetime-capture changes are out of scope for the timer host module.
///
/// This boundary is enforced by the token scanner below, not by comparing the
/// working tree against `HEAD`: a `HEAD` comparison only observes uncommitted
/// edits (making it tautological for committed work) and cannot run in a
/// packaged source tree without `.git`.
const PROHIBITED_BASE_FILES: &[&str] = &[
    "src/vm/execution_scope.rs",
    "src/vm/host_runtime.rs",
    "src/vm/resource/table.rs",
    "src/compiler/codegen.rs",
    "src/compiler/lifetime/availability/captures.rs",
];

fn manifest_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let path = entry.expect("readable entry").path();
        let metadata = fs::metadata(&path).expect("source metadata");
        if metadata.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Every production `.rs` file under `src/`, sorted for deterministic output.
fn production_sources() -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_rs(&manifest_root().join("src"), &mut files);
    files.sort();
    files
}

fn relative(path: &Path) -> String {
    path.strip_prefix(manifest_root())
        .expect("source under manifest root")
        .to_string_lossy()
        .replace('\\', "/")
}

/// Replaces one literal with whitespace while preserving line numbers.
fn blank_literal(out: &mut String, source: &str, start: usize, end: usize) {
    for character in source[start..end].chars() {
        if character == '\n' {
            out.push('\n');
        } else {
            out.push(' ');
        }
    }
}

/// Returns the exclusive end of a raw string/byte-string literal.
fn raw_string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut cursor = start;
    if bytes.get(cursor) == Some(&b'b') {
        if bytes.get(cursor + 1) != Some(&b'r') {
            return None;
        }
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'r') {
        return None;
    }
    cursor += 1;
    let mut hashes = 0usize;
    while bytes.get(cursor) == Some(&b'#') {
        hashes += 1;
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'"') {
        return None;
    }
    let content_start = cursor + 1;
    let mut candidate = content_start;
    while candidate < bytes.len() {
        if bytes[candidate] == b'"'
            && bytes
                .get(candidate + 1..candidate + 1 + hashes)
                .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
        {
            return Some(candidate + 1 + hashes);
        }
        candidate += 1;
    }
    None
}

/// Returns the exclusive end of a normal quoted literal.
fn quoted_literal_end(bytes: &[u8], start: usize, quote: u8) -> usize {
    let mut cursor = start + 1;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor = cursor.saturating_add(2),
            value if value == quote => return cursor + 1,
            _ => cursor += 1,
        }
    }
    bytes.len()
}

/// Returns the exclusive end of a character literal, if `start` begins one.
fn char_literal_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut cursor = start + 1;
    if cursor >= bytes.len() || bytes[cursor] == b'\n' {
        return None;
    }
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor = cursor.saturating_add(2),
            b'\n' => return None,
            b'\'' => return Some(cursor + 1),
            _ => cursor += 1,
        }
    }
    None
}

/// Removes comments and blanks string/character literals so the guards inspect
/// Rust tokens rather than text embedded in source literals.
fn strip_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if let Some(end) = raw_string_end(bytes, cursor) {
            blank_literal(&mut out, source, cursor, end);
            cursor = end;
            continue;
        }
        match bytes[cursor] {
            b'"' => {
                let end = quoted_literal_end(bytes, cursor, b'"');
                blank_literal(&mut out, source, cursor, end);
                cursor = end;
            }
            b'\'' => {
                if let Some(end) = char_literal_end(bytes, cursor) {
                    blank_literal(&mut out, source, cursor, end);
                    cursor = end;
                } else {
                    let character = source[cursor..]
                        .chars()
                        .next()
                        .expect("cursor must point inside source");
                    out.push(character);
                    cursor += character.len_utf8();
                }
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                out.push(' ');
                cursor += 2;
                while cursor < bytes.len() && bytes[cursor] != b'\n' {
                    cursor += 1;
                }
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                out.push(' ');
                cursor += 2;
                let mut depth = 1usize;
                while cursor < bytes.len() && depth > 0 {
                    if bytes.get(cursor..cursor + 2) == Some(b"/*") {
                        depth += 1;
                        cursor += 2;
                    } else if bytes.get(cursor..cursor + 2) == Some(b"*/") {
                        depth -= 1;
                        cursor += 2;
                    } else {
                        if bytes[cursor] == b'\n' {
                            out.push('\n');
                        }
                        cursor += 1;
                    }
                }
            }
            _ => {
                let character = source[cursor..]
                    .chars()
                    .next()
                    .expect("cursor must point inside source");
                out.push(character);
                cursor += character.len_utf8();
            }
        }
    }
    out
}

/// Blanks every `#[cfg(test)]`-attributed module/function body so unit-test
/// fixtures (which exist to *discuss* forbidden tokens) never trip the
/// production scan.
fn strip_cfg_test_blocks(mut code: String) -> String {
    let needle = "#[cfg(test)]";
    let mut out = String::new();
    loop {
        let Some(index) = code.find(needle) else {
            out.push_str(&code);
            break;
        };
        out.push_str(&code[..index]);
        code = code[index + needle.len()..].to_string();
        code = code.trim_start().to_string();
        while code.starts_with('#') {
            let Some(attr_end) = code.find(']') else {
                break;
            };
            code = code[attr_end + 1..].to_string();
            code = code.trim_start().to_string();
        }
        // The item body starts at the first `{` after the attributes. A
        // bodyless item declaration (`mod tests;`) is dropped at its `;`.
        let Some(open) = code.find('{') else {
            continue;
        };
        if let Some(semicolon) = code.find(';')
            && semicolon < open
        {
            // `#[cfg(test)] mod tests;` — a module declaration, not a body.
            continue;
        }
        let mut depth = 0usize;
        let mut close = None;
        for (i, byte) in code[open..].bytes().enumerate() {
            match byte {
                b'{' => depth += 1,
                b'}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        close = Some(open + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        match close {
            Some(end) => {
                code = code[end + 1..].to_string();
            }
            None => {
                code.clear();
            }
        }
    }
    out
}

/// Production code of one source file: comments/string literals and
/// `#[cfg(test)]` bodies removed.
fn production_code(path: &Path) -> String {
    let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    strip_cfg_test_blocks(strip_comments(&raw))
}

fn files_under_tree(relative_tree: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_rs(&manifest_root().join(relative_tree), &mut files);
    files.sort();
    files
}

fn matched_terms(code: &str, terms: &[&str]) -> Vec<String> {
    terms
        .iter()
        .filter(|term| code.contains(**term))
        .map(|term| (*term).to_string())
        .collect()
}

/// The scanner form of the boundary guard: no timer implementation symbol and
/// no generic owned-dispatch primitive may appear in the review-address files.
/// Unlike a working-tree/`HEAD` comparison this runs from a packaged source
/// tree without `.git` and inspects production tokens (comments, string
/// literals, and `#[cfg(test)]` bodies stripped).
#[test]
fn prohibited_timer_review_files_contain_no_timer_or_owned_dispatch_symbols() {
    let mut forbidden = TIMER_IMPLEMENTATION_SYMBOLS.to_vec();
    forbidden.extend(OWNED_DISPATCH_SYMBOLS);
    let mut offenders = Vec::new();
    for relative in PROHIBITED_BASE_FILES {
        let path = manifest_root().join(relative);
        let code = production_code(&path);
        let matched = matched_terms(&code, &forbidden);
        if !matched.is_empty() {
            offenders.push(format!("{relative} → {}", matched.join(", ")));
        }
    }
    assert!(
        offenders.is_empty(),
        "resource-table, execution-scope, host-runtime, and compiler files \
         must stay outside the timer/owned-dispatch boundary; offenders:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn timer_host_module_exists() {
    let path = manifest_root().join(TIMER_HOST_MODULE);
    assert!(
        path.is_file(),
        "the standard timer host module must exist at {TIMER_HOST_MODULE}"
    );
}

#[test]
fn timer_module_is_registered_only_from_builtin_composition() {
    let composition = manifest_root().join("src/builtins/runtime/mod.rs");
    let code = production_code(&composition);
    for symbol in [
        "register_timer_builtin_module",
        "timer_host_catalog",
        "timer",
    ] {
        assert!(
            code.contains(symbol),
            "src/builtins/runtime/mod.rs must compose the timer host module \
             (missing `{symbol}`)"
        );
    }

    // No production file outside the timer module and the composition /
    // re-export files may reference the timer module's registration entry
    // points.
    let mut offenders = Vec::new();
    for path in production_sources() {
        let rel = relative(&path);
        if rel == TIMER_HOST_MODULE || TIMER_COMPOSITION_FILES.contains(&rel.as_str()) {
            continue;
        }
        let code = production_code(&path);
        for symbol in [
            "register_timer_builtin_module",
            "timer_host_catalog",
            "timer::",
        ] {
            if code.contains(symbol) {
                offenders.push(format!("{rel} → {symbol}"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "the timer host module must be registered only from the builtin \
         runtime composition; offenders:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn vm_core_paths_contain_no_timer_symbols() {
    let mut offenders = Vec::new();
    for file in TIMER_FREE_VM_CORE_FILES {
        let path = manifest_root().join(file);
        if !path.is_file() {
            continue;
        }
        let code = production_code(&path);
        let matched = matched_terms(&code, TIMER_DOMAIN_TERMS);
        if !matched.is_empty() {
            offenders.push(format!("{file} → {}", matched.join(", ")));
        }
    }
    for tree in TIMER_FREE_VM_CORE_TREES {
        for path in files_under_tree(tree) {
            let rel = relative(&path);
            let code = production_code(&path);
            let matched = matched_terms(&code, TIMER_DOMAIN_TERMS);
            if !matched.is_empty() {
                offenders.push(format!("{rel} → {}", matched.join(", ")));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "timer domain terms leaked into VM-core paths (run/resume, frames, \
         async-host bridge, execution scope, resource tables):\n{}",
        offenders.join("\n")
    );
}

#[test]
fn host_registry_exposes_generic_owned_dispatch_without_timer_terms() {
    let path = manifest_root().join(HOST_REGISTRY_FILE);
    let code = production_code(&path);
    for symbol in OWNED_DISPATCH_SYMBOLS {
        assert!(
            code.contains(symbol),
            "src/vm/host.rs must expose the generic owned-call registry \
             primitive `{symbol}`"
        );
    }
    let mut forbidden = TIMER_DOMAIN_TERMS.to_vec();
    forbidden.extend(["deadline", "interval"]);
    let matched = matched_terms(&code, &forbidden);
    assert!(
        matched.is_empty(),
        "src/vm/host.rs is the generic registry exception and must contain no \
         timer domain terms; found: {}",
        matched.join(", ")
    );
}

#[test]
fn timer_implementation_symbols_are_confined_to_the_timer_host_module() {
    let mut offenders = Vec::new();
    for path in production_sources() {
        let rel = relative(&path);
        if rel == TIMER_HOST_MODULE {
            continue;
        }
        let code = production_code(&path);
        let matched = matched_terms(&code, TIMER_IMPLEMENTATION_SYMBOLS);
        if matched.is_empty() {
            continue;
        }
        if TIMER_COMPOSITION_FILES.contains(&rel.as_str()) {
            // Composition and public re-export files may forward public timer
            // types and entry points. Definitions and behavior remain in the
            // dedicated module; this guard separately checks all other
            // production sources.
            continue;
        }
        offenders.push(format!("{rel} → {}", matched.join(", ")));
    }
    assert!(
        offenders.is_empty(),
        "timer implementation symbols must live only in {TIMER_HOST_MODULE}:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn architecture_scanner_ignores_literals_and_comment_markers_inside_them() {
    let source = r####"
        fn sample() {
            let normal = "timer deadline // not a comment /* not a block */";
            let raw = r###"timer interval // /* braces { } */"###;
            let byte = br##"timer every #{}"##;
            let character = '/';
            // timer max_running
            /* timer poll_reporting */
        }
    "####;
    let code = strip_comments(source);
    assert!(!code.contains("timer"));
    assert!(!code.contains("deadline"));
    assert!(!code.contains("interval"));
    assert!(!code.contains("max_running"));
    assert!(!code.contains("poll_reporting"));
}
