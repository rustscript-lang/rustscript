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

#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// Installs a new async host bridge as the *current generation*.
    ///
    /// The currently waited-on host operation (if any) is cancelled first,
    /// matching legacy swap semantics. Every *other* bridge-submitted
    /// operation — including ones that were submitted but never awaited —
    /// keeps polling and cancelling against its original bridge generation:
    /// each such operation's driver holds a clone of the generation's
    /// `Arc<Mutex<Box<dyn HostAsyncBridge>>>`, so swapping the current
    /// generation never invalidates outstanding operations, and the old
    /// bridge box drops only after every driver that references it finishes.
    /// New submissions from this point use the new bridge generation.
    pub fn set_async_bridge(&mut self, bridge: Box<dyn HostAsyncBridge>) {
        self.cancel_waiting_host_op();
        self.host.async_bridge = Some(Arc::new(Mutex::new(bridge)));
    }

    /// Removes the current async host bridge generation.
    ///
    /// The currently waited-on host operation (if any) is cancelled first.
    /// Outstanding bridge-submitted operations from earlier generations are
    /// *not* invalidated: they keep polling and cancelling against the
    /// generation they were submitted to, and that generation drops once all
    /// of its drivers finish. Only *new* `submit_host_future` calls are
    /// rejected after a clear, with the usual "requires a host async bridge"
    /// error.
    pub fn clear_async_bridge(&mut self) {
        self.cancel_waiting_host_op();
        self.host.async_bridge = None;
    }

    pub fn submit_host_future(&mut self, future: HostFuture) -> VmResult<CallOutcome> {
        // The future is handed to the bridge (which owns the runtime context
        // needed to poll it) under the id the modern registry allocates. The
        // driver clones the *current* bridge generation (`Arc<Mutex<Box<dyn
        // HostAsyncBridge>>>`) before the registry is borrowed, so the two
        // host fields never conflict and a later `set_async_bridge` /
        // `clear_async_bridge` swap cannot invalidate this operation: the
        // driver owns its generation and drops it exactly once it finishes.
        let bridge = match self.host.async_bridge.clone() {
            Some(bridge) => bridge,
            None => {
                return Err(VmError::HostError(
                    "async host function requires a host async bridge".to_string(),
                ));
            }
        };
        let output_cell: std::sync::Arc<
            std::sync::Mutex<Option<VmResult<HostFutureOutput<CallReturn>>>>,
        > = std::sync::Arc::new(std::sync::Mutex::new(None));
        let id_cell: std::sync::Arc<std::sync::Mutex<Option<HostOpId>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let driver = HostFutureOperation {
            op_id: std::sync::Arc::clone(&id_cell),
            bridge: Arc::clone(&bridge),
            output: std::sync::Arc::clone(&output_cell),
        };
        let scope_id = self
            .host
            .execution_scope_start_operation(crate::vm::operation::OperationSpec::new(driver))
            .map_err(|error| VmError::HostError(error.to_string()))?;
        let op_id = scope_id.raw();
        *id_cell
            .lock()
            .expect("bridge id cell lock should not be poisoned") = Some(op_id);
        // Hand the future to the bridge, then install the pending-result
        // adapter that materializes the produced HostFutureOutput. The
        // current bridge generation is used for the initial submission;
        // outstanding operations keep living against it even after a later
        // swap.
        //
        // The handoff is failure-atomic: once `start_operation` succeeds,
        // *every* later error — a poisoned generation lock (`Err` from
        // `with_bridge` when `submit_op` was never reached) or a typed bridge
        // rejection (`Ok(Err(_))`) — rolls back the operation through
        // `abort_operation`, which cancels the driver exactly once and then
        // consumes/releases the slot immediately. That restores full registry
        // capacity, makes the id stale, and leaves no dangling pending-result
        // adapter (the adapter that would materialize the return is installed
        // only on the success path below).
        //
        // On a poisoned lock the rollback could only re-enter the bridge
        // through `cancel`; that dispatch surfaces a typed error (never a
        // panic or deadlock, because `with_bridge` maps the poisoned lock to
        // a `VmError` and scopes the guard to one call), which the registry
        // records as the first internal `Failed` reason exactly once while
        // still releasing the slot.
        let submit = match with_bridge(&bridge, |current| current.submit_op(op_id, future)) {
            Ok(result) => result,
            Err(error) => {
                // Poisoned generation lock: the operation was registered but the
                // bridge never saw the submission. Roll it back; the driver's
                // cancel re-entry surfaces a typed poison failure that the
                // registry records as the first internal reason, and the slot
                // is still released.
                let _ = self.host.execution_scope_abort_operation(
                    scope_id,
                    crate::vm::operation::OperationCancelReason::Requested,
                );
                return Err(error);
            }
        };
        if let Err(error) = submit {
            // The bridge explicitly rejected the submission (e.g. a full or
            // policy-blocked bridge). Roll it back exactly like the poison
            // path: cancel the driver once and release the slot immediately.
            let _ = self.host.execution_scope_abort_operation(
                scope_id,
                crate::vm::operation::OperationCancelReason::Requested,
            );
            return Err(error);
        }
        let materialize = std::sync::Arc::clone(&output_cell);
        self.host.register_pending_op_result(
            op_id,
            Box::new(move |vm: &mut Vm| {
                let output = materialize
                    .lock()
                    .expect("bridge output cell lock should not be poisoned")
                    .take()
                    .ok_or_else(|| {
                        VmError::HostError(format!(
                            "host operation {op_id} completed without a result"
                        ))
                    })??;
                output.finish(vm)
            }),
        );
        Ok(CallOutcome::Pending(op_id))
    }

    pub fn waiting_host_op_id(&self) -> Option<HostOpId> {
        self.instance.waiting_host_op.as_ref().map(|op| op.op_id)
    }

    pub fn cancel_waiting_host_op(&mut self) {
        self.cancel_waiting_host_op_with_reason(CancellationReason::Requested);
    }

    pub(crate) fn cancel_waiting_host_op_with_reason(
        &mut self,
        reason: crate::builtins::runtime::cancellation::CancellationReason,
    ) {
        let Some(waiting) = self.instance.waiting_host_op.take() else {
            return;
        };
        // A callable stream is cancelled through its VM-side continuation
        // (callback cleanup + operation release through its driver).
        if self
            .instance
            .host_stream
            .as_ref()
            .is_some_and(|stream| stream.op_id == waiting.op_id)
        {
            self.cancel_callable_stream();
            return;
        }
        let scope_reason = scope_reason(reason);
        // Every production pending host operation is a real execution-scope
        // operation with a packed id; cancel it through its own driver with
        // the parallel operation-cancellation vocabulary. A manually-fabricated
        // wait id (test-only) has no scope entry to cancel in the single modern
        // lifecycle.
        if let Ok(scope_id) = crate::vm::operation::OperationId::from_raw(waiting.op_id) {
            let _ = self
                .host
                .execution_scope_cancel_operation(scope_id, scope_reason);
        }
    }

    pub fn complete_host_op(
        &mut self,
        op_id: HostOpId,
        values: impl Into<CallReturn>,
    ) -> VmResult<()> {
        let waiting = self.instance.waiting_host_op.clone().ok_or_else(|| {
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
        // Every production pending host operation is a real execution-scope
        // operation with a packed id: external completion cancels its driver so
        // its bridge work stops exactly once. If the id is not a registered
        // scope operation (a manually-fabricated wait in tests), there is no
        // external work to cancel in the single modern lifecycle.
        if let Ok(scope_id) = crate::vm::operation::OperationId::from_raw(op_id)
            && self
                .host
                .execution_scope()
                .operations()
                .status(scope_id)
                .is_ok()
        {
            let _ = self.host.execution_scope_cancel_operation(
                scope_id,
                crate::vm::operation::OperationCancelReason::Requested,
            );
            self.host.remove_pending_op_result(op_id);
        }
        self.complete_waiting_host_op(op_id, values.into())
    }

    pub fn poll_waiting_host_op(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<()>> {
        let Some(waiting) = self.instance.waiting_host_op.clone() else {
            return Poll::Ready(Ok(()));
        };
        // A callable stream is driven through its VM-side continuation, which
        // polls the producer through the shared driver slot.
        if self
            .instance
            .host_stream
            .as_ref()
            .is_some_and(|stream| stream.op_id == waiting.op_id)
        {
            return self.poll_callable_stream(waiting.op_id, cx);
        }
        // Every other pending host operation is a real execution-scope
        // operation (bridge-submitted future or a generic HostOperation
        // registered by a host-SDK consumer) driven through the single scope
        // registry.
        self.poll_execution_scope_waiting_op(waiting.op_id, cx)
    }

    pub async fn await_waiting_host_op(&mut self) -> VmResult<()> {
        std::future::poll_fn(|cx| self.poll_waiting_host_op(cx)).await
    }

    /// Drives a waiting host operation that lives in the execution scope — a
    /// generic [`HostOperation`] registered by a host-SDK consumer or a
    /// bridge-submitted future driver. This is the single awaiting path for
    /// every modern registered operation: it polls the operation through its
    /// own driver, then materializes the guest-visible value through the
    /// module-registered pending-result adapter for the raw operation id.
    fn poll_execution_scope_waiting_op(
        &mut self,
        op_id: HostOpId,
        cx: &mut Context<'_>,
    ) -> Poll<VmResult<()>> {
        let scope_operation_id = match crate::vm::operation::OperationId::from_raw(op_id) {
            Ok(id) => id,
            Err(error) => return Poll::Ready(Err(VmError::HostError(error.to_string()))),
        };
        match self
            .host
            .execution_scope_poll_operation(scope_operation_id, cx)
        {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => {
                self.instance.waiting_host_op = None;
                Poll::Ready(Err(VmError::HostError(error.to_string())))
            }
            Poll::Ready(Ok(outcome)) => match outcome {
                crate::vm::operation::OperationOutcome::Completed => {
                    let value = match self.host.take_pending_op_result(op_id) {
                        Some(provider) => provider(self),
                        None => Err(VmError::HostError(format!(
                            "host operation {op_id} completed without a result"
                        ))),
                    };
                    match value {
                        Ok(values) => match self.complete_waiting_host_op(op_id, values) {
                            Ok(()) => Poll::Ready(Ok(())),
                            Err(error) => Poll::Ready(Err(error)),
                        },
                        Err(error) => {
                            self.instance.waiting_host_op = None;
                            Poll::Ready(Err(error))
                        }
                    }
                }
                crate::vm::operation::OperationOutcome::Failed(error) => {
                    // Record the typed failure on the active invocation (if
                    // any) so `map_invocation_error` recovers a structured
                    // capability error instead of flattening to a string.
                    // The registry released the operation slot on this poll,
                    // so the id can no longer be re-queried afterwards.
                    if let Some(state) = self.instance.invocation.as_mut() {
                        state.pending_error =
                            Some(crate::vm::invocation::runtime_error_from_operation(
                                op_id,
                                error.clone(),
                            ));
                    }
                    self.instance.waiting_host_op = None;
                    Poll::Ready(Err(VmError::HostError(error.to_string())))
                }
                crate::vm::operation::OperationOutcome::Cancelled(reason) => {
                    self.instance.waiting_host_op = None;
                    Poll::Ready(Err(VmError::HostError(format!(
                        "host operation {op_id} cancelled ({reason})"
                    ))))
                }
            },
        }
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

/// Dispatches one bridge call under the generation's lock, mapping a poisoned
/// mutex to a typed [`VmError::HostError`].
///
/// The guard is scoped strictly to the single bridge dispatch: it is dropped
/// before the caller resumes any other host-runtime work, so no lock is held
/// across a callback that could re-enter the VM (bridge implementations must
/// not re-enter `set_async_bridge`/`clear_async_bridge`/`submit_host_future`
/// from inside their own methods, which would deadlock on the same mutex).
///
/// Poison is surfaced as a typed error rather than panicking or silently
/// reading inconsistent bridge state.
pub(super) fn with_bridge<R>(
    bridge: &Arc<Mutex<Box<dyn HostAsyncBridge>>>,
    op: impl FnOnce(&mut dyn HostAsyncBridge) -> R,
) -> VmResult<R> {
    let mut guard = bridge
        .lock()
        .map_err(|_| VmError::HostError("async host bridge lock is poisoned".to_string()))?;
    Ok(op(&mut **guard))
}

/// The modern `HostOperation` driver wrapping a bridge-submitted future.
///
/// The future itself lives in the bridge (which owns the runtime context);
/// polling and cancellation forward to the bridge through the *generation*
/// this operation was submitted against. The driver holds an
/// [`Arc`] clone of the generation's `Arc<Mutex<Box<dyn HostAsyncBridge>>>`,
/// so a later `set_async_bridge`/`clear_async_bridge` swap on the VM can
/// never invalidate this operation: the old bridge box stays alive as long as
/// this driver (and any sibling driver of the same generation) is registered,
/// and drops exactly once the last clone is released. The produced
/// [`HostFutureOutput`] is parked in a shared cell and materialized by the
/// VM through the pending-result adapter registered at submission time. The
/// operation id is written once the registry allocates it (the driver cannot
/// know it before registration).
struct HostFutureOperation {
    op_id: std::sync::Arc<std::sync::Mutex<Option<HostOpId>>>,
    bridge: Arc<Mutex<Box<dyn HostAsyncBridge>>>,
    output: std::sync::Arc<std::sync::Mutex<Option<VmResult<HostFutureOutput<CallReturn>>>>>,
}

impl crate::vm::operation::HostOperation for HostFutureOperation {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<crate::vm::operation::OperationResult<()>> {
        let op_id = self
            .op_id
            .lock()
            .expect("bridge id cell lock should not be poisoned")
            .expect("bridge driver id is set before any poll");
        let polled = with_bridge(&self.bridge, |current| current.poll_submitted_op(op_id, cx));
        match polled {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(Ok(output))) => {
                *self
                    .output
                    .lock()
                    .expect("bridge output cell lock should not be poisoned") = Some(Ok(output));
                Poll::Ready(Ok(()))
            }
            Ok(Poll::Ready(Err(error))) => Poll::Ready(Err(driver_failure(error))),
            Err(error) => Poll::Ready(Err(driver_failure(error))),
        }
    }

    fn cancel(
        &mut self,
        reason: crate::vm::operation::OperationCancelReason,
    ) -> crate::vm::operation::OperationResult<()> {
        let op_id = self
            .op_id
            .lock()
            .expect("bridge id cell lock should not be poisoned")
            .expect("bridge driver id is set before any cancel");
        with_bridge(&self.bridge, |current| {
            current.cancel_op_with_reason(op_id, legacy_reason(reason));
        })
        .map_err(driver_failure)
    }
}

/// Maps a [`VmError`] surfaced from the bridge (or from a poisoned generation
/// lock) onto the typed modern operation failure vocabulary.
fn driver_failure(error: VmError) -> crate::vm::operation::OperationError {
    crate::vm::operation::OperationError::new(
        crate::vm::operation::OperationErrorCode::OperationDriverFailed,
        "vm::async_host",
        error.to_string(),
    )
}

/// Maps the modern operation cancellation reason onto the legacy public
/// vocabulary exposed at the VM boundary.
fn legacy_reason(
    reason: crate::vm::operation::OperationCancelReason,
) -> crate::builtins::runtime::cancellation::CancellationReason {
    match reason {
        crate::vm::operation::OperationCancelReason::Requested => {
            crate::builtins::runtime::cancellation::CancellationReason::Requested
        }
        crate::vm::operation::OperationCancelReason::Deadline => {
            crate::builtins::runtime::cancellation::CancellationReason::Deadline
        }
        crate::vm::operation::OperationCancelReason::VmReset => {
            crate::builtins::runtime::cancellation::CancellationReason::VmReset
        }
        crate::vm::operation::OperationCancelReason::Parent => {
            crate::builtins::runtime::cancellation::CancellationReason::Parent
        }
        crate::vm::operation::OperationCancelReason::ResourceClosed => {
            crate::builtins::runtime::cancellation::CancellationReason::ResourceClosed
        }
        crate::vm::operation::OperationCancelReason::VmDrop => {
            crate::builtins::runtime::cancellation::CancellationReason::VmDrop
        }
    }
}

/// Maps the legacy public cancellation vocabulary onto the modern operation
/// cancellation reason.
fn scope_reason(
    reason: crate::builtins::runtime::cancellation::CancellationReason,
) -> crate::vm::operation::OperationCancelReason {
    match reason {
        crate::builtins::runtime::cancellation::CancellationReason::Requested => {
            crate::vm::operation::OperationCancelReason::Requested
        }
        crate::builtins::runtime::cancellation::CancellationReason::Deadline => {
            crate::vm::operation::OperationCancelReason::Deadline
        }
        crate::builtins::runtime::cancellation::CancellationReason::VmReset => {
            crate::vm::operation::OperationCancelReason::VmReset
        }
        crate::builtins::runtime::cancellation::CancellationReason::Parent => {
            crate::vm::operation::OperationCancelReason::Parent
        }
        crate::builtins::runtime::cancellation::CancellationReason::ResourceClosed => {
            crate::vm::operation::OperationCancelReason::ResourceClosed
        }
        crate::builtins::runtime::cancellation::CancellationReason::VmDrop => {
            crate::vm::operation::OperationCancelReason::VmDrop
        }
    }
}
