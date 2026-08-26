//! Host runtime shell.
//!
//! [`HostRuntime`] owns the host-facing capability surface: bound host
//! functions and their symbol table, builtin overrides, resolved call slots,
//! the IO subsystem state, host operation id allocation, the async bridge,
//! the execution scope (one resource table + one operation registry), and
//! the print sink. Interpreter state and run budgets live outside this
//! struct (see [`Instance`](super::instance::Instance) and
//! [`RunContext`](super::run_context::RunContext)).
//!
//! This mechanical decomposition groups host-facing ownership and reset/drop
//! behavior. The execution scope is the isolated resource/operation owner
//! that host code addresses through the generic, host-agnostic
//! [`ExecutionScope`] lifecycle.

use std::any::{Any, TypeId};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::builtins::runtime::IoState;
#[cfg(all(feature = "sqlite", not(target_arch = "wasm32")))]
use crate::builtins::runtime::SqliteState;
use crate::vm::execution_scope::ExecutionScope;
use crate::vm::host::{HostAsyncBridge, HostOpId, VmHostFunction};

/// Embedder-supplied print sink for `print`/`debug` output.
pub(crate) type RuntimePrintSink = dyn FnMut(String) + Send;

/// Host-owned capabilities, resources, operations, and subsystem state.
///
/// Thread safety: `HostRuntime` is `!Sync` (host functions, IO state and the
/// execution scope are mutable and not shareable) and not shared; one facade
/// owns one host runtime. Clone semantics: not `Clone` — host bindings and IO
/// handles must not be duplicated across VMs.
pub(crate) struct HostRuntime {
    pub(super) host_functions: Vec<VmHostFunction>,
    pub(crate) host_function_symbols: HashMap<String, u16>,
    pub(crate) builtin_overrides: HashMap<u16, u16>,
    pub(crate) resolved_calls: Vec<u16>,
    pub(crate) resolved_calls_dirty: bool,
    pub(crate) async_bridge: Option<Box<dyn HostAsyncBridge>>,
    pub(crate) runtime_print_sink: Option<Box<RuntimePrintSink>>,
    pub(crate) io_state: IoState,
    #[cfg(all(feature = "sqlite", not(target_arch = "wasm32")))]
    pub(crate) sqlite_state: SqliteState,
    pub(crate) next_host_op_id: HostOpId,
    /// The isolated execution scope owned by this host runtime.
    pub(super) execution_scope: ExecutionScope,
    /// Host-owned typed configuration/policy state, keyed by `TypeId`.
    ///
    /// This is the generic host-owned policy storage: host implementations
    /// (IO, SQLite, external resources) store their policy/configuration
    /// here instead of adding bespoke fields. The state is opaque to the VM
    /// and replaced wholesale on scope reset.
    host_function_state: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
    /// The generic per-VM module-state store surfaced to external host
    /// extensions through [`HostContext`](super::host_context::HostContext).
    ///
    /// This is the same generic host-owned policy/configuration storage, but
    /// exposed through the public [`HostContext`] boundary. It is typed
    /// (keyed by `TypeId`), per-`Vm`, and deliberately **not** cleared on
    /// scope reset, so extension policy/configuration survives reset and
    /// scope recycling for the lifetime of the VM.
    pub(crate) module_state_store: super::host_context::ModuleStateStore,
    /// Whether the default builtin capability set is enabled for this VM.
    ///
    /// A restricted registry (`HostFunctionRegistry::restricted`) binds this
    /// to `false`, making privileged builtins require an explicit capability
    /// grant before execution.
    pub(crate) allow_default_builtin_capabilities: bool,
    /// Explicitly allowed builtin call indices (from the bound capability
    /// profile), enforced when `allow_default_builtin_capabilities` is off.
    pub(crate) allowed_builtin_calls: Vec<u16>,
    /// Whether the default host capability set is enabled for this VM.
    pub(crate) allow_default_host_capabilities: bool,
    /// Host-function slots permitted by the bound capability profile,
    /// enforced when `allow_default_host_capabilities` is off.
    pub(crate) allowed_host_function_slots: Vec<u16>,
    /// Whether unbound host imports fall back to the default host functions.
    pub(crate) allow_default_host_fallback: bool,
    /// Ops submitted to the async host bridge (via `Vm::submit_host_future`)
    /// that are still pending. These route to the bridge's
    /// `poll_submitted_op` instead of a runtime-owned operation driver.
    pub(crate) submitted_host_ops: HashSet<HostOpId>,
}

impl HostRuntime {
    /// Creates an empty host runtime with no bound functions, no IO state, and
    /// no async bridge or print sink, plus a fresh active `ExecutionScope`.
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
            io_state: IoState::default(),
            #[cfg(all(feature = "sqlite", not(target_arch = "wasm32")))]
            sqlite_state: SqliteState::default(),
            next_host_op_id: 1,
            execution_scope: ExecutionScope::new()
                .expect("host runtime execution-scope identity space must be available"),
            host_function_state: HashMap::new(),
            module_state_store: super::host_context::ModuleStateStore::new(),
            allow_default_builtin_capabilities: true,
            allowed_builtin_calls: Vec::new(),
            allow_default_host_capabilities: true,
            allowed_host_function_slots: Vec::new(),
            allow_default_host_fallback: true,
            submitted_host_ops: HashSet::new(),
        }
    }

    /// Stores host-owned typed policy/configuration state.
    pub(crate) fn set_host_function_state<T>(&mut self, state: T)
    where
        T: Send + Sync + 'static,
    {
        self.host_function_state
            .insert(TypeId::of::<T>(), Arc::new(state));
    }

    /// Returns host-owned typed policy/configuration state, if any.
    pub(crate) fn host_function_state<T>(&self) -> Option<&T>
    where
        T: Send + Sync + 'static,
    {
        self.host_function_state
            .get(&TypeId::of::<T>())
            .and_then(|state| state.downcast_ref::<T>())
    }

    /// Removes host-owned typed policy/configuration state.
    pub(crate) fn remove_host_function_state<T>(&mut self) -> Option<Arc<dyn Any + Send + Sync>>
    where
        T: Send + Sync + 'static,
    {
        self.host_function_state.remove(&TypeId::of::<T>())
    }

    /// Whether the default builtin capability set is enabled.
    pub(crate) fn default_builtin_capabilities_enabled(&self) -> bool {
        self.allow_default_builtin_capabilities
    }

    /// Replaces the active execution scope with a fresh one.
    ///
    /// Dropping the old scope runs its generic close sweep, retiring every
    /// in-flight IO operation and closing every IO handle/process resource
    /// before the new scope starts. Used by `Vm::reset_for_reuse` so IO
    /// retirement goes through the generic scope lifecycle.
    pub(crate) fn reset_execution_scope(&mut self) {
        self.io_state = IoState::default();
        #[cfg(all(feature = "sqlite", not(target_arch = "wasm32")))]
        {
            self.sqlite_state = SqliteState::default();
        }
        self.host_function_state.clear();
        self.execution_scope = ExecutionScope::new()
            .expect("host runtime execution-scope identity space must be available");
    }
}

impl Default for HostRuntime {
    fn default() -> Self {
        Self::new()
    }
}
