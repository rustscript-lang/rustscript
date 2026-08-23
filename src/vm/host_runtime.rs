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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use crate::vm::async_host::{HostAsyncBridge, HostStreamDriver};
use crate::vm::execution_scope::{
    ExecutionScope, ExecutionScopeError, ExecutionScopeResult, ScopeCloseOutcome, ScopeState,
};
use crate::vm::host::VmHostFunction;
use crate::vm::host_context::{HostModule, HostModuleStore};
use crate::vm::operation::{
    OperationCancelReason, OperationId, OperationOutcome, OperationResult, OperationSpec,
};
use crate::vm::resource::{
    HostResource, Resource, ResourceAccessFrame, ResourceAccessRequest, ResourceCloseReason,
    ResourceTypeKey,
};

/// Embedder-supplied print sink for `print`/`debug` output.
pub(crate) type RuntimePrintSink = dyn FnMut(String) + Send;

/// Typed failure of [`HostRuntime::new`].
///
/// Construction fails only when a process-unique identity space is exhausted
/// (the execution-scope arena or its operation-registry tag space). Every
/// variant carries the typed underlying error so callers can match on stable
/// codes instead of parsing messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HostRuntimeInitError {
    /// The execution-scope arena identity space is exhausted
    /// ([`ExecutionScopeError::ArenaExhausted`]).
    Scope(ExecutionScopeError),
}

impl std::fmt::Display for HostRuntimeInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Scope(error) => write!(f, "host runtime scope creation failed: {error}"),
        }
    }
}

impl std::error::Error for HostRuntimeInitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Scope(error) => Some(error),
        }
    }
}

impl From<HostRuntimeInitError> for crate::vm::VmError {
    fn from(error: HostRuntimeInitError) -> Self {
        match error {
            HostRuntimeInitError::Scope(ExecutionScopeError::Resource(resource)) => {
                Self::Resource(resource)
            }
            HostRuntimeInitError::Scope(ExecutionScopeError::ArenaExhausted(resource)) => {
                Self::Resource(resource)
            }
            HostRuntimeInitError::Scope(ExecutionScopeError::Operation(operation)) => {
                Self::Operation(operation)
            }
            HostRuntimeInitError::Scope(other) => Self::ExecutionScope(other),
        }
    }
}

/// Generic adapter that turns a completed execution-scope host operation into
/// the guest-visible call return.
///
/// Host modules (e.g. the sqlite builtin) register one of these against the
/// raw operation id when they start an async operation; the VM's pending
/// host-call awaiting invokes it once when it observes the operation
/// terminal. The core never inspects the concrete module value — only this
/// module-provided closure does.
pub(crate) type PendingOpResult =
    Box<dyn FnOnce(&mut crate::vm::Vm) -> crate::vm::VmResult<crate::vm::CallReturn> + Send>;

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
    /// Monotonic allocator for bridge-external host-operation ids (small
    /// values that can never collide with the packed ids of the single
    /// execution-scope operation registry). Survives VM resets.
    next_host_op_id: u64,
    /// The host-agnostic execution scope of this runtime, created Active.
    ///
    /// One host runtime always owns exactly one live scope. Scope close
    /// drives this scope's resource table and operation registry to
    /// quiescence; it must never clear [`HostModuleStore`] state
    /// (`module_state`), whose lifecycle is deliberately independent. This
    /// scope is authoritative for both resources and operations.
    execution_scope: ExecutionScope,
    module_state: HostModuleStore,
    /// The current async host bridge generation, if any.
    ///
    /// Each installed bridge is a distinct *generation*: a fresh
    /// `Arc<Mutex<Box<dyn HostAsyncBridge>>>` carrying its own bridge box and
    /// its own mutex. Bridge-submitted operation drivers clone the exact
    /// generation they were submitted against, so replacing or clearing this
    /// current generation never invalidates outstanding operations: old
    /// generations drop only after every driver that holds a clone finishes.
    /// This is the enforceable ownership that eliminates the previous raw
    /// `BridgePtr` lifetime coupling (no raw pointer can outlive its bridge
    /// allocation under the public APIs).
    pub(crate) async_bridge: Option<Arc<Mutex<Box<dyn HostAsyncBridge>>>>,
    pub(crate) stream_drivers: HashMap<u64, Box<dyn HostStreamDriver>>,
    pub(crate) runtime_print_sink: Option<Box<RuntimePrintSink>>,
    /// Module-registered adapters that materialize the guest-visible return of
    /// a completed execution-scope host operation, keyed by raw operation id.
    /// Populated by generic host-SDK consumers and cleared on scope reset.
    pub(crate) pending_op_results: HashMap<u64, PendingOpResult>,
}

impl HostRuntime {
    /// Creates an empty host runtime with default capability and resource
    /// limits and no bound functions.
    ///
    /// Fallible: the execution-scope identity spaces (the resource arena and
    /// the operation-registry tag space) can be exhausted. Callers must
    /// propagate the typed [`HostRuntimeInitError`]; there is no infallible
    /// construction path that can panic on exhaustion.
    pub(crate) fn new() -> Result<Self, HostRuntimeInitError> {
        Ok(Self {
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
            next_host_op_id: 1,
            execution_scope: ExecutionScope::new().map_err(HostRuntimeInitError::Scope)?,
            module_state: HostModuleStore::new(),
            async_bridge: None,
            stream_drivers: HashMap::new(),
            runtime_print_sink: None,
            pending_op_results: HashMap::new(),
        })
    }

    /// Closes run-scoped host state between runs: the async bridge, callable
    /// streams and pending-result adapters are cleared. Host bindings,
    /// capability allow-lists, module state and the execution scope are the
    /// authoritative lifecycle owners; interpreter resets drive the scope
    /// close/recycle themselves.
    ///
    /// The return type stays `()` for the migration-period builtin caller
    /// (`close_all_handles`); the execution scope's own close/recycle reports
    /// failures through the typed two-phase reset path.
    pub(crate) fn reset_for_reuse(&mut self) {
        self.stream_drivers.clear();
        // Drop any module-registered pending-call adapters: they belong to
        // execution-scope operations that a reset is cancelling/closing, and
        // the concrete value cells they reference are released by the
        // modules' own scope-close lifecycle.
        self.pending_op_results.clear();
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

    pub(crate) fn execution_scope_push_resource_with_key<T: HostResource>(
        &mut self,
        value: T,
        key: ResourceTypeKey,
    ) -> ExecutionScopeResult<Resource<T>> {
        self.execution_scope.push_resource_with_key(value, key)
    }

    /// Starts the exact resource access frame after checking operation
    /// associations and all handle/type/key/alias constraints.
    pub(crate) fn execution_scope_begin_resource_access(
        &mut self,
        requests: Vec<ResourceAccessRequest>,
    ) -> ExecutionScopeResult<ResourceAccessFrame<'_>> {
        self.execution_scope.begin_resource_access(requests)
    }

    /// Inserts a typed child resource linked to `parent` (guarded).
    pub(crate) fn execution_scope_push_child_resource<T: HostResource, P: HostResource>(
        &mut self,
        value: T,
        parent: &Resource<P>,
    ) -> ExecutionScopeResult<Resource<T>> {
        self.execution_scope.push_child_resource(value, parent)
    }

    pub(crate) fn execution_scope_push_child_resource_with_key<T: HostResource, P: HostResource>(
        &mut self,
        value: T,
        parent: &Resource<P>,
        key: ResourceTypeKey,
    ) -> ExecutionScopeResult<Resource<T>> {
        self.execution_scope
            .push_child_resource_with_key(value, parent, key)
    }

    /// Starts an operation in the owned execution scope with the full generic
    /// spec (driver, resource association, deadline, cleanup/cancel).
    pub(crate) fn execution_scope_start_operation(
        &mut self,
        spec: OperationSpec,
    ) -> ExecutionScopeResult<OperationId> {
        self.execution_scope.start_operation(spec)
    }

    /// Polls one registered execution-scope operation to its terminal state
    /// (generic `HostOperation` driver; no domain owner/poller dispatch).
    pub(crate) fn execution_scope_poll_operation(
        &mut self,
        id: OperationId,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<OperationResult<OperationOutcome>> {
        self.execution_scope.poll_operation(id, cx)
    }

    /// Cancels one registered execution-scope operation by id, forwarding the
    /// reason to its driver.
    pub(crate) fn execution_scope_cancel_operation(
        &mut self,
        id: OperationId,
        reason: OperationCancelReason,
    ) -> ExecutionScopeResult<bool> {
        self.execution_scope.cancel_operation(id, reason)
    }

    /// Aborts one registered execution-scope operation so it never produces a
    /// guest-visible result: cancels the driver exactly once if pending, then
    /// consumes and immediately releases the slot (restoring full capacity
    /// and making the id stale). Used to roll back a registered operation
    /// whose fallible handoff (e.g. a bridge submission) failed.
    pub(crate) fn execution_scope_abort_operation(
        &mut self,
        id: OperationId,
        reason: OperationCancelReason,
    ) -> ExecutionScopeResult<bool> {
        self.execution_scope.abort_operation(id, reason)
    }

    /// Registers the module-provided adapter that materializes the
    /// guest-visible return of the execution-scope operation `raw` once it
    /// completes. Overwrites any earlier provider for the same raw id.
    #[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
    pub(crate) fn register_pending_op_result(&mut self, raw: u64, provider: PendingOpResult) {
        self.pending_op_results.insert(raw, provider);
    }

    /// Takes (removes and returns) the module adapter for `raw`, so the
    /// awaiting path can materialize the operation's value exactly once.
    pub(crate) fn take_pending_op_result(&mut self, raw: u64) -> Option<PendingOpResult> {
        self.pending_op_results.remove(&raw)
    }

    /// Removes (discards) the module adapter for `raw` without running it.
    pub(crate) fn remove_pending_op_result(&mut self, raw: u64) {
        self.pending_op_results.remove(&raw);
    }

    /// Allocates the next bridge-external host-operation id.
    ///
    /// The counter is deliberately independent of the execution-scope
    /// registry: bridge-external ids are small, monotonic values that never
    /// collide with the packed modern operation ids (a valid modern id
    /// requires a nonzero registry-tag field). The counter survives resets
    /// and saturates instead of wrapping on exhaustion.
    pub(crate) fn allocate_host_op_id(&mut self) -> u64 {
        let id = self.next_host_op_id;
        self.next_host_op_id = self.next_host_op_id.saturating_add(1);
        id
    }

    /// Closes one resource in the owned execution scope, cancelling its
    /// associated operations first (generic association logic).
    pub(crate) fn execution_scope_close_resource<T: HostResource>(
        &mut self,
        handle: crate::vm::resource::ResourceHandle,
        reason: crate::vm::resource::ResourceCloseReason,
    ) -> ExecutionScopeResult<crate::vm::resource::CloseProgress> {
        self.execution_scope.close_resource::<T>(handle, reason)
    }

    /// Marks a resource in the owned execution scope as guest-owned (exact
    /// host-return ownership transfer). See
    /// [`ExecutionScope::mark_resource_guest_owned`].
    pub(crate) fn execution_scope_mark_guest_owned(
        &mut self,
        handle: crate::vm::resource::ResourceHandle,
    ) -> ExecutionScopeResult<()> {
        self.execution_scope.mark_resource_guest_owned(handle)
    }

    /// Marks a host-owned resource guest-owned after verifying its live slot
    /// key (C4 exact-return ownership transfer).
    pub(crate) fn execution_scope_mark_guest_owned_with_key(
        &mut self,
        handle: crate::vm::resource::ResourceHandle,
        expected_key: &crate::host_api::ResourceTypeKey,
    ) -> ExecutionScopeResult<()> {
        self.execution_scope
            .mark_resource_guest_owned_with_key(handle, expected_key)
    }

    /// Read-only exact-argument preflight (arena/generation/key/open/
    /// ownership/children/operation) used by the manual exact host-call
    /// contract before the user function runs.
    pub(crate) fn execution_scope_validate_exact_access(
        &self,
        handle: crate::vm::resource::ResourceHandle,
        expected_key: &crate::host_api::ResourceTypeKey,
        mode: crate::vm::resource::ResourceAccessMode,
    ) -> ExecutionScopeResult<()> {
        self.execution_scope
            .validate_exact_access(handle, expected_key, mode)
    }

    /// Releases the guest owner of a resource in the owned execution scope
    /// (guest local death). See
    /// [`ExecutionScope::release_guest_owner`].
    pub(crate) fn execution_scope_release_guest_owner(
        &mut self,
        handle: crate::vm::resource::ResourceHandle,
        release: crate::vm::resource::OwnershipRelease,
    ) -> ExecutionScopeResult<crate::vm::resource::GuestReleaseOutcome> {
        self.execution_scope.release_guest_owner(handle, release)
    }

    /// Records a best-effort guest-release failure in the scope's first-error
    /// latch. See [`ExecutionScope::record_guest_release_error`].
    pub(crate) fn execution_scope_record_release_error(
        &mut self,
        error: crate::vm::resource::ResourceError,
    ) {
        self.execution_scope.record_guest_release_error(error);
    }

    /// Atomically takes a guest-owned resource out of the owned execution
    /// scope. See [`ExecutionScope::take_resource`].
    pub(crate) fn execution_scope_take_resource<T: HostResource>(
        &mut self,
        handle: crate::vm::resource::ResourceHandle,
    ) -> ExecutionScopeResult<T> {
        self.execution_scope.take_resource::<T>(handle)
    }

    pub(crate) fn execution_scope_take_resource_with_key<T: HostResource>(
        &mut self,
        handle: crate::vm::resource::ResourceHandle,
        key: ResourceTypeKey,
    ) -> ExecutionScopeResult<T> {
        self.execution_scope
            .take_resource_with_key::<T>(handle, key)
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

    /// Drives exactly one round of the closing scope's close pipeline with a
    /// no-op waker (nonblocking). Used only by `Vm::drop`: it synchronously
    /// cancels every pending operation and issues child-first `begin_close` to
    /// every live resource, then polls that single round. It never loops and
    /// never waits for quiescence — genuinely event-driven Pending resources
    /// stay in `Closing` and are released by their own `Drop` guards.
    pub(crate) fn drive_execution_scope_close_once_with_noop_waker(&mut self) {
        struct DropNoopWake;
        impl std::task::Wake for DropNoopWake {
            fn wake(self: Arc<Self>) {}
        }
        let waker = Arc::new(DropNoopWake).into();
        let mut cx = Context::from_waker(&waker);
        let _ = self.execution_scope.poll_close(&mut cx);
    }

    /// Recycles the owned execution scope to a fresh, empty, Active scope.
    ///
    /// Takes the current scope out **only** once it is Quiescent (all cleanup
    /// finished), installs a brand-new scope in its place, and returns the old
    /// quiescent scope so the caller can inspect its terminal outcome.
    ///
    /// The replacement scope is always created internally via
    /// [`ExecutionScope::new`] — no caller can inject a Closing, Quiescent,
    /// or resource-bearing `next`. The fresh scope is Active, holds 0
    /// resources and 0 operations, and carries a brand-new arena/registry
    /// identity that cannot alias any handle or operation id from the old
    /// scope.
    ///
    /// A non-Quiescent (Active or Closing) scope is rejected with
    /// [`ExecutionScopeError::ScopeNotQuiescent`] *before any mutation*, so a
    /// failed recycle leaves the owned scope and its content untouched
    /// (atomic). This is the only scope-replacement path, so cleanup can never
    /// be bypassed.
    ///
    /// Identity exhaustion: if a fresh resource arena or operation-registry
    /// identity cannot be allocated, replacement fails with the corresponding
    /// typed [`ExecutionScopeError`] *before any mutation*: the old (quiescent)
    /// scope stays installed and intact for diagnostics, and no partial scope is
    /// ever installed. The caller (the Vm reset path) must treat this as a
    /// terminal recycle failure and poison the VM.
    ///
    /// Consumed by the next-scope reset integration (`Vm::reset_for_reuse` →
    /// scope recycle); this wiring-only commit keeps it crate-private and
    /// gated rather than connecting it to `Vm` reset semantics.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn take_quiescent_scope(&mut self) -> ExecutionScopeResult<ExecutionScope> {
        if !self.execution_scope.is_quiescent() {
            return Err(ExecutionScopeError::ScopeNotQuiescent);
        }
        // Allocate the replacement identity *before* touching the owned
        // scope, so an exhausted arena leaves the old scope untouched.
        let replacement = ExecutionScope::new()?;
        Ok(std::mem::replace(&mut self.execution_scope, replacement))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::execution_scope::ScopeCloseOutcome;
    use crate::vm::operation::{
        HostOperation, OperationCancelReason, OperationResult, OperationSpec,
    };
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

    /// A generic fake operation that stays pending until cancelled.
    struct TestOperation;

    impl HostOperation for TestOperation {
        fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
            Poll::Pending
        }

        fn cancel(&mut self, _reason: OperationCancelReason) -> OperationResult<()> {
            Ok(())
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
        let host = HostRuntime::new().expect("host runtime");
        assert_eq!(host.execution_scope_state(), ScopeState::Active);
        assert!(host.execution_scope_is_active());
        assert!(!host.execution_scope_is_quiescent());
        assert_eq!(host.execution_scope_resource_count(), 0);
        assert_eq!(host.execution_scope_operation_count(), 0);
    }

    #[test]
    fn take_quiescent_scope_rejects_non_quiescent_scope_atomically() {
        let mut host = HostRuntime::new().expect("host runtime");

        // An Active scope (close was never begun) is not quiescent.
        let result = host.take_quiescent_scope();
        let Err(error) = result else {
            panic!("an active scope must refuse recycle");
        };
        assert_eq!(error, ExecutionScopeError::ScopeNotQuiescent);
        // Failure is atomic: the owned scope and its content are untouched.
        assert_eq!(host.execution_scope_state(), ScopeState::Active);
        assert_eq!(host.execution_scope_resource_count(), 0);
        assert_eq!(host.execution_scope_operation_count(), 0);

        let mut host = HostRuntime::new().expect("host runtime");
        let old_handle = host
            .execution_scope_push_resource(TestResource)
            .expect("push into active scope");
        // A Closing scope (close begun, not driven to quiescence) also refuses.
        assert!(
            host.execution_scope_begin_close(ResourceCloseReason::Requested)
                .expect("begin close")
        );
        assert_eq!(host.execution_scope_state(), ScopeState::Closing);
        let result = host.take_quiescent_scope();
        let Err(error) = result else {
            panic!("a closing scope must refuse recycle");
        };
        assert_eq!(error, ExecutionScopeError::ScopeNotQuiescent);
        // Atomic: no mutation — the scope is still Closing and its resource
        // table/arena/state are exactly what they were before the attempt.
        assert_eq!(host.execution_scope_state(), ScopeState::Closing);
        assert_eq!(host.execution_scope_resource_count(), 1);
        assert_eq!(host.execution_scope_operation_count(), 0);
        host.execution_scope()
            .resources()
            .get(&old_handle)
            .expect("the rejected recycle must leave the owned resource table intact");
    }

    #[test]
    fn take_quiescent_scope_yields_fresh_active_empty_isolated_scope() {
        let mut host = HostRuntime::new().expect("host runtime");
        let old_handle = host
            .execution_scope_push_resource(TestResource)
            .expect("push into active scope");
        let old_op = host
            .execution_scope_start_operation(OperationSpec::new(TestOperation))
            .expect("start operation in active scope");
        assert_eq!(host.execution_scope_resource_count(), 1);
        assert_eq!(host.execution_scope_operation_count(), 1);

        // Close fully: only a Quiescent scope may be recycled.
        assert!(
            host.execution_scope_begin_close(ResourceCloseReason::VmReset)
                .expect("begin close")
        );
        drive_scope_quiescent(&mut host);

        let old_scope = host
            .take_quiescent_scope()
            .expect("quiescent scope is recyclable");
        assert_eq!(old_scope.state(), ScopeState::Quiescent);
        assert_eq!(old_scope.resources().len(), 0, "old scope is fully closed");
        assert_eq!(
            old_scope.operations().len(),
            0,
            "old scope drained all operations"
        );
        assert_eq!(old_scope.terminal(), Some(&ScopeCloseOutcome::Success));

        // The fresh scope starts Active and empty.
        assert_eq!(host.execution_scope_state(), ScopeState::Active);
        assert!(host.execution_scope_is_active());
        assert!(!host.execution_scope_is_quiescent());
        assert_eq!(host.execution_scope_resource_count(), 0);
        assert_eq!(host.execution_scope_operation_count(), 0);

        // Arena/table isolation: a handle from the recycled scope must not
        // resolve in the new scope.
        let error = host
            .execution_scope()
            .resources()
            .get(&old_handle)
            .expect_err("an old-scope handle must be rejected by the new scope");
        assert_eq!(error.code(), ResourceErrorCode::ResourceHandleWrongTable);

        // Operation-registry isolation: an id from the old scope is rejected.
        let status = host.execution_scope().operations().status(old_op);
        assert!(
            status.is_err(),
            "an old-scope operation id must be rejected"
        );

        // The new scope is live: fresh inserts/operations land and resolve.
        let new_handle = host
            .execution_scope_push_resource(TestResource)
            .expect("fresh scope accepts a new resource");
        assert_eq!(host.execution_scope_resource_count(), 1);
        let _new_op = host
            .execution_scope_start_operation(OperationSpec::new(TestOperation))
            .expect("fresh scope accepts a new operation");
        assert_eq!(host.execution_scope_operation_count(), 1);
        host.execution_scope()
            .resources()
            .get(&new_handle)
            .expect("the new-scope handle must resolve in its own table");
    }

    #[test]
    fn host_runtime_construction_propagates_typed_arena_exhaustion() {
        // The first construction consumes the max handout; the second is the
        // first call after the max and must fail typed, never panic.
        static COUNTER: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(crate::vm::resource::handle::MAX_HANDLE_ARENA_ID);
        let _source = crate::vm::resource::table::test_seam::ScopedArenaSource::install(&COUNTER);

        let _first = HostRuntime::new().expect("last arena id must construct");
        let error = match HostRuntime::new() {
            Ok(_) => panic!("arena space must be exhausted"),
            Err(error) => error,
        };
        match error {
            HostRuntimeInitError::Scope(ExecutionScopeError::ArenaExhausted(resource)) => {
                assert_eq!(
                    resource.code(),
                    ResourceErrorCode::ResourceTableArenaExhausted,
                    "typed arena-exhaustion code must survive ResourceTable -> ExecutionScope -> HostRuntime"
                );
            }
            other => panic!("expected scope arena exhaustion, got {other:?}"),
        }
    }

    #[test]
    fn host_runtime_construction_propagates_typed_operation_tag_exhaustion() {
        static COUNTER: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(crate::vm::operation::id::MAX_REGISTRY_TAG + 1);
        let _source =
            crate::vm::operation::id::test_seam::ScopedRegistryTagSource::install(&COUNTER);

        let error = match HostRuntime::new() {
            Ok(_) => panic!("operation registry tag exhaustion must fail construction"),
            Err(error) => error,
        };
        match error {
            HostRuntimeInitError::Scope(ExecutionScopeError::Operation(operation)) => {
                assert_eq!(
                    operation.code(),
                    crate::vm::operation::OperationErrorCode::OperationRegistryTagExhausted
                );
                assert_eq!(
                    operation.limit(),
                    Some(crate::vm::operation::id::MAX_REGISTRY_TAG)
                );
                assert_eq!(
                    operation.value(),
                    Some(crate::vm::operation::id::MAX_REGISTRY_TAG + 1)
                );
            }
            other => panic!("expected scope operation exhaustion, got {other:?}"),
        }
    }

    #[test]
    fn recycle_at_arena_exhaustion_fails_typed_without_partial_scope_swap() {
        let mut host = HostRuntime::new().expect("host runtime");
        let old_handle = host
            .execution_scope_push_resource(TestResource)
            .expect("push into active scope");
        assert!(
            host.execution_scope_begin_close(ResourceCloseReason::VmReset)
                .expect("begin close")
        );
        drive_scope_quiescent(&mut host);
        assert!(host.execution_scope_is_quiescent());

        // Exhaust the arena so the replacement scope cannot be created. Set
        // the counter past the max valid handout so the *first* allocation
        // inside the recycle already fails.
        static COUNTER: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(crate::vm::resource::handle::MAX_HANDLE_ARENA_ID + 1);
        let _source = crate::vm::resource::table::test_seam::ScopedArenaSource::install(&COUNTER);

        let result = host.take_quiescent_scope();
        let Err(error) = result else {
            panic!("recycle must fail at arena exhaustion");
        };
        match error {
            ExecutionScopeError::ArenaExhausted(resource) => {
                assert_eq!(
                    resource.code(),
                    ResourceErrorCode::ResourceTableArenaExhausted,
                    "typed arena-exhaustion code must survive the recycle path"
                );
            }
            other => panic!("expected ArenaExhausted, got {other:?}"),
        }

        // Atomic failure: the old quiescent scope stays installed and intact
        // (no partial replacement, no malformed scope).
        assert!(host.execution_scope_is_quiescent());
        assert_eq!(host.execution_scope_resource_count(), 0);
        assert_eq!(host.execution_scope_operation_count(), 0);
        let old_error = host
            .execution_scope()
            .resources()
            .get(&old_handle)
            .expect_err("old handle must still be resolvable to a closed resource");
        assert_eq!(old_error.code(), ResourceErrorCode::ResourceAlreadyClosed);
        assert_eq!(
            host.execution_scope().terminal(),
            Some(&ScopeCloseOutcome::Success)
        );
    }

    #[test]
    fn recycle_after_exhaustion_guard_drop_succeeds_and_keeps_uniqueness() {
        let mut host = HostRuntime::new().expect("host runtime");
        let _ = host.execution_scope_push_resource(TestResource).unwrap();
        assert!(
            host.execution_scope_begin_close(ResourceCloseReason::VmReset)
                .expect("begin close")
        );
        drive_scope_quiescent(&mut host);

        // A real construction attempt under the active exhaustion window must
        // fail with the typed scope error.
        {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(
                crate::vm::resource::handle::MAX_HANDLE_ARENA_ID + 1,
            );
            let _source =
                crate::vm::resource::table::test_seam::ScopedArenaSource::install(&COUNTER);
            let error = match HostRuntime::new() {
                Ok(_) => panic!("active arena exhaustion must reject construction"),
                Err(error) => error,
            };
            assert!(matches!(
                error,
                HostRuntimeInitError::Scope(ExecutionScopeError::ArenaExhausted(resource))
                    if resource.code() == ResourceErrorCode::ResourceTableArenaExhausted
            ));
        }
        // The guard dropped: the real global source is authoritative again and
        // this independent host construction succeeds.
        let _independent_host = HostRuntime::new().expect("construction recovers after guard drop");
        let old_scope = host
            .take_quiescent_scope()
            .expect("the existing host can still recycle after the guard is gone");
        assert_eq!(old_scope.state(), ScopeState::Quiescent);
        assert!(host.execution_scope_is_active());
        let new_handle = host
            .execution_scope_push_resource(TestResource)
            .expect("fresh scope accepts a new resource");
        host.execution_scope()
            .resources()
            .get(&new_handle)
            .expect("new-scope handle resolves in its own table");
    }
}
