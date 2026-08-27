use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread::JoinHandle;

use pd_host_function::pd_host_function;

use super::HostCallResult;
use crate::vm::operation::driver::HostOperation;
use crate::vm::operation::error::{OperationError, OperationErrorCode, OperationResult};
use crate::vm::operation::reason::OperationCancelReason;
use crate::vm::operation::{OperationId, OperationOutcome, OperationSpec};
use crate::vm::resource::close::{CloseProgress, HostResource};
use crate::vm::resource::error::{ResourceError, ResourceErrorCode, ResourceResult};
use crate::vm::resource::{ResourceCloseReason, ResourceHandle};
use crate::vm::{CallReturn, HostOpId, Value, Vm, VmError, VmResult};

/// A file / child-process backed IO handle.
pub(super) enum IoHandle {
    File(std::fs::File),
    PopenRead { child: Child },
    PopenWrite { child: Child },
}

/// Shared lifecycle state for one typed IO resource.
///
/// The handle cell is also the admission lock for workers: a worker increments
/// `active_workers` while holding the cell lock before taking the handle, and a
/// close marks the resource closed before inspecting that same cell. This
/// makes a close racing with a worker either reject the worker or observe it as
/// active; it can never mistake an owned handle for an idle resource.
struct IoResourceState {
    handle: Mutex<Option<IoHandle>>,
    closed: AtomicBool,
    active_workers: AtomicUsize,
    close_waker: Mutex<Option<Waker>>,
    close_error: Mutex<Option<String>>,
}

impl IoResourceState {
    fn new(handle: IoHandle) -> Self {
        Self {
            handle: Mutex::new(Some(handle)),
            closed: AtomicBool::new(false),
            active_workers: AtomicUsize::new(0),
            close_waker: Mutex::new(None),
            close_error: Mutex::new(None),
        }
    }

    /// Takes the handle for one worker and records its ownership before
    /// releasing the admission lock.
    fn take_handle(self: &Arc<Self>) -> Option<IoHandleLease> {
        let mut slot = self
            .handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        let handle = slot.take()?;
        self.active_workers.fetch_add(1, Ordering::AcqRel);
        Some(IoHandleLease {
            state: Arc::clone(self),
            handle: Some(handle),
            active: true,
        })
    }

    fn mark_closed(&self) {
        let _guard = self
            .handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.closed.store(true, Ordering::Release);
    }

    fn register_close_waker(&self, waker: &Waker) {
        let mut guard = self
            .close_waker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.active_workers.load(Ordering::Acquire) == 0 {
            return;
        }
        *guard = Some(waker.clone());
        if self.active_workers.load(Ordering::Acquire) == 0
            && let Some(waker) = guard.take()
        {
            waker.wake();
        }
    }

    fn release_worker(&self) {
        let previous = self.active_workers.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "IO worker release without an active worker");
        if previous == 1
            && let Some(waker) = self
                .close_waker
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
        {
            waker.wake();
        }
    }

    fn record_close_error(&self, error: &VmError) {
        let mut guard = self
            .close_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.is_none() {
            *guard = Some(error.to_string());
        }
    }

    fn cleanup_error(&self) -> Option<ResourceError> {
        self.close_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|message| {
                ResourceError::new(
                    ResourceErrorCode::ResourceCleanupFailed,
                    "io::resource",
                    message.clone(),
                )
            })
    }
}

impl Drop for IoResourceState {
    fn drop(&mut self) {
        let handle = self
            .handle
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(handle) = handle {
            let _ = close_io_handle(handle);
        }
    }
}

/// A worker-owned handle lease. Normal completion explicitly restores the
/// handle to an open resource, while close/cancellation/unwind paths close it
/// instead. In either case the active-worker count is decremented and a
/// pending resource close is woken.
struct IoHandleLease {
    state: Arc<IoResourceState>,
    handle: Option<IoHandle>,
    active: bool,
}

impl Deref for IoHandleLease {
    type Target = IoHandle;

    fn deref(&self) -> &Self::Target {
        self.handle.as_ref().expect("active IO lease has a handle")
    }
}

impl DerefMut for IoHandleLease {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.handle.as_mut().expect("active IO lease has a handle")
    }
}

impl IoHandleLease {
    fn restore(mut self) -> VmResult<()> {
        self.release_inner(false)
    }

    fn close(mut self) -> VmResult<()> {
        self.release_inner(true)
    }

    fn release_inner(&mut self, force_close: bool) -> VmResult<()> {
        if !self.active {
            return Ok(());
        }
        let Some(handle) = self.handle.take() else {
            self.active = false;
            self.state.release_worker();
            return Ok(());
        };

        let mut handle = Some(handle);
        let should_close = if force_close {
            true
        } else {
            let mut slot = self
                .state
                .handle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if self.state.closed.load(Ordering::Acquire) {
                true
            } else {
                *slot = handle.take();
                false
            }
        };

        let result = if should_close {
            close_io_handle(handle.expect("IO lease close owns its handle"))
        } else {
            Ok(())
        };
        if let Err(error) = &result {
            self.state.record_close_error(error);
        }
        self.active = false;
        self.state.release_worker();
        result
    }
}

impl Drop for IoHandleLease {
    fn drop(&mut self) {
        if self.active {
            // A normal worker calls `restore`/`close` explicitly. Reaching this
            // guard means an unwind or failed handoff, so never return a live
            // process handle to the resource table implicitly.
            let _ = self.release_inner(true);
        }
    }
}

/// The typed resource stored in the execution scope for one IO handle.
struct IoResource {
    state: Arc<IoResourceState>,
}

impl IoResource {
    fn new(handle: IoHandle) -> Self {
        Self {
            state: Arc::new(IoResourceState::new(handle)),
        }
    }

    /// Takes the inner handle for a worker thread and records its lease.
    fn take_handle(&self) -> Option<IoHandleLease> {
        self.state.take_handle()
    }
}

impl HostResource for IoResource {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        // Marking closed while holding the same admission lock used by worker
        // leases makes the close boundary linearizable: a worker either
        // restores before close begins, or observes closed and cleans up.
        let mut slot = self
            .state
            .handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.state.closed.store(true, Ordering::Release);
        if self.state.active_workers.load(Ordering::Acquire) != 0 {
            return Ok(CloseProgress::Pending);
        }
        let handle = slot.take();
        drop(slot);

        if let Some(error) = self.state.cleanup_error() {
            return Err(error);
        }
        if let Some(handle) = handle {
            close_io_handle(handle).map_err(|error| {
                self.state.record_close_error(&error);
                ResourceError::new(
                    ResourceErrorCode::ResourceCleanupFailed,
                    "io::resource",
                    error.to_string(),
                )
            })?;
        }
        Ok(CloseProgress::Ready)
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        if self.state.active_workers.load(Ordering::Acquire) != 0 {
            self.state.register_close_waker(cx.waker());
            if self.state.active_workers.load(Ordering::Acquire) != 0 {
                return Poll::Pending;
            }
        }

        if let Some(error) = self.state.cleanup_error() {
            return Poll::Ready(Err(error));
        }
        let handle = self
            .state
            .handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(handle) = handle
            && let Err(error) = close_io_handle(handle)
        {
            self.state.record_close_error(&error);
            return Poll::Ready(Err(ResourceError::new(
                ResourceErrorCode::ResourceCleanupFailed,
                "io::resource",
                error.to_string(),
            )));
        }
        Poll::Ready(Ok(()))
    }
}

/// Shared state between one IO worker thread, its [`IoOpDriver`] operation,
/// and the adapter-owned completion hook on the VM thread.
///
/// The worker writes the terminal [`signal`](IoOpShared::signal), the
/// guest-visible [`value`](IoOpShared::value), and any opened handle or
/// close target; the driver reflects the signal into the operation registry
/// and the VM wrapper reads the value out of the mailbox after the registry
/// drive returns terminal.
struct IoOpShared {
    cancelled: AtomicBool,
    worker_done: AtomicBool,
    signal: Mutex<Option<Result<(), String>>>,
    value: Mutex<Option<VmResult<CallReturn>>>,
    opened: Mutex<Option<IoHandle>>,
    target: Mutex<Option<ResourceHandle>>,
    waker: Mutex<Option<Waker>>,
    quiescence_waker: Mutex<Option<Waker>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    cancel_hook: Mutex<Option<Box<dyn FnOnce() + Send + 'static>>>,
}

impl IoOpShared {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            worker_done: AtomicBool::new(false),
            signal: Mutex::new(None),
            value: Mutex::new(None),
            opened: Mutex::new(None),
            target: Mutex::new(None),
            waker: Mutex::new(None),
            quiescence_waker: Mutex::new(None),
            worker: Mutex::new(None),
            cancel_hook: Mutex::new(None),
        }
    }

    fn mark_worker_done(&self) {
        self.worker_done.store(true, Ordering::Release);
        if let Some(waker) = self
            .quiescence_waker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            waker.wake();
        }
    }

    fn is_quiescent(&self) -> bool {
        self.worker_done.load(Ordering::Acquire)
    }

    fn register_quiescence_waker(&self, waker: &Waker) {
        let mut guard = self
            .quiescence_waker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.is_quiescent() {
            return;
        }
        *guard = Some(waker.clone());
        if self.is_quiescent()
            && let Some(waker) = guard.take()
        {
            waker.wake();
        }
    }

    fn install_cancel_hook(&self, hook: impl FnOnce() + Send + 'static) {
        let mut hook = Some(Box::new(hook) as Box<dyn FnOnce() + Send + 'static>);
        {
            let mut guard = self
                .cancel_hook
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !self.cancelled.load(Ordering::Acquire) {
                *guard = hook.take();
            }
        }
        if let Some(hook) = hook {
            hook();
        }
    }

    fn cancel_work(&self) {
        if let Some(hook) = self
            .cancel_hook
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            hook();
        }
    }

    fn set_worker(&self, worker: JoinHandle<()>) {
        *self
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(worker);
    }

    fn join_worker(&self) -> bool {
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        worker.is_some_and(|worker| worker.join().is_err())
    }

    /// The worker's terminal publish: stores the signal and wakes any
    /// registered waker (check-register-double-check in the driver's poll).
    fn publish(&self, signal: Result<(), String>) {
        *self
            .signal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(signal);
        if let Some(waker) = self
            .waker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            waker.wake();
        }
    }

    fn take_signal(&self) -> Option<Result<(), String>> {
        self.signal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    fn register_waker(&self, waker: &Waker) {
        *self
            .waker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(waker.clone());
    }

    /// The worker's failure path: records the guest-visible `VmError` in the
    /// value mailbox and publishes a textual signal for the operation driver.
    fn fail(&self, error: VmError) {
        let message = error.to_string();
        *self
            .value
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Err(error));
        self.publish(Err(message));
    }

    /// Publishes a terminal operation while preserving a guest-visible value
    /// (including an error that must still retire a close target).
    fn complete(&self, value: VmResult<CallReturn>) {
        *self
            .value
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(value);
        self.publish(Ok(()));
    }

    /// The worker's success path: records the guest-visible value and
    /// publishes a success signal.
    fn succeed(&self, value: CallReturn) {
        self.complete(Ok(value));
    }
}

impl Drop for IoOpShared {
    fn drop(&mut self) {
        let handle = self
            .opened
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(handle) = handle {
            let _ = close_io_handle(handle);
        }
    }
}

/// A concrete [`HostOperation`] driver for one pending IO operation.
///
/// The worker thread performs the actual IO; this driver reflects the
/// worker's terminal signal into the operation registry and honours
/// cancellation by flagging the shared state so the worker aborts promptly.
struct IoOpDriver {
    shared: Arc<IoOpShared>,
    name: String,
}

impl IoOpDriver {
    fn new(shared: Arc<IoOpShared>, name: impl Into<String>) -> Self {
        Self {
            shared,
            name: name.into(),
        }
    }

    fn worker_failed(&self, message: impl Into<String>) -> Poll<OperationResult<()>> {
        Poll::Ready(Err(OperationError::new(
            OperationErrorCode::OperationDriverFailed,
            "io::operation",
            message,
        )))
    }
}

impl HostOperation for IoOpDriver {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
        if !self.shared.is_quiescent() {
            self.shared.register_waker(cx.waker());
            self.shared.register_quiescence_waker(cx.waker());
            if !self.shared.is_quiescent() {
                return Poll::Pending;
            }
        }
        if self.shared.cancelled.load(Ordering::Acquire) {
            return self.worker_failed(format!("{} was cancelled", self.name));
        }
        match self.shared.take_signal() {
            Some(Ok(())) => Poll::Ready(Ok(())),
            Some(Err(message)) => self.worker_failed(message),
            None => self.worker_failed(format!(
                "{} worker terminated without a completion signal",
                self.name
            )),
        }
    }

    fn cancel(&mut self, _reason: OperationCancelReason) -> OperationResult<()> {
        self.shared.cancelled.store(true, Ordering::Release);
        self.shared.cancel_work();
        Ok(())
    }

    fn is_quiescent(&self) -> bool {
        self.shared.is_quiescent()
    }

    fn register_quiescence_waker(&mut self, cx: &Context<'_>) {
        self.shared.register_quiescence_waker(cx.waker());
    }

    fn cancel_and_wait(&mut self, reason: OperationCancelReason) -> OperationResult<()> {
        self.cancel(reason)?;
        if self.shared.join_worker() {
            return Err(OperationError::new(
                OperationErrorCode::OperationDriverFailed,
                "io::operation",
                format!("{} worker panicked while cancelling", self.name),
            ));
        }
        Ok(())
    }
}

impl Drop for IoOpDriver {
    fn drop(&mut self) {
        if !self.shared.is_quiescent() {
            self.shared.cancelled.store(true, Ordering::Release);
            self.shared.cancel_work();
        }
        let _ = self.shared.join_worker();
    }
}

/// Completes one operation after the generic scope registry reports a
/// terminal outcome. The adapter owns the mailbox and any resource-table
/// mutation; the VM only invokes this opaque completion hook.
fn finish_io_operation(
    vm: &mut Vm,
    op_id: OperationId,
    outcome: OperationOutcome,
    shared: Arc<IoOpShared>,
) -> VmResult<CallReturn> {
    if matches!(outcome, OperationOutcome::Cancelled(_)) || shared.cancelled.load(Ordering::Acquire)
    {
        // The completion hook can be discarded after cancellation. Clean up
        // an opened child here as well as in `IoOpShared::drop`, so ownership
        // is released as soon as the worker has quiesced.
        if let Some(handle) = shared
            .opened
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = close_io_handle(handle);
        }
        return Err(VmError::HostError("IO operation cancelled".to_string()));
    }

    // An opened handle (io::open / io::popen) becomes a typed IO resource in
    // the scope; the script-visible handle is its raw resource token. The
    // resource state's drop guard closes the handle if table admission fails.
    if let Some(handle) = shared
        .opened
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        let resource = IoResource::new(handle);
        let token = vm
            .execution_scope()
            .push_resource(resource)
            .map_err(|error| {
                VmError::HostError(format!(
                    "scoped operation {} resource insert failed: {error}",
                    op_id.raw()
                ))
            })?;
        *shared
            .value
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(Ok(CallReturn::one(Value::Int(token.handle().raw() as i64))));
    }

    // A closed handle (io::close) retires the exact resource entry through
    // the generic scope close (exact-once). A close operation is successful
    // only once both the underlying handle and the scope entry are retired.
    if let Some(target) = shared
        .target
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        let progress = vm
            .execution_scope()
            .close_resource::<IoResource>(target, ResourceCloseReason::Requested)
            .map_err(VmError::ExecutionScope)?;
        if progress != CloseProgress::Ready {
            return Err(VmError::HostError(format!(
                "scoped operation {} resource retirement remained pending",
                op_id.raw()
            )));
        }
    }

    let value = shared
        .value
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    value.ok_or_else(|| {
        VmError::HostError(format!(
            "scoped operation {} completed without a result",
            op_id.raw()
        ))
    })?
}

/// Maximum UTF-8 byte length passed to `thread::Builder::name` for an IO
/// worker. The sanitized ASCII name also avoids embedded NULs and platform
/// surprises from an operation name supplied by a future caller.
const IO_WORKER_THREAD_NAME_MAX_LEN: usize = 32;

fn io_worker_thread_name(operation: &str) -> String {
    let mut name = String::from("pd-vm-io-");
    for byte in operation.bytes() {
        if name.len() == IO_WORKER_THREAD_NAME_MAX_LEN {
            break;
        }
        let safe = match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' => byte,
            _ => b'_',
        };
        name.push(safe as char);
    }
    name
}

struct WorkerCompletion {
    shared: Arc<IoOpShared>,
}

impl Drop for WorkerCompletion {
    fn drop(&mut self) {
        self.shared.mark_worker_done();
    }
}

/// Spawns a worker thread for an IO operation and registers its driver in
/// the VM's execution scope. Returns the packed [`OperationId`] raw value
/// to hand to the guest as the pending op id.
fn schedule_io_task(
    vm: &mut Vm,
    name: &str,
    work: impl FnOnce(&IoOpShared) + Send + 'static,
) -> VmResult<HostOpId> {
    let name = name.to_string();
    let shared = Arc::new(IoOpShared::new());
    let driver_shared = Arc::clone(&shared);
    let worker_shared = Arc::clone(&shared);
    let worker_name = name.clone();

    let op_id = vm
        .execution_scope()
        .start_operation(OperationSpec::new(IoOpDriver::new(driver_shared, name)))
        .map_err(|error| {
            VmError::HostError(format!(
                "failed to start io operation '{}': {error}",
                worker_name
            ))
        })?;
    if let Err(error) = vm.register_scoped_operation_completion(op_id, {
        let completion_shared = Arc::clone(&shared);
        move |vm, outcome| finish_io_operation(vm, op_id, outcome, completion_shared)
    }) {
        let _ = vm
            .execution_scope()
            .abort_operation(op_id, OperationCancelReason::Requested);
        return Err(error);
    }
    let raw = op_id.raw();
    let thread_name = io_worker_thread_name(&worker_name);

    std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let _completion = WorkerCompletion {
                shared: Arc::clone(&worker_shared),
            };
            if worker_shared.cancelled.load(Ordering::Acquire) {
                worker_shared.publish(Err(format!("io operation '{worker_name}' was cancelled")));
                return;
            }
            work(&worker_shared);
        })
        .map(|worker| {
            shared.set_worker(worker);
        })
        .map_err(|error| {
            // Roll back the registered operation so no orphaned op lingers.
            shared.mark_worker_done();
            vm.discard_scoped_operation_completion(op_id);
            let _ = vm
                .execution_scope()
                .abort_operation(op_id, OperationCancelReason::Requested);
            VmError::HostError(format!("failed to spawn io task: {error}"))
        })?;

    Ok(raw)
}

fn finish_io_worker(shared: &IoOpShared, handle: IoHandleLease, result: VmResult<CallReturn>) {
    let result = match handle.restore() {
        Ok(()) => result,
        Err(error) => Err(error),
    };
    match result {
        Ok(value) => shared.succeed(value),
        Err(error) => shared.fail(error),
    }
}

/// Opens a file handle for runtime I/O.
#[pd_host_function(name = "io::open")]
pub(super) fn builtin_io_open(
    vm: &mut Vm,
    path: &str,
    mode: &str,
) -> VmResult<HostCallResult<i64>> {
    let writes = matches!(mode, "w" | "a" | "r+" | "w+" | "a+");
    let path = authorize_blocking_io_path(vm, path, writes)?
        .display()
        .to_string();
    let mode = mode.to_string();
    let op_id = schedule_io_task(vm, "io::open", move |shared| {
        let mut options = OpenOptions::new();
        match mode.as_str() {
            "r" => {
                options.read(true);
            }
            "w" => {
                options.write(true).create(true).truncate(true);
            }
            "a" => {
                options.write(true).create(true).append(true);
            }
            "r+" => {
                options.read(true).write(true);
            }
            "w+" => {
                options.read(true).write(true).create(true).truncate(true);
            }
            "a+" => {
                options.read(true).write(true).create(true).append(true);
            }
            other => {
                shared.fail(VmError::HostError(format!(
                    "unsupported io_open mode '{other}', expected r/w/a/r+/w+/a+",
                )));
                return;
            }
        }

        match options.open(path) {
            Ok(file) => {
                *shared
                    .opened
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(IoHandle::File(file));
                shared.publish(Ok(()));
            }
            Err(err) => {
                shared.fail(VmError::HostError(format!("io_open failed: {err}")));
            }
        }
    })?;
    Ok(HostCallResult::Pending(op_id))
}

/// Starts a child process and returns a process-backed handle.
#[pd_host_function(name = "io::popen")]
pub(super) fn builtin_io_popen(
    vm: &mut Vm,
    command: &str,
    mode: &str,
) -> VmResult<HostCallResult<i64>> {
    if mode != "r" && mode != "w" {
        return Err(VmError::HostError(format!(
            "unsupported io_popen mode '{mode}', expected r or w"
        )));
    }
    if super::io_policy(vm)
        .as_ref()
        .is_some_and(|policy| !policy.allow_process)
    {
        return Err(VmError::HostError(
            "io_popen requires the process capability".to_string(),
        ));
    }
    let command = command.to_string();
    let mode = mode.to_string();
    let op_id = schedule_io_task(vm, "io::popen", move |shared| {
        let child = match spawn_shell_command(command.as_str(), mode.as_str()) {
            Ok(child) => child,
            Err(err) => {
                shared.fail(err);
                return;
            }
        };
        let child_guard = SpawnedChildGuard::new(child);
        let child_pid = child_guard.id();
        shared.install_cancel_hook(move || terminate_process_tree(child_pid));
        let handle = match mode.as_str() {
            "r" => {
                if child_guard.stdout_is_none() {
                    let err =
                        VmError::HostError("io_popen('r') did not provide stdout pipe".to_string());
                    shared.fail(err);
                    return;
                }
                IoHandle::PopenRead {
                    child: child_guard.into_child(),
                }
            }
            "w" => {
                if child_guard.stdin_is_none() {
                    let err =
                        VmError::HostError("io_popen('w') did not provide stdin pipe".to_string());
                    shared.fail(err);
                    return;
                }
                IoHandle::PopenWrite {
                    child: child_guard.into_child(),
                }
            }
            _ => unreachable!("mode validated above"),
        };
        *shared
            .opened
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handle);
        shared.publish(Ok(()));
    })?;
    Ok(HostCallResult::Pending(op_id))
}

/// Reads all remaining text from an I/O handle.
#[pd_host_function(name = "io::read_all")]
pub(super) fn builtin_io_read_all(vm: &mut Vm, handle_id: i64) -> VmResult<HostCallResult<String>> {
    let (_handle, resource) = io_resource_for_handle(vm, handle_id)?;
    let op_id = schedule_io_task(vm, "io::read_all", move |shared| {
        let mut handle = match resource.take_handle() {
            Some(handle) => handle,
            None => {
                let err = VmError::HostError("io_read_all handle is already closing".to_string());
                shared.fail(err);
                return;
            }
        };
        install_process_cancel_hook(shared, &handle, &resource.state);
        let mut out = String::new();
        let result = match &mut *handle {
            IoHandle::File(file) => file
                .read_to_string(&mut out)
                .map_err(|err| VmError::HostError(format!("io_read_all failed: {err}")))
                .map(|_| CallReturn::one(Value::string(out))),
            IoHandle::PopenRead { child } => match child.stdout.as_mut() {
                Some(stdout) => stdout
                    .read_to_string(&mut out)
                    .map_err(|err| VmError::HostError(format!("io_read_all failed: {err}")))
                    .map(|_| CallReturn::one(Value::string(out))),
                None => Err(VmError::HostError(
                    "io_read_all popen handle missing stdout".to_string(),
                )),
            },
            IoHandle::PopenWrite { .. } => Err(VmError::HostError(
                "io_read_all requires a readable handle".to_string(),
            )),
        };
        finish_io_worker(shared, handle, result);
    })?;
    Ok(HostCallResult::Pending(op_id))
}

/// Reads a single line of text from an I/O handle.
#[pd_host_function(name = "io::read_line")]
pub(super) fn builtin_io_read_line(
    vm: &mut Vm,
    handle_id: i64,
) -> VmResult<HostCallResult<String>> {
    let (_handle, resource) = io_resource_for_handle(vm, handle_id)?;
    let op_id = schedule_io_task(vm, "io::read_line", move |shared| {
        let mut handle = match resource.take_handle() {
            Some(handle) => handle,
            None => {
                let err = VmError::HostError("io_read_line handle is already closing".to_string());
                shared.fail(err);
                return;
            }
        };
        install_process_cancel_hook(shared, &handle, &resource.state);
        let result = match &mut *handle {
            IoHandle::File(file) => {
                read_line_from_reader(file).map(|line| CallReturn::one(Value::string(line)))
            }
            IoHandle::PopenRead { child } => match child.stdout.as_mut() {
                Some(stdout) => {
                    read_line_from_reader(stdout).map(|line| CallReturn::one(Value::string(line)))
                }
                None => Err(VmError::HostError(
                    "io_read_line popen handle missing stdout".to_string(),
                )),
            },
            IoHandle::PopenWrite { .. } => Err(VmError::HostError(
                "io_read_line requires a readable handle".to_string(),
            )),
        };
        finish_io_worker(shared, handle, result);
    })?;
    Ok(HostCallResult::Pending(op_id))
}

/// Writes text to an I/O handle.
#[pd_host_function(name = "io::write")]
pub(super) fn builtin_io_write(
    vm: &mut Vm,
    handle_id: i64,
    text: &str,
) -> VmResult<HostCallResult<i64>> {
    if super::io_policy(vm)
        .as_ref()
        .is_some_and(|policy| text.len() > policy.max_write_bytes)
    {
        return Err(VmError::HostError(
            "io_write exceeded write limit".to_string(),
        ));
    }
    let bytes = text.as_bytes().to_vec();
    let (_handle, resource) = io_resource_for_handle(vm, handle_id)?;
    let op_id = schedule_io_task(vm, "io::write", move |shared| {
        let mut handle = match resource.take_handle() {
            Some(handle) => handle,
            None => {
                let err = VmError::HostError("io_write handle is already closing".to_string());
                shared.fail(err);
                return;
            }
        };
        install_process_cancel_hook(shared, &handle, &resource.state);
        let result = match &mut *handle {
            IoHandle::File(file) => file
                .write(&bytes)
                .map_err(|err| VmError::HostError(format!("io_write failed: {err}")))
                .map(|written| CallReturn::one(Value::Int(written as i64))),
            IoHandle::PopenWrite { child } => match child.stdin.as_mut() {
                Some(stdin) => stdin
                    .write(&bytes)
                    .map_err(|err| VmError::HostError(format!("io_write failed: {err}")))
                    .map(|written| CallReturn::one(Value::Int(written as i64))),
                None => Err(VmError::HostError(
                    "io_write popen handle missing stdin".to_string(),
                )),
            },
            IoHandle::PopenRead { .. } => Err(VmError::HostError(
                "io_write requires a writable handle".to_string(),
            )),
        };
        finish_io_worker(shared, handle, result);
    })?;
    Ok(HostCallResult::Pending(op_id))
}

/// Flushes buffered output for an I/O handle.
#[pd_host_function(name = "io::flush")]
pub(super) fn builtin_io_flush(vm: &mut Vm, handle_id: i64) -> VmResult<HostCallResult<bool>> {
    let (_handle, resource) = io_resource_for_handle(vm, handle_id)?;
    let op_id = schedule_io_task(vm, "io::flush", move |shared| {
        let mut handle = match resource.take_handle() {
            Some(handle) => handle,
            None => {
                let err = VmError::HostError("io_flush handle is already closing".to_string());
                shared.fail(err);
                return;
            }
        };
        install_process_cancel_hook(shared, &handle, &resource.state);
        let result = match &mut *handle {
            IoHandle::File(file) => file
                .flush()
                .map_err(|err| VmError::HostError(format!("io_flush failed: {err}")))
                .map(|_| CallReturn::one(Value::Bool(true))),
            IoHandle::PopenWrite { child } => match child.stdin.as_mut() {
                Some(stdin) => stdin
                    .flush()
                    .map_err(|err| VmError::HostError(format!("io_flush failed: {err}")))
                    .map(|_| CallReturn::one(Value::Bool(true))),
                None => Err(VmError::HostError(
                    "io_flush popen handle missing stdin".to_string(),
                )),
            },
            IoHandle::PopenRead { .. } => Ok(CallReturn::one(Value::Bool(true))),
        };
        finish_io_worker(shared, handle, result);
    })?;
    Ok(HostCallResult::Pending(op_id))
}

/// Closes an I/O handle.
#[pd_host_function(name = "io::close")]
pub(super) fn builtin_io_close(vm: &mut Vm, handle_id: i64) -> VmResult<HostCallResult<bool>> {
    let (target, resource) = io_resource_for_handle(vm, handle_id)?;
    let op_id = schedule_io_task(vm, "io::close", move |shared| {
        // Close the underlying handle exactly once on the worker thread.
        let result = match resource.take_handle() {
            Some(handle) => {
                install_process_cancel_hook(shared, &handle, &resource.state);
                handle.close()
            }
            None => Err(VmError::HostError(
                "io_close handle is already closing".to_string(),
            )),
        };
        *shared
            .target
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(target);
        match result {
            Ok(()) => shared.succeed(CallReturn::one(Value::Bool(true))),
            Err(error) => shared.complete(Err(error)),
        }
    })?;
    Ok(HostCallResult::Pending(op_id))
}

/// Returns whether a file system path exists.
#[pd_host_function(name = "io::exists")]
pub(super) fn builtin_io_exists(vm: &mut Vm, path: &str) -> VmResult<HostCallResult<bool>> {
    let path = authorize_blocking_io_path(vm, path, false)?
        .display()
        .to_string();
    let op_id = schedule_io_task(vm, "io::exists", move |shared| {
        shared.succeed(CallReturn::one(Value::Bool(
            std::path::Path::new(path.as_str()).exists(),
        )));
    })?;
    Ok(HostCallResult::Pending(op_id))
}

struct SpawnedChildGuard {
    child: Option<Child>,
}

impl SpawnedChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn id(&self) -> u32 {
        self.child.as_ref().expect("child guard owns a child").id()
    }

    fn stdout_is_none(&self) -> bool {
        self.child
            .as_ref()
            .expect("child guard owns a child")
            .stdout
            .is_none()
    }

    fn stdin_is_none(&self) -> bool {
        self.child
            .as_ref()
            .expect("child guard owns a child")
            .stdin
            .is_none()
    }

    fn into_child(mut self) -> Child {
        self.child.take().expect("child guard owns a child")
    }
}

impl Drop for SpawnedChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = terminate_child_tree(child);
        }
    }
}

fn install_process_cancel_hook(
    shared: &IoOpShared,
    handle: &IoHandle,
    state: &Arc<IoResourceState>,
) {
    let pid = match handle {
        IoHandle::PopenRead { child } | IoHandle::PopenWrite { child } => child.id(),
        IoHandle::File(_) => return,
    };
    let state = Arc::clone(state);
    shared.install_cancel_hook(move || {
        // A cancelled process operation has already invalidated the process
        // stream. Marking the resource closed makes the worker lease reap the
        // child instead of restoring a killed, unreaped Child.
        state.mark_closed();
        terminate_process_tree(pid);
    });
}

fn terminate_process_tree(pid: u32) {
    let _ = terminate_process_tree_result(pid);
}

fn terminate_process_tree_result(pid: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let pid = libc::pid_t::try_from(pid).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid child pid")
        })?;
        // `spawn_shell_command` puts the shell in its own process group, so a
        // negative pid terminates the shell and descendants without touching
        // the VM process group.
        let result = unsafe { libc::kill(-pid, libc::SIGKILL) };
        if result == 0 {
            Ok(())
        } else {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                Ok(())
            } else {
                Err(error)
            }
        }
    }
    #[cfg(windows)]
    {
        let status = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("taskkill exited with {status}"),
            ))
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        Ok(())
    }
}

/// Terminates a child and reaps it. The only `wait` below is reached after a
/// tree termination signal and a direct leader kill have been attempted; an
/// already exited child is reaped by `try_wait` instead.
fn terminate_child_tree(child: &mut Child) -> VmResult<()> {
    let tree_error = terminate_process_tree_result(child.id()).err();
    let status = child
        .try_wait()
        .map_err(|error| VmError::HostError(format!("io_close popen status failed: {error}")))?;
    if status.is_some() {
        return Ok(());
    }

    let mut reaped = false;
    let direct_error = match child.kill() {
        Ok(()) => None,
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
            if child
                .try_wait()
                .map_err(|status_error| {
                    VmError::HostError(format!("io_close popen status failed: {status_error}"))
                })?
                .is_some()
            {
                reaped = true;
                None
            } else {
                Some(error)
            }
        }
        Err(error) => Some(error),
    };
    if let Some(error) = direct_error {
        return Err(VmError::HostError(format!(
            "io_close popen terminate failed: {error}"
        )));
    }

    if !reaped {
        child
            .wait()
            .map_err(|error| VmError::HostError(format!("io_close popen wait failed: {error}")))?;
    }
    if let Some(error) = tree_error {
        return Err(VmError::HostError(format!(
            "io_close popen process-tree terminate failed: {error}"
        )));
    }
    Ok(())
}

fn spawn_shell_command(command: &str, mode: &str) -> VmResult<Child> {
    let mut process = if cfg!(windows) {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    } else {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    };

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        process.process_group(0);
    }

    match mode {
        "r" => {
            process.stdout(Stdio::piped()).stdin(Stdio::null());
        }
        "w" => {
            process.stdin(Stdio::piped()).stdout(Stdio::null());
        }
        _ => {}
    }

    process
        .spawn()
        .map_err(|err| VmError::HostError(format!("io_popen failed: {err}")))
}

/// Parses a script-visible integer handle into a typed scope token and
/// returns the raw scope handle plus shared resource cells, validating
/// staleness and type through the generic typed table.
fn io_resource_for_handle(
    vm: &mut Vm,
    handle_id: i64,
) -> VmResult<(ResourceHandle, Arc<IoResource>)> {
    let handle = io_parse_handle(handle_id)?;
    let token = vm
        .execution_scope()
        .resources()
        .typed::<IoResource>(handle)
        .map_err(|error| {
            VmError::HostError(format!(
                "io handle {handle_id} is not a live IO handle: {error}"
            ))
        })?;
    let resource = vm
        .execution_scope()
        .resources()
        .get::<IoResource>(&token)
        .map_err(|error| {
            VmError::HostError(format!("io handle {handle_id} borrow failed: {error}"))
        })?;
    // Clone the shared resource state so the worker can take/restore the
    // handle while the resource itself stays in the scope table.
    Ok((
        handle,
        Arc::new(IoResource {
            state: Arc::clone(&resource.state),
        }),
    ))
}

fn io_parse_handle(handle_id: i64) -> VmResult<ResourceHandle> {
    if handle_id <= 0 {
        return Err(VmError::HostError(format!(
            "invalid io handle id {handle_id}; expected positive handle id"
        )));
    }
    ResourceHandle::from_raw(handle_id as u64)
        .map_err(|error| VmError::HostError(format!("invalid io handle id {handle_id}: {error}")))
}

/// Authorizes one IO path against the configured policy, mirroring the
/// async path: a policy with no matching allowed root denies the path.
fn authorize_blocking_io_path(vm: &Vm, path: &str, writes: bool) -> VmResult<PathBuf> {
    let requested = PathBuf::from(path);
    let Some(policy) = super::io_policy(vm) else {
        return Ok(requested);
    };
    if writes && !policy.allow_write {
        return Err(VmError::HostError(
            "io path write requires the write capability".to_string(),
        ));
    }
    let absolute = if requested.is_absolute() {
        requested
    } else {
        std::env::current_dir()
            .map_err(|error| VmError::HostError(format!("io path resolution failed: {error}")))?
            .join(requested)
    };
    let canonical = canonicalize_blocking_target(&absolute)?;
    for root in &policy.allowed_roots {
        let root = std::fs::canonicalize(Path::new(root)).map_err(|error| {
            VmError::HostError(format!(
                "io allowed root '{root}' cannot be resolved: {error}"
            ))
        })?;
        if canonical.starts_with(root) {
            return Ok(canonical);
        }
    }
    Err(VmError::HostError(format!(
        "io path '{}' is outside the allowed roots",
        canonical.display()
    )))
}

fn canonicalize_blocking_target(path: &Path) -> VmResult<PathBuf> {
    if path.exists() {
        return std::fs::canonicalize(path)
            .map_err(|error| VmError::HostError(format!("io path resolution failed: {error}")));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let canonical_parent = std::fs::canonicalize(parent)
        .map_err(|error| VmError::HostError(format!("io path resolution failed: {error}")))?;
    let name = path
        .file_name()
        .ok_or_else(|| VmError::HostError("io path has no file name".to_string()))?;
    Ok(canonical_parent.join(name))
}

fn close_io_handle(mut handle: IoHandle) -> VmResult<()> {
    match &mut handle {
        IoHandle::File(file) => file
            .flush()
            .map_err(|err| VmError::HostError(format!("io_close flush failed: {err}"))),
        IoHandle::PopenRead { child } => terminate_child_tree(child),
        IoHandle::PopenWrite { child } => {
            let _ = child.stdin.take();
            terminate_child_tree(child)
        }
    }
}

fn read_line_from_reader(reader: &mut impl Read) -> VmResult<String> {
    let mut bytes = Vec::new();
    let mut one = [0u8; 1];
    loop {
        let read = reader
            .read(&mut one)
            .map_err(|err| VmError::HostError(format!("io_read_line failed: {err}")))?;
        if read == 0 {
            break;
        }
        bytes.push(one[0]);
        if one[0] == b'\n' {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_worker_thread_name_is_sanitized_and_bounded() {
        assert_eq!(
            io_worker_thread_name("io::read_all"),
            "pd-vm-io-io__read_all"
        );
        let name = io_worker_thread_name("io::operation/with spaces\0 and a very long suffix");
        assert!(name.len() <= IO_WORKER_THREAD_NAME_MAX_LEN);
        assert!(name.is_ascii());
        assert!(
            name.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        );
        assert!(!name.contains('\0'));
    }

    #[test]
    fn io_driver_waits_for_worker_completion_before_reporting_ready() {
        let shared = Arc::new(IoOpShared::new());
        let release = Arc::new(AtomicBool::new(false));
        let worker_shared = Arc::clone(&shared);
        let worker_release = Arc::clone(&release);
        let worker = std::thread::spawn(move || {
            while !worker_release.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            worker_shared.publish(Ok(()));
            worker_shared.mark_worker_done();
        });
        shared.set_worker(worker);
        let mut driver = IoOpDriver::new(Arc::clone(&shared), "io::test");
        let mut cx = Context::from_waker(Waker::noop());

        assert!(matches!(driver.poll(&mut cx), Poll::Pending));
        assert!(!driver.is_quiescent());

        release.store(true, Ordering::Release);
        while !driver.is_quiescent() {
            std::thread::yield_now();
        }
        assert!(matches!(driver.poll(&mut cx), Poll::Ready(Ok(()))));
    }

    #[test]
    fn io_resource_close_stays_pending_while_worker_owns_handle() {
        let path = std::env::temp_dir().join(format!(
            "pd-vm-blocking-io-resource-close-{}",
            std::process::id()
        ));
        let file = std::fs::File::create(&path).expect("test file should open");
        let mut resource = IoResource::new(IoHandle::File(file));
        let worker_handle = resource.take_handle().expect("worker should take handle");
        let close = resource
            .begin_close(ResourceCloseReason::Requested)
            .expect("begin close should succeed");
        assert_eq!(close, CloseProgress::Pending);

        let wake_count = Arc::new(AtomicUsize::new(0));
        struct CloseWake(Arc<AtomicUsize>);
        impl std::task::Wake for CloseWake {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let waker = Waker::from(Arc::new(CloseWake(Arc::clone(&wake_count))));
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(
            HostResource::poll_close(&mut resource, &mut cx),
            Poll::Pending
        ));
        drop(worker_handle);
        assert_eq!(wake_count.load(Ordering::SeqCst), 1);

        assert!(matches!(
            HostResource::poll_close(&mut resource, &mut cx),
            Poll::Ready(Ok(()))
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn io_resource_worker_release_after_close_does_not_restore_handle() {
        let path = std::env::temp_dir().join(format!(
            "pd-vm-blocking-io-resource-worker-close-{}",
            std::process::id()
        ));
        let file = std::fs::File::create(&path).expect("test file should open");
        let mut resource = IoResource::new(IoHandle::File(file));
        let worker_handle = resource.take_handle().expect("worker should take handle");
        assert_eq!(
            resource
                .begin_close(ResourceCloseReason::Requested)
                .expect("begin close should succeed"),
            CloseProgress::Pending
        );

        worker_handle
            .restore()
            .expect("worker cleanup after close should succeed");
        assert_eq!(resource.state.active_workers.load(Ordering::Acquire), 0);
        assert!(
            resource
                .state
                .handle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_none()
        );
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(
            HostResource::poll_close(&mut resource, &mut cx),
            Poll::Ready(Ok(()))
        ));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    struct ProcessTreeCleanup {
        leader: u32,
        descendant: i32,
        marker: PathBuf,
    }

    #[cfg(unix)]
    impl Drop for ProcessTreeCleanup {
        fn drop(&mut self) {
            terminate_process_tree(self.leader);
            unsafe {
                libc::kill(self.descendant, libc::SIGKILL);
            }
            let _ = std::fs::remove_file(&self.marker);
        }
    }

    #[cfg(unix)]
    fn live_popen_for_test() -> (SpawnedChildGuard, PathBuf, i32) {
        static TEST_PROCESS_COUNTER: AtomicUsize = AtomicUsize::new(0);
        let suffix = TEST_PROCESS_COUNTER.fetch_add(1, Ordering::Relaxed);
        let marker = std::env::temp_dir().join(format!(
            "pd-vm-blocking-io-popen-{0}-{suffix}.marker",
            std::process::id()
        ));
        let command = format!(
            r#"sleep 30 & child=$!; printf '%s\n' "$child" > '{}'; wait "$child""#,
            marker.display()
        );
        let child = spawn_shell_command(&command, "r").expect("test popen should spawn");
        let guard = SpawnedChildGuard::new(child);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let descendant = loop {
            if let Ok(contents) = std::fs::read_to_string(&marker)
                && let Ok(pid) = contents.trim().parse::<i32>()
            {
                break pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "popen test child did not publish its descendant marker"
            );
            std::thread::yield_now();
        };
        (guard, marker, descendant)
    }

    #[cfg(unix)]
    fn process_is_running(pid: i32) -> bool {
        let path = format!("/proc/{pid}/stat");
        let Ok(stat) = std::fs::read_to_string(path) else {
            return false;
        };
        let Some((_, state)) = stat.split_once(") ") else {
            return true;
        };
        !state.starts_with('Z')
    }

    #[cfg(unix)]
    fn wait_for_process_exit(pid: i32) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while process_is_running(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "popen descendant remained alive after process-tree close"
            );
            std::thread::yield_now();
        }
    }

    #[cfg(unix)]
    #[test]
    fn closing_live_popen_terminates_and_reaps_the_process_tree() {
        let (child, marker, descendant) = live_popen_for_test();
        let _cleanup = ProcessTreeCleanup {
            leader: child.id(),
            descendant,
            marker: marker.clone(),
        };
        close_io_handle(IoHandle::PopenRead {
            child: child.into_child(),
        })
        .expect("closing a live popen must terminate and reap it");
        wait_for_process_exit(descendant);
        let _ = std::fs::remove_file(marker);
    }

    #[cfg(unix)]
    #[test]
    fn failed_worker_lease_drop_terminates_and_reaps_process_tree() {
        let (child, marker, descendant) = live_popen_for_test();
        let leader = child.id();
        let _cleanup = ProcessTreeCleanup {
            leader,
            descendant,
            marker: marker.clone(),
        };
        let resource = IoResource::new(IoHandle::PopenRead {
            child: child.into_child(),
        });
        let worker_handle = resource.take_handle().expect("worker should take handle");
        drop(worker_handle);
        wait_for_process_exit(descendant);
        let _ = std::fs::remove_file(marker);
    }

    #[cfg(unix)]
    #[test]
    fn failed_resource_handoff_terminates_and_reaps_opened_process_tree() {
        let (child, marker, descendant) = live_popen_for_test();
        let _cleanup = ProcessTreeCleanup {
            leader: child.id(),
            descendant,
            marker: marker.clone(),
        };
        let mut vm = Vm::new(crate::Program::new(
            Vec::new(),
            vec![crate::OpCode::Ret as u8],
        ));
        let shared = Arc::new(IoOpShared::new());
        *shared
            .opened
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(IoHandle::PopenRead {
            child: child.into_child(),
        });
        let op_id = vm
            .execution_scope()
            .start_operation(OperationSpec::new(IoOpDriver::new(
                Arc::clone(&shared),
                "io::test-handoff",
            )))
            .expect("test operation should start");
        vm.execution_scope()
            .begin_close(ResourceCloseReason::Requested)
            .expect("scope should start closing");
        let error = finish_io_operation(&mut vm, op_id, OperationOutcome::Completed, shared)
            .expect_err("resource insertion into a closing scope must fail");
        assert!(error.to_string().contains("resource insert failed"));
        wait_for_process_exit(descendant);
        let _ = std::fs::remove_file(marker);
    }

    #[test]
    fn close_completion_does_not_report_success_while_resource_retirement_is_pending() {
        let path = std::env::temp_dir().join(format!(
            "pd-vm-blocking-io-close-pending-{}",
            std::process::id()
        ));
        let file = std::fs::File::create(&path).expect("test file should open");
        let resource = IoResource::new(IoHandle::File(file));
        let worker_resource = IoResource {
            state: Arc::clone(&resource.state),
        };
        let mut vm = Vm::new(crate::Program::new(
            Vec::new(),
            vec![crate::OpCode::Ret as u8],
        ));
        let token = vm
            .execution_scope()
            .push_resource(resource)
            .expect("resource should insert");
        let worker_handle = worker_resource
            .take_handle()
            .expect("worker should take handle");
        let shared = Arc::new(IoOpShared::new());
        *shared
            .target
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(token.handle());
        *shared
            .value
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(Ok(CallReturn::one(Value::Bool(true))));
        let op_id = vm
            .execution_scope()
            .start_operation(OperationSpec::new(IoOpDriver::new(
                Arc::clone(&shared),
                "io::test-close-pending",
            )))
            .expect("test operation should start");

        let error = finish_io_operation(&mut vm, op_id, OperationOutcome::Completed, shared)
            .expect_err("pending resource retirement must not report success");
        assert!(error.to_string().contains("remained pending"));

        worker_handle
            .restore()
            .expect("worker cleanup after close should succeed");
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(
            vm.execution_scope()
                .resources_mut()
                .poll_close(token, &mut cx),
            Poll::Ready(Ok(()))
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn close_completion_reports_scope_retirement_errors() {
        let mut vm = Vm::new(crate::Program::new(
            Vec::new(),
            vec![crate::OpCode::Ret as u8],
        ));
        let file = std::fs::File::open("Cargo.toml").expect("test file should open");
        let token = vm
            .execution_scope()
            .push_resource(IoResource::new(IoHandle::File(file)))
            .expect("resource should insert");
        vm.execution_scope()
            .close_resource::<IoResource>(token.handle(), ResourceCloseReason::Requested)
            .expect("initial close should retire resource");

        let shared = Arc::new(IoOpShared::new());
        *shared
            .target
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(token.handle());
        *shared
            .value
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(Ok(CallReturn::one(Value::Bool(true))));
        let op_id = vm
            .execution_scope()
            .start_operation(OperationSpec::new(IoOpDriver::new(
                Arc::clone(&shared),
                "io::test-close",
            )))
            .expect("test operation should start");
        let error = finish_io_operation(&mut vm, op_id, OperationOutcome::Completed, shared)
            .expect_err("stale scope retirement must be visible to the caller");
        assert!(error.to_string().contains("execution scope"));
    }
}
