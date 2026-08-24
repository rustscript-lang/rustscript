//! Canonical shared implementation of IO resource types, helpers, and builtin
//! function bodies. This file is compiled for both the `async` and `blocking`
//! feature matrices. The per-feature files (`async_io.rs`, `blocking.rs`) are
//! thin wrappers: both apply `#[pd_host_function]` annotations and delegate
//! to the bodies here.
//!
//! ## Design
//!
//! - `IoFileResource` and the aggregate `IoPipeResource` own every concrete
//!   resource lifecycle. A pipe contains its child-process close state, so only
//!   the pipe is inserted in the execution scope.
//! - Helper functions (`register_threaded_operation`, `authorize_io_path`, …) are here.
//! - Builtin function bodies are here as `pub(crate) fn …_body(…)` — the entry
//!   points in `async_io.rs` / `blocking.rs` delegate to them.
//! - `PipeTransferGuard` from `ops` is used in every pipe-offload operation to
//!   prevent OS-handle leaks on cancellation-before-start.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::thread::JoinHandle;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use super::super::HostCallResult;
use super::ops::{
    CloseCompletionOperation, CloseCompletionState, PipeTransferGuard, ReadyOperation,
    ThreadedOperation, ThreadedWorkerPublisher, restore_reader_or_drop, restore_writer_or_drop,
};
use crate::host_api::ResourceTypeKey;
use crate::vm::operation::{OperationCancelReason, OperationId, OperationSpec};
use crate::vm::resource::{
    CloseProgress, HostResource, ResourceCloseReason, ResourceHandle, ResourceResult,
};
pub(crate) use crate::vm::{CallReturn, Value, Vm, VmError, VmResult};

fn resource_cleanup_error(
    operation: &'static str,
    message: impl Into<String>,
) -> crate::vm::resource::ResourceError {
    crate::vm::resource::ResourceError::new(
        crate::vm::resource::ResourceErrorCode::ResourceCleanupFailed,
        operation,
        message,
    )
}

// ============================================================================
// HostResource types
// ============================================================================

/// A file handle stored as a concrete HostResource.
pub(crate) struct IoFileResource {
    pub(crate) handle: Mutex<Option<std::fs::File>>,
    pub(crate) close_worker: Mutex<Option<JoinHandle<()>>>,
    pub(crate) closed: AtomicBool,
    /// Shared state set by the close worker when it finishes.
    pub(crate) close_completion: Arc<CloseCompletionState>,
}

impl HostResource for IoFileResource {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(super::io_file_key())
    }

    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.closed.store(true, Ordering::SeqCst);
        // Take the file handle and spawn a worker to flush/close it.
        let file = self.handle.lock().unwrap_or_else(|e| e.into_inner()).take();
        let close_completion = self.close_completion.clone();
        if let Some(mut file) = file {
            let worker_completion = Arc::clone(&close_completion);
            match std::thread::Builder::new()
                .name("io-file-close".into())
                .spawn(move || {
                    let result = match file.flush() {
                        Ok(()) => Ok(()),
                        Err(e) => Err(format!("io file close: flush failed: {e}")),
                    };
                    worker_completion.complete(result);
                    // file is dropped here, which closes the OS handle.
                }) {
                Ok(handle) => {
                    *self.close_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
                }
                Err(error) => close_completion
                    .complete(Err(format!("io file close worker spawn failed: {error}"))),
            }
        } else {
            close_completion.complete(Ok(()));
        }
        Ok(CloseProgress::Pending)
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        let mut guard = self.close_worker.lock().unwrap_or_else(|e| e.into_inner());
        if self.close_completion.result().is_none()
            && let Some(handle) = guard.as_ref()
            && handle.is_finished()
        {
            let handle = guard.take().expect("finished close worker must exist");
            let message = match handle.join() {
                Ok(()) => "io file close worker exited without publishing a result",
                Err(_) => "io file close worker panicked",
            };
            self.close_completion.complete(Err(message.to_string()));
        }
        let Some(result) = self.close_completion.poll_result(cx) else {
            return Poll::Pending;
        };
        if let Some(handle) = guard.take()
            && handle.join().is_err()
        {
            return Poll::Ready(Err(resource_cleanup_error(
                "io.file",
                "io file close worker panicked",
            )));
        }
        Poll::Ready(result.map_err(|message| resource_cleanup_error("io.file", message)))
    }
}

impl IoFileResource {
    pub(crate) fn new(file: std::fs::File) -> Self {
        Self {
            handle: Mutex::new(Some(file)),
            close_worker: Mutex::new(None),
            closed: AtomicBool::new(false),
            close_completion: Arc::new(CloseCompletionState::new()),
        }
    }

    pub(crate) fn with_handle_mut<T>(
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

/// Armed rollback owner created immediately after process spawn. Until the
/// child is handed to an inserted aggregate resource, every exit path
/// terminates and reaps it.
struct ArmedChild {
    child: Option<std::process::Child>,
}

impl ArmedChild {
    fn new(child: std::process::Child) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> &mut std::process::Child {
        self.child.as_mut().expect("armed child must exist")
    }

    fn handoff(mut self) -> std::process::Child {
        self.child.take().expect("armed child must exist")
    }
}

impl Drop for ArmedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            terminate_process_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Child-process close state embedded in an aggregate pipe resource.
struct IoProcessState {
    child: Mutex<Option<std::process::Child>>,
    close_worker: Mutex<Option<JoinHandle<()>>>,
    process_id: u32,
    close_completion: Arc<CloseCompletionState>,
}

impl IoProcessState {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        let child = self.child.lock().unwrap_or_else(|e| e.into_inner()).take();
        let process_id = self.process_id;
        let close_completion = self.close_completion.clone();
        if let Some(child) = child {
            let worker_completion = Arc::clone(&close_completion);
            let armed = ArmedChild::new(child);
            match std::thread::Builder::new()
                .name("io-process-close".into())
                .spawn(move || {
                    let mut child = armed.handoff();
                    terminate_process_group(process_id);
                    let kill_result = child.kill();
                    let wait_result = child.wait();
                    let result = match (kill_result, wait_result) {
                        (Ok(()), Ok(_)) => Ok(()),
                        (Err(kill), Ok(_)) => Err(format!("io process close: kill failed: {kill}")),
                        (Ok(()), Err(wait)) => {
                            Err(format!("io process close: wait failed: {wait}"))
                        }
                        (Err(kill), Err(wait)) => Err(format!(
                            "io process close: kill failed: {kill}; wait failed: {wait}"
                        )),
                    };
                    worker_completion.complete(result);
                }) {
                Ok(handle) => {
                    *self.close_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
                }
                Err(error) => close_completion.complete(Err(format!(
                    "io process close worker spawn failed: {error}"
                ))),
            }
        } else {
            close_completion.complete(Ok(()));
        }
        Ok(CloseProgress::Pending)
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        let mut guard = self.close_worker.lock().unwrap_or_else(|e| e.into_inner());
        if self.close_completion.result().is_none()
            && let Some(handle) = guard.as_ref()
            && handle.is_finished()
        {
            let handle = guard.take().expect("finished close worker must exist");
            let message = match handle.join() {
                Ok(()) => "io process close worker exited without publishing a result",
                Err(_) => "io process close worker panicked",
            };
            self.close_completion.complete(Err(message.to_string()));
        }
        let Some(result) = self.close_completion.poll_result(cx) else {
            return Poll::Pending;
        };
        if let Some(handle) = guard.take()
            && handle.join().is_err()
        {
            return Poll::Ready(Err(resource_cleanup_error(
                "io.pipe",
                "io process close worker panicked",
            )));
        }
        Poll::Ready(result.map_err(|message| resource_cleanup_error("io.pipe", message)))
    }
}

impl Drop for IoProcessState {
    fn drop(&mut self) {
        if let Some(mut child) = self
            .child
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            terminate_process_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handle) = self
            .close_worker
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            && handle.is_finished()
        {
            let _ = handle.join();
        }
    }
}

impl IoProcessState {
    fn new_with_completion(
        child: std::process::Child,
        close_completion: Arc<CloseCompletionState>,
    ) -> Self {
        let process_id = child.id();
        Self {
            child: Mutex::new(Some(child)),
            close_worker: Mutex::new(None),
            process_id,
            close_completion,
        }
    }
}

/// Aggregate stdio-pipe resource that also owns its child process lifecycle.
pub(crate) struct IoPipeResource {
    pipe: Mutex<IoPipeInner>,
    process: Option<IoProcessState>,
    closed: AtomicBool,
    close_completion: Arc<CloseCompletionState>,
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

    fn begin_close(&mut self, reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.closed.store(true, Ordering::SeqCst);
        *self.pipe.lock().unwrap_or_else(|e| e.into_inner()) = IoPipeInner::Closed;
        match self.process.as_mut() {
            Some(process) => process.begin_close(reason),
            None => Ok(CloseProgress::Ready),
        }
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        match self.process.as_mut() {
            Some(process) => process.poll_close(cx),
            None => Poll::Ready(Ok(())),
        }
    }
}

impl IoPipeResource {
    pub(crate) fn new_read_process(
        pipe: std::process::ChildStdout,
        child: std::process::Child,
    ) -> Self {
        let close_completion = Arc::new(CloseCompletionState::new());
        Self {
            pipe: Mutex::new(IoPipeInner::Read(pipe)),
            process: Some(IoProcessState::new_with_completion(
                child,
                Arc::clone(&close_completion),
            )),
            closed: AtomicBool::new(false),
            close_completion,
        }
    }

    pub(crate) fn new_write_process(
        pipe: std::process::ChildStdin,
        child: std::process::Child,
    ) -> Self {
        let close_completion = Arc::new(CloseCompletionState::new());
        Self {
            pipe: Mutex::new(IoPipeInner::Write(pipe)),
            process: Some(IoProcessState::new_with_completion(
                child,
                Arc::clone(&close_completion),
            )),
            closed: AtomicBool::new(false),
            close_completion,
        }
    }

    /// Take the reader pipe handle, replacing with `Closed`.
    pub(crate) fn take_reader(&mut self) -> VmResult<std::process::ChildStdout> {
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
    pub(crate) fn take_writer(&mut self) -> VmResult<std::process::ChildStdin> {
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
    pub(crate) fn restore_reader(&mut self, pipe: std::process::ChildStdout) {
        let mut guard = self.pipe.lock().unwrap_or_else(|e| e.into_inner());
        *guard = IoPipeInner::Read(pipe);
    }

    /// Restore a writer pipe handle that was taken for offloaded IO.
    pub(crate) fn restore_writer(&mut self, pipe: std::process::ChildStdin) {
        let mut guard = self.pipe.lock().unwrap_or_else(|e| e.into_inner());
        *guard = IoPipeInner::Write(pipe);
    }

    /// Whether the pipe resource has been closed (begin_close was called).
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Check if this pipe is a read-only pipe (ChildStdout).
    pub(crate) fn is_read_pipe(&self) -> bool {
        let guard = self.pipe.lock().unwrap_or_else(|e| e.into_inner());
        matches!(&*guard, IoPipeInner::Read(_))
    }
}

/// Admit an operation before any descriptor transfer or worker spawn.
pub(crate) fn register_threaded_operation(
    vm: &mut Vm,
    operation: ThreadedOperation,
    resource_handle: Option<ResourceHandle>,
) -> VmResult<OperationId> {
    let mut spec = OperationSpec::new(operation);
    if let Some(handle) = resource_handle {
        spec = spec.with_resource(handle).close_resource_on_cancel();
    }
    vm.host_context()
        .start_operation(spec)
        .map_err(|error| VmError::HostError(format!("io operation start failed: {error}")))
}

fn rollback_threaded_start(vm: &mut Vm, id: OperationId, cause: VmError) -> VmError {
    match vm
        .host_context()
        .abort_operation(id, OperationCancelReason::Requested)
    {
        Ok(_) => cause,
        Err(cleanup) => VmError::HostError(format!(
            "{cause}; operation startup rollback failed: {cleanup}"
        )),
    }
}

// ============================================================================
// Builtin function bodies (called by the per-feature wrappers)
// ============================================================================

/// Opens a file handle for runtime I/O. Body shared by async and blocking paths.
pub(crate) fn builtin_io_open_body(
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
    let op_id = register_threaded_operation(vm, operation, None)?;
    let raw = op_id.raw();
    let raw_state = state.clone();

    if let Err(message) = ThreadedOperation::spawn_worker(
        "io::open",
        raw_state,
        tx,
        move |state, tx: ThreadedWorkerPublisher| {
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
    ) {
        return Err(rollback_threaded_start(
            vm,
            op_id,
            VmError::HostError(message),
        ));
    }

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
                    vm.transfer_legacy_materialized_resource(
                        resource_handle(handle)?,
                        super::io_file_key(),
                    )?;
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

/// Starts a child process and returns a process-backed handle. Body shared by
/// async and blocking paths.
pub(crate) fn builtin_io_popen_body(
    vm: &mut Vm,
    command: &str,
    mode: &str,
) -> VmResult<HostCallResult<i64>> {
    if mode != "r" && mode != "w" {
        return Err(VmError::HostError(format!(
            "unsupported io_popen mode '{mode}', expected r or w"
        )));
    }
    if let Some(policy) = super::io_policy(vm)
        && !policy.allow_process
    {
        return Err(VmError::HostError(
            "io_popen requires the process capability".to_string(),
        ));
    }
    let command = command.to_string();
    let mode_str = mode.to_string();

    let shared: Arc<Mutex<Option<Result<ArmedChild, String>>>> = Arc::new(Mutex::new(None));
    let shared_worker = shared.clone();

    let (operation, tx, state) = ThreadedOperation::prepare("io::popen");
    let op_id = register_threaded_operation(vm, operation, None)?;
    let raw = op_id.raw();
    let raw_state = state.clone();
    let mode_for_worker = mode_str.clone();

    if let Err(message) = ThreadedOperation::spawn_worker(
        "io::popen",
        raw_state,
        tx,
        move |state, tx: ThreadedWorkerPublisher| {
            if state.cancelled.load(Ordering::SeqCst) {
                let _ = tx.send(Err("io::popen was cancelled before starting".to_string()));
                return;
            }
            match spawn_shell_command(&command, &mode_for_worker) {
                Ok(child) => {
                    *shared_worker.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(Ok(ArmedChild::new(child)));
                    let _ = tx.send(Ok(()));
                }
                Err(err) => {
                    let _ = tx.send(Err(format!("io_popen failed: {err}")));
                }
            }
        },
    ) {
        return Err(rollback_threaded_start(
            vm,
            op_id,
            VmError::HostError(message),
        ));
    }

    let shared_provider = shared.clone();
    vm.host.register_pending_op_result(
        raw,
        Box::new(move |vm: &mut Vm| {
            (|| {
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
                        let stdout = child.child_mut().stdout.take().ok_or_else(|| {
                            VmError::HostError(
                                "io_popen('r') did not provide stdout pipe".to_string(),
                            )
                        })?;
                        let process = child.handoff();
                        let pipe_resource = IoPipeResource::new_read_process(stdout, process);
                        let pipe_token = insert_io_pipe_resource(vm, pipe_resource)?;
                        let handle = pipe_token.handle().as_value();
                        match handle {
                            Value::Int(value) => value,
                            _ => unreachable!(),
                        }
                    }
                    "w" => {
                        let stdin = child.child_mut().stdin.take().ok_or_else(|| {
                            VmError::HostError(
                                "io_popen('w') did not provide stdin pipe".to_string(),
                            )
                        })?;
                        let process = child.handoff();
                        let pipe_resource = IoPipeResource::new_write_process(stdin, process);
                        let pipe_token = insert_io_pipe_resource(vm, pipe_resource)?;
                        let handle = pipe_token.handle().as_value();
                        match handle {
                            Value::Int(value) => value,
                            _ => unreachable!(),
                        }
                    }
                    _ => unreachable!("mode validated above"),
                };
                vm.transfer_legacy_materialized_resource(
                    resource_handle(handle)?,
                    super::io_pipe_key(),
                )?;
                Ok(CallReturn::one(Value::Int(handle)))
            })()
        }),
    );

    Ok(HostCallResult::Pending(raw))
}

/// Reads all remaining text from an I/O handle. Body shared by async and
/// blocking paths.
pub(crate) fn builtin_io_read_all_body(
    vm: &mut Vm,
    handle_id: i64,
) -> VmResult<HostCallResult<String>> {
    let max_read_bytes = super::io_policy(vm).map(|policy| policy.max_read_bytes);
    let handle = resource_handle(handle_id)?;

    let shared: Arc<Mutex<Option<Result<String, String>>>> = Arc::new(Mutex::new(None));
    let shared_worker = shared.clone();
    // For pipes, the worker returns the transferred descriptor through this
    // slot so successful completion can restore the live guest resource.
    let pipe_shared: Arc<Mutex<Option<std::process::ChildStdout>>> = Arc::new(Mutex::new(None));
    let pipe_shared_worker = pipe_shared.clone();

    let (operation, tx, state) = ThreadedOperation::prepare("io::read_all");
    let op_id = register_threaded_operation(vm, operation, Some(handle))?;
    let raw = op_id.raw();

    let (cloned_file, taken_pipe) = take_file_or_pipe_handle(vm, handle)
        .map_err(|error| rollback_threaded_start(vm, op_id, error))?;
    // Use PipeTransferGuard: the guard holds the pipe handle. The worker takes
    // it when it starts work. If cancelled before take, the PendingOpResult
    // restores it to the resource.
    let pipe_guard: Option<PipeTransferGuard<std::process::ChildStdout>> =
        taken_pipe.map(|p| PipeTransferGuard::new(p, "io::read_all"));
    let pipe_guard_worker = pipe_guard.clone();
    let raw_state = state.clone();

    if let Err(message) = ThreadedOperation::spawn_worker(
        "io::read_all",
        raw_state,
        tx,
        move |state, tx: ThreadedWorkerPublisher| {
            if state.cancelled.load(Ordering::SeqCst) {
                if let Some(ref guard) = pipe_guard_worker
                    && let Some(pipe) = guard.take()
                {
                    *pipe_shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(pipe);
                }
                let _ = tx.send(Err("io::read_all was cancelled before starting".to_string()));
                return;
            }
            let result = if let Some(mut file) = cloned_file {
                let mut out = String::new();
                let r = read_to_string_with_limit(&mut file, max_read_bytes, &mut out);
                drop(file);
                r.map(|_| out)
            } else if let Some(ref guard) = pipe_guard_worker {
                let mut pipe = match guard.take() {
                    Some(p) => p,
                    None => {
                        let _ = tx.send(Err("io handle was already closed".to_string()));
                        return;
                    }
                };
                let mut out = String::new();
                let r = read_pipe_to_string_with_limit(
                    &mut pipe,
                    max_read_bytes,
                    &mut out,
                    &state.cancelled,
                );
                *pipe_shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(pipe);
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
                    let msg = match &err {
                        VmError::HostError(m) => m.clone(),
                        _ => err.to_string(),
                    };
                    *shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(Err(msg));
                    let _ = tx.send(Ok(()));
                }
            }
        },
    ) {
        if let Some(ref guard) = pipe_guard {
            guard.restore_or_drop();
        }
        return Err(rollback_threaded_start(
            vm,
            op_id,
            VmError::HostError(message),
        ));
    }

    let shared_provider = shared.clone();
    let pipe_provider = pipe_shared.clone();
    vm.host.register_pending_op_result(
        raw,
        Box::new(move |vm: &mut Vm| {
            if let Some(pipe) = pipe_provider
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                restore_reader_or_drop(vm, handle, pipe);
            }

            // The guard still owns the pipe only when cancellation won before
            // the worker transferred it; in that case the resource close path
            // is authoritative and this drop is the final owner release.
            if let Some(ref guard) = pipe_guard {
                guard.restore_or_drop();
            }

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

/// Reads a single line of text from an I/O handle. Body shared by async and
/// blocking paths.
pub(crate) fn builtin_io_read_line_body(
    vm: &mut Vm,
    handle_id: i64,
) -> VmResult<HostCallResult<String>> {
    let max_read_bytes = super::io_policy(vm).map(|policy| policy.max_read_bytes);
    let handle = resource_handle(handle_id)?;

    let shared: Arc<Mutex<Option<Result<String, String>>>> = Arc::new(Mutex::new(None));
    let shared_worker = shared.clone();
    // For pipes, the worker returns the pipe handle through this channel.
    let pipe_shared: Arc<Mutex<Option<std::process::ChildStdout>>> = Arc::new(Mutex::new(None));
    let pipe_shared_worker = pipe_shared.clone();

    let (operation, tx, state) = ThreadedOperation::prepare("io::read_line");
    let op_id = register_threaded_operation(vm, operation, Some(handle))?;
    let raw = op_id.raw();

    let (cloned_file, taken_pipe) = take_file_or_pipe_handle(vm, handle)
        .map_err(|error| rollback_threaded_start(vm, op_id, error))?;
    // Use PipeTransferGuard: protects the pipe handle from being dropped on
    // cancellation before the worker starts.
    let pipe_guard: Option<PipeTransferGuard<std::process::ChildStdout>> =
        taken_pipe.map(|p| PipeTransferGuard::new(p, "io::read_line"));
    let pipe_guard_worker = pipe_guard.clone();
    let raw_state = state.clone();

    if let Err(message) = ThreadedOperation::spawn_worker(
        "io::read_line",
        raw_state,
        tx,
        move |state, tx: ThreadedWorkerPublisher| {
            if state.cancelled.load(Ordering::SeqCst) {
                // Return the pipe handle through pipe_shared so PendingOpResult
                // can restore it — cancellation before worker start.
                if let Some(ref guard) = pipe_guard_worker
                    && let Some(pipe) = guard.take()
                {
                    *pipe_shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(pipe);
                }
                let _ = tx.send(Err(
                    "io::read_line was cancelled before starting".to_string()
                ));
                return;
            }
            let result = if let Some(mut file) = cloned_file {
                read_line_from_reader(&mut file, max_read_bytes)
            } else if let Some(ref guard) = pipe_guard_worker {
                let mut pipe = match guard.take() {
                    Some(p) => p,
                    None => {
                        let _ = tx.send(Err("io handle was already closed".to_string()));
                        return;
                    }
                };
                let r = read_pipe_line_from_reader(&mut pipe, max_read_bytes, &state.cancelled);
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
                    let msg = match &err {
                        VmError::HostError(m) => m.clone(),
                        _ => err.to_string(),
                    };
                    *shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(Err(msg));
                    let _ = tx.send(Ok(()));
                }
            }
        },
    ) {
        if let Some(ref guard) = pipe_guard {
            guard.restore_or_drop();
        }
        return Err(rollback_threaded_start(
            vm,
            op_id,
            VmError::HostError(message),
        ));
    }

    let shared_provider = shared.clone();
    let pipe_provider = pipe_shared.clone();
    vm.host.register_pending_op_result(
        raw,
        Box::new(move |vm: &mut Vm| {
            if let Some(pipe) = pipe_provider
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                restore_reader_or_drop(vm, handle, pipe);
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

/// Writes text to an I/O handle. Body shared by async and blocking paths.
pub(crate) fn builtin_io_write_body(
    vm: &mut Vm,
    handle_id: i64,
    text: &str,
) -> VmResult<HostCallResult<i64>> {
    if let Some(policy) = super::io_policy(vm)
        && text.len() > policy.max_write_bytes
    {
        return Err(VmError::HostError(format!(
            "io_write exceeds the configured write limit of {} bytes",
            policy.max_write_bytes
        )));
    }
    let bytes = text.as_bytes().to_vec();
    let handle = resource_handle(handle_id)?;

    let shared: Arc<Mutex<Option<Result<i64, String>>>> = Arc::new(Mutex::new(None));
    let shared_worker = shared.clone();
    // For pipes, the worker returns the pipe handle through this channel.
    let pipe_shared: Arc<Mutex<Option<std::process::ChildStdin>>> = Arc::new(Mutex::new(None));
    let pipe_shared_worker = pipe_shared.clone();

    let (operation, tx, state) = ThreadedOperation::prepare("io::write");
    let op_id = register_threaded_operation(vm, operation, Some(handle))?;
    let raw = op_id.raw();

    let (cloned_file, taken_pipe) = take_file_or_write_pipe_handle(vm, handle)
        .map_err(|error| rollback_threaded_start(vm, op_id, error))?;
    // Use PipeTransferGuard: protects the pipe handle from being dropped on
    // cancellation before the worker starts.
    let pipe_guard: Option<PipeTransferGuard<std::process::ChildStdin>> =
        taken_pipe.map(|p| PipeTransferGuard::new(p, "io::write"));
    let pipe_guard_worker = pipe_guard.clone();
    let raw_state = state.clone();

    if let Err(message) = ThreadedOperation::spawn_worker(
        "io::write",
        raw_state,
        tx,
        move |state, tx: ThreadedWorkerPublisher| {
            if state.cancelled.load(Ordering::SeqCst) {
                if let Some(ref guard) = pipe_guard_worker
                    && let Some(pipe) = guard.take()
                {
                    *pipe_shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(pipe);
                }
                let _ = tx.send(Err("io::write was cancelled before starting".to_string()));
                return;
            }
            let result = if let Some(mut file) = cloned_file {
                Write::write(&mut file, &bytes)
                    .map_err(|err| format!("io_write failed: {err}"))
                    .map(|n| n as i64)
            } else if let Some(ref guard) = pipe_guard_worker {
                let mut pipe = match guard.take() {
                    Some(p) => p,
                    None => {
                        let _ = tx.send(Err("io handle was already closed".to_string()));
                        return;
                    }
                };
                let result = write_pipe_interruptible(&mut pipe, &bytes, &state.cancelled)
                    .map(|written| written as i64);
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
    ) {
        if let Some(ref guard) = pipe_guard {
            guard.restore_or_drop();
        }
        return Err(rollback_threaded_start(
            vm,
            op_id,
            VmError::HostError(message),
        ));
    }

    let shared_provider = shared.clone();
    let pipe_provider = pipe_shared.clone();
    vm.host.register_pending_op_result(
        raw,
        Box::new(move |vm: &mut Vm| {
            if let Some(pipe) = pipe_provider
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                restore_writer_or_drop(vm, handle, pipe);
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

/// Flushes buffered output for an I/O handle. Body shared by async and blocking
/// paths.
pub(crate) fn builtin_io_flush_body(vm: &mut Vm, handle_id: i64) -> VmResult<HostCallResult<bool>> {
    let handle = resource_handle(handle_id)?;

    // First check if the handle is a read-only pipe — flush is a no-op.
    let is_read_pipe = {
        let mut ctx = vm.host_context();
        if let Ok(token) = ctx.typed_resource::<IoPipeResource>(handle) {
            if let Ok(mut resource) = ctx.resource_mut(&token) {
                resource.get().is_read_pipe()
            } else {
                false
            }
        } else {
            false
        }
    };

    if is_read_pipe {
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

    let shared: Arc<Mutex<Option<Result<(), String>>>> = Arc::new(Mutex::new(None));
    let shared_worker = shared.clone();
    let pipe_shared: Arc<Mutex<Option<std::process::ChildStdin>>> = Arc::new(Mutex::new(None));
    let pipe_shared_worker = pipe_shared.clone();

    let (operation, tx, state) = ThreadedOperation::prepare("io::flush");
    let op_id = register_threaded_operation(vm, operation, Some(handle))?;
    let raw = op_id.raw();

    let (cloned_file, taken_pipe) = take_file_or_write_pipe_handle(vm, handle)
        .map_err(|error| rollback_threaded_start(vm, op_id, error))?;
    // Use PipeTransferGuard: protects the pipe handle from being dropped on
    // cancellation before the worker starts.
    let pipe_guard: Option<PipeTransferGuard<std::process::ChildStdin>> =
        taken_pipe.map(|p| PipeTransferGuard::new(p, "io::flush"));
    let pipe_guard_worker = pipe_guard.clone();
    let raw_state = state.clone();

    if let Err(message) = ThreadedOperation::spawn_worker(
        "io::flush",
        raw_state,
        tx,
        move |state, tx: ThreadedWorkerPublisher| {
            if state.cancelled.load(Ordering::SeqCst) {
                if let Some(ref guard) = pipe_guard_worker
                    && let Some(pipe) = guard.take()
                {
                    *pipe_shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(pipe);
                }
                let _ = tx.send(Err("io::flush was cancelled before starting".to_string()));
                return;
            }
            let result = if let Some(mut file) = cloned_file {
                file.flush()
                    .map_err(|err| format!("io_flush failed: {err}"))
            } else if let Some(ref guard) = pipe_guard_worker {
                let mut pipe = match guard.take() {
                    Some(p) => p,
                    None => {
                        let _ = tx.send(Err("io handle was already closed".to_string()));
                        return;
                    }
                };
                let result = pipe
                    .flush()
                    .map_err(|err| format!("io_flush failed: {err}"));
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
    ) {
        if let Some(ref guard) = pipe_guard {
            guard.restore_or_drop();
        }
        return Err(rollback_threaded_start(
            vm,
            op_id,
            VmError::HostError(message),
        ));
    }

    let shared_provider = shared.clone();
    let pipe_provider = pipe_shared.clone();
    vm.host.register_pending_op_result(
        raw,
        Box::new(move |vm: &mut Vm| {
            if let Some(pipe) = pipe_provider
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                restore_writer_or_drop(vm, handle, pipe);
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

/// Resource kind enumeration for close dispatch.
enum ResourceKind {
    File,
    Pipe,
}

/// Closes an I/O handle. Body shared by async and blocking paths.
pub(crate) fn builtin_io_close_body(vm: &mut Vm, handle_id: i64) -> VmResult<HostCallResult<bool>> {
    let handle = resource_handle(handle_id)?;

    let resource_kind = {
        let ctx = vm.host_context();
        if ctx.typed_resource::<IoFileResource>(handle).is_ok() {
            ResourceKind::File
        } else if let Err(err) = ctx.typed_resource::<IoFileResource>(handle) {
            if !err.message().contains("resource_type_mismatch") {
                return Err(VmError::HostError(format!("io_close failed: {err}")));
            }
            if ctx.typed_resource::<IoPipeResource>(handle).is_ok() {
                ResourceKind::Pipe
            } else if let Err(err) = ctx.typed_resource::<IoPipeResource>(handle) {
                if !err.message().contains("resource_type_mismatch") {
                    return Err(VmError::HostError(format!("io_close failed: {err}")));
                }
                return Err(VmError::HostError(format!(
                    "io_close failed: unknown resource type for handle {}",
                    handle_id
                )));
            } else {
                unreachable!()
            }
        } else {
            unreachable!()
        }
    };

    let close_completion = Arc::new(CloseCompletionState::new());

    let operation = CloseCompletionOperation::new(close_completion.clone());
    let spec = OperationSpec::new(operation);
    let op_id = vm
        .host_context()
        .start_operation(spec)
        .map_err(|error| VmError::HostError(format!("io operation start failed: {error}")))?;
    let raw = op_id.raw();

    let result_completion = Arc::clone(&close_completion);
    vm.host.register_pending_op_result(
        raw,
        Box::new(move |_vm| match result_completion.result() {
            Some(Ok(())) => Ok(CallReturn::one(Value::Bool(true))),
            Some(Err(message)) => Err(VmError::HostError(message)),
            None => Err(VmError::HostError(
                "io::close completed without a resource close result".to_string(),
            )),
        }),
    );

    let close_result = {
        let mut ctx = vm.host_context();
        match resource_kind {
            ResourceKind::File => {
                let inject_result =
                    ctx.borrow_resource_mut::<IoFileResource>(handle)
                        .map(|mut res| {
                            res.close_completion = close_completion.clone();
                        });
                match inject_result {
                    Ok(()) => ctx
                        .close_resource::<IoFileResource>(handle, ResourceCloseReason::Requested)
                        .map_err(|error| VmError::HostError(format!("io_close failed: {error}"))),
                    Err(error) => Err(VmError::HostError(format!("io_close failed: {error}"))),
                }
            }
            ResourceKind::Pipe => {
                let inject_result =
                    ctx.borrow_resource_mut::<IoPipeResource>(handle)
                        .map(|mut resource| {
                            resource.close_completion = Arc::clone(&close_completion);
                            if let Some(process) = resource.process.as_mut() {
                                process.close_completion = Arc::clone(&close_completion);
                            }
                        });
                match inject_result {
                    Ok(()) => ctx
                        .close_resource::<IoPipeResource>(handle, ResourceCloseReason::Requested)
                        .map_err(|error| VmError::HostError(format!("io_close failed: {error}"))),
                    Err(error) => Err(VmError::HostError(format!("io_close failed: {error}"))),
                }
            }
        }
    };

    if let Err(error) = close_result {
        close_completion.complete(Err(error.to_string()));
    }

    Ok(HostCallResult::Pending(raw))
}

/// Returns whether a file system path exists. Body shared by async and blocking
/// paths.
pub(crate) fn builtin_io_exists_body(vm: &mut Vm, path: &str) -> VmResult<HostCallResult<bool>> {
    let path = authorize_io_path(vm, path, false)?;
    let path_buf = path.to_path_buf();

    let shared: Arc<Mutex<Option<Result<bool, String>>>> = Arc::new(Mutex::new(None));
    let shared_worker = shared.clone();

    let (operation, tx, state) = ThreadedOperation::prepare("io::exists");
    let op_id = register_threaded_operation(vm, operation, None)?;
    let raw = op_id.raw();
    let raw_state = state.clone();

    if let Err(message) = ThreadedOperation::spawn_worker(
        "io::exists",
        raw_state,
        tx,
        move |state, tx: ThreadedWorkerPublisher| {
            if state.cancelled.load(Ordering::SeqCst) {
                let _ = tx.send(Err("io::exists was cancelled before starting".to_string()));
                return;
            }
            let found = path_buf.exists();
            *shared_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ok(found));
            let _ = tx.send(Ok(()));
        },
    ) {
        return Err(rollback_threaded_start(
            vm,
            op_id,
            VmError::HostError(message),
        ));
    }

    let shared_provider = shared.clone();
    vm.host.register_pending_op_result(
        raw,
        Box::new(move |_vm: &mut Vm| {
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

// ============================================================================
// Synchronous read/write/flush helpers
// ============================================================================

/// Clone (for files) or take (for pipes) the handle from a resource, so the
/// actual IO work can be offloaded to a worker thread.
pub(crate) fn take_file_or_pipe_handle(
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
pub(crate) fn take_file_or_write_pipe_handle(
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

// ============================================================================
// Resource helpers
// ============================================================================

pub(crate) fn insert_io_file_resource(vm: &mut Vm, resource: IoFileResource) -> VmResult<i64> {
    let mut ctx = vm.host_context();
    let token = ctx
        .push_resource_with_key(resource, super::io_file_key())
        .map_err(|error| VmError::HostError(format!("io resource insert failed: {error}")))?;
    let handle = token.handle();
    let raw = match handle.as_value() {
        Value::Int(value) => value,
        _ => unreachable!(),
    };
    Ok(raw)
}

pub(crate) fn insert_io_pipe_resource(
    vm: &mut Vm,
    resource: IoPipeResource,
) -> VmResult<crate::vm::resource::Resource<IoPipeResource>> {
    vm.host_context()
        .push_resource_with_key(resource, super::io_pipe_key())
        .map_err(|error| VmError::HostError(format!("io pipe resource insert failed: {error}")))
}

// ============================================================================
// Process helpers
// ============================================================================

#[cfg(unix)]
pub(crate) fn terminate_process_group(process_id: u32) {
    if let Ok(pid) = libc::pid_t::try_from(process_id) {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn terminate_process_group(process_id: u32) {
    #[cfg(windows)]
    crate::builtins::runtime::io::windows_process_tree::terminate_process_tree(process_id);
    #[cfg(not(windows))]
    let _ = process_id;
}

pub(crate) fn spawn_shell_command(command: &str, mode: &str) -> VmResult<std::process::Child> {
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

// ============================================================================
// Path helpers
// ============================================================================

pub(crate) fn authorize_io_path(vm: &Vm, path: &str, writes: bool) -> VmResult<PathBuf> {
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

pub(crate) fn canonicalize_io_target(path: &Path) -> VmResult<PathBuf> {
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

// ============================================================================
// Interruptible pipe helpers
// ============================================================================

#[cfg(unix)]
fn set_pipe_nonblocking(pipe: &impl std::os::fd::AsRawFd) -> std::io::Result<()> {
    let fd = pipe.as_raw_fd();
    // SAFETY: `fd` is borrowed from a live child-pipe object for the duration
    // of each fcntl call; no ownership is transferred.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: same valid borrowed descriptor, with the existing flags retained.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_pipe_nonblocking<T>(_pipe: &T) -> std::io::Result<()> {
    Ok(())
}

fn read_pipe_to_string_with_limit(
    pipe: &mut std::process::ChildStdout,
    max_read_bytes: Option<usize>,
    out: &mut String,
    cancelled: &AtomicBool,
) -> VmResult<()> {
    set_pipe_nonblocking(pipe)
        .map_err(|error| VmError::HostError(format!("io_read_all setup failed: {error}")))?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if cancelled.load(Ordering::SeqCst) {
            return Err(VmError::HostError("io_read_all was cancelled".to_string()));
        }
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                bytes.extend_from_slice(&chunk[..read]);
                if let Some(limit) = max_read_bytes
                    && bytes.len() > limit
                {
                    return Err(VmError::HostError(format!(
                        "io_read_all exceeds the configured read limit of {limit} bytes"
                    )));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(error) => {
                return Err(VmError::HostError(format!("io_read_all failed: {error}")));
            }
        }
    }
    *out = String::from_utf8(bytes)
        .map_err(|error| VmError::HostError(format!("io_read_all failed: {error}")))?;
    Ok(())
}

fn read_pipe_line_from_reader(
    pipe: &mut std::process::ChildStdout,
    max_read_bytes: Option<usize>,
    cancelled: &AtomicBool,
) -> VmResult<String> {
    set_pipe_nonblocking(pipe)
        .map_err(|error| VmError::HostError(format!("io_read_line setup failed: {error}")))?;
    let mut bytes = Vec::new();
    let mut one = [0u8; 1];
    loop {
        if cancelled.load(Ordering::SeqCst) {
            return Err(VmError::HostError("io_read_line was cancelled".to_string()));
        }
        match pipe.read(&mut one) {
            Ok(0) => break,
            Ok(_) => {
                bytes.push(one[0]);
                if let Some(limit) = max_read_bytes
                    && bytes.len() > limit
                {
                    return Err(VmError::HostError(format!(
                        "io_read_line exceeds the configured read limit of {limit} bytes"
                    )));
                }
                if one[0] == b'\n' {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(error) => {
                return Err(VmError::HostError(format!("io_read_line failed: {error}")));
            }
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn write_pipe_interruptible(
    pipe: &mut std::process::ChildStdin,
    bytes: &[u8],
    cancelled: &AtomicBool,
) -> Result<usize, String> {
    set_pipe_nonblocking(pipe).map_err(|error| format!("io_write setup failed: {error}"))?;
    loop {
        if cancelled.load(Ordering::SeqCst) {
            return Err("io_write was cancelled".to_string());
        }
        match pipe.write(bytes) {
            Ok(written) => return Ok(written),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(error) => return Err(format!("io_write failed: {error}")),
        }
    }
}

// ============================================================================
// Read helpers
// ============================================================================

pub(crate) fn read_to_string_with_limit(
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

pub(crate) fn read_line_from_reader(
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
        if let Some(limit) = max_read_bytes
            && bytes.len() > limit
        {
            return Err(VmError::HostError(format!(
                "io_read_line exceeds the configured read limit of {} bytes",
                limit
            )));
        }
        if one[0] == b'\n' {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub(crate) fn resource_handle(handle_id: i64) -> VmResult<ResourceHandle> {
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
