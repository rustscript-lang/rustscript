//! Private, transport-free ownership of an HTTP producer's original native join.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::Waker;

use futures_util::task::AtomicWaker;

pub(super) struct NativeJoin {
    pub(super) notifier_running: AtomicBool,
    pub(super) state: Mutex<JoinState>,
    input_ready: Condvar,
    pub(super) close_waker: AtomicWaker,
    pub(super) quiescence_waker: AtomicWaker,
    label: &'static str,
    thread_name: &'static str,
}

#[derive(Default)]
pub(super) struct JoinState {
    pub(super) handle: Option<std::thread::JoinHandle<()>>,
    notifier: Option<std::thread::JoinHandle<()>>,
    pub(super) armed: bool,
    joined: Option<Result<(), String>>,
    // Body wakes and noop handoffs must not replace the retained real waiter.
    pub(super) stream_waker: Option<Waker>,
}

struct NotifierTail(Arc<NativeJoin>);
impl Drop for NotifierTail {
    fn drop(&mut self) {
        self.0.notifier_running.store(false, Ordering::Release);
    }
}

/// An admission-owned input: every path either installs the unique original
/// handle or terminalizes workerless input, including unwinding before spawn.
pub(super) struct PrearmedInput(Option<Arc<NativeJoin>>);
impl PrearmedInput {
    pub(super) fn install(mut self, handle: std::thread::JoinHandle<()>) {
        self.0
            .take()
            .expect("prearmed input is owned")
            .install(handle);
    }
}
impl Drop for PrearmedInput {
    fn drop(&mut self) {
        if let Some(completion) = self.0.take() {
            completion.workerless();
        }
    }
}

impl NativeJoin {
    pub(super) fn new(label: &'static str, thread_name: &'static str) -> Self {
        Self {
            notifier_running: AtomicBool::new(false),
            state: Mutex::new(JoinState::default()),
            input_ready: Condvar::new(),
            close_waker: AtomicWaker::new(),
            quiescence_waker: AtomicWaker::new(),
            label,
            thread_name,
        }
    }

    pub(super) fn register_stream(&self, waker: &Waker) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.joined.is_none()
            && (state.stream_waker.is_none() || !waker.will_wake(Waker::noop()))
        {
            state.stream_waker = Some(waker.clone());
        }
    }

    pub(super) fn wake_stream(&self) {
        let waker = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stream_waker
            .clone();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn publish_joined(&self, result: Result<(), String>) {
        let waker = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.joined.is_some() {
                return;
            }
            state.joined = Some(result);
            state.stream_waker.take()
        };
        self.input_ready.notify_one();
        // Original native join precedes durable publication. Wake only outside
        // locks, draining retained task roots before the resource-free tail.
        if let Some(waker) = waker {
            waker.wake();
        }
        self.close_waker.wake();
        self.quiescence_waker.wake();
    }

    pub(super) fn install(&self, handle: std::thread::JoinHandle<()>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            state.handle.is_none() && state.joined.is_none(),
            "unique HTTP native input"
        );
        state.handle = Some(handle);
        drop(state);
        self.input_ready.notify_one();
    }

    pub(super) fn prearm(self: &Arc<Self>) -> Result<PrearmedInput, String> {
        self.prearm_with(|task| {
            std::thread::Builder::new()
                .name(self.thread_name.into())
                .spawn(task)
        })
    }

    pub(super) fn prearm_with(
        self: &Arc<Self>,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<std::thread::JoinHandle<()>>,
    ) -> Result<PrearmedInput, String> {
        self.arm_with(true, spawn)?;
        Ok(PrearmedInput(Some(Arc::clone(self))))
    }

    pub(super) fn poll_join(self: &Arc<Self>) -> Result<bool, String> {
        self.poll_join_with(|task| {
            std::thread::Builder::new()
                .name(self.thread_name.into())
                .spawn(task)
        })
    }

    pub(super) fn poll_join_with(
        self: &Arc<Self>,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<std::thread::JoinHandle<()>>,
    ) -> Result<bool, String> {
        self.arm_with(false, spawn)
    }

    fn arm_with(
        self: &Arc<Self>,
        await_input: bool,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<std::thread::JoinHandle<()>>,
    ) -> Result<bool, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(result) = state.joined.clone() {
            // Its own Packet is not a readiness gate: only finite infrastructure
            // Arc bookkeeping remains after publication, never VM/permit/IO.
            if let Some(handle) = state.notifier.take() {
                if handle.is_finished() {
                    let _ = handle.join();
                } else {
                    drop(handle);
                }
            }
            return result.map(|()| true);
        }
        if state.armed || (!await_input && state.handle.is_none()) {
            return Ok(false);
        }
        let completion = Arc::clone(self);
        // Keep original ownership recoverable if spawn fails. The closure
        // cannot take input until the admission/poll lock has been released.
        let notifier = spawn(Box::new(move || {
            let _tail = NotifierTail(Arc::clone(&completion));
            let handle = {
                let mut state = completion
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                while state.handle.is_none() && state.joined.is_none() {
                    state = completion
                        .input_ready
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                state.handle.take()
            };
            if let Some(handle) = handle {
                let result = handle.join().map_err(|panic| {
                    if let Some(message) = panic.downcast_ref::<&str>() {
                        (*message).to_string()
                    } else if let Some(message) = panic.downcast_ref::<String>() {
                        message.clone()
                    } else {
                        format!("{} worker thread panicked", completion.label)
                    }
                });
                completion.publish_joined(result);
            }
        }))
        .map_err(|error| {
            format!(
                "failed to start {} join notifier (worker ownership retained): {error}",
                self.label
            )
        })?;
        self.notifier_running.store(true, Ordering::Release);
        state.armed = true;
        state.notifier = Some(notifier);
        Ok(false)
    }

    pub(super) fn workerless(&self) {
        self.publish_joined(Ok(()));
    }

    pub(super) fn is_joined(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .joined
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::NativeJoin;
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    struct Wake(mpsc::Sender<()>);
    impl futures_util::task::ArcWake for Wake {
        fn wake_by_ref(this: &Arc<Self>) {
            let _ = this.0.send(());
        }
    }

    #[test]
    fn prearmed_original_exit_before_install_and_join_before_registration() {
        let completion = Arc::new(NativeJoin::new("HTTP request", "test-http-join"));
        let input = completion.prearm().unwrap();
        let handle = std::thread::spawn(|| {});
        let deadline = Instant::now() + Duration::from_secs(2);
        while !handle.is_finished() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let (tx, rx) = mpsc::channel();
        let wake = futures_util::task::waker(Arc::new(Wake(tx)));
        completion.close_waker.register(&wake);
        input.install(handle);
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(completion.is_joined());
        // Register strictly after durable join. No missing-wake assumption is
        // needed: the consumer's register/recheck immediately sees readiness.
        completion.quiescence_waker.register(&wake);
        assert!(completion.poll_join().unwrap());
        assert!(completion.poll_join().unwrap());
        assert!(completion.state.lock().unwrap().handle.is_none());
    }

    #[test]
    fn prearmed_input_drop_releases_workerless_notifier_without_orphan() {
        let completion = Arc::new(NativeJoin::new("HTTP request", "test-http-join"));
        let (exit_tx, exit_rx) = mpsc::channel();
        let input = completion
            .prearm_with(|task| {
                std::thread::Builder::new().spawn(move || {
                    task();
                    exit_tx.send(()).unwrap();
                })
            })
            .unwrap();
        assert!(!completion.is_joined());
        drop(input);
        exit_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(completion.poll_join().unwrap());
        assert!(
            !completion
                .notifier_running
                .load(std::sync::atomic::Ordering::Acquire)
        );
        assert!(completion.state.lock().unwrap().handle.is_none());
    }

    #[test]
    fn joined_readiness_does_not_wait_for_resource_free_notifier_packet_tail() {
        let completion = Arc::new(NativeJoin::new("HTTP request", "test-http-join"));
        let (published_tx, published_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (exit_tx, exit_rx) = mpsc::channel();
        let input = completion
            .prearm_with(|task| {
                std::thread::Builder::new().spawn(move || {
                    task();
                    published_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                    exit_tx.send(()).unwrap();
                })
            })
            .unwrap();
        input.install(std::thread::spawn(|| {}));
        published_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(completion.is_joined());
        assert!(
            !completion
                .state
                .lock()
                .unwrap()
                .notifier
                .as_ref()
                .unwrap()
                .is_finished()
        );
        assert!(
            completion.poll_join().unwrap(),
            "original joined, helper packet still held"
        );
        assert_eq!(
            Arc::strong_count(&completion),
            1,
            "tail retains no completion or producer root"
        );
        release_tx.send(()).unwrap();
        exit_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }
}
