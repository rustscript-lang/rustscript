//! Object-safe operation driver contract.
//!
//! This module defines the [`HostOperation`] driver contract that the
//! operation registry drives. Each pending operation owns its poll and
//! cancel behaviour; the registry performs no owner/poller dispatch. This is
//! the host-agnostic replacement for the old `OperationOwner` +
//! `RUNTIME_OPERATION_POLLERS` static table.
//!
//! Cancellation has a single authority: the operation's *owner* (or the
//! scope that owns the operation, integrated later). Drivers implement the
//! concrete [`HostOperation::cancel`] action; the registry records the first
//! [`CancellationReason`] and the terminal status but does not build a
//! parent/child [`CancellationToken`] signal graph.

use std::task::{Context, Poll};

use super::error::OperationError;
use super::reason::OperationCancelReason;
use crate::vm::CancellationReason;
use crate::vm::resource::ResourceHandle;

/// Opaque terminal result reported by an operation once it finishes.
///
/// A driver returns this from [`HostOperation::poll`]. The registry stores it
/// as the operation's terminal result and exposes it through
/// [`crate::vm::operation::OperationRegistry::outcome`]. The actual host
/// *value* the operation produced is delivered by the driver to its own
/// consumer (e.g. a captured completion callback); the operation layer tracks
/// lifecycle and status, not the concrete produced byte stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationOutcome {
    /// Operation finished successfully.
    Completed,
    /// Operation failed with a runtime error.
    Failed(OperationError),
    /// Operation was cancelled; carries the first recorded cancellation
    /// reason.
    Cancelled(OperationCancelReason),
}

/// Object-safe driver contract for a single in-flight host operation.
///
/// Implementors must be `Send` (the operation may be owned by a host that
/// runs work on another thread) and not borrow from the VM across a poll.
/// Polling advances the operation; cancellation is delivered in-band through
/// [`HostOperation::cancel`].
pub trait HostOperation: Send + 'static {
    /// Drive the operation one step.
    ///
    /// Return `Poll::Pending` while the operation is still running, or
    /// `Poll::Ready(Ok(()))` / `Poll::Ready(Err(error))` once it reaches a
    /// terminal state. Implementors must be cancellation-aware: after
    /// [`HostOperation::cancel`] has been observed they should return
    /// `Poll::Ready` promptly so the registry can record the terminal status.
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<OperationResult<()>>;

    /// Ask the driver to stop the underlying work.
    ///
    /// Must be idempotent: it is invoked at most once per operation
    /// (later calls on an already-cancelled operation are suppressed by the
    /// registry). The reason is typed for diagnostics and for the driver to
    /// distinguish scope reset, deadline and explicit requests. This is the
    /// single cancellation authority; drivers must not build their own
    /// parent/child token trees.
    fn cancel(&mut self, reason: OperationCancelReason) -> OperationResult<()>;
}

/// Optional per-operation cleanup, called exactly once on the first terminal
/// transition. Failures are isolated by the registry: the operation still
/// becomes terminal and any batch cancellation continues past a failing
/// cleanup.
pub type OperationCleanup =
    Box<dyn FnOnce(&OperationOutcome) -> OperationResult<()> + Send + 'static>;

/// Configuration describing one operation for
/// [`OperationRegistry::start`](crate::vm::operation::OperationRegistry::start).
pub struct OperationSpec {
    /// Optional absolute deadline. If a deadline elapses while the operation
    /// is still pending, the registry cancels it with
    /// [`CancellationReason::Deadline`] (unless it was already cancelled with
    /// an earlier reason).
    pub deadline: Option<std::time::Instant>,
    /// Optional associated resource handle. Cancelling/`closing` that exact
    /// resource also cancels this operation (fan-out integrated later at the
    /// scope level).
    pub resource: Option<ResourceHandle>,
    /// The driver that owns poll/cancel behaviour.
    pub driver: Box<dyn HostOperation>,
    /// Optional cleanup run once on the first terminal transition.
    pub cleanup: Option<OperationCleanup>,
}

impl OperationSpec {
    /// Builds a spec from a driver, leaving deadline/resource/cleanup unset.
    pub fn new(driver: impl HostOperation + 'static) -> Self {
        Self {
            deadline: None,
            resource: None,
            driver: Box::new(driver),
            cleanup: None,
        }
    }

    /// Sets an optional deadline for the operation.
    pub fn with_deadline(mut self, deadline: std::time::Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Associates the operation with a resource so closing the resource
    /// cancels the operation.
    pub fn with_resource(mut self, resource: ResourceHandle) -> Self {
        self.resource = Some(resource);
        self
    }

    /// Attaches a cleanup hook.
    pub fn with_cleanup(mut self, cleanup: OperationCleanup) -> Self {
        self.cleanup = Some(cleanup);
        self
    }
}
