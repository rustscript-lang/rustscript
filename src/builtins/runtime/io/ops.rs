//! Shared [`HostOperation`] drivers for IO operations.
//!
//! These operation drivers are used by both the blocking and async IO paths.
//! Each driver implements [`HostOperation`] with a one-shot pending-result
//! provider: a synchronous operation completes immediately (returns `Ready` on
//! first poll), while an operation that runs on a worker thread stores a
//! `mpsc::Receiver` and returns `Pending` until the worker completes.
//!
//! Cancellation uses typed [`OperationCancelReason`] and is idempotent.
//! Workers are registered as [`IoWorkerResource`] resources in the execution
//! scope so the generic close lifecycle handles them.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::task::{Context, Poll};
use std::thread;

use crate::vm::operation::driver::HostOperation;
use crate::vm::operation::error::{OperationError, OperationErrorCode, OperationResult};
use crate::vm::operation::reason::OperationCancelReason;

use super::worker::IoWorkerResource;

/// A one-shot operation that waits for a close-completion signal (an
/// `Arc<AtomicBool>` set by the resource's `poll_close` when the close
/// worker finishes).
///
/// Unlike `ReadyOperation`, this operation returns `Pending` until the
/// close worker actually completes, making it a true completion-driven
/// close operation rather than a post-close notification.
///
/// This operation is deliberately **not** associated with the target
/// resource handle (via `with_resource`): `close_resource` cancels every
/// operation associated with the target, which would self-cancel the
/// close-completion driver. Instead, the operation is registered as a
/// freestanding operation that shares the `close_completion` flag with
/// the resource through a shared `Arc`.
pub(crate) struct CloseCompletionOperation {
    close_completion: Arc<AtomicBool>,
}

impl CloseCompletionOperation {
    pub(crate) fn new(close_completion: Arc<AtomicBool>) -> Self {
        Self { close_completion }
    }
}

impl HostOperation for CloseCompletionOperation {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
        if self.close_completion.load(Ordering::SeqCst) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn cancel(&mut self, _reason: OperationCancelReason) -> OperationResult<()> {
        // Close is already in progress; cancellation is a no-op.
        // The close worker will finish regardless, and the operation
        // will complete when the close is done.
        Ok(())
    }
}

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

/// Signal sent from a worker thread back to the operation driver.
/// The actual result data is communicated through a separate shared
/// [`Arc`]`<`[`Mutex`]`<Option<...>>>` that the [`PendingOpResult`] closure
/// reads from.
pub(crate) type ThreadedWorkerSignal = Result<(), String>;

/// Shared state between a [`ThreadedOperation`] and its corresponding
/// [`IoWorkerResource`]. Both reference the same `Arc<SharedWorkerState>`.
pub(crate) struct SharedWorkerState {
    /// Cancellation flag — set by either operation cancel or resource close.
    pub(crate) cancelled: AtomicBool,
    /// One-shot result signalled by the worker thread.
    pub(crate) result: Mutex<Option<ThreadedWorkerSignal>>,
    /// Terminal error from the worker (beyond the signal — e.g. panic).
    pub(crate) terminal_error: Mutex<Option<String>>,
}

impl SharedWorkerState {
    pub(crate) fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            result: Mutex::new(None),
            terminal_error: Mutex::new(None),
        }
    }
}

/// A cancellation-aware operation that runs work on a dedicated thread and
/// reports completion via a channel.
///
/// The worker thread checks the cancellation flag before starting work.
/// Once cancelled, the receiver returns `Cancelled` on the next poll.
/// The actual IO result value is communicated through a separate shared
/// state (e.g. `Arc<Mutex<Option<...>>>`) that the `PendingOpResult` closure
/// reads from when the operation completes.
///
/// Both [`ThreadedOperation`] and [`IoWorkerResource`] share the same
/// [`SharedWorkerState`] reference. The former drives the VM operation
/// lifecycle; the latter manages the thread lifecycle (close/drop).
pub(crate) struct ThreadedOperation {
    state: Arc<SharedWorkerState>,
    /// The worker sends a completion signal through this channel.
    receiver: Option<Receiver<ThreadedWorkerSignal>>,
    name: String,
}

impl ThreadedOperation {
    /// Create a new threaded operation from a pre-constructed shared state.
    pub(crate) fn new(
        name: impl Into<String>,
        state: Arc<SharedWorkerState>,
        receiver: Receiver<ThreadedWorkerSignal>,
    ) -> Self {
        Self {
            state,
            receiver: Some(receiver),
            name: name.into(),
        }
    }

    /// Create the channel, shared state, and operation BEFORE spawning the
    /// worker thread. Returns `(Self, Sender, Arc<SharedWorkerState>)` so the
    /// caller can register the operation and resource first, then spawn the
    /// worker with `spawn_worker`.
    pub(crate) fn prepare(
        name: impl Into<String>,
    ) -> (Self, Sender<ThreadedWorkerSignal>, Arc<SharedWorkerState>) {
        let (tx, rx) = mpsc::channel();
        let name: String = name.into();
        let state = Arc::new(SharedWorkerState::new());
        let operation = Self {
            state: state.clone(),
            receiver: Some(rx),
            name,
        };
        (operation, tx, state)
    }

    /// Spawn a worker thread using the sender and shared state from
    /// [`Self::prepare`]. Returns an [`IoWorkerResource`] that manages the
    /// thread lifecycle.
    ///
    /// The worker should call `work(state, tx)` and signal completion
    /// through the sender. The `work` closure should check
    /// `state.cancelled.load(Ordering::SeqCst)` before starting and
    /// periodically during long-running work.
    pub(crate) fn spawn_worker(
        name: impl Into<String>,
        state: Arc<SharedWorkerState>,
        tx: Sender<ThreadedWorkerSignal>,
        work: impl FnOnce(Arc<SharedWorkerState>, Sender<ThreadedWorkerSignal>) + Send + 'static,
    ) -> IoWorkerResource {
        let name: String = name.into();
        let name_clone = name.clone();
        let state_clone = state.clone();
        let handle = thread::Builder::new()
            .name(name_clone)
            .spawn(move || {
                work(state_clone, tx);
            })
            .expect("io worker thread must spawn");
        IoWorkerResource::new(name, state, handle)
    }

    /// Create a shared worker state and channel pair, then spawn the worker
    /// thread. Returns `(Self, IoWorkerResource)` — the operation driver and
    /// the resource that manages the thread lifecycle.
    ///
    /// Convenience wrapper when deferred spawning is not needed (the thread
    /// is spawned immediately).
    pub(crate) fn spawn(
        name: impl Into<String>,
        work: impl FnOnce(Arc<SharedWorkerState>, Sender<ThreadedWorkerSignal>) + Send + 'static,
    ) -> (Self, IoWorkerResource) {
        let (operation, tx, state) = Self::prepare(name);
        let worker = Self::spawn_worker(&operation.name, state, tx, work);
        (operation, worker)
    }

    /// Returns the cancelled flag from the shared state.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::SeqCst)
    }

    /// Returns a reference to the shared state.
    pub(crate) fn shared_state(&self) -> &Arc<SharedWorkerState> {
        &self.state
    }
}

impl HostOperation for ThreadedOperation {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
        // Check if cancelled.
        if self.state.cancelled.load(Ordering::SeqCst) {
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
                // Worker completed successfully.
                self.receiver.take();
                Poll::Ready(Ok(()))
            }
            Ok(Err(msg)) => {
                // Worker reported an error.
                self.receiver.take();
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
        self.state.cancelled.store(true, Ordering::SeqCst);
        let _ = reason;
        Ok(())
    }
}

/// A typed cancellation reason for blocking IO operations.
/// Carries the reason for cancellation so the close-coordination layer
/// can determine what to do (e.g. kill the process for a pipe read).
pub(crate) enum IoCancelReason {
    /// Operation was cancelled by the VM (e.g. reset).
    OperationCancelled,
    /// Resource was closed.
    ResourceClosed,
    /// VM is being reset.
    VmReset,
}
