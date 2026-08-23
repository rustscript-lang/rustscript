//! [`IoWorkerResource`] — a cooperative worker-thread resource.
//!
//! Worker threads are used by the blocking and async IO paths to run
//! synchronous file/process operations without blocking the VM. The worker
//! owns a [`SharedWorkerState`] and a [`JoinHandle`]:
//!
//! - [`begin_close`](HostResource::begin_close) sets the cancellation flag
//!   so the worker's loop can observe it.
//! - [`poll_close`](HostResource::poll_close) checks
//!   [`JoinHandle::is_finished`] and joins only when the thread has
//!   terminated, reporting any panic.
//!
//! A worker is registered as a [`HostResource`] in the execution scope so
//! the generic close lifecycle handles it. It may also be associated as a
//! child of a process resource (e.g. when a popen read is offloaded to a
//! worker).
//!
//! ## Drop invariant
//!
//! [`IoWorkerResource::Drop`] asserts that the thread has already finished
//! (via [`JoinHandle::is_finished`]) and joins to observe panics. A live
//! worker must never reach Drop — the close lifecycle
//! ([`begin_close`](HostResource::begin_close) /
//! [`poll_close`](HostResource::poll_close)) or the
//! [`ThreadedOperation`] completion path must retire the worker before
//! Drop. If a catastrophic Drop fallback must detach a live thread, the
//! cancellation flag is set first and no OS handles are retained by the
//! detached thread (it checks cancellation promptly).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};
use std::thread::JoinHandle;

use crate::host_api::ResourceTypeKey;
use crate::vm::resource::{CloseProgress, HostResource, ResourceCloseReason, ResourceResult};

use super::ops::SharedWorkerState;

/// Stable catalog identity for an IO worker-thread resource.
pub(crate) fn io_worker_key() -> ResourceTypeKey {
    ResourceTypeKey::new("io.worker").expect("io.worker resource type key must be valid")
}

/// A worker thread resource.
///
/// The worker owns a shared state (cancellation flag, result, terminal error)
/// The worker thread checks the shared cancellation flag periodically and exits.
///
/// Both [`IoWorkerResource`] and the corresponding
/// [`ThreadedOperation`](super::ops::ThreadedOperation) share the same
/// [`SharedWorkerState`] reference. The former manages the thread lifecycle
/// (close/drop); the latter drives the VM operation lifecycle.
pub(crate) struct IoWorkerResource {
    state: Arc<SharedWorkerState>,
    handle: Option<JoinHandle<()>>,
    name: String,
}

impl IoWorkerResource {
    /// Create a new worker resource from a shared state and join handle.
    ///
    /// `state` must be the same `Arc<SharedWorkerState>` that the worker
    /// thread and the `ThreadedOperation` use; `handle` is the join handle.
    pub(crate) fn new(
        name: impl Into<String>,
        state: Arc<SharedWorkerState>,
        handle: JoinHandle<()>,
    ) -> Self {
        Self {
            state,
            handle: Some(handle),
            name: name.into(),
        }
    }
}

impl HostResource for IoWorkerResource {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(io_worker_key())
    }

    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        // Signal cancellation so the worker thread can observe it.
        self.state.cancelled.store(true, Ordering::SeqCst);
        Ok(CloseProgress::Pending)
    }

    fn poll_close(&mut self, _cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        if let Some(handle) = self.handle.as_ref() {
            if !handle.is_finished() {
                return Poll::Pending;
            }
        }
        // Thread has finished; join to observe panics.
        if let Some(handle) = self.handle.take() {
            handle.join().map_err(|_| {
                crate::vm::resource::ResourceError::new(
                    crate::vm::resource::ResourceErrorCode::ResourceCleanupFailed,
                    "io.worker",
                    format!("worker thread '{}' panicked", self.name),
                )
            })?;
        }
        Poll::Ready(Ok(()))
    }
}

impl Drop for IoWorkerResource {
    fn drop(&mut self) {
        self.state.cancelled.store(true, Ordering::SeqCst);
        // Invariant: the thread must already be finished by the time Drop
        // runs. The close lifecycle (begin_close/poll_close) or the
        // ThreadedOperation completion path drives the worker to completion
        // before Drop.
        if let Some(handle) = self.handle.take() {
            debug_assert!(
                handle.is_finished(),
                "IoWorkerResource dropped while thread '{}' is still running — \
                 the close lifecycle must retire the worker before Drop",
                self.name
            );
            if handle.is_finished() {
                let _ = handle.join();
            }
            // If the thread is not finished at this point, something went
            // wrong in the lifecycle — the handle is dropped and the thread
            // is detached. This is a last-resort fallback only; the
            // cancellation flag was set above so the thread exits promptly
            // and retains no OS handles. In debug builds, the assertion
            // above will catch this.
        }
    }
}
