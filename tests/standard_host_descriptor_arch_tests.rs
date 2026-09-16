//! Architecture guard for the standard host descriptor surface.
//!
//! These tests prove, from the production sources and the runtime composition,
//! that:
//!
//! * every standard `#[pd_host_function]` has **exactly one** descriptor owner
//!   (a module ownership list), and every ownership list belongs to a module
//!   composed in this build by [`vm::standard_host_modules`];
//! * no migrated module keeps a hand-written parallel catalog, adapter table,
//!   or registry glue;
//! * the standard guest catalog is derived from the module descriptors, and its
//!   fingerprint is byte-for-byte the published one;
//! * feature gates deterministically include or exclude whole modules;
//! * every resource key in the derived catalog has a declaration, and the typed
//!   named-struct set is preserved.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Whether this build composes the HTTP host module (and therefore its guest
/// surface).
const HTTP_SURFACE_ENABLED: bool = cfg!(all(feature = "http-client", not(target_family = "wasm")));

/// Fingerprint of the published standard host catalog **with** the HTTP
/// surface.
///
/// Both fingerprints are exact goldens of the descriptor migration: the whole
/// point of the change is that guest contracts did not move. The standard
/// catalog is the merge of the composed module surfaces, so the golden depends
/// on the composed set — the default build (no `http-client`) composes one
/// module fewer and must reproduce [`STANDARD_CATALOG_FINGERPRINT_NO_HTTP`].
const STANDARD_CATALOG_FINGERPRINT: &str = "6607e4fcb3187e73";
/// Fingerprint of the published standard host catalog **without** the HTTP
/// surface: the `--workspace` default build and every wasm build.
const STANDARD_CATALOG_FINGERPRINT_NO_HTTP: &str = "a6b4b2dcadc5df14";
const IO_CATALOG_FINGERPRINT: &str = "234a7fdc3aaa3f95";
const SQLITE_CATALOG_FINGERPRINT: &str = "b6d4c278145edacf";
const JIT_CATALOG_FINGERPRINT: &str = "d0a3efbca2d0923c";
const TIMER_CATALOG_FINGERPRINT: &str = "4af2dfa2aee1f42e";
#[cfg(all(feature = "http-client", not(target_family = "wasm")))]
const HTTP_CATALOG_FINGERPRINT: &str = "18a4033f5857c033";

/// The standard catalog fingerprint this build must reproduce exactly.
fn standard_catalog_fingerprint() -> &'static str {
    if HTTP_SURFACE_ENABLED {
        STANDARD_CATALOG_FINGERPRINT
    } else {
        STANDARD_CATALOG_FINGERPRINT_NO_HTTP
    }
}

/// Resource keys the standard catalog always publishes.
const STANDARD_RESOURCE_KEYS: &[&str] = &["io.file", "sqlite.connection"];

/// Resource keys the HTTP module publishes when it is composed.
const HTTP_RESOURCE_KEYS: &[&str] = &["http.request", "http.response", "http.sse"];

/// Named structs the standard catalog always declares.
const STANDARD_NAMED_STRUCTS: &[&str] = &[
    "JitConfig",
    "SqliteValue",
    "SqliteQueryResult",
    "SqliteTransactionResult",
];

/// Named structs the HTTP module declares when it is composed.
const HTTP_NAMED_STRUCTS: &[&str] = &[
    "HttpRequest",
    "HttpResponse",
    "SseEvent",
    "SseSummary",
    "SseCallbackAction",
];

/// Source files that must contain no hand-written catalog or registry glue for
/// their migrated host module.
const MIGRATED_MODULE_FILES: &[&str] = &[
    "src/builtins/runtime/http/mod.rs",
    "src/builtins/runtime/http/sse.rs",
    "src/builtins/runtime/io/mod.rs",
    "src/builtins/runtime/io/async_io.rs",
    "src/builtins/runtime/io/blocking.rs",
    "src/builtins/runtime/io_wasm.rs",
    "src/builtins/runtime/jit.rs",
    "src/builtins/runtime/sqlite.rs",
    "src/builtins/runtime/timer.rs",
];

fn manifest_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn relative(path: &Path) -> String {
    path.strip_prefix(manifest_root())
        .expect("source under manifest root")
        .to_string_lossy()
        .replace('\\', "/")
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let path = entry.expect("readable entry").path();
        if fs::metadata(&path).expect("source metadata").is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn production_sources() -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_rs(&manifest_root().join("src/builtins"), &mut files);
    files.sort();
    files
}

/// Replaces one literal or comment with whitespace while preserving both line
/// numbers and byte offsets, so the raw text stays addressable.
fn blank_literal(out: &mut String, source: &str, start: usize, end: usize) {
    for byte in &source.as_bytes()[start..end] {
        out.push(if *byte == b'\n' { '\n' } else { ' ' });
    }
}

/// Returns the exclusive end of a character literal, if one starts at `start`.
///
/// A lifetime (`'static`, `'_, `'a`) is deliberately not a literal: treating it
/// as one would make the scan swallow every byte up to the next apostrophe.
fn char_literal_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut cursor = start + 1;
    loop {
        match bytes.get(cursor)? {
            b'\\' => cursor = cursor.checked_add(2)?,
            b'\'' => return Some(cursor + 1),
            b'\n' => return None,
            _ => cursor += 1,
        }
        if cursor - start > 8 {
            return None;
        }
    }
}

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

/// Removes comments so the guard inspects Rust tokens rather than prose.
fn strip_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'"' => {
                let end = quoted_literal_end(bytes, cursor, b'"');
                blank_literal(&mut out, source, cursor, end);
                cursor = end;
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                let start = cursor;
                while cursor < bytes.len() && bytes[cursor] != b'\n' {
                    cursor += 1;
                }
                blank_literal(&mut out, source, start, cursor);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                let start = cursor;
                cursor += 2;
                while cursor < bytes.len() && bytes.get(cursor..cursor + 2) != Some(b"*/") {
                    cursor += 1;
                }
                cursor = (cursor + 2).min(bytes.len());
                blank_literal(&mut out, source, start, cursor);
            }
            b'\'' => match char_literal_end(bytes, cursor) {
                // A character literal.
                Some(end) => {
                    blank_literal(&mut out, source, cursor, end);
                    cursor = end;
                }
                // A lifetime: keep the quote so `'static` stays visible.
                None => {
                    out.push('\'');
                    cursor += 1;
                }
            },
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

/// Production code of one source file: comments removed and unit-test
/// fixtures dropped.
fn production_code(path: &Path) -> String {
    strip_comments(&production_raw(path))
}

/// Production source of one file with its unit-test items blanked.
///
/// Attribute parsing needs the raw text: a `#[pd_host_function(name = ...)]`
/// value *is* a string literal, so the comment/literal-stripped form cannot be
/// used to read it.
///
/// Test items are replaced by whitespace instead of being truncated, so the
/// production text keeps its byte offsets and line numbers: diagnostics are
/// reported as `file:line`, and the comment stripper asserts the stripped form
/// has the same length. Truncating at the first `#[cfg(test)]` marker instead
/// would hide every production declaration that follows a test-only item,
/// which is exactly the shape `src/builtins/runtime/http/sse.rs` has.
fn production_raw(path: &Path) -> String {
    let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    strip_test_items(&raw)
}

/// The marker that introduces a unit-test item.
const CFG_TEST_ATTRIBUTE: &str = "#[cfg(test)]";

/// Blanks every `#[cfg(test)]` item in `source` while preserving byte offsets
/// and line numbers.
fn strip_test_items(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0usize;
    while let Some(offset) = source[cursor..].find(CFG_TEST_ATTRIBUTE) {
        let start = cursor + offset;
        out.push_str(&source[cursor..start]);
        let end = test_item_end(source, start);
        blank_literal(&mut out, source, start, end);
        cursor = end;
    }
    out.push_str(&source[cursor..]);
    out
}

/// Returns the exclusive end of the item or statement introduced by the
/// `#[cfg(test)]` attribute starting at `attr_start`.
///
/// This is a bounded lexer over the raw bytes rather than a full Rust parser:
/// it tracks bracket depth, skips string literals and comments, and stops at
/// the first top-level statement terminator (`;`) or at the closing brace that
/// returns to the enclosing block. That covers every shape the marker takes in
/// this crate: a constant, a free function, a method inside an `impl`, a
/// `static`, an `if` statement inside a function body, and a whole
/// `mod tests { ... }`.
fn test_item_end(source: &str, attr_start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut cursor = attr_start;
    let mut depth = 0usize;
    while cursor < bytes.len() {
        if let Some(end) = raw_string_end(bytes, cursor) {
            cursor = end;
            continue;
        }
        match bytes[cursor] {
            b'"' => cursor = quoted_literal_end(bytes, cursor, b'"'),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                while cursor < bytes.len() && bytes[cursor] != b'\n' {
                    cursor += 1;
                }
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor += 2;
                while cursor < bytes.len() && bytes.get(cursor..cursor + 2) != Some(b"*/") {
                    cursor += 1;
                }
                cursor = (cursor + 2).min(bytes.len());
            }
            b'{' | b'(' | b'[' => {
                depth += 1;
                cursor += 1;
            }
            b'}' => {
                if depth == 0 {
                    // The attributed item sits in a block that ends here.
                    return cursor + 1;
                }
                depth -= 1;
                cursor += 1;
                if depth == 0 {
                    // The attributed item closed with its own group.
                    return cursor;
                }
            }
            b')' | b']' => {
                depth = depth.saturating_sub(1);
                cursor += 1;
                if depth == 0 && bytes.get(cursor) == Some(&b';') {
                    return cursor + 1;
                }
            }
            b';' if depth == 0 => return cursor + 1,
            _ => cursor += 1,
        }
    }
    bytes.len()
}

/// Returns the exclusive end of a raw string/byte-string literal, if one starts
/// at `start`.
///
/// Raw strings carry unbalanced `{`/`}` and quotes verbatim (a test fixture
/// shell script is one), so the depth scan must skip them as a unit.
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

/// One standard `#[pd_host_function]` declaration found in the sources.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct StandardHostFunction {
    name: String,
    descriptor: String,
    file: String,
}

fn standard_host_functions() -> Vec<StandardHostFunction> {
    let mut found = Vec::new();
    for path in production_sources() {
        let code = production_code(&path);
        let raw = production_raw(&path);
        assert_eq!(
            code.len(),
            raw.len(),
            "{}: comment stripping must preserve byte offsets",
            relative(&path)
        );
        let file = relative(&path);
        let mut cursor = 0usize;
        while let Some(offset) = code[cursor..].find("#[pd_host_function(") {
            let attribute_start = cursor + offset;
            let body_start = attribute_start + "#[pd_host_function(".len();
            let Some(body_end) = code[body_start..].find(']') else {
                break;
            };
            // A `name = "..."` value is a string literal, so it is read from the
            // raw attribute body; the attribute may span several lines.
            let attribute = &raw[attribute_start..body_start + body_end];
            let line = code[..attribute_start].lines().count();
            let name = attribute
                .split("name =")
                .nth(1)
                .and_then(|tail| tail.trim_start().strip_prefix('"'))
                .and_then(|tail| tail.split('"').next())
                .unwrap_or_else(|| panic!("{file}:{line}: unparsable host function name"))
                .to_string();
            cursor = body_start + body_end;
            let Some(ident) = raw[cursor..].lines().find_map(|candidate| {
                let candidate = candidate.trim();
                let (_, tail) = candidate.split_once("fn ")?;
                let ident: String = tail
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                (!ident.is_empty()).then_some(ident)
            }) else {
                panic!("{file}:{line}: no function follows the attribute");
            };
            let descriptor = match ident.strip_suffix("_impl") {
                Some(prefix) => format!("{prefix}_descriptor"),
                None => format!("{ident}_descriptor"),
            };
            found.push(StandardHostFunction {
                name,
                descriptor,
                file: file.clone(),
            });
        }
    }
    found
}

/// Descriptor factories referenced by an ownership list, keyed by the
/// declaration count. An identifier introduced twice by two files of the same
/// module (the mutually exclusive IO backends) is one owner, not two.
fn declared_ownership() -> BTreeMap<String, Vec<String>> {
    /// The type that introduces an ownership list.
    const TYPE: &str = "HostFunctionDescriptor]";
    /// The list opener, which rustfmt may place on the following line.
    const OPENER: &str = "&[";

    let mut owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in production_sources() {
        let code = production_code(&path);
        let file = relative(&path);
        let mut remaining = code.as_str();
        while let Some(type_at) = remaining.find(TYPE) {
            let after_type = &remaining[type_at + TYPE.len()..];
            let Some(open_at) = after_type.find(OPENER) else {
                break;
            };
            let body_start = type_at + TYPE.len() + open_at + OPENER.len();
            let Some(close) = remaining[body_start..].find(']') else {
                break;
            };
            for entry in remaining[body_start..body_start + close].split(',') {
                let entry = entry.trim();
                if entry.is_empty() {
                    continue;
                }
                // An ownership list may name a descriptor through the module
                // that declares it (`sse::builtin_http_client_sse_descriptor`);
                // the owner is the descriptor alone.
                let entry = entry.rsplit("::").next().unwrap_or(entry).trim();
                if entry.is_empty() {
                    continue;
                }
                owners
                    .entry(entry.to_string())
                    .or_default()
                    .push(file.clone());
            }
            remaining = &remaining[body_start + close + 1..];
        }
    }
    owners
}

/// A standard host module named by a source file, with the descriptor
/// factories the file's ownership lists declare for it.
#[derive(Debug, Default, PartialEq, Eq)]
struct OwnershipSource {
    /// Module names the file declares through `StandardHostModule { name: ... }`,
    /// `descriptor_only_module(...)`, or `catalog_module(...)`.
    modules: BTreeSet<String>,
    /// Descriptor factories the file's `&[fn() -> HostFunctionDescriptor]`
    /// lists name, normalized to the descriptor's own ident.
    entries: BTreeSet<String>,
}

/// Source files that supply the ownership list of a module declared in another
/// file, mapped to the module they belong to.
///
/// The guard cannot derive that link from the descriptor lists alone: the
/// native SQLite adapter lives in `sqlite.rs` while the module that owns it is
/// declared in `sqlite_schema.rs`. Stating it here keeps it checkable — every
/// module named here must be composed in this build.
const SHARED_MODULE_SOURCES: &[(&str, &str)] = &[("src/builtins/runtime/sqlite.rs", "sqlite")];

/// Module names a source file declares as standard host modules.
fn declared_host_modules(source: &str) -> BTreeSet<String> {
    let code = strip_comments(source);
    let mut names = BTreeSet::new();
    for marker in [
        "StandardHostModule {",
        "descriptor_only_module(",
        "catalog_module(",
    ] {
        let mut cursor = 0usize;
        while let Some(offset) = code[cursor..].find(marker) {
            let after = cursor + offset + marker.len();
            cursor = after;
            // The marker is found in the comment-stripped text (so prose does
            // not match), but the module name is a string literal and must be
            // read from the raw source at the same offset.
            if let Some(name) = first_string_literal(&source[after..]) {
                names.insert(name);
            }
        }
    }
    names
}

/// The first string literal in `source`, if one starts before any other token.
fn first_string_literal(source: &str) -> Option<String> {
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'"' => {
                let end = quoted_literal_end(bytes, cursor, b'"');
                return Some(source[cursor + 1..end.saturating_sub(1)].to_string());
            }
            b' ' | b'\t' | b'\r' | b'\n' => cursor += 1,
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                while cursor < bytes.len() && bytes[cursor] != b'\n' {
                    cursor += 1;
                }
            }
            _ => return None,
        }
    }
    None
}

/// Ownership lists that no module composed in this build owns.
///
/// `sources` maps each file that declares an ownership list to the modules it
/// declares and the descriptors it lists, `composed` maps every composed
/// module name to the guest names it owns, `guest_names` maps a descriptor
/// factory to the name its `#[pd_host_function]` declares (absent for the
/// descriptors a module builds by hand, which stay outside this check).
fn unknown_ownership_entries(
    sources: &BTreeMap<String, OwnershipSource>,
    guest_names: &BTreeMap<String, String>,
    composed: &BTreeMap<String, BTreeSet<String>>,
    disabled_modules: &BTreeSet<String>,
    shared_sources: &[(&str, &str)],
) -> Vec<String> {
    let mut offenders = Vec::new();
    for (file, source) in sources {
        let mut owners: BTreeSet<String> = source.modules.clone();
        if owners.is_empty() {
            match shared_sources.iter().find(|(path, _)| path == file) {
                Some((_, module)) => {
                    owners.insert((*module).to_string());
                }
                None => {
                    // A module this build's feature gates turn off keeps its
                    // sources in the tree; the guard cannot see the gate from
                    // the module name alone, so the disabled set is the
                    // exemption.
                    offenders.push(format!(
                        "{file} declares an ownership list but no standard host module"
                    ));
                    continue;
                }
            }
        }
        for owner in &owners {
            if !composed.contains_key(owner) && !disabled_modules.contains(owner) {
                offenders.push(format!(
                    "{file} declares an ownership list for `{owner}`, which no build composes"
                ));
            }
        }
        // Every list entry the guard can resolve to a declaration must be owned
        // by the module the declaring file belongs to.
        for entry in &source.entries {
            let Some(name) = guest_names.get(entry) else {
                continue;
            };
            // A module this build's gate turns off keeps its own list intact
            // and cannot be cross-checked against the composition.
            if owners.iter().any(|owner| disabled_modules.contains(owner)) {
                continue;
            }
            if owners.iter().any(|owner| {
                composed
                    .get(owner)
                    .is_some_and(|owned| owned.contains(name))
            }) {
                continue;
            }
            offenders.push(format!(
                "{file} lists `{entry}` (`{name}`) for {owners:?}, which does not own it"
            ));
        }
    }
    offenders
}

/// Focused scanner tests: the production scan must drop test items without
/// hiding the production declarations that follow them.
#[test]
fn production_scan_drops_test_items_without_hiding_later_declarations() {
    let source = "\
#[cfg(test)]
const _: () = assert!(SSE_CHANNEL_CAPACITY == 1);

/// Production declaration below a test-only constant.
#[pd_host_function(name = \"demo::late\")]
fn builtin_demo_late() -> i64 {
    0
}
\n";
    let stripped = strip_test_items(source);
    assert_eq!(
        stripped.len(),
        source.len(),
        "blanking a test item must preserve byte offsets"
    );
    assert_eq!(
        stripped.lines().count(),
        source.lines().count(),
        "blanking a test item must preserve line numbers"
    );
    assert!(
        !stripped.contains("assert!(SSE_CHANNEL_CAPACITY"),
        "the test item must not survive the production scan"
    );
    assert!(
        stripped.contains("#[pd_host_function(name = \"demo::late\")]"),
        "a production declaration after a test item must stay visible"
    );
}

/// Every shape a `#[cfg(test)]` marker takes in this crate is dropped, and the
/// production statements around it survive.
#[test]
fn production_scan_drops_test_items_in_every_declaration_shape() {
    let source = "\
fn body() {
    #[cfg(test)]
    if FAIL_NEXT.swap(false, Ordering::AcqRel) {
        return Err(\"injected\");
    }
    work();
}

#[cfg(test)]
static FAIL_NEXT: AtomicBool = AtomicBool::new(false);

impl Thing {
    #[cfg(test)]
    fn helper(&self) -> u32 {
        1
    }

    fn production(&self) -> u32 {
        2
    }
}

#[cfg(test)]
use super::{fixture_a, fixture_b};
\n";
    let stripped = strip_test_items(source);
    assert_eq!(stripped.len(), source.len(), "offsets must be preserved");
    for survivor in ["fn body()", "work();", "fn production(&self)", "impl Thing"] {
        assert!(stripped.contains(survivor), "`{survivor}` must survive");
    }
    for dropped in ["injected", "static FAIL_NEXT", "fn helper", "fixture_a"] {
        assert!(
            !stripped.contains(dropped),
            "test-only `{dropped}` must be dropped"
        );
    }
}

/// A raw string inside a test item may carry unbalanced braces and quotes; the
/// scan must skip it as a unit instead of reading its bytes as code.
#[test]
fn production_scan_skips_raw_strings_inside_test_items() {
    let source = "\
#[cfg(test)]
mod fixtures {
    const SCRIPT: &str = r#\"
        if [ -n \"$child\" ]; then { echo 'unbalanced' \"$child\" > '{}'
    \"#;
}

/// Production declaration after a raw-string fixture.
#[pd_host_function(name = \"demo::raw\")]
fn builtin_demo_raw() {}
\n";
    let stripped = strip_test_items(source);
    assert_eq!(stripped.len(), source.len(), "offsets must be preserved");
    assert!(
        !stripped.contains("unbalanced"),
        "the raw-string fixture must be dropped"
    );
    assert!(
        stripped.contains("#[pd_host_function(name = \"demo::raw\")]"),
        "a production declaration after a raw-string fixture must stay visible"
    );
}

/// The discriminating regression: a test-only constant early in a file used to
/// truncate the scan, hiding the `http::client::sse` declaration that follows
/// it. Ownership must still be scanned exactly once.
#[test]
fn sse_stream_declaration_is_scanned_and_owned_exactly_once() {
    let file = "src/builtins/runtime/http/sse.rs";
    let path = manifest_root().join(file);
    let raw = production_raw(&path);
    assert_eq!(
        raw.len(),
        fs::read_to_string(&path).expect("sse source").len(),
        "{file}: the production scan must preserve byte offsets"
    );
    assert!(
        raw.contains("#[pd_host_function(name = \"http::client::sse\""),
        "{file}: the production host declaration must survive the test-item scan"
    );

    let functions = standard_host_functions();
    let sse: Vec<&StandardHostFunction> = functions
        .iter()
        .filter(|function| function.name == "http::client::sse")
        .collect();
    assert_eq!(
        sse.len(),
        1,
        "the SSE stream host function must be discovered exactly once: {sse:?}"
    );
    assert_eq!(sse[0].file, file);
    assert_eq!(sse[0].descriptor, "builtin_http_client_sse_descriptor");

    let owners = declared_ownership();
    let claimants = owners
        .get("builtin_http_client_sse_descriptor")
        .expect("the SSE stream descriptor must have an ownership list");
    assert_eq!(
        claimants,
        &vec!["src/builtins/runtime/http/mod.rs".to_string()],
        "SSE ownership must be declared exactly once"
    );
}

/// Every file that declares an ownership list, with the modules it declares
/// and the descriptors it lists.
fn ownership_sources() -> BTreeMap<String, OwnershipSource> {
    let mut sources: BTreeMap<String, OwnershipSource> = BTreeMap::new();
    for (descriptor, files) in declared_ownership() {
        for file in files {
            sources
                .entry(file)
                .or_default()
                .entries
                .insert(descriptor.clone());
        }
    }
    for file in sources.keys().cloned().collect::<Vec<_>>() {
        let path = manifest_root().join(&file);
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        // The module declarations are read from the whole file; the entries
        // come from the production scan above.
        let modules = declared_host_modules(&source);
        sources.entry(file).or_default().modules = modules;
    }
    sources
}

/// Every guest name each module composed in this build owns.
fn composed_module_ownership() -> BTreeMap<String, BTreeSet<String>> {
    vm::standard_host_modules()
        .iter()
        .map(|module| {
            let owned = module
                .owned_descriptors()
                .into_iter()
                .map(|descriptor| descriptor.schema.name)
                .collect();
            (module.name.to_string(), owned)
        })
        .collect()
}

/// Module names this build's feature gates turn off.
fn disabled_standard_modules() -> BTreeSet<String> {
    let mut disabled = BTreeSet::new();
    if !HTTP_SURFACE_ENABLED {
        disabled.insert("http".to_string());
    }
    disabled
}

/// Every ownership list must belong to a module `standard_host_modules()`
/// composes in this build.
#[test]
fn every_ownership_list_belongs_to_a_composed_standard_module() {
    let sources = ownership_sources();
    assert!(
        sources.len() > 10,
        "the standard ownership lists must be discovered from the sources, found {}",
        sources.len()
    );
    let guest_names: BTreeMap<String, String> = standard_host_functions()
        .into_iter()
        .map(|function| (function.descriptor, function.name))
        .collect();
    let composed = composed_module_ownership();
    assert!(
        composed.values().map(BTreeSet::len).sum::<usize>() > 100,
        "the composed standard modules must own the discovered functions"
    );

    let offenders = unknown_ownership_entries(
        &sources,
        &guest_names,
        &composed,
        &disabled_standard_modules(),
        SHARED_MODULE_SOURCES,
    );
    assert!(
        offenders.is_empty(),
        "every ownership list must belong to a module composed in this build:\n{}",
        offenders.join("\n")
    );
}

/// The check above is only evidence if it can fail.
#[test]
fn ownership_check_reports_lists_no_composed_module_owns() {
    let composed = BTreeMap::from([
        ("io".to_string(), BTreeSet::from(["io::open".to_string()])),
        (
            "sqlite".to_string(),
            BTreeSet::from(["sqlite::open".to_string()]),
        ),
    ]);
    let disabled = BTreeSet::new();

    // A file that declares no module and no shared-module entry is an orphan.
    let orphan = BTreeMap::from([(
        "src/builtins/runtime/orphan.rs".to_string(),
        OwnershipSource {
            modules: BTreeSet::new(),
            entries: BTreeSet::new(),
        },
    )]);
    let offenders = unknown_ownership_entries(
        &orphan,
        &BTreeMap::new(),
        &composed,
        &disabled,
        SHARED_MODULE_SOURCES,
    );
    assert_eq!(offenders.len(), 1, "{offenders:?}");

    // A module the composition does not include is reported.
    let uncomposed = BTreeMap::from([(
        "src/builtins/runtime/ghost.rs".to_string(),
        OwnershipSource {
            modules: BTreeSet::from(["ghost".to_string()]),
            entries: BTreeSet::new(),
        },
    )]);
    let offenders = unknown_ownership_entries(
        &uncomposed,
        &BTreeMap::new(),
        &composed,
        &disabled,
        SHARED_MODULE_SOURCES,
    );
    assert_eq!(offenders.len(), 1, "{offenders:?}");
    assert!(offenders[0].contains("ghost"), "{offenders:?}");

    // A list entry the guard can resolve must be owned by the same module.
    let misfiled = BTreeMap::from([(
        "src/builtins/runtime/jit.rs".to_string(),
        OwnershipSource {
            modules: BTreeSet::from(["jit".to_string()]),
            entries: BTreeSet::from(["builtin_io_open_descriptor".to_string()]),
        },
    )]);
    let names = BTreeMap::from([(
        "builtin_io_open_descriptor".to_string(),
        "io::open".to_string(),
    )]);
    let mut composed = composed.clone();
    composed.insert(
        "jit".to_string(),
        BTreeSet::from(["jit::get_config".to_string()]),
    );
    let offenders = unknown_ownership_entries(
        &misfiled,
        &names,
        &composed,
        &disabled,
        SHARED_MODULE_SOURCES,
    );
    assert_eq!(offenders.len(), 1, "{offenders:?}");
    assert!(offenders[0].contains("io::open"), "{offenders:?}");

    // A feature-disabled module and a shared-module source are both accepted.
    let shared = BTreeMap::from([(
        "src/builtins/runtime/sqlite.rs".to_string(),
        OwnershipSource {
            modules: BTreeSet::new(),
            entries: BTreeSet::new(),
        },
    )]);
    assert!(
        unknown_ownership_entries(
            &shared,
            &BTreeMap::new(),
            &composed,
            &disabled,
            SHARED_MODULE_SOURCES,
        )
        .is_empty()
    );
    let gated = BTreeMap::from([(
        "src/builtins/runtime/http/mod.rs".to_string(),
        OwnershipSource {
            modules: BTreeSet::from(["http".to_string()]),
            entries: BTreeSet::new(),
        },
    )]);
    assert!(
        unknown_ownership_entries(
            &gated,
            &BTreeMap::new(),
            &composed,
            &BTreeSet::from(["http".to_string()]),
            SHARED_MODULE_SOURCES,
        )
        .is_empty()
    );
}

#[test]
fn every_standard_host_function_has_exactly_one_descriptor_owner() {
    let functions = standard_host_functions();
    assert!(
        functions.len() > 100,
        "the standard host inventory must be discovered from the sources, found {}",
        functions.len()
    );
    let owners = declared_ownership();
    assert!(
        !owners.is_empty(),
        "the standard host modules must declare explicit ownership lists"
    );

    let mut offenders = Vec::new();
    for function in &functions {
        match owners.get(&function.descriptor) {
            None => offenders.push(format!(
                "{}: `{}` ({} in {}) has no descriptor owner",
                function.file, function.name, function.descriptor, function.file
            )),
            Some(claimants) => {
                let unique: BTreeSet<&String> = claimants.iter().collect();
                if unique.len() != 1 {
                    offenders.push(format!(
                        "{}: `{}` is owned by more than one module: {claimants:?}",
                        function.file, function.name
                    ));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "every standard #[pd_host_function] must have exactly one descriptor owner:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn migrated_modules_keep_no_hand_written_catalog_or_registry_glue() {
    let forbidden = [
        "HostApiBuilder::new()",
        "builder.resource(",
        "builder.function(",
        "builder.named_struct(",
        "register_exact_static(",
        "register_exact_owned(",
        "ADAPTER_CONTRACTS",
    ];
    let mut offenders = Vec::new();
    for file in MIGRATED_MODULE_FILES {
        let path = manifest_root().join(file);
        if !path.is_file() {
            continue;
        }
        let code = production_code(&path);
        for symbol in forbidden {
            if code.contains(symbol) {
                offenders.push(format!("{file} → {symbol}"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "migrated host modules must derive their catalog and registry bindings \
         from descriptors:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn standard_catalog_is_derived_from_one_descriptor_per_module() {
    let modules = vm::standard_host_modules();
    assert!(!modules.is_empty(), "the standard host modules must exist");

    let names: Vec<&str> = modules.iter().map(|module| module.name).collect();
    let unique: BTreeSet<&str> = names.iter().copied().collect();
    assert_eq!(
        names.len(),
        unique.len(),
        "standard host module names must be unique: {names:?}"
    );
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(
        names, sorted,
        "the standard host module aggregation must be deterministic (sorted by name)"
    );

    for module in modules {
        assert!(
            !module.owned.is_empty(),
            "module '{}' must own at least one descriptor",
            module.name
        );
        let Some(surface) = module.catalog_module() else {
            assert!(
                module.catalog_module().is_none(),
                "a descriptor-only module must not publish a catalog surface"
            );
            continue;
        };
        assert!(
            !surface.functions.is_empty(),
            "module '{}' published an empty catalog surface",
            module.name
        );
        for factory in surface.functions {
            let descriptor = factory();
            assert!(
                module
                    .owned
                    .iter()
                    .any(|owned| owned().schema.name == descriptor.schema.name),
                "module '{}' publishes `{}` without owning it",
                module.name,
                descriptor.schema.name
            );
        }
    }

    let catalog = vm::standard_host_catalog();
    let merged_owner = vm::standard_catalog_modules();
    assert_eq!(
        merged_owner.len(),
        modules
            .iter()
            .filter(|m| m.catalog_module().is_some())
            .count(),
        "every catalog surface must be merged exactly once"
    );
    assert_eq!(
        catalog.functions().len(),
        merged_owner
            .iter()
            .map(|module| module.functions.len())
            .sum::<usize>(),
        "the standard catalog must be the merge of the module surfaces"
    );
    assert_eq!(
        catalog.fingerprint().to_string(),
        standard_catalog_fingerprint(),
        "the standard catalog fingerprint must be byte-for-byte unchanged for the composed module set"
    );
}

/// The HTTP catalog fingerprint check, absent when the feature is disabled.
#[cfg(all(feature = "http-client", not(target_family = "wasm")))]
fn http_catalog_surface() -> Option<(&'static str, &'static str, vm::HostApiFingerprint)> {
    Some((
        "http",
        HTTP_CATALOG_FINGERPRINT,
        vm::http_host_catalog().fingerprint(),
    ))
}

#[cfg(not(all(feature = "http-client", not(target_family = "wasm"))))]
fn http_catalog_surface() -> Option<(&'static str, &'static str, vm::HostApiFingerprint)> {
    None
}

#[test]
fn module_catalogs_keep_their_published_fingerprints() {
    let mut surfaces: Vec<(&str, &str, vm::HostApiFingerprint)> = vec![
        (
            "io",
            IO_CATALOG_FINGERPRINT,
            vm::io_host_catalog().fingerprint(),
        ),
        (
            "jit",
            JIT_CATALOG_FINGERPRINT,
            vm::jit_host_catalog().fingerprint(),
        ),
        (
            "timer",
            TIMER_CATALOG_FINGERPRINT,
            vm::timer_host_catalog().fingerprint(),
        ),
        (
            "sqlite",
            SQLITE_CATALOG_FINGERPRINT,
            vm::sqlite_host_catalog().fingerprint(),
        ),
    ];
    surfaces.extend(http_catalog_surface());
    for (name, golden, fingerprint) in surfaces {
        assert_eq!(
            fingerprint.to_string(),
            golden,
            "the `{name}` host catalog fingerprint must be byte-for-byte unchanged"
        );
    }
}

#[test]
fn every_catalog_resource_key_has_a_declaration() {
    let catalog = vm::standard_host_catalog();
    let mut undeclared = Vec::new();
    for resource in catalog.resources() {
        if resource.description.trim().is_empty() {
            undeclared.push(resource.key.to_string());
        }
    }
    assert!(
        undeclared.is_empty(),
        "every guest resource key must come from a HostResourceType declaration: {undeclared:?}"
    );

    let expected: BTreeSet<String> = [
        STANDARD_RESOURCE_KEYS,
        if HTTP_SURFACE_ENABLED {
            HTTP_RESOURCE_KEYS
        } else {
            &[]
        },
    ]
    .concat()
    .into_iter()
    .map(str::to_string)
    .collect();
    let actual: BTreeSet<String> = catalog
        .resources()
        .iter()
        .map(|resource| resource.key.to_string())
        .collect();
    assert_eq!(
        actual, expected,
        "the standard resource set must be unchanged for the composed module set"
    );
    if !HTTP_SURFACE_ENABLED {
        for key in HTTP_RESOURCE_KEYS {
            assert!(
                !actual.contains(*key),
                "`{key}` must not leak into a build without the HTTP module"
            );
        }
    }
}

#[test]
fn typed_named_struct_contract_is_preserved() {
    let catalog = vm::standard_host_catalog();
    let declared: BTreeSet<&str> = catalog
        .structs()
        .iter()
        .map(|schema| schema.name.as_str())
        .collect();
    for name in STANDARD_NAMED_STRUCTS {
        assert!(declared.contains(name), "named struct `{name}` is missing");
    }
    for name in HTTP_NAMED_STRUCTS {
        assert_eq!(
            declared.contains(name),
            HTTP_SURFACE_ENABLED,
            "named struct `{name}` must follow the HTTP module's composition gate"
        );
    }

    // The typed shapes themselves are unchanged, not just the names.
    let query = catalog
        .struct_named("SqliteQueryResult")
        .expect("SqliteQueryResult");
    assert_eq!(
        query.fields.len(),
        4,
        "SqliteQueryResult keeps columns/rows/truncated/next_cursor"
    );
    let value = catalog.struct_named("SqliteValue").expect("SqliteValue");
    assert_eq!(
        value.fields.len(),
        5,
        "SqliteValue keeps kind plus four typed payload slots"
    );

    // No public standard function regressed to a dynamic return.
    let mut dynamic = Vec::new();
    for function in catalog.functions() {
        if function.return_type == vm::HostTypeSchema::Unknown {
            dynamic.push(function.name.clone());
        }
    }
    assert!(
        dynamic.is_empty(),
        "a public host function must not return `unknown`: {dynamic:?}"
    );
}

#[test]
fn feature_gates_select_whole_modules_deterministically() {
    let names: BTreeSet<&str> = vm::standard_host_modules()
        .iter()
        .map(|module| module.name)
        .collect();
    for always in ["io", "jit", "timer", "regex", "core", "math"] {
        assert!(names.contains(always), "`{always}` must always be composed");
    }

    #[cfg(all(feature = "http-client", not(target_family = "wasm")))]
    assert!(
        names.contains("http"),
        "the http module must be composed when the feature is enabled"
    );
    #[cfg(not(all(feature = "http-client", not(target_family = "wasm"))))]
    assert!(
        !names.contains("http"),
        "the http module must be excluded when the feature is disabled"
    );

    // The SQLite catalog surface is composed in every build: the feature only
    // selects whether its adapters are the real host functions.
    assert!(
        names.contains("sqlite"),
        "the sqlite catalog surface must be composed in every build"
    );

    // A gated module must not leak into the derived guest catalog.
    let catalog = vm::standard_host_catalog();
    let has_http_function = catalog
        .functions()
        .iter()
        .any(|function| function.name.starts_with("http::"));
    assert_eq!(
        has_http_function,
        cfg!(all(feature = "http-client", not(target_family = "wasm"))),
        "the guest catalog must follow the same gate as the module"
    );
    assert!(
        catalog
            .functions()
            .iter()
            .any(|function| function.name.starts_with("sqlite::")),
        "the SQLite guest imports must resolve in every build"
    );
}

#[test]
fn legacy_registration_apis_stay_public_for_downstream_migration() {
    // The builder and the low-level registry stay usable: the descriptor path
    // is the preferred authoring surface, not the only one.
    let mut builder = vm::HostApiBuilder::new();
    builder.resource(vm::ResourceTypeSchema::new(
        vm::ResourceTypeKey::new("demo.legacy").expect("key"),
        "A legacy resource",
    ));
    builder.named_struct(vm::HostStructSchema::new("LegacyPoint", vec![]));
    builder.function(vm::HostFunctionSchema::with_return(
        "demo::legacy",
        vec![],
        vm::HostTypeSchema::Int,
    ));
    let catalog = builder.build().expect("the legacy builder still builds");

    let mut registry = vm::HostFunctionRegistry::empty();
    registry.register_static_stack("demo::legacy", 0, |_vm, _args| {
        Ok(vm::CallOutcome::Return(vm::CallReturn::None))
    });
    assert!(registry.contains_name("demo::legacy"));
    assert_eq!(catalog.functions().len(), 1);
}
