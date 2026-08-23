use std::sync::Arc;
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
        // Shared producer slot: the VM polls through it, and the registered
        // StreamScopeOperation takes (drops) the driver out of it on cancel so
        // the producer resource is released exactly once through the operation
        // driver.
        let slot: Arc<std::sync::Mutex<Option<Box<dyn HostStreamDriver>>>> =
            Arc::new(std::sync::Mutex::new(Some(Box::new(driver))));
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let scope_op = StreamScopeOperation {
            slot: Arc::clone(&slot),
            cancelled: Arc::clone(&cancelled),
        };
        let scope_id = self
            .host
            .execution_scope_start_operation(crate::vm::operation::OperationSpec::new(scope_op))
            .map_err(|error| VmError::HostError(error.to_string()))?;
        let op_id = scope_id.raw();
        self.host.stream_drivers.insert(op_id, Arc::clone(&slot));
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

    pub(crate) fn cancel_callable_stream(&mut self) {
        if let Some(stream) = self.instance.host_stream.take() {
            if let Some(slot) = self.host.stream_drivers.remove(&stream.op_id)
                && let Ok(mut guard) = slot.lock()
            {
                // Release the producer through the operation driver path: taking
                // the driver out of the shared slot drops it (producer cleanup).
                guard.take();
            }
            let _ = self.host.execution_scope_abort_operation(
                crate::vm::operation::OperationId::from_raw(stream.op_id)
                    .expect("callable stream op id is a packed scope id"),
                crate::vm::operation::OperationCancelReason::Requested,
            );
            if let Some(item) = stream.item {
                self.drop_value_with_contract(item);
            }
            self.drop_value_with_contract(stream.callback);
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
        let polled = match self.host.stream_drivers.get(&op_id) {
            Some(slot) => match slot.lock() {
                Ok(mut guard) => match guard.as_mut() {
                    Some(driver) => driver.poll_next(cx),
                    None => {
                        return Poll::Ready(Err(VmError::HostError(format!(
                            "callable stream driver {op_id} was already released"
                        ))));
                    }
                },
                Err(_) => {
                    return Poll::Ready(Err(VmError::HostError(format!(
                        "callable stream driver slot {op_id} is poisoned"
                    ))));
                }
            },
            None => {
                return Poll::Ready(Err(VmError::HostError(format!(
                    "missing callable stream driver {op_id}"
                ))));
            }
        };
        match polled {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => {
                self.abort_callable_stream();
                Poll::Ready(Err(error))
            }
            Poll::Ready(Ok(HostStreamPoll::Complete(summary))) => {
                self.finish_callable_stream(summary);
                Poll::Ready(Ok(()))
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
                        self.abort_callable_stream();
                        Poll::Ready(Err(error))
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

    pub(crate) fn abort_callable_stream_on_run_error(&mut self) {
        if self
            .instance
            .host_stream
            .as_ref()
            .is_some_and(|stream| stream.phase == HostStreamPhase::RunCallback)
        {
            self.abort_callable_stream();
        }
    }

    fn finish_callable_stream_callback(&mut self) -> VmResult<VmStatus> {
        let Some(action) = self.instance.host_return.take() else {
            self.abort_callable_stream();
            return Err(VmError::InvalidFrameState(
                "callable stream callback returned no action",
            ));
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
        let applied = {
            let slot = self.host.stream_drivers.get(&op_id).ok_or_else(|| {
                VmError::HostError(format!("missing callable stream driver {op_id}"))
            })?;
            let mut guard = slot.lock().map_err(|_| {
                VmError::HostError(format!("callable stream driver slot {op_id} is poisoned"))
            })?;
            let driver = guard.as_mut().ok_or_else(|| {
                VmError::HostError(format!(
                    "callable stream driver {op_id} was already released"
                ))
            })?;
            driver.apply_action(action)
        };
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
                self.finish_callable_stream(summary);
                Ok(VmStatus::Halted)
            }
            Err(error) => {
                self.abort_callable_stream();
                Err(error)
            }
        }
    }

    fn finish_callable_stream(&mut self, summary: Value) {
        let Some(stream) = self.instance.host_stream.take() else {
            return;
        };
        if let Some(slot) = self.host.stream_drivers.remove(&stream.op_id)
            && let Ok(mut guard) = slot.lock()
        {
            guard.take();
        }
        let _ = self.host.execution_scope_abort_operation(
            crate::vm::operation::OperationId::from_raw(stream.op_id)
                .expect("callable stream op id is a packed scope id"),
            crate::vm::operation::OperationCancelReason::Requested,
        );
        self.instance.waiting_host_op = None;
        self.drop_value_with_contract(stream.callback);
        if let Some(item) = stream.item {
            self.drop_value_with_contract(item);
        }
        self.instance.stack.push(summary);
    }

    fn abort_callable_stream(&mut self) {
        let Some(stream) = self.instance.host_stream.take() else {
            return;
        };
        if let Some(slot) = self.host.stream_drivers.remove(&stream.op_id)
            && let Ok(mut guard) = slot.lock()
        {
            guard.take();
        }
        let _ = self.host.execution_scope_abort_operation(
            crate::vm::operation::OperationId::from_raw(stream.op_id)
                .expect("callable stream op id is a packed scope id"),
            crate::vm::operation::OperationCancelReason::Requested,
        );
        self.instance.waiting_host_op = None;
        self.abort_host_invocation(stream.parent_stack_base, stream.parent_frame_count);
        self.drop_value_with_contract(stream.callback);
        if let Some(item) = stream.item {
            self.drop_value_with_contract(item);
        }
    }
}

/// The `HostOperation` driver that keeps a callable stream registered in the
/// current `ExecutionScope`.
///
/// The producer driver lives in a shared slot jointly owned with the VM's
/// `stream_drivers` map. The VM polls the producer through the slot (never
/// through this operation's poll); this operation exists so the stream is a
/// first-class pending operation with a packed scope `OperationId` and so
/// scope reset/drop cancellation reaches the producer. `poll` stays pending
/// (the operation is released explicitly by the VM on stream finish/cancel/
/// abort), and `cancel` takes (drops) the producer driver out of the shared
/// slot so the producer resource is released exactly once through the
/// operation driver.
struct StreamScopeOperation {
    slot: std::sync::Arc<std::sync::Mutex<Option<Box<dyn HostStreamDriver>>>>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl crate::vm::operation::HostOperation for StreamScopeOperation {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<crate::vm::operation::OperationResult<()>> {
        if self.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            Poll::Ready(Err(crate::vm::operation::OperationError::new(
                crate::vm::operation::OperationErrorCode::OperationDriverFailed,
                "vm::stream",
                "callable stream was cancelled",
            )))
        } else {
            Poll::Pending
        }
    }

    fn cancel(
        &mut self,
        _reason: crate::vm::operation::OperationCancelReason,
    ) -> crate::vm::operation::OperationResult<()> {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // Release the producer driver now: dropping it releases producer
        // resources without requiring another poll.
        if let Ok(mut guard) = self.slot.lock() {
            guard.take();
        }
        Ok(())
    }
}
