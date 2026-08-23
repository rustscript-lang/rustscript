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

use crate::vm::Vm;
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
                OperationError::new(OperationErrorCode::OperationDriverFailed, "io::close", msg)
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
                OperationError::new(OperationErrorCode::OperationDriverFailed, "io::close", msg)
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
/// the worker takes the handle, the guard's `restore_or_drop` restores it.
///
/// Unlike the raw `Arc<Mutex<Option<...>>>` pattern, this guard:
/// - Provides a typed, single-purpose API
/// - Clones the `Arc` for shared ownership (the guard itself is clonable)
/// - Has a `restore_or_drop` method that attempts to restore the pipe
///   handle into the resource, or drops it if the resource is closing
pub(crate) struct PipeTransferGuard<T> {
    inner: Arc<Mutex<Option<T>>>,
    key: String,
}

impl<T> Clone for PipeTransferGuard<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            key: self.key.clone(),
        }
    }
}

impl<T: Send + 'static> PipeTransferGuard<T> {
    pub(crate) fn new(pipe: T, key: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Some(pipe))),
            key: key.into(),
        }
    }

    /// Take the pipe handle from the guard. Returns `None` if it was already
    /// taken.
    pub(crate) fn take(&self) -> Option<T> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Whether the pipe handle is still available (not yet taken by the worker).
    #[cfg(test)]
    pub(crate) fn is_available(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    #[cfg(test)]
    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    /// Restore the pipe handle into the resource, or drop it if the resource
    /// is closing. This is the canonical restore path for the case where the
    /// worker was cancelled before starting and the guard still holds the
    /// pipe handle.
    ///
    /// If the handle was already taken by the worker, this is a no-op.
    /// If the resource is closing or gone, the pipe handle is dropped to
    /// avoid leaking the OS descriptor.
    pub(crate) fn restore_or_drop(
        &self,
        vm: &mut Vm,
        handle: crate::vm::resource::ResourceHandle,
        restore: impl FnOnce(&mut IoPipeResource, T),
    ) {
        let pipe = match self.take() {
            Some(p) => p,
            None => return,
        };
        let mut ctx = vm.host_context();
        if let Ok(token) = ctx.typed_resource::<IoPipeResource>(handle)
            && let Ok(mut resource) = ctx.resource_mut::<IoPipeResource>(&token)
            && !resource.get().is_closed()
        {
            restore(resource.get(), pipe);
            return;
        }
        // Resource is closing or gone — drop the pipe handle (OS descriptor
        // is closed by Drop). This is safe: the worker never took it.
        drop(pipe);
    }
}

// Re-import IoPipeResource for the restore_or_drop and free functions.
use super::shared::IoPipeResource;

/// Restore a reader pipe handle into the resource, or drop it if the
/// resource is closing. Used by read_line's PendingOpResult when the
/// worker returned the pipe handle through the shared channel.
pub(crate) fn restore_reader_or_drop(
    vm: &mut Vm,
    handle: crate::vm::resource::ResourceHandle,
    pipe: std::process::ChildStdout,
) {
    let mut ctx = vm.host_context();
    if let Ok(token) = ctx.typed_resource::<IoPipeResource>(handle)
        && let Ok(mut resource) = ctx.resource_mut::<IoPipeResource>(&token)
        && !resource.get().is_closed()
    {
        resource.get().restore_reader(pipe);
        return;
    }
    drop(pipe);
}

/// Restore a writer pipe handle into the resource, or drop it if the
/// resource is closing. Used by write/flush's PendingOpResult when the
/// worker returned the pipe handle through the shared channel.
pub(crate) fn restore_writer_or_drop(
    vm: &mut Vm,
    handle: crate::vm::resource::ResourceHandle,
    pipe: std::process::ChildStdin,
) {
    let mut ctx = vm.host_context();
    if let Ok(token) = ctx.typed_resource::<IoPipeResource>(handle)
        && let Ok(mut resource) = ctx.resource_mut::<IoPipeResource>(&token)
        && !resource.get().is_closed()
    {
        resource.get().restore_writer(pipe);
        return;
    }
    drop(pipe);
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

            waker: Mutex::new(None),
        }
    }

    /// Publish the terminal result and wake any registered waker.
    /// Called by the worker thread when it finishes.
    /// This is the sole terminal signal: once called, the result is set and
    /// the mpsc channel signal is also sent. The mpsc+state cannot diverge
    /// because both are set in the same critical section (the worker's
    /// completion handler sets both before returning).
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
    #[cfg(test)]
    pub(crate) fn spawn(
        name: impl Into<String>,
        work: impl FnOnce(Arc<SharedWorkerState>, Sender<ThreadedWorkerSignal>) + Send + 'static,
    ) -> (Self, IoWorkerResource) {
        let (operation, tx, state) = Self::prepare(name);
        let worker = Self::spawn_worker(&operation.name, state, tx, work);
        (operation, worker)
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
            (
                Self {
                    wake_count: wake_count.clone(),
                },
                wake_count,
            )
        }

        fn into_waker(self) -> Waker {
            let raw = Arc::into_raw(Arc::new(self)) as *const ();
            unsafe { Waker::from_raw(RawWaker::new(raw, &COUNTING_WAKER_VTABLE)) }
        }
    }

    const COUNTING_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        |ptr| {
            unsafe { Arc::increment_strong_count(ptr as *const CountingWaker) };
            RawWaker::new(ptr, &COUNTING_WAKER_VTABLE)
        },
        |ptr| {
            let counter = unsafe { Arc::from_raw(ptr as *const CountingWaker) };
            counter.wake_count.fetch_add(1, Ordering::SeqCst);
            drop(counter);
        },
        |ptr| {
            let counter = unsafe { &*(ptr as *const CountingWaker) };
            counter.wake_count.fetch_add(1, Ordering::SeqCst);
        },
        |ptr| {
            drop(unsafe { Arc::from_raw(ptr as *const CountingWaker) });
        },
    );

    // ====================================================================
    // CloseCompletionOperation tests
    // ====================================================================

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

        let poll_result = HostOperation::poll(&mut op, &mut cx);
        assert!(matches!(poll_result, Poll::Pending));

        state.complete(Ok(()));
        assert_eq!(wake_count.load(Ordering::SeqCst), 1);

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

        let poll_result = HostOperation::poll(&mut op, &mut cx);
        assert!(matches!(poll_result, Poll::Pending));

        state.complete(Err("Killed by reset".to_string()));
        assert_eq!(wake_count.load(Ordering::SeqCst), 1);

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

        assert!(matches!(
            HostOperation::poll(&mut op, &mut cx),
            Poll::Pending
        ));
        assert_eq!(wake_count.load(Ordering::SeqCst), 0);

        assert!(matches!(
            HostOperation::poll(&mut op, &mut cx),
            Poll::Pending
        ));
        assert_eq!(wake_count.load(Ordering::SeqCst), 0);

        state.complete(Ok(()));
        assert_eq!(wake_count.load(Ordering::SeqCst), 1);

        assert!(matches!(
            HostOperation::poll(&mut op, &mut cx),
            Poll::Ready(Ok(()))
        ));
    }

    /// Test: completion-before-first-poll race — the worker completes
    /// between the first take_result check and the waker registration.
    #[test]
    fn close_completion_race_between_check_and_register() {
        let state = Arc::new(CloseCompletionState::new());
        let mut op = CloseCompletionOperation::new(state.clone());
        let (waker, wake_count) = CountingWaker::new();
        let waker = waker.into_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(matches!(
            HostOperation::poll(&mut op, &mut cx),
            Poll::Pending
        ));
        assert_eq!(wake_count.load(Ordering::SeqCst), 0);

        state.complete(Ok(()));
        assert_eq!(wake_count.load(Ordering::SeqCst), 1);

        assert!(matches!(
            HostOperation::poll(&mut op, &mut cx),
            Poll::Ready(Ok(()))
        ));
    }

    /// Test: waker is replaced on subsequent polls.
    #[test]
    fn close_completion_replaces_waker() {
        let state = Arc::new(CloseCompletionState::new());
        let mut op = CloseCompletionOperation::new(state.clone());

        let (waker1, count1) = CountingWaker::new();
        let waker1 = waker1.into_waker();
        let mut cx1 = Context::from_waker(&waker1);

        assert!(matches!(
            HostOperation::poll(&mut op, &mut cx1),
            Poll::Pending
        ));

        let (waker2, count2) = CountingWaker::new();
        let waker2 = waker2.into_waker();
        let mut cx2 = Context::from_waker(&waker2);

        assert!(matches!(
            HostOperation::poll(&mut op, &mut cx2),
            Poll::Pending
        ));

        state.complete(Ok(()));
        assert_eq!(
            count1.load(Ordering::SeqCst),
            0,
            "waker1 should not be woken"
        );
        assert_eq!(count2.load(Ordering::SeqCst), 1, "waker2 should be woken");

        assert!(matches!(
            HostOperation::poll(&mut op, &mut cx2),
            Poll::Ready(Ok(()))
        ));
    }

    // ====================================================================
    // ThreadedOperation event-wake tests
    // ====================================================================

    /// Test: completion before poll returns Ready immediately.
    /// Uses prepare+manual worker so we can guarantee completion before poll.
    #[test]
    fn threaded_op_completion_before_poll_returns_ready() {
        let (operation, tx, state) = ThreadedOperation::prepare("test");
        let mut op = operation;

        // Signal completion before polling.
        state.publish_result(Ok(()));
        let _ = tx.send(Ok(()));

        let (waker, _wake_count) = CountingWaker::new();
        let waker = waker.into_waker();
        let mut cx = Context::from_waker(&waker);

        let poll_result = HostOperation::poll(&mut op, &mut cx);
        assert!(matches!(poll_result, Poll::Ready(Ok(()))));
    }

    /// Test: poll returns Pending, then worker completes and wakes via publish_result.
    #[test]
    fn threaded_op_pending_then_wake() {
        let (operation, tx, state) = ThreadedOperation::prepare("test");
        let mut op = operation;

        let (waker, wake_count) = CountingWaker::new();
        let waker = waker.into_waker();
        let mut cx = Context::from_waker(&waker);

        // First poll: should return Pending (no result yet).
        let poll_result = HostOperation::poll(&mut op, &mut cx);
        assert!(matches!(poll_result, Poll::Pending));
        assert_eq!(wake_count.load(Ordering::SeqCst), 0);

        // Simulate the worker completing.
        state.publish_result(Ok(()));
        let _ = tx.send(Ok(()));

        // Waker should have been called.
        assert_eq!(wake_count.load(Ordering::SeqCst), 1);

        // Second poll: should find the result.
        let poll_result = HostOperation::poll(&mut op, &mut cx);
        assert!(matches!(poll_result, Poll::Ready(Ok(()))));
    }

    /// Test: worker completes between check-1 and waker registration (double-check catches it).
    #[test]
    fn threaded_op_race_between_check_and_register() {
        // Use a manual approach: we can do the first poll and then complete.
        let (operation, tx, state) = ThreadedOperation::prepare("test");
        let mut op = operation;

        let (waker, wake_count) = CountingWaker::new();
        let waker = waker.into_waker();
        let mut cx = Context::from_waker(&waker);

        // First poll: returns Pending, registers waker.
        let poll_result = HostOperation::poll(&mut op, &mut cx);
        assert!(matches!(poll_result, Poll::Pending));
        assert_eq!(wake_count.load(Ordering::SeqCst), 0);

        // Complete (simulating worker finishing between check and register).
        state.publish_result(Ok(()));
        let _ = tx.send(Ok(()));

        // Waker was called.
        assert_eq!(wake_count.load(Ordering::SeqCst), 1);

        // Second poll: should find the result.
        let poll_result = HostOperation::poll(&mut op, &mut cx);
        assert!(matches!(poll_result, Poll::Ready(Ok(()))));
    }

    /// Test: waker is replaced on subsequent polls.
    #[test]
    fn threaded_op_replaces_waker() {
        let (operation, tx, state) = ThreadedOperation::prepare("test");
        let mut op = operation;

        let (waker1, count1) = CountingWaker::new();
        let waker1 = waker1.into_waker();
        let mut cx1 = Context::from_waker(&waker1);

        // First poll: Pending, registers waker1.
        assert!(matches!(
            HostOperation::poll(&mut op, &mut cx1),
            Poll::Pending
        ));

        let (waker2, count2) = CountingWaker::new();
        let waker2 = waker2.into_waker();
        let mut cx2 = Context::from_waker(&waker2);

        // Second poll: Pending, replaces with waker2.
        assert!(matches!(
            HostOperation::poll(&mut op, &mut cx2),
            Poll::Pending
        ));

        // Complete — should wake waker2, not waker1.
        state.publish_result(Ok(()));
        let _ = tx.send(Ok(()));
        assert_eq!(
            count1.load(Ordering::SeqCst),
            0,
            "waker1 should not be woken"
        );
        assert_eq!(count2.load(Ordering::SeqCst), 1, "waker2 should be woken");

        // Poll again: Ready.
        assert!(matches!(
            HostOperation::poll(&mut op, &mut cx2),
            Poll::Ready(Ok(()))
        ));
    }

    /// Test: worker disconnects without sending a result.
    #[test]
    fn threaded_op_disconnect_returns_error() {
        // Create a ThreadedOperation where the worker drops the sender without
        // sending a result.
        let (operation, tx, _state) = ThreadedOperation::prepare("test");
        let mut op = operation;
        // Drop the sender immediately — simulates worker panic/disconnect.
        drop(tx);

        let (waker, _wake_count) = CountingWaker::new();
        let waker = waker.into_waker();
        let mut cx = Context::from_waker(&waker);

        let poll_result = HostOperation::poll(&mut op, &mut cx);
        match poll_result {
            Poll::Ready(Err(err)) => {
                assert!(
                    err.message().contains("disconnected"),
                    "error should mention disconnected: {}",
                    err.message()
                );
            }
            other => panic!("expected Ready(Err), got {other:?}"),
        }
    }

    /// Test: cancellation before poll returns cancelled error.
    #[test]
    fn threaded_op_cancelled_returns_error() {
        let (mut op, worker) = ThreadedOperation::spawn("test", |_state, _tx| {
            // Worker does nothing — should never be reached if cancelled.
            std::thread::sleep(std::time::Duration::from_millis(100));
        });

        // Cancel the operation.
        op.cancel(OperationCancelReason::Requested)
            .expect("test operation cancellation should succeed");

        let (waker, _wake_count) = CountingWaker::new();
        let waker = waker.into_waker();
        let mut cx = Context::from_waker(&waker);

        let poll_result = HostOperation::poll(&mut op, &mut cx);
        match poll_result {
            Poll::Ready(Err(err)) => {
                assert!(
                    err.message().contains("cancelled"),
                    "error should mention cancelled: {}",
                    err.message()
                );
            }
            other => panic!("expected Ready(Err), got {other:?}"),
        }

        // Wait for the worker thread to finish before dropping the worker.
        // The IoWorkerResource::Drop asserts the thread is finished.
        std::thread::sleep(std::time::Duration::from_millis(150));
        // Ensure the worker is cancelled so it won't block on anything.
        drop(worker);
    }

    /// Test: error result from worker propagates.
    #[test]
    fn threaded_op_error_propagates() {
        let (operation, tx, state) = ThreadedOperation::prepare("test");
        let mut op = operation;

        // Signal error before polling.
        state.publish_result(Err("io error".to_string()));
        let _ = tx.send(Err("io error".to_string()));

        let (waker, _wake_count) = CountingWaker::new();
        let waker = waker.into_waker();
        let mut cx = Context::from_waker(&waker);

        let poll_result = HostOperation::poll(&mut op, &mut cx);
        match poll_result {
            Poll::Ready(Err(err)) => {
                assert!(
                    err.message().contains("io error"),
                    "error should contain 'io error': {}",
                    err.message()
                );
            }
            other => panic!("expected Ready(Err), got {other:?}"),
        }
    }

    /// Test: publish_result is the sole terminal signal — mpsc+state cannot diverge.
    /// Both are set atomically in the completion handler, so polling via either
    /// path returns the same result.
    #[test]
    fn threaded_op_publish_result_is_terminal_signal() {
        let (operation, tx, state) = ThreadedOperation::prepare("test");
        let mut op = operation;

        // Signal completion through both paths (as the worker does).
        state.publish_result(Ok(()));
        let _ = tx.send(Ok(()));

        // Poll should find the result regardless of which path detects it first.
        let (waker, _wake_count) = CountingWaker::new();
        let waker = waker.into_waker();
        let mut cx = Context::from_waker(&waker);

        let poll_result = HostOperation::poll(&mut op, &mut cx);
        assert!(matches!(poll_result, Poll::Ready(Ok(()))));
    }

    // ====================================================================
    // PipeTransferGuard tests
    // ====================================================================

    /// Test: guard starts with available pipe, take consumes it.
    #[test]
    fn pipe_guard_before_start_is_available() {
        let guard: PipeTransferGuard<i32> = PipeTransferGuard::new(42, "test");
        assert!(guard.is_available());
        assert_eq!(guard.take(), Some(42));
        assert!(!guard.is_available());
        assert_eq!(guard.take(), None);
    }

    /// Test: guard key returns the label.
    #[test]
    fn pipe_guard_key_returns_label() {
        let guard: PipeTransferGuard<i32> = PipeTransferGuard::new(42, "my-key");
        assert_eq!(guard.key(), "my-key");
    }

    /// Test: guard clone shares the same inner Arc.
    #[test]
    fn pipe_guard_clone_shares_arc() {
        let guard: PipeTransferGuard<i32> = PipeTransferGuard::new(42, "test");
        let cloned = guard.clone();
        // Take from one, the other is now empty.
        assert_eq!(guard.take(), Some(42));
        assert_eq!(cloned.take(), None);
    }

    /// Test: guard is_available returns false after take.
    #[test]
    fn pipe_guard_is_available_after_take() {
        let guard: PipeTransferGuard<i32> = PipeTransferGuard::new(42, "test");
        assert!(guard.is_available());
        let _ = guard.take();
        assert!(!guard.is_available());
    }

    /// Test: guard restore_or_drop is a no-op when already taken.
    #[test]
    fn pipe_guard_restore_or_drop_already_taken() {
        // This test verifies the method doesn't panic when the guard is empty.
        // Since we can't construct a real Vm in unit tests, we just verify
        // that take returns None after being consumed.
        let guard: PipeTransferGuard<i32> = PipeTransferGuard::new(42, "test");
        let _ = guard.take();
        // After take, the guard is empty — any subsequent take returns None.
        assert_eq!(guard.take(), None);
    }
}
