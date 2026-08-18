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
//!   remaining closes. The **first** cleanup failure is preserved and the
//!   terminal state expresses it
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

use super::operation::driver::OperationSpec;
use super::operation::error::OperationError;
use super::operation::id::OperationId;
use super::operation::reason::OperationCancelReason;
use super::operation::registry::OperationRegistry;
use super::resource::close::HostResource;
use super::resource::error::ResourceError;
use super::resource::handle::Resource;
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
    /// The underlying resource insert failed (limit, invalid parent, …).
    Resource(ResourceError),
    /// The underlying operation start failed (limit, sealed, …).
    Operation(OperationError),
}

/// First cleanup failure preserved across the close sweep.
///
/// Best-effort shutdown continues past a failing entry; this carries the
/// earliest failure so the terminal state never claims a fake success.
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
    /// preserved, never overwritten by later successes or failures.
    SuccessWithErrors(ScopeCloseError),
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
    terminal: Option<ScopeCloseOutcome>,
}

impl ExecutionScope {
    /// Creates a fresh, independent execution scope.
    ///
    /// The resource table gets a brand-new process-unique arena identity and
    /// the operation registry a brand-new process-unique tag, so nothing in a
    /// new scope can alias handles/ids from any other scope.
    pub fn new() -> Self {
        Self {
            resources: ResourceTable::new(),
            operations: OperationRegistry::default(),
            state: ScopeState::Active,
            close_reason: None,
            operations_drained: false,
            first_error: None,
            terminal: None,
        }
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

    /// Registers a host operation while the scope is Active.
    pub fn start_operation(&mut self, spec: OperationSpec) -> ExecutionScopeResult<OperationId> {
        self.ensure_accepting()?;
        self.operations
            .start(spec)
            .map_err(ExecutionScopeError::Operation)
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
                self.first_error
                    .get_or_insert(ScopeCloseError::Operation(error.clone()));
            }
            self.operations_drained = true;
        }
        if !self.operations.is_empty() {
            // A still-registered operation (not yet drained) blocks quiescence.
            return Poll::Pending;
        }

        // Phase 2 — resources: child-first, best-effort, caller-context close.
        match self.resources.poll_close_all(reason, cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(_closed)) => {
                self.finish_close();
                Poll::Ready(Ok(self
                    .terminal
                    .clone()
                    .expect("finish_close set terminal")))
            }
            Poll::Ready(Err(error)) => {
                self.first_error
                    .get_or_insert(ScopeCloseError::Resource(error));
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

    /// Freezes the terminal outcome once both registries are empty.
    fn finish_close(&mut self) {
        debug_assert!(self.operations.is_empty(), "operations must be drained");
        debug_assert!(self.resources.is_empty(), "resources must be closed");
        self.state = ScopeState::Quiescent;
        self.terminal = Some(match self.first_error.take() {
            Some(first) => ScopeCloseOutcome::SuccessWithErrors(first),
            None => ScopeCloseOutcome::Success,
        });
    }
}

impl Default for ExecutionScope {
    fn default() -> Self {
        Self::new()
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
    }
}
