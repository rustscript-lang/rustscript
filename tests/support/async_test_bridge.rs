use std::collections::HashMap;
use std::task::{Context, Poll};

use vm::{
    CallReturn, HostAsyncBridge, HostFuture, HostFutureOutput, HostOpId, Vm, VmError, VmResult,
};

struct TokioTestBridge {
    runtime: tokio::runtime::Runtime,
    futures: HashMap<HostOpId, HostFuture>,
}

impl TokioTestBridge {
    fn new() -> Self {
        Self {
            runtime: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("test runtime should build"),
            futures: HashMap::new(),
        }
    }
}

impl HostAsyncBridge for TokioTestBridge {
    fn submit_op(&mut self, op_id: HostOpId, future: HostFuture) -> VmResult<()> {
        if self.futures.insert(op_id, future).is_some() {
            return Err(VmError::HostError(format!(
                "duplicate submitted host op {op_id}"
            )));
        }
        Ok(())
    }

    fn poll_op(&mut self, op_id: HostOpId, _cx: &mut Context<'_>) -> Poll<VmResult<CallReturn>> {
        Poll::Ready(Err(VmError::HostError(format!(
            "unexpected external op {op_id}"
        ))))
    }

    fn poll_submitted_op(
        &mut self,
        op_id: HostOpId,
        cx: &mut Context<'_>,
    ) -> Poll<VmResult<HostFutureOutput>> {
        let poll = {
            let future = match self.futures.get_mut(&op_id) {
                Some(future) => future,
                None => {
                    return Poll::Ready(Err(VmError::HostError(format!(
                        "unknown submitted host op {op_id}"
                    ))));
                }
            };
            let _guard = self.runtime.enter();
            future.as_mut().poll(cx)
        };
        if poll.is_ready() {
            self.futures.remove(&op_id);
        }
        poll
    }

    fn cancel_op(&mut self, op_id: HostOpId) {
        self.futures.remove(&op_id);
    }
}

pub(crate) fn install(vm: &mut Vm) {
    vm.set_async_bridge(Box::new(TokioTestBridge::new()));
}
