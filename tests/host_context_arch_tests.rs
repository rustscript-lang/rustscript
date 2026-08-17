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
//!    typed, per-VM module state and reaches the resource/operation registration
//!    ports purely through the public [`HostContext`] surface, without ever
//!    touching host-runtime internals or a builtin domain type.

use std::any::Any;
use std::fs;
use std::path::{Path, PathBuf};

use vm::{
    HostContextError, HostHandle, HostKind, HostOperation, HostOperationHandle,
    HostOperationRegistry, HostResource, HostResourceRegistry, Program, Vm,
};

/// The builtin *domain* modules that `src/vm` must not import. Generic host
/// substrate (e.g. resource handle encoding / cancellation primitives) is
/// scheduled to be re-homed by the sibling resource-table / operation-driver
/// scopes; the concrete domain implementations named here are the ones this
/// boundary must keep out of `src/vm`.
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
    for contract in [
        "pub struct HostContext",
        "pub trait HostModule",
        "pub trait HostResourceRegistry",
        "pub trait HostOperationRegistry",
    ] {
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

// ---------------------------------------------------------------------------
// Resource / operation registration ports (host-agnostic adapters)
// ---------------------------------------------------------------------------

struct FakeResource {
    value: u32,
}
impl HostResource for FakeResource {
    const KIND: HostKind = HostKind::new_unchecked(1);
}

struct FakeResourceTable {
    next: u64,
    slots: Vec<Option<Box<dyn Any + Send>>>,
    closed: u32,
}

impl HostResourceRegistry for FakeResourceTable {
    fn insert_value(&mut self, value: Box<dyn Any + Send>) -> Result<HostHandle, HostContextError> {
        self.next += 1;
        let handle = HostHandle::from_raw(self.next)?;
        self.slots.push(Some(value));
        Ok(handle)
    }

    fn insert_value_with_cleanup(
        &mut self,
        value: Box<dyn Any + Send>,
        _cleanup: Box<dyn FnOnce() + Send>,
    ) -> Result<HostHandle, HostContextError> {
        self.insert_value(value)
    }

    fn borrow_value(&self, handle: HostHandle) -> Result<&(dyn Any + Send), HostContextError> {
        let idx = (handle.raw() - 1) as usize;
        self.slots
            .get(idx)
            .and_then(|slot| slot.as_ref().map(|v| v.as_ref()))
            .ok_or_else(|| HostContextError::new("host::resource", "no such resource"))
    }

    fn borrow_value_mut(
        &mut self,
        handle: HostHandle,
    ) -> Result<&mut (dyn Any + Send), HostContextError> {
        let idx = (handle.raw() - 1) as usize;
        self.slots
            .get_mut(idx)
            .and_then(|slot| slot.as_mut().map(|v| &mut **v))
            .ok_or_else(|| HostContextError::new("host::resource", "no such resource"))
    }

    fn close(&mut self, handle: HostHandle) -> Result<(), HostContextError> {
        let idx = (handle.raw() - 1) as usize;
        if self.slots.get_mut(idx).and_then(|s| s.take()).is_some() {
            self.closed += 1;
        }
        Ok(())
    }
}

struct SomeOperation {
    name: &'static str,
}
impl HostOperation for SomeOperation {
    fn name(&self) -> &'static str {
        self.name
    }
}

struct FakeOperationRegistry {
    next: u64,
    pending: Vec<HostOperationHandle>,
}

impl HostOperationRegistry for FakeOperationRegistry {
    fn submit(
        &mut self,
        _operation: Box<dyn HostOperation + Send>,
    ) -> Result<HostOperationHandle, HostContextError> {
        self.next += 1;
        let handle = HostOperationHandle::from_raw(self.next)?;
        self.pending.push(handle);
        Ok(handle)
    }

    fn cancel(
        &mut self,
        handle: HostOperationHandle,
        _reason: &'static str,
    ) -> Result<bool, HostContextError> {
        let Some(idx) = self.pending.iter().position(|h| *h == handle) else {
            return Ok(false);
        };
        self.pending.swap_remove(idx);
        Ok(true)
    }
}

#[test]
fn resource_and_operation_ports_work_without_any_builtin_import() {
    // Resource port: typed insert / borrow / borrow_mut / close.
    let mut table = FakeResourceTable {
        next: 0,
        slots: Vec::new(),
        closed: 0,
    };
    let handle = table.insert(FakeResource { value: 5 }).expect("insert");
    assert_eq!(
        table.borrow::<FakeResource>(handle).expect("borrow").value,
        5
    );
    table
        .borrow_mut::<FakeResource>(handle)
        .expect("borrow_mut")
        .value += 1;
    assert_eq!(
        table.borrow::<FakeResource>(handle).expect("borrow").value,
        6
    );
    table.close(handle).expect("close");
    assert_eq!(table.closed, 1);

    // Operation port: submit + cancel through a host-agnostic registry.
    let mut ops = FakeOperationRegistry {
        next: 0,
        pending: Vec::new(),
    };
    let op_handle = ops
        .submit(Box::new(SomeOperation { name: "test-op" }))
        .expect("submit");
    assert!(ops.cancel(op_handle, "host::test").expect("cancel"));
    assert!(!ops.cancel(op_handle, "host::test").expect("cancel-twice"));
}
