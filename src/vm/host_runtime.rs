//! Host runtime shell.
//!
//! [`HostRuntime`] owns the host-facing capability surface: bound host
//! functions and their symbol table, capability allow-lists, builtin
//! overrides, resolved call slots, the opaque resource arena, the pending
//! operation registry, a type-erased host-function state store, the async
//! bridge, and the print sink. Concrete IO/HTTP/SQLite state is defined and
//! interpreted only by those host modules. Interpreter state and run budgets
//! live outside this struct (see [`Instance`](super::instance::Instance) and
//! [`RunContext`](super::run_context::RunContext)).
//!
//! The VM provides lifecycle storage without depending on host-specific state
//! types or configuration APIs.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::task::{Context, Poll};

use crate::builtins::runtime::cancellation::{
    CancellationReason, DEFAULT_MAX_PENDING_OPERATIONS, OperationRegistry,
};
use crate::builtins::runtime::resource::{DEFAULT_MAX_RESOURCES, ResourceArena};

use crate::vm::async_host::{HostAsyncBridge, HostStreamDriver};
use crate::vm::execution_scope::{
    ExecutionScope, ExecutionScopeError, ExecutionScopeResult, ScopeCloseOutcome, ScopeState,
};
use crate::vm::host::VmHostFunction;
use crate::vm::host_context::{HostModule, HostModuleStore};
use crate::vm::operation::{OperationId, OperationSpec};
use crate::vm::resource::{HostResource, Resource, ResourceCloseReason};

/// Embedder-supplied print sink for `print`/`debug` output.
pub(crate) type RuntimePrintSink = dyn FnMut(String) + Send;

/// Host-owned capabilities, resources, operations, and subsystem state.
///
/// Thread safety: `HostRuntime` is `!Sync` (host functions, resources, and
/// operations are mutable and not shareable) and not shared; one facade owns
/// one host runtime. Clone semantics: not `Clone` — host bindings and resource
/// handles must not be duplicated across VMs.
pub(crate) struct HostRuntime {
    pub(super) host_functions: Vec<VmHostFunction>,
    pub(crate) host_function_symbols: HashMap<String, u16>,
    pub(crate) allow_default_host_fallback: bool,
    pub(crate) allowed_builtin_calls: Vec<u16>,
    pub(crate) allow_default_builtin_capabilities: bool,
    pub(crate) allowed_host_function_slots: Vec<u16>,
    pub(crate) allow_default_host_capabilities: bool,
    pub(crate) builtin_overrides: HashMap<u16, u16>,
    pub(crate) runtime_owned_pending_host_slots: HashSet<u16>,
    pub(crate) resolved_calls: Vec<u16>,
    pub(crate) resolved_calls_dirty: bool,
    pub(crate) runtime_resources: ResourceArena,
    pub(crate) runtime_operations: OperationRegistry,
    /// The host-agnostic execution scope of this runtime, created Active.
    ///
    /// One host runtime always owns exactly one live scope. Scope close
    /// drives this scope's resource table and operation registry to
    /// quiescence; it must never clear [`HostModuleStore`] state
    /// (`module_state`), whose lifecycle is deliberately independent. The
    /// legacy `runtime_resources` / `runtime_operations` are retained for the
    /// migration of existing builtins and never pretend to belong to this
    /// scope.
    execution_scope: ExecutionScope,
    module_state: HostModuleStore,
    pub(crate) async_bridge: Option<Box<dyn HostAsyncBridge>>,
    pub(crate) submitted_host_ops: HashSet<u64>,
    pub(crate) stream_drivers: HashMap<u64, Box<dyn HostStreamDriver>>,
    pub(crate) runtime_print_sink: Option<Box<RuntimePrintSink>>,
}

impl HostRuntime {
    /// Creates an empty host runtime with default capability and resource
    /// limits and no bound functions.
    pub(crate) fn new() -> Self {
        Self {
            host_functions: Vec::new(),
            host_function_symbols: HashMap::new(),
            allow_default_host_fallback: true,
            allowed_builtin_calls: Vec::new(),
            allow_default_builtin_capabilities: true,
            allowed_host_function_slots: Vec::new(),
            allow_default_host_capabilities: true,
            builtin_overrides: HashMap::new(),
            runtime_owned_pending_host_slots: HashSet::new(),
            resolved_calls: Vec::new(),
            resolved_calls_dirty: true,
            runtime_resources: ResourceArena::with_limit(DEFAULT_MAX_RESOURCES)
                .expect("default runtime resource limit should be valid"),
            runtime_operations: OperationRegistry::with_limit(DEFAULT_MAX_PENDING_OPERATIONS)
                .expect("default runtime operation limit should be valid"),
            execution_scope: ExecutionScope::new(),
            module_state: HostModuleStore::new(),
            async_bridge: None,
            submitted_host_ops: HashSet::new(),
            stream_drivers: HashMap::new(),
            runtime_print_sink: None,
        }
    }

    /// Closes run-scoped host state between runs: pending operations are
    /// cancelled, resources are closed, and the IO subsystem is recreated.
    /// Host bindings, capability allow-lists, and the async bridge are
    /// preserved (documented reusable state).
    pub(crate) fn reset_for_reuse(&mut self) {
        let _ = self
            .runtime_operations
            .cancel_all(CancellationReason::VmReset);
        let _ = self
            .runtime_resources
            .close_all(CancellationReason::VmReset);
        self.submitted_host_ops.clear();
        self.stream_drivers.clear();
    }

    pub(crate) fn set_host_function_state<T>(&mut self, state: T)
    where
        T: Any + Send + 'static,
    {
        self.module_state.set(state);
    }

    pub(crate) fn host_function_state<T>(&self) -> Option<&T>
    where
        T: Any + Send + 'static,
    {
        self.module_state.get()
    }

    #[cfg(feature = "http-client")]
    pub(crate) fn host_function_state_mut<T>(&mut self) -> Option<&mut T>
    where
        T: Any + Send + 'static,
    {
        self.module_state.get_mut()
    }

    pub(crate) fn remove_host_function_state<T>(&mut self) -> Option<T>
    where
        T: Any + Send + 'static,
    {
        self.module_state.take::<T>()
    }

    /// Registers typed per-VM module state through the host boundary, returning
    /// `true` when a value of the same type was replaced.
    pub(crate) fn set_module_state<M: HostModule>(&mut self, state: M) -> bool {
        self.module_state.set(state)
    }

    /// Borrows the registered typed module state, if any.
    pub(crate) fn get_module_state<M: HostModule>(&self) -> Option<&M> {
        self.module_state.get()
    }

    /// Borrows the registered typed module state mutably, if any.
    pub(crate) fn get_module_state_mut<M: HostModule>(&mut self) -> Option<&mut M> {
        self.module_state.get_mut()
    }

    /// Removes and returns the registered typed module state, if any.
    pub(crate) fn remove_module_state<M: HostModule>(&mut self) -> Option<M> {
        self.module_state.take::<M>()
    }

    /// Returns `true` when no module state is currently registered.
    pub(crate) fn is_module_state_empty(&self) -> bool {
        self.module_state.is_empty()
    }

    pub(crate) fn default_builtin_capabilities_enabled(&self) -> bool {
        self.allow_default_builtin_capabilities
    }

    // ---- execution scope: read-only access ---------------------------------

    /// Read-only access to the owned execution scope (observe state, counts,
    /// typed borrows, operation status). The scope itself is never handed out
    /// mutably: all mutations go through the controlled entry points below.
    pub(crate) fn execution_scope(&self) -> &ExecutionScope {
        &self.execution_scope
    }

    /// The current lifecycle phase of the owned execution scope.
    pub(crate) fn execution_scope_state(&self) -> ScopeState {
        self.execution_scope.state()
    }

    /// Whether the owned execution scope is still accepting inserts.
    pub(crate) fn execution_scope_is_active(&self) -> bool {
        self.execution_scope.is_active()
    }

    /// Whether the owned execution scope reached terminal quiescence.
    pub(crate) fn execution_scope_is_quiescent(&self) -> bool {
        self.execution_scope.is_quiescent()
    }

    /// Number of live resources in the owned execution scope.
    pub(crate) fn execution_scope_resource_count(&self) -> usize {
        self.execution_scope.resources().len()
    }

    /// Number of occupied operation slots in the owned execution scope.
    pub(crate) fn execution_scope_operation_count(&self) -> usize {
        self.execution_scope.operations().len()
    }

    // ---- execution scope: controlled mut entry points ----------------------

    /// Inserts a root resource into the owned execution scope (guarded: the
    /// scope rejects inserts once Active).
    pub(crate) fn execution_scope_push_resource<T: HostResource>(
        &mut self,
        value: T,
    ) -> ExecutionScopeResult<Resource<T>> {
        self.execution_scope.push_resource(value)
    }

    /// Inserts a typed child resource linked to `parent` (guarded).
    pub(crate) fn execution_scope_push_child_resource<T: HostResource, P: HostResource>(
        &mut self,
        value: T,
        parent: &Resource<P>,
    ) -> ExecutionScopeResult<Resource<T>> {
        self.execution_scope.push_child_resource(value, parent)
    }

    /// Starts an operation in the owned execution scope with the full generic
    /// spec (driver, resource association, deadline, cleanup/cancel).
    pub(crate) fn execution_scope_start_operation(
        &mut self,
        spec: OperationSpec,
    ) -> ExecutionScopeResult<OperationId> {
        self.execution_scope.start_operation(spec)
    }

    /// Begins scope shutdown (Active → Closing, sealing new inserts).
    pub(crate) fn execution_scope_begin_close(
        &mut self,
        reason: ResourceCloseReason,
    ) -> ExecutionScopeResult<bool> {
        self.execution_scope.begin_close(reason)
    }

    /// Drives the closing scope to quiescence with the caller's context.
    pub(crate) fn execution_scope_poll_close(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<ExecutionScopeResult<ScopeCloseOutcome>> {
        self.execution_scope.poll_close(cx)
    }

    /// Atomically replaces the owned execution scope with `next`, **only**
    /// once the current scope is Quiescent (all cleanup finished). Returns the
    /// old quiescent scope so the caller can inspect its terminal outcome.
    ///
    /// A non-Quiescent (Active or Closing) scope is rejected with
    /// [`ExecutionScopeError::ScopeNotQuiescent`]; this is the only replacement
    /// path, so cleanup can never be bypassed.
    ///
    /// Consumed by the next-scope reset integration (`Vm::reset_for_reuse` →
    /// scope recycle); this wiring-only commit keeps it crate-private and
    /// gated rather than connecting it to `Vm` reset semantics.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn replace_execution_scope(
        &mut self,
        next: ExecutionScope,
    ) -> ExecutionScopeResult<ExecutionScope> {
        if !self.execution_scope.is_quiescent() {
            return Err(ExecutionScopeError::ScopeNotQuiescent);
        }
        Ok(std::mem::replace(&mut self.execution_scope, next))
    }

    /// Recycles the owned execution scope: takes the current scope out only
    /// once it is Quiescent, installs a fresh Active scope in its place, and
    /// returns the old quiescent scope.
    ///
    /// Equivalent to [`replace_execution_scope`](Self::replace_execution_scope)
    /// with a fresh scope, so the same quiescence gate applies. Forward reset
    /// hook — see [`replace_execution_scope`](Self::replace_execution_scope).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn take_quiescent_scope(&mut self) -> ExecutionScopeResult<ExecutionScope> {
        self.replace_execution_scope(ExecutionScope::new())
    }
}

impl Default for HostRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::execution_scope::ScopeCloseOutcome;
    use crate::vm::resource::{CloseProgress, ResourceErrorCode, ResourceResult};
    use std::sync::Arc;
    use std::task::Wake;

    /// A generic fake resource that closes synchronously.
    #[derive(Debug, PartialEq, Eq)]
    struct TestResource;

    impl HostResource for TestResource {
        fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
            Ok(CloseProgress::Ready)
        }
    }

    struct NoopWake;

    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    fn drive_scope_quiescent(host: &mut HostRuntime) {
        let waker = Arc::new(NoopWake).into();
        let mut cx = Context::from_waker(&waker);
        loop {
            match host.execution_scope_poll_close(&mut cx) {
                Poll::Pending => continue,
                Poll::Ready(result) => {
                    assert_eq!(
                        result.expect("scope close should succeed"),
                        ScopeCloseOutcome::Success
                    );
                    break;
                }
            }
        }
        assert!(host.execution_scope_is_quiescent());
    }

    #[test]
    fn host_runtime_owns_an_active_execution_scope_from_construction() {
        let host = HostRuntime::new();
        assert_eq!(host.execution_scope_state(), ScopeState::Active);
        assert!(host.execution_scope_is_active());
        assert!(!host.execution_scope_is_quiescent());
        assert_eq!(host.execution_scope_resource_count(), 0);
        assert_eq!(host.execution_scope_operation_count(), 0);
    }

    #[test]
    fn replace_execution_scope_rejects_non_quiescent_scope() {
        let mut host = HostRuntime::new();

        // An Active scope (close was never begun) is not quiescent.
        let result = host.replace_execution_scope(ExecutionScope::new());
        let Err(error) = result else {
            panic!("an active scope must refuse replacement");
        };
        assert_eq!(error, ExecutionScopeError::ScopeNotQuiescent);

        // A Closing scope (close begun, not driven to quiescence) also refuses.
        assert!(
            host.execution_scope_begin_close(ResourceCloseReason::Requested)
                .expect("begin close")
        );
        assert_eq!(host.execution_scope_state(), ScopeState::Closing);
        let result = host.replace_execution_scope(ExecutionScope::new());
        let Err(error) = result else {
            panic!("a closing scope must refuse replacement");
        };
        assert_eq!(error, ExecutionScopeError::ScopeNotQuiescent);

        // take_quiescent_scope shares the same gate.
        let Err(error) = host.take_quiescent_scope() else {
            panic!("take_quiescent_scope must reject a non-quiescent scope");
        };
        assert_eq!(error, ExecutionScopeError::ScopeNotQuiescent);
    }

    #[test]
    fn replace_execution_scope_after_quiescence_yields_fresh_active_scope() {
        let mut host = HostRuntime::new();
        let old_handle = host
            .execution_scope_push_resource(TestResource)
            .expect("push into active scope");
        assert_eq!(host.execution_scope_resource_count(), 1);

        // Close fully: only a Quiescent scope may be replaced.
        assert!(
            host.execution_scope_begin_close(ResourceCloseReason::VmReset)
                .expect("begin close")
        );
        drive_scope_quiescent(&mut host);

        let old_scope = host
            .replace_execution_scope(ExecutionScope::new())
            .expect("quiescent scope is replaceable");
        assert_eq!(old_scope.state(), ScopeState::Quiescent);
        assert_eq!(old_scope.resources().len(), 0, "old scope is fully closed");
        assert_eq!(old_scope.terminal(), Some(&ScopeCloseOutcome::Success));

        // The fresh scope starts Active and empty.
        assert!(host.execution_scope_is_active());
        assert_eq!(host.execution_scope_resource_count(), 0);
        assert_eq!(host.execution_scope_operation_count(), 0);

        // A handle from the replaced scope must not resolve in the new scope.
        let error = host
            .execution_scope()
            .resources()
            .get(&old_handle)
            .expect_err("an old-scope handle must be rejected by the new scope");
        assert_eq!(error.code(), ResourceErrorCode::ResourceHandleWrongTable);
    }

    #[test]
    fn take_quiescent_scope_recycles_with_fresh_active_scope() {
        let mut host = HostRuntime::new();
        let _old_handle = host
            .execution_scope_push_resource(TestResource)
            .expect("push into active scope");
        assert!(
            host.execution_scope_begin_close(ResourceCloseReason::Requested)
                .expect("begin close")
        );
        drive_scope_quiescent(&mut host);

        let old_scope = host
            .take_quiescent_scope()
            .expect("quiescent scope is recyclable");
        assert_eq!(old_scope.state(), ScopeState::Quiescent);
        assert!(host.execution_scope_is_active());
        assert_eq!(host.execution_scope_resource_count(), 0);
        assert_eq!(host.execution_scope_operation_count(), 0);
    }
}
