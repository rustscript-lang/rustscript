//! Host-agnostic execution scope: one resource registry plus one operation
//! registry with a single Active → Closing → Quiescent lifecycle.
//!
//! An [`ExecutionScope`] is the isolated ownership unit the VM exposes to
//! host code: it owns exactly one [`ResourceTable`] and exactly one
//! [`OperationRegistry`], so nothing in one scope can alias handles or
//! operation ids from another. New inserts are guarded by the scope state;
//! shutdown cancels and drains operations before closing resources, and the
//! terminal outcome is fixed once (idempotent) when both registries empty.
//!
//! The scope stays host-agnostic: it never dispatches on a concrete resource
//! class or a host operation domain. Concrete drivers own poll/cancel (see
//! [`HostOperation`](crate::vm::operation::HostOperation)) and concrete
//! resources own their close (see
//! [`HostResource`](crate::vm::resource::HostResource)).

use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use super::operation::driver::{OperationOutcome, OperationSpec};
use super::operation::error::OperationError;
use super::operation::id::OperationId;
use super::operation::reason::OperationCancelReason;
use super::operation::registry::{DEFAULT_MAX_PENDING_OPERATIONS, OperationRegistry};
use super::resource::HostResource;
use super::resource::close::CloseProgress;
use super::resource::error::ResourceError;
use super::resource::handle::{Resource, ResourceHandle};
use super::resource::reason::ResourceCloseReason;
use super::resource::table::ResourceTable;

/// Result alias used by the execution-scope surface.
pub type ExecutionScopeResult<T> = Result<T, ExecutionScopeError>;

/// Lifecycle phase of one execution scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeState {
    /// The scope accepts new resources and operations through the generic API.
    Active,
    /// Shutdown has begun: new inserts are rejected and [`ExecutionScope::poll_close`]
    /// drives operations then resources to quiescence.
    Closing,
    /// Both the resource table and the operation registry are empty and the
    /// terminal outcome is fixed (idempotent).
    Quiescent,
}

/// Structured error returned on a scope-state violation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionScopeError {
    /// A close was already begun with a different reason (first-reason-wins).
    ///
    /// `current` is the already-bound reason, `requested` the rejected one.
    CloseAlreadyInProgress {
        current: Option<ResourceCloseReason>,
        requested: ResourceCloseReason,
    },
    /// A new resource/operation insert was rejected because the scope is
    /// Closing or Quiescent.
    ScopeClosing,
    /// A close/poll was requested while the scope was still Active.
    ScopeNotClosing,
    /// Construction of a fresh scope failed because the process-unique
    /// resource-arena identity space is exhausted. Carries the typed resource
    /// error ([`ResourceErrorCode::ResourceTableArenaExhausted`]); the scope
    /// was not created and no partial state exists.
    ArenaExhausted(ResourceError),
    /// The underlying resource insert/close failed.
    Resource(ResourceError),
    /// The underlying operation start/cancel failed.
    Operation(OperationError),
}

impl std::fmt::Display for ExecutionScopeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CloseAlreadyInProgress { current, requested } => write!(
                formatter,
                "execution scope close already in progress with {current:?}; conflicting {requested:?} rejected",
            ),
            Self::ScopeClosing => {
                write!(
                    formatter,
                    "execution scope is closing and rejects new inserts"
                )
            }
            Self::ScopeNotClosing => {
                write!(
                    formatter,
                    "execution scope close was requested on an active scope"
                )
            }
            Self::ArenaExhausted(error) => {
                write!(formatter, "execution scope creation failed: {error}")
            }
            Self::Resource(error) => write!(formatter, "execution scope resource error: {error}"),
            Self::Operation(error) => {
                write!(formatter, "execution scope operation error: {error}")
            }
        }
    }
}

impl ExecutionScopeError {
    /// Recovers the underlying `OperationError` when the failure is an
    /// operation-domain error; returns `None` for scope-state violations.
    pub fn into_operation_error(self) -> Option<OperationError> {
        match self {
            ExecutionScopeError::Operation(error) => Some(error),
            _ => None,
        }
    }

    /// Recovers the underlying `ResourceError` when the failure is a
    /// resource-domain error; returns `None` for scope-state violations.
    pub fn into_resource_error(self) -> Option<ResourceError> {
        match self {
            ExecutionScopeError::Resource(error) | ExecutionScopeError::ArenaExhausted(error) => {
                Some(error)
            }
            _ => None,
        }
    }
}

impl std::error::Error for ExecutionScopeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ArenaExhausted(error) | Self::Resource(error) => Some(error),
            Self::Operation(error) => Some(error),
            _ => None,
        }
    }
}

/// First cleanup failure preserved across the close sweep, plus the total
/// number of failed cleanups observed.
///
/// Best-effort shutdown continues past a failing entry; this carries the
/// earliest failure so the terminal state never claims a fake success, and
/// the failure count so the caller can size the blast radius.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeCloseFailure {
    /// The earliest cleanup failure (first-error-wins).
    pub first: ScopeCloseError,
    /// Total number of cleanup failures observed during the sweep
    /// (operations then resources), including `first`.
    pub failed: usize,
}

/// One typed cleanup failure in the scope close sweep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeCloseError {
    /// An operation driver/cleanup failed during the operation drain.
    Operation(OperationError),
    /// A resource cleanup failed during resource close.
    Resource(ResourceError),
}

/// Terminal result of a fully-driven scope shutdown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeCloseOutcome {
    /// Every operation drained and every resource closed cleanly.
    Success,
    /// The scope quiesced but at least one cleanup failed; the first error is
    /// preserved, never overwritten by later successes or failures, and the
    /// total failure count is carried alongside it.
    SuccessWithErrors(ScopeCloseFailure),
}

/// One execution scope: an isolated resource arena plus an isolated operation
/// registry, with an Active → Closing → Quiescent lifecycle.
///
/// `Send + !Sync`: the scope owns its registries and must be driven by a
/// single thread.
pub struct ExecutionScope {
    operations: OperationRegistry,
    resources: ResourceTable,
    state: ScopeState,
    close_reason: Option<ResourceCloseReason>,
    /// Whether the operation phase of this close already ran (idempotent).
    operations_drained: bool,
    /// First cleanup failure across the whole shutdown (operations then resources).
    first_error: Option<ScopeCloseError>,
    /// Total cleanup failures observed across the whole shutdown (operations
    /// then resources); includes the failure recorded in `first_error`.
    failed_count: usize,
    terminal: Option<ScopeCloseOutcome>,
}

impl ExecutionScope {
    /// Creates a fresh, independent execution scope.
    ///
    /// The resource table gets a brand-new process-unique arena identity and
    /// the operation registry a brand-new process-unique tag, so nothing in a
    /// new scope can alias handles/ids from any other scope.
    ///
    /// Fallible: arena identity or operation-registry tag allocation can fail
    /// with [`ExecutionScopeError::ArenaExhausted`] or
    /// [`ExecutionScopeError::Operation`] once the process-unique identity
    /// space is exhausted. No partial scope is created on failure.
    pub fn new() -> ExecutionScopeResult<Self> {
        let resources = ResourceTable::new().map_err(ExecutionScopeError::ArenaExhausted)?;
        Ok(Self {
            resources,
            operations: OperationRegistry::with_limit(DEFAULT_MAX_PENDING_OPERATIONS)
                .map_err(ExecutionScopeError::Operation)?,
            state: ScopeState::Active,
            close_reason: None,
            operations_drained: false,
            first_error: None,
            failed_count: 0,
            terminal: None,
        })
    }

    /// The current lifecycle phase.
    pub fn state(&self) -> ScopeState {
        self.state
    }

    /// Whether the scope is still accepting new resources/operations.
    pub fn is_active(&self) -> bool {
        self.state == ScopeState::Active
    }

    /// Whether shutdown has begun but is not yet quiescent.
    pub fn is_closing(&self) -> bool {
        self.state == ScopeState::Closing
    }

    /// Whether both registries are empty and the terminal outcome is fixed.
    pub fn is_quiescent(&self) -> bool {
        self.state == ScopeState::Quiescent
    }

    /// The first-close reason bound by [`begin_close`](Self::begin_close), if any.
    pub fn close_reason(&self) -> Option<ResourceCloseReason> {
        self.close_reason
    }

    /// Read access to the owned resource table (observe counts, borrow, type
    /// validation). New inserts must go through the guarded scope API.
    pub fn resources(&self) -> &ResourceTable {
        &self.resources
    }

    // ---- typed scope-state arena -------------------------------------------------

    /// Returns a mutable handle to the `T`-typed scope state, creating it with
    /// `init` on first access while the scope is Active.
    ///
    /// A Closing/Quiescent scope rejects the insert with
    /// [`ExecutionScopeError::ScopeClosing`] (the existing admission guard).
    /// The state lives in the arena-owned map on the underlying
    /// [`ResourceTable`], separate from ordinary resource slots.
    pub fn scope_state_or_insert_with<T: Send + 'static, F: FnOnce() -> T>(
        &mut self,
        init: F,
    ) -> ExecutionScopeResult<&mut T> {
        self.ensure_accepting()?;
        Ok(self.resources.scope_state_or_insert_with(init))
    }

    /// Borrows the `T`-typed scope state, if present.
    ///
    /// Returns `None` after the terminal close cleared the arena (and for a
    /// type that was never inserted).
    pub fn scope_state<T: Send + 'static>(&self) -> Option<&T> {
        self.resources.scope_state::<T>()
    }

    /// Mutably borrows the `T`-typed scope state, if present.
    ///
    /// Returns `None` after the terminal close cleared the arena (and for a
    /// type that was never inserted).
    pub fn scope_state_mut<T: Send + 'static>(&mut self) -> Option<&mut T> {
        self.resources.scope_state_mut::<T>()
    }

    /// Removes and returns the `T`-typed scope state, if present.
    pub fn take_scope_state<T: Send + 'static>(&mut self) -> Option<T> {
        self.resources.take_scope_state::<T>()
    }

    /// Read access to the owned operation registry (observe counts/status).
    /// New starts must go through the guarded scope API.
    pub fn operations(&self) -> &OperationRegistry {
        &self.operations
    }

    /// The fixed terminal outcome, once the scope reached quiescence.
    pub fn terminal(&self) -> Option<&ScopeCloseOutcome> {
        self.terminal.as_ref()
    }

    /// Inserts a root resource while the scope is Active.
    ///
    /// A Closing/Quiescent scope rejects the insert with
    /// [`ExecutionScopeError::ScopeClosing`].
    pub fn push_resource<T: HostResource>(
        &mut self,
        value: T,
    ) -> ExecutionScopeResult<Resource<T>> {
        self.ensure_accepting()?;
        self.resources
            .push(value)
            .map_err(ExecutionScopeError::Resource)
    }

    /// Registers a host operation while the scope is Active.
    pub fn start_operation(&mut self, spec: OperationSpec) -> ExecutionScopeResult<OperationId> {
        self.ensure_accepting()?;
        self.operations
            .start(spec)
            .map_err(ExecutionScopeError::Operation)
    }

    /// Cancels one registered operation by id, forwarding the reason to its
    /// driver. Generic and host-agnostic; returns `false` when the operation
    /// was already terminal.
    pub fn cancel_operation(
        &mut self,
        id: OperationId,
        reason: OperationCancelReason,
    ) -> ExecutionScopeResult<bool> {
        self.operations
            .cancel(id, reason)
            .map_err(ExecutionScopeError::Operation)
    }

    /// Marks an operation completed without polling. The terminal slot remains
    /// occupied until [`take_operation_outcome`](Self::take_operation_outcome).
    pub fn complete_operation(&mut self, id: OperationId) -> ExecutionScopeResult<bool> {
        self.operations
            .complete(id)
            .map_err(ExecutionScopeError::Operation)
    }

    /// Consumes one terminal outcome and releases its slot for generation reuse.
    pub fn take_operation_outcome(
        &mut self,
        id: OperationId,
    ) -> ExecutionScopeResult<OperationOutcome> {
        self.operations
            .take_outcome(id)
            .map_err(ExecutionScopeError::Operation)
    }

    /// Drives one operation to terminal, polling its concrete driver.
    ///
    /// Forwarding the operation registry's [`poll`](OperationRegistry::poll)
    /// through the scope keeps the concrete driver's `poll`/cancel running in
    /// the operation layer while the scope remains the single ownership unit.
    pub fn poll_operation(
        &mut self,
        id: OperationId,
        cx: &mut Context<'_>,
    ) -> Poll<ExecutionScopeResult<OperationOutcome>> {
        match self.operations.poll(id, cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => Poll::Ready(result.map_err(ExecutionScopeError::Operation)),
        }
    }

    /// Aborts a started operation in one step so it never produces a
    /// guest-visible result: cancels the driver exactly once if pending
    /// (recording the first reason), then consumes and immediately releases
    /// the slot, restoring full registry capacity and making the id stale.
    ///
    /// This is the rollback counterpart to
    /// [`start_operation`](Self::start_operation), intended for call sites
    /// that register an operation and then hit a fallible handoff. Even when
    /// the driver's `cancel` reports a typed failure, the slot is still
    /// released. A stale/foreign/out-of-range id is rejected with the typed
    /// error and no registry mutation.
    pub fn abort_operation(
        &mut self,
        id: OperationId,
        reason: OperationCancelReason,
    ) -> ExecutionScopeResult<bool> {
        self.operations
            .abort(id, reason)
            .map_err(ExecutionScopeError::Operation)
    }

    /// Begins closing the resource through the generic table contract.
    ///
    /// This is the generic "close one resource" adapter (host-agnostic): the
    /// resource arena/type/generation/live checks and `begin_close` happen
    /// before any state mutation, so a rejected close leaves the table
    /// untouched. A `Pending` close is driven by the usual scope
    /// [`poll_close`](Self::poll_close) machinery, so the caller never has to
    /// dispatch on a concrete resource class.
    pub fn close_resource<T: HostResource>(
        &mut self,
        handle: ResourceHandle,
        reason: ResourceCloseReason,
    ) -> ExecutionScopeResult<CloseProgress> {
        let token = self
            .resources
            .typed::<T>(handle)
            .map_err(ExecutionScopeError::Resource)?;
        self.resources
            .begin_close(token, reason)
            .map_err(ExecutionScopeError::Resource)
    }

    /// The first cleanup failure recorded so far, if any.
    pub fn first_error(&self) -> Option<&ScopeCloseError> {
        self.first_error.as_ref()
    }

    /// Total cleanup failures recorded so far across the whole shutdown
    /// (operations then resources), including the one in
    /// [`first_error`](Self::first_error).
    pub fn failed_count(&self) -> usize {
        self.failed_count
    }

    /// Begins scope shutdown: **Active → Closing**, sealing new inserts.
    ///
    /// Idempotent and first-reason-wins:
    /// - `Ok(true)` on the first transition;
    /// - `Ok(false)` on a repeat with the already-bound reason;
    /// - `Err([`ExecutionScopeError::CloseAlreadyInProgress`])` on a conflicting
    ///   reason (the first reason is preserved).
    pub fn begin_close(&mut self, reason: ResourceCloseReason) -> ExecutionScopeResult<bool> {
        match self.state {
            ScopeState::Active => {
                self.state = ScopeState::Closing;
                self.close_reason = Some(reason);
                // Operationally seal the registry so no operation can start after
                // this point, in addition to the scope-level guard.
                self.operations.seal();
                Ok(true)
            }
            ScopeState::Closing | ScopeState::Quiescent => {
                if self.close_reason == Some(reason) {
                    Ok(false)
                } else {
                    Err(ExecutionScopeError::CloseAlreadyInProgress {
                        current: self.close_reason,
                        requested: reason,
                    })
                }
            }
        }
    }

    /// Runs the VM-Drop-only nonblocking resource close launch after the normal
    /// scope close poll has cancelled operations and begun all current leaves.
    /// This never changes the scope state or claims quiescence.
    pub(crate) fn begin_drop_resource_close_nonblocking(&mut self) -> ExecutionScopeResult<()> {
        debug_assert_eq!(self.state, ScopeState::Closing);
        let reason = self.close_reason.unwrap_or(ResourceCloseReason::VmDrop);
        self.resources
            .begin_close_remaining_for_drop(reason)
            .map_err(ExecutionScopeError::Resource)
    }

    /// Drives the closing scope to quiescence.
    ///
    /// Pipeline (in order):
    /// 1. *operations* (once): every pending operation is cancelled;
    /// 2. *resources*: every resource closes child-first via the table's
    ///    caller-context poll close.
    ///
    /// Returns [`Poll::Pending`] while any operation or resource is still
    /// pending (quiescence is blocked), and [`Poll::Ready`] with the fixed
    /// terminal outcome exactly once both registries are empty. Once quiescent,
    /// repeated polls return the same terminal outcome (idempotent).
    ///
    /// An Active scope (no close requested) returns
    /// [`ExecutionScopeError::ScopeNotClosing`].
    pub fn poll_close(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<ExecutionScopeResult<ScopeCloseOutcome>> {
        match self.state {
            ScopeState::Active => {
                return Poll::Ready(Err(ExecutionScopeError::ScopeNotClosing));
            }
            ScopeState::Quiescent => {
                return Poll::Ready(Ok(self.terminal.clone().expect("quiescent has terminal")));
            }
            ScopeState::Closing => {}
        }

        let reason = self.close_reason.expect("closing scope has a bound reason");

        // Phase 1 — operations: cancel every pending operation exactly once.
        if !self.operations_drained {
            let summary = self.operations.cancel_all(operation_reason(reason));
            if let Some(error) = summary.first_error() {
                self.record_failure(ScopeCloseError::Operation(error.clone()));
            }
            // Every failed operation cancellation/cleanup counts toward the
            // failure total; `failed` includes the first-error case above.
            self.failed_count += summary
                .failed()
                .saturating_sub(usize::from(summary.first_error().is_some()));
            self.operations_drained = true;
        }

        // A cancelled worker may keep its terminal slot until its driver is
        // polled to quiescence; keep the scope Closing and let the worker's
        // completion waker drive the next poll.
        if !self.operations.poll_quiescence(cx) {
            return Poll::Pending;
        }
        if !self.operations.is_empty() {
            // A still-registered operation (not yet drained) blocks quiescence.
            return Poll::Pending;
        }

        // Phase 2 — resources: child-first, best-effort, caller-context close.
        match self.resources.poll_close_all_report(reason, cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(report)) => {
                if let Some(error) = report.first_error.clone() {
                    self.record_failure(ScopeCloseError::Resource(error));
                }
                // The resource sweep's failure count already includes the
                // first error (recorded above); only the remainder is new.
                self.failed_count += report
                    .failed
                    .saturating_sub(usize::from(report.first_error.is_some()));
                self.finish_close();
                Poll::Ready(Ok(self
                    .terminal
                    .clone()
                    .expect("finish_close set terminal")))
            }
            Poll::Ready(Err(error)) => {
                self.record_failure(ScopeCloseError::Resource(error));
                self.finish_close();
                Poll::Ready(Ok(self
                    .terminal
                    .clone()
                    .expect("finish_close set terminal")))
            }
        }
    }

    /// Guard applied before any new resource/operation insert.
    fn ensure_accepting(&self) -> ExecutionScopeResult<()> {
        if self.state == ScopeState::Active {
            Ok(())
        } else {
            Err(ExecutionScopeError::ScopeClosing)
        }
    }

    /// Records a cleanup failure: first-error-wins plus a failure-count
    /// increment (host-agnostic; used by operations and resources).
    fn record_failure(&mut self, error: ScopeCloseError) {
        if self.first_error.is_none() {
            self.first_error = Some(error);
        }
        self.failed_count += 1;
    }

    /// Freezes the terminal outcome once both registries are empty.
    fn finish_close(&mut self) {
        debug_assert!(self.operations.is_empty(), "operations must be drained");
        debug_assert!(self.resources.is_empty(), "resources must be closed");
        self.state = ScopeState::Quiescent;
        self.terminal = Some(match self.first_error.take() {
            Some(first) => ScopeCloseOutcome::SuccessWithErrors(ScopeCloseFailure {
                first,
                failed: self.failed_count,
            }),
            None => ScopeCloseOutcome::Success,
        });
    }
}

struct ScopeDropWake;

impl Wake for ScopeDropWake {
    fn wake(self: Arc<Self>) {}
}

impl Drop for ExecutionScope {
    fn drop(&mut self) {
        if self.state == ScopeState::Active {
            self.state = ScopeState::Closing;
            self.close_reason = Some(ResourceCloseReason::VmDrop);
            self.operations.seal();
        }
        if self.state != ScopeState::Closing {
            return;
        }
        let waker = Waker::from(Arc::new(ScopeDropWake));
        let mut cx = Context::from_waker(&waker);
        let _ = self.poll_close(&mut cx);
        if self.state == ScopeState::Closing {
            // A standalone scope drop cannot keep polling a Pending resource,
            // but it must still launch every remaining ancestor close with the
            // VmDrop reason before ResourceTable itself is dropped.
            let _ = self.begin_drop_resource_close_nonblocking();
        }
    }
}

/// Maps the generic resource-layer close reason onto the parallel generic
/// operation-layer cancellation reason. Both vocabularies are stable and
/// 1:1; the scope stays host-agnostic.
fn operation_reason(reason: ResourceCloseReason) -> OperationCancelReason {
    match reason {
        ResourceCloseReason::Requested => OperationCancelReason::Requested,
        ResourceCloseReason::Deadline => OperationCancelReason::Deadline,
        ResourceCloseReason::VmReset => OperationCancelReason::VmReset,
        ResourceCloseReason::Parent => OperationCancelReason::Parent,
        ResourceCloseReason::ResourceClosed => OperationCancelReason::ResourceClosed,
        ResourceCloseReason::VmDrop => OperationCancelReason::VmDrop,
    }
}

#[cfg(test)]
mod tests {
    use super::{ExecutionScope, ExecutionScopeError};
    use crate::vm::operation::error::OperationErrorCode;
    use crate::vm::operation::id::MAX_REGISTRY_TAG;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn construction_propagates_operation_registry_tag_exhaustion() {
        static COUNTER: AtomicU64 = AtomicU64::new(MAX_REGISTRY_TAG + 1);
        let _source =
            crate::vm::operation::id::test_seam::ScopedRegistryTagSource::install(&COUNTER);

        let error = match ExecutionScope::new() {
            Ok(_) => panic!("operation registry tag exhaustion must be fallible"),
            Err(error) => error,
        };
        let ExecutionScopeError::Operation(error) = error else {
            panic!("expected the operation exhaustion variant");
        };
        assert_eq!(
            error.code(),
            OperationErrorCode::OperationRegistryTagExhausted
        );
        assert_eq!(error.limit(), Some(MAX_REGISTRY_TAG));
        assert_eq!(error.value(), Some(MAX_REGISTRY_TAG + 1));
    }
}
