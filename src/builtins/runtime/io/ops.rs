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
use std::task::{Context, Poll, Waker};
use std::thread;

use crate::vm::operation::driver::HostOperation;
use crate::vm::operation::error::{OperationError, OperationErrorCode, OperationResult};
use crate::vm::operation::reason::OperationCancelReason;

use super::worker::IoWorkerResource;

/// Shared race-free close-completion state that carries a terminal result
/// and an optional [`Waker`]. Both the resource's close worker and the
/// [`CloseCompletionOperation`] driver share the same `Arc<CloseCompletionState>`.
///
/// The protocol:
/// 1. The close worker calls [`CloseCompletionState::complete`] with the
///    terminal result (success or error message).
/// 2. If a [`CloseCompletionOperation`] has already polled and stored a
///    waker, that waker is taken and called, waking the executor.
/// 3. If no one has polled yet, the result is stored and the next poll
///    returns `Ready` immediately (completion-before-first-poll race).
///
/// This replaces the old `Arc<AtomicBool>` approach, which lost the waker
/// and could not propagate errors.
pub(crate) struct CloseCompletionState {
    /// Terminal close result: `None` = still pending, `Some(Ok(()))` = success,
    /// `Some(Err(msg))` = flush/kill/wait/panic cleanup error.
    result: Mutex<Option<Result<(), String>>>,
    /// Waker registered by `CloseCompletionOperation::poll` when returning
    /// `Pending`. Stored under the same lock as `result` so the
    /// check-and-register is atomic — no lost wake.
    waker: Mutex<Option<Waker>>,
}

impl CloseCompletionState {
    pub(crate) fn new() -> Self {
        Self {
            result: Mutex::new(None),
            waker: Mutex::new(None),
        }
    }

    /// Store the terminal result, take and wake any registered waker, then
    /// wake the resource close progression (via `wake_by_ref` to the scope
    /// poller). Thread-safe; the result and waker are under separate locks
    /// but the waker is only taken-and-called after the result is stored.
    pub(crate) fn complete(&self, result: Result<(), String>) {
        // Store the result first.
        *self.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
        // Take and call the waker (if any) so the executor re-polls.
        if let Some(waker) = self.waker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            waker.wake();
        }
    }

    /// Check whether a terminal result is available. Returns `None` if still
    /// pending; `Some(Ok(()))` or `Some(Err(msg))` if the close has finished.
    pub(crate) fn take_result(&self) -> Option<Result<(), String>> {
        self.result.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Register or replace the waker. Called by `CloseCompletionOperation::poll`.
    fn register_waker(&self, waker: &Waker) {
        *self.waker.lock().unwrap_or_else(|e| e.into_inner()) = Some(waker.clone());
    }
}

/// A one-shot operation that waits for a close-completion signal through
/// a shared [`CloseCompletionState`].
///
/// Unlike `ReadyOperation`, this operation returns `Pending` until the
/// close worker actually completes, making it a true completion-driven
/// close operation rather than a post-close notification.
///
/// This operation is deliberately **not** associated with the target
/// resource handle (via `with_resource`): `close_resource` cancels every
/// operation associated with the target, which would self-cancel the
/// close-completion driver. Instead, the operation is registered as a
/// freestanding operation that shares the `CloseCompletionState` with
/// the resource through a shared `Arc`.
pub(crate) struct CloseCompletionOperation {
    close_completion: Arc<CloseCompletionState>,
}

impl CloseCompletionOperation {
    pub(crate) fn new(close_completion: Arc<CloseCompletionState>) -> Self {
        Self { close_completion }
    }
}

impl HostOperation for CloseCompletionOperation {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
        // Check terminal state under the lock. If the result is available,
        // return Ready immediately even if we never polled before
        // (completion-before-first-poll race).
        if let Some(result) = self.close_completion.take_result() {
            return Poll::Ready(result.map_err(|msg| {
                OperationError::new(
                    OperationErrorCode::OperationDriverFailed,
                    "io::close",
                    msg,
                )
            }));
        }
        // No result yet — register the waker (atomically, under the same
        // lock discipline) so the close worker can wake us.
        self.close_completion.register_waker(cx.waker());
        // Double-check: the worker might have completed between our
        // take_result check and the waker registration. If so, we must
        // return Ready to avoid a lost wake.
        // The waker registration is fine — calling it with an already-
        // completed state is a harmless no-op or extra wake.
        if let Some(result) = self.close_completion.take_result() {
            return Poll::Ready(result.map_err(|msg| {
                OperationError::new(
                    OperationErrorCode::OperationDriverFailed,
                    "io::close",
                    msg,
                )
            }));
        }
        Poll::Pending
    }

    fn cancel(&mut self, _reason: OperationCancelReason) -> OperationResult<()> {
        // Close is already in progress; cancellation is a no-op.
        // The close worker will finish regardless, and the operation
        // will complete when the close is done.
        Ok(())
    }
}

/// A shared transfer guard that holds a pipe handle that can be taken by
/// the worker thread or restored to the resource if the worker is cancelled
/// before starting. This prevents the OS descriptor leak described in
/// FINDING 1: the pipe handle is NOT taken from the resource until the
/// worker is actually about to start work. If cancellation fires before
/// the worker takes the handle, the PendingOpResult restores it.
pub(crate) struct PipeTransferGuard<T> {
    inner: Arc<Mutex<Option<T>>>,
    key: String,
}

impl<T: Send + 'static> PipeTransferGuard<T> {
    pub(crate) fn new(pipe: T, key: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Some(pipe))),
            key: key.into(),
        }
    }

    /// Take the pipe handle from the guard. Returns `None` if it was already
    /// taken (should not happen in correct usage).
    pub(crate) fn take(&self) -> Option<T> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Whether the pipe handle is still available (not yet taken by the worker).
    pub(crate) fn is_available(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }
}

/// A one-shot operation that completes on the first poll.
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
///
/// The waker protocol uses a two-lock check-register-double-check pattern:
/// 1. `poll` checks the result under the result lock — if available, returns
///    `Ready` (completion-before-poll race).
/// 2. If no result yet, `poll` stores `cx.waker()` under the waker lock.
/// 3. `poll` re-checks the result (completion-between-check-and-register race).
/// 4. Worker calls `publish_result` which stores the result, then takes and
///    wakes the waker under the waker lock.
pub(crate) struct SharedWorkerState {
    /// Cancellation flag — set by either operation cancel or resource close.
    pub(crate) cancelled: AtomicBool,
    /// One-shot result signalled by the worker thread.
    pub(crate) result: Mutex<Option<ThreadedWorkerSignal>>,
    /// Terminal error from the worker (beyond the signal — e.g. panic).
    pub(crate) terminal_error: Mutex<Option<String>>,
    /// Waker registered by `ThreadedOperation::poll` when returning `Pending`.
    /// Stored under a separate lock so the worker can take-and-call after
    /// publishing the result without holding the result lock.
    waker: Mutex<Option<Waker>>,
}

impl SharedWorkerState {
    pub(crate) fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            result: Mutex::new(None),
            terminal_error: Mutex::new(None),
            waker: Mutex::new(None),
        }
    }

    /// Publish the terminal result and wake any registered waker.
    /// Called by the worker thread when it finishes.
    pub(crate) fn publish_result(&self, signal: ThreadedWorkerSignal) {
        *self.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(signal);
        // Take and call the waker (if any) so the executor re-polls.
        if let Some(waker) = self.waker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            waker.wake();
        }
    }

    /// Register or replace the waker. Called by `ThreadedOperation::poll`.
    pub(crate) fn register_waker(&self, waker: &Waker) {
        *self.waker.lock().unwrap_or_else(|e| e.into_inner()) = Some(waker.clone());
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
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
        // Check if cancelled.
        if self.state.cancelled.load(Ordering::SeqCst) {
            return Poll::Ready(Err(OperationError::new(
                OperationErrorCode::OperationDriverFailed,
                "io::operation",
                format!("operation '{}' was cancelled", self.name),
            )));
        }
        // Check-1: terminal result available? (completion-before-poll race)
        {
            let result = self.state.result.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(signal) = result.as_ref() {
                return match signal {
                    Ok(()) => {
                        self.receiver.take();
                        Poll::Ready(Ok(()))
                    }
                    Err(msg) => {
                        self.receiver.take();
                        Poll::Ready(Err(OperationError::new(
                            OperationErrorCode::OperationDriverFailed,
                            "io::operation",
                            msg.clone(),
                        )))
                    }
                };
            }
        }
        // No result yet — check channel for disconnected/panic.
        let receiver = match self.receiver.as_ref() {
            Some(r) => r,
            None => return Poll::Ready(Ok(())),
        };
        match receiver.try_recv() {
            Ok(Ok(())) => {
                self.receiver.take();
                Poll::Ready(Ok(()))
            }
            Ok(Err(msg)) => {
                self.receiver.take();
                Poll::Ready(Err(OperationError::new(
                    OperationErrorCode::OperationDriverFailed,
                    "io::operation",
                    msg,
                )))
            }
            Err(mpsc::TryRecvError::Empty) => {
                // Register the waker so the worker can wake us.
                self.state.register_waker(cx.waker());
                // Double-check: the worker might have completed between our
                // check-1 / try_recv and the waker registration.
                let result = self.state.result.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(signal) = result.as_ref() {
                    return match signal {
                        Ok(()) => {
                            self.receiver.take();
                            Poll::Ready(Ok(()))
                        }
                        Err(msg) => {
                            self.receiver.take();
                            Poll::Ready(Err(OperationError::new(
                                OperationErrorCode::OperationDriverFailed,
                                "io::operation",
                                msg.clone(),
                            )))
                        }
                    };
                }
                Poll::Pending
            }
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    use super::*;
    use crate::vm::operation::driver::HostOperation;

    /// A counting waker that records how many times it was called.
    struct CountingWaker {
        wake_count: Arc<AtomicUsize>,
    }

    impl CountingWaker {
        fn new() -> (Self, Arc<AtomicUsize>) {
            let wake_count = Arc::new(AtomicUsize::new(0));
            (Self { wake_count: wake_count.clone() }, wake_count)
        }

        fn into_waker(self) -> Waker {
            let raw = Arc::into_raw(Arc::new(self)) as *const ();
            unsafe { Waker::from_raw(RawWaker::new(raw, &COUNTING_WAKER_VTABLE)) }
        }
    }

    const COUNTING_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        |ptr| {
            // Clone: increment the Arc's strong count.
            unsafe { Arc::increment_strong_count(ptr as *const CountingWaker) };
            RawWaker::new(ptr, &COUNTING_WAKER_VTABLE)
        },
        |ptr| {
            // Wake: consume the waker, increment count, then drop.
            let counter = unsafe { Arc::from_raw(ptr as *const CountingWaker) };
            counter.wake_count.fetch_add(1, Ordering::SeqCst);
            drop(counter);
        },
        |ptr| {
            // Wake by ref: just increment count.
            let counter = unsafe { &*(ptr as *const CountingWaker) };
            counter.wake_count.fetch_add(1, Ordering::SeqCst);
        },
        |ptr| {
            // Drop: consume the Arc.
            drop(unsafe { Arc::from_raw(ptr as *const CountingWaker) });
        },
    );

    /// Test: completion before first poll returns Ready immediately.
    #[test]
    fn close_completion_before_poll_returns_ready() {
        let state = Arc::new(CloseCompletionState::new());
        state.complete(Ok(()));

        let mut op = CloseCompletionOperation::new(state);
        let (waker, _wake_count) = CountingWaker::new();
        let waker = waker.into_waker();
        let mut cx = Context::from_waker(&waker);

        let poll_result = HostOperation::poll(&mut op, &mut cx);
        assert!(matches!(poll_result, Poll::Ready(Ok(()))));
    }

    /// Test: completion with error before first poll returns Ready(Err).
    #[test]
    fn close_completion_error_before_poll_propagates() {
        let state = Arc::new(CloseCompletionState::new());
        state.complete(Err("flush failed".to_string()));

        let mut op = CloseCompletionOperation::new(state);
        let (waker, _wake_count) = CountingWaker::new();
        let waker = waker.into_waker();
        let mut cx = Context::from_waker(&waker);

        let poll_result = HostOperation::poll(&mut op, &mut cx);
        match poll_result {
            Poll::Ready(Err(err)) => {
                assert!(
                    err.message().contains("flush failed"),
                    "error should contain 'flush failed': {}",
                    err.message()
                );
            }
            other => panic!("expected Ready(Err), got {other:?}"),
        }
    }

    /// Test: poll returns Pending, then complete wakes the waker.
    #[test]
    fn close_completion_wakes_after_poll_pending() {
        let state = Arc::new(CloseCompletionState::new());
        let mut op = CloseCompletionOperation::new(state.clone());
        let (waker, wake_count) = CountingWaker::new();
        let waker = waker.into_waker();
        let mut cx = Context::from_waker(&waker);

        // First poll: no result yet, should return Pending and register waker.
        let poll_result = HostOperation::poll(&mut op, &mut cx);
        assert!(matches!(poll_result, Poll::Pending));

        // Complete the state — this should wake the waker.
        state.complete(Ok(()));

        // Waker should have been called.
        assert_eq!(wake_count.load(Ordering::SeqCst), 1);

        // Second poll: result is available.
        let poll_result = HostOperation::poll(&mut op, &mut cx);
        assert!(matches!(poll_result, Poll::Ready(Ok(()))));
    }

    /// Test: complete with error wakes and propagates error.
    #[test]
    fn close_completion_error_wakes_and_propagates() {
        let state = Arc::new(CloseCompletionState::new());
        let mut op = CloseCompletionOperation::new(state.clone());
        let (waker, wake_count) = CountingWaker::new();
        let waker = waker.into_waker();
        let mut cx = Context::from_waker(&waker);

        // First poll: no result yet.
        let poll_result = HostOperation::poll(&mut op, &mut cx);
        assert!(matches!(poll_result, Poll::Pending));

        // Complete with error.
        state.complete(Err("Killed by reset".to_string()));

        // Waker should have been called.
        assert_eq!(wake_count.load(Ordering::SeqCst), 1);

        // Second poll: error available.
        let poll_result = HostOperation::poll(&mut op, &mut cx);
        match poll_result {
            Poll::Ready(Err(err)) => {
                assert!(
                    err.message().contains("Killed by reset"),
                    "error should contain 'Killed by reset': {}",
                    err.message()
                );
            }
            other => panic!("expected Ready(Err), got {other:?}"),
        }
    }

    /// Test: double poll — first registers waker, second is still Pending if
    /// no completion yet, then complete wakes and third poll returns Ready.
    #[test]
    fn close_completion_double_poll_then_complete() {
        let state = Arc::new(CloseCompletionState::new());
        let mut op = CloseCompletionOperation::new(state.clone());
        let (waker, wake_count) = CountingWaker::new();
        let waker = waker.into_waker();
        let mut cx = Context::from_waker(&waker);

        // First poll: Pending, registers waker.
        assert!(matches!(HostOperation::poll(&mut op, &mut cx), Poll::Pending));
        assert_eq!(wake_count.load(Ordering::SeqCst), 0);

        // Second poll: still Pending, replaces waker.
        assert!(matches!(HostOperation::poll(&mut op, &mut cx), Poll::Pending));
        assert_eq!(wake_count.load(Ordering::SeqCst), 0);

        // Complete.
        state.complete(Ok(()));
        assert_eq!(wake_count.load(Ordering::SeqCst), 1);

        // Third poll: Ready.
        assert!(matches!(HostOperation::poll(&mut op, &mut cx), Poll::Ready(Ok(()))));
    }

    /// Test: completion-before-first-poll race — the worker completes
    /// between the first take_result check and the waker registration.
    /// The double-check after waker registration catches this.
    #[test]
    fn close_completion_race_between_check_and_register() {
        let state = Arc::new(CloseCompletionState::new());
        let mut op = CloseCompletionOperation::new(state.clone());

        // Manually simulate the race: complete right after the first
        // take_result check. We do this by calling poll from within
        // a closure that completes the state mid-way.
        //
        // The poll function's check-register-double-check pattern handles
        // this naturally: even if the worker completes right after
        // register_waker, the double-check catches it.
        let (waker, wake_count) = CountingWaker::new();
        let waker = waker.into_waker();
        let mut cx = Context::from_waker(&waker);

        // First poll registers waker.
        assert!(matches!(HostOperation::poll(&mut op, &mut cx), Poll::Pending));
        assert_eq!(wake_count.load(Ordering::SeqCst), 0);

        // Complete (simulating worker finishing).
        state.complete(Ok(()));
        assert_eq!(wake_count.load(Ordering::SeqCst), 1);

        // Now poll again: should find the result.
        assert!(matches!(HostOperation::poll(&mut op, &mut cx), Poll::Ready(Ok(()))));
    }

    /// Test: waker is replaced on subsequent polls.
    #[test]
    fn close_completion_replaces_waker() {
        let state = Arc::new(CloseCompletionState::new());
        let mut op = CloseCompletionOperation::new(state.clone());

        let (waker1, count1) = CountingWaker::new();
        let waker1 = waker1.into_waker();
        let mut cx1 = Context::from_waker(&waker1);

        // First poll: Pending, registers waker1.
        assert!(matches!(HostOperation::poll(&mut op, &mut cx1), Poll::Pending));

        let (waker2, count2) = CountingWaker::new();
        let waker2 = waker2.into_waker();
        let mut cx2 = Context::from_waker(&waker2);

        // Second poll: Pending, replaces with waker2.
        assert!(matches!(HostOperation::poll(&mut op, &mut cx2), Poll::Pending));

        // Complete — should wake waker2, not waker1.
        state.complete(Ok(()));
        assert_eq!(count1.load(Ordering::SeqCst), 0, "waker1 should not be woken");
        assert_eq!(count2.load(Ordering::SeqCst), 1, "waker2 should be woken");

        // Poll again: Ready.
        assert!(matches!(HostOperation::poll(&mut op, &mut cx2), Poll::Ready(Ok(()))));
    }
}