//! Host-agnostic execution-scope core state machine.
//!
//! [`ExecutionScope`] owns the [`ResourceTable`] and [`OperationRegistry`] of
//! one execution and drives the **Active → Closing → Quiescent** lifecycle
//! around them, without naming any concrete host domain (no host function, no
//! domain/resource-class enum, no sql/io/http/SSE/tokio/rusqlite, no
//! sqlite/io/http dispatch).
//!
//! # State machine
//!
//! - **Active** — resources and operations may be inserted through the generic
//!   scope API ([`ExecutionScope::push_resource`],
//!   [`ExecutionScope::push_child_resource`],
//!   [`ExecutionScope::start_operation`]).
//! - [`ExecutionScope::begin_close`] is idempotent and **first-reason-wins**:
//!   the first reason is bound deterministically; repeating it is a no-op and a
//!   conflicting reason is rejected ([`ExecutionScopeError::CloseAlreadyInProgress`]).
//!   It moves the scope to **Closing** and seals the operation registry, so any
//!   further insert is rejected with [`ExecutionScopeError::ScopeClosing`].
//! - [`ExecutionScope::poll_close`] drives the shutdown pipeline in order:
//!   1. *operations* — every pending operation is cancelled (driver
//!      [`HostOperation::cancel`](super::operation::HostOperation::cancel)) and
//!      drained to quiescence;
//!   2. *resources* — every resource closes child-first (leaves before their
//!      parents) through the table's caller-context poll close.
//! - Quiescence requires **both** the operation registry and the resource table
//!   to be empty. A genuinely `Pending` resource keeps
//!   [`ExecutionScope::poll_close`] returning [`Poll::Pending`]; a still-pending
//!   (or otherwise not-drained) operation likewise prevents quiescence.
//! - Cleanup is best-effort: a failing resource/operation close never stops the
//!   remaining closes. The **first** cleanup failure is preserved, the total
//!   failure count is accumulated, and the terminal state expresses both
//!   ([`ScopeCloseOutcome::SuccessWithErrors`]) instead of a fake success.
//! - Terminal state is reached at **Quiescent** and is idempotent: repeated
//!   [`ExecutionScope::begin_close`] / [`ExecutionScope::poll_close`] calls
//!   return the same result and never mutate state.
//!
//! A fresh [`ExecutionScope::new`] creates a brand-new resource arena and a
//! brand-new tagged operation registry, so handles and operation ids from one
//! execution are structurally rejected by any other scope (arena/generation and
//! registry-tag isolation): no domain registry, no global owner table, and no
//! host dispatch.
//!
//! The scope is `Send` (each layer is), but intentionally `!Sync`: it must be
//! owned and mutated by a single thread.

use std::task::{Context, Poll};

use crate::host_api::ResourceTypeKey;

use super::operation::driver::{OperationOutcome, OperationSpec};
use super::operation::error::{OperationError, OperationResult};
use super::operation::id::OperationId;
use super::operation::reason::OperationCancelReason;
use super::operation::registry::{DEFAULT_MAX_PENDING_OPERATIONS, OperationRegistry};
use super::resource::close::{CloseProgress, HostResource};
use super::resource::error::ResourceError;
use super::resource::handle::{Resource, ResourceHandle};
use super::resource::reason::ResourceCloseReason;
use super::resource::table::{
    GuestReleaseOutcome, OwnershipRelease, ResourceAccessFrame, ResourceAccessMode,
    ResourceAccessRequest, ResourceOwnership, ResourceTable,
};

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
    /// A request to replace this already-terminal scope was made before the
    /// scope actually reached quiescence. Cleanup must be driven to
    /// completion first; replacement is only legal from Quiescent.
    ScopeNotQuiescent,
    /// Construction of a fresh scope failed because the process-unique
    /// resource-arena identity space is exhausted. Carries the typed resource
    /// error ([`ResourceErrorCode::ResourceTableArenaExhausted`]); the scope
    /// was not created and no partial state exists.
    ArenaExhausted(ResourceError),
    /// The underlying resource insert failed (limit, invalid parent, …).
    Resource(ResourceError),
    /// The underlying operation start failed (limit, sealed, …).
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
            Self::ScopeNotQuiescent => write!(
                formatter,
                "execution scope replacement requires the current scope to be quiescent",
            ),
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
    /// A resource cleanup failed during child-first resource close.
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
    resources: ResourceTable,
    operations: OperationRegistry,
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

    /// Inserts a resource linked as a child of `parent` while the scope is
    /// Active, so the parent cannot close before its children.
    pub fn push_child_resource<T: HostResource, P: HostResource>(
        &mut self,
        value: T,
        parent: &Resource<P>,
    ) -> ExecutionScopeResult<Resource<T>> {
        self.ensure_accepting()?;
        self.resources
            .push_child(value, parent)
            .map_err(ExecutionScopeError::Resource)
    }

    /// Inserts a typed resource with an explicit exact catalog key while the
    /// scope is Active.
    pub fn push_resource_with_key<T: HostResource>(
        &mut self,
        value: T,
        key: ResourceTypeKey,
    ) -> ExecutionScopeResult<Resource<T>> {
        self.ensure_accepting()?;
        self.resources
            .push_with_key(value, key)
            .map_err(ExecutionScopeError::Resource)
    }

    /// Inserts a typed child with an explicit exact catalog key while the
    /// scope is Active.
    pub fn push_child_resource_with_key<T: HostResource, P: HostResource>(
        &mut self,
        value: T,
        parent: &Resource<P>,
        key: ResourceTypeKey,
    ) -> ExecutionScopeResult<Resource<T>> {
        self.ensure_accepting()?;
        self.resources
            .push_child_with_key(value, parent, key)
            .map_err(ExecutionScopeError::Resource)
    }

    /// Starts an exact resource access frame after operation association and
    /// table preflight. Consuming requests never bypass an active operation.
    pub fn begin_resource_access(
        &mut self,
        requests: Vec<ResourceAccessRequest>,
    ) -> ExecutionScopeResult<ResourceAccessFrame<'_>> {
        self.ensure_accepting()?;
        for request in &requests {
            if request.mode().is_consuming()
                && !self
                    .operations
                    .operations_for_resource(request.handle())
                    .is_empty()
            {
                return Err(ExecutionScopeError::Resource(ResourceError::new(
                    super::resource::error::ResourceErrorCode::ResourceOperationActive,
                    "resource::access",
                    "resource has an associated operation that is still active",
                )));
            }
        }
        self.resources
            .begin_resource_access(requests)
            .map_err(ExecutionScopeError::Resource)
    }

    /// Read-only, TypeId-free argument preflight for the exact manual host-call
    /// contract (C1/C2).
    ///
    /// Validates a raw handle + expected key against the borrow/take contract
    /// (arena, generation, slot key, not taken, open, and for `TakeOwned` also
    /// guest-owned, child-free, and free of any associated active operation).
    /// Rejections mutate nothing, so a bad argument never reaches the user
    /// host function.
    pub fn validate_exact_access(
        &self,
        handle: ResourceHandle,
        expected_key: &ResourceTypeKey,
        mode: ResourceAccessMode,
    ) -> ExecutionScopeResult<()> {
        if mode == ResourceAccessMode::TakeOwned
            && !self.operations.operations_for_resource(handle).is_empty()
        {
            return Err(ExecutionScopeError::Resource(ResourceError::new(
                super::resource::error::ResourceErrorCode::ResourceOperationActive,
                "resource::access",
                "resource has an associated operation that is still active",
            )));
        }
        self.resources
            .validate_access_keyed(handle, expected_key, mode)
            .map_err(ExecutionScopeError::Resource)
    }

    /// Marks an open, host-owned resource as guest-owned after verifying its
    /// live slot key equals `expected_key` (C4 exact-return ownership
    /// transfer). See [`ResourceTable::mark_guest_owned_with_key`] for the
    /// contract.
    pub fn mark_resource_guest_owned_with_key(
        &mut self,
        handle: ResourceHandle,
        expected_key: &ResourceTypeKey,
    ) -> ExecutionScopeResult<()> {
        self.resources
            .mark_guest_owned_with_key(handle, expected_key)
            .map_err(ExecutionScopeError::Resource)
    }

    /// Registers a host operation while the scope is Active.
    pub fn start_operation(&mut self, spec: OperationSpec) -> ExecutionScopeResult<OperationId> {
        self.ensure_accepting()?;
        self.operations
            .start(spec)
            .map_err(ExecutionScopeError::Operation)
    }

    /// Polls one registered operation to its terminal state using the
    /// caller's context.
    ///
    /// This is a narrowly reusable, host-agnostic adapter: the VM's pending
    /// host-call awaiting drives an execution-scope operation (e.g. one
    /// started by a generic host-SDK consumer via
    /// [`start_operation`](Self::start_operation)) through its
    /// [`HostOperation`] driver without any domain owner/poller dispatch.
    /// The returned [`OperationOutcome`] carries only lifecycle/status; the
    /// concrete operation value is delivered by the driver's own consumer.
    pub fn poll_operation(
        &mut self,
        id: OperationId,
        cx: &mut Context<'_>,
    ) -> Poll<OperationResult<OperationOutcome>> {
        self.operations.poll(id, cx)
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

    /// Aborts a started operation in one step so it never produces a
    /// guest-visible result: cancels the driver exactly once if pending
    /// (recording the first reason), then consumes and immediately releases
    /// the slot, restoring full registry capacity and making the id stale.
    ///
    /// This is the rollback counterpart to
    /// [`start_operation`](Self::start_operation), intended for call sites
    /// that register an operation and then hit a fallible handoff (such as a
    /// bridge submission) before a pending-result adapter is installed. Even
    /// when the driver's `cancel` reports a typed failure, the slot is still
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

    /// Cancels every pending operation associated with `handle`, then begins
    /// closing the underlying resource through the generic table contract.
    ///
    /// This is the generic "close one resource plus its dependent operations"
    /// adapter (host-agnostic): closing a resource first cancels the
    /// operations associated with it — mapping `reason` onto the parallel
    /// operation cancellation vocabulary — then launches the resource's close
    /// via [`HostResource::begin_close`]. A `Pending` close is driven by the
    /// usual scope [`poll_close`](Self::poll_close) machinery, so the caller
    /// never has to dispatch on a concrete resource class.
    ///
    /// Cancellation is best-effort: a failing operation cancel never prevents
    /// the resource close request from being launched.
    pub fn close_resource<T: HostResource>(
        &mut self,
        handle: ResourceHandle,
        reason: ResourceCloseReason,
    ) -> ExecutionScopeResult<CloseProgress> {
        let _ = self
            .operations
            .cancel_for_resource(handle, operation_reason(reason));
        let token = self
            .resources
            .typed::<T>(handle)
            .map_err(ExecutionScopeError::Resource)?;
        self.resources
            .begin_close(token, reason)
            .map_err(ExecutionScopeError::Resource)
    }

    /// Marks an open, host-owned resource as guest-owned (ownership transfer
    /// from the host to the guest script). This is the exact-host-return
    /// ownership transfer point: it succeeds only for a resource that is open,
    /// host-owned, and in *this* scope's table; every rejection is a
    /// structured, atomic `ResourceError` (no state mutated on failure).
    ///
    /// The scope is not required to be Active: a mark is a pure ownership
    /// bookkeeping transition on a live resource and must remain possible
    /// while the VM is executing (the scope stays Active during a run).
    pub fn mark_resource_guest_owned(
        &mut self,
        handle: ResourceHandle,
    ) -> ExecutionScopeResult<()> {
        self.resources
            .mark_guest_owned(handle)
            .map_err(ExecutionScopeError::Resource)
    }

    /// Releases the guest owner of one resource, launching its close exactly
    /// once with the release's reason.
    ///
    /// - `Ok(GuestReleaseOutcome::Released(progress))` — the resource was
    ///   guest-owned and open; `begin_close` fired exactly once with
    ///   `progress` (`Pending` means the close is now driven by the usual
    ///   scope poll machinery).
    /// - `Ok(GuestReleaseOutcome::NotGuestOwned)` — idempotent no-op (never
    ///   guest-owned, already released/closing, taken, stale, or foreign).
    /// - `Err(ResourceError)` — the close launch itself failed (e.g. live
    ///   children); the resource stays guest-owned and open, and the error is
    ///   returned so the caller can record it in the scope error latch.
    ///
    /// The scope is not required to be Active: a release is the guest-side
    /// teardown of a local's death and can occur while the VM is executing.
    pub fn release_guest_owner(
        &mut self,
        handle: ResourceHandle,
        release: OwnershipRelease,
    ) -> ExecutionScopeResult<GuestReleaseOutcome> {
        self.resources
            .release_guest_owner(handle, release)
            .map_err(ExecutionScopeError::Resource)
    }

    /// The current [`ResourceOwnership`] of the slot `handle` names, or
    /// `None` when the handle is foreign or stale (names no live slot here).
    pub fn resource_ownership(&self, handle: ResourceHandle) -> Option<ResourceOwnership> {
        self.resources.ownership(handle)
    }

    /// Atomically takes the concrete guest-owned resource out of the table,
    /// transferring ownership to the caller. See
    /// [`ResourceTable::take_owned`] for the exact validation contract.
    pub fn take_resource<T: HostResource>(
        &mut self,
        handle: ResourceHandle,
    ) -> ExecutionScopeResult<T> {
        let request = ResourceAccessRequest::take_owned::<T>(handle);
        let frame = self.begin_resource_access(vec![request])?;
        frame.take_owned(0).map_err(ExecutionScopeError::Resource)
    }

    /// Takes a guest-owned resource using an explicit catalog key through the
    /// same operation-aware preflight as the inferred-key path.
    pub fn take_resource_with_key<T: HostResource>(
        &mut self,
        handle: ResourceHandle,
        key: ResourceTypeKey,
    ) -> ExecutionScopeResult<T> {
        let request = ResourceAccessRequest::take_owned_with_key::<T>(handle, key);
        let frame = self.begin_resource_access(vec![request])?;
        frame
            .take_owned::<T>(0)
            .map_err(ExecutionScopeError::Resource)
    }

    /// Records a best-effort guest-release failure in the scope's first-error
    /// latch (first-error-wins, host-agnostic) and increments the failure
    /// count. Used by the VM when a local's ownership release hits a
    /// synchronous close error: the failure is preserved so the terminal
    /// scope outcome reports it, while the current execution continues
    /// without panicking.
    pub fn record_guest_release_error(&mut self, error: ResourceError) {
        if self.first_error.is_none() {
            self.first_error = Some(ScopeCloseError::Resource(error));
        }
        self.failed_count += 1;
    }

    /// The first cleanup failure recorded so far, if any. A close-failure
    /// latch does not require the scope to be closing: a guest release error
    /// can be recorded mid-run and is surfaced at the terminal outcome.
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

    /// Drives the closing scope to quiescence.
    ///
    /// Pipeline (in order):
    /// 1. *operations* (once): every pending operation is cancelled and drained;
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

        // Phase 1 — operations: cancel and drain every pending operation.
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
    /// increment (host-agnostic; used by operations, resources and guest
    /// releases).
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
        // A guest ownership release is an explicit caller-initiated close
        // request, so dependent operations see it as a requested cancel.
        ResourceCloseReason::OwnershipRelease => OperationCancelReason::Requested,
    }
}

#[cfg(test)]
mod tests {
    use super::{ExecutionScope, ExecutionScopeError};
    use crate::vm::operation::OperationErrorCode;
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
