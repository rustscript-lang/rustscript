use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::{Duration, Instant};

use vm::{Vm, VmError};

const RESET_TIMEOUT: Duration = Duration::from_secs(5);

struct ResetWake(thread::Thread);

impl Wake for ResetWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Starts a reset and drives its asynchronous close to completion.
pub fn reset_for_reuse_to_ready(vm: &mut Vm) -> Result<(), VmError> {
    vm.reset_for_reuse()?;
    if !vm.scope_reset_pending() {
        return Ok(());
    }

    let deadline = Instant::now() + RESET_TIMEOUT;
    let waker = Waker::from(Arc::new(ResetWake(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        match vm.poll_reset_for_reuse(&mut cx) {
            Poll::Ready(result) => return result,
            Poll::Pending => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(
                    !remaining.is_zero(),
                    "reset did not reach quiescence in time"
                );
                thread::park_timeout(remaining);
            }
        }
    }
}
