use std::fs::OpenOptions;
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use futures_channel::oneshot;
use pd_host_function::pd_host_function;

use super::super::HostCallResult;
use super::super::cancellation::{CancellationReason, OperationId, OperationOwner};
use super::super::error::{RuntimeError, RuntimeErrorCode};
use super::super::resource::{ResourceHandle, ResourceTypeId};
use crate::vm::{CallReturn, HostOpId, Value, Vm, VmError, VmResult};

pub(crate) enum IoHandle {
    File(std::fs::File),
    PopenRead { child: Child },
    PopenWrite { child: Child },
}

struct IoResource {
    handle: Mutex<Option<IoHandle>>,
    process_id: AtomicU32,
}

impl IoResource {
    fn new(handle: IoHandle) -> Self {
        let process_id = match &handle {
            IoHandle::PopenRead { child } | IoHandle::PopenWrite { child } => Some(child.id()),
            IoHandle::File(_) => None,
        };
        Self {
            handle: Mutex::new(Some(handle)),
            process_id: AtomicU32::new(process_id.unwrap_or(0)),
        }
    }

    fn with_handle_mut<T>(&self, apply: impl FnOnce(&mut IoHandle) -> VmResult<T>) -> VmResult<T> {
        let mut handle = self
            .handle
            .lock()
            .map_err(|_| VmError::HostError("io resource lock was poisoned".to_string()))?;
        let handle = handle
            .as_mut()
            .ok_or_else(|| VmError::HostError("io resource is already closing".to_string()))?;
        apply(handle)
    }

    fn take_handle(&self) -> VmResult<IoHandle> {
        self.handle
            .lock()
            .map_err(|_| VmError::HostError("io resource lock was poisoned".to_string()))?
            .take()
            .ok_or_else(|| VmError::HostError("io resource is already closing".to_string()))
    }

    fn close(&self, reason: CancellationReason) -> VmResult<()> {
        let process_id = self.process_id.swap(0, Ordering::AcqRel);
        let termination_error = if reason != CancellationReason::ResourceClosed && process_id != 0 {
            terminate_process_tree(process_id).err()
        } else {
            None
        };

        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match self.handle.try_lock() {
                Ok(mut handle) => {
                    let close_result = match handle.take() {
                        Some(handle) => close_io_handle(handle, reason),
                        None => Ok(()),
                    };
                    return match close_result {
                        Err(error) => Err(error),
                        Ok(()) => termination_error.map_or(Ok(()), Err),
                    };
                }
                Err(TryLockError::Poisoned(_)) => {
                    return Err(VmError::HostError(
                        "io resource lock was poisoned".to_string(),
                    ));
                }
                Err(TryLockError::WouldBlock) if Instant::now() >= deadline => {
                    let termination_detail = termination_error
                        .as_ref()
                        .map(|error| format!("; process termination failed: {error}"))
                        .unwrap_or_default();
                    return Err(VmError::HostError(format!(
                        "timed out interrupting pending io operation{termination_detail}"
                    )));
                }
                Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
}

impl Drop for IoResource {
    fn drop(&mut self) {
        let _ = self.close(CancellationReason::VmReset);
    }
}

struct IoAsyncCompletion {
    opened_handle: Option<IoHandle>,
    closed_handle: Option<ResourceHandle>,
    result: VmResult<CallReturn>,
}

impl IoAsyncCompletion {
    fn result(result: VmResult<CallReturn>) -> Self {
        Self {
            opened_handle: None,
            closed_handle: None,
            result,
        }
    }
}

impl Drop for IoAsyncCompletion {
    fn drop(&mut self) {
        let Some(handle) = self.opened_handle.take() else {
            return;
        };
        let _ = IoResource::new(handle).close(CancellationReason::VmReset);
    }
}

pub(crate) fn poll_builtin_io_op(
    vm: &mut Vm,
    op_id: HostOpId,
    cx: &mut Context<'_>,
) -> Poll<VmResult<CallReturn>> {
    let operation_id = match OperationId::from_raw(op_id) {
        Ok(operation_id) => operation_id,
        Err(error) => return Poll::Ready(Err(runtime_host_error(error))),
    };
    let operation = match vm.host.runtime_operations.get(operation_id) {
        Ok(operation) => operation,
        Err(error) => return Poll::Ready(Err(runtime_host_error(error))),
    };
    let Some(callback) = operation.payload() else {
        return Poll::Ready(Err(VmError::HostError(format!(
            "builtin io op {op_id} has no completion payload",
        ))));
    };
    let poll_result = {
        let receiver = match vm
            .host
            .runtime_resources
            .get_mut::<oneshot::Receiver<IoAsyncCompletion>>(callback, ResourceTypeId::CALLBACK)
        {
            Ok(receiver) => receiver,
            Err(error) => return Poll::Ready(Err(runtime_host_error(error))),
        };
        Pin::new(receiver).poll(cx)
    };

    match poll_result {
        Poll::Pending => Poll::Pending,
        Poll::Ready(Ok(mut completion)) => {
            let _ = super::super::close_runtime_resource(
                vm,
                callback,
                CancellationReason::ResourceClosed,
            );

            if let Some(closed_handle) = completion.closed_handle
                && let Err(error) = super::super::close_runtime_resource(
                    vm,
                    closed_handle,
                    CancellationReason::ResourceClosed,
                )
            {
                completion.result = Err(runtime_host_error(error));
            }
            if let Some(opened_handle) = completion.opened_handle.take() {
                let result = insert_io_resource(vm, opened_handle)
                    .map(|handle| CallReturn::one(handle.as_value()));
                completion.result = result;
            }
            Poll::Ready(std::mem::replace(
                &mut completion.result,
                Ok(CallReturn::none()),
            ))
        }
        Poll::Ready(Err(_)) => {
            let _ =
                super::super::close_runtime_resource(vm, callback, CancellationReason::Requested);
            Poll::Ready(Err(VmError::HostError(format!(
                "builtin io op {op_id} was cancelled",
            ))))
        }
    }
}

/// Opens a file handle for runtime I/O.
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
    let op_id = schedule_io_task(vm, None, move || {
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
                return IoAsyncCompletion::result(Err(VmError::HostError(format!(
                    "unsupported io_open mode '{other}', expected r/w/a/r+/w+/a+",
                ))));
            }
        }

        match options.open(path) {
            Ok(file) => IoAsyncCompletion {
                opened_handle: Some(IoHandle::File(file)),
                closed_handle: None,
                result: Ok(CallReturn::none()),
            },
            Err(err) => {
                IoAsyncCompletion::result(Err(VmError::HostError(format!("io_open failed: {err}"))))
            }
        }
    })?;
    Ok(HostCallResult::Pending(op_id))
}

/// Starts a child process and returns a process-backed handle.
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
    if super::io_policy(vm).is_some_and(|policy| !policy.allow_process) {
        return Err(VmError::HostError(
            "io_popen requires the process capability".to_string(),
        ));
    }
    let command = command.to_string();
    let mode = mode.to_string();
    let op_id = schedule_io_task(vm, None, move || {
        let child = match spawn_shell_command(command.as_str(), mode.as_str()) {
            Ok(child) => child,
            Err(err) => return IoAsyncCompletion::result(Err(err)),
        };
        let handle = match mode.as_str() {
            "r" => {
                if child.stdout.is_none() {
                    return IoAsyncCompletion::result(Err(VmError::HostError(
                        "io_popen('r') did not provide stdout pipe".to_string(),
                    )));
                }
                IoHandle::PopenRead { child }
            }
            "w" => {
                if child.stdin.is_none() {
                    return IoAsyncCompletion::result(Err(VmError::HostError(
                        "io_popen('w') did not provide stdin pipe".to_string(),
                    )));
                }
                IoHandle::PopenWrite { child }
            }
            _ => unreachable!("mode validated above"),
        };
        IoAsyncCompletion {
            opened_handle: Some(handle),
            closed_handle: None,
            result: Ok(CallReturn::none()),
        }
    })?;
    Ok(HostCallResult::Pending(op_id))
}

/// Reads all remaining text from an I/O handle.
#[pd_host_function(name = "io::read_all")]
pub(crate) fn builtin_io_read_all(vm: &mut Vm, handle_id: i64) -> VmResult<HostCallResult<String>> {
    let max_read_bytes = super::io_policy(vm).map(|policy| policy.max_read_bytes);
    let handle = resource_handle(handle_id)?;
    let resource = io_resource_for_handle(vm, handle)?;
    let op_id = schedule_io_task(vm, Some(handle), move || {
        let result = resource.with_handle_mut(|handle| {
            let mut out = String::new();
            match handle {
                IoHandle::File(file) => {
                    read_to_string_with_limit(file, max_read_bytes, &mut out)?;
                }
                IoHandle::PopenRead { child } => {
                    read_to_string_with_limit(
                        child.stdout.as_mut().ok_or_else(|| {
                            VmError::HostError(
                                "io_read_all popen handle missing stdout".to_string(),
                            )
                        })?,
                        max_read_bytes,
                        &mut out,
                    )?;
                }
                IoHandle::PopenWrite { .. } => {
                    return Err(VmError::HostError(
                        "io_read_all requires a readable handle".to_string(),
                    ));
                }
            };
            Ok(CallReturn::one(Value::string(out)))
        });
        IoAsyncCompletion::result(result)
    })?;
    Ok(HostCallResult::Pending(op_id))
}

/// Reads a single line of text from an I/O handle.
#[pd_host_function(name = "io::read_line")]
pub(crate) fn builtin_io_read_line(
    vm: &mut Vm,
    handle_id: i64,
) -> VmResult<HostCallResult<String>> {
    let max_read_bytes = super::io_policy(vm).map(|policy| policy.max_read_bytes);
    let handle = resource_handle(handle_id)?;
    let resource = io_resource_for_handle(vm, handle)?;
    let op_id = schedule_io_task(vm, Some(handle), move || {
        let result = resource.with_handle_mut(|handle| {
            let line = match handle {
                IoHandle::File(file) => read_line_from_reader(file, max_read_bytes)?,
                IoHandle::PopenRead { child } => read_line_from_reader(
                    child.stdout.as_mut().ok_or_else(|| {
                        VmError::HostError("io_read_line popen handle missing stdout".to_string())
                    })?,
                    max_read_bytes,
                )?,
                IoHandle::PopenWrite { .. } => {
                    return Err(VmError::HostError(
                        "io_read_line requires a readable handle".to_string(),
                    ));
                }
            };
            Ok(CallReturn::one(Value::string(line)))
        });
        IoAsyncCompletion::result(result)
    })?;
    Ok(HostCallResult::Pending(op_id))
}

/// Writes text to an I/O handle.
#[pd_host_function(name = "io::write")]
pub(crate) fn builtin_io_write(
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
    let resource = io_resource_for_handle(vm, handle)?;
    let op_id = schedule_io_task(vm, Some(handle), move || {
        let result = resource.with_handle_mut(|handle| {
            let written = match handle {
                IoHandle::File(file) => file
                    .write(&bytes)
                    .map_err(|err| VmError::HostError(format!("io_write failed: {err}")))?,
                IoHandle::PopenWrite { child } => child
                    .stdin
                    .as_mut()
                    .ok_or_else(|| {
                        VmError::HostError("io_write popen handle missing stdin".to_string())
                    })?
                    .write(&bytes)
                    .map_err(|err| VmError::HostError(format!("io_write failed: {err}")))?,
                IoHandle::PopenRead { .. } => {
                    return Err(VmError::HostError(
                        "io_write requires a writable handle".to_string(),
                    ));
                }
            };
            Ok(CallReturn::one(Value::Int(written as i64)))
        });
        IoAsyncCompletion::result(result)
    })?;
    Ok(HostCallResult::Pending(op_id))
}

/// Flushes buffered output for an I/O handle.
#[pd_host_function(name = "io::flush")]
pub(crate) fn builtin_io_flush(vm: &mut Vm, handle_id: i64) -> VmResult<HostCallResult<bool>> {
    let handle = resource_handle(handle_id)?;
    let resource = io_resource_for_handle(vm, handle)?;
    let op_id = schedule_io_task(vm, Some(handle), move || {
        let result = resource.with_handle_mut(|handle| {
            match handle {
                IoHandle::File(file) => file
                    .flush()
                    .map_err(|err| VmError::HostError(format!("io_flush failed: {err}")))?,
                IoHandle::PopenWrite { child } => child
                    .stdin
                    .as_mut()
                    .ok_or_else(|| {
                        VmError::HostError("io_flush popen handle missing stdin".to_string())
                    })?
                    .flush()
                    .map_err(|err| VmError::HostError(format!("io_flush failed: {err}")))?,
                IoHandle::PopenRead { .. } => {}
            }
            Ok(CallReturn::one(Value::Bool(true)))
        });
        IoAsyncCompletion::result(result)
    })?;
    Ok(HostCallResult::Pending(op_id))
}

/// Closes an I/O handle.
#[pd_host_function(name = "io::close")]
pub(crate) fn builtin_io_close(vm: &mut Vm, handle_id: i64) -> VmResult<HostCallResult<bool>> {
    let handle = resource_handle(handle_id)?;
    let resource = io_resource_for_handle(vm, handle)?;
    let op_id = schedule_io_task(vm, Some(handle), move || {
        let result = resource
            .take_handle()
            .and_then(|handle| close_io_handle(handle, CancellationReason::ResourceClosed))
            .map(|_| CallReturn::one(Value::Bool(true)));
        IoAsyncCompletion {
            opened_handle: None,
            closed_handle: Some(handle),
            result,
        }
    })?;
    Ok(HostCallResult::Pending(op_id))
}

/// Returns whether a file system path exists.
#[pd_host_function(name = "io::exists")]
pub(crate) fn builtin_io_exists(vm: &mut Vm, path: &str) -> VmResult<HostCallResult<bool>> {
    let path = authorize_io_path(vm, path, false)?;
    let op_id = schedule_io_task(vm, None, move || {
        IoAsyncCompletion::result(Ok(CallReturn::one(Value::Bool(path.exists()))))
    })?;
    Ok(HostCallResult::Pending(op_id))
}

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
    process.process_group(0);

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

fn resource_handle(handle_id: i64) -> VmResult<ResourceHandle> {
    if handle_id <= 0 {
        return Err(VmError::HostError(format!(
            "invalid io handle id {handle_id}; expected positive handle id"
        )));
    }
    ResourceHandle::from_value(&Value::Int(handle_id)).map_err(runtime_host_error)
}

fn io_resource_for_handle(vm: &Vm, handle: ResourceHandle) -> VmResult<Arc<IoResource>> {
    vm.host
        .runtime_resources
        .get::<Arc<IoResource>>(handle, ResourceTypeId::IO_FILE)
        .cloned()
        .map_err(runtime_host_error)
}

fn insert_io_resource(vm: &mut Vm, handle: IoHandle) -> VmResult<ResourceHandle> {
    vm.host
        .runtime_resources
        .insert_with_cleanup(
            ResourceTypeId::IO_FILE,
            Arc::new(IoResource::new(handle)),
            |resource, reason| resource.close(reason).map_err(io_cleanup_error),
        )
        .map_err(runtime_host_error)
}

fn schedule_io_task(
    vm: &mut Vm,
    target_resource: Option<ResourceHandle>,
    task: impl FnOnce() -> IoAsyncCompletion + Send + 'static,
) -> VmResult<HostOpId> {
    let operation = vm
        .host
        .runtime_operations
        .start_owned(
            OperationOwner::Io,
            Some(&vm.run_ctx.cancellation),
            None,
            None,
        )
        .map_err(runtime_host_error)?;
    if let Some(target_resource) = target_resource {
        operation.set_resource(target_resource);
    }
    let op_id = operation.id().raw();
    let worker_operation = operation.clone();
    let worker_token = operation.token();
    let (sender, receiver) = oneshot::channel();
    let callback = match vm
        .host
        .runtime_resources
        .insert(ResourceTypeId::CALLBACK, receiver)
    {
        Ok(callback) => callback,
        Err(error) => {
            let _ = vm
                .host
                .runtime_operations
                .cancel(operation.id(), CancellationReason::Requested);
            return Err(runtime_host_error(error));
        }
    };
    operation.set_payload(callback);

    let completion = if let Some(reason) = worker_token.reason() {
        IoAsyncCompletion::result(Err(VmError::HostError(format!(
            "io operation cancelled: {reason:?}"
        ))))
    } else {
        task()
    };
    match &completion.result {
        Ok(_) => {
            let _ = worker_operation.complete();
        }
        Err(error) => {
            let _ = worker_operation.fail(
                RuntimeError::new(
                    RuntimeErrorCode::OperationFailed,
                    "io::operation",
                    error.to_string(),
                )
                .with_value(op_id),
            );
        }
    }
    let _ = sender.send(completion);

    Ok(op_id)
}

fn runtime_host_error(error: impl std::fmt::Display) -> VmError {
    VmError::HostError(error.to_string())
}

fn io_cleanup_error(error: VmError) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::ResourceCleanupFailed,
        "io::close",
        error.to_string(),
    )
}

fn close_io_handle(mut handle: IoHandle, reason: CancellationReason) -> VmResult<()> {
    match &mut handle {
        IoHandle::File(file) => {
            file.flush().ok();
        }
        IoHandle::PopenRead { child } => wait_for_child(child, reason)?,
        IoHandle::PopenWrite { child } => {
            let _ = child.stdin.take();
            wait_for_child(child, reason)?;
        }
    }
    Ok(())
}

fn wait_for_child(child: &mut Child, reason: CancellationReason) -> VmResult<()> {
    if reason == CancellationReason::ResourceClosed {
        child
            .wait()
            .map_err(|err| VmError::HostError(format!("io_close popen wait failed: {err}")))?;
        return Ok(());
    }

    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() >= deadline => {
                if let Err(kill_error) = child.kill() {
                    return match child.try_wait() {
                        Ok(Some(_)) => Ok(()),
                        Ok(None) => Err(VmError::HostError(format!(
                            "timed out waiting for cancelled io process; direct child fallback failed: {kill_error}"
                        ))),
                        Err(wait_error) => Err(VmError::HostError(format!(
                            "direct child fallback failed: {kill_error}; child status check failed: {wait_error}"
                        ))),
                    };
                }
                child.wait().map_err(|error| {
                    VmError::HostError(format!(
                        "io_close popen wait after direct child fallback failed: {error}"
                    ))
                })?;
                return Ok(());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                return Err(VmError::HostError(format!(
                    "io_close popen wait failed: {error}"
                )));
            }
        }
    }
}

#[cfg(unix)]
fn terminate_process_tree(process_id: u32) -> VmResult<()> {
    let process_id = libc::pid_t::try_from(process_id).map_err(|_| {
        VmError::HostError(format!(
            "io_close popen process id {process_id} exceeds the platform pid range"
        ))
    })?;
    let group_result = signal_unix_process(-process_id);
    match group_result {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
        Err(group_error) => {
            let fallback_result = signal_unix_process(process_id);
            let fallback_detail = match fallback_result {
                Ok(()) => "direct process fallback succeeded".to_string(),
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {
                    "direct process had already exited".to_string()
                }
                Err(error) => format!("direct process fallback failed: {error}"),
            };
            Err(VmError::HostError(format!(
                "io_close popen process-group termination failed: {group_error}; {fallback_detail}"
            )))
        }
    }
}

#[cfg(unix)]
fn signal_unix_process(process_id: libc::pid_t) -> std::io::Result<()> {
    // SAFETY: process_id is either the tracked child pid or its negative process-group id.
    if unsafe { libc::kill(process_id, libc::SIGKILL) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn terminate_process_tree(process_id: u32) -> VmResult<()> {
    windows_process_tree::terminate(process_id)
}

#[cfg(windows)]
mod windows_process_tree {
    use std::collections::{HashMap, HashSet};
    use std::ffi::c_void;
    use std::io;
    use std::mem;
    use std::ptr;

    use super::{VmError, VmResult};

    type Handle = *mut c_void;

    const INVALID_HANDLE_VALUE: Handle = -1_isize as Handle;
    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    const PROCESS_TERMINATE: u32 = 0x0001;
    const ERROR_NO_MORE_FILES: i32 = 18;
    const ERROR_INVALID_PARAMETER: i32 = 87;

    #[repr(C)]
    struct ProcessEntry32W {
        size: u32,
        usage_count: u32,
        process_id: u32,
        default_heap_id: usize,
        module_id: u32,
        thread_count: u32,
        parent_process_id: u32,
        base_priority: i32,
        flags: u32,
        executable: [u16; 260],
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;
        fn Process32FirstW(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;
        fn OpenProcess(access: u32, inherit_handle: i32, process_id: u32) -> Handle;
        fn TerminateProcess(process: Handle, exit_code: u32) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }

    pub(crate) fn terminate(root_process_id: u32) -> VmResult<()> {
        let descendants = match descendant_processes(root_process_id) {
            Ok(descendants) => descendants,
            Err(snapshot_error) => {
                let fallback_detail = match terminate_process(root_process_id) {
                    Ok(()) => "direct process fallback succeeded".to_string(),
                    Err(error) => format!("direct process fallback failed: {error}"),
                };
                return Err(VmError::HostError(format!(
                    "io_close popen Windows process-tree snapshot failed: {snapshot_error}; {fallback_detail}"
                )));
            }
        };
        let mut first_error = None;
        for process_id in descendants.into_iter().rev() {
            if let Err(error) = terminate_process(process_id) {
                first_error.get_or_insert(error);
            }
        }
        if let Err(error) = terminate_process(root_process_id) {
            first_error.get_or_insert(error);
        }

        match first_error {
            Some(error) => Err(VmError::HostError(format!(
                "io_close popen Windows process-tree termination failed: {error}"
            ))),
            None => Ok(()),
        }
    }

    fn descendant_processes(root_process_id: u32) -> VmResult<Vec<u32>> {
        let entries = snapshot_processes().map_err(|error| {
            VmError::HostError(format!(
                "io_close popen Windows process snapshot failed: {error}"
            ))
        })?;
        let mut children_by_parent = HashMap::<u32, Vec<u32>>::new();
        for (process_id, parent_process_id) in entries {
            children_by_parent
                .entry(parent_process_id)
                .or_default()
                .push(process_id);
        }

        let mut descendants = Vec::new();
        let mut visited = HashSet::new();
        let mut pending = vec![root_process_id];
        visited.insert(root_process_id);
        while let Some(parent_process_id) = pending.pop() {
            let Some(children) = children_by_parent.get(&parent_process_id) else {
                continue;
            };
            for &child_process_id in children {
                if visited.insert(child_process_id) {
                    descendants.push(child_process_id);
                    pending.push(child_process_id);
                }
            }
        }
        Ok(descendants)
    }

    fn snapshot_processes() -> io::Result<Vec<(u32, u32)>> {
        // SAFETY: the snapshot API receives fixed constants and initialized storage of the
        // documented PROCESSENTRY32W layout. Every acquired handle is closed below.
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }

            let mut entry: ProcessEntry32W = mem::zeroed();
            entry.size = mem::size_of::<ProcessEntry32W>() as u32;
            let mut entries = Vec::new();
            if Process32FirstW(snapshot, &mut entry) == 0 {
                let error = io::Error::last_os_error();
                let _ = CloseHandle(snapshot);
                if error.raw_os_error() == Some(ERROR_NO_MORE_FILES) {
                    return Ok(entries);
                }
                return Err(error);
            }

            loop {
                entries.push((entry.process_id, entry.parent_process_id));
                entry = mem::zeroed();
                entry.size = mem::size_of::<ProcessEntry32W>() as u32;
                if Process32NextW(snapshot, &mut entry) == 0 {
                    let error = io::Error::last_os_error();
                    let close_result = CloseHandle(snapshot);
                    if error.raw_os_error() != Some(ERROR_NO_MORE_FILES) {
                        return Err(error);
                    }
                    if close_result == 0 {
                        return Err(io::Error::last_os_error());
                    }
                    return Ok(entries);
                }
            }
        }
    }

    fn terminate_process(process_id: u32) -> io::Result<()> {
        // SAFETY: OpenProcess returns an owned kernel handle which is closed on every path.
        unsafe {
            let process = OpenProcess(PROCESS_TERMINATE, 0, process_id);
            if process == ptr::null_mut() {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER) {
                    return Ok(());
                }
                return Err(error);
            }
            let terminate_result = TerminateProcess(process, 1);
            let terminate_error = (terminate_result == 0).then(io::Error::last_os_error);
            let close_result = CloseHandle(process);
            if let Some(error) = terminate_error {
                return Err(error);
            }
            if close_result == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn terminate_process_tree(process_id: u32) -> VmResult<()> {
    Err(VmError::HostError(format!(
        "io_close popen process-tree termination is unsupported for process {process_id}"
    )))
}

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
        if max_read_bytes.is_some_and(|limit| bytes.len() > limit) {
            return Err(VmError::HostError(format!(
                "io_read_line exceeds the configured read limit of {} bytes",
                max_read_bytes.expect("read limit should be present")
            )));
        }
        if one[0] == b'\n' {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
