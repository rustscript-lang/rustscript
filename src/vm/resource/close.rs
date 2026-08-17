//! Poll-based close contract for host resources.
//!
//! Concrete resource types implement [`HostResource`] to own their cancellation
//! and teardown. The core table never dispatches on a concrete class; it only
//! records opaque cleanup errors and drives the two-phase close below.

use std::any::Any;
use std::task::{Context, Poll};

use crate::builtins::runtime::cancellation::CancellationReason;
use crate::builtins::runtime::error::RuntimeResult;

/// Outcome of synchronously beginning a close.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseProgress {
    /// The resource finished closing synchronously; no further polling needed.
    Ready,
    /// The resource is now closing asynchronously; call [`poll_close`](HostResource::poll_close).
    Pending,
}

/// Object-safe resource owned (erased) by a [`ResourceTable`](super::table::ResourceTable).
///
/// Concrete resources are never enumerated by the core. They implement this
/// trait and the core invokes the begin/poll close state machine generically.
///
/// Contract:
/// - [`begin_close`](HostResource::begin_close) must be idempotent and must
///   synchronously issue any cancel/close request.
/// - [`poll_close`](HostResource::poll_close) is called only after
///   `begin_close` returns [`CloseProgress::Pending`].
/// - A concrete `Drop` remains the last-resort guard, but the VM may only reuse
///   a resource and its slot once `poll_close` completes.
///
/// The `Any` supertrait lets the table reconnect each erased value to its
/// concrete `TypeId` without ever naming a concrete class.
pub trait HostResource: Any + Send + 'static {
    /// Begins closing the resource, emitting a synchronous cancel/close request.
    ///
    /// The default is a synchronous no-op close.
    fn begin_close(&mut self, reason: CancellationReason) -> RuntimeResult<CloseProgress> {
        let _ = reason;
        Ok(CloseProgress::Ready)
    }

    /// Polls an in-progress close to completion.
    ///
    /// Only invoked after `begin_close` returned [`CloseProgress::Pending`].
    /// The default completes synchronously. An `Err` is a cleanup failure
    /// recorded by the table as a generic close error.
    fn poll_close(&mut self, _cx: &mut Context<'_>) -> Poll<RuntimeResult<()>> {
        Poll::Ready(Ok(()))
    }
}
