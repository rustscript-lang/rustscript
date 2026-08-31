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

/// The generic VM must not depend on the builtin registration/adapter crate
/// path. Standard registration remains outside the recursive VM production
/// tree.
const FORBIDDEN_BUILTIN_CRATE_PATH: &str = "crate::builtins";

/// Concrete adapter state, policy, and dispatch symbols do not belong in the
/// generic VM. Keep these tokens explicit so a future adapter integration
/// cannot quietly reintroduce a domain branch under a different module.
const FORBIDDEN_ADAPTER_TOKENS: &[&str] = &[
    "BuiltinIo",
    "BuiltinSqlite",
    "poll_builtin_",
    "poll_builtin_io",
    "poll_builtin_io_op",
    "poll_builtin_sqlite",
    "poll_builtin_sqlite_op",
    "cancel_builtin_",
    "cancel_builtin_io",
    "cancel_builtin_io_op",
    "cancel_builtin_sqlite",
    "cancel_builtin_sqlite_op",
    "IoHostExt",
    "SqliteHostExt",
    "IoHostState",
    "SqliteHostState",
    "IoState",
    "SqliteState",
    "IoPolicy",
    "SqlitePolicy",
    "IoLimits",
    "SqliteLimits",
    "IoResource",
    "SqliteResource",
    "IoHandle",
    "ConnectionSlot",
    "IoOpDriver",
    "SqliteOpDriver",
    "IoOpShared",
    "SqliteOpShared",
    "io_policy",
    "sqlite_policy",
    "sqlite_state",
    "current_policy",
    "OperationOwner::Sqlite",
    "ResourceTypeId::IO_FILE",
    "ResourceTypeId::SQLITE_CONNECTION",
    "cancel_operations_by_owner",
    "close_resources_by_type",
    "builtins::runtime::io",
    "builtins::runtime::sqlite",
];

/// Adapter feature selection must remain in the adapter modules, never in the
/// recursive generic-VM production tree.
const FORBIDDEN_ADAPTER_FEATURES: &[&str] = &["async", "sqlite", "io", "http", "sse"];

fn vm_source_files() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let vm_dir = root.join("src").join("vm");
    let mut files = Vec::new();

    fn visit(path: &Path, files: &mut Vec<PathBuf>) {
        let mut entries = fs::read_dir(path)
            .unwrap_or_else(|error| panic!("read VM source directory {}: {error}", path.display()))
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|error| panic!("read VM source entry {}: {error}", path.display()));
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, files);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }

    assert!(vm_dir.is_dir(), "expected VM source directory to exist");
    visit(&vm_dir, &mut files);
    assert!(
        !files.is_empty(),
        "expected production Rust files under src/vm"
    );
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
fn execution_scope_hides_raw_mutable_resource_table() {
    let source = include_str!("../src/vm/execution_scope.rs");
    assert!(
        source.contains("pub(crate) fn resources_mut"),
        "raw ResourceTable access must stay inside the VM crate"
    );
    assert!(
        !source.contains("pub fn resources_mut"),
        "public callers must use lifecycle-checked typed resource operations"
    );
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
        assert!(
            !code.contains(FORBIDDEN_BUILTIN_CRATE_PATH),
            "`src/vm` file `{}` must not reference the builtin registration crate",
            file.display()
        );
        for forbidden in FORBIDDEN_ADAPTER_TOKENS {
            assert!(
                !code.contains(forbidden),
                "`src/vm` file `{}` must not reference concrete adapter token `{forbidden}`",
                file.display()
            );
        }
        let compact = code
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        for feature in FORBIDDEN_ADAPTER_FEATURES {
            let guard = format!("feature=\"{feature}\"");
            assert!(
                !compact.contains(&guard),
                "`src/vm` file `{}` must not select adapter feature `{feature}`",
                file.display()
            );
        }
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
