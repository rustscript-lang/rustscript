use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use vm::{
    CallReturn, HostAsyncBridge, HostFuture, HostFutureOutput, HostOpId, Vm, VmError, VmResult,
};

/// Embedding-owned executor; submitted futures can progress while the test
/// waits for a server-side readiness barrier. Cancellation is acknowledged
/// only after Tokio has dropped the entire submitted future.
pub struct TokioHostDriver {
    runtime: Option<tokio::runtime::Runtime>,
    tasks: HashMap<HostOpId, tokio::task::JoinHandle<VmResult<HostFutureOutput>>>,
}
impl Default for TokioHostDriver {
    fn default() -> Self {
        Self {
            runtime: Some(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .unwrap(),
            ),
            tasks: HashMap::new(),
        }
    }
}
impl HostAsyncBridge for TokioHostDriver {
    fn submit_op(&mut self, id: HostOpId, future: HostFuture) -> VmResult<()> {
        if self.tasks.contains_key(&id) {
            return Err(VmError::HostError("duplicate submitted operation".into()));
        }
        let task = self.runtime.as_ref().unwrap().spawn(future);
        self.tasks.insert(id, task);
        Ok(())
    }
    fn poll_op(&mut self, id: HostOpId, _: &mut Context<'_>) -> Poll<VmResult<CallReturn>> {
        Poll::Ready(Err(VmError::HostError(format!(
            "unknown external operation {id}"
        ))))
    }
    fn poll_submitted_op(
        &mut self,
        id: HostOpId,
        cx: &mut Context<'_>,
    ) -> Poll<VmResult<HostFutureOutput>> {
        let task = self
            .tasks
            .get_mut(&id)
            .ok_or_else(|| VmError::HostError(format!("unknown submitted operation {id}")))?;
        match Pin::new(task).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.tasks.remove(&id);
                Poll::Ready(result.map_err(|e| VmError::HostError(e.to_string()))?)
            }
        }
    }
    fn request_cancel_op(
        &mut self,
        id: HostOpId,
        _: vm::operation::OperationCancelReason,
    ) -> VmResult<()> {
        if let Some(task) = self.tasks.get(&id) {
            task.abort();
        }
        Ok(())
    }
    fn poll_cancel_op(&mut self, id: HostOpId, cx: &mut Context<'_>) -> Poll<VmResult<()>> {
        let Some(task) = self.tasks.get_mut(&id) else {
            return Poll::Ready(Ok(()));
        };
        match Pin::new(task).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.tasks.remove(&id);
                Poll::Ready(match result {
                    Ok(_) => Ok(()),
                    Err(error) if error.is_cancelled() => Ok(()),
                    Err(error) => Err(VmError::HostError(error.to_string())),
                })
            }
        }
    }
    fn cleanup_op(&mut self, id: HostOpId, _: vm::HostAsyncOpTerminal) -> VmResult<()> {
        if let Some(task) = self.tasks.remove(&id) {
            task.abort();
        }
        Ok(())
    }
}
impl Drop for TokioHostDriver {
    fn drop(&mut self) {
        for task in self.tasks.values() {
            task.abort();
        }
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}
pub fn install(vm: &mut Vm) {
    vm.set_async_bridge(Box::<TokioHostDriver>::default())
        .unwrap();
}
