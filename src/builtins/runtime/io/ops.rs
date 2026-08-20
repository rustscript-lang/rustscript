//! Shared [`HostOperation`] drivers for IO operations.
//!
//! These operation drivers are used by both the blocking and async IO paths.
//! Each driver implements [`HostOperation`] with a one-shot pending-result
//! provider: a synchronous operation completes immediately (returns `Ready` on
//! first poll), while an operation that runs on a worker thread stores a
//! `oneshot::Receiver` and returns `Pending` until the worker completes.
//!
//! Cancellation uses typed [`OperationCancelReason`] and is idempotent.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::task::{Context, Poll};
use std::thread;

use crate::vm::operation::driver::HostOperation;
use crate::vm::operation::error::{OperationError, OperationErrorCode, OperationResult};
use crate::vm::operation::reason::OperationCancelReason;

/// A one-shot operation that completes on the first poll.
///
/// Used for operations that finished synchronously but still need to go
/// through the `HostOperation` lifecycle (e.g. for registered pending
/// operations in the execution scope).
pub(crate) struct ReadyOperation;

impl HostOperation for ReadyOperation {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
        Poll::Ready(Ok(()))
    }

    fn cancel(&mut self, _reason: OperationCancelReason) -> OperationResult<()> {
        Ok(())
    }
}

/// A cancellation-aware operation that runs work on a dedicated thread and
/// reports the result via a oneshot channel.
///
/// The worker thread checks the cancellation flag before starting work.
/// Once cancelled, the receiver returns `Cancelled` on the next poll.
pub(crate) struct ThreadedOperation {
    cancelled: Arc<AtomicBool>,
    receiver: Option<Receiver<ThreadedResult>>,
    name: String,
    handle: Option<thread::JoinHandle<()>>,
}

/// Result sent from a worker thread back to the operation driver.
pub(crate) type ThreadedResult = Result<(), String>;

impl ThreadedOperation {
    /// Create a new threaded operation.
    ///
    /// The `work` closure receives the cancellation flag and a sender;
    /// it should check `cancelled.load(Ordering::SeqCst)` before starting
    /// and periodically during long-running work.
    pub(crate) fn new(
        name: impl Into<String>,
        cancelled: Arc<AtomicBool>,
        work: impl FnOnce(Arc<AtomicBool>, Sender<ThreadedResult>) + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let name = name.into();
        let name_clone = name.clone();
        let cancelled_clone = cancelled.clone();
        let handle = thread::Builder::new()
            .name(name_clone)
            .spawn(move || {
                work(cancelled_clone, tx);
            })
            .expect("io worker thread must spawn");
        Self {
            cancelled,
            receiver: Some(rx),
            name,
            handle: Some(handle),
        }
    }

    /// Create a synchronous operation that always returns Ready on first poll.
    pub(crate) fn ready() -> Self {
        let (tx, rx) = mpsc::channel();
        let _ = tx.send(Ok(()));
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            receiver: Some(rx),
            name: String::new(),
            handle: None,
        }
    }
}

impl HostOperation for ThreadedOperation {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
        // Check if cancelled.
        if self.cancelled.load(Ordering::SeqCst) {
            return Poll::Ready(Err(OperationError::new(
                OperationErrorCode::OperationDriverFailed,
                "io::operation",
                format!("operation '{}' was cancelled", self.name),
            )));
        }
        let receiver = match self.receiver.as_ref() {
            Some(r) => r,
            None => return Poll::Ready(Ok(())),
        };
        match receiver.try_recv() {
            Ok(Ok(())) => {
                self.receiver.take();
                self.handle.take();
                Poll::Ready(Ok(()))
            }
            Ok(Err(msg)) => {
                self.receiver.take();
                self.handle.take();
                Poll::Ready(Err(OperationError::new(
                    OperationErrorCode::OperationDriverFailed,
                    "io::operation",
                    msg,
                )))
            }
            Err(mpsc::TryRecvError::Empty) => Poll::Pending,
            Err(mpsc::TryRecvError::Disconnected) => {
                // Worker thread panicked or exited without sending a result.
                self.receiver.take();
                self.handle.take();
                Poll::Ready(Err(OperationError::new(
                    OperationErrorCode::OperationDriverFailed,
                    "io::operation",
                    format!(
                        "worker thread '{}' disconnected without a result",
                        self.name
                    ),
                )))
            }
        }
    }

    fn cancel(&mut self, reason: OperationCancelReason) -> OperationResult<()> {
        self.cancelled.store(true, Ordering::SeqCst);
        let _ = reason;
        Ok(())
    }
}

impl Drop for ThreadedOperation {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
