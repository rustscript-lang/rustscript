use std::task::{Context, Poll};

use crate::compiler::TypeSchema;
use crate::vm::{CallOutcome, HostOpId, Value, Vm, VmError, VmResult, VmStatus};

/// The result of one host-side producer poll for a callable stream.
///
/// This is a host-only embedding extension point. It does not expose a stream
/// handle or polling operation to scripts. A [`HostStreamDriver::poll_next`]
/// call may yield at most one `Item`; the VM serializes that item with its
/// script callback before polling the producer again.
#[cfg_attr(not(feature = "http-client"), allow(dead_code))]
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
#[cfg_attr(not(feature = "http-client"), allow(dead_code))]
#[derive(Debug)]
pub(crate) enum HostStreamAction {
    /// Continue by returning control to producer polling.
    Continue,
    /// Finish the stream and return the supplied summary to the script call.
    Complete(Value),
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
#[cfg_attr(not(feature = "http-client"), allow(dead_code))]
pub(crate) trait HostStreamDriver: Send + 'static {
    /// Polls the producer for at most one item or its final summary.
    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<HostStreamPoll>>;

    /// Validates and applies one callback-returned action value.
    fn apply_action(&mut self, action: Value) -> VmResult<HostStreamAction>;

    /// Receives the exact lifecycle cancellation reason before producer release.
    /// Successful completion and operation failure drop the driver without
    /// invoking this hook.
    fn cancel(
        &mut self,
        _reason: crate::builtins::runtime::cancellation::CancellationReason,
    ) -> VmResult<()> {
        Ok(())
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

enum CallableStreamRetirement {
    Completed,
    Cancelled(crate::builtins::runtime::cancellation::CancellationReason),
    Failed(String),
    Polled,
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
    /// The driver is registered as a [`HostOperation`] in the current
    /// `ExecutionScope` (a [`StreamScopeOperation`]), so the returned pending id
    /// is a *packed* scope id and scope reset/drop cancellation reaches the
    /// producer through the operation driver. The VM polls the producer through
    /// a shared driver slot; scope cancellation releases the producer through the
    /// operation driver's `cancel`.
    ///
    /// The driver contract is documented on [`HostStreamDriver`]. In
    /// particular, producer polling and callback action application stay
    /// serialized and neither driver method may re-enter the VM.
    #[cfg_attr(not(feature = "http-client"), allow(dead_code))]
    pub(crate) fn submit_callable_stream(
        &mut self,
        callback: Value,
        driver: impl HostStreamDriver,
    ) -> VmResult<CallOutcome> {
        self.validate_stream_callback_value(&callback)?;
        if self.instance.host_stream.is_some() {
            return Err(VmError::HostError(
                "vm already owns an active callable stream".to_string(),
            ));
        }
        let scope_op = StreamScopeOperation {
            driver: Box::new(driver),
        };
        let scope_id = self
            .host
            .execution_scope_start_operation(crate::vm::operation::OperationSpec::new(scope_op))
            .map_err(|error| VmError::HostError(error.to_string()))?;
        let op_id = scope_id.raw();
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

    pub(crate) fn record_callable_stream_resume_ip(&mut self, op_id: HostOpId, resume_ip: usize) {
        if let Some(stream) = self.instance.host_stream.as_mut()
            && stream.op_id == op_id
        {
            stream.parent_ip = resume_ip;
        }
    }

    pub(crate) fn cancel_callable_stream(
        &mut self,
        reason: crate::builtins::runtime::cancellation::CancellationReason,
    ) -> VmResult<()> {
        let Some(stream) = self.instance.host_stream.take() else {
            return Ok(());
        };
        self.retire_callable_stream(stream, CallableStreamRetirement::Cancelled(reason))
    }

    pub(crate) fn clear_callable_stream_after_scope_close(&mut self) {
        let Some(stream) = self.instance.host_stream.take() else {
            self.instance.waiting_host_op = None;
            return;
        };
        self.retire_callable_stream(stream, CallableStreamRetirement::Polled)
            .expect("polled callable-stream retirement is infallible");
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
        let scope_id = crate::vm::operation::OperationId::from_raw(op_id)
            .expect("callable stream op id is a packed scope id");
        let polled = self
            .host
            .with_operation_driver_mut::<StreamScopeOperation, _>(scope_id, |operation| {
                operation.driver.poll_next(cx)
            })
            .map_err(VmError::from);
        let polled = match polled {
            Ok(polled) => polled,
            Err(error) => return Poll::Ready(Err(error)),
        };
        match polled {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(self
                .abort_callable_stream(&error)
                .err()
                .unwrap_or(error))),
            Poll::Ready(Ok(HostStreamPoll::Complete(summary))) => {
                Poll::Ready(self.finish_callable_stream(summary))
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
                    Err(error) => Poll::Ready(Err(self
                        .abort_callable_stream(&error)
                        .err()
                        .unwrap_or(error))),
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

    pub(crate) fn abort_callable_stream_on_run_error(&mut self, error: &VmError) -> VmResult<()> {
        if self
            .instance
            .host_stream
            .as_ref()
            .is_some_and(|stream| stream.phase == HostStreamPhase::RunCallback)
        {
            self.abort_callable_stream(error)?;
        }
        Ok(())
    }

    fn finish_callable_stream_callback(&mut self) -> VmResult<VmStatus> {
        let Some(action) = self.instance.host_return.take() else {
            let error = VmError::InvalidFrameState("callable stream callback returned no action");
            return Err(self.abort_callable_stream(&error).err().unwrap_or(error));
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
        let scope_id = crate::vm::operation::OperationId::from_raw(op_id)
            .expect("callable stream op id is a packed scope id");
        let applied = self
            .host
            .with_operation_driver_mut::<StreamScopeOperation, _>(scope_id, |operation| {
                operation.driver.apply_action(action)
            })
            .map_err(VmError::from)?;
        match applied {
            Ok(HostStreamAction::Continue) => {
                if let Some(stream) = self.instance.host_stream.as_mut() {
                    stream.phase = HostStreamPhase::AwaitItem;
                }
                self.instance.waiting_host_op = Some(super::WaitingHostOp {
                    op_id,
                    // Callable-stream items are not host-import resource
                    // returns; they keep the legacy policy (the stream poll
                    // path never runs exact-return validation).
                    exact_policy: super::host::ExactHostReturnPolicy::Legacy,
                });
                Ok(VmStatus::Waiting(op_id))
            }
            Ok(HostStreamAction::Complete(summary)) => {
                self.finish_callable_stream(summary)?;
                Ok(VmStatus::Halted)
            }
            Err(error) => Err(self.abort_callable_stream(&error).err().unwrap_or(error)),
        }
    }

    fn finish_callable_stream(&mut self, summary: Value) -> VmResult<()> {
        let Some(stream) = self.instance.host_stream.take() else {
            return Ok(());
        };
        self.retire_callable_stream(stream, CallableStreamRetirement::Completed)?;
        self.instance.stack.push(summary);
        Ok(())
    }

    fn abort_callable_stream(&mut self, failure: &VmError) -> VmResult<()> {
        let Some(stream) = self.instance.host_stream.take() else {
            return Ok(());
        };
        let parent_stack_base = stream.parent_stack_base;
        let parent_frame_count = stream.parent_frame_count;
        let retired = self.retire_callable_stream(
            stream,
            CallableStreamRetirement::Failed(failure.to_string()),
        );
        self.abort_host_invocation(parent_stack_base, parent_frame_count);
        retired
    }

    /// Central terminal teardown for a callable stream.
    ///
    /// The operation registry owns the producer lifecycle transition. Normal
    /// completion marks/consumes `Completed`; operation failures mark/consume
    /// `Failed`; lifecycle cancellation aborts with its exact reason. Every
    /// path then removes the VM map entry and continuation values exactly once.
    fn retire_callable_stream(
        &mut self,
        stream: HostStreamContinuation,
        retirement: CallableStreamRetirement,
    ) -> VmResult<()> {
        let scope_id = crate::vm::operation::OperationId::from_raw(stream.op_id)
            .expect("callable stream op id is a packed scope id");
        let retirement = match retirement {
            CallableStreamRetirement::Completed => {
                crate::vm::host_runtime::OperationRetirement::Completed
            }
            CallableStreamRetirement::Cancelled(reason) => {
                crate::vm::host_runtime::OperationRetirement::Cancelled(super::scope_reason(reason))
            }
            CallableStreamRetirement::Failed(message) => {
                crate::vm::host_runtime::OperationRetirement::Failed(
                    crate::vm::operation::OperationError::new(
                        crate::vm::operation::OperationErrorCode::OperationDriverFailed,
                        "vm::callable-stream",
                        message,
                    )
                    .with_value(stream.op_id),
                )
            }
            CallableStreamRetirement::Polled => {
                crate::vm::host_runtime::OperationRetirement::Polled
            }
        };
        let retired = self
            .host
            .retire_operation(scope_id, retirement)
            .map_err(VmError::from);
        self.instance.waiting_host_op = None;
        self.drop_value_with_contract(stream.callback);
        if let Some(item) = stream.item {
            self.drop_value_with_contract(item);
        }
        retired.map(|_| ())
    }
}

/// The sole owner and polling authority for a callable-stream producer.
/// VM continuation state retains only the packed operation id and callback
/// state; every poll, action, terminal transition, cancellation, and final
/// producer drop goes through this registered operation.
struct StreamScopeOperation {
    driver: Box<dyn HostStreamDriver>,
}

impl crate::vm::operation::HostOperation for StreamScopeOperation {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<crate::vm::operation::OperationResult<()>> {
        // Producer polling is requested through the registry's typed driver
        // access so callback execution remains serialized with it. Generic
        // scope polling stays pending until the VM's terminal transition.
        Poll::Pending
    }

    fn cancel(
        &mut self,
        reason: crate::vm::operation::OperationCancelReason,
    ) -> crate::vm::operation::OperationResult<()> {
        self.driver
            .cancel(cancellation_reason(reason))
            .map_err(|error| {
                crate::vm::operation::OperationError::new(
                    crate::vm::operation::OperationErrorCode::OperationDriverFailed,
                    "vm::callable-stream",
                    error.to_string(),
                )
            })
    }
}

fn cancellation_reason(
    reason: crate::vm::operation::OperationCancelReason,
) -> crate::builtins::runtime::cancellation::CancellationReason {
    use crate::builtins::runtime::cancellation::CancellationReason;
    match reason {
        crate::vm::operation::OperationCancelReason::Requested => CancellationReason::Requested,
        crate::vm::operation::OperationCancelReason::Deadline => CancellationReason::Deadline,
        crate::vm::operation::OperationCancelReason::VmReset => CancellationReason::VmReset,
        crate::vm::operation::OperationCancelReason::Parent => CancellationReason::Parent,
        crate::vm::operation::OperationCancelReason::ResourceClosed => {
            CancellationReason::ResourceClosed
        }
        crate::vm::operation::OperationCancelReason::VmDrop => CancellationReason::VmDrop,
    }
}
