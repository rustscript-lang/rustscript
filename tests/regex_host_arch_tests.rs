//! Source-level architecture guard for regex host-private state.
//!
//! The regex cache belongs to the regex host module. `src/vm/**` owns the
//! *generic* host-private state table (type-erased storage, providers,
//! borrow guards) but no regex concept: no cache type, no `regex::Regex`
//! import, no regex-specific field, and no inherent regex cache API on `Vm`.
//!
//! These tests are source-only: they read the production sources next to this
//! manifest, so they run without executing any regex and without depending on
//! the working tree being committed (they scan tokens, not `git diff`).

use std::fs;
use std::path::{Path, PathBuf};

/// Regex-cache *ownership* tokens that must never appear in `src/vm/**`.
///
/// Generic host-state references (`HostState`, `HostStateMut`,
/// `HostStateProvider`, `host_state_mut`, ...) are explicitly allowed: they are
/// the generic mechanism the VM core owns.
const FORBIDDEN_VM_REGEX_TOKENS: &[&str] = &[
    "RegexCache",
    "regex_cache",
    "cached_regex",
    "get_or_compile",
    "regex::Regex",
    "regex::Error",
    "use regex",
    "regex =",
];

/// The module that owned the regex cache before it moved to the host module.
const REMOVED_VM_REGEX_MODULE: &str = "src/vm/regex_cache.rs";

/// The host-module files that own the cache after the migration.
const REGEX_OWNED_FILES: &[&str] = &[
    "src/builtins/runtime/regex.rs",
    "src/builtins/runtime/regex/cache.rs",
];

/// Tokens that prove the regex module owns cache state and configuration.
const REGEX_OWNERSHIP_TOKENS: &[&str] = &[
    "struct RegexCache",
    "DEFAULT_REGEX_CACHE_CAPACITY",
    "impl HostState for RegexCache",
    "trait RegexCacheVmExt",
    "impl RegexCacheVmExt for Vm",
    "HostStateMut<'_, RegexCache>",
];

/// Generic host-state machinery the VM core is allowed (and required) to own.
const GENERIC_VM_STATE_TOKENS: &[&str] = &[
    "HostStateProvider",
    "ensure_host_state",
    "host_state_mut",
    "HostStateError",
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

/// Every production `.rs` file under `src/vm/`, sorted for deterministic output.
fn vm_sources() -> Vec<PathBuf> {
    let vm_dir = manifest_root().join("src").join("vm");
    assert!(vm_dir.is_dir(), "expected a VM source directory");
    let mut files = Vec::new();
    collect_rs(&vm_dir, &mut files);
    files.sort();
    assert!(!files.is_empty(), "expected production files under src/vm");
    files
}

fn relative(path: &Path) -> String {
    path.strip_prefix(manifest_root())
        .expect("source under manifest root")
        .to_string_lossy()
        .replace('\\', "/")
}

/// Removes `//` line comments and `/* ... */` block comments so the guards
/// inspect code rather than prose that merely *discusses* the boundary.
fn strip_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let bytes = source.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[test]
fn vm_layer_owns_no_regex_cache_state_or_api() {
    for file in vm_sources() {
        let source = fs::read_to_string(&file).expect("read vm source");
        let code = strip_comments(&source);
        for forbidden in FORBIDDEN_VM_REGEX_TOKENS {
            assert!(
                !code.contains(forbidden),
                "`src/vm` file `{}` must not own regex cache state (`{forbidden}`)",
                relative(&file)
            );
        }
    }
}

#[test]
fn former_vm_regex_cache_module_is_removed() {
    let path = manifest_root().join(REMOVED_VM_REGEX_MODULE);
    assert!(
        !path.exists(),
        "`{REMOVED_VM_REGEX_MODULE}` must be deleted; the cache lives in the regex host module"
    );
    let module_declaration =
        fs::read_to_string(manifest_root().join("src/vm/mod.rs")).expect("read src/vm/mod.rs");
    assert!(
        !module_declaration.contains("regex_cache"),
        "src/vm/mod.rs must not declare a regex cache module"
    );
}

#[test]
fn regex_host_module_owns_the_cache_its_lifecycle_and_its_api() {
    for relative_path in REGEX_OWNED_FILES {
        let path = manifest_root().join(relative_path);
        assert!(
            path.is_file(),
            "`{relative_path}` must own part of the regex cache"
        );
    }
    let owned = REGEX_OWNED_FILES
        .iter()
        .map(|relative_path| {
            fs::read_to_string(manifest_root().join(relative_path))
                .unwrap_or_else(|error| panic!("read {relative_path}: {error}"))
        })
        .collect::<Vec<_>>()
        .join("\n");
    for token in REGEX_OWNERSHIP_TOKENS {
        assert!(
            owned.contains(token),
            "the regex host module must own `{token}`"
        );
    }
}

#[test]
fn every_interpreter_regex_host_declares_the_private_state_effect() {
    let source = fs::read_to_string(manifest_root().join("src/builtins/runtime/regex.rs"))
        .expect("read the regex host module");
    let host_count = source.matches("#[pd_host_function(name = \"re::").count();
    let state_parameters = source.matches("HostStateMut<'_, RegexCache>").count();
    assert!(
        host_count >= 5,
        "expected every re::* host (found {host_count})"
    );
    assert!(
        state_parameters >= host_count,
        "every re::* host must take the hidden per-VM cache (hosts: {host_count}, \
         state parameters: {state_parameters})"
    );

    // No annotated host may take a raw `&mut Vm`; only the native/JIT/AOT
    // shortcuts do, and they resolve the same per-VM cache from host state.
    // Each host is inspected through its own signature (up to the body brace).
    let mut remainder = source.as_str();
    let mut inspected = 0usize;
    while let Some(index) = remainder.find("#[pd_host_function(name = \"re::") {
        let rest = &remainder[index..];
        let signature_end = rest.find('{').expect("a host signature always has a body");
        let signature = &rest[..signature_end];
        assert!(
            signature.contains("HostStateMut<'_, RegexCache>"),
            "regex hosts must resolve the cache through host state: {signature}"
        );
        assert!(
            !signature.contains("&mut Vm"),
            "regex hosts must not take a raw Vm parameter: {signature}"
        );
        inspected += 1;
        remainder = &remainder[index + 1..];
    }
    assert_eq!(inspected, host_count, "every re::* host must be inspected");
}

#[test]
fn generic_host_state_machinery_stays_available_and_regex_free() {
    let host_state = fs::read_to_string(manifest_root().join("src/vm/host_state.rs"))
        .expect("read the generic host-state module");
    for token in GENERIC_VM_STATE_TOKENS {
        assert!(
            host_state.contains(token),
            "the generic VM layer must keep owning `{token}`"
        );
    }
    assert!(
        !host_state.to_lowercase().contains("regex"),
        "the generic host-state table must not know about regex"
    );
}
