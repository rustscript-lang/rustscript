use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use super::*;

pub(crate) mod stream;
pub(crate) use stream::{HostStreamAction, HostStreamDriver, HostStreamPoll};

type HostVmCompletion<T> = Box<dyn FnOnce(&mut Vm) -> VmResult<T> + Send + 'static>;

pub enum HostFutureOutput<T = CallReturn> {
    Return(T),
    VmCompletion(HostVmCompletion<T>),
}

impl<T> HostFutureOutput<T> {
    pub fn returning(value: T) -> Self {
        Self::Return(value)
    }

    pub fn complete(completion: impl FnOnce(&mut Vm) -> VmResult<T> + Send + 'static) -> Self {
        Self::VmCompletion(Box::new(completion))
    }

    pub fn map<U: Send + 'static>(
        self,
        map: impl FnOnce(T) -> U + Send + 'static,
    ) -> HostFutureOutput<U>
    where
        T: Send + 'static,
    {
        match self {
            Self::Return(value) => HostFutureOutput::Return(map(value)),
            Self::VmCompletion(completion) => {
                HostFutureOutput::VmCompletion(Box::new(move |vm| completion(vm).map(map)))
            }
        }
    }
}

impl HostFutureOutput<CallReturn> {
    fn finish(self, vm: &mut Vm) -> VmResult<CallReturn> {
        match self {
            Self::Return(values) => Ok(values),
            Self::VmCompletion(completion) => completion(vm),
        }
    }
}

impl From<CallReturn> for HostFutureOutput<CallReturn> {
    fn from(values: CallReturn) -> Self {
        Self::Return(values)
    }
}

pub type HostFuture = Pin<Box<dyn Future<Output = VmResult<HostFutureOutput>> + Send + 'static>>;

pub trait CaptureAsyncHostContext: Send + 'static + Sized {
    fn capture(vm: &mut Vm) -> VmResult<Self>;

    fn capture_with_args(vm: &mut Vm, _args: &[Value]) -> VmResult<Self> {
        Self::capture(vm)
    }
}

pub trait HostAsyncBridge: Send {
    fn submit_op(&mut self, _op_id: HostOpId, _future: HostFuture) -> VmResult<()> {
        Err(VmError::HostError(
            "async host bridge does not accept submitted futures".to_string(),
        ))
    }

    fn poll_op(&mut self, op_id: HostOpId, cx: &mut Context<'_>) -> Poll<VmResult<CallReturn>>;

    fn poll_submitted_op(
        &mut self,
        op_id: HostOpId,
        cx: &mut Context<'_>,
    ) -> Poll<VmResult<HostFutureOutput>> {
        self.poll_op(op_id, cx)
            .map(|result| result.map(HostFutureOutput::Return))
    }

    fn cancel_op(&mut self, _op_id: HostOpId) {}

    fn cancel_op_with_reason(&mut self, op_id: HostOpId, _reason: CancellationReason) {
        self.cancel_op(op_id);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WaitingHostOp {
    pub(super) op_id: HostOpId,
    /// Exact host-return policy captured from the *actual call-site resolved
    /// import* when the pending host op was created (never a name lookup).
    /// Consumed by `complete_waiting_host_op` to validate async completion
    /// values before any stack/frame mutation. `Legacy` for non-schema /
    /// non-resource-exact / runtime-owned builtin / callable-stream ops.
    pub(super) exact_policy: super::host::ExactHostReturnPolicy,
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
        self.host.submitted_host_ops.insert(op_id);
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
        if self.host.stream_drivers.contains_key(&waiting.op_id) {
            self.cancel_callable_stream();
            return;
        }
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
            self.host.submitted_host_ops.remove(&waiting.op_id);
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
        if self.host.submitted_host_ops.remove(&op_id)
            && let Some(bridge) = self.host.async_bridge.as_mut()
        {
            bridge.cancel_op(op_id);
        }
        self.complete_waiting_host_op(op_id, values.into())
    }

    pub fn poll_waiting_host_op(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<()>> {
        let Some(waiting) = self.instance.waiting_host_op else {
            return Poll::Ready(Ok(()));
        };
        if self.host.stream_drivers.contains_key(&waiting.op_id) {
            return self.poll_callable_stream(waiting.op_id, cx);
        }
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
            if self.host.submitted_host_ops.contains(&waiting.op_id) {
                unsafe { (&mut *bridge_ptr).poll_submitted_op(waiting.op_id, cx) }
            } else {
                unsafe { (&mut *bridge_ptr).poll_op(waiting.op_id, cx) }
                    .map(|result| result.map(HostFutureOutput::Return))
            }
        } else {
            crate::builtins::runtime::poll_builtin_io_op(self, waiting.op_id, cx)
                .map(|result| result.map(HostFutureOutput::Return))
        };

        match poll_result {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(output)) => {
                let values = match output.finish(self) {
                    Ok(values) => values,
                    Err(err) => {
                        if host_bridge_owned {
                            self.host.submitted_host_ops.remove(&waiting.op_id);
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
                        return Poll::Ready(Err(err));
                    }
                };
                if host_bridge_owned {
                    self.host
                        .runtime_operations
                        .complete(operation_id)
                        .map_err(|error| VmError::HostError(error.to_string()))?;
                    self.host.submitted_host_ops.remove(&waiting.op_id);
                }
                self.complete_waiting_host_op(waiting.op_id, values)?;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) => {
                if host_bridge_owned {
                    self.host.submitted_host_ops.remove(&waiting.op_id);
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
