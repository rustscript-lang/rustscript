//! Source-level architecture boundary for the host-agnostic resource scope.
//!
//! SQLite is an optional, same-crate builtin that consumes the *generic* host
//! SDK. This test proves the core stays domain-agnostic: nothing under
//! `src/vm`, the generic resource/operation cores, or `ExecutionScope` may
//! import the SQLite builtin or `rusqlite`, define a domain resource-type
//! constant or operation-owner variant, or dispatch on a SQLite owner/type.
//!
//! The scan is source-only (it reads the manifest-adjacent source files), so
//! it runs under `--no-default-features --features runtime` without the
//! sqlite feature, and it deliberately inspects *production* code paths
//! rather than comments, string literals, or test fixtures (which unavoidably
//! name the forbidden tokens while discussing them).

use std::fs;
use std::path::{Path, PathBuf};

/// Recursively enumerate production `.rs` files under `src/vm`, excluding
/// `#[cfg(test)]`-only modules (unit-test and fixture harnesses).
fn core_sources() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/vm");
    let mut files = Vec::new();
    collect(&root, &mut files);
    files.retain(|path| {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        name != "tests.rs" && name != "host_stream_tests.rs"
    });
    files.sort();
    files
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read source directory") {
        let path = entry.expect("readable entry").path();
        let metadata = fs::metadata(&path).expect("source metadata");
        if metadata.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Blank the body of every `#[cfg(test)]`-attributed module/function, so test
/// fixtures (which exist to *discuss* the forbidden tokens) never trip the
/// production boundary scan.
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
        // Skip any further `#[...]` attributes before the item kind.
        while code.starts_with('#') {
            let Some(attr_end) = code.find(']') else {
                break;
            };
            code = code[attr_end + 1..].to_string();
            code = code.trim_start().to_string();
        }
        // Expect `mod <name> {` or `fn <name>(...) {`.
        if code.find('{').is_none() {
            // No body: just drop the attribute and continue on.
            continue;
        }
        // Blank from the opening brace to its matching close.
        let mut depth = 0usize;
        let mut close = None;
        for (i, byte) in code[..].bytes().enumerate() {
            match byte {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let body_end = match close {
            Some(i) => i + 1,
            None => code.len(),
        };
        let blanked: String = code[..body_end]
            .chars()
            .map(|character| if character == '\n' { '\n' } else { ' ' })
            .collect();
        out.push_str(&blanked);
        code = code[body_end..].to_string();
    }
    out
}

/// Comments and string/char literals blanked, then `#[cfg(test)]` fixture
/// bodies removed, preserving production code tokens and newlines.
fn sanitize(source: &str) -> String {
    let blanked = blank_literals(source);
    strip_cfg_test_blocks(blanked)
}

fn blank_literals(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    out.push(b' ');
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    if bytes[index] == b'\n' {
                        out.push(b'\n');
                    } else {
                        out.push(b' ');
                    }
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
            }
            b'"' => {
                out.push(b' ');
                index += 1;
                while index < bytes.len() && bytes[index] != b'"' {
                    if bytes[index] == b'\\' && index + 1 < bytes.len() {
                        out.push(b' ');
                        out.push(b' ');
                        index += 2;
                        continue;
                    }
                    if bytes[index] == b'\n' {
                        out.push(b'\n');
                    } else {
                        out.push(b' ');
                    }
                    index += 1;
                }
                if index < bytes.len() {
                    out.push(b' ');
                    index += 1;
                }
            }
            b'\'' => {
                // Char/byte-char literal (not a lifetime: lifetime names are
                // single-quoted with no closing quote).
                let next = bytes.get(index + 1).copied();
                let l = match next {
                    Some(b'\\') => 3,
                    Some(b'\'') => 0,
                    Some(c) if c.is_ascii_alphabetic() || c == b'_' => {
                        // Could be a lifetime (`'a`) or a char literal (`'a'`).
                        let mut j = index + 1;
                        while j < bytes.len()
                            && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_')
                        {
                            j += 1;
                        }
                        if bytes.get(j) == Some(&b'\'') {
                            1
                        } else {
                            usize::MAX // lifetime: keep as code
                        }
                    }
                    Some(_) => 1,
                    None => usize::MAX,
                };
                if l == usize::MAX {
                    out.push(b'\'');
                    index += 1;
                    continue;
                }
                out.push(b' ');
                out.push(b' ');
                index += 1;
                while index < bytes.len() && bytes[index] != b'\'' {
                    if bytes[index] == b'\\' && index + 1 < bytes.len() {
                        out.push(b' ');
                        out.push(b' ');
                        index += 2;
                        continue;
                    }
                    out.push(b' ');
                    index += 1;
                }
                if index < bytes.len() {
                    out.push(b' ');
                    index += 1;
                }
            }
            _ => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).expect("sanitized source is valid UTF-8")
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn contains_token(code: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    let bytes = code.as_bytes();
    let mut index = 0usize;
    while index + needle.len() <= bytes.len() {
        if &bytes[index..index + needle.len()] == needle {
            let before_ok = index == 0 || !is_ident_byte(bytes[index - 1]);
            let after_ok =
                index + needle.len() == bytes.len() || !is_ident_byte(bytes[index + needle.len()]);
            if before_ok && after_ok {
                return true;
            }
        }
        index += 1;
    }
    false
}

fn contains_substring(code: &str, needle: &str) -> bool {
    code.contains(needle)
}

/// Production source under the boundary, sanitized and ready for scanning.
fn scanned_core() -> Vec<(PathBuf, String)> {
    core_sources()
        .into_iter()
        .map(|path| {
            let source = fs::read_to_string(&path).expect("read core source");
            let code = sanitize(&source);
            (path, code)
        })
        .collect()
}

#[test]
fn vm_core_never_imports_sqlite_or_rusqlite() {
    let sources = scanned_core();
    assert!(
        !sources.is_empty(),
        "the core boundary scan must find production sources"
    );
    for (path, code) in &sources {
        // The SQLite builtin must never be referenced from the core (as an
        // import path or a named domain type). The `sqlite` crate-internal
        // builtin module lives in `builtins::runtime` and must stay unreached.
        assert!(
            !contains_token(code, "sqlite::")
                && !contains_token(code, "Sqlite")
                && !contains_token(code, "SQLITE"),
            "{} must not reference the sqlite builtin from the core",
            path.display(),
        );
        // rusqlite is the concrete host binding; the core must not link it.
        assert!(
            !contains_token(code, "rusqlite"),
            "{} must not import the rusqlite host binding",
            path.display(),
        );
        // The sqlite builtin module must never be imported from the core
        // (the generic resource/operation SDK is the only bridge).
        assert!(
            !contains_substring(code, "runtime::sqlite") && !contains_substring(code, "::sqlite::"),
            "{} must not import the sqlite builtin module",
            path.display(),
        );
    }
}

/// The concrete standard-surface registration entrypoints must never be
/// invoked from `src/vm`. Staging which same-crate builtin surfaces a program
/// needs (IO, HTTP, SQLite) is a *composition* decision that belongs in the
/// standard builtin layer (`builtins::runtime`), not the host-agnostic core.
///
/// The earlier token scan only matched a standalone `sqlite` identifier;
/// identifiers such as `register_sqlite_builtin_module` embedded `sqlite`
/// between identifier characters and slipped through. This pin catches the
/// full registration entrypoint names so any direct concrete staging from the
/// core fails the gate.
#[test]
fn vm_core_never_stages_concrete_standard_surfaces() {
    let sources = scanned_core();
    assert!(
        !sources.is_empty(),
        "the core boundary scan must find production sources"
    );
    // These identifiers are the concrete same-crate builtin registration
    // entrypoints. Presence of any of them in `src/vm` means the host-agnostic
    // core is directly coupling to a concrete standard domain module.
    let staging_entrypoints = [
        "register_io_builtin_module",
        "register_http_builtin_module",
        "register_sqlite_builtin_module",
    ];
    for (path, code) in &sources {
        for name in staging_entrypoints {
            assert!(
                !contains_token(code, name),
                "{} must not register the concrete standard surface `{name}`; \
                 standard-surface staging belongs in the builtin composition layer",
                path.display(),
            );
        }
    }
}

#[test]
fn execution_scope_has_no_sqlite_dispatch() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/vm/execution_scope.rs");
    let code = sanitize(&fs::read_to_string(&path).expect("read execution_scope source"));
    assert!(
        !contains_token(&code, "Sqlite"),
        "ExecutionScope carries no sqlite type"
    );
    assert!(
        !contains_token(&code, "rusqlite"),
        "ExecutionScope carries no rusqlite binding"
    );
    assert!(
        !contains_substring(&code, "cancel_operations_by_owner")
            && !contains_substring(&code, "close_resources_by_type"),
        "ExecutionScope must not call the retired owner/type dispatch helpers"
    );
}

#[test]
fn vm_reset_uses_no_domain_owner_or_type_dispatch() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/vm/mod.rs");
    let code = sanitize(&fs::read_to_string(&path).expect("read vm/mod.rs source"));
    for (needle, label) in [
        ("close_resources_by_type", "close_resources_by_type"),
        ("cancel_operations_by_owner", "cancel_operations_by_owner"),
        ("OperationOwner", "OperationOwner"),
        ("ResourceTypeId::", "domain ResourceTypeId constant"),
    ] {
        assert!(
            !code.contains(needle),
            "vm reset/execution path must not dispatch by {label}"
        );
    }
}

/// The host-agnostic VM core must not *compose* the standard host surfaces.
///
/// Beyond the concrete `register_*_builtin_module` entrypoints (pinned by
/// `vm_core_never_stages_concrete_standard_surfaces`), the core must also not
/// reach into the standard builtin composition layer through wrapper
/// functions or classify imports by concrete `io::` / `http::` / `sqlite::`
/// namespaces. Composition, missing-surface staging, default-registry
/// construction and default host-function fallback all belong to the standard
/// builtin layer; `src/vm` consumes only generic caller-provided registry /
/// catalog / binding abstractions and exact `HostImport` schemas.
///
/// Each needle below is an identifier that names a concrete standard-surface
/// composition entrypoint (or the concrete surface-flag struct). The scan is
/// token-based after comments/strings/fixtures are sanitized, so doc comments
/// discussing the boundary never trip it.
#[test]
fn vm_core_never_composes_standard_surfaces() {
    let sources = scanned_core();
    assert!(
        !sources.is_empty(),
        "the core boundary scan must find production sources"
    );
    let composition_entrypoints = [
        // Catalog composition / fingerprint access.
        "standard_host_catalog",
        "standard_host_catalog_fingerprint",
        // Concrete namespace classification of exact imports.
        "standard_exact_surface_requirements",
        // The concrete surface-flag struct (io/http/database).
        "StandardSurfaces",
        // Missing-surface staging on a registry.
        "stage_missing_standard_surfaces",
        // Fresh full-standard default registry construction.
        "standard_host_registry",
        // Legacy by-name default host-function fallback.
        "bind_default_host_function",
    ];
    for (path, code) in &sources {
        for name in composition_entrypoints {
            assert!(
                !contains_token(code, name),
                "{} must not invoke the standard composition entrypoint `{name}`; \
                 standard catalog composition / missing-surface staging / default \
                 registry / default fallback belong in the builtin composition layer",
                path.display(),
            );
        }
    }
}

/// The host-agnostic VM core must never classify exact imports by a concrete
/// standard namespace prefix (`io::`, `http::`, `sqlite::`). Surface
/// classification is a composition-layer concern: the core only sees opaque
/// surface flags supplied by the caller-provided composition abstraction.
///
/// The structural vehicle of namespace classification is a core helper that
/// receives a concrete namespace prefix and reports whether that surface is
/// already registered (`has_standard_surface`). After the dependency
/// inversion the core no longer defines it — surface presence is computed by
/// the composition implementation from the generic `exact_entries`
/// enumeration.
#[test]
fn vm_core_never_classifies_imports_by_concrete_namespace() {
    let sources = scanned_core();
    assert!(
        !sources.is_empty(),
        "the core boundary scan must find production sources"
    );
    for (path, code) in &sources {
        assert!(
            !contains_token(code, "has_standard_surface"),
            "{} must not classify imports by concrete namespace via `has_standard_surface`; \
             concrete surface classification belongs in the builtin composition layer",
            path.display(),
        );
    }
}

/// The legacy parallel resource system (`ResourceArena`,
/// `ResourceTypeId::{IO_FILE,CALLBACK,...}`) is retired. Production code must
/// not define or use it; the generic `vm::resource::ResourceTable` /
/// `ExecutionScope` contract is the single resource authority. Test fixtures
/// (the `tests/` tree and `#[cfg(test)]` modules) are deliberately not
/// scanned: they exist to discuss the forbidden tokens.
///
/// The scan covers the two former homes of the retired system — the VM core
/// and the standard builtin runtime layer. (A whole-`src/` scan would drag in
/// unrelated compiler files whose `#[cfg(test)]` block structure the
/// sanitizer's brace-balancer does not handle; those files can never define
/// the retired arena anyway.)
#[test]
fn production_never_defines_or_uses_legacy_resource_arena() {
    let mut files = Vec::new();
    for dir in ["src/vm", "src/builtins/runtime"] {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(dir);
        collect(&root, &mut files);
    }
    files.retain(|path| {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        name != "tests.rs" && name != "host_stream_tests.rs"
    });
    files.sort();
    files.dedup();
    assert!(!files.is_empty(), "production sources must be scanned");
    for path in files {
        let source = fs::read_to_string(&path).expect("read production source");
        let code = sanitize(&source);
        for (needle, label) in [
            ("ResourceArena", "ResourceArena"),
            ("ResourceTypeId", "ResourceTypeId"),
            ("IO_FILE", "domain resource constant IO_FILE"),
            ("CALLBACK", "domain resource constant CALLBACK"),
        ] {
            assert!(
                !contains_token(&code, needle),
                "{} must not define or use the retired {label}",
                path.display(),
            );
        }
    }
}

#[test]
fn resource_core_and_operation_core_are_domain_free() {
    for dir in ["src/vm/resource", "src/vm/operation"] {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(dir);
        let mut files = Vec::new();
        collect(&root, &mut files);
        files.sort();
        assert!(!files.is_empty(), "{dir} must contain production sources");
        for path in files {
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == "mod.rs")
            {
                // The architecture-gate test modules inside mod.rs construct
                // forbidden needles at runtime; skip nothing, sanitize first.
            }
            let code = sanitize(&fs::read_to_string(&path).expect("read core source"));
            assert!(
                !contains_token(&code, "Sqlite")
                    && !contains_token(&code, "rusqlite")
                    && !contains_substring(&code, "::builtins::"),
                "{} (in {dir}) must stay domain-free",
                path.display(),
            );
        }
    }
}

/// Guest-facing raw handles crossing the host boundary are generic integer
/// tokens owned by the scope; the core never needs to name a sqlite
/// connection class. This proves the only "sqlite" spellings in the core are
/// inside test-gated discussion, never production tokens.
#[test]
fn core_source_manifest_has_no_production_sqlite_identifier() {
    let sources = scanned_core();
    let offenders: Vec<String> = sources
        .into_iter()
        .filter(|(_, code)| contains_token(code, "sqlite"))
        .map(|(path, _)| path.display().to_string())
        .collect();
    assert!(
        offenders.is_empty(),
        "production core files must not even use a lowercase `sqlite` identifier: {offenders:?}"
    );
}
