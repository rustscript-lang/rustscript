//! Architecture tests for the generic host-context boundary.
//!
//! These tests verify two properties that the host-context commit guarantees:
//!
//! 1. **Boundary hygiene** — `src/vm` (and, in particular, the boundary file
//!    `src/vm/host_context.rs`) does not import builtin *domain* modules
//!    (`sqlite`, `io`, `http`, `json`, ...) nor `rusqlite`. Standard SQLite /
//!    IO / HTTP / SSE remain same-crate builtins; `src/vm` only owns the generic
//!    boundary and must stay domain-agnostic.
//! 2. **Generic external registration** — an external host *extension* registers
//!    typed, per-VM module state purely through the public [`HostContext`]
//!    surface, without ever touching host-runtime internals (which stay private)
//!    or a builtin domain type.

use std::fs;
use std::path::{Path, PathBuf};

use vm::{Program, Vm};

/// The builtin *domain* modules that `src/vm` must not import.
const FORBIDDEN_DOMAIN_IMPORTS: &[&str] = &[
    "builtins::runtime::sqlite",
    "builtins::runtime::io",
    "builtins::runtime::http",
    "builtins::runtime::json",
    "builtins::runtime::typed",
];

/// `rusqlite` must never appear in `src/vm`.
const FORBIDDEN_RUSQLITE: &str = "rusqlite";

fn vm_source_files() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let vm_dir = root.join("src").join("vm");
    let mut files = Vec::new();
    let mut stack = vec![vm_dir.clone()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read src/vm directory") {
            let entry = entry.expect("read dir entry");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
                files.push(path);
            }
        }
    }
    assert!(
        !files.is_empty(),
        "expected to find source files under {}",
        vm_dir.display()
    );
    files
}

/// Removes `//` line comments and `/* ... */` block comments so the import
/// guards inspect actual code (imports / inline paths) rather than doc prose
/// that merely *discusses* the boundary rules.
fn strip_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(pos) = rest.find("//").or_else(|| rest.find("/*")) {
        let is_line = rest[pos..].starts_with("//");
        out.push_str(&rest[..pos]);
        if is_line {
            let tail = &rest[pos..];
            let line_end = tail.find('\n').map(|n| pos + n + 1).unwrap_or(rest.len());
            out.push('\n');
            rest = &rest[line_end..];
        } else {
            let tail = &rest[pos + 2..];
            let block_end = tail.find("*/").map(|n| pos + 2 + n + 2);
            match block_end {
                Some(end) => {
                    out.push('\n');
                    rest = &rest[end..];
                }
                None => {
                    out.push('\n');
                    rest = "";
                }
            }
        }
    }
    out.push_str(rest);
    out
}

#[test]
fn src_vm_never_imports_builtin_domain_modules_or_rusqlite() {
    let offenders = vm_source_files()
        .into_iter()
        .filter_map(|path| {
            let raw = fs::read_to_string(&path).expect("read source file");
            let source = strip_comments(&raw);
            let mut matched = Vec::new();
            for forbidden in FORBIDDEN_DOMAIN_IMPORTS {
                if source.contains(forbidden) {
                    matched.push((*forbidden).to_string());
                }
            }
            if source.contains(FORBIDDEN_RUSQLITE) {
                matched.push(FORBIDDEN_RUSQLITE.to_string());
            }
            if matched.is_empty() {
                None
            } else {
                Some((path, matched))
            }
        })
        .collect::<Vec<_>>();

    assert!(
        offenders.is_empty(),
        "src/vm must not import builtin domain modules or rusqlite; found:\n{}",
        offenders
            .iter()
            .map(|(p, m)| format!("  {} → {}", p.display(), m.join(", ")))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn host_context_boundary_file_is_builtin_free() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("vm")
        .join("host_context.rs");
    let raw = fs::read_to_string(&path).expect("read host_context.rs");
    let source = strip_comments(&raw);
    assert!(
        !source.contains("builtins::") && !source.contains("rusqlite"),
        "the HostContext boundary file itself must be fully host-agnostic \
         (no builtins:: and no rusqlite)"
    );
    for contract in ["pub struct HostContext", "pub trait HostModule"] {
        assert!(
            raw.contains(contract),
            "expected `{contract}` in host_context.rs"
        );
    }
}

// ---------------------------------------------------------------------------
// Generic external-registration proof
// ---------------------------------------------------------------------------

/// An "external" host extension state type, defined outside builtins. Because
/// `HostModule` is implemented on-disk through a blanket marker, any `Send`
/// value can be registered as typed per-VM module state.
#[derive(Clone, Debug, PartialEq)]
struct CounterState {
    count: u32,
}

#[derive(Clone, Debug, PartialEq)]
struct FlagState {
    enabled: bool,
}

#[test]
fn external_host_extension_registers_typed_state_through_generic_surface() {
    let mut vm = Vm::new(Program::new(vec![], vec![]));

    // Freshly registered value is new (no replacement).
    {
        let mut cx = vm.host_context();
        assert!(!cx.set_module_state(CounterState { count: 7 }));
        cx.set_module_state(FlagState { enabled: true });
        assert!(!cx.is_module_state_empty());
    }

    // Distinct types coexist; retrieval is typed.
    {
        let cx = vm.host_context();
        assert_eq!(cx.module_state::<CounterState>().unwrap().count, 7);
        assert!(cx.module_state::<FlagState>().unwrap().enabled);
    }

    // Mutable borrow + replacement semantics.
    {
        let mut cx = vm.host_context();
        cx.module_state_mut::<CounterState>().unwrap().count += 1;
        assert_eq!(cx.module_state::<CounterState>().unwrap().count, 8);
        assert!(cx.set_module_state(CounterState { count: 0 }));
    }

    // Remove.
    {
        let mut cx = vm.host_context();
        assert_eq!(
            cx.take_module_state::<FlagState>(),
            Some(FlagState { enabled: true })
        );
        assert!(cx.module_state::<FlagState>().is_none());
    }

    // is-empty after removing the last entry.
    {
        let mut cx = vm.host_context();
        cx.take_module_state::<CounterState>();
        assert!(cx.is_module_state_empty());
    }
}

#[test]
fn host_module_state_survives_invocation_reset() {
    let mut vm = Vm::new(Program::new(vec![], vec![]));

    {
        let mut cx = vm.host_context();
        cx.set_module_state(CounterState { count: 7 });
    }
    // A later invocation reset must NOT clear registered module state.
    vm.reset_for_reuse();
    {
        let cx = vm.host_context();
        assert_eq!(cx.module_state::<CounterState>().unwrap().count, 7);
    }
}
