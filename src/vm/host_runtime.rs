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
//! [`ExecutionScope`] lifecycle. Host adapters retain their concrete worker
//! state and result mailboxes behind opaque scoped-operation completion hooks;
//! persistent policy lives in the generic module-state store, while
//! [`HostRuntime`] stays feature-neutral and owns no concrete adapter state
//! fields.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::host_api::HostImportSchema;
use crate::vm::execution_scope::ExecutionScope;
use crate::vm::host::{HostAsyncBridge, HostOpId, ScopedOperationCompletion, VmHostFunction};
use crate::vm::operation::{OperationCancelReason, OperationId};
use crate::vm::standard_composition::StandardSurfaceComposition;

/// Embedder-supplied print sink for `print`/`debug` output.
pub(crate) type RuntimePrintSink = dyn FnMut(String) + Send;

/// Host-owned capabilities, resources, operations, and subsystem state.
///
/// Thread safety: `HostRuntime` is `!Sync` (host functions, the execution
/// scope — including generic resource/operation registries — are mutable and
/// not shareable) and not shared; one facade owns one host runtime. Clone
/// semantics: not `Clone` — host bindings and scoped operation completions
/// must not be duplicated across VMs.
pub(crate) struct HostRuntime {
    pub(super) host_functions: Vec<VmHostFunction>,
    pub(crate) host_function_schemas: Vec<Option<HostImportSchema>>,
    pub(crate) host_function_symbols: HashMap<String, u16>,
    pub(crate) builtin_overrides: HashMap<u16, u16>,
    pub(crate) resolved_calls: Vec<u16>,
    pub(crate) resolved_calls_dirty: bool,
    pub(crate) async_bridge: Option<Box<dyn HostAsyncBridge>>,
    pub(crate) runtime_print_sink: Option<Box<RuntimePrintSink>>,
    pub(crate) next_host_op_id: HostOpId,
    /// The isolated execution scope owned by this host runtime.
    pub(super) execution_scope: ExecutionScope,
    /// The generic per-VM module-state store surfaced to external host
    /// extensions through [`HostContext`](super::host_context::HostContext).
    ///
    /// This is the same generic host-owned policy/configuration storage, but
    /// exposed through the public [`HostContext`] boundary. It is typed
    /// (keyed by `TypeId`), per-`Vm`, and deliberately **not** cleared on
    /// scope reset, so extension policy/configuration survives reset and
    /// scope recycling for the lifetime of the VM.
    pub(crate) module_state_store: super::host_state::ModuleStateStore,
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
    /// The standard-surface composition used to construct this VM's default
    /// host registry and resolve its unbound imports.
    pub(crate) standard_composition: Option<Arc<dyn StandardSurfaceComposition>>,
    /// Ops submitted to the async host bridge (via `Vm::submit_host_future`)
    /// that are still pending. These route to the bridge's
    /// `poll_submitted_op` instead of a runtime-owned operation driver.
    pub(crate) submitted_host_ops: HashSet<HostOpId>,
    /// Adapter-owned completions for operations driven by the execution scope.
    pub(crate) scoped_operation_completions: HashMap<OperationId, ScopedOperationCompletion>,
}

impl HostRuntime {
    /// Creates an empty host runtime with no bound functions, no async bridge
    /// or print sink, and no adapter state, plus a fresh active
    /// `ExecutionScope`.
    ///
    /// The execution-scope construction is fallible only when a process-unique
    /// identity space (resource arena or operation-registry tag) is exhausted,
    /// which cannot happen in a host runtime owned by a single `Vm` in one
    /// process. The scope-owned `expect` keeps `Vm::new` infallible while
    /// still giving every VM a live, independent scope.
    pub(crate) fn new() -> Self {
        Self {
            host_functions: Vec::new(),
            host_function_schemas: Vec::new(),
            host_function_symbols: HashMap::new(),
            builtin_overrides: HashMap::new(),
            resolved_calls: Vec::new(),
            resolved_calls_dirty: true,
            async_bridge: None,
            runtime_print_sink: None,
            next_host_op_id: 1,
            execution_scope: ExecutionScope::new()
                .expect("host runtime execution-scope identity space must be available"),
            module_state_store: super::host_state::ModuleStateStore::new(),
            allow_default_builtin_capabilities: true,
            allowed_builtin_calls: Vec::new(),
            allow_default_host_capabilities: true,
            allowed_host_function_slots: Vec::new(),
            allow_default_host_fallback: true,
            standard_composition: None,
            submitted_host_ops: HashSet::new(),
            scoped_operation_completions: HashMap::new(),
        }
    }

    pub(crate) fn with_standard_composition(
        composition: Arc<dyn StandardSurfaceComposition>,
    ) -> Self {
        let mut runtime = Self::new();
        runtime.standard_composition = Some(composition);
        runtime
    }

    /// Whether the default builtin capability set is enabled.
    pub(crate) fn default_builtin_capabilities_enabled(&self) -> bool {
        self.allow_default_builtin_capabilities
    }

    /// Cancels every bridge-submitted operation still owned by this runtime.
    ///
    /// The set is drained before invoking the bridge so each submitted id is
    /// handed to the bridge at most once, even when a later lifecycle path
    /// runs again. This also makes bridge replacement safe: no old submitted
    /// id remains after the old bridge is discarded.
    pub(crate) fn cancel_submitted_host_ops(&mut self, reason: OperationCancelReason) {
        let mut op_ids = self.submitted_host_ops.drain().collect::<Vec<_>>();
        op_ids.sort_unstable();
        if let Some(bridge) = self.async_bridge.as_mut() {
            for op_id in op_ids {
                bridge.cancel_op_with_reason(op_id, reason);
            }
        }
    }

    /// Replaces the active execution scope with a fresh one.
    ///
    /// Dropping the old scope runs its generic close sweep, retiring every
    /// in-flight operation, closing every resource, and dropping every
    /// adapter-declared scope-arena typed-state entry before the new scope
    /// starts. Any bridge-submitted futures are cancelled with `VmReset` and
    /// the submitted-id set is drained before the old scope is dropped. Used
    /// by `Vm::reset_for_reuse` so adapter runtime-state teardown goes through
    /// the generic scope lifecycle.
    ///
    /// Persistent policy/configuration — including the HTTP host
    /// configuration and max-in-flight policy, IO policy, SQLite policy, and
    /// external-extension module state — lives in the persistent
    /// `ModuleStateStore`, which is deliberately **not** touched here. It
    /// therefore survives reset (only per-invocation resources, operations,
    /// and scope-arena runtime state are retired). This function contains no
    /// adapter name, feature, or concrete `TypeId`: it is wholly feature-neutral.
    pub(crate) fn reset_execution_scope(&mut self) {
        self.cancel_submitted_host_ops(OperationCancelReason::VmReset);
        self.scoped_operation_completions.clear();
        let _ = self
            .execution_scope
            .begin_close(crate::vm::resource::ResourceCloseReason::VmReset);
        self.execution_scope = ExecutionScope::new()
            .expect("host runtime execution-scope identity space must be available");
    }
}

impl Drop for HostRuntime {
    fn drop(&mut self) {
        self.cancel_submitted_host_ops(OperationCancelReason::VmDrop);
    }
}

impl Default for HostRuntime {
    fn default() -> Self {
        Self::new()
    }
}
