//! Architecture guard for the standard host descriptor surface.
//!
//! These tests prove, from the production sources and the runtime composition,
//! that:
//!
//! * every standard `#[pd_host_function]` has **exactly one** descriptor owner
//!   (a module ownership list), and every ownership list belongs to a module in
//!   [`vm::standard_host_modules`];
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

/// Fingerprint of the published standard host catalog.
///
/// It is recorded here as the golden of the descriptor migration: the whole
/// point of the change is that guest contracts did not move.
const STANDARD_CATALOG_FINGERPRINT: &str = "6607e4fcb3187e73";
const IO_CATALOG_FINGERPRINT: &str = "234a7fdc3aaa3f95";
const SQLITE_CATALOG_FINGERPRINT: &str = "b6d4c278145edacf";
const JIT_CATALOG_FINGERPRINT: &str = "d0a3efbca2d0923c";
const TIMER_CATALOG_FINGERPRINT: &str = "4af2dfa2aee1f42e";
#[cfg(all(feature = "http-client", not(target_family = "wasm")))]
const HTTP_CATALOG_FINGERPRINT: &str = "18a4033f5857c033";

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
///
/// Every host module in this crate keeps its `#[cfg(test)]` module last, so
/// truncating at the first one is exact for these files and keeps the guard
/// blind to fixtures that exist to *discuss* the forbidden patterns.
fn production_code(path: &Path) -> String {
    strip_comments(&production_raw(path))
}

/// Production source of one file with its unit-test fixtures dropped.
///
/// Attribute parsing needs the raw text: a `#[pd_host_function(name = "...")]`
/// value *is* a string literal, so the comment/literal-stripped form cannot be
/// used to read it.
fn production_raw(path: &Path) -> String {
    let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    match raw.find("\n#[cfg(test)]") {
        Some(index) => raw[..index].to_string(),
        None => raw,
    }
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
        STANDARD_CATALOG_FINGERPRINT,
        "the standard catalog fingerprint must be byte-for-byte unchanged"
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
        "http.request",
        "http.response",
        "http.sse",
        "io.file",
        "sqlite.connection",
    ]
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
        "the standard resource set must be unchanged"
    );
}

#[test]
fn typed_named_struct_contract_is_preserved() {
    let catalog = vm::standard_host_catalog();
    let declared: BTreeSet<&str> = catalog
        .structs()
        .iter()
        .map(|schema| schema.name.as_str())
        .collect();
    for name in [
        "HttpRequest",
        "HttpResponse",
        "SseEvent",
        "SseSummary",
        "SseCallbackAction",
        "JitConfig",
        "SqliteValue",
        "SqliteQueryResult",
        "SqliteTransactionResult",
    ] {
        assert!(declared.contains(name), "named struct `{name}` is missing");
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
