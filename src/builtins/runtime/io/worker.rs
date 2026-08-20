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
/// and a [`JoinHandle`]. The thread should periodically check
/// [`Self::is_cancelled`] and exit.
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

    /// Whether the worker has been asked to stop.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::SeqCst)
    }

    /// Returns a reference to the shared state.
    pub(crate) fn shared_state(&self) -> &Arc<SharedWorkerState> {
        &self.state
    }

    /// Take the result from the shared state, if available.
    pub(crate) fn take_result(&self) -> Option<Result<(), String>> {
        self.state
            .result
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
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
        // Never block joining a live worker. If the thread has finished
        // (e.g. after poll_close drove it to completion), join to observe
        // panics. Otherwise, detach — the thread checks cancellation and
        // exits quickly on its own.
        if let Some(handle) = self.handle.take() {
            if handle.is_finished() {
                let _ = handle.join();
            }
            // If not finished: handle is dropped → thread is detached.
            // This is safe because we set cancelled, and the worker checks
            // cancelled before doing any work, so it exits immediately.
        }
    }
}
