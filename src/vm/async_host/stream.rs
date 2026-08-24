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
    operation_state: std::sync::Arc<StreamOperationState>,
    pub(crate) phase: HostStreamPhase,
    pub(crate) parent_stack_base: usize,
    pub(crate) parent_frame_count: usize,
    pub(crate) parent_ip: usize,
}

struct StreamOperationState {
    inner: std::sync::Mutex<StreamOperationStateInner>,
}

struct StreamOperationStateInner {
    event: Option<HostStreamPoll>,
    action: Option<Value>,
}

impl StreamOperationState {
    fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(StreamOperationStateInner {
                event: None,
                action: None,
            }),
        }
    }

    fn take_event(&self) -> VmResult<Option<HostStreamPoll>> {
        self.inner
            .lock()
            .map(|mut state| state.event.take())
            .map_err(|_| VmError::HostError("callable stream state lock is poisoned".to_string()))
    }

    fn publish_event(&self, event: HostStreamPoll) -> crate::vm::operation::OperationResult<()> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| stream_operation_error("callable stream state lock is poisoned"))?;
        if state.event.is_some() {
            return Err(stream_operation_error(
                "callable stream producer published more than one unconsumed event",
            ));
        }
        state.event = Some(event);
        Ok(())
    }

    fn set_action(&self, action: Value) -> VmResult<()> {
        let mut state = self.inner.lock().map_err(|_| {
            VmError::HostError("callable stream state lock is poisoned".to_string())
        })?;
        if state.action.is_some() {
            return Err(VmError::InvalidFrameState(
                "callable stream already has a pending callback action",
            ));
        }
        state.action = Some(action);
        Ok(())
    }

    fn take_action(&self) -> crate::vm::operation::OperationResult<Option<Value>> {
        self.inner
            .lock()
            .map(|mut state| state.action.take())
            .map_err(|_| stream_operation_error("callable stream state lock is poisoned"))
    }

    fn has_event(&self) -> crate::vm::operation::OperationResult<bool> {
        self.inner
            .lock()
            .map(|state| state.event.is_some())
            .map_err(|_| stream_operation_error("callable stream state lock is poisoned"))
    }

    fn drain_values(&self) -> VmResult<Vec<Value>> {
        let mut state = self.inner.lock().map_err(|_| {
            VmError::HostError("callable stream state lock is poisoned".to_string())
        })?;
        let mut values = Vec::new();
        if let Some(event) = state.event.take() {
            values.push(match event {
                HostStreamPoll::Item(value) | HostStreamPoll::Complete(value) => value,
            });
        }
        if let Some(action) = state.action.take() {
            values.push(action);
        }
        Ok(values)
    }
}

enum CallableStreamRetirement {
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
    /// producer through the operation driver. Producer polling and callback
    /// action application both pass through that registered operation; scope
    /// cancellation releases the producer through the operation driver's
    /// `cancel`.
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
        let operation_state = std::sync::Arc::new(StreamOperationState::new());
        let scope_op = StreamScopeOperation {
            driver: Box::new(driver),
            state: std::sync::Arc::clone(&operation_state),
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
            operation_state,
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
        let polled = self.host.execution_scope_poll_operation(scope_id, cx);
        let event = match polled {
            Poll::Pending => {
                let state = std::sync::Arc::clone(
                    &self
                        .instance
                        .host_stream
                        .as_ref()
                        .expect("callable stream continuation exists")
                        .operation_state,
                );
                match state.take_event() {
                    Ok(Some(event)) => event,
                    Ok(None) => return Poll::Pending,
                    Err(error) => return Poll::Ready(Err(error)),
                }
            }
            Poll::Ready(Err(error)) => {
                let error = VmError::HostError(error.to_string());
                return Poll::Ready(Err(self
                    .abort_callable_stream_after_registry_poll(&error)
                    .err()
                    .unwrap_or(error)));
            }
            Poll::Ready(Ok(crate::vm::operation::OperationOutcome::Completed)) => {
                let state = std::sync::Arc::clone(
                    &self
                        .instance
                        .host_stream
                        .as_ref()
                        .expect("callable stream continuation exists")
                        .operation_state,
                );
                match state.take_event() {
                    Ok(Some(event)) => event,
                    Ok(None) => {
                        let error = VmError::InvalidFrameState(
                            "completed callable stream produced no terminal event",
                        );
                        return Poll::Ready(Err(self
                            .abort_callable_stream_after_registry_poll(&error)
                            .err()
                            .unwrap_or(error)));
                    }
                    Err(error) => return Poll::Ready(Err(error)),
                }
            }
            Poll::Ready(Ok(crate::vm::operation::OperationOutcome::Failed(failure))) => {
                let error = VmError::HostError(failure.message().to_string());
                return Poll::Ready(Err(self
                    .abort_callable_stream_after_registry_poll(&error)
                    .err()
                    .unwrap_or(error)));
            }
            Poll::Ready(Ok(crate::vm::operation::OperationOutcome::Cancelled(reason))) => {
                let error =
                    VmError::HostError(format!("callable stream operation cancelled ({reason})"));
                return Poll::Ready(Err(self
                    .abort_callable_stream_after_registry_poll(&error)
                    .err()
                    .unwrap_or(error)));
            }
        };

        match event {
            HostStreamPoll::Complete(summary) => {
                Poll::Ready(self.finish_callable_stream_after_registry_poll(summary))
            }
            HostStreamPoll::Item(item) => {
                self.instance.waiting_host_op = None;
                if let Some(stream) = self.instance.host_stream.as_mut() {
                    stream.phase = HostStreamPhase::RunCallback;
                    stream.item = Some(item);
                }
                match self.start_callable_stream_callback() {
                    Ok(VmStatus::Halted) => match self.finish_callable_stream_callback(Some(cx)) {
                        Ok(VmStatus::Halted) => Poll::Ready(Ok(())),
                        Ok(VmStatus::Waiting(_)) => Poll::Pending,
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
        self.finish_callable_stream_callback(None)
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

    fn finish_callable_stream_callback(
        &mut self,
        cx: Option<&mut Context<'_>>,
    ) -> VmResult<VmStatus> {
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
        let operation_state = std::sync::Arc::clone(
            &self
                .instance
                .host_stream
                .as_ref()
                .ok_or(VmError::InvalidFrameState(
                    "missing callable stream continuation",
                ))?
                .operation_state,
        );
        operation_state.set_action(action)?;
        if let Some(stream) = self.instance.host_stream.as_mut() {
            stream.phase = HostStreamPhase::AwaitItem;
        }
        self.instance.waiting_host_op = Some(super::WaitingHostOp {
            op_id,
            // Callable-stream items are not host-import resource returns; they
            // keep the legacy policy (the stream poll path never runs exact-
            // return validation).
            exact_policy: super::host::ExactHostReturnPolicy::Legacy,
        });
        let driven = if let Some(cx) = cx {
            self.drive_callable_stream_action(op_id, cx)
        } else {
            let waker = super::noop_waker();
            let mut cx = Context::from_waker(&waker);
            self.drive_callable_stream_action(op_id, &mut cx)
        };
        match driven {
            Poll::Pending => Ok(VmStatus::Waiting(op_id)),
            Poll::Ready(Ok(())) => Ok(VmStatus::Halted),
            Poll::Ready(Err(error)) => Err(error),
        }
    }

    fn drive_callable_stream_action(
        &mut self,
        op_id: HostOpId,
        cx: &mut Context<'_>,
    ) -> Poll<VmResult<()>> {
        let scope_id = crate::vm::operation::OperationId::from_raw(op_id)
            .expect("callable stream op id is a packed scope id");
        match self.host.execution_scope_poll_operation(scope_id, cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => {
                let error = VmError::HostError(error.to_string());
                Poll::Ready(Err(self
                    .abort_callable_stream_after_registry_poll(&error)
                    .err()
                    .unwrap_or(error)))
            }
            Poll::Ready(Ok(crate::vm::operation::OperationOutcome::Completed)) => {
                let state = std::sync::Arc::clone(
                    &self
                        .instance
                        .host_stream
                        .as_ref()
                        .expect("callable stream continuation exists")
                        .operation_state,
                );
                match state.take_event() {
                    Ok(Some(HostStreamPoll::Complete(summary))) => {
                        Poll::Ready(self.finish_callable_stream_after_registry_poll(summary))
                    }
                    Ok(Some(HostStreamPoll::Item(_))) => {
                        let error = VmError::InvalidFrameState(
                            "callable stream action application polled the producer",
                        );
                        Poll::Ready(Err(self
                            .abort_callable_stream_after_registry_poll(&error)
                            .err()
                            .unwrap_or(error)))
                    }
                    Ok(None) => {
                        let error = VmError::InvalidFrameState(
                            "completed callable stream action produced no summary",
                        );
                        Poll::Ready(Err(self
                            .abort_callable_stream_after_registry_poll(&error)
                            .err()
                            .unwrap_or(error)))
                    }
                    Err(error) => Poll::Ready(Err(error)),
                }
            }
            Poll::Ready(Ok(crate::vm::operation::OperationOutcome::Failed(failure))) => {
                let error = VmError::HostError(failure.message().to_string());
                Poll::Ready(Err(self
                    .abort_callable_stream_after_registry_poll(&error)
                    .err()
                    .unwrap_or(error)))
            }
            Poll::Ready(Ok(crate::vm::operation::OperationOutcome::Cancelled(reason))) => {
                let error =
                    VmError::HostError(format!("callable stream operation cancelled ({reason})"));
                Poll::Ready(Err(self
                    .abort_callable_stream_after_registry_poll(&error)
                    .err()
                    .unwrap_or(error)))
            }
        }
    }

    fn finish_callable_stream_after_registry_poll(&mut self, summary: Value) -> VmResult<()> {
        let Some(stream) = self.instance.host_stream.take() else {
            return Ok(());
        };
        self.retire_callable_stream(stream, CallableStreamRetirement::Polled)?;
        self.instance.stack.push(summary);
        Ok(())
    }

    fn abort_callable_stream_after_registry_poll(&mut self, _failure: &VmError) -> VmResult<()> {
        let Some(stream) = self.instance.host_stream.take() else {
            return Ok(());
        };
        let parent_stack_base = stream.parent_stack_base;
        let parent_frame_count = stream.parent_frame_count;
        let retired = self.retire_callable_stream(stream, CallableStreamRetirement::Polled);
        self.abort_host_invocation(parent_stack_base, parent_frame_count);
        retired
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
        for value in stream.operation_state.drain_values()? {
            self.drop_value_with_contract(value);
        }
        retired.map(|_| ())
    }
}

/// The sole owner and polling authority for a callable-stream producer.
/// VM continuation state retains only the packed operation id, callback state,
/// and the operation-owned event/action adapter. Producer polling, callback
/// action application, deadlines, cancellation, terminal transition, and final
/// producer drop all pass through this registered operation.
struct StreamScopeOperation {
    driver: Box<dyn HostStreamDriver>,
    state: std::sync::Arc<StreamOperationState>,
}

impl crate::vm::operation::HostOperation for StreamScopeOperation {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<crate::vm::operation::OperationResult<()>> {
        if let Some(action) = match self.state.take_action() {
            Ok(action) => action,
            Err(error) => return Poll::Ready(Err(error)),
        } {
            match self.driver.apply_action(action) {
                Ok(HostStreamAction::Continue) => {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Ok(HostStreamAction::Complete(summary)) => {
                    return Poll::Ready(
                        self.state.publish_event(HostStreamPoll::Complete(summary)),
                    );
                }
                Err(error) => return Poll::Ready(Err(stream_vm_error(error))),
            }
        }

        match self.state.has_event() {
            Ok(true) => return Poll::Pending,
            Ok(false) => {}
            Err(error) => return Poll::Ready(Err(error)),
        }

        match self.driver.poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(HostStreamPoll::Item(item))) => {
                match self.state.publish_event(HostStreamPoll::Item(item)) {
                    Ok(()) => Poll::Pending,
                    Err(error) => Poll::Ready(Err(error)),
                }
            }
            Poll::Ready(Ok(HostStreamPoll::Complete(summary))) => {
                Poll::Ready(self.state.publish_event(HostStreamPoll::Complete(summary)))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(stream_vm_error(error))),
        }
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

fn stream_vm_error(error: VmError) -> crate::vm::operation::OperationError {
    let message = match error {
        VmError::HostError(message) => message,
        other => other.to_string(),
    };
    stream_operation_error(message)
}

fn stream_operation_error(message: impl Into<String>) -> crate::vm::operation::OperationError {
    crate::vm::operation::OperationError::new(
        crate::vm::operation::OperationErrorCode::OperationDriverFailed,
        "vm::callable-stream",
        message,
    )
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
