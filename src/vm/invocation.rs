//! Invocation item stream.
//!
//! One exported callable started with ordinary arguments behaves like
//! `Stream<Item = Result<InvocationItem, InvocationError>>`: zero or more
//! `Event` items produced by `stream::emit`, then exactly one `Complete` item
//! or one typed error, then a fused end of stream. Polling drives execution;
//! the VM does not produce items while the consumer is not polling, and at most
//! one event item is buffered between polls (natural backpressure).
//!
//! The invocation reuses the existing callable execution state
//! ([`Vm::start_callable`], [`Vm::run`], [`Vm::take_callable_result`]) and the
//! existing async host bridge; it does not duplicate interpreter or host loops,
//! and it does not add an executor, generator syntax, an event queue, or event
//! persistence policy.

use std::fmt;
use std::task::{Context, Poll, Waker};

use crate::builtins::runtime::cancellation::{
    CancellationReason, CancellationToken, OperationId, OperationState, OperationStatus,
};
use crate::builtins::runtime::error::RuntimeError;
use crate::vm::{CallOutcome, CallReturn, Value, Vm, VmError, VmResult, VmStatus, VmYieldReason};

/// One item yielded by an invocation stream.
#[derive(Clone, Debug, PartialEq)]
pub enum InvocationItem {
    /// One bounded event produced by `stream::emit(value)`.
    Event(Value),
    /// The callable's return value; exactly one per invocation.
    Complete(Value),
}

/// Typed terminal failure of an invocation stream.
///
/// The failure is machine-readable: cancellation keeps its reason, fuel and
/// deadline failures keep their numeric state, and runtime capability failures
/// keep their structured [`RuntimeError`] instead of being flattened to a
/// string.
#[derive(Debug)]
pub enum InvocationError {
    /// The invocation was cancelled with this reason.
    Cancelled(CancellationReason),
    /// The configured fuel budget was exhausted.
    OutOfFuel { needed: u64, remaining: u64 },
    /// The configured epoch deadline expired.
    DeadlineReached { current: u64, deadline: u64 },
    /// A runtime capability failure with its machine-readable code.
    Capability(RuntimeError),
    /// An embedding host failure without a structured runtime code.
    Host { message: String },
    /// A low-level VM failure (script error or invalid frame state).
    Vm(VmError),
}

/// Poll outcome of an invocation stream.
#[derive(Debug)]
pub enum InvocationPoll {
    /// The VM is paused (waiting on a host operation or a host-driven yield);
    /// drive the outstanding work and poll again.
    Pending,
    /// One stream item, or `None` after the fused end of stream.
    Ready(Option<Result<InvocationItem, InvocationError>>),
}

/// Run-scoped state of the single active invocation on a VM.
#[derive(Debug)]
pub(crate) struct InvocationState {
    pub(crate) phase: InvocationPhase,
    /// True while the VM is yielded at a `stream::emit` call site whose event
    /// has already been delivered. The resumed call site re-enters
    /// `stream::emit` and consumes this marker instead of emitting a second
    /// event for the same call.
    pub(crate) emit_yield_pending: bool,
    /// A structured runtime error produced by `stream::emit` validation,
    /// preserved for the terminal error item without string flattening.
    pub(crate) pending_error: Option<RuntimeError>,
    /// Stack and frame position recorded when the invocation started, used to
    /// release interpreter state on terminal failure.
    pub(crate) stack_base: usize,
    pub(crate) frame_count: usize,
}

#[derive(Debug)]
pub(crate) enum InvocationPhase {
    Running,
    EventPending(Value),
    CompletePending(Value),
    ErrorPending(InvocationError),
    Fused,
}

/// One active invocation handle borrowing the VM.
///
/// Polling drives execution; dropping the handle abandons the invocation but
/// keeps it active on the VM until it fuses (a new invocation is rejected while
/// one is active).
pub struct Invocation<'vm> {
    vm: &'vm mut Vm,
}

impl fmt::Debug for Invocation<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Invocation").finish_non_exhaustive()
    }
}

impl Invocation<'_> {
    /// Polls the invocation stream.
    ///
    /// Returns `Ready(Some(Ok(Event(value))))` for each emitted event,
    /// `Ready(Some(Ok(Complete(value))))` exactly once for the callable return
    /// value, `Ready(Some(Err(error)))` exactly once for a typed terminal
    /// failure, and `Ready(None)` on every poll after the stream has fused.
    /// `Pending` means the VM is paused on an outstanding host operation or
    /// host-driven yield; drive it and poll again.
    pub fn poll_next(&mut self) -> VmResult<InvocationPoll> {
        self.vm.poll_invocation()
    }

    /// Cancels the active invocation with a typed reason.
    ///
    /// Outstanding owned host operations are cancelled with the same reason.
    /// The next poll produces exactly one `Cancelled(reason)` error item, after
    /// which the stream is fused.
    pub fn cancel(&mut self, reason: CancellationReason) -> VmResult<()> {
        let cancellation_result = self.vm.run_ctx.cancel(reason);
        self.vm.cancel_waiting_host_op_with_reason(reason);
        self.vm.cancel_callable_stream();
        cancellation_result
    }
}

/// One poll step selected from the current invocation phase.
enum InvocationAction {
    Cancelled,
    Event,
    Complete,
    Error,
    Fused,
    Drive,
}

impl Vm {
    /// Starts one invocation of an exported callable with ordinary arguments.
    ///
    /// The VM must be halted (complete the root frame with [`Vm::run`] first),
    /// and must not already have an active invocation. A second invocation on
    /// the same VM is rejected while one is active.
    pub fn start_invocation(
        &mut self,
        callable: Value,
        args: Vec<Value>,
    ) -> VmResult<Invocation<'_>> {
        if !matches!(callable, Value::Callable(_)) {
            return Err(VmError::InvalidCallable);
        }
        if self
            .instance
            .invocation
            .as_ref()
            .is_some_and(|state| !matches!(state.phase, InvocationPhase::Fused))
        {
            return Err(VmError::InvalidFrameState(
                "an invocation is already active on this vm",
            ));
        }
        let stack_base = self.instance.stack.len();
        let frame_count = self.instance.execution_frames.len();
        self.instance.invocation = Some(InvocationState {
            phase: InvocationPhase::Running,
            emit_yield_pending: false,
            pending_error: None,
            stack_base,
            frame_count,
        });

        // A cancellation that predates the invocation terminates it
        // immediately. No callable, frame, or host operation has started yet,
        // so there is nothing to release here: the stream transitions
        // directly to the typed error and normal error delivery releases the
        // invocation exactly once when the item is consumed.
        if let Some(reason) = self.run_ctx.cancellation.reason() {
            self.instance
                .invocation
                .as_mut()
                .expect("invocation state")
                .phase = InvocationPhase::ErrorPending(InvocationError::Cancelled(reason));
            return Ok(Invocation { vm: self });
        }

        match self.start_callable(callable, &args) {
            Ok(VmStatus::Halted) => {
                let result = self
                    .take_callable_result()
                    .ok_or(VmError::InvalidFrameState(
                        "invocation halted without a callable result",
                    ))?;
                self.instance
                    .invocation
                    .as_mut()
                    .expect("invocation state")
                    .phase = InvocationPhase::CompletePending(result);
            }
            Ok(VmStatus::Yielded) => {
                // Either `stream::emit` placed one pending event, or the
                // embedding must drive a host-owned yield; both are serviced by
                // the next poll.
            }
            Ok(VmStatus::Waiting(_)) => {}
            Err(error) => {
                let error = self.map_invocation_error(error, None);
                self.release_invocation();
                self.instance
                    .invocation
                    .as_mut()
                    .expect("invocation state")
                    .phase = InvocationPhase::ErrorPending(error);
            }
        }
        Ok(Invocation { vm: self })
    }

    fn poll_invocation(&mut self) -> VmResult<InvocationPoll> {
        loop {
            let action = match self.instance.invocation.as_ref() {
                Some(state) => {
                    // Authoritative cancellation supersedes a pending Event or
                    // Complete: the pending value is discarded (through the
                    // drop-contract path) and the stream transitions to one
                    // Cancelled item, then a fused end.
                    if self.run_ctx.cancellation.reason().is_some()
                        && matches!(
                            state.phase,
                            InvocationPhase::EventPending(_) | InvocationPhase::CompletePending(_)
                        )
                    {
                        InvocationAction::Cancelled
                    } else {
                        match state.phase {
                            InvocationPhase::EventPending(_) => InvocationAction::Event,
                            InvocationPhase::CompletePending(_) => InvocationAction::Complete,
                            InvocationPhase::ErrorPending(_) => InvocationAction::Error,
                            InvocationPhase::Fused => InvocationAction::Fused,
                            InvocationPhase::Running => InvocationAction::Drive,
                        }
                    }
                }
                None => return Ok(InvocationPoll::Ready(None)),
            };
            match action {
                InvocationAction::Cancelled => {
                    let reason = self
                        .run_ctx
                        .cancellation
                        .reason()
                        .expect("a cancelled action requires a cancellation reason");
                    let discarded = self.replace_invocation_phase(InvocationPhase::ErrorPending(
                        InvocationError::Cancelled(reason),
                    ));
                    match discarded {
                        InvocationPhase::EventPending(value)
                        | InvocationPhase::CompletePending(value) => {
                            self.drop_value_with_contract(value);
                        }
                        _ => unreachable!("the cancelled action matched a pending phase above"),
                    }
                }
                InvocationAction::Event => {
                    let value = match self.replace_invocation_phase(InvocationPhase::Running) {
                        InvocationPhase::EventPending(value) => value,
                        _ => unreachable!("phase matched above"),
                    };
                    // `emit_yield_pending` stays set until the resumed call
                    // site re-enters `stream::emit`.
                    return Ok(InvocationPoll::Ready(Some(Ok(InvocationItem::Event(
                        value,
                    )))));
                }
                InvocationAction::Complete => {
                    let value = match self.replace_invocation_phase(InvocationPhase::Fused) {
                        InvocationPhase::CompletePending(value) => value,
                        _ => unreachable!("phase matched above"),
                    };
                    self.release_invocation();
                    return Ok(InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(
                        value,
                    )))));
                }
                InvocationAction::Error => {
                    let error = match self.replace_invocation_phase(InvocationPhase::Fused) {
                        InvocationPhase::ErrorPending(error) => error,
                        _ => unreachable!("phase matched above"),
                    };
                    self.release_invocation();
                    return Ok(InvocationPoll::Ready(Some(Err(error))));
                }
                InvocationAction::Fused => return Ok(InvocationPoll::Ready(None)),
                InvocationAction::Drive => {
                    let result = self.drive_invocation();
                    match result {
                        DriveOutcome::Continue => {}
                        DriveOutcome::Pending => return Ok(InvocationPoll::Pending),
                        DriveOutcome::Error(error) => {
                            self.release_invocation();
                            self.instance
                                .invocation
                                .as_mut()
                                .expect("invocation state")
                                .phase = InvocationPhase::ErrorPending(error);
                        }
                    }
                }
            }
        }
    }

    /// Runs the low-level pump once and folds the outcome into the invocation
    /// phase. `Vm::run` itself is unchanged.
    fn drive_invocation(&mut self) -> DriveOutcome {
        if let Some(reason) = self.run_ctx.cancellation.reason() {
            return DriveOutcome::Error(InvocationError::Cancelled(reason));
        }
        match self.run() {
            Ok(VmStatus::Halted) => {
                let result = match self.take_callable_result() {
                    Some(result) => result,
                    None => {
                        return DriveOutcome::Error(InvocationError::Vm(
                            VmError::InvalidFrameState(
                                "invocation halted without a callable result",
                            ),
                        ));
                    }
                };
                self.instance
                    .invocation
                    .as_mut()
                    .expect("invocation state")
                    .phase = InvocationPhase::CompletePending(result);
                DriveOutcome::Continue
            }
            Ok(VmStatus::Yielded) => match self.last_yield_reason() {
                Some(VmYieldReason::Fuel) => DriveOutcome::Error(InvocationError::OutOfFuel {
                    needed: u64::from(self.run_ctx.fuel_check_interval),
                    remaining: self.run_ctx.fuel_remaining,
                }),
                Some(VmYieldReason::Epoch) => {
                    DriveOutcome::Error(InvocationError::DeadlineReached {
                        current: self.run_ctx.epoch_handle.current(),
                        deadline: self.run_ctx.epoch_deadline,
                    })
                }
                _ => {
                    // A `stream::emit` yield leaves one pending event; any other
                    // host-driven yield is paused for the embedding.
                    let event_pending = matches!(
                        self.instance.invocation.as_ref().map(|state| &state.phase),
                        Some(InvocationPhase::EventPending(_))
                    );
                    if event_pending {
                        DriveOutcome::Continue
                    } else {
                        DriveOutcome::Pending
                    }
                }
            },
            Ok(VmStatus::Waiting(_)) => {
                // Capture the waiting operation AFTER `run()`: the step may
                // have registered a new host op. The operation state is
                // retained before polling because failing the operation
                // removes it from the registry; `map_invocation_error` must
                // still be able to recover its typed `OperationStatus::Failed`
                // error after the first poll clears the waiting state.
                let waiting_operation = self.capture_waiting_operation();
                // Poll the outstanding host operation once with a noop waker.
                // The embedding-owned driver completes it; re-polling observes
                // readiness.
                let waker = Waker::noop();
                let mut cx = Context::from_waker(waker);
                match self.poll_waiting_host_op(&mut cx) {
                    Poll::Ready(Ok(())) => DriveOutcome::Continue,
                    Poll::Ready(Err(error)) => {
                        DriveOutcome::Error(self.map_invocation_error(error, waiting_operation))
                    }
                    Poll::Pending => DriveOutcome::Pending,
                }
            }
            Err(error) => {
                // `run()` may have registered a new host op before failing;
                // retain its operation state for typed error mapping.
                let waiting_operation = self.capture_waiting_operation();
                DriveOutcome::Error(self.map_invocation_error(error, waiting_operation))
            }
        }
    }

    /// Captures the state of the host operation the VM is waiting on, if any.
    ///
    /// The waiting state must be captured after `run()` (the step may have
    /// registered a new host op) and before a poll that may fail and remove
    /// the operation from the registry: `map_invocation_error` needs the
    /// retained state to recover the typed `OperationStatus::Failed` error
    /// once the waiting state has been cleared.
    fn capture_waiting_operation(&self) -> Option<OperationState> {
        self.instance
            .waiting_host_op
            .and_then(|op| OperationId::from_raw(op.op_id).ok())
            .and_then(|operation_id| self.host.runtime_operations.get(operation_id).ok())
    }

    /// Maps a low-level VM failure to the typed invocation error, preserving
    /// structured runtime errors from `stream::emit` validation and from failed
    /// host operations. The waiting operation state is captured by the caller
    /// before the poll that may fail and remove it from the registry.
    fn map_invocation_error(
        &mut self,
        error: VmError,
        waiting_operation: Option<OperationState>,
    ) -> InvocationError {
        if let Some(state) = self.instance.invocation.as_mut()
            && let Some(runtime_error) = state.pending_error.take()
        {
            return InvocationError::Capability(runtime_error);
        }
        if let Some(operation) = waiting_operation
            && let OperationStatus::Failed(runtime_error) = operation.status()
        {
            return InvocationError::Capability(runtime_error);
        }
        match error {
            VmError::OutOfFuel { needed, remaining } => {
                InvocationError::OutOfFuel { needed, remaining }
            }
            VmError::EpochDeadlineReached { current, deadline } => {
                InvocationError::DeadlineReached { current, deadline }
            }
            VmError::HostError(message) => InvocationError::Host { message },
            other => InvocationError::Vm(other),
        }
    }

    /// Replaces the active invocation phase, returning the previous one so the
    /// caller can consume it or drop it (the pending-event drop contract stays
    /// with the caller).
    fn replace_invocation_phase(&mut self, phase: InvocationPhase) -> InvocationPhase {
        std::mem::replace(
            &mut self
                .instance
                .invocation
                .as_mut()
                .expect("invocation state")
                .phase,
            phase,
        )
    }

    /// Releases the active invocation: cancels outstanding owned operations,
    /// drops interpreter frames and stack entries introduced by the
    /// invocation, and fuses the stream.
    ///
    /// Releasing is the invocation boundary for VM-level cancellation: the
    /// run-context cancellation token is replaced with a fresh root so the
    /// reason consumed by this invocation (or any stale pre-invocation
    /// cancellation) cannot leak into a later invocation on the same VM.
    /// Outstanding operations were cancelled above; operations registered by
    /// a later invocation attach to the fresh token, preserving per-invocation
    /// parent cancellation semantics.
    fn release_invocation(&mut self) {
        let (stack_base, frame_count) = self
            .instance
            .invocation
            .as_ref()
            .map(|state| (state.stack_base, state.frame_count))
            .unwrap_or((0, 0));
        self.cancel_callable_stream();
        self.abort_host_invocation(stack_base, frame_count);
        if let Some(state) = self.instance.invocation.as_mut() {
            state.phase = InvocationPhase::Fused;
            state.emit_yield_pending = false;
            state.pending_error = None;
        }
        self.run_ctx.cancellation = CancellationToken::root();
    }

    /// Implements the script-visible `stream::emit(value)` builtin: validates
    /// the per-item bound, places one pending event, and yields control to the
    /// invocation poller. `stream::emit` still evaluates to `()` inside RSS.
    ///
    /// When the poller has delivered the event and the VM resumes, the call
    /// site re-executes; the second entry consumes the `emit_yield_pending`
    /// marker and returns normally instead of emitting a second event.
    pub(crate) fn emit_stream_item(&mut self, value: Value) -> VmResult<CallOutcome> {
        let state = self.instance.invocation.as_mut().ok_or_else(|| {
            VmError::HostError("stream::emit requires an active invocation".to_string())
        })?;
        if !matches!(state.phase, InvocationPhase::Running) {
            return Err(VmError::HostError(
                "stream::emit is only valid while the invocation is running".to_string(),
            ));
        }
        if state.emit_yield_pending {
            state.emit_yield_pending = false;
            return Ok(CallOutcome::Return(CallReturn::none()));
        }
        let limits = self.run_ctx.runtime_context.event_limits();
        match crate::builtins::runtime::event::EventPayload::try_new(value, limits) {
            Ok(payload) => {
                state.phase = InvocationPhase::EventPending(payload.into_value());
                state.emit_yield_pending = true;
                Ok(CallOutcome::Yield)
            }
            Err(runtime_error) => {
                let message = runtime_error.to_string();
                state.pending_error = Some(runtime_error);
                Err(VmError::HostError(message))
            }
        }
    }
}

/// Outcome of one low-level drive step.
enum DriveOutcome {
    Continue,
    Pending,
    Error(InvocationError),
}
