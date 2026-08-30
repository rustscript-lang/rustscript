//! Generic async host execution SDK.
//!
//! This module provides the public surface for submitting async host
//! functions that do not borrow the VM across a poll. The two pieces are:
//!
//! * [`HostFutureOutput`] — the terminal result of an async host call,
//!   either an already-produced [`CallReturn`] or a [`VmCompletion`] closure
//!   that must run against the VM (e.g. to insert a resource into the
//!   execution scope) before the call can return to the guest.
//! * [`CaptureAsyncHostContext`] — the trait async host functions use to
//!   capture owned, `'static` host context from the VM before submission.
//!
//! Submitted futures are handed to the configured [`HostAsyncBridge`], which
//! owns their polling on its own executor. The returned host-operation id is
//! tracked in the host runtime's `submitted_host_ops` set so the VM routes
//! the waiting dispatch to the bridge's [`poll_submitted_op`] instead of a
//! runtime-owned operation driver. Async host operations therefore go
//! through the bridge (the concrete driver of the submitted future); the VM
//! never builds a second cancellation framework or static poller table.

use std::future::Future;
use std::pin::Pin;

use super::*;

mod stream;

#[allow(unused_imports)]
pub(crate) use stream::{
    HostStreamAction, HostStreamAdmissionError, HostStreamAdmissionRollback,
    HostStreamContinuation, HostStreamDriver, HostStreamPoll, HostStreamTermination,
    PendingHostStreamTermination, preserve_stream_cleanup,
};

/// A completion closure that runs against the VM after the async call's
/// future has resolved.
pub type HostVmCompletion<T> = Box<dyn FnOnce(&mut Vm) -> VmResult<T> + Send + 'static>;

/// The terminal result of a submitted async host call.
///
/// `T` is the value produced without further VM access (`Return`), or the
/// value produced by a completion closure that borrows the VM once
/// (`VmCompletion`).
pub enum HostFutureOutput<T = CallReturn> {
    Return(T),
    VmCompletion(HostVmCompletion<T>),
}

impl<T> HostFutureOutput<T> {
    /// Wraps an already-produced value.
    pub fn returning(value: T) -> Self {
        Self::Return(value)
    }

    /// Wraps a completion closure that must run against the VM to produce
    /// the value.
    pub fn complete(completion: impl FnOnce(&mut Vm) -> VmResult<T> + Send + 'static) -> Self {
        Self::VmCompletion(Box::new(completion))
    }

    /// Maps the produced value through `map`, deferring the mapping until
    /// the completion closure (if any) has run against the VM.
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
    /// Resolves the terminal output against the VM: a `Return` value is
    /// returned directly; a `VmCompletion` closure runs with `&mut Vm`.
    pub(crate) fn finish(self, vm: &mut Vm) -> VmResult<CallReturn> {
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

/// A boxed, owned future produced by an async host function submission.
pub type HostFuture = Pin<Box<dyn Future<Output = VmResult<HostFutureOutput>> + Send + 'static>>;

/// Allows an async host function to capture owned host context from the VM
/// before its future is submitted.
///
/// Async host functions cannot borrow the VM across a poll; they must
/// capture everything they need as owned, `'static` values. Implementors
/// run in the VM thread during the originating host call.
pub trait CaptureAsyncHostContext: Send + 'static + Sized {
    fn capture(vm: &mut Vm) -> VmResult<Self>;

    fn capture_with_args(vm: &mut Vm, _args: &[Value]) -> VmResult<Self> {
        Self::capture(vm)
    }
}

impl Vm {
    /// Submits an async host future to the configured async host bridge.
    ///
    /// The future is handed to the bridge, which owns its polling on its own
    /// executor. A fresh host-operation id is allocated, recorded in the
    /// bridge's submitted set, and returned as a `Pending` call outcome.
    ///
    /// Requires a configured [`HostAsyncBridge`] that accepts submitted
    /// futures; otherwise a host error is returned.
    pub fn submit_host_future(&mut self, future: HostFuture) -> VmResult<CallOutcome> {
        if self.host.async_bridge.is_none() {
            return Err(VmError::HostError(
                "async host function requires a host async bridge".to_string(),
            ));
        }
        let op_id = self.host.reserve_submitted_host_op()?;
        let submit_result = self
            .host
            .async_bridge
            .as_mut()
            .expect("bridge presence checked before reservation")
            .submit_op(op_id, future);
        if let Err(error) = submit_result {
            self.host.rollback_submitted_host_op(op_id);
            return Err(error);
        }
        Ok(CallOutcome::Pending(op_id))
    }
}
