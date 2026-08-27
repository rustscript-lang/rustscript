//! Host runtime shell.
//!
//! [`HostRuntime`] owns the host-facing capability surface: bound host
//! functions and their symbol table, builtin overrides, resolved call slots,
//! host operation id allocation, the async bridge, the execution scope (one
//! resource table + one operation registry), and the print sink. Interpreter
//! state and run budgets live outside this struct (see
//! [`Instance`](super::instance::Instance) and
//! [`RunContext`](super::run_context::RunContext)).
//!
//! This mechanical decomposition groups host-facing ownership and reset/drop
//! behavior. The execution scope is the isolated resource/operation owner
//! that host code addresses through the generic, host-agnostic
//! [`ExecutionScope`] lifecycle. Persistent adapter policy/configuration
//! lives in the generic [`ModuleStateStore`](super::host_state::ModuleStateStore);
//! [`HostRuntime`] stays feature-neutral and owns no concrete adapter state
//! fields (the legacy IO completion mailbox remains on the `Vm` facade until
//! the adapter migrates it onto the scope lifecycle).

use std::any::Any;
use std::collections::HashMap;

use crate::vm::execution_scope::ExecutionScope;
use crate::vm::host::{HostAsyncBridge, HostOpId, VmHostFunction};
use crate::vm::host_state::ModuleStateStore;

/// Embedder-supplied print sink for `print`/`debug` output.
pub(crate) type RuntimePrintSink = dyn FnMut(String) + Send;

/// Host-owned capabilities, resources, operations, and subsystem state.
///
/// Thread safety: `HostRuntime` is `!Sync` (host functions and the execution
/// scope are mutable and not shareable) and not shared; one facade owns one
/// host runtime. Clone semantics: not `Clone` — host bindings must not be
/// duplicated across VMs.
pub(crate) struct HostRuntime {
    pub(super) host_functions: Vec<VmHostFunction>,
    pub(crate) host_function_symbols: HashMap<String, u16>,
    pub(crate) builtin_overrides: HashMap<u16, u16>,
    pub(crate) resolved_calls: Vec<u16>,
    pub(crate) resolved_calls_dirty: bool,
    pub(crate) async_bridge: Option<Box<dyn HostAsyncBridge>>,
    pub(crate) runtime_print_sink: Option<Box<RuntimePrintSink>>,
    pub(crate) next_host_op_id: HostOpId,
    /// The isolated execution scope owned by this host runtime.
    pub(super) execution_scope: ExecutionScope,
    /// The single generic per-VM module-state store.
    ///
    /// Persistent adapter policy/configuration (and later external-extension
    /// module state) lives here, keyed by `TypeId`, and deliberately survives
    /// execution-scope reset for the lifetime of the VM.
    pub(crate) module_state_store: ModuleStateStore,
}

impl HostRuntime {
    /// Creates an empty host runtime with no bound functions, no async bridge
    /// or print sink, plus a fresh active `ExecutionScope`.
    ///
    /// The execution-scope construction is fallible only when a process-unique
    /// identity space (resource arena or operation-registry tag) is exhausted,
    /// which cannot happen in a host runtime owned by a single `Vm` in one
    /// process. The scope-owned `expect` keeps `Vm::new` infallible while
    /// still giving every VM a live, independent scope.
    pub(crate) fn new() -> Self {
        Self {
            host_functions: Vec::new(),
            host_function_symbols: HashMap::new(),
            builtin_overrides: HashMap::new(),
            resolved_calls: Vec::new(),
            resolved_calls_dirty: true,
            async_bridge: None,
            runtime_print_sink: None,
            next_host_op_id: 1,
            execution_scope: ExecutionScope::new()
                .expect("host runtime execution-scope identity space must be available"),
            module_state_store: ModuleStateStore::new(),
        }
    }

    /// Stores host-owned typed module state, replacing any earlier value of
    /// the same type.
    pub(crate) fn set_module_state<T: Any + Send + 'static>(&mut self, state: T) -> bool {
        self.module_state_store.set(state)
    }

    /// Borrows the registered typed module state, if any.
    pub(crate) fn get_module_state<T: Any + Send + 'static>(&self) -> Option<&T> {
        self.module_state_store.get()
    }

    /// Borrows the registered typed module state mutably, if any.
    #[allow(dead_code)] // used by later host layers (SQLite/capability) in c4/c5
    pub(crate) fn get_module_state_mut<T: Any + Send + 'static>(&mut self) -> Option<&mut T> {
        self.module_state_store.get_mut()
    }

    /// Removes and returns the registered typed module state, if any.
    pub(crate) fn remove_module_state<T: Any + Send + 'static>(&mut self) -> Option<T> {
        self.module_state_store.remove()
    }

    /// Returns `true` when no module state is currently registered.
    #[allow(dead_code)] // used by later host layers (SQLite/capability) in c4/c5
    pub(crate) fn is_module_state_empty(&self) -> bool {
        self.module_state_store.is_empty()
    }

    /// Replaces the active execution scope with a fresh one.
    ///
    /// Dropping the old scope runs its generic close sweep, retiring every
    /// in-flight operation and closing every resource before the new scope
    /// starts. Used by `Vm::reset_for_reuse` so resource/operation teardown
    /// goes through the generic scope lifecycle.
    ///
    /// Persistent policy/configuration in the `ModuleStateStore` is
    /// deliberately **not** touched here; it survives reset. This function
    /// contains no adapter name, feature, or concrete `TypeId`: it is wholly
    /// feature-neutral.
    pub(crate) fn reset_execution_scope(&mut self) {
        self.execution_scope = ExecutionScope::new()
            .expect("host runtime execution-scope identity space must be available");
    }
}

impl Default for HostRuntime {
    fn default() -> Self {
        Self::new()
    }
}
