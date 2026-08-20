//! Async (tokio-based) IO builtin implementations.
//!
//! Uses the same concrete [`HostResource`] types as the blocking path:
//! [`IoFileResource`], [`IoProcessResource`], [`IoPipeResource`] stored in
//! the execution scope via `push_resource_with_key` /
//! `push_child_resource_with_key`. Operations use [`HostOperation`] drivers
//! and the scope's [`OperationRegistry`].
//!
//! Unlike the blocking path, the async path uses tokio types for file,
//! process, and pipe handles. All blocking I/O is offloaded to scoped
//! worker threads via [`ThreadedOperation`] and [`IoWorkerResource`], so
//! the VM thread never calls [`tokio::runtime::Handle::block_on`] directly.
//! A dedicated worker thread may create/use a tokio runtime and `block_on`
//! there; the VM thread always returns `Pending` promptly.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::task::{Context, Poll};
use std::thread::JoinHandle;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[cfg(unix)]
use libc;

use pd_host_function::pd_host_function;

use super::super::HostCallResult;
use super::ops::{
    CloseCompletionOperation, ReadyOperation, ThreadedOperation, ThreadedWorkerSignal,
};
use super::worker::IoWorkerResource;
use crate::host_api::ResourceTypeKey;
use crate::vm::operation::OperationSpec;
use crate::vm::resource::{
    CloseProgress, HostResource, ResourceCloseReason, ResourceHandle, ResourceResult,
};
use crate::vm::{CallReturn, Value, Vm, VmError, VmResult};

// ---- HostResource implementations for IO resources (async-aware) ----

/// A file handle stored as a concrete HostResource.
pub(crate) struct IoFileResource {
    handle: Mutex<Option<std::fs::File>>,
    close_worker: Mutex<Option<JoinHandle<()>>>,
    closed: AtomicBool,
    /// Shared flag set by `poll_close` when the close worker finishes.
    close_completion: Arc<AtomicBool>,
}

impl HostResource for IoFileResource {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(super::io_file_key())
    }

    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.closed.store(true, Ordering::SeqCst);
        // Take the file handle and spawn a worker to flush/close it.
        // This ensures the VM thread never blocks on file I/O during close.
        let file = self.handle.lock().unwrap_or_else(|e| e.into_inner()).take();
        let close_completion = self.close_completion.clone();
        if let Some(mut file) = file {
            let handle = std::thread::Builder::new()
                .name("io-file-close".into())
                .spawn(move || {
                    let _ = file.flush();
                    // Signal completion before the file handle drops.
                    close_completion.store(true, Ordering::SeqCst);
                    // file is dropped here, which closes the OS handle.
                })
                .expect("io file close worker must spawn");
            *self.close_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        } else {
            // No file handle to close; signal completion immediately.
            close_completion.store(true, Ordering::SeqCst);
        }
        Ok(CloseProgress::Pending)
    }

    fn poll_close(&mut self, _cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        let mut guard = self.close_worker.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(handle) = guard.as_ref() {
            if !handle.is_finished() {
                return Poll::Pending;
            }
        }
        // Worker finished; join to observe panics.
        if let Some(handle) = guard.take() {
            let _ = handle.join();
        }
        // Signal completion to any waiting CloseCompletionOperation.
        self.close_completion.store(true, Ordering::SeqCst);
        Poll::Ready(Ok(()))
    }
}

impl IoFileResource {
    fn new(file: std::fs::File) -> Self {
        Self {
            handle: Mutex::new(Some(file)),
            close_worker: Mutex::new(None),
            closed: AtomicBool::new(false),
            close_completion: Arc::new(AtomicBool::new(false)),
        }
    }

    fn with_handle_mut<T>(
        &self,
        apply: impl FnOnce(&mut std::fs::File) -> VmResult<T>,
    ) -> VmResult<T> {
        let mut guard = self
            .handle
            .lock()
            .map_err(|_| VmError::HostError("io resource lock was poisoned".to_string()))?;
        let handle = guard
            .as_mut()
            .ok_or_else(|| VmError::HostError("io resource is already closing".to_string()))?;
        apply(handle)
    }
}

/// A child process resource.
pub(crate) struct IoProcessResource {
    child: Mutex<Option<std::process::Child>>,
    close_worker: Mutex<Option<JoinHandle<()>>>,
    process_id: u32,
    closed: AtomicBool,
    /// Shared flag set by `poll_close` when the close worker finishes.
    close_completion: Arc<AtomicBool>,
}

impl HostResource for IoProcessResource {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(super::io_process_key())
    }

    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.closed.store(true, Ordering::SeqCst);
        // Take the child process and spawn a worker to kill/wait it.
        // This ensures the VM thread never blocks on process teardown.
        let child = self.child.lock().unwrap_or_else(|e| e.into_inner()).take();
        let process_id = self.process_id;
        let close_completion = self.close_completion.clone();
        if let Some(mut child) = child {
            let handle = std::thread::Builder::new()
                .name("io-process-close".into())
                .spawn(move || {
                    terminate_process_group(process_id);
                    let _ = child.kill();
                    let _ = child.wait();
                    // Signal completion after the process is fully cleaned up.
                    close_completion.store(true, Ordering::SeqCst);
                })
                .expect("io process close worker must spawn");
            *self.close_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        } else {
            close_completion.store(true, Ordering::SeqCst);
        }
        Ok(CloseProgress::Pending)
    }

    fn poll_close(&mut self, _cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        let mut guard = self.close_worker.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(handle) = guard.as_ref() {
            if !handle.is_finished() {
                return Poll::Pending;
            }
        }
        if let Some(handle) = guard.take() {
            let _ = handle.join();
        }
        // Signal completion to any waiting CloseCompletionOperation.
        self.close_completion.store(true, Ordering::SeqCst);
        Poll::Ready(Ok(()))
    }
}

impl IoProcessResource {
    fn new(child: std::process::Child) -> Self {
        let process_id = child.id();
        Self {
            child: Mutex::new(Some(child)),
            close_worker: Mutex::new(None),
            process_id,
            closed: AtomicBool::new(false),
            close_completion: Arc::new(AtomicBool::new(false)),
        }
    }

    #[allow(dead_code)]
    fn process_id(&self) -> u32 {
        self.process_id
    }
}

/// A stdio pipe resource (child of a process resource).
pub(crate) struct IoPipeResource {
    pipe: Mutex<IoPipeInner>,
    closed: AtomicBool,
}

enum IoPipeInner {
    Read(std::process::ChildStdout),
    Write(std::process::ChildStdin),
    Closed,
}

impl HostResource for IoPipeResource {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(super::io_pipe_key())
    }

    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.closed.store(true, Ordering::SeqCst);
        *self.pipe.lock().unwrap_or_else(|e| e.into_inner()) = IoPipeInner::Closed;
        // Pipe state close is immediate — dropping the pipe handle is non-blocking.
        Ok(CloseProgress::Ready)
    }

    fn poll_close(&mut self, _cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        Poll::Ready(Ok(()))
    }
}

impl IoPipeResource {
    fn new_read(pipe: std::process::ChildStdout) -> Self {
        Self {
            pipe: Mutex::new(IoPipeInner::Read(pipe)),
            closed: AtomicBool::new(false),
        }
    }

    fn new_write(pipe: std::process::ChildStdin) -> Self {
        Self {
            pipe: Mutex::new(IoPipeInner::Write(pipe)),
            closed: AtomicBool::new(false),
        }
    }

    /// Take the reader pipe handle, replacing with `Closed`.
    /// Used to offload the read to a worker thread.
    fn take_reader(&mut self) -> VmResult<std::process::ChildStdout> {
        let mut guard = self
            .pipe
            .lock()
            .map_err(|_| VmError::HostError("io pipe lock was poisoned".to_string()))?;
        let old = std::mem::replace(&mut *guard, IoPipeInner::Closed);
        match old {
            IoPipeInner::Read(pipe) => Ok(pipe),
            IoPipeInner::Write(_) => Err(VmError::HostError(
                "io_read_all requires a readable handle".to_string(),
            )),
            IoPipeInner::Closed => Err(VmError::HostError("io pipe is already closed".to_string())),
        }
    }

    /// Take the writer pipe handle, replacing with `Closed`.
    /// Used to offload the write to a worker thread.
    fn take_writer(&mut self) -> VmResult<std::process::ChildStdin> {
        let mut guard = self
            .pipe
            .lock()
            .map_err(|_| VmError::HostError("io pipe lock was poisoned".to_string()))?;
        let old = std::mem::replace(&mut *guard, IoPipeInner::Closed);
        match old {
            IoPipeInner::Write(pipe) => Ok(pipe),
            IoPipeInner::Read(_) => Err(VmError::HostError(
                "io_write requires a writable handle".to_string(),
            )),
            IoPipeInner::Closed => Err(VmError::HostError("io pipe is already closed".to_string())),
        }
    }

    /// Restore a reader pipe handle that was taken for offloaded IO.
    /// The pipe is replaced from `Closed` back to `Read(pipe)`.
    fn restore_reader(&mut self, pipe: std::process::ChildStdout) {
        let mut guard = self.pipe.lock().unwrap_or_else(|e| e.into_inner());
        *guard = IoPipeInner::Read(pipe);
    }

    /// Restore a writer pipe handle that was taken for offloaded IO.
    fn restore_writer(&mut self, pipe: std::process::ChildStdin) {
        let mut guard = self.pipe.lock().unwrap_or_else(|e| e.into_inner());
        *guard = IoPipeInner::Write(pipe);
    }

    /// Check if this pipe is a read-only pipe (ChildStdout).
    fn is_read_pipe(&self) -> bool {
        let guard = self.pipe.lock().unwrap_or_else(|e| e.into_inner());
        matches!(&*guard, IoPipeInner::Read(_))
    }

    fn with_reader<T>(
        &self,
        apply: impl FnOnce(&mut std::process::ChildStdout) -> VmResult<T>,
    ) -> VmResult<T> {
        let mut guard = self
            .pipe
            .lock()
            .map_err(|_| VmError::HostError("io pipe lock was poisoned".to_string()))?;
        match &mut *guard {
            IoPipeInner::Read(pipe) => apply(pipe),
            IoPipeInner::Write(_) => Err(VmError::HostError(
                "io_read_all requires a readable handle".to_string(),
            )),
            IoPipeInner::Closed => Err(VmError::HostError("io pipe is already closed".to_string())),
        }
    }

    fn with_writer<T>(
        &self,
        apply: impl FnOnce(&mut std::process::ChildStdin) -> VmResult<T>,
    ) -> VmResult<T> {
        let mut guard = self
            .pipe
            .lock()
            .map_err(|_| VmError::HostError("io pipe lock was poisoned".to_string()))?;
        match &mut *guard {
            IoPipeInner::Write(pipe) => apply(pipe),
            IoPipeInner::Read(_) => Err(VmError::HostError(
                "io_write requires a writable handle".to_string(),
            )),
            IoPipeInner::Closed => Err(VmError::HostError("io pipe is already closed".to_string())),
        }
    }
}

// ---- Helpers: dispatch to file or pipe resource ----

fn with_file_or_pipe_mut<T>(
    vm: &mut Vm,
    handle: ResourceHandle,
    file_op: impl FnOnce(&mut IoFileResource) -> VmResult<T>,
    pipe_op: impl FnOnce(&mut IoPipeResource) -> VmResult<T>,
) -> VmResult<T> {
    let mut ctx = vm.host_context();
    let token = ctx.typed_resource::<IoFileResource>(handle);
    if let Ok(token) = token {
        let mut resource = ctx
            .resource_mut(&token)
            .map_err(|error| VmError::HostError(format!("io handle lookup failed: {error}")))?;
        file_op(resource.get())
    } else {
        let token = ctx
            .typed_resource::<IoPipeResource>(handle)
            .map_err(|error| VmError::HostError(format!("io handle lookup failed: {error}")))?;
        let mut resource = ctx
            .resource_mut(&token)
            .map_err(|error| VmError::HostError(format!("io handle lookup failed: {error}")))?;
        pipe_op(resource.get())
    }
}

/// Register a worker resource in the execution scope, returning its handle.
/// The worker resource is registered as a root (not a child) so it can be
/// independently cancelled/closed without blocking the parent resource close.
fn register_worker_resource(vm: &mut Vm, worker: IoWorkerResource) -> VmResult<i64> {
    let mut ctx = vm.host_context();
    let token = ctx
        .push_resource_with_key(worker, super::io_worker_key())
        .map_err(|error| {
            VmError::HostError(format!("io worker resource insert failed: {error}"))
        })?;
    let handle = token.handle();
    let raw = match handle.as_value() {
        Value::Int(value) => value,
        _ => unreachable!(),
    };
    ctx.mark_resource_guest_owned(handle).map_err(|error| {
        VmError::HostError(format!("io worker resource ownership failed: {error}"))
    })?;
    Ok(raw)
}

/// Register a ThreadedOperation + IoWorkerResource pair in the VM scope.
/// Returns the raw operation id. The worker is registered as a root resource.
fn register_operation_with_worker(
    vm: &mut Vm,
    operation: ThreadedOperation,
    worker: IoWorkerResource,
    resource_handle: Option<ResourceHandle>,
) -> VmResult<u64> {
    // Register worker resource first (non-destructive).
    // If this fails, no resources have been created yet.
    let _worker_handle = register_worker_resource(vm, worker)?;

    // Register the operation.
    let mut spec = OperationSpec::new(operation);
    if let Some(h) = resource_handle {
        spec = spec.with_resource(h);
    }
    let op_id = vm
        .host_context()
        .start_operation(spec)
        .map_err(|error| VmError::HostError(format!("io operation start failed: {error}")))?;
    Ok(op_id.raw())
}

// ---- IO builtin functions ----

/// Opens a file handle for runtime I/O.
/// The actual file open runs on a worker thread; the resource is created
/// by the PendingOpResult provider after the worker completes.
#[pd_host_function(name = "io::open")]
pub(crate) fn builtin_io_open(
    vm: &mut Vm,
    path: &str,
    mode: &str,
) -> VmResult<HostCallResult<i64>> {
    let writes = match mode {
        "r" => false,
        "w" | "a" | "r+" | "w+" | "a+" => true,
        other => {
            return Err(VmError::HostError(format!(
                "unsupported io_open mode '{other}', expected r/w/a/r+/w+/a+"
            )));
        }
    };
    let path = authorize_io_path(vm, path, writes)?;
    let mode = mode.to_string();
    let path_buf = path.to_path_buf();

    let shared: Arc<Mutex<Option<Result<std::fs::File, String>>>> = Arc::new(Mutex::new(None));
    let shared_worker = shared.clone();

    let (operation, tx, state) = ThreadedOperation::prepare("io::open");
    let raw_state = state.clone();

    // Register the worker resource and operation first.
    let worker = ThreadedOperation::spawn_worker(
        "io::open",
        raw_state,
        tx,
        move |state, tx: Sender<ThreadedWorkerSignal>| {
            if state.cancelled.load(Ordering::SeqCst) {
                let _ = tx.send(Err("io::open was cancelled before starting".to_string()));
                return;
            }
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
                    let _ = tx.send(Err(format!("unsupported io_open mode '{other}'")));
                    return;
                }
            }
            match options.open(&path_buf) {
                Ok(file) => {
                    *shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ok(file));
                    let _ = tx.send(Ok(()));
                }
                Err(err) => {
                    let _ = tx.send(Err(format!("io_open failed: {err}")));
                }
            }
        },
    );

    let raw = register_operation_with_worker(vm, operation, worker, None)?;

    let shared_provider = shared.clone();
    vm.host.register_pending_op_result(
        raw,
        Box::new(move |vm: &mut Vm| {
            match shared_provider
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                Some(Ok(file)) => {
                    let resource = IoFileResource::new(file);
                    let handle = insert_io_file_resource(vm, resource)?;
                    Ok(CallReturn::one(Value::Int(handle)))
                }
                Some(Err(msg)) => Err(VmError::HostError(msg)),
                None => Err(VmError::HostError(
                    "io::open worker did not produce a result".to_string(),
                )),
            }
        }),
    );

    Ok(HostCallResult::Pending(raw))
}

/// Starts a child process and returns a process-backed handle.
/// The process spawn runs on a worker thread.
#[pd_host_function(name = "io::popen")]
pub(crate) fn builtin_io_popen(
    vm: &mut Vm,
    command: &str,
    mode: &str,
) -> VmResult<HostCallResult<i64>> {
    if mode != "r" && mode != "w" {
        return Err(VmError::HostError(format!(
            "unsupported io_popen mode '{mode}', expected r or w"
        )));
    }
    if let Some(policy) = super::io_policy(vm) {
        if !policy.allow_process {
            return Err(VmError::HostError(
                "io_popen requires the process capability".to_string(),
            ));
        }
    }
    let command = command.to_string();
    let mode_str = mode.to_string();

    // Shared state to pass the result from worker to PendingOpResult.
    let shared: Arc<Mutex<Option<Result<std::process::Child, String>>>> =
        Arc::new(Mutex::new(None));
    let shared_worker = shared.clone();

    let (operation, tx, state) = ThreadedOperation::prepare("io::popen");
    let raw_state = state.clone();
    let mode_for_worker = mode_str.clone();

    let worker = ThreadedOperation::spawn_worker(
        "io::popen",
        raw_state,
        tx,
        move |state, tx: Sender<ThreadedWorkerSignal>| {
            if state.cancelled.load(Ordering::SeqCst) {
                let _ = tx.send(Err("io::popen was cancelled before starting".to_string()));
                return;
            }
            match spawn_shell_command(&command, &mode_for_worker) {
                Ok(child) => {
                    *shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ok(child));
                    let _ = tx.send(Ok(()));
                }
                Err(err) => {
                    let _ = tx.send(Err(format!("io_popen failed: {err}")));
                }
            }
        },
    );

    let raw = register_operation_with_worker(vm, operation, worker, None)?;

    let shared_provider = shared.clone();
    vm.host.register_pending_op_result(
        raw,
        Box::new(move |vm: &mut Vm| {
            let mut child = match shared_provider
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                Some(Ok(child)) => child,
                Some(Err(msg)) => return Err(VmError::HostError(msg)),
                None => {
                    return Err(VmError::HostError(
                        "io::popen worker did not produce a result".to_string(),
                    ));
                }
            };
            let handle = match mode_str.as_str() {
                "r" => {
                    let stdout = child.stdout.take().ok_or_else(|| {
                        VmError::HostError("io_popen('r') did not provide stdout pipe".to_string())
                    })?;
                    let process_resource = IoProcessResource::new(child);
                    let process_token = insert_io_process_resource(vm, process_resource)?;
                    let pipe_resource = IoPipeResource::new_read(stdout);
                    let pipe_token =
                        insert_io_pipe_child_resource(vm, pipe_resource, &process_token)?;
                    let handle = pipe_token.handle().as_value();
                    match handle {
                        Value::Int(value) => value,
                        _ => unreachable!(),
                    }
                }
                "w" => {
                    let stdin = child.stdin.take().ok_or_else(|| {
                        VmError::HostError("io_popen('w') did not provide stdin pipe".to_string())
                    })?;
                    let process_resource = IoProcessResource::new(child);
                    let process_token = insert_io_process_resource(vm, process_resource)?;
                    let pipe_resource = IoPipeResource::new_write(stdin);
                    let pipe_token =
                        insert_io_pipe_child_resource(vm, pipe_resource, &process_token)?;
                    let handle = pipe_token.handle().as_value();
                    match handle {
                        Value::Int(value) => value,
                        _ => unreachable!(),
                    }
                }
                _ => unreachable!("mode validated above"),
            };
            Ok(CallReturn::one(Value::Int(handle)))
        }),
    );

    Ok(HostCallResult::Pending(raw))
}

/// Reads all remaining text from an I/O handle.
/// The actual read runs on a worker thread.
#[pd_host_function(name = "io::read_all")]
pub(crate) fn builtin_io_read_all(vm: &mut Vm, handle_id: i64) -> VmResult<HostCallResult<String>> {
    let max_read_bytes = super::io_policy(vm).map(|policy| policy.max_read_bytes);
    let handle = resource_handle(handle_id)?;

    // Clone the file handle (or take the pipe handle) and offload the read
    // to a worker thread so the VM thread never blocks on IO.
    let shared: Arc<Mutex<Option<Result<String, String>>>> = Arc::new(Mutex::new(None));
    let shared_worker = shared.clone();

    let (operation, tx, state) = ThreadedOperation::prepare("io::read_all");

    let (cloned_file, taken_pipe) = take_file_or_pipe_handle(vm, handle)?;
    let raw_state = state.clone();

    let worker = ThreadedOperation::spawn_worker(
        "io::read_all",
        raw_state,
        tx,
        move |state, tx: Sender<ThreadedWorkerSignal>| {
            if state.cancelled.load(Ordering::SeqCst) {
                let _ = tx.send(Err("io::read_all was cancelled before starting".to_string()));
                return;
            }
            let result = if let Some(mut file) = cloned_file {
                let mut out = String::new();
                let r = read_to_string_with_limit(&mut file, max_read_bytes, &mut out);
                drop(file); // close before signalling
                r.map(|_| out)
            } else if let Some(mut pipe) = taken_pipe {
                let mut out = String::new();
                let r = read_to_string_with_limit(&mut pipe, max_read_bytes, &mut out);
                drop(pipe);
                r.map(|_| out)
            } else {
                Err(VmError::HostError(
                    "io handle was already closed".to_string(),
                ))
            };
            match result {
                Ok(text) => {
                    *shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ok(text));
                    let _ = tx.send(Ok(()));
                }
                Err(err) => {
                    // Extract the inner message from VmError::HostError
                    let msg = match &err {
                        VmError::HostError(m) => m.clone(),
                        _ => err.to_string(),
                    };
                    *shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(Err(msg));
                    let _ = tx.send(Ok(()));
                }
            }
        },
    );

    let raw = register_operation_with_worker(vm, operation, worker, Some(handle))?;

    let shared_provider = shared.clone();
    vm.host.register_pending_op_result(
        raw,
        Box::new(move |_vm| {
            match shared_provider
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                Some(Ok(text)) => Ok(CallReturn::one(Value::string(text))),
                Some(Err(msg)) => Err(VmError::HostError(msg)),
                None => Err(VmError::HostError(
                    "io::read_all worker did not produce a result".to_string(),
                )),
            }
        }),
    );

    Ok(HostCallResult::Pending(raw))
}

/// Reads a single line of text from an I/O handle.
#[pd_host_function(name = "io::read_line")]
pub(crate) fn builtin_io_read_line(
    vm: &mut Vm,
    handle_id: i64,
) -> VmResult<HostCallResult<String>> {
    let max_read_bytes = super::io_policy(vm).map(|policy| policy.max_read_bytes);
    let handle = resource_handle(handle_id)?;

    // Offload the read to a worker thread.
    // For files we clone the handle; for pipes we take it and return it
    // through shared state.
    let shared: Arc<Mutex<Option<Result<String, String>>>> = Arc::new(Mutex::new(None));
    let shared_worker = shared.clone();
    // For pipes, the worker returns the pipe handle through this channel.
    let pipe_shared: Arc<Mutex<Option<std::process::ChildStdout>>> = Arc::new(Mutex::new(None));
    let pipe_shared_worker = pipe_shared.clone();

    let (operation, tx, state) = ThreadedOperation::prepare("io::read_line");

    let (cloned_file, taken_pipe) = take_file_or_pipe_handle(vm, handle)?;
    let raw_state = state.clone();

    let worker = ThreadedOperation::spawn_worker(
        "io::read_line",
        raw_state,
        tx,
        move |state, tx: Sender<ThreadedWorkerSignal>| {
            if state.cancelled.load(Ordering::SeqCst) {
                let _ = tx.send(Err(
                    "io::read_line was cancelled before starting".to_string()
                ));
                return;
            }
            let result = if let Some(mut file) = cloned_file {
                let r = read_line_from_reader(&mut file, max_read_bytes);
                r
            } else if let Some(mut pipe) = taken_pipe {
                let r = read_line_from_reader(&mut pipe, max_read_bytes);
                // Return the pipe handle for subsequent reads
                *pipe_shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(pipe);
                r
            } else {
                Err(VmError::HostError(
                    "io handle was already closed".to_string(),
                ))
            };
            match result {
                Ok(text) => {
                    *shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ok(text));
                    let _ = tx.send(Ok(()));
                }
                Err(err) => {
                    // Extract the inner message from VmError::HostError
                    let msg = match &err {
                        VmError::HostError(m) => m.clone(),
                        _ => err.to_string(),
                    };
                    *shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(Err(msg));
                    let _ = tx.send(Ok(()));
                }
            }
        },
    );

    let raw = register_operation_with_worker(vm, operation, worker, Some(handle))?;

    let shared_provider = shared.clone();
    let pipe_provider = pipe_shared.clone();
    vm.host.register_pending_op_result(
        raw,
        Box::new(move |vm: &mut Vm| {
            // If a pipe handle was returned, put it back in the resource.
            if let Some(pipe) = pipe_provider
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                let mut ctx = vm.host_context();
                if let Ok(token) = ctx.typed_resource::<IoPipeResource>(handle) {
                    if let Ok(mut resource) = ctx.resource_mut(&token) {
                        resource.get().restore_reader(pipe);
                    }
                }
            }
            match shared_provider
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                Some(Ok(text)) => Ok(CallReturn::one(Value::string(text))),
                Some(Err(msg)) => Err(VmError::HostError(msg)),
                None => Err(VmError::HostError(
                    "io::read_line worker did not produce a result".to_string(),
                )),
            }
        }),
    );

    Ok(HostCallResult::Pending(raw))
}

/// Writes text to an I/O handle.
#[pd_host_function(name = "io::write")]
pub(crate) fn builtin_io_write(
    vm: &mut Vm,
    handle_id: i64,
    text: &str,
) -> VmResult<HostCallResult<i64>> {
    if let Some(policy) = super::io_policy(vm) {
        if text.len() > policy.max_write_bytes {
            return Err(VmError::HostError(format!(
                "io_write exceeds the configured write limit of {} bytes",
                policy.max_write_bytes
            )));
        }
    }
    let bytes = text.as_bytes().to_vec();
    let handle = resource_handle(handle_id)?;

    // Offload the write to a worker thread.
    let shared: Arc<Mutex<Option<Result<i64, String>>>> = Arc::new(Mutex::new(None));
    let shared_worker = shared.clone();
    // For pipes, the worker returns the pipe handle through this channel.
    let pipe_shared: Arc<Mutex<Option<std::process::ChildStdin>>> = Arc::new(Mutex::new(None));
    let pipe_shared_worker = pipe_shared.clone();

    let (operation, tx, state) = ThreadedOperation::prepare("io::write");

    let (cloned_file, taken_pipe) = take_file_or_write_pipe_handle(vm, handle)?;
    let raw_state = state.clone();

    let worker = ThreadedOperation::spawn_worker(
        "io::write",
        raw_state,
        tx,
        move |state, tx: Sender<ThreadedWorkerSignal>| {
            if state.cancelled.load(Ordering::SeqCst) {
                let _ = tx.send(Err("io::write was cancelled before starting".to_string()));
                return;
            }
            let result = if let Some(mut file) = cloned_file {
                std::io::Write::write(&mut file, &bytes)
                    .map_err(|err| format!("io_write failed: {err}"))
                    .map(|n| n as i64)
            } else if let Some(mut pipe) = taken_pipe {
                let result = std::io::Write::write(&mut pipe, &bytes)
                    .map_err(|err| format!("io_write failed: {err}"))
                    .map(|n| n as i64);
                // Return the pipe handle for subsequent writes
                *pipe_shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(pipe);
                result
            } else {
                Err("io handle was already closed".to_string())
            };
            match result {
                Ok(written) => {
                    *shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ok(written));
                    let _ = tx.send(Ok(()));
                }
                Err(err) => {
                    *shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(Err(err));
                    let _ = tx.send(Ok(()));
                }
            }
        },
    );

    let raw = register_operation_with_worker(vm, operation, worker, Some(handle))?;

    let shared_provider = shared.clone();
    let pipe_provider = pipe_shared.clone();
    vm.host.register_pending_op_result(
        raw,
        Box::new(move |vm: &mut Vm| {
            // If a pipe handle was returned, put it back in the resource.
            if let Some(pipe) = pipe_provider
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                let mut ctx = vm.host_context();
                if let Ok(token) = ctx.typed_resource::<IoPipeResource>(handle) {
                    if let Ok(mut resource) = ctx.resource_mut(&token) {
                        resource.get().restore_writer(pipe);
                    }
                }
            }
            match shared_provider
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                Some(Ok(written)) => Ok(CallReturn::one(Value::Int(written))),
                Some(Err(msg)) => Err(VmError::HostError(msg)),
                None => Err(VmError::HostError(
                    "io::write worker did not produce a result".to_string(),
                )),
            }
        }),
    );

    Ok(HostCallResult::Pending(raw))
}

/// Flushes buffered output for an I/O handle.
#[pd_host_function(name = "io::flush")]
pub(crate) fn builtin_io_flush(vm: &mut Vm, handle_id: i64) -> VmResult<HostCallResult<bool>> {
    let handle = resource_handle(handle_id)?;

    // First check if the handle is a read-only pipe — flush is a no-op.
    let mut ctx = vm.host_context();
    let is_read_pipe = if let Ok(token) = ctx.typed_resource::<IoPipeResource>(handle) {
        if let Ok(mut resource) = ctx.resource_mut(&token) {
            resource.get().is_read_pipe()
        } else {
            false
        }
    } else {
        false
    };
    drop(ctx);

    if is_read_pipe {
        // Flush on a read pipe is a no-op. Return immediately.
        let operation = ReadyOperation;
        let spec = OperationSpec::new(operation).with_resource(handle);
        let op_id = vm
            .host_context()
            .start_operation(spec)
            .map_err(|error| VmError::HostError(format!("io operation start failed: {error}")))?;
        let raw = op_id.raw();
        vm.host.register_pending_op_result(
            raw,
            Box::new(move |_vm| Ok(CallReturn::one(Value::Bool(true)))),
        );
        return Ok(HostCallResult::Pending(raw));
    }

    // Offload the flush to a worker thread.
    let shared: Arc<Mutex<Option<Result<(), String>>>> = Arc::new(Mutex::new(None));
    let shared_worker = shared.clone();
    // For pipes, the worker returns the pipe handle through this channel.
    let pipe_shared: Arc<Mutex<Option<std::process::ChildStdin>>> = Arc::new(Mutex::new(None));
    let pipe_shared_worker = pipe_shared.clone();

    let (operation, tx, state) = ThreadedOperation::prepare("io::flush");

    let (cloned_file, taken_pipe) = take_file_or_write_pipe_handle(vm, handle)?;
    let raw_state = state.clone();

    let worker = ThreadedOperation::spawn_worker(
        "io::flush",
        raw_state,
        tx,
        move |state, tx: Sender<ThreadedWorkerSignal>| {
            if state.cancelled.load(Ordering::SeqCst) {
                let _ = tx.send(Err("io::flush was cancelled before starting".to_string()));
                return;
            }
            let result = if let Some(mut file) = cloned_file {
                file.flush()
                    .map_err(|err| format!("io_flush failed: {err}"))
            } else if let Some(mut pipe) = taken_pipe {
                let result = pipe
                    .flush()
                    .map_err(|err| format!("io_flush failed: {err}"));
                // Return the pipe handle for subsequent writes
                *pipe_shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(pipe);
                result
            } else {
                Err("io handle was already closed".to_string())
            };
            match result {
                Ok(()) => {
                    *shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ok(()));
                    let _ = tx.send(Ok(()));
                }
                Err(err) => {
                    *shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(Err(err));
                    let _ = tx.send(Ok(()));
                }
            }
        },
    );

    let raw = register_operation_with_worker(vm, operation, worker, Some(handle))?;

    let shared_provider = shared.clone();
    let pipe_provider = pipe_shared.clone();
    vm.host.register_pending_op_result(
        raw,
        Box::new(move |vm: &mut Vm| {
            // If a pipe handle was returned, put it back in the resource.
            if let Some(pipe) = pipe_provider
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                let mut ctx = vm.host_context();
                if let Ok(token) = ctx.typed_resource::<IoPipeResource>(handle) {
                    if let Ok(mut resource) = ctx.resource_mut(&token) {
                        resource.get().restore_writer(pipe);
                    }
                }
            }
            match shared_provider
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                Some(Ok(())) => Ok(CallReturn::one(Value::Bool(true))),
                Some(Err(msg)) => Err(VmError::HostError(msg)),
                None => Err(VmError::HostError(
                    "io::flush worker did not produce a result".to_string(),
                )),
            }
        }),
    );

    Ok(HostCallResult::Pending(raw))
}

/// Closes an I/O handle.
/// The actual close teardown (flush, process kill) is delegated to the
/// resource's begin_close/poll_close lifecycle, which spawns a worker.
/// The close-completion operation is registered first (before calling
/// close_resource) to guarantee failure atomicity: if operation
/// registration fails, the target resource remains fully live and
/// guest-owned. The operation is deliberately NOT associated with the
/// target resource handle (via `with_resource`) because close_resource
/// cancels operations associated with the target, which would
/// self-cancel the close-completion driver.
#[pd_host_function(name = "io::close")]
pub(crate) fn builtin_io_close(vm: &mut Vm, handle_id: i64) -> VmResult<HostCallResult<bool>> {
    let handle = resource_handle(handle_id)?;

    // Create a shared close-completion flag.
    let close_completion = Arc::new(AtomicBool::new(false));

    // Register the close-completion operation FIRST (failure-atomic).
    // No resource association: close_resource cancels ops associated
    // with the target handle, which would self-cancel us.
    let operation = CloseCompletionOperation::new(close_completion.clone());
    let spec = OperationSpec::new(operation);
    let op_id = vm
        .host_context()
        .start_operation(spec)
        .map_err(|error| VmError::HostError(format!("io operation start failed: {error}")))?;
    let raw = op_id.raw();

    // Register the PendingOpResult provider before close_resource.
    vm.host.register_pending_op_result(
        raw,
        Box::new(move |_vm| Ok(CallReturn::one(Value::Bool(true)))),
    );

    // Inject the close_completion flag into the target resource, then
    // close it. Try file first, then pipe (popen returns a pipe handle).
    let mut ctx = vm.host_context();
    let inject_result = ctx
        .borrow_resource_mut::<IoFileResource>(handle)
        .map(|mut res| {
            res.close_completion = close_completion.clone();
        });
    match inject_result {
        Ok(()) => {
            ctx.close_resource::<IoFileResource>(handle, ResourceCloseReason::Requested)
                .map_err(|error| VmError::HostError(format!("io_close failed: {error}")))?;
        }
        Err(ref error) if error.message().contains("resource_type_mismatch") => {
            // Pipe close is synchronous (begin_close returns Ready),
            // so we signal completion immediately.
            ctx.close_resource::<IoPipeResource>(handle, ResourceCloseReason::Requested)
                .map_err(|error| VmError::HostError(format!("io_close failed: {error}")))?;
            close_completion.store(true, Ordering::SeqCst);
        }
        Err(error) => {
            return Err(VmError::HostError(format!("io_close failed: {error}")));
        }
    }
    drop(ctx);

    Ok(HostCallResult::Pending(raw))
}

/// Returns whether a file system path exists.
/// The actual filesystem check runs on a worker thread so the VM thread
/// never blocks on IO.
#[pd_host_function(name = "io::exists")]
pub(crate) fn builtin_io_exists(vm: &mut Vm, path: &str) -> VmResult<HostCallResult<bool>> {
    let path = authorize_io_path(vm, path, false)?;
    let path_buf = path.to_path_buf();

    let shared: Arc<Mutex<Option<Result<bool, String>>>> = Arc::new(Mutex::new(None));
    let shared_worker = shared.clone();

    let (operation, tx, state) = ThreadedOperation::prepare("io::exists");
    let raw_state = state.clone();

    let worker = ThreadedOperation::spawn_worker(
        "io::exists",
        raw_state,
        tx,
        move |state, tx: Sender<ThreadedWorkerSignal>| {
            if state.cancelled.load(Ordering::SeqCst) {
                let _ = tx.send(Err("io::exists was cancelled before starting".to_string()));
                return;
            }
            let found = path_buf.exists();
            *shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ok(found));
            let _ = tx.send(Ok(()));
        },
    );

    let raw = register_operation_with_worker(vm, operation, worker, None)?;

    let shared_provider = shared.clone();
    vm.host.register_pending_op_result(
        raw,
        Box::new(move |_vm| {
            match shared_provider
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                Some(Ok(found)) => Ok(CallReturn::one(Value::Bool(found))),
                Some(Err(msg)) => Err(VmError::HostError(msg)),
                None => Err(VmError::HostError(
                    "io::exists worker did not produce a result".to_string(),
                )),
            }
        }),
    );

    Ok(HostCallResult::Pending(raw))
}

// ---- Synchronous read/write/flush helpers (run on VM thread but bounded) ----

/// Clone (for files) or take (for pipes) the handle from a resource, so the
/// actual IO work can be offloaded to a worker thread. The VM thread only
/// validates arguments/policy and clones safe shared resource state.
/// Returns `(Option<File>, Option<ChildStdout>)` — at most one is `Some`.
fn take_file_or_pipe_handle(
    vm: &mut Vm,
    handle: ResourceHandle,
) -> VmResult<(Option<std::fs::File>, Option<std::process::ChildStdout>)> {
    let mut ctx = vm.host_context();
    let token = ctx.typed_resource::<IoFileResource>(handle);
    if let Ok(token) = token {
        let mut resource = ctx
            .resource_mut(&token)
            .map_err(|error| VmError::HostError(format!("io handle lookup failed: {error}")))?;
        let file = resource.get().with_handle_mut(|f| {
            f.try_clone()
                .map_err(|err| VmError::HostError(format!("io handle clone failed: {err}")))
        })?;
        Ok((Some(file), None))
    } else {
        let token = ctx
            .typed_resource::<IoPipeResource>(handle)
            .map_err(|error| VmError::HostError(format!("io handle lookup failed: {error}")))?;
        let mut resource = ctx
            .resource_mut(&token)
            .map_err(|error| VmError::HostError(format!("io handle lookup failed: {error}")))?;
        let pipe = resource.get().take_reader()?;
        Ok((None, Some(pipe)))
    }
}

/// Clone (for files) or take (for pipes) a WRITABLE handle from a resource.
/// Returns `(Option<File>, Option<ChildStdin>)` — at most one is `Some`.
fn take_file_or_write_pipe_handle(
    vm: &mut Vm,
    handle: ResourceHandle,
) -> VmResult<(Option<std::fs::File>, Option<std::process::ChildStdin>)> {
    let mut ctx = vm.host_context();
    let token = ctx.typed_resource::<IoFileResource>(handle);
    if let Ok(token) = token {
        let mut resource = ctx
            .resource_mut(&token)
            .map_err(|error| VmError::HostError(format!("io handle lookup failed: {error}")))?;
        let file = resource.get().with_handle_mut(|f| {
            f.try_clone()
                .map_err(|err| VmError::HostError(format!("io handle clone failed: {err}")))
        })?;
        Ok((Some(file), None))
    } else {
        let token = ctx
            .typed_resource::<IoPipeResource>(handle)
            .map_err(|error| VmError::HostError(format!("io handle lookup failed: {error}")))?;
        let mut resource = ctx
            .resource_mut(&token)
            .map_err(|error| VmError::HostError(format!("io handle lookup failed: {error}")))?;
        let pipe = resource.get().take_writer()?;
        Ok((None, Some(pipe)))
    }
}

// ---- Resource helpers ----

fn insert_io_file_resource(vm: &mut Vm, resource: IoFileResource) -> VmResult<i64> {
    let mut ctx = vm.host_context();
    let token = ctx
        .push_resource_with_key(resource, super::io_file_key())
        .map_err(|error| VmError::HostError(format!("io resource insert failed: {error}")))?;
    let handle = token.handle();
    let raw = match handle.as_value() {
        Value::Int(value) => value,
        _ => unreachable!(),
    };
    ctx.mark_resource_guest_owned(handle)
        .map_err(|error| VmError::HostError(format!("io resource ownership failed: {error}")))?;
    Ok(raw)
}

fn insert_io_process_resource(
    vm: &mut Vm,
    resource: IoProcessResource,
) -> VmResult<crate::vm::resource::Resource<IoProcessResource>> {
    let mut ctx = vm.host_context();
    let token = ctx
        .push_resource_with_key(resource, super::io_process_key())
        .map_err(|error| {
            VmError::HostError(format!("io process resource insert failed: {error}"))
        })?;
    let handle = token.handle();
    ctx.mark_resource_guest_owned(handle).map_err(|error| {
        VmError::HostError(format!("io process resource ownership failed: {error}"))
    })?;
    Ok(token)
}

fn insert_io_pipe_child_resource(
    vm: &mut Vm,
    resource: IoPipeResource,
    parent: &crate::vm::resource::Resource<IoProcessResource>,
) -> VmResult<crate::vm::resource::Resource<IoPipeResource>> {
    let mut ctx = vm.host_context();
    let token = ctx
        .push_child_resource_with_key(resource, parent, super::io_pipe_key())
        .map_err(|error| VmError::HostError(format!("io pipe resource insert failed: {error}")))?;
    let handle = token.handle();
    ctx.mark_resource_guest_owned(handle).map_err(|error| {
        VmError::HostError(format!("io pipe resource ownership failed: {error}"))
    })?;
    Ok(token)
}

// ---- Process helpers ----

#[cfg(unix)]
fn terminate_process_group(process_id: u32) {
    if let Ok(pid) = libc::pid_t::try_from(process_id) {
        // SAFETY: process_id is the tracked child pid; sending SIGKILL to
        // the negated value kills the whole process group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn terminate_process_group(_process_id: u32) {}

fn spawn_shell_command(command: &str, mode: &str) -> VmResult<std::process::Child> {
    let mut process = if cfg!(windows) {
        let mut cmd = std::process::Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    } else {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    };

    #[cfg(unix)]
    process.process_group(0);

    match mode {
        "r" => {
            process
                .stdout(std::process::Stdio::piped())
                .stdin(std::process::Stdio::null());
        }
        "w" => {
            process
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null());
        }
        _ => {}
    }

    process
        .spawn()
        .map_err(|err| VmError::HostError(format!("io_popen failed: {err}")))
}

// ---- Path helpers ----

fn authorize_io_path(vm: &Vm, path: &str, writes: bool) -> VmResult<PathBuf> {
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
    let canonical = canonicalize_io_target(&absolute)?;
    for root in &policy.allowed_roots {
        let root = Path::new(root).canonicalize().map_err(|error| {
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

fn canonicalize_io_target(path: &Path) -> VmResult<PathBuf> {
    if path.exists() {
        return path
            .canonicalize()
            .map_err(|error| VmError::HostError(format!("io path resolution failed: {error}")));
    }
    let parent = path
        .parent()
        .ok_or_else(|| VmError::HostError(format!("io path '{}' has no parent", path.display())))?;
    let file_name = path.file_name().ok_or_else(|| {
        VmError::HostError(format!("io path '{}' has no file name", path.display()))
    })?;
    parent
        .canonicalize()
        .map(|parent| parent.join(file_name))
        .map_err(|error| VmError::HostError(format!("io path resolution failed: {error}")))
}

// ---- Read helpers ----

fn read_to_string_with_limit(
    reader: &mut impl Read,
    max_read_bytes: Option<usize>,
    out: &mut String,
) -> VmResult<()> {
    match max_read_bytes {
        None => {
            reader
                .read_to_string(out)
                .map_err(|err| VmError::HostError(format!("io_read_all failed: {err}")))?;
        }
        Some(limit) => {
            let take_limit = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
            reader
                .take(take_limit)
                .read_to_string(out)
                .map_err(|err| VmError::HostError(format!("io_read_all failed: {err}")))?;
            if out.len() > limit {
                return Err(VmError::HostError(format!(
                    "io_read_all exceeds the configured read limit of {limit} bytes"
                )));
            }
        }
    }
    Ok(())
}

fn read_line_from_reader(
    reader: &mut impl Read,
    max_read_bytes: Option<usize>,
) -> VmResult<String> {
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
        if let Some(limit) = max_read_bytes {
            if bytes.len() > limit {
                return Err(VmError::HostError(format!(
                    "io_read_line exceeds the configured read limit of {} bytes",
                    limit
                )));
            }
        }
        if one[0] == b'\n' {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn resource_handle(handle_id: i64) -> VmResult<ResourceHandle> {
    if handle_id <= 0 {
        return Err(VmError::HostError(format!(
            "invalid io handle id {handle_id}; expected positive handle id"
        )));
    }
    ResourceHandle::from_value(&Value::Int(handle_id)).map_err(runtime_host_error)
}

fn runtime_host_error(error: impl std::fmt::Display) -> VmError {
    VmError::HostError(error.to_string())
}
