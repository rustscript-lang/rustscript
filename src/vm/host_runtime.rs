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
use std::task::{Context, Poll};

use crate::host_api::HostImportSchema;
use crate::vm::async_host::{
    HostStreamAdmissionRollback, HostStreamDriver, HostStreamTermination,
    PendingHostStreamTermination, preserve_stream_cleanup,
};
use crate::vm::execution_scope::{ExecutionScope, ExecutionScopeError, ScopeCloseOutcome};
use crate::vm::host::{
    HostAsyncBridge, HostAsyncOpTerminal, HostOpId, ScopedOperationCompletion, VmHostFunction,
};
use crate::vm::operation::{OperationCancelReason, OperationId};
use crate::vm::standard_composition::StandardSurfaceComposition;
use crate::vm::{VmError, VmResult};

/// Embedder-supplied print sink for `print`/`debug` output.
pub(crate) type RuntimePrintSink = dyn FnMut(String) + Send;

#[derive(Debug)]
struct BridgeOperationState {
    cancellation_reason: Option<OperationCancelReason>,
    cancellation_error: Option<String>,
    terminal: Option<HostAsyncOpTerminal>,
    cleanup_error: Option<String>,
}

impl BridgeOperationState {
    fn new() -> Self {
        Self {
            cancellation_reason: None,
            cancellation_error: None,
            terminal: None,
            cleanup_error: None,
        }
    }
}

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
    /// True after reset has begun closing the old scope but before the generic
    /// close driver has reached quiescence. No usable replacement scope may be
    /// admitted while this flag is set.
    pub(crate) scope_reset_pending: bool,
    /// The terminal failure of the current reset attempt, if any. A failed
    /// reset must stay non-reusable and must not silently start another reset
    /// or publish a callback registry on a later poll.
    scope_reset_error: Option<ExecutionScopeError>,
    /// An early reset failure that is outside the generic scope error domain.
    /// This remains authoritative so an empty active scope cannot look reusable.
    reset_error: Option<String>,
    /// The one replacement scope allocated for the current reset. It remains
    /// unpublished until the old scope in `execution_scope` reaches
    /// quiescence.
    replacement_execution_scope: Option<ExecutionScope>,
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
    /// Every active bridge-owned operation, including pending host calls that
    /// did not originate from `submit_host_future`. Entries remain until the
    /// bridge acknowledges a terminal/quiescent state and cleanup succeeds.
    bridge_operations: HashMap<HostOpId, BridgeOperationState>,
    /// Adapter-owned completions for operations driven by the execution scope.
    pub(crate) scoped_operation_completions: HashMap<OperationId, ScopedOperationCompletion>,
    /// Host-owned callable stream drivers. The VM stores only this generic
    /// driver contract; HTTP/SSE state remains in the adapter module.
    pub(crate) stream_drivers: HashMap<HostOpId, Box<dyn HostStreamDriver>>,
    /// Drivers whose callable continuation has ended but whose worker/resource
    /// cleanup still needs asynchronous polling.
    pub(crate) pending_stream_terminations: HashMap<HostOpId, PendingHostStreamTermination>,
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
            scope_reset_pending: false,
            scope_reset_error: None,
            reset_error: None,
            replacement_execution_scope: None,
            module_state_store: super::host_state::ModuleStateStore::new(),
            allow_default_builtin_capabilities: true,
            allowed_builtin_calls: Vec::new(),
            allow_default_host_capabilities: true,
            allowed_host_function_slots: Vec::new(),
            allow_default_host_fallback: true,
            standard_composition: None,
            submitted_host_ops: HashSet::new(),
            bridge_operations: HashMap::new(),
            scoped_operation_completions: HashMap::new(),
            stream_drivers: HashMap::new(),
            pending_stream_terminations: HashMap::new(),
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

    pub(crate) fn reserve_submitted_host_op(&mut self) -> VmResult<HostOpId> {
        let op_id = self.next_host_op_id;
        if op_id == 0 || op_id == HostOpId::MAX {
            return Err(VmError::HostError(
                "async host operation id space exhausted".to_string(),
            ));
        }
        if self.submitted_host_ops.contains(&op_id) {
            return Err(VmError::HostError(format!(
                "submitted host op {op_id} is already tracked"
            )));
        }
        if self.bridge_operations.contains_key(&op_id) {
            return Err(VmError::HostError(format!(
                "bridge host op {op_id} is already tracked"
            )));
        }
        self.submitted_host_ops.insert(op_id);
        self.bridge_operations
            .insert(op_id, BridgeOperationState::new());
        self.next_host_op_id = op_id + 1;
        Ok(op_id)
    }

    pub(crate) fn rollback_submitted_host_op(&mut self, op_id: HostOpId) {
        let removed_submitted = self.submitted_host_ops.remove(&op_id);
        let removed_bridge = self.bridge_operations.remove(&op_id).is_some();
        if removed_submitted || removed_bridge {
            self.next_host_op_id = op_id;
        }
    }

    pub(crate) fn track_bridge_host_op(&mut self, op_id: HostOpId) -> VmResult<()> {
        if self.bridge_operations.contains_key(&op_id) {
            return Ok(());
        }
        self.bridge_operations
            .insert(op_id, BridgeOperationState::new());
        Ok(())
    }

    pub(crate) fn has_active_bridge_operations(&self) -> bool {
        !self.bridge_operations.is_empty()
    }

    pub(crate) fn has_pending_bridge_cancellations(&self) -> bool {
        self.bridge_operations
            .values()
            .any(|state| state.cancellation_reason.is_some())
    }

    pub(crate) fn is_bridge_operation_tracked(&self, op_id: HostOpId) -> bool {
        self.bridge_operations.contains_key(&op_id)
    }

    pub(crate) fn request_cancel_host_op(
        &mut self,
        op_id: HostOpId,
        reason: OperationCancelReason,
    ) -> VmResult<()> {
        let should_request = {
            let state = self.bridge_operations.get_mut(&op_id).ok_or_else(|| {
                VmError::HostError(format!("bridge host op {op_id} is not tracked"))
            })?;
            if let Some(error) = state.cancellation_error.as_ref() {
                return Err(VmError::HostError(error.clone()));
            }
            if let Some(error) = state.cleanup_error.as_ref() {
                return Err(VmError::HostError(error.clone()));
            }
            if state.terminal.is_some() || state.cancellation_reason.is_some() {
                false
            } else {
                state.cancellation_reason = Some(reason);
                true
            }
        };
        if !should_request {
            return Ok(());
        }

        let result = match self.async_bridge.as_mut() {
            Some(bridge) => bridge.request_cancel_op(op_id, reason),
            None => Err(VmError::HostError(format!(
                "cannot cancel bridge host op {op_id} without an async bridge"
            ))),
        };
        if let Err(error) = result {
            if let Some(state) = self.bridge_operations.get_mut(&op_id) {
                state.cancellation_error = Some(error.to_string());
            }
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn request_cancel_submitted_host_ops(
        &mut self,
        reason: OperationCancelReason,
    ) -> VmResult<()> {
        let mut op_ids = self.bridge_operations.keys().copied().collect::<Vec<_>>();
        op_ids.sort_unstable();
        let mut first_error = None;
        for op_id in op_ids {
            let result = self.request_cancel_host_op(op_id, reason);
            if first_error.is_none() {
                first_error = result.err();
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Requests cancellation for every bridge operation as a best-effort
    /// terminal/drop action. IDs are deliberately retained until a later
    /// acknowledgement poll or until this runtime is dropped.
    pub(crate) fn cancel_submitted_host_ops(&mut self, reason: OperationCancelReason) {
        let _ = self.request_cancel_submitted_host_ops(reason);
    }

    fn bridge_operation_error(message: String) -> VmError {
        VmError::HostError(message)
    }

    pub(crate) fn complete_bridge_operation(
        &mut self,
        op_id: HostOpId,
        terminal: HostAsyncOpTerminal,
    ) -> VmResult<()> {
        let state_snapshot = self.bridge_operations.get(&op_id).map(|state| {
            (
                state.cancellation_reason,
                state.cancellation_error.clone(),
                state.terminal,
                state.cleanup_error.clone(),
            )
        });
        let Some((cancellation_reason, cancellation_error, previous_terminal, cleanup_error)) =
            state_snapshot
        else {
            return Err(Self::bridge_operation_error(format!(
                "bridge host op {op_id} is not tracked"
            )));
        };
        if let Some(error) = cancellation_error.or(cleanup_error) {
            return Err(Self::bridge_operation_error(error));
        }
        if cancellation_reason.is_some() && terminal != HostAsyncOpTerminal::Cancelled {
            return Err(Self::bridge_operation_error(format!(
                "bridge host op {op_id} has an outstanding cancellation request"
            )));
        }
        if let Some(previous_terminal) = previous_terminal {
            if previous_terminal != terminal {
                return Err(Self::bridge_operation_error(format!(
                    "bridge host op {op_id} reached conflicting terminal states"
                )));
            }
        } else if let Some(state) = self.bridge_operations.get_mut(&op_id) {
            state.terminal = Some(terminal);
        }

        let cleanup_result = match self.async_bridge.as_mut() {
            Some(bridge) => bridge.cleanup_op(op_id, terminal),
            None => Err(Self::bridge_operation_error(format!(
                "cannot clean up bridge host op {op_id} without an async bridge"
            ))),
        };
        match cleanup_result {
            Ok(()) => {
                self.bridge_operations.remove(&op_id);
                self.submitted_host_ops.remove(&op_id);
                Ok(())
            }
            Err(error) => {
                if let Some(state) = self.bridge_operations.get_mut(&op_id) {
                    state.cleanup_error = Some(error.to_string());
                }
                Err(error)
            }
        }
    }

    pub(crate) fn bridge_cancellation_requested(&self, op_id: HostOpId) -> bool {
        self.bridge_operations
            .get(&op_id)
            .is_some_and(|state| state.cancellation_reason.is_some())
    }

    /// Polls one cancellation acknowledgement. An operation is removed only
    /// after `poll_cancel_op` reports quiescence and `cleanup_op` succeeds.
    pub(crate) fn poll_bridge_operation_cancellation(
        &mut self,
        op_id: HostOpId,
        cx: &mut Context<'_>,
    ) -> Poll<VmResult<()>> {
        let state_snapshot = self.bridge_operations.get(&op_id).map(|state| {
            (
                state.cancellation_reason,
                state.cancellation_error.clone(),
                state.terminal,
                state.cleanup_error.clone(),
            )
        });
        let Some((cancellation_reason, cancellation_error, terminal, cleanup_error)) =
            state_snapshot
        else {
            return Poll::Ready(Ok(()));
        };
        if let Some(error) = cancellation_error.or(cleanup_error) {
            return Poll::Ready(Err(Self::bridge_operation_error(error)));
        }
        if terminal.is_some() {
            return Poll::Ready(Ok(()));
        }
        if cancellation_reason.is_none() {
            return Poll::Ready(Err(Self::bridge_operation_error(format!(
                "bridge host op {op_id} has no cancellation request"
            ))));
        }

        let poll_result = match self.async_bridge.as_mut() {
            Some(bridge) => bridge.poll_cancel_op(op_id, cx),
            None => Poll::Ready(Err(Self::bridge_operation_error(format!(
                "cannot poll cancellation for bridge host op {op_id} without an async bridge"
            )))),
        };
        match poll_result {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(())) => {
                Poll::Ready(self.complete_bridge_operation(op_id, HostAsyncOpTerminal::Cancelled))
            }
            Poll::Ready(Err(error)) => {
                if let Some(state) = self.bridge_operations.get_mut(&op_id) {
                    state.cancellation_error = Some(error.to_string());
                }
                Poll::Ready(Err(error))
            }
        }
    }

    /// Polls all cancellation acknowledgements. Entries without a cancellation
    /// request remain pending; reset callers request all of them first.
    pub(crate) fn poll_bridge_operations(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<()>> {
        let mut op_ids = self.bridge_operations.keys().copied().collect::<Vec<_>>();
        op_ids.sort_unstable();
        let mut first_error = None;
        for op_id in op_ids {
            if !self.bridge_cancellation_requested(op_id) {
                continue;
            }
            match self.poll_bridge_operation_cancellation(op_id, cx) {
                Poll::Pending | Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(error)) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        if let Some(error) = first_error {
            Poll::Ready(Err(error))
        } else if self.bridge_operations.is_empty() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    /// Starts generic execution-scope reset and replaces the old scope only
    /// after its operation/resource registries report quiescence. Exactly one
    /// replacement is allocated at reset start and retained privately while
    /// the old scope closes; callers must poll
    /// [`poll_reset_execution_scope`](Self::poll_reset_execution_scope) before
    /// the VM can be reused.
    ///
    /// Persistent policy/configuration — including the HTTP host
    /// configuration and max-in-flight policy, IO policy, SQLite policy, and
    /// external-extension module state — lives in the persistent
    /// `ModuleStateStore`, which is deliberately **not** touched here. It
    /// therefore survives reset (only per-invocation resources, operations,
    /// and scope-arena runtime state are retired). This function contains no
    /// adapter name, feature, or concrete `TypeId`: it is wholly feature-neutral.
    pub(crate) fn reset_execution_scope(&mut self) -> VmResult<()> {
        if let Some(error) = self.scope_reset_error.clone() {
            return Err(VmError::ExecutionScope(error));
        }
        if let Some(error) = self.reset_error.clone() {
            return Err(VmError::HostError(error));
        }

        if !self.scope_reset_pending {
            if let Err(error) =
                self.request_cancel_submitted_host_ops(OperationCancelReason::VmReset)
            {
                self.mark_reset_failed(&error);
                return Err(error);
            }
            // Allocate the replacement before publishing or closing anything.
            // There is no second allocation after the old scope quiesces.
            let replacement = match ExecutionScope::new() {
                Ok(scope) => scope,
                Err(error) => {
                    self.fail_reset(error.clone());
                    return Err(VmError::ExecutionScope(error));
                }
            };
            let close_result = self.execution_scope.is_active().then(|| {
                self.execution_scope
                    .begin_close(crate::vm::resource::ResourceCloseReason::VmReset)
            });
            if let Some(Err(error)) = close_result {
                self.fail_reset(error.clone());
                return Err(VmError::ExecutionScope(error));
            }
            self.scoped_operation_completions.clear();
            self.replacement_execution_scope = Some(replacement);
            self.scope_reset_pending = true;
        } else {
            if let Err(error) =
                self.request_cancel_submitted_host_ops(OperationCancelReason::VmReset)
            {
                self.mark_reset_failed(&error);
                return Err(error);
            }
            self.scoped_operation_completions.clear();
        }

        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        match self.poll_reset_execution_scope(&mut cx) {
            Poll::Pending | Poll::Ready(Ok(())) => Ok(()),
            Poll::Ready(Err(error)) => Err(error),
        }
    }

    /// Records a terminal reset failure and closes the old scope without
    /// publishing any replacement. The error itself remains authoritative for
    /// all later reset/poll attempts.
    fn fail_reset(&mut self, error: ExecutionScopeError) {
        if self.execution_scope.is_active() {
            let _ = self
                .execution_scope
                .begin_close(crate::vm::resource::ResourceCloseReason::VmReset);
        }
        self.scoped_operation_completions.clear();
        self.stream_drivers.clear();
        self.replacement_execution_scope = None;
        self.scope_reset_pending = false;
        self.scope_reset_error = Some(error);
    }

    /// The old scope remains the guarded `execution_scope` while it is closing;
    /// the one fresh Active scope is installed only after the old scope reaches
    /// `Quiescent`. This is the pool/reuse boundary that prevents stale
    /// operation/resource workers from overlapping a replacement VM run.
    pub(crate) fn poll_reset_execution_scope(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<VmResult<()>> {
        if let Some(error) = self.scope_reset_error.clone() {
            return Poll::Ready(Err(VmError::ExecutionScope(error)));
        }
        if let Some(error) = self.reset_error.clone() {
            return Poll::Ready(Err(VmError::HostError(error)));
        }
        match self.poll_bridge_operations(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(error)) => {
                self.mark_reset_failed(&error);
                return Poll::Ready(Err(error));
            }
            Poll::Ready(Ok(())) => {}
        }
        if !self.scope_reset_pending {
            return Poll::Ready(Ok(()));
        }

        let result = self.execution_scope.poll_close(cx);
        match result {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => {
                self.scope_reset_error = Some(error.clone());
                Poll::Ready(Err(VmError::ExecutionScope(error)))
            }
            Poll::Ready(Ok(ScopeCloseOutcome::Success)) => {
                let replacement = self
                    .replacement_execution_scope
                    .take()
                    .expect("pending scope reset must retain one replacement scope");
                self.execution_scope = replacement;
                self.stream_drivers.clear();
                self.pending_stream_terminations.clear();
                self.scope_reset_pending = false;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Ok(outcome @ ScopeCloseOutcome::SuccessWithErrors(_))) => {
                // The old scope is quiescent but not clean. Keep it and the
                // unpublished replacement in place so a failed reset cannot
                // make the VM/pool appear reusable; retain the exact outcome
                // for the caller instead of collapsing it into success.
                let error = ExecutionScopeError::Close(outcome);
                self.scope_reset_error = Some(error.clone());
                Poll::Ready(Err(VmError::ExecutionScope(error)))
            }
        }
    }

    pub(crate) fn begin_stream_termination(
        &mut self,
        op_id: HostOpId,
        termination: HostStreamTermination,
    ) -> VmResult<()> {
        if self.pending_stream_terminations.contains_key(&op_id) {
            return Ok(());
        }
        let Some(mut driver) = self.stream_drivers.remove(&op_id) else {
            return Err(VmError::HostError(format!(
                "missing callable stream driver {op_id}"
            )));
        };
        if let Err(error) = driver.begin_termination(&mut self.execution_scope, termination) {
            self.pending_stream_terminations.insert(
                op_id,
                PendingHostStreamTermination {
                    driver,
                    termination,
                    admission_error: None,
                    termination_started: false,
                    cleanup_error: Some(VmError::HostError(error.to_string())),
                },
            );
            return Err(error);
        }
        self.pending_stream_terminations.insert(
            op_id,
            PendingHostStreamTermination {
                driver,
                termination,
                admission_error: None,
                termination_started: true,
                cleanup_error: None,
            },
        );
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn retain_stream_admission_rollback(
        &mut self,
        rollback: HostStreamAdmissionRollback,
        primary: VmError,
    ) -> HostOpId {
        let op_id = self.next_host_op_id;
        self.next_host_op_id = self.next_host_op_id.wrapping_add(1).max(1);
        self.pending_stream_terminations.insert(
            op_id,
            PendingHostStreamTermination {
                driver: rollback.driver,
                termination: rollback.termination,
                admission_error: Some(primary),
                termination_started: false,
                cleanup_error: None,
            },
        );
        op_id
    }

    pub(crate) fn poll_stream_terminations(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<()>> {
        let ids: Vec<HostOpId> = self.pending_stream_terminations.keys().copied().collect();
        let has_admission_rollback = ids.iter().any(|op_id| {
            self.pending_stream_terminations
                .get(op_id)
                .is_some_and(|pending| pending.admission_error.is_some())
        });
        let mut completed = Vec::new();
        let mut first_error = None;
        let mut immediate_cleanup_error = false;
        for op_id in ids {
            let Some(pending) = self.pending_stream_terminations.get_mut(&op_id) else {
                continue;
            };
            if !pending.termination_started {
                match pending
                    .driver
                    .begin_termination(&mut self.execution_scope, pending.termination)
                {
                    Ok(()) => pending.termination_started = true,
                    Err(error) => {
                        if pending.cleanup_error.is_none() {
                            pending.cleanup_error = Some(error);
                        }
                        if let Some(primary) = pending.admission_error.as_ref() {
                            if first_error.is_none() {
                                first_error = Some(VmError::HostError(format!(
                                    "{primary}; cleanup failed: {}",
                                    pending
                                        .cleanup_error
                                        .as_ref()
                                        .expect("cleanup error recorded"),
                                )));
                            }
                            immediate_cleanup_error = true;
                        }
                        continue;
                    }
                }
            }
            match pending.driver.poll_termination(
                &mut self.execution_scope,
                pending.termination,
                cx,
            ) {
                Poll::Pending => {}
                Poll::Ready(Ok(())) => completed.push(op_id),
                Poll::Ready(Err(error)) => {
                    completed.push(op_id);
                    if pending.cleanup_error.is_none() {
                        pending.cleanup_error = Some(error);
                    }
                }
            }
        }
        for op_id in completed {
            if let Some(pending) = self.pending_stream_terminations.remove(&op_id) {
                let cleanup = pending.cleanup_error;
                let error = match (pending.admission_error, cleanup) {
                    (Some(primary), Some(cleanup)) => {
                        preserve_stream_cleanup(primary, Err(cleanup))
                    }
                    (Some(primary), None) => primary,
                    (None, Some(cleanup)) => cleanup,
                    (None, None) => continue,
                };
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        if immediate_cleanup_error {
            Poll::Ready(Err(first_error.expect("cleanup error recorded")))
        } else if has_admission_rollback && !self.pending_stream_terminations.is_empty() {
            Poll::Pending
        } else if let Some(error) = first_error {
            Poll::Ready(Err(error))
        } else if self.pending_stream_terminations.is_empty() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    pub(crate) fn has_pending_stream_terminations(&self) -> bool {
        !self.pending_stream_terminations.is_empty()
    }

    pub(crate) fn scope_reset_error(&self) -> Option<&ExecutionScopeError> {
        self.scope_reset_error.as_ref()
    }

    pub(crate) fn mark_reset_failed(&mut self, error: &VmError) {
        if self.scope_reset_error.is_none() && self.reset_error.is_none() {
            self.reset_error = Some(error.to_string());
        }
    }

    pub(crate) fn reset_error(&self) -> Option<VmError> {
        self.reset_error
            .as_ref()
            .map(|error| VmError::HostError(error.clone()))
    }

    pub(crate) fn is_reusable(&self) -> bool {
        !self.scope_reset_pending
            && self.scope_reset_error.is_none()
            && self.reset_error.is_none()
            && self.execution_scope.is_reusable()
            && self.bridge_operations.is_empty()
            && self.scoped_operation_completions.is_empty()
            && self.stream_drivers.is_empty()
            && self.pending_stream_terminations.is_empty()
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
