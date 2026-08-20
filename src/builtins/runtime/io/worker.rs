//! [`IoWorkerResource`] — a cooperative worker-thread resource.
//!
//! Worker threads are used by the blocking and async IO paths to run
//! synchronous file/process operations without blocking the VM. The worker
//! owns a cancellation flag and a [`JoinHandle`]:
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::thread::JoinHandle;

use crate::host_api::ResourceTypeKey;
use crate::vm::resource::{CloseProgress, HostResource, ResourceCloseReason, ResourceResult};

/// Stable catalog identity for an IO worker-thread resource.
pub(crate) fn io_worker_key() -> ResourceTypeKey {
    ResourceTypeKey::new("io.worker").expect("io.worker resource type key must be valid")
}

/// A worker thread resource.
///
/// The worker owns a cancellation flag and a [`JoinHandle`]. The thread
/// should periodically check [`Self::is_cancelled`] and exit.
pub(crate) struct IoWorkerResource {
    cancelled: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    name: String,
}

impl IoWorkerResource {
    /// Create a new worker resource from a spawned thread.
    ///
    /// `cancel_flag` must be the same `Arc<AtomicBool>` that the worker
    /// thread checks; `handle` is the join handle.
    pub(crate) fn new(
        name: impl Into<String>,
        cancelled: Arc<AtomicBool>,
        handle: JoinHandle<()>,
    ) -> Self {
        Self {
            cancelled,
            handle: Some(handle),
            name: name.into(),
        }
    }

    /// Whether the worker has been asked to stop.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

impl HostResource for IoWorkerResource {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(io_worker_key())
    }

    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        // Signal cancellation so the worker thread can observe it.
        self.cancelled.store(true, Ordering::SeqCst);
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
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
