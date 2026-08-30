use std::task::{Context, Poll};

use crate::compiler::TypeSchema;
use crate::vm::execution_scope::ExecutionScope;
use crate::vm::operation::OperationCancelReason;
use crate::vm::{CallOutcome, HostOpId, Value, Vm, VmError, VmResult, VmStatus};

/// The result of one host-side producer poll for a callable stream.
///
/// This is a host-only embedding extension point. It does not expose a stream
/// handle or polling operation to scripts. A [`HostStreamDriver::poll_next`]
/// call may yield at most one `Item`; the VM serializes that item with its
/// script callback before polling the producer again.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum HostStreamPoll {
    /// Deliver one producer item to the script callback.
    Item(Value),
    /// Finish the stream and return the supplied summary to the script call.
    Complete(Value),
}

/// The host driver's response to one completed script callback.
///
/// Values returned by the callback remain inside the host embedding boundary:
/// no action handle is exposed to scripts.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum HostStreamAction {
    /// Continue by returning control to producer polling.
    Continue,
    /// Cancel the producer after returning the supplied final value. This is
    /// distinct from normal completion because the producer may still be
    /// blocked publishing the item whose callback requested the stop.
    Cancel(Value, OperationCancelReason),
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum HostStreamTermination {
    Completed,
    Cancelled(OperationCancelReason),
}

pub(crate) struct PendingHostStreamTermination {
    pub(crate) driver: Box<dyn HostStreamDriver>,
    pub(crate) termination: HostStreamTermination,
    pub(crate) admission_error: Option<VmError>,
    pub(crate) termination_started: bool,
    pub(crate) cleanup_error: Option<VmError>,
}

#[allow(dead_code)]
pub(crate) struct HostStreamAdmissionRollback {
    pub(crate) driver: Box<dyn HostStreamDriver>,
    pub(crate) termination: HostStreamTermination,
}

#[allow(dead_code)]
pub(crate) struct HostStreamAdmissionError {
    pub(crate) primary: VmError,
    pub(crate) rollback: HostStreamAdmissionRollback,
}

/// Host-only producer integration for a VM-serialized callable stream.
///
/// The VM always validates the callback's callable provenance and arity before
/// installing a driver. When its metadata is [`TypeSchema::Callable`], it also
/// validates the argument and result schemas against `fn(map) -> map`. Scripts
/// receive ordinary callback items and a final value; they never receive a
/// stream handle or a producer poll API.
///
/// Implementors must observe these contracts:
///
/// - [`poll_next`](Self::poll_next) yields at most one item per call and must
///   never re-enter the VM.
/// - [`apply_action`](Self::apply_action) takes ownership of the callback's
///   returned [`Value`], validates it as a driver-specific action, and must not
///   poll the producer.
/// - Dropping the driver is terminal resource cleanup after normal completion,
///   cancellation, or error. Only an early drop represents cancellation, and a
///   `Drop` implementation cannot infer the terminal reason; it must release
///   producer resources without requiring another poll.
#[allow(dead_code)]
pub(crate) trait HostStreamDriver: Send + 'static {
    /// Polls the producer for at most one item or its final summary.
    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<HostStreamPoll>>;

    /// Validates and applies one callback-returned action value.
    fn apply_action(&mut self, action: Value) -> VmResult<HostStreamAction>;

    /// Acknowledges the item currently owned by the VM callback. Drivers that
    /// use a producer-side acknowledgement gate override this hook; generic
    /// VM code remains unaware of the transport or adapter implementation.
    fn acknowledge_item(&mut self) {}

    /// Completes or cancels adapter-owned scope state after producer
    /// quiescence has been established by the driver's operation/resource.
    /// The default is suitable for drivers with no scoped child state.
    fn terminate(
        &mut self,
        _scope: &mut ExecutionScope,
        _termination: HostStreamTermination,
    ) -> VmResult<()> {
        Ok(())
    }

    /// Starts stream termination without waiting for an asynchronous producer.
    ///
    /// The default preserves the legacy one-shot termination contract. Drivers
    /// with worker-backed resources override this and retain their state until
    /// [`poll_termination`](Self::poll_termination) reports completion.
    fn begin_termination(
        &mut self,
        scope: &mut ExecutionScope,
        termination: HostStreamTermination,
    ) -> VmResult<()> {
        self.terminate(scope, termination)
    }

    /// Polls a previously started termination. The default driver has no
    /// asynchronous cleanup left after `begin_termination` returns.
    fn poll_termination(
        &mut self,
        _scope: &mut ExecutionScope,
        _termination: HostStreamTermination,
        _cx: &mut Context<'_>,
    ) -> Poll<VmResult<()>> {
        Poll::Ready(Ok(()))
    }
}

pub(crate) fn preserve_stream_cleanup(primary: VmError, cleanup: VmResult<()>) -> VmError {
    match cleanup {
        Ok(()) => primary,
        Err(cleanup) => {
            use std::fmt::Write as _;
            let mut message = primary.to_string();
            let _ = write!(message, "; cleanup failed: {cleanup}");
            VmError::HostError(message)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostStreamPhase {
    AwaitItem,
    RunCallback,
}

pub(crate) struct HostStreamContinuation {
    pub(crate) op_id: HostOpId,
    pub(crate) callback: Value,
    pub(crate) item: Option<Value>,
    pub(crate) phase: HostStreamPhase,
    pub(crate) parent_stack_base: usize,
    pub(crate) parent_frame_count: usize,
    pub(crate) parent_ip: usize,
}

impl Vm {
    /// Installs a host-only callable stream and suspends the current VM call.
    ///
    /// This Rust embedding API does not create a script-visible handle. The VM
    /// always validates that `callback` is a callable owned by this VM and has
    /// arity one. When its metadata is [`TypeSchema::Callable`], the VM also
    /// validates its argument and result schemas against `fn(map) -> map`. It
    /// then owns the callback and driver until completion, cancellation, reset,
    /// or error; removing the driver drops it to release producer resources.
    ///
    /// The driver contract is documented on [`HostStreamDriver`]. In
    /// particular, producer polling and callback action application stay
    /// serialized and neither driver method may re-enter the VM.
    #[allow(dead_code)]
    pub(crate) fn submit_callable_stream(
        &mut self,
        callback: Value,
        driver: impl HostStreamDriver,
    ) -> Result<CallOutcome, HostStreamAdmissionError> {
        if let Err(error) = self.validate_stream_callback_value(&callback) {
            return Err(HostStreamAdmissionError {
                primary: error,
                rollback: HostStreamAdmissionRollback {
                    driver: Box::new(driver),
                    termination: HostStreamTermination::Cancelled(OperationCancelReason::Requested),
                },
            });
        }
        if self.instance.host_stream.is_some() {
            return Err(HostStreamAdmissionError {
                primary: VmError::HostError(
                    "vm already owns an active callable stream".to_string(),
                ),
                rollback: HostStreamAdmissionRollback {
                    driver: Box::new(driver),
                    termination: HostStreamTermination::Cancelled(OperationCancelReason::Requested),
                },
            });
        }
        let op_id = self.allocate_host_op_id();
        self.host.stream_drivers.insert(op_id, Box::new(driver));
        self.instance.host_stream = Some(HostStreamContinuation {
            op_id,
            callback,
            item: None,
            phase: HostStreamPhase::AwaitItem,
            parent_stack_base: self.instance.stack.len(),
            parent_frame_count: self.instance.execution_frames.len(),
            parent_ip: self.instance.ip,
        });
        Ok(CallOutcome::Pending(op_id))
    }

    #[allow(dead_code)]
    pub(crate) fn rollback_rejected_callable_stream(
        &mut self,
        rejection: HostStreamAdmissionError,
    ) -> VmError {
        let primary_message = rejection.primary.to_string();
        self.host
            .retain_stream_admission_rollback(rejection.rollback, rejection.primary);
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        match self.host.poll_stream_terminations(&mut cx) {
            Poll::Ready(Err(error)) => error,
            Poll::Ready(Ok(())) => VmError::HostError(primary_message),
            Poll::Pending => VmError::HostError(format!(
                "{primary_message}; cleanup pending: callable stream admission rollback"
            )),
        }
    }

    pub fn validate_stream_callback_value(&self, callback: &Value) -> VmResult<()> {
        let Value::Callable(callable) = callback else {
            return Err(VmError::TypeMismatch("callable"));
        };
        if !self.owns_callable(callback) {
            return Err(VmError::InvalidCallable);
        }
        let prototype = self
            .program
            .callable_prototypes
            .get(callable.prototype_id as usize)
            .ok_or(VmError::InvalidCallablePrototype(callable.prototype_id))?;
        if prototype.arity != 1 {
            return Err(VmError::CallableArityMismatch {
                prototype_id: callable.prototype_id,
                expected: 1,
                got: prototype.arity,
            });
        }
        if let Some(TypeSchema::Callable { params, result }) = &prototype.schema
            && (!matches!(params.as_slice(), [TypeSchema::Map(_)])
                || !matches!(result.as_ref(), TypeSchema::Map(_)))
        {
            return Err(VmError::TypeMismatch("fn(map) -> map"));
        }
        Ok(())
    }

    pub(crate) fn cancel_callable_stream_with_reason(
        &mut self,
        reason: OperationCancelReason,
    ) -> VmResult<()> {
        let Some(stream) = self.instance.host_stream.take() else {
            return Ok(());
        };
        let cleanup = self
            .host
            .begin_stream_termination(stream.op_id, HostStreamTermination::Cancelled(reason))
            .and_then(|()| self.poll_stream_termination_once());
        self.instance.waiting_host_op = None;
        self.abort_host_invocation(stream.parent_stack_base, stream.parent_frame_count);
        if let Some(item) = stream.item {
            self.drop_value_with_contract(item);
        }
        self.drop_value_with_contract(stream.callback);
        cleanup
    }

    pub(crate) fn terminate_all_callable_streams_with_reason(
        &mut self,
        reason: OperationCancelReason,
    ) -> VmResult<()> {
        let mut first_error = None;
        if self.instance.host_stream.is_some() {
            match self.cancel_callable_stream_with_reason(reason) {
                Ok(()) => {}
                Err(error) => first_error = Some(error),
            }
        }
        let ids: Vec<HostOpId> = self.host.stream_drivers.keys().copied().collect();
        for op_id in ids {
            match self
                .host
                .begin_stream_termination(op_id, HostStreamTermination::Cancelled(reason))
            {
                Ok(()) => {}
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        match self.poll_stream_termination_once() {
            Ok(()) => {}
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub(crate) fn poll_callable_stream(
        &mut self,
        op_id: HostOpId,
        cx: &mut Context<'_>,
    ) -> Poll<VmResult<()>> {
        if self
            .instance
            .host_stream
            .as_ref()
            .map(|stream| stream.phase)
            != Some(HostStreamPhase::AwaitItem)
        {
            return Poll::Ready(Err(VmError::InvalidFrameState(
                "callable stream producer polled during callback",
            )));
        }
        let polled = match self.host.stream_drivers.get_mut(&op_id) {
            Some(driver) => driver.poll_next(cx),
            None => {
                return Poll::Ready(Err(VmError::HostError(format!(
                    "missing callable stream driver {op_id}"
                ))));
            }
        };
        match polled {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => {
                let cleanup = self.abort_callable_stream();
                Poll::Ready(Err(preserve_stream_cleanup(error, cleanup)))
            }
            Poll::Ready(Ok(HostStreamPoll::Complete(summary))) => {
                match self.finish_callable_stream(summary) {
                    Ok(true) => Poll::Ready(Ok(())),
                    Ok(false) => Poll::Pending,
                    Err(error) => Poll::Ready(Err(error)),
                }
            }
            Poll::Ready(Ok(HostStreamPoll::Item(item))) => {
                self.instance.waiting_host_op = None;
                if let Some(stream) = self.instance.host_stream.as_mut() {
                    stream.phase = HostStreamPhase::RunCallback;
                    stream.item = Some(item);
                }
                match self.start_callable_stream_callback() {
                    Ok(VmStatus::Halted) => match self.finish_callable_stream_callback() {
                        Ok(VmStatus::Halted) => Poll::Ready(Ok(())),
                        Ok(VmStatus::Waiting(_)) => {
                            cx.waker().wake_by_ref();
                            Poll::Pending
                        }
                        Ok(VmStatus::Yielded) => Poll::Ready(Ok(())),
                        Err(error) => Poll::Ready(Err(error)),
                    },
                    Ok(VmStatus::Yielded | VmStatus::Waiting(_)) => Poll::Ready(Ok(())),
                    Err(error) => {
                        let cleanup = self.abort_callable_stream();
                        Poll::Ready(Err(preserve_stream_cleanup(error, cleanup)))
                    }
                }
            }
        }
    }

    fn start_callable_stream_callback(&mut self) -> VmResult<VmStatus> {
        let (callback, item) = {
            let stream = self
                .instance
                .host_stream
                .as_mut()
                .ok_or(VmError::InvalidFrameState(
                    "missing callable stream continuation",
                ))?;
            (
                stream.callback.clone(),
                stream
                    .item
                    .take()
                    .ok_or(VmError::InvalidFrameState("missing callable stream item"))?,
            )
        };
        let operand_stack_base = self.instance.stack.len();
        let Value::Callable(callable) = callback else {
            return Err(VmError::InvalidCallable);
        };
        let outcome = self.enter_script_frame(
            callable.prototype_id,
            Some(callable),
            vec![item],
            operand_stack_base,
            None,
            crate::vm::instance::FrameContinuation::ReturnToHost,
        )?;
        match outcome {
            crate::vm::ExecOutcome::Continue => self.run_internal(None, false),
            crate::vm::ExecOutcome::Halted => Ok(VmStatus::Halted),
            crate::vm::ExecOutcome::Yielded => Ok(VmStatus::Yielded),
            crate::vm::ExecOutcome::Waiting(id) => Ok(VmStatus::Waiting(id)),
        }
    }

    pub(crate) fn resume_callable_stream_after_run(
        &mut self,
        status: VmStatus,
    ) -> VmResult<VmStatus> {
        if self
            .instance
            .host_stream
            .as_ref()
            .is_none_or(|stream| stream.phase != HostStreamPhase::RunCallback)
            || status != VmStatus::Halted
        {
            return Ok(status);
        }
        self.finish_callable_stream_callback()
    }

    pub(crate) fn abort_callable_stream_on_run_error(&mut self) -> VmResult<()> {
        if self
            .instance
            .host_stream
            .as_ref()
            .is_some_and(|stream| stream.phase == HostStreamPhase::RunCallback)
        {
            self.abort_callable_stream()
        } else {
            Ok(())
        }
    }

    fn finish_callable_stream_callback(&mut self) -> VmResult<VmStatus> {
        let Some(action) = self.instance.host_return.take() else {
            let error = VmError::InvalidFrameState("callable stream callback returned no action");
            return Err(preserve_stream_cleanup(error, self.abort_callable_stream()));
        };
        let op_id = self
            .instance
            .host_stream
            .as_ref()
            .ok_or(VmError::InvalidFrameState(
                "missing callable stream continuation",
            ))?
            .op_id;
        if let Some(stream) = self.instance.host_stream.as_ref() {
            self.instance.ip = stream.parent_ip;
        }
        let applied = self
            .host
            .stream_drivers
            .get_mut(&op_id)
            .ok_or_else(|| VmError::HostError(format!("missing callable stream driver {op_id}")))?
            .apply_action(action);
        match applied {
            Ok(HostStreamAction::Continue) => {
                if let Some(driver) = self.host.stream_drivers.get_mut(&op_id) {
                    driver.acknowledge_item();
                }
                if let Some(stream) = self.instance.host_stream.as_mut() {
                    stream.phase = HostStreamPhase::AwaitItem;
                }
                self.instance.waiting_host_op = Some(crate::vm::host::WaitingHostOp {
                    op_id,
                    source: crate::vm::host::WaitingHostOpSource::CallableStream,
                    expected_return_type: None,
                    expected_return_schema: None,
                });
                Ok(VmStatus::Waiting(op_id))
            }
            Ok(HostStreamAction::Cancel(summary, reason)) => {
                match self.finish_callable_stream_with_termination(
                    summary,
                    HostStreamTermination::Cancelled(reason),
                ) {
                    Ok(true) => Ok(VmStatus::Halted),
                    Ok(false) => Ok(VmStatus::Waiting(op_id)),
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(preserve_stream_cleanup(error, self.abort_callable_stream())),
        }
    }

    fn finish_callable_stream(&mut self, summary: Value) -> VmResult<bool> {
        self.finish_callable_stream_with_termination(summary, HostStreamTermination::Completed)
    }

    fn finish_callable_stream_with_termination(
        &mut self,
        summary: Value,
        termination: HostStreamTermination,
    ) -> VmResult<bool> {
        let Some(stream) = self.instance.host_stream.take() else {
            return Err(VmError::InvalidFrameState(
                "missing callable stream continuation",
            ));
        };
        let cleanup = self
            .host
            .begin_stream_termination(stream.op_id, termination)
            .and_then(|()| self.poll_stream_termination_once());
        self.instance.waiting_host_op = None;
        self.drop_value_with_contract(stream.callback);
        if let Some(item) = stream.item {
            self.drop_value_with_contract(item);
        }
        if let Err(error) = cleanup {
            self.abort_host_invocation(stream.parent_stack_base, stream.parent_frame_count);
            return Err(error);
        }
        self.instance.stack.push(summary);
        if self.host.has_pending_stream_terminations() {
            self.instance.waiting_host_op = Some(crate::vm::host::WaitingHostOp {
                op_id: stream.op_id,
                source: crate::vm::host::WaitingHostOpSource::CallableStreamTermination,
                expected_return_type: None,
                expected_return_schema: None,
            });
            Ok(false)
        } else {
            Ok(true)
        }
    }

    fn poll_stream_termination_once(&mut self) -> VmResult<()> {
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        match self.host.poll_stream_terminations(&mut cx) {
            Poll::Pending | Poll::Ready(Ok(())) => Ok(()),
            Poll::Ready(Err(error)) => Err(error),
        }
    }

    fn abort_callable_stream(&mut self) -> VmResult<()> {
        let Some(stream) = self.instance.host_stream.take() else {
            return Ok(());
        };
        let cleanup = self
            .host
            .begin_stream_termination(
                stream.op_id,
                HostStreamTermination::Cancelled(OperationCancelReason::Requested),
            )
            .and_then(|()| self.poll_stream_termination_once());
        self.instance.waiting_host_op = None;
        self.abort_host_invocation(stream.parent_stack_base, stream.parent_frame_count);
        self.drop_value_with_contract(stream.callback);
        if let Some(item) = stream.item {
            self.drop_value_with_contract(item);
        }
        cleanup
    }
}
