use std::task::{Context, Poll};

use super::HostVmCompletion;
use crate::compiler::TypeSchema;
use crate::vm::execution_scope::{ExecutionScope, ExecutionScopeError};
use crate::vm::operation::OperationCancelReason;
use crate::vm::{CallOutcome, HostOpId, Value, Vm, VmError, VmResult, VmStatus};

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostStreamCallback {
    Event,
    Open,
}

/// The result of one host-side producer poll for a callable stream.
///
/// This is a host-only embedding extension point. It does not expose a stream
/// handle or polling operation to scripts. A [`HostStreamDriver::poll_next`]
/// call may yield at most one `Item`; the VM serializes that item with its
/// script callback before polling the producer again.
#[allow(dead_code)]
pub(crate) enum HostStreamPoll {
    /// Deliver one producer item to the script callback.
    Item(Value),
    /// Host-only positional arguments routed to exactly one callback.
    Call(HostStreamCallback, Vec<Value>),
    /// Materialize callback arguments on the VM thread (e.g. keyed resources).
    CallWithVm(HostStreamCallback, HostVmCompletion<Vec<Value>>),
    /// Finish the stream and return the supplied summary to the script call.
    Complete(Value),
    /// Materialize the terminal value on the VM thread after termination.
    CompleteWithVm(HostVmCompletion<Value>),
}

/// The host driver's response to one completed script callback.
///
/// Values returned by the callback remain inside the host embedding boundary:
/// no action handle is exposed to scripts.
#[allow(dead_code)]
pub(crate) enum HostStreamAction {
    /// Continue by returning control to producer polling.
    Continue,
    /// Cancel the producer after returning the supplied final value. This is
    /// distinct from normal completion because the producer may still be
    /// blocked publishing the item whose callback requested the stop.
    Cancel(Value, OperationCancelReason),
    CancelWithVm(HostVmCompletion<Value>, OperationCancelReason),
}

enum HostStreamSummary {
    Value(Value),
    WithVm(HostVmCompletion<Value>),
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
/// The single-item entry point validates the legacy map/Named callback
/// contract. [`Vm::submit_callable_stream_callbacks`] additionally validates
/// each selected callable against its exact positional parameter schema and
/// `bool` result. The producer selects the callback for each item; it cannot
/// call the VM directly or expose a stream handle to scripts.
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
    AwaitTermination,
    ExecutingCompletion,
    RollbackCompletion,
}

pub(crate) struct HostStreamContinuation {
    pub(crate) op_id: HostOpId,
    pub(crate) callback: Value,
    pub(crate) open_callback: Option<Value>,
    pub(crate) item: Option<Vec<Value>>,
    pub(crate) item_callback: HostStreamCallback,
    summary: Option<HostStreamSummary>,
    rollback: Option<(
        std::collections::HashSet<crate::vm::resource::ResourceHandle>,
        VmError,
        Option<crate::vm::resource::ResourceError>,
    )>,
    pub(crate) phase: HostStreamPhase,
    pub(crate) parent_stack_base: usize,
    pub(crate) parent_frame_count: usize,
    pub(crate) parent_ip: usize,
}

/// HTTP SSE callback results retain the existing named action/object runtime
/// compatibility, while callback inputs use the exact `SseEvent` named schema.
#[cfg(feature = "http-client")]
fn sse_callback_input_schema(params: &[TypeSchema]) -> bool {
    matches!(
        params,
        [TypeSchema::Named(name, args)] if name == "SseEvent" && args.is_empty()
    )
}

#[cfg(feature = "http-client")]
fn sse_callback_action_result_schema(result: &TypeSchema) -> bool {
    match result {
        TypeSchema::Named(name, args) => name == "SseCallbackAction" && args.is_empty(),
        TypeSchema::Object(fields) => {
            fields.len() == 1
                && fields
                    .get("action")
                    .is_some_and(|ty| matches!(ty, TypeSchema::String))
        }
        _ => false,
    }
}

impl Vm {
    /// Installs a host-only callable stream and suspends the current VM call.
    ///
    /// This Rust embedding API does not create a script-visible handle. The VM
    /// Validates the legacy single-argument callable and map/Named action
    /// contract. Positional bool callbacks use
    /// [`Self::submit_callable_stream_callbacks`].
    /// The VM then owns the callback and driver until completion, cancellation,
    /// reset, or error; removing the driver drops it to release producer
    /// resources.
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
        self.install_callable_stream(callback, None, driver)
    }

    /// Host-only positional callback stream. The open callback may be absent;
    /// an open item then fails rather than calling the event callback.
    #[allow(dead_code)]
    pub(crate) fn submit_callable_stream_callbacks(
        &mut self,
        on_event: Value,
        on_open: Option<Value>,
        event_params: &[TypeSchema],
        open_params: &[TypeSchema],
        driver: impl HostStreamDriver,
    ) -> Result<CallOutcome, HostStreamAdmissionError> {
        let validation = self
            .validate_stream_positional_callback(&on_event, event_params)
            .and_then(|()| match on_open.as_ref() {
                Some(callback) => self.validate_stream_positional_callback(callback, open_params),
                None => Ok(()),
            });
        if let Err(primary) = validation {
            return Err(HostStreamAdmissionError {
                primary,
                rollback: HostStreamAdmissionRollback {
                    driver: Box::new(driver),
                    termination: HostStreamTermination::Cancelled(OperationCancelReason::Requested),
                },
            });
        }
        self.install_callable_stream(on_event, on_open, driver)
    }

    fn install_callable_stream(
        &mut self,
        callback: Value,
        open_callback: Option<Value>,
        driver: impl HostStreamDriver,
    ) -> Result<CallOutcome, HostStreamAdmissionError> {
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
            open_callback,
            item: None,
            item_callback: HostStreamCallback::Event,
            summary: None,
            rollback: None,
            phase: HostStreamPhase::AwaitItem,
            parent_stack_base: self.instance.stack.len(),
            parent_frame_count: self.instance.execution_frames.len(),
            parent_ip: self.instance.ip,
        });
        Ok(CallOutcome::Pending(op_id))
    }

    pub(crate) fn validate_stream_positional_callback(
        &self,
        callback: &Value,
        expected_params: &[TypeSchema],
    ) -> VmResult<()> {
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
        if usize::from(prototype.arity) != expected_params.len() {
            return Err(VmError::CallableArityMismatch {
                prototype_id: callable.prototype_id,
                expected: expected_params.len() as u8,
                got: prototype.arity,
            });
        }
        if !matches!(&prototype.schema,
            Some(TypeSchema::Callable { params, result })
                if params == expected_params && result.as_ref() == &TypeSchema::Bool)
        {
            return Err(VmError::TypeMismatch(
                "callable stream callback parameter or bool result schema",
            ));
        }
        Ok(())
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
            && (!matches!(
                params.as_slice(),
                [TypeSchema::Map(_)] | [TypeSchema::Named(_, _)]
            ) || !matches!(
                result.as_ref(),
                TypeSchema::Map(_) | TypeSchema::Named(_, _) | TypeSchema::Object(_)
            ))
        {
            return Err(VmError::TypeMismatch(
                "callable stream callback must accept one map or named input and return a map, named value, or object",
            ));
        }
        Ok(())
    }

    #[cfg(feature = "http-client")]
    pub fn validate_sse_callback_value(&self, callback: &Value) -> VmResult<()> {
        self.validate_stream_callback_value(callback)?;
        let Value::Callable(callable) = callback else {
            return Ok(());
        };
        let Some(prototype) = self
            .program
            .callable_prototypes
            .get(callable.prototype_id as usize)
        else {
            return Ok(());
        };
        if let Some(TypeSchema::Callable { params, result, .. }) = &prototype.schema
            && (!sse_callback_input_schema(params) || !sse_callback_action_result_schema(result))
        {
            return Err(VmError::TypeMismatch("fn(SseEvent) -> SseCallbackAction"));
        }
        Ok(())
    }

    pub(crate) fn cancel_callable_stream_with_reason(
        &mut self,
        reason: OperationCancelReason,
    ) -> VmResult<()> {
        if self.stream_completion_running() {
            return Err(VmError::InvalidFrameState(
                "callable stream completion is running",
            ));
        }
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
            for value in item {
                self.drop_value_with_contract(value);
            }
        }
        self.drop_value_with_contract(stream.callback);
        if let Some(open) = stream.open_callback {
            self.drop_value_with_contract(open);
        }
        cleanup
    }

    pub(crate) fn stream_completion_running(&self) -> bool {
        self.instance
            .host_stream
            .as_ref()
            .is_some_and(|stream| stream.phase == HostStreamPhase::ExecutingCompletion)
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
            == Some(HostStreamPhase::RollbackCompletion)
        {
            return self.poll_failed_stream_completion(cx);
        }
        if self
            .instance
            .host_stream
            .as_ref()
            .map(|stream| stream.phase)
            == Some(HostStreamPhase::AwaitTermination)
        {
            return self.poll_callable_stream_completion(cx);
        }
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
                match self.finish_callable_stream_with_termination(
                    HostStreamSummary::Value(summary),
                    HostStreamTermination::Completed,
                ) {
                    Ok(true) => Poll::Ready(Ok(())),
                    Ok(false) => Poll::Pending,
                    Err(error) => Poll::Ready(Err(error)),
                }
            }
            Poll::Ready(Ok(HostStreamPoll::CompleteWithVm(completion))) => {
                match self.finish_callable_stream_with_termination(
                    HostStreamSummary::WithVm(completion),
                    HostStreamTermination::Completed,
                ) {
                    Ok(true) => Poll::Ready(Ok(())),
                    Ok(false) => Poll::Pending,
                    Err(error) => Poll::Ready(Err(error)),
                }
            }
            Poll::Ready(Ok(
                item @ (HostStreamPoll::Item(_)
                | HostStreamPoll::Call(_, _)
                | HostStreamPoll::CallWithVm(_, _)),
            )) => {
                let mut materialized_before = None;
                let (kind, args) = match item {
                    HostStreamPoll::Item(value) => (HostStreamCallback::Event, vec![value]),
                    HostStreamPoll::Call(kind, args) => (kind, args),
                    HostStreamPoll::CallWithVm(kind, materialize) => {
                        let before = match self.host.execution_scope.resources_mut().live_handles()
                        {
                            Ok(handles) => handles,
                            Err(error) => {
                                let primary =
                                    VmError::ExecutionScope(ExecutionScopeError::Resource(error));
                                return Poll::Ready(Err(preserve_stream_cleanup(
                                    primary,
                                    self.abort_callable_stream(),
                                )));
                            }
                        };
                        if let Some(stream) = self.instance.host_stream.as_mut() {
                            stream.phase = HostStreamPhase::ExecutingCompletion;
                        }
                        let result = materialize(self);
                        if let Some(stream) = self.instance.host_stream.as_mut() {
                            stream.phase = HostStreamPhase::AwaitItem;
                        }
                        match result {
                            Ok(args) => {
                                materialized_before = Some(before);
                                (kind, args)
                            }
                            Err(error) => {
                                let cleanup = self
                                    .host
                                    .begin_stream_termination(
                                        op_id,
                                        HostStreamTermination::Cancelled(
                                            OperationCancelReason::Requested,
                                        ),
                                    )
                                    .and_then(|()| self.poll_stream_termination_once());
                                let stream =
                                    self.instance.host_stream.as_mut().expect("stream exists");
                                stream.rollback =
                                    Some((before, preserve_stream_cleanup(error, cleanup), None));
                                stream.phase = HostStreamPhase::RollbackCompletion;
                                return self.poll_failed_stream_completion(cx);
                            }
                        }
                    }
                    _ => unreachable!(),
                };
                self.instance.waiting_host_op = None;
                if let Some(stream) = self.instance.host_stream.as_mut() {
                    stream.phase = HostStreamPhase::RunCallback;
                    stream.item_callback = kind;
                    stream.item = Some(args);
                }
                let mut callback_entered = false;
                match self.start_callable_stream_callback(&mut callback_entered) {
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
                        if !callback_entered {
                            if let Some(before) = materialized_before {
                                let cleanup = self
                                    .host
                                    .begin_stream_termination(
                                        op_id,
                                        HostStreamTermination::Cancelled(
                                            OperationCancelReason::Requested,
                                        ),
                                    )
                                    .and_then(|()| self.poll_stream_termination_once());
                                let stream =
                                    self.instance.host_stream.as_mut().expect("stream exists");
                                stream.rollback =
                                    Some((before, preserve_stream_cleanup(error, cleanup), None));
                                stream.phase = HostStreamPhase::RollbackCompletion;
                                self.poll_failed_stream_completion(cx)
                            } else {
                                let cleanup = self.abort_callable_stream();
                                Poll::Ready(Err(preserve_stream_cleanup(error, cleanup)))
                            }
                        } else {
                            let cleanup = self.abort_callable_stream();
                            Poll::Ready(Err(preserve_stream_cleanup(error, cleanup)))
                        }
                    }
                }
            }
        }
    }

    fn start_callable_stream_callback(&mut self, entered: &mut bool) -> VmResult<VmStatus> {
        let (callback, item) = {
            let stream = self
                .instance
                .host_stream
                .as_mut()
                .ok_or(VmError::InvalidFrameState(
                    "missing callable stream continuation",
                ))?;
            (
                match stream.item_callback {
                    HostStreamCallback::Event => stream.callback.clone(),
                    HostStreamCallback::Open => stream
                        .open_callback
                        .clone()
                        .ok_or(VmError::InvalidFrameState("open item without callback"))?,
                },
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
            item,
            operand_stack_base,
            None,
            crate::vm::instance::FrameContinuation::ReturnToHost,
        )?;
        *entered = true;
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
                    HostStreamSummary::Value(summary),
                    HostStreamTermination::Cancelled(reason),
                ) {
                    Ok(true) => Ok(VmStatus::Halted),
                    Ok(false) => Ok(VmStatus::Waiting(op_id)),
                    Err(error) => Err(error),
                }
            }
            Ok(HostStreamAction::CancelWithVm(completion, reason)) => {
                match self.finish_callable_stream_with_termination(
                    HostStreamSummary::WithVm(completion),
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

    fn finish_callable_stream_with_termination(
        &mut self,
        summary: HostStreamSummary,
        termination: HostStreamTermination,
    ) -> VmResult<bool> {
        if let HostStreamSummary::Value(value) = summary {
            return self.finish_callable_stream_value(value, termination);
        }
        let op_id = self
            .instance
            .host_stream
            .as_ref()
            .ok_or(VmError::InvalidFrameState(
                "missing callable stream continuation",
            ))?
            .op_id;
        if let Err(error) = self.host.begin_stream_termination(op_id, termination) {
            return Err(preserve_stream_cleanup(error, self.abort_callable_stream()));
        }
        let stream = self
            .instance
            .host_stream
            .as_mut()
            .expect("stream checked above");
        stream.summary = Some(summary);
        stream.phase = HostStreamPhase::AwaitTermination;
        self.instance.waiting_host_op = Some(crate::vm::host::WaitingHostOp {
            op_id,
            source: crate::vm::host::WaitingHostOpSource::CallableStream,
            expected_return_type: None,
            expected_return_schema: None,
        });
        let mut cx = Context::from_waker(std::task::Waker::noop());
        match self.poll_callable_stream_completion(&mut cx) {
            Poll::Ready(result) => result.map(|()| true),
            Poll::Pending => Ok(false),
        }
    }

    fn poll_callable_stream_completion(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<()>> {
        match self.host.poll_stream_terminations(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => {
                let cleanup = self.abort_callable_stream();
                Poll::Ready(Err(preserve_stream_cleanup(error, cleanup)))
            }
            Poll::Ready(Ok(())) => {
                let before = match self.host.execution_scope.resources_mut().live_handles() {
                    Ok(handles) => handles,
                    Err(error) => {
                        let error = VmError::ExecutionScope(
                            crate::vm::execution_scope::ExecutionScopeError::Resource(error),
                        );
                        self.host.mark_reset_failed(&error);
                        return Poll::Ready(Err(error));
                    }
                };
                let stream = self.instance.host_stream.as_mut().expect("stream exists");
                let summary = stream.summary.take().expect("terminal summary exists");
                let result = match summary {
                    HostStreamSummary::Value(value) => Ok(value),
                    HostStreamSummary::WithVm(completion) => {
                        self.instance
                            .host_stream
                            .as_mut()
                            .expect("stream exists")
                            .phase = HostStreamPhase::ExecutingCompletion;
                        let result = completion(self);
                        self.instance
                            .host_stream
                            .as_mut()
                            .expect("stream exists")
                            .phase = HostStreamPhase::AwaitTermination;
                        result
                    }
                };
                match result {
                    Ok(summary) => {
                        let stream = self.instance.host_stream.take().expect("stream exists");
                        self.instance.waiting_host_op = None;
                        self.instance.stack.push(summary);
                        self.drop_value_with_contract(stream.callback);
                        if let Some(open) = stream.open_callback {
                            self.drop_value_with_contract(open);
                        }
                        if let Some(items) = stream.item {
                            for item in items {
                                self.drop_value_with_contract(item);
                            }
                        }
                        Poll::Ready(Ok(()))
                    }
                    Err(error) => {
                        let stream = self.instance.host_stream.as_mut().expect("stream exists");
                        stream.rollback = Some((before, error, None));
                        stream.phase = HostStreamPhase::RollbackCompletion;
                        self.poll_failed_stream_completion(cx)
                    }
                }
            }
        }
    }

    fn poll_failed_stream_completion(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<()>> {
        let (before, _, cleanup_error) = self
            .instance
            .host_stream
            .as_mut()
            .and_then(|stream| stream.rollback.as_mut())
            .expect("rollback phase has snapshot and error");
        let result =
            self.host
                .execution_scope
                .resources_mut()
                .poll_close_added(before, cleanup_error, cx);
        match result {
            Poll::Pending => Poll::Pending,
            Poll::Ready(cleanup) => {
                let mut stream = self.instance.host_stream.take().expect("stream exists");
                let (_, error, _) = stream.rollback.take().expect("rollback error exists");
                self.instance.waiting_host_op = None;
                self.abort_host_invocation(stream.parent_stack_base, stream.parent_frame_count);
                self.drop_value_with_contract(stream.callback);
                if let Some(open) = stream.open_callback {
                    self.drop_value_with_contract(open);
                }
                if let Some(items) = stream.item {
                    for item in items {
                        self.drop_value_with_contract(item);
                    }
                }
                let cleanup = cleanup.map_err(|error| {
                    VmError::ExecutionScope(
                        crate::vm::execution_scope::ExecutionScopeError::Resource(error),
                    )
                });
                if let Err(cleanup_error) = &cleanup {
                    self.host.mark_reset_failed(cleanup_error);
                }
                Poll::Ready(Err(preserve_stream_cleanup(error, cleanup)))
            }
        }
    }

    fn finish_callable_stream_value(
        &mut self,
        summary: Value,
        termination: HostStreamTermination,
    ) -> VmResult<bool> {
        let stream = self
            .instance
            .host_stream
            .take()
            .ok_or(VmError::InvalidFrameState(
                "missing callable stream continuation",
            ))?;
        let cleanup = self
            .host
            .begin_stream_termination(stream.op_id, termination)
            .and_then(|()| self.poll_stream_termination_once());
        self.instance.waiting_host_op = None;
        self.drop_value_with_contract(stream.callback);
        if let Some(open) = stream.open_callback {
            self.drop_value_with_contract(open);
        }
        if let Some(items) = stream.item {
            for item in items {
                self.drop_value_with_contract(item);
            }
        }
        if let Err(error) = cleanup {
            self.abort_host_invocation(stream.parent_stack_base, stream.parent_frame_count);
            self.drop_value_with_contract(summary);
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
        if let Some(open) = stream.open_callback {
            self.drop_value_with_contract(open);
        }
        if let Some(item) = stream.item {
            for value in item {
                self.drop_value_with_contract(value);
            }
        }
        cleanup
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile_source;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    struct ScriptedDriver {
        items: VecDeque<HostStreamPoll>,
        actions: Arc<Mutex<Vec<Value>>>,
    }

    impl HostStreamDriver for ScriptedDriver {
        fn poll_next(&mut self, _cx: &mut Context<'_>) -> Poll<VmResult<HostStreamPoll>> {
            Poll::Ready(Ok(self
                .items
                .pop_front()
                .expect("producer polled after completion")))
        }

        fn apply_action(&mut self, action: Value) -> VmResult<HostStreamAction> {
            self.actions.lock().unwrap().push(action);
            Ok(HostStreamAction::Continue)
        }
    }

    #[test]
    fn multi_argument_events_dispatch_only_the_selected_callback_in_order() {
        let program = compile_source(
            r#"
            pub fn event(kind: string, data: string, id: int) -> bool {
                kind == "message" && data == "payload" && id == 7
            }
            pub fn open(status: int, url: string) -> bool {
                status == 200 && url == "https://example.test"
            }
            "#,
        )
        .unwrap()
        .program;
        let mut vm = Vm::new(program);
        assert_eq!(vm.run().unwrap(), VmStatus::Halted);
        let event = vm.resolve_exported_callable("event").unwrap();
        let open = vm.resolve_exported_callable("open").unwrap();
        let actions = Arc::new(Mutex::new(Vec::new()));
        let driver = ScriptedDriver {
            items: VecDeque::from([
                HostStreamPoll::Call(
                    HostStreamCallback::Open,
                    vec![Value::Int(200), Value::string("https://example.test")],
                ),
                HostStreamPoll::Call(
                    HostStreamCallback::Event,
                    vec![
                        Value::string("message"),
                        Value::string("payload"),
                        Value::Int(7),
                    ],
                ),
                HostStreamPoll::Complete(Value::Int(42)),
            ]),
            actions: Arc::clone(&actions),
        };
        let CallOutcome::Pending(id) = vm
            .submit_callable_stream_callbacks(
                event,
                Some(open),
                &[TypeSchema::String, TypeSchema::String, TypeSchema::Int],
                &[TypeSchema::Int, TypeSchema::String],
                driver,
            )
            .unwrap_or_else(|err| panic!("stream admission: {}", err.primary))
        else {
            panic!("stream must suspend")
        };
        let mut cx = Context::from_waker(std::task::Waker::noop());
        for _ in 0..2 {
            let poll = vm.poll_callable_stream(id, &mut cx);
            assert!(matches!(poll, Poll::Pending), "callback poll: {poll:?}");
        }
        assert_eq!(
            *actions.lock().unwrap(),
            vec![Value::Bool(true), Value::Bool(true)]
        );
        assert!(matches!(
            vm.poll_callable_stream(id, &mut cx),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(vm.stack().last(), Some(&Value::Int(42)));
    }

    #[test]
    fn positional_admission_validates_both_real_arities_and_schemas() {
        let mut vm = Vm::new(
            compile_source(
                r#"
            pub fn event(event: string, data: string) -> bool { true }
            pub fn open(status: int, url: string) -> int { status }
        "#,
            )
            .unwrap()
            .program,
        );
        assert_eq!(vm.run().unwrap(), VmStatus::Halted);
        let event = vm.resolve_exported_callable("event").unwrap();
        let open = vm.resolve_exported_callable("open").unwrap();
        let driver = || ScriptedDriver {
            items: VecDeque::new(),
            actions: Arc::new(Mutex::new(vec![])),
        };
        let rejection = vm
            .submit_callable_stream_callbacks(
                event.clone(),
                None,
                &[TypeSchema::String],
                &[],
                driver(),
            )
            .err()
            .expect("arity rejection");
        assert!(matches!(
            rejection.primary,
            VmError::CallableArityMismatch { .. }
        ));
        vm.rollback_rejected_callable_stream(rejection);
        let rejection = vm
            .submit_callable_stream_callbacks(
                event.clone(),
                Some(open),
                &[TypeSchema::String, TypeSchema::String],
                &[TypeSchema::Int, TypeSchema::String],
                driver(),
            )
            .err()
            .expect("open result rejection");
        assert!(matches!(rejection.primary, VmError::TypeMismatch(_)));
        vm.rollback_rejected_callable_stream(rejection);
        let rejection = vm
            .submit_callable_stream_callbacks(
                event,
                None,
                &[TypeSchema::Int, TypeSchema::String],
                &[],
                driver(),
            )
            .err()
            .expect("event parameter rejection");
        assert!(matches!(rejection.primary, VmError::TypeMismatch(_)));
        vm.rollback_rejected_callable_stream(rejection);
    }

    #[test]
    fn failed_vm_argument_materialization_rolls_back_new_resources() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let mut vm = Vm::new(
            compile_source("pub fn event(x: int) -> bool { true }")
                .unwrap()
                .program,
        );
        assert_eq!(vm.run().unwrap(), VmStatus::Halted);
        let callback = vm.resolve_exported_callable("event").unwrap();
        let existing = vm.host_context().push_resource(SummaryResource).unwrap();
        let ready = Arc::new(AtomicBool::new(false));
        let closes = Arc::new(AtomicUsize::new(0));
        let driver = ScriptedDriver {
            items: VecDeque::from([HostStreamPoll::CallWithVm(
                HostStreamCallback::Event,
                Box::new({
                    let ready = Arc::clone(&ready);
                    let closes = Arc::clone(&closes);
                    move |vm| {
                        vm.host_context()
                            .push_resource(RollbackResource { ready, closes })
                            .unwrap();
                        Err(VmError::HostError("argument failed".into()))
                    }
                }),
            )]),
            actions: Arc::new(Mutex::new(vec![])),
        };
        let id = match vm
            .submit_callable_stream_callbacks(callback, None, &[TypeSchema::Int], &[], driver)
            .unwrap_or_else(|error| panic!("{}", error.primary))
        {
            CallOutcome::Pending(id) => id,
            _ => panic!("pending expected"),
        };
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            vm.poll_callable_stream(id, &mut cx),
            Poll::Pending
        ));
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        ready.store(true, Ordering::SeqCst);
        assert!(
            matches!(vm.poll_callable_stream(id, &mut cx), Poll::Ready(Err(VmError::HostError(message))) if message == "argument failed")
        );
        assert!(vm.instance.host_stream.is_none());
        assert_eq!(vm.host_context().resource_count(), 1);
        assert!(vm.host_context().resource(&existing).is_ok());
    }

    #[test]
    fn failed_callback_entry_after_vm_materialization_rolls_back_new_resources() {
        let mut vm = Vm::new(
            compile_source("pub fn event(x: int) -> bool { true }")
                .unwrap()
                .program,
        );
        assert_eq!(vm.run().unwrap(), VmStatus::Halted);
        let callback = vm.resolve_exported_callable("event").unwrap();
        let existing = vm.host_context().push_resource(SummaryResource).unwrap();
        let driver = ScriptedDriver {
            items: VecDeque::from([HostStreamPoll::CallWithVm(
                HostStreamCallback::Event,
                Box::new(|vm| {
                    vm.host_context().push_resource(SummaryResource).unwrap();
                    // Callback entry rejects the wrong argument type after materialization.
                    Ok(vec![Value::string("wrong type")])
                }),
            )]),
            actions: Arc::new(Mutex::new(vec![])),
        };
        let CallOutcome::Pending(id) = vm
            .submit_callable_stream_callbacks(callback, None, &[TypeSchema::Int], &[], driver)
            .unwrap_or_else(|error| panic!("{}", error.primary))
        else {
            panic!("pending expected")
        };
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            vm.poll_callable_stream(id, &mut cx),
            Poll::Ready(Err(VmError::TypeMismatch(_)))
        ));
        assert!(vm.instance.host_stream.is_none());
        assert_eq!(vm.host_context().resource_count(), 1);
        assert!(vm.host_context().resource(&existing).is_ok());
    }

    #[test]
    fn callback_failure_after_entry_leaves_materialized_resources_script_owned() {
        let mut vm = Vm::new(
            compile_source("pub fn event(x: int) -> bool { let zero = 0; x / zero > 0 }")
                .unwrap()
                .program,
        );
        assert_eq!(vm.run().unwrap(), VmStatus::Halted);
        let callback = vm.resolve_exported_callable("event").unwrap();
        let driver = ScriptedDriver {
            items: VecDeque::from([HostStreamPoll::CallWithVm(
                HostStreamCallback::Event,
                Box::new(|vm| {
                    vm.host_context().push_resource(SummaryResource).unwrap();
                    Ok(vec![Value::Int(1)])
                }),
            )]),
            actions: Arc::new(Mutex::new(vec![])),
        };
        let CallOutcome::Pending(id) = vm
            .submit_callable_stream_callbacks(callback, None, &[TypeSchema::Int], &[], driver)
            .unwrap_or_else(|error| panic!("{}", error.primary))
        else {
            panic!("pending expected")
        };
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            vm.poll_callable_stream(id, &mut cx),
            Poll::Ready(Err(_))
        ));
        assert_eq!(vm.host_context().resource_count(), 1);
    }

    #[test]
    fn vm_thread_callback_arguments_materialize_resources_before_dispatch() {
        let mut vm = Vm::new(compile_source("pub fn open(handle: int) -> bool { handle > 0 } pub fn event(x: int) -> bool { true }").unwrap().program);
        assert_eq!(vm.run().unwrap(), VmStatus::Halted);
        let open = vm.resolve_exported_callable("open").unwrap();
        let event = vm.resolve_exported_callable("event").unwrap();
        let actions = Arc::new(Mutex::new(vec![]));
        let driver = ScriptedDriver {
            items: VecDeque::from([
                HostStreamPoll::CallWithVm(
                    HostStreamCallback::Open,
                    Box::new(|vm| {
                        let resource = vm
                            .host_context()
                            .push_resource(SummaryResource)
                            .map_err(|error| VmError::HostError(error.to_string()))?;
                        Ok(vec![Value::Int(resource.handle().raw() as i64)])
                    }),
                ),
                HostStreamPoll::Complete(Value::Int(7)),
            ]),
            actions: Arc::clone(&actions),
        };
        let id = match vm
            .submit_callable_stream_callbacks(
                event,
                Some(open),
                &[TypeSchema::Int],
                &[TypeSchema::Int],
                driver,
            )
            .unwrap_or_else(|error| panic!("{}", error.primary))
        {
            CallOutcome::Pending(id) => id,
            _ => panic!("pending expected"),
        };
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            vm.poll_callable_stream(id, &mut cx),
            Poll::Pending
        ));
        assert_eq!(*actions.lock().unwrap(), vec![Value::Bool(true)]);
        assert!(matches!(
            vm.poll_callable_stream(id, &mut cx),
            Poll::Ready(Ok(()))
        ));
    }

    #[derive(Debug)]
    struct SummaryResource;
    impl crate::vm::resource::HostResource for SummaryResource {
        fn resource_type_key() -> Option<crate::host_api::ResourceTypeKey> {
            Some(crate::host_api::ResourceTypeKey::new("test.stream.summary").unwrap())
        }
    }

    struct DelayedDriver {
        item: Option<HostStreamPoll>,
        ready: Arc<std::sync::atomic::AtomicBool>,
        cancel_on_action: bool,
    }
    impl HostStreamDriver for DelayedDriver {
        fn poll_next(&mut self, _cx: &mut Context<'_>) -> Poll<VmResult<HostStreamPoll>> {
            Poll::Ready(Ok(self.item.take().expect("poll once")))
        }
        fn apply_action(&mut self, _action: Value) -> VmResult<HostStreamAction> {
            assert!(self.cancel_on_action);
            Ok(HostStreamAction::CancelWithVm(
                Box::new(|vm| {
                    let resource = vm
                        .host_context()
                        .push_resource(SummaryResource)
                        .map_err(|error| VmError::HostError(error.to_string()))?;
                    Ok(Value::Int(resource.handle().raw() as i64))
                }),
                OperationCancelReason::Requested,
            ))
        }
        fn poll_termination(
            &mut self,
            _scope: &mut ExecutionScope,
            _reason: HostStreamTermination,
            _cx: &mut Context<'_>,
        ) -> Poll<VmResult<()>> {
            if self.ready.load(std::sync::atomic::Ordering::SeqCst) {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
    }

    #[test]
    fn vm_completion_waits_for_termination_before_publishing_keyed_resource() {
        use std::sync::atomic::{AtomicBool, Ordering};
        for cancel in [false, true] {
            let mut vm = Vm::new(
                compile_source("pub fn event(x: int) -> bool { true }")
                    .unwrap()
                    .program,
            );
            assert_eq!(vm.run().unwrap(), VmStatus::Halted);
            let event = vm.resolve_exported_callable("event").unwrap();
            let ready = Arc::new(AtomicBool::new(false));
            let completion: HostVmCompletion<Value> = Box::new(|vm| {
                let resource = vm
                    .host_context()
                    .push_resource(SummaryResource)
                    .map_err(|error| VmError::HostError(error.to_string()))?;
                Ok(Value::Int(resource.handle().raw() as i64))
            });
            let item = if cancel {
                HostStreamPoll::Call(HostStreamCallback::Event, vec![Value::Int(1)])
            } else {
                HostStreamPoll::CompleteWithVm(completion)
            };
            let driver = DelayedDriver {
                item: Some(item),
                ready: Arc::clone(&ready),
                cancel_on_action: cancel,
            };
            let CallOutcome::Pending(id) = vm
                .submit_callable_stream_callbacks(event, None, &[TypeSchema::Int], &[], driver)
                .unwrap_or_else(|err| panic!("admission: {}", err.primary))
            else {
                panic!("pending expected")
            };
            let mut cx = Context::from_waker(std::task::Waker::noop());
            assert!(matches!(
                vm.poll_callable_stream(id, &mut cx),
                Poll::Pending
            ));
            assert_eq!(vm.host_context().resource_count(), 0);
            ready.store(true, Ordering::SeqCst);
            assert!(matches!(
                vm.poll_callable_stream(id, &mut cx),
                Poll::Ready(Ok(()))
            ));
            let Value::Int(raw) = vm.stack().last().expect("summary published") else {
                panic!("handle expected")
            };
            let handle = crate::vm::resource::ResourceHandle::from_raw(*raw as u64).unwrap();
            let key = crate::host_api::ResourceTypeKey::new("test.stream.summary").unwrap();
            assert!(
                vm.host_context()
                    .typed_resource_with_key::<SummaryResource>(handle, &key)
                    .is_ok()
            );
        }
    }

    #[test]
    fn reset_inside_vm_completion_is_rejected_without_disturbing_success() {
        use std::sync::atomic::AtomicBool;
        let mut vm = Vm::new(
            compile_source("pub fn event(x: int) -> bool { true }")
                .unwrap()
                .program,
        );
        assert_eq!(vm.run().unwrap(), VmStatus::Halted);
        let callback = vm.resolve_exported_callable("event").unwrap();
        let driver = DelayedDriver {
            item: Some(HostStreamPoll::CompleteWithVm(Box::new(|vm| {
                assert!(matches!(
                    vm.reset_for_reuse(),
                    Err(VmError::InvalidFrameState(
                        "callable stream completion is running"
                    ))
                ));
                assert!(matches!(
                    vm.cancel_callable_stream_with_reason(OperationCancelReason::Requested),
                    Err(VmError::InvalidFrameState(
                        "callable stream completion is running"
                    ))
                ));
                vm.shutdown();
                assert!(vm.instance.host_stream.is_some());
                assert!(vm.waiting_host_op_id().is_some());
                assert!(!vm.scope_reset_pending());
                let resource = vm.host_context().push_resource(SummaryResource).unwrap();
                Ok(Value::Int(resource.handle().raw() as i64))
            }))),
            ready: Arc::new(AtomicBool::new(true)),
            cancel_on_action: false,
        };
        let CallOutcome::Pending(id) = vm
            .submit_callable_stream_callbacks(callback, None, &[TypeSchema::Int], &[], driver)
            .unwrap_or_else(|err| panic!("admission: {}", err.primary))
        else {
            panic!("pending expected")
        };
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            vm.poll_callable_stream(id, &mut cx),
            Poll::Ready(Ok(()))
        ));
        assert!(vm.instance.host_stream.is_none());
        assert!(vm.host.stream_drivers.is_empty());
        assert_eq!(vm.host_context().resource_count(), 1);
        assert!(matches!(vm.stack().last(), Some(Value::Int(_))));
        vm.reset_for_reuse().unwrap();
        assert!(matches!(
            vm.poll_reset_for_reuse(&mut cx),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(vm.host_context().resource_count(), 0);
    }

    #[test]
    fn reset_inside_vm_completion_is_rejected_without_disturbing_error_rollback() {
        use std::sync::atomic::AtomicBool;
        let mut vm = Vm::new(
            compile_source("pub fn event(x: int) -> bool { true }")
                .unwrap()
                .program,
        );
        assert_eq!(vm.run().unwrap(), VmStatus::Halted);
        let existing = vm.host_context().push_resource(SummaryResource).unwrap();
        let callback = vm.resolve_exported_callable("event").unwrap();
        let driver = DelayedDriver {
            item: Some(HostStreamPoll::CompleteWithVm(Box::new(|vm| {
                assert!(matches!(
                    vm.reset_for_reuse(),
                    Err(VmError::InvalidFrameState(
                        "callable stream completion is running"
                    ))
                ));
                assert!(vm.instance.host_stream.is_some());
                vm.host_context().push_resource(SummaryResource).unwrap();
                Err(VmError::HostError("summary failed".into()))
            }))),
            ready: Arc::new(AtomicBool::new(true)),
            cancel_on_action: false,
        };
        let CallOutcome::Pending(id) = vm
            .submit_callable_stream_callbacks(callback, None, &[TypeSchema::Int], &[], driver)
            .unwrap_or_else(|err| panic!("admission: {}", err.primary))
        else {
            panic!("pending expected")
        };
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            vm.poll_callable_stream(id, &mut cx),
            Poll::Ready(Err(VmError::HostError(message))) if message == "summary failed"
        ));
        assert!(vm.instance.host_stream.is_none());
        assert!(vm.host.stream_drivers.is_empty());
        assert!(vm.stack().is_empty());
        assert_eq!(vm.host_context().resource_count(), 1);
        assert!(vm.host_context().resource(&existing).is_ok());
        vm.reset_for_reuse().unwrap();
        assert!(matches!(
            vm.poll_reset_for_reuse(&mut cx),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(vm.host_context().resource_count(), 0);
    }

    #[test]
    fn reset_waits_for_unscoped_stream_driver_and_fails_closed_on_error() {
        use std::sync::atomic::{AtomicBool, Ordering};
        for fails in [false, true] {
            let mut vm = Vm::new(compile_source("").unwrap().program);
            let ready = Arc::new(AtomicBool::new(false));
            let id = vm.allocate_host_op_id();
            vm.host.stream_drivers.insert(
                id,
                Box::new(TerminationGateDriver {
                    ready: Arc::clone(&ready),
                    fails,
                }),
            );
            // This driver has no execution-scope registration. Its own termination
            // acknowledgement is the only quiescence barrier.
            vm.reset_for_reuse().unwrap();
            assert!(vm.scope_reset_pending());
            assert!(!vm.is_reusable());
            let mut cx = Context::from_waker(std::task::Waker::noop());
            assert!(matches!(vm.poll_reset_for_reuse(&mut cx), Poll::Pending));
            ready.store(true, Ordering::SeqCst);
            if fails {
                assert!(matches!(
                    vm.poll_reset_for_reuse(&mut cx),
                    Poll::Ready(Err(_))
                ));
                assert!(!vm.is_reusable());
                assert!(matches!(
                    vm.poll_reset_for_reuse(&mut cx),
                    Poll::Ready(Err(_))
                ));
            } else {
                assert!(matches!(
                    vm.poll_reset_for_reuse(&mut cx),
                    Poll::Ready(Ok(()))
                ));
                assert!(vm.is_reusable());
            }
        }
    }

    struct TerminationGateDriver {
        ready: Arc<std::sync::atomic::AtomicBool>,
        fails: bool,
    }
    impl HostStreamDriver for TerminationGateDriver {
        fn poll_next(&mut self, _: &mut Context<'_>) -> Poll<VmResult<HostStreamPoll>> {
            Poll::Pending
        }
        fn apply_action(&mut self, _: Value) -> VmResult<HostStreamAction> {
            unreachable!()
        }
        fn poll_termination(
            &mut self,
            _: &mut ExecutionScope,
            _: HostStreamTermination,
            _: &mut Context<'_>,
        ) -> Poll<VmResult<()>> {
            use std::sync::atomic::Ordering;
            if !self.ready.load(Ordering::SeqCst) {
                Poll::Pending
            } else if self.fails {
                Poll::Ready(Err(VmError::HostError("driver termination failed".into())))
            } else {
                Poll::Ready(Ok(()))
            }
        }
    }

    #[test]
    fn missing_open_callback_aborts_without_routing_into_event() {
        let mut vm = Vm::new(
            compile_source("pub fn event(x: int) -> bool { true }")
                .unwrap()
                .program,
        );
        assert_eq!(vm.run().unwrap(), VmStatus::Halted);
        let callback = vm.resolve_exported_callable("event").unwrap();
        let actions = Arc::new(Mutex::new(vec![]));
        let driver = ScriptedDriver {
            items: VecDeque::from([HostStreamPoll::Call(
                HostStreamCallback::Open,
                vec![Value::Int(1)],
            )]),
            actions: Arc::clone(&actions),
        };
        let CallOutcome::Pending(id) = vm
            .submit_callable_stream_callbacks(
                callback,
                None,
                &[TypeSchema::Int],
                &[TypeSchema::Int],
                driver,
            )
            .unwrap_or_else(|err| panic!("admission: {}", err.primary))
        else {
            panic!("pending expected")
        };
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            vm.poll_callable_stream(id, &mut cx),
            Poll::Ready(Err(VmError::InvalidFrameState(
                "open item without callback"
            )))
        ));
        assert!(actions.lock().unwrap().is_empty());
        assert!(vm.instance.host_stream.is_none());
        assert!(!vm.host.has_pending_stream_terminations());
    }

    #[test]
    fn completion_failure_rolls_back_stream_and_does_not_publish_a_summary() {
        use std::sync::atomic::AtomicBool;
        let mut vm = Vm::new(
            compile_source("pub fn event(x: int) -> bool { true }")
                .unwrap()
                .program,
        );
        assert_eq!(vm.run().unwrap(), VmStatus::Halted);
        let callback = vm.resolve_exported_callable("event").unwrap();
        let driver = DelayedDriver {
            item: Some(HostStreamPoll::CompleteWithVm(Box::new(|_vm| {
                Err(VmError::HostError("summary failed".to_owned()))
            }))),
            ready: Arc::new(AtomicBool::new(true)),
            cancel_on_action: false,
        };
        let CallOutcome::Pending(id) = vm
            .submit_callable_stream_callbacks(callback, None, &[TypeSchema::Int], &[], driver)
            .unwrap_or_else(|err| panic!("admission: {}", err.primary))
        else {
            panic!("pending expected")
        };
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(
            matches!(vm.poll_callable_stream(id, &mut cx), Poll::Ready(Err(VmError::HostError(message))) if message == "summary failed")
        );
        assert!(vm.instance.host_stream.is_none());
        assert!(vm.stack().is_empty());
        assert_eq!(vm.host_context().resource_count(), 0);
    }

    struct RollbackResource {
        ready: Arc<std::sync::atomic::AtomicBool>,
        closes: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl crate::vm::resource::HostResource for RollbackResource {
        fn begin_close(
            &mut self,
            _: crate::vm::resource::ResourceCloseReason,
        ) -> crate::vm::resource::ResourceResult<crate::vm::resource::CloseProgress> {
            self.closes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::vm::resource::CloseProgress::Pending)
        }
        fn poll_close(
            &mut self,
            _: &mut Context<'_>,
        ) -> Poll<crate::vm::resource::ResourceResult<()>> {
            if self.ready.load(std::sync::atomic::Ordering::SeqCst) {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
    }

    #[test]
    fn failed_vm_completion_rolls_back_new_resource_after_pending_close() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        for cancel in [false, true] {
            let mut vm = Vm::new(
                compile_source("pub fn event(x: int) -> bool { true }")
                    .unwrap()
                    .program,
            );
            assert_eq!(vm.run().unwrap(), VmStatus::Halted);
            let existing = vm.host_context().push_resource(SummaryResource).unwrap();
            let ready = Arc::new(AtomicBool::new(false));
            let closes = Arc::new(AtomicUsize::new(0));
            let make_completion = || {
                let ready = Arc::clone(&ready);
                let closes = Arc::clone(&closes);
                Box::new(move |vm: &mut Vm| {
                    vm.host_context()
                        .push_resource(RollbackResource { ready, closes })
                        .unwrap();
                    Err(VmError::HostError("after push".into()))
                }) as HostVmCompletion<Value>
            };
            let item = if cancel {
                HostStreamPoll::Call(HostStreamCallback::Event, vec![Value::Int(1)])
            } else {
                HostStreamPoll::CompleteWithVm(make_completion())
            };
            let driver = DelayedDriver {
                item: Some(item),
                ready: Arc::new(AtomicBool::new(true)),
                cancel_on_action: cancel,
            };
            let event = vm.resolve_exported_callable("event").unwrap();
            let id = match vm
                .submit_callable_stream_callbacks(event, None, &[TypeSchema::Int], &[], driver)
                .unwrap_or_else(|err| panic!("admission: {}", err.primary))
            {
                CallOutcome::Pending(id) => id,
                _ => panic!("pending expected"),
            };
            // For cancellation the scripted action supplies the failing hook.
            if cancel {
                let inner = vm.host.stream_drivers.remove(&id).unwrap();
                vm.host.stream_drivers.insert(
                    id,
                    Box::new(FailingCompletionDriver {
                        inner,
                        completion: Some(make_completion()),
                    }),
                );
            }
            let mut cx = Context::from_waker(std::task::Waker::noop());
            assert!(matches!(
                vm.poll_callable_stream(id, &mut cx),
                Poll::Pending
            ));
            assert_eq!(vm.host_context().resource_count(), 2);
            assert_eq!(closes.load(Ordering::SeqCst), 1);
            assert!(matches!(
                vm.poll_callable_stream(id, &mut cx),
                Poll::Pending
            ));
            ready.store(true, Ordering::SeqCst);
            assert!(
                matches!(vm.poll_callable_stream(id, &mut cx), Poll::Ready(Err(VmError::HostError(message))) if message == "after push")
            );
            assert_eq!(vm.host_context().resource_count(), 1);
            assert!(vm.host_context().resource(&existing).is_ok());
            assert_eq!(closes.load(Ordering::SeqCst), 1);
        }
    }

    struct FailingCompletionDriver {
        inner: Box<dyn HostStreamDriver>,
        completion: Option<HostVmCompletion<Value>>,
    }
    impl HostStreamDriver for FailingCompletionDriver {
        fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<HostStreamPoll>> {
            self.inner.poll_next(cx)
        }
        fn apply_action(&mut self, _: Value) -> VmResult<HostStreamAction> {
            Ok(HostStreamAction::CancelWithVm(
                self.completion.take().unwrap(),
                OperationCancelReason::Requested,
            ))
        }
    }

    #[test]
    fn invalid_positional_item_aborts_before_invoking_callback() {
        let mut vm = Vm::new(
            compile_source("pub fn event(x: int) -> bool { true }")
                .unwrap()
                .program,
        );
        assert_eq!(vm.run().unwrap(), VmStatus::Halted);
        let callback = vm.resolve_exported_callable("event").unwrap();
        let actions = Arc::new(Mutex::new(vec![]));
        let driver = ScriptedDriver {
            items: VecDeque::from([HostStreamPoll::Call(
                HostStreamCallback::Event,
                vec![Value::string("wrong")],
            )]),
            actions: Arc::clone(&actions),
        };
        let CallOutcome::Pending(id) = vm
            .submit_callable_stream_callbacks(callback, None, &[TypeSchema::Int], &[], driver)
            .unwrap_or_else(|err| panic!("admission: {}", err.primary))
        else {
            panic!("pending expected")
        };
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            vm.poll_callable_stream(id, &mut cx),
            Poll::Ready(Err(VmError::TypeMismatch(_)))
        ));
        assert!(actions.lock().unwrap().is_empty());
        assert!(vm.instance.host_stream.is_none());
    }

    #[test]
    fn cancellation_discards_unexecuted_vm_completion() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut vm = Vm::new(
            compile_source("pub fn event(x: int) -> bool { true }")
                .unwrap()
                .program,
        );
        assert_eq!(vm.run().unwrap(), VmStatus::Halted);
        let callback = vm.resolve_exported_callable("event").unwrap();
        let ready = Arc::new(AtomicBool::new(false));
        let invoked = Arc::new(AtomicBool::new(false));
        let completion: HostVmCompletion<Value> = {
            let invoked = Arc::clone(&invoked);
            Box::new(move |_vm| {
                invoked.store(true, Ordering::SeqCst);
                Ok(Value::Int(1))
            })
        };
        let driver = DelayedDriver {
            item: Some(HostStreamPoll::CompleteWithVm(completion)),
            ready: Arc::clone(&ready),
            cancel_on_action: false,
        };
        let CallOutcome::Pending(id) = vm
            .submit_callable_stream_callbacks(callback, None, &[TypeSchema::Int], &[], driver)
            .unwrap_or_else(|err| panic!("admission: {}", err.primary))
        else {
            panic!("pending expected")
        };
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            vm.poll_callable_stream(id, &mut cx),
            Poll::Pending
        ));
        vm.cancel_callable_stream_with_reason(OperationCancelReason::Requested)
            .unwrap();
        assert!(!invoked.load(Ordering::SeqCst));
        ready.store(true, Ordering::SeqCst);
        assert!(matches!(
            vm.host.poll_stream_terminations(&mut cx),
            Poll::Ready(Ok(()))
        ));
        assert!(!invoked.load(Ordering::SeqCst));
        assert!(vm.stack().is_empty());
    }
}
