use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread::JoinHandle;

use pd_host_function::pd_host_function;

use super::HostCallResult;
use crate::vm::operation::driver::HostOperation;
use crate::vm::operation::error::{OperationError, OperationErrorCode, OperationResult};
use crate::vm::operation::reason::OperationCancelReason;
use crate::vm::operation::{OperationId, OperationSpec};
use crate::vm::resource::close::{CloseProgress, HostResource};
use crate::vm::resource::error::{ResourceError, ResourceErrorCode, ResourceResult};
use crate::vm::resource::{ResourceCloseReason, ResourceHandle};
use crate::vm::{CallReturn, HostOpId, Value, Vm, VmError, VmResult};

/// Per-VM IO host state.
///
/// Live IO handles are typed [`IoResource`]s owned by the VM's execution
/// scope; in-flight IO work is driven by concrete [`HostOperation`] drivers
/// registered in the same scope. The only state kept here is the per-op
/// completion mailbox that carries the guest-visible result value from the
/// worker thread back to [`poll_builtin_io_op`]. Polling and cancellation
/// of the operations themselves go directly through the scope's operation
/// registry — this map is a value mailbox, not a poller table.
pub(crate) struct IoState {
    /// Packed [`OperationId::raw`] -> completion mailbox for pending IO ops.
    pending_results: HashMap<HostOpId, Arc<IoOpShared>>,
}

impl Default for IoState {
    fn default() -> Self {
        Self {
            pending_results: HashMap::new(),
        }
    }
}

/// A file / child-process backed IO handle.
pub(super) enum IoHandle {
    File(std::fs::File),
    PopenRead { child: Child },
    PopenWrite { child: Child },
}

/// The typed resource stored in the execution scope for one IO handle.
///
/// The handle lives behind an `Arc<Mutex<Option<...>>>` so a worker thread
/// performing read/write/flush/close can transiently take the handle while
/// the resource itself stays in the scope table. Closing is exact-once: the
/// first close (via `io::close` worker or the generic scope close) takes the
/// handle and releases the OS resource.
struct IoResource {
    handle: Arc<Mutex<Option<IoHandle>>>,
    closed: Arc<AtomicBool>,
}

impl IoResource {
    fn new(handle: IoHandle) -> Self {
        Self {
            handle: Arc::new(Mutex::new(Some(handle))),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Takes the inner handle for a worker thread (exact-once per close).
    fn take_handle(&self) -> Option<IoHandle> {
        self.handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// Restores a handle a worker took, unless the resource is already
    /// closing — in which case the handle is dropped to release the OS
    /// resource rather than re-inserted into a closing resource.
    fn restore_handle(&self, handle: IoHandle) {
        if self.closed.load(Ordering::SeqCst) {
            let _ = close_io_handle(handle);
            return;
        }
        *self
            .handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handle);
    }
}

impl HostResource for IoResource {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(handle) = self.take_handle() {
            close_io_handle(handle).map_err(|error| {
                ResourceError::new(
                    ResourceErrorCode::ResourceCleanupFailed,
                    "io::resource",
                    error.to_string(),
                )
            })?;
        }
        Ok(CloseProgress::Ready)
    }
}

/// Shared state between one IO worker thread, its [`IoOpDriver`] operation,
/// and [`poll_builtin_io_op`] on the VM thread.
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
        if self.is_quiescent() {
            if let Some(waker) = guard.take() {
                waker.wake();
            }
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

    /// The worker's success path: records the guest-visible value and
    /// publishes a success signal.
    fn succeed(&self, value: CallReturn) {
        *self
            .value
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Ok(value));
        self.publish(Ok(()));
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

/// Cancels one pending builtin IO operation through the execution scope.
pub(super) fn cancel_pending_op(vm: &mut Vm, op_id: HostOpId) {
    let Ok(id) = OperationId::from_raw(op_id) else {
        return;
    };
    // Drop the completion mailbox; the operation's driver is cancelled
    // through the registry (which forwards to the driver's `cancel`).
    vm.host.io_state.pending_results.remove(&op_id);
    let _ = vm
        .execution_scope()
        .cancel_operation(id, OperationCancelReason::Requested);
}

/// Polls one pending builtin IO operation through the execution scope's
/// operation registry, delivering the worker's guest-visible value.
pub(super) fn poll_builtin_io_op(
    vm: &mut Vm,
    op_id: HostOpId,
    cx: &mut Context<'_>,
) -> Poll<VmResult<CallReturn>> {
    let id = match OperationId::from_raw(op_id) {
        Ok(id) => id,
        Err(error) => {
            return Poll::Ready(Err(VmError::HostError(format!(
                "invalid builtin io op {op_id}: {error}"
            ))));
        }
    };

    let poll_result = vm.execution_scope().poll_operation(id, cx);
    match poll_result {
        Poll::Pending => Poll::Pending,
        Poll::Ready(Err(error)) => {
            vm.host.io_state.pending_results.remove(&op_id);
            Poll::Ready(Err(VmError::HostError(format!(
                "builtin io op {op_id} failed: {error}"
            ))))
        }
        Poll::Ready(Ok(outcome)) => {
            // The worker wrote the authoritative guest-visible result into
            // the completion mailbox before signalling terminal.
            let Some(shared) = vm.host.io_state.pending_results.remove(&op_id) else {
                return Poll::Ready(Err(VmError::HostError(format!(
                    "builtin io op {op_id} has no completion mailbox"
                ))));
            };
            if matches!(
                outcome,
                crate::vm::operation::driver::OperationOutcome::Cancelled(_)
            ) || shared.cancelled.load(Ordering::Acquire)
            {
                return Poll::Ready(Err(VmError::HostError(
                    "IO operation cancelled".to_string(),
                )));
            }

            // An opened handle (io::open / io::popen) becomes a typed IO
            // resource in the scope; the script-visible handle is its raw
            // resource token.
            if let Some(handle) = shared
                .opened
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                let token = match vm.execution_scope().push_resource(IoResource::new(handle)) {
                    Ok(token) => token,
                    Err(error) => {
                        return Poll::Ready(Err(VmError::HostError(format!(
                            "builtin io op {op_id} resource insert failed: {error}"
                        ))));
                    }
                };
                *shared
                    .value
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some(Ok(CallReturn::one(Value::Int(token.handle().raw() as i64))));
            }

            // A closed handle (io::close) retires the exact resource entry
            // through the generic scope close (exact-once).
            if let Some(target) = shared
                .target
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                let _ = vm
                    .execution_scope()
                    .close_resource::<IoResource>(target, ResourceCloseReason::Requested);
            }

            let value = shared
                .value
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            match value {
                Some(value) => Poll::Ready(value),
                None => Poll::Ready(Err(VmError::HostError(format!(
                    "builtin io op {op_id} completed without a result"
                )))),
            }
        }
    }
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
            let _ = vm
                .execution_scope()
                .abort_operation(op_id, OperationCancelReason::Requested);
            VmError::HostError(format!("failed to spawn io task: {error}"))
        })?;

    vm.host.io_state.pending_results.insert(raw, shared);
    Ok(raw)
}

/// Opens a file handle for runtime I/O.
#[pd_host_function(name = "io::open")]
pub(super) fn builtin_io_open(
    vm: &mut Vm,
    path: &str,
    mode: &str,
) -> VmResult<HostCallResult<i64>> {
    let path = path.to_string();
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
        let child_pid = child.id();
        shared.install_cancel_hook(move || terminate_process_tree(child_pid));
        let handle = match mode.as_str() {
            "r" => {
                if child.stdout.is_none() {
                    let err =
                        VmError::HostError("io_popen('r') did not provide stdout pipe".to_string());
                    shared.fail(err);
                    return;
                }
                IoHandle::PopenRead { child }
            }
            "w" => {
                if child.stdin.is_none() {
                    let err =
                        VmError::HostError("io_popen('w') did not provide stdin pipe".to_string());
                    shared.fail(err);
                    return;
                }
                IoHandle::PopenWrite { child }
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
        install_process_cancel_hook(shared, &handle);
        let mut out = String::new();
        let result = match &mut handle {
            IoHandle::File(file) => file
                .read_to_string(&mut out)
                .map_err(|err| VmError::HostError(format!("io_read_all failed: {err}")))
                .map(|_| CallReturn::one(Value::string(out))),
            IoHandle::PopenRead { child } => {
                let stdout = match child.stdout.as_mut() {
                    Some(stdout) => stdout,
                    None => {
                        resource.restore_handle(handle);
                        let err = VmError::HostError(
                            "io_read_all popen handle missing stdout".to_string(),
                        );
                        shared.fail(err);
                        return;
                    }
                };
                stdout
                    .read_to_string(&mut out)
                    .map_err(|err| VmError::HostError(format!("io_read_all failed: {err}")))
                    .map(|_| CallReturn::one(Value::string(out)))
            }
            IoHandle::PopenWrite { .. } => Err(VmError::HostError(
                "io_read_all requires a readable handle".to_string(),
            )),
        };
        resource.restore_handle(handle);
        match result {
            Ok(value) => {
                shared.succeed(value);
            }
            Err(err) => {
                shared.fail(err);
            }
        }
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
        install_process_cancel_hook(shared, &handle);
        let result = match &mut handle {
            IoHandle::File(file) => {
                read_line_from_reader(file).map(|line| CallReturn::one(Value::string(line)))
            }
            IoHandle::PopenRead { child } => {
                let stdout = match child.stdout.as_mut() {
                    Some(stdout) => stdout,
                    None => {
                        resource.restore_handle(handle);
                        let err = VmError::HostError(
                            "io_read_line popen handle missing stdout".to_string(),
                        );
                        shared.fail(err);
                        return;
                    }
                };
                read_line_from_reader(stdout).map(|line| CallReturn::one(Value::string(line)))
            }
            IoHandle::PopenWrite { .. } => Err(VmError::HostError(
                "io_read_line requires a readable handle".to_string(),
            )),
        };
        resource.restore_handle(handle);
        match result {
            Ok(value) => {
                shared.succeed(value);
            }
            Err(err) => {
                shared.fail(err);
            }
        }
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
        install_process_cancel_hook(shared, &handle);
        let result = match &mut handle {
            IoHandle::File(file) => file
                .write(&bytes)
                .map_err(|err| VmError::HostError(format!("io_write failed: {err}")))
                .map(|written| CallReturn::one(Value::Int(written as i64))),
            IoHandle::PopenWrite { child } => {
                let stdin = match child.stdin.as_mut() {
                    Some(stdin) => stdin,
                    None => {
                        resource.restore_handle(handle);
                        let err =
                            VmError::HostError("io_write popen handle missing stdin".to_string());
                        shared.fail(err);
                        return;
                    }
                };
                stdin
                    .write(&bytes)
                    .map_err(|err| VmError::HostError(format!("io_write failed: {err}")))
                    .map(|written| CallReturn::one(Value::Int(written as i64)))
            }
            IoHandle::PopenRead { .. } => Err(VmError::HostError(
                "io_write requires a writable handle".to_string(),
            )),
        };
        resource.restore_handle(handle);
        match result {
            Ok(value) => {
                shared.succeed(value);
            }
            Err(err) => {
                shared.fail(err);
            }
        }
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
        install_process_cancel_hook(shared, &handle);
        let result = match &mut handle {
            IoHandle::File(file) => file
                .flush()
                .map_err(|err| VmError::HostError(format!("io_flush failed: {err}")))
                .map(|_| CallReturn::one(Value::Bool(true))),
            IoHandle::PopenWrite { child } => {
                let stdin = match child.stdin.as_mut() {
                    Some(stdin) => stdin,
                    None => {
                        resource.restore_handle(handle);
                        let err =
                            VmError::HostError("io_flush popen handle missing stdin".to_string());
                        shared.fail(err);
                        return;
                    }
                };
                stdin
                    .flush()
                    .map_err(|err| VmError::HostError(format!("io_flush failed: {err}")))
                    .map(|_| CallReturn::one(Value::Bool(true)))
            }
            IoHandle::PopenRead { .. } => Ok(CallReturn::one(Value::Bool(true))),
        };
        resource.restore_handle(handle);
        match result {
            Ok(value) => {
                shared.succeed(value);
            }
            Err(err) => {
                shared.fail(err);
            }
        }
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
                install_process_cancel_hook(shared, &handle);
                close_io_handle(handle)
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
            Ok(()) => {
                shared.succeed(CallReturn::one(Value::Bool(true)));
            }
            Err(err) => {
                shared.fail(err);
            }
        }
    })?;
    Ok(HostCallResult::Pending(op_id))
}

/// Returns whether a file system path exists.
#[pd_host_function(name = "io::exists")]
pub(super) fn builtin_io_exists(vm: &mut Vm, path: &str) -> VmResult<HostCallResult<bool>> {
    let path = path.to_string();
    let op_id = schedule_io_task(vm, "io::exists", move |shared| {
        shared.succeed(CallReturn::one(Value::Bool(
            std::path::Path::new(path.as_str()).exists(),
        )));
    })?;
    Ok(HostCallResult::Pending(op_id))
}

fn install_process_cancel_hook(shared: &IoOpShared, handle: &IoHandle) {
    let pid = match handle {
        IoHandle::PopenRead { child } | IoHandle::PopenWrite { child } => child.id(),
        IoHandle::File(_) => return,
    };
    shared.install_cancel_hook(move || terminate_process_tree(pid));
}

fn terminate_process_tree(pid: u32) {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return;
        };
        // `spawn_shell_command` puts the shell in its own process group, so a
        // negative pid terminates the shell and descendants without touching
        // the VM process group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .status();
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
    }
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
    // Clone the shared cells so the worker can take/restore the handle while
    // the resource itself stays in the scope table.
    Ok((
        handle,
        Arc::new(IoResource {
            handle: Arc::clone(&resource.handle),
            closed: Arc::clone(&resource.closed),
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

fn close_io_handle(mut handle: IoHandle) -> VmResult<()> {
    match &mut handle {
        IoHandle::File(file) => {
            file.flush().ok();
        }
        IoHandle::PopenRead { child } => {
            child
                .wait()
                .map_err(|err| VmError::HostError(format!("io_close popen wait failed: {err}")))?;
        }
        IoHandle::PopenWrite { child } => {
            let _ = child.stdin.take();
            child
                .wait()
                .map_err(|err| VmError::HostError(format!("io_close popen wait failed: {err}")))?;
        }
    }
    Ok(())
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
}
