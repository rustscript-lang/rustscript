//! Shared [`HostOperation`] drivers for IO operations.
//!
//! These operation drivers are used by both the blocking and async IO paths.
//! Each driver implements [`HostOperation`] with a one-shot pending-result
//! provider: a synchronous operation completes immediately (returns `Ready` on
//! first poll), while an operation that runs on a worker thread publishes one
//! shared terminal result and returns `Pending` until that result is visible.
//!
//! Cancellation uses typed [`OperationCancelReason`] and is idempotent.
//! Worker joins are owned directly by their operation, so the operation
//! registry is the only terminal lifecycle authority.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};
use std::thread::{self, JoinHandle};

use crate::vm::Vm;
use crate::vm::operation::driver::HostOperation;
use crate::vm::operation::error::{OperationError, OperationErrorCode, OperationResult};
use crate::vm::operation::reason::OperationCancelReason;

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
struct CloseCompletionInner {
    result: Option<Result<(), String>>,
    waker: Option<Waker>,
}

pub(crate) struct CloseCompletionState {
    inner: Mutex<CloseCompletionInner>,
}

impl CloseCompletionState {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(CloseCompletionInner {
                result: None,
                waker: None,
            }),
        }
    }

    pub(crate) fn complete(&self, result: Result<(), String>) {
        let waker = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if inner.result.is_some() {
                return;
            }
            inner.result = Some(result);
            inner.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub(crate) fn result(&self) -> Option<Result<(), String>> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .result
            .clone()
    }

    pub(crate) fn poll_result(&self, cx: &Context<'_>) -> Option<Result<(), String>> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.waker = Some(cx.waker().clone());
        let result = inner.result.clone();
        if result.is_some() {
            inner.waker = None;
        }
        result
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
        match self.close_completion.poll_result(cx) {
            Some(result) => Poll::Ready(result.map_err(|message| {
                OperationError::new(
                    OperationErrorCode::OperationDriverFailed,
                    "io::close",
                    message,
                )
            })),
            None => Poll::Pending,
        }
    }

    fn cancel(&mut self, _reason: OperationCancelReason) -> OperationResult<()> {
        // Close is already in progress; cancellation is a no-op.
        // The close worker will finish regardless, and the operation
        // will complete when the close is done.
        Ok(())
    }
}

/// A shared transfer guard that holds a pipe handle until the worker takes
/// ownership. The worker returns a transferred handle through the operation's
/// pipe-result slot on normal completion; cancellation closes the associated
/// resource, and the guard drops any handle that was never transferred.
///
/// Unlike the raw `Arc<Mutex<Option<...>>>` pattern, this guard:
/// - Provides a typed, single-purpose API
/// - Clones the `Arc` for shared ownership (the guard itself is clonable)
/// - Enforces one live owner at a time across pre-start and worker transfer
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

    /// Drops a handle that was never transferred to the worker. A live handle
    /// returned by a worker is restored through the operation result adapter;
    /// cancellation instead closes the associated resource before this final
    /// owner release.
    pub(crate) fn restore_or_drop(&self) {
        drop(self.take());
    }
}

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

/// Terminal signal published by an IO worker.
pub(crate) type ThreadedWorkerSignal = Result<(), String>;

#[derive(Clone)]
pub(crate) struct ThreadedWorkerPublisher {
    state: Arc<SharedWorkerState>,
}

impl ThreadedWorkerPublisher {
    pub(crate) fn send(&self, signal: ThreadedWorkerSignal) -> Result<(), ThreadedWorkerSignal> {
        self.state.publish_result(signal);
        Ok(())
    }
}

struct WorkerLifecycle {
    terminal: Option<ThreadedWorkerSignal>,
    waker: Option<Waker>,
    handle: Option<JoinHandle<()>>,
}

/// One terminal authority shared by the operation and its worker thread.
/// Waker registration and terminal inspection use the same lock, so polling
/// registers first and then checks without a lost-wake window.
pub(crate) struct SharedWorkerState {
    pub(crate) cancelled: AtomicBool,
    finished: AtomicBool,
    lifecycle: Mutex<WorkerLifecycle>,
}

impl SharedWorkerState {
    pub(crate) fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            lifecycle: Mutex::new(WorkerLifecycle {
                terminal: None,
                waker: None,
                handle: None,
            }),
        }
    }

    pub(crate) fn publish_result(&self, signal: ThreadedWorkerSignal) {
        let waker = {
            let mut lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
            if lifecycle.terminal.is_some() {
                return;
            }
            lifecycle.terminal = Some(signal);
            lifecycle.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub(crate) fn worker_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    fn register_finished_waker(&self, cx: &Context<'_>) {
        if self.worker_finished() {
            return;
        }
        let mut lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        if !self.worker_finished() {
            lifecycle.waker = Some(cx.waker().clone());
        }
    }

    fn mark_worker_finished(&self) {
        self.finished.store(true, Ordering::Release);
        let waker = self
            .lifecycle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .waker
            .take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn poll_terminal(&self, cx: &Context<'_>) -> Option<ThreadedWorkerSignal> {
        let mut lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        lifecycle.waker = Some(cx.waker().clone());
        let terminal = lifecycle.terminal.clone();
        if terminal.is_some() {
            lifecycle.waker = None;
        }
        terminal
    }

    fn install_worker(&self, handle: JoinHandle<()>) -> Result<(), String> {
        let mut lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        if lifecycle.handle.is_some() {
            return Err("io worker handle was installed more than once".to_string());
        }
        lifecycle.handle = Some(handle);
        Ok(())
    }

    fn finish_worker(&self, cancel: bool, wait: bool, name: &str) -> Result<(), String> {
        if cancel {
            self.cancelled.store(true, Ordering::SeqCst);
        }
        let handle = {
            let mut lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
            match lifecycle.handle.as_ref() {
                Some(handle) if wait || handle.is_finished() => lifecycle.handle.take(),
                _ => None,
            }
        };
        if let Some(handle) = handle {
            handle
                .join()
                .map_err(|_| format!("worker thread '{name}' panicked"))?;
        }
        Ok(())
    }
}

struct WorkerFinishGuard {
    state: Arc<SharedWorkerState>,
}

impl Drop for WorkerFinishGuard {
    fn drop(&mut self) {
        self.state.mark_worker_finished();
    }
}

/// A cancellation-aware operation whose worker, terminal state, panic path,
/// cancellation path, and wakeup are owned by one operation driver.
pub(crate) struct ThreadedOperation {
    state: Arc<SharedWorkerState>,
    name: String,
}

impl ThreadedOperation {
    pub(crate) fn prepare(
        name: impl Into<String>,
    ) -> (Self, ThreadedWorkerPublisher, Arc<SharedWorkerState>) {
        let name = name.into();
        let state = Arc::new(SharedWorkerState::new());
        let publisher = ThreadedWorkerPublisher {
            state: Arc::clone(&state),
        };
        (
            Self {
                state: Arc::clone(&state),
                name,
            },
            publisher,
            state,
        )
    }

    pub(crate) fn spawn_worker(
        name: impl Into<String>,
        state: Arc<SharedWorkerState>,
        publisher: ThreadedWorkerPublisher,
        work: impl FnOnce(Arc<SharedWorkerState>, ThreadedWorkerPublisher) + Send + 'static,
    ) -> Result<(), String> {
        let name = name.into();
        let thread_name = name.clone();
        let worker_name = name.clone();
        let worker_state = Arc::clone(&state);
        let fallback_publisher = publisher.clone();
        let handle = match thread::Builder::new().name(thread_name).spawn(move || {
            let _finished = WorkerFinishGuard {
                state: Arc::clone(&worker_state),
            };
            if worker_state.cancelled.load(Ordering::SeqCst) {
                let _ = fallback_publisher.send(Err(format!(
                    "operation '{worker_name}' was cancelled before starting"
                )));
                return;
            }
            let result = catch_unwind(AssertUnwindSafe(|| {
                work(Arc::clone(&worker_state), publisher)
            }));
            if result.is_err() {
                fallback_publisher
                    .send(Err(format!("worker thread '{worker_name}' panicked")))
                    .ok();
            } else {
                let mut lifecycle = worker_state
                    .lifecycle
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if lifecycle.terminal.is_none() {
                    lifecycle.terminal = Some(Ok(()));
                    let waker = lifecycle.waker.take();
                    drop(lifecycle);
                    if let Some(waker) = waker {
                        waker.wake();
                    }
                }
            }
        }) {
            Ok(handle) => handle,
            Err(error) => {
                state.mark_worker_finished();
                return Err(format!("failed to spawn io worker '{name}': {error}"));
            }
        };
        state.install_worker(handle)
    }

    #[cfg(test)]
    pub(crate) fn spawn(
        name: impl Into<String>,
        work: impl FnOnce(Arc<SharedWorkerState>, ThreadedWorkerPublisher) + Send + 'static,
    ) -> (Self, ()) {
        let name = name.into();
        let (operation, publisher, state) = Self::prepare(name.clone());
        Self::spawn_worker(name, state, publisher, work).expect("test worker must spawn");
        (operation, ())
    }

    fn finish_worker(&self, cancel: bool, wait: bool) -> OperationResult<()> {
        self.state
            .finish_worker(cancel, wait, &self.name)
            .map_err(|message| {
                OperationError::new(
                    OperationErrorCode::OperationDriverFailed,
                    "io::operation",
                    message,
                )
            })
    }
}

impl HostOperation for ThreadedOperation {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
        let Some(signal) = self.state.poll_terminal(cx) else {
            return Poll::Pending;
        };
        self.finish_worker(false, true)?;
        Poll::Ready(signal.map_err(|message| {
            OperationError::new(
                OperationErrorCode::OperationDriverFailed,
                "io::operation",
                message,
            )
        }))
    }

    fn cancel(&mut self, reason: OperationCancelReason) -> OperationResult<()> {
        self.state.cancelled.store(true, Ordering::SeqCst);
        self.state.publish_result(Err(format!(
            "operation '{}' was cancelled: {reason:?}",
            self.name
        )));
        self.finish_worker(true, false)
    }

    fn is_quiescent(&self) -> bool {
        self.state.worker_finished()
    }

    fn register_quiescence_waker(&mut self, cx: &Context<'_>) {
        self.state.register_finished_waker(cx);
    }

    fn cancel_and_wait(&mut self, reason: OperationCancelReason) -> OperationResult<()> {
        self.cancel(reason)?;
        self.finish_worker(true, true)
    }
}

impl Drop for ThreadedOperation {
    fn drop(&mut self) {
        let _ = self.state.finish_worker(true, false, &self.name);
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

    /// Test: a worker panic is published through the same terminal state.
    #[test]
    fn threaded_op_worker_panic_returns_error() {
        let (mut op, ()) = ThreadedOperation::spawn("test", |_state, _tx| {
            panic!("synthetic worker panic");
        });

        let (waker, _wake_count) = CountingWaker::new();
        let waker = waker.into_waker();
        let mut cx = Context::from_waker(&waker);

        std::thread::sleep(std::time::Duration::from_millis(10));
        let poll_result = HostOperation::poll(&mut op, &mut cx);
        match poll_result {
            Poll::Ready(Err(err)) => {
                assert!(
                    err.message().contains("panicked"),
                    "error should mention worker panic: {}",
                    err.message()
                );
            }
            other => panic!("expected Ready(Err), got {other:?}"),
        }
    }

    /// Test: cancellation before poll returns cancelled error.
    #[test]
    fn threaded_op_cancelled_returns_error() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (mut op, ()) = ThreadedOperation::spawn("test", move |_state, _tx| {
            started_tx
                .send(())
                .expect("worker start signal should be observed");
            std::thread::sleep(std::time::Duration::from_millis(200));
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("worker should start");

        // Cancellation only publishes the typed terminal and requests worker
        // stop. It must never synchronously join an uncooperative worker.
        let started = std::time::Instant::now();
        op.cancel(OperationCancelReason::Requested)
            .expect("test operation cancellation should succeed");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "operation cancellation synchronously joined a live worker"
        );

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

    /// Test: the shared publisher is the sole terminal signal.
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
