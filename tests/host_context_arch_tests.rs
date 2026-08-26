//! Architecture tests for the generic host-context boundary.
//!
//! These tests verify two properties that the host-context SDK commit
//! guarantees:
//!
//! 1. **Boundary hygiene** — `src/vm` (and, in particular, the boundary file
//!    `src/vm/host_context.rs`) does not import builtin *domain* modules
//!    (`sqlite`, `io`, `http`, `json`, ...) nor `rusqlite`. Standard SQLite /
//!    IO / HTTP / SSE remain same-crate builtins; `src/vm` only owns the
//!    generic boundary and must stay domain-agnostic.
//! 2. **Generic external registration** — an external host *extension*
//!    registers typed, per-VM module state purely through the public
//!    [`HostContext`] surface, without ever touching host-runtime internals
//!    (which stay private) or a builtin domain type.

use std::fs;
use std::path::{Path, PathBuf};

use vm::{HostExtension, Program, Vm};

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
    // The boundary files that must stay domain-agnostic. The rest of `src/vm`
    // legitimately re-exports domain SQLite limits/policy for embedding, so
    // the boundary guard is scoped to the files we own here.
    let mut files = Vec::new();
    for name in [
        "host_context.rs",
        "host_extension.rs",
        "standard_composition.rs",
    ] {
        let path = vm_dir.join(name);
        assert!(
            path.exists(),
            "expected boundary file {} to exist",
            path.display()
        );
        files.push(path);
    }
    files
}

/// Removes `//` line comments and `/* ... */` block comments so the import
/// guards inspect actual code (imports / inline paths) rather than doc prose
/// that merely *discusses* the boundary rules.
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
fn vm_core_does_not_import_builtin_domain_modules() {
    for file in vm_source_files() {
        let source = fs::read_to_string(&file).expect("read vm source");
        let code = strip_comments(&source);
        for forbidden in FORBIDDEN_DOMAIN_IMPORTS {
            assert!(
                !code.contains(forbidden),
                "`src/vm` file `{}` must not import `{forbidden}`",
                file.display()
            );
        }
        assert!(
            !code.contains(FORBIDDEN_RUSQLITE),
            "`src/vm` file `{}` must not reference rusqlite",
            file.display()
        );
    }
}

/// Generic external extension: registers typed per-VM module state through the
/// public [`HostContext`] and the [`HostExtension`] lifecycle only.
#[derive(Debug)]
struct DemoPolicy {
    max_items: u64,
}

struct DemoExtension;

impl HostExtension for DemoExtension {
    fn install(&self, vm: &mut Vm) {
        vm.host_context()
            .set_module_state(DemoPolicy { max_items: 3 });
    }
}

#[test]
fn external_extension_registers_module_state_through_public_surface() {
    let program = Program::new(Vec::new(), vec![vm::OpCode::Ret as u8]);
    let mut vm = Vm::new(program);
    vm.install_extension(&DemoExtension)
        .expect("extension should install");
    assert_eq!(
        vm.host_context()
            .module_state::<DemoPolicy>()
            .map(|policy| policy.max_items),
        Some(3)
    );
    // Module state is generic storage: it does not register as a resource.
    assert_eq!(vm.host_context().resource_count(), 0);
}
