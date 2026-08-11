use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use super::*;

pub type HostFuture = Pin<Box<dyn Future<Output = VmResult<CallReturn>> + Send + 'static>>;

pub trait CaptureAsyncHostContext: Send + 'static + Sized {
    fn capture(vm: &mut Vm) -> VmResult<Self>;
}

pub trait HostAsyncBridge: Send {
    fn submit_op(&mut self, _op_id: HostOpId, _future: HostFuture) -> VmResult<()> {
        Err(VmError::HostError(
            "async host bridge does not accept submitted futures".to_string(),
        ))
    }

    fn poll_op(&mut self, op_id: HostOpId, cx: &mut Context<'_>) -> Poll<VmResult<CallReturn>>;

    fn cancel_op(&mut self, _op_id: HostOpId) {}

    fn cancel_op_with_reason(&mut self, op_id: HostOpId, _reason: CancellationReason) {
        self.cancel_op(op_id);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WaitingHostOp {
    pub(super) op_id: HostOpId,
}

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn noop_waker() -> Waker {
    Waker::from(Arc::new(NoopWake))
}

impl Vm {
    pub fn set_async_bridge(&mut self, bridge: Box<dyn HostAsyncBridge>) {
        self.cancel_waiting_host_op();
        self.host.async_bridge = Some(bridge);
    }

    pub fn clear_async_bridge(&mut self) {
        self.cancel_waiting_host_op();
        self.host.async_bridge = None;
    }

    pub fn allocate_host_op_id(&mut self) -> HostOpId {
        self.host
            .runtime_operations
            .allocate_id()
            .expect("host operation id space should not be exhausted")
            .raw()
    }

    pub fn submit_host_future(&mut self, future: HostFuture) -> VmResult<CallOutcome> {
        let op_id = self.allocate_host_op_id();
        let bridge = self.host.async_bridge.as_mut().ok_or_else(|| {
            VmError::HostError("async host function requires a host async bridge".to_string())
        })?;
        bridge.submit_op(op_id, future)?;
        Ok(CallOutcome::Pending(op_id))
    }

    pub fn waiting_host_op_id(&self) -> Option<HostOpId> {
        self.instance.waiting_host_op.map(|op| op.op_id)
    }

    pub fn cancel_waiting_host_op(&mut self) {
        self.cancel_waiting_host_op_with_reason(
            crate::builtins::runtime::cancellation::CancellationReason::Requested,
        );
    }

    pub(crate) fn cancel_waiting_host_op_with_reason(
        &mut self,
        reason: crate::builtins::runtime::cancellation::CancellationReason,
    ) {
        let Some(waiting) = self.instance.waiting_host_op.take() else {
            return;
        };
        let Ok(operation_id) =
            crate::builtins::runtime::cancellation::OperationId::from_raw(waiting.op_id)
        else {
            return;
        };
        let owner = self
            .host
            .runtime_operations
            .get(operation_id)
            .ok()
            .map(|operation| operation.owner());
        if owner == Some(crate::builtins::runtime::cancellation::OperationOwner::HostBridge) {
            if let Some(bridge) = self.host.async_bridge.as_mut() {
                bridge.cancel_op_with_reason(waiting.op_id, reason);
            }
            let _ = self.host.runtime_operations.cancel(operation_id, reason);
        } else {
            crate::builtins::runtime::cancel_builtin_io_op_with_reason(self, waiting.op_id, reason);
        }
    }

    pub fn complete_host_op(
        &mut self,
        op_id: HostOpId,
        values: impl Into<CallReturn>,
    ) -> VmResult<()> {
        let waiting = self.instance.waiting_host_op.ok_or_else(|| {
            VmError::HostError(format!(
                "host op {op_id} completed but vm is not waiting on any op",
            ))
        })?;
        if waiting.op_id != op_id {
            return Err(VmError::HostError(format!(
                "host op {op_id} completed while vm waits on {}",
                waiting.op_id
            )));
        }
        let operation_id = crate::builtins::runtime::cancellation::OperationId::from_raw(op_id)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        let operation = self
            .host
            .runtime_operations
            .get(operation_id)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        if operation.owner() != crate::builtins::runtime::cancellation::OperationOwner::HostBridge {
            return Err(VmError::HostError(format!(
                "host bridge cannot complete runtime-owned operation {op_id}",
            )));
        }
        self.host
            .runtime_operations
            .complete(operation_id)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        self.complete_waiting_host_op(op_id, values.into())
    }

    pub fn poll_waiting_host_op(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<()>> {
        let Some(waiting) = self.instance.waiting_host_op else {
            return Poll::Ready(Ok(()));
        };
        let operation_id =
            match crate::builtins::runtime::cancellation::OperationId::from_raw(waiting.op_id) {
                Ok(operation_id) => operation_id,
                Err(error) => return Poll::Ready(Err(VmError::HostError(error.to_string()))),
            };
        let operation = match self.host.runtime_operations.get(operation_id) {
            Ok(operation) => operation,
            Err(error) => return Poll::Ready(Err(VmError::HostError(error.to_string()))),
        };
        let host_bridge_owned =
            operation.owner() == crate::builtins::runtime::cancellation::OperationOwner::HostBridge;

        let poll_result = if host_bridge_owned {
            let bridge_ptr = match self.host.async_bridge.as_mut() {
                Some(bridge) => bridge.as_mut() as *mut dyn HostAsyncBridge,
                None => {
                    return Poll::Ready(Err(VmError::HostError(format!(
                        "vm waiting on host op {} without an async bridge",
                        waiting.op_id
                    ))));
                }
            };
            unsafe { (&mut *bridge_ptr).poll_op(waiting.op_id, cx) }
        } else {
            crate::builtins::runtime::poll_builtin_io_op(self, waiting.op_id, cx)
        };

        match poll_result {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(values)) => {
                if host_bridge_owned {
                    self.host
                        .runtime_operations
                        .complete(operation_id)
                        .map_err(|error| VmError::HostError(error.to_string()))?;
                }
                self.complete_waiting_host_op(waiting.op_id, values)?;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) => {
                if host_bridge_owned {
                    let runtime_error = crate::builtins::runtime::error::RuntimeError::new(
                        crate::builtins::runtime::error::RuntimeErrorCode::OperationFailed,
                        "runtime::host_bridge",
                        err.to_string(),
                    )
                    .with_value(waiting.op_id);
                    let _ = self
                        .host
                        .runtime_operations
                        .fail(operation_id, runtime_error);
                }
                self.instance.waiting_host_op = None;
                Poll::Ready(Err(err))
            }
        }
    }

    pub async fn await_waiting_host_op(&mut self) -> VmResult<()> {
        std::future::poll_fn(|cx| self.poll_waiting_host_op(cx)).await
    }

    pub fn wait_for_host_op_blocking(&mut self) -> VmResult<()> {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        loop {
            match self.poll_waiting_host_op(&mut cx) {
                Poll::Ready(result) => return result,
                Poll::Pending => {
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    #[cfg(target_arch = "wasm32")]
                    {
                        return Err(VmError::HostError(
                            "blocking host-op wait is unsupported on wasm32 runtime".to_string(),
                        ));
                    }
                }
            }
        }
    }

    pub fn wait_for_host_op_blocking_with_cancel<F>(&mut self, mut should_cancel: F) -> VmResult<()>
    where
        F: FnMut() -> bool,
    {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        loop {
            if should_cancel() {
                let cancellation_result = self
                    .run_ctx
                    .cancel(crate::builtins::runtime::cancellation::CancellationReason::Requested);
                self.cancel_waiting_host_op();
                cancellation_result?;
                return Err(VmError::HostError("host operation cancelled".to_string()));
            }
            match self.poll_waiting_host_op(&mut cx) {
                Poll::Ready(result) => return result,
                Poll::Pending => {
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    #[cfg(target_arch = "wasm32")]
                    {
                        return Err(VmError::HostError(
                            "blocking host-op wait is unsupported on wasm32 runtime".to_string(),
                        ));
                    }
                }
            }
        }
    }
}
