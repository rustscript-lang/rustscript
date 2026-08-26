//! Feature-selected async IO host implementation.
//!
//! This is the `async`-feature counterpart of the worker-thread
//! [`blocking`](super::blocking) implementation. Live handles are typed
//! [`IoResource`]s owned by the VM's execution scope (exactly like the
//! blocking path) and in-flight IO work runs through tokio; the guest-facing
//! builtins are async host functions that capture owned host context and
//! submit a future through the generic async host bridge.
//!
//! The guest-visible handle id is the raw resource token, so handles opened
//! on one path can be closed/read on the other.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use pd_host_function::pd_host_function;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use super::{IoPolicy, io_policy};
use crate::vm::operation::reason::OperationCancelReason;
use crate::vm::resource::close::{CloseProgress, HostResource};
use crate::vm::resource::error::ResourceResult;
use crate::vm::resource::{ResourceCloseReason, ResourceHandle};
use crate::vm::{
    CallReturn, CaptureAsyncHostContext, HostFutureOutput, HostOpId, Value, Vm, VmError, VmResult,
};

/// Per-VM IO host state for the async implementation.
///
/// Async IO builtins submit their work through the async host bridge
/// (`submitted_host_ops`), so no per-op value mailbox is needed here. The
/// empty state exists so the host runtime keeps one uniform `IoState` slot
/// across the blocking and async implementations.
#[derive(Default)]
pub(crate) struct IoState {}

/// Cancels one pending builtin IO operation through the execution scope.
///
/// Async IO ops are bridge-submitted; cancellation is delivered by the VM
/// through the bridge's `cancel_op`, so the runtime-owned registry never
/// sees them. This stub keeps the uniform `cancel_builtin_io_op` surface.
pub(crate) fn cancel_pending_op(_vm: &mut Vm, _op_id: HostOpId) {}

/// Polls one pending builtin IO operation.
///
/// Async IO ops are polled through the bridge's `poll_submitted_op` (the
/// VM routes them via `submitted_host_ops`), so this uniform surface is
/// never reached; it exists for shape parity with the blocking path.
pub(crate) fn poll_builtin_io_op(
    _vm: &mut Vm,
    op_id: HostOpId,
    _cx: &mut std::task::Context<'_>,
) -> std::task::Poll<VmResult<CallReturn>> {
    std::task::Poll::Ready(Err(VmError::HostError(format!(
        "async io op {op_id} has no runtime mailbox; expected bridge-driven poll"
    ))))
}

/// A file / child-process backed IO handle.
#[derive(Debug)]
pub(crate) enum IoHandle {
    File(BufReader<File>),
    PopenRead {
        child: Child,
        stdout: BufReader<ChildStdout>,
    },
    PopenWrite {
        child: Child,
        stdin: ChildStdin,
    },
}

/// The typed resource stored in the execution scope for one async IO handle.
///
/// Mirrors the blocking path: the handle lives behind an `Arc<Mutex<...>>`
/// so the async builtin can take/restore it while the resource stays in the
/// scope table. Closing is exact-once.
struct IoResource {
    handle: Arc<Mutex<Option<IoHandle>>>,
    closed: Arc<AtomicBool>,
    process_id: Arc<AtomicU32>,
}

impl IoResource {
    fn new(handle: IoHandle) -> Self {
        let process_id = match &handle {
            IoHandle::PopenRead { child, .. } | IoHandle::PopenWrite { child, .. } => {
                child.id().unwrap_or(0)
            }
            IoHandle::File(_) => 0,
        };
        Self {
            handle: Arc::new(Mutex::new(Some(handle))),
            closed: Arc::new(AtomicBool::new(false)),
            process_id: Arc::new(AtomicU32::new(process_id)),
        }
    }

    fn new_shared(cells: &IoResource) -> Self {
        Self {
            handle: Arc::clone(&cells.handle),
            closed: Arc::clone(&cells.closed),
            process_id: Arc::clone(&cells.process_id),
        }
    }

    async fn take_handle(&self) -> VmResult<IoHandle> {
        self.handle
            .lock()
            .await
            .take()
            .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))
    }
}

impl HostResource for IoResource {
    fn begin_close(&mut self, reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.closed.store(true, Ordering::SeqCst);
        // The handle cells are shared with in-flight futures; take the
        // handle exactly once (best-effort) and release the OS resource.
        if let Some(handle) = self
            .handle
            .try_lock()
            .ok()
            .and_then(|mut guard| guard.take())
        {
            close_io_handle_now(handle);
        }
        let pid = self.process_id.swap(0, Ordering::AcqRel);
        if pid != 0 {
            terminate_process_id(pid, reason);
        }
        Ok(CloseProgress::Ready)
    }
}

/// Closes an IO handle synchronously (dropping tokio handles releases the
/// underlying OS resources; a child process is terminated by pid).
fn close_io_handle_now(mut handle: IoHandle) {
    match &mut handle {
        IoHandle::File(file) => {
            std::mem::drop(file.get_mut().flush());
        }
        IoHandle::PopenRead { child, .. } | IoHandle::PopenWrite { child, .. } => {
            let _ = child.start_kill();
            std::mem::drop(child.wait());
        }
    }
}

async fn close_io_handle(handle: IoHandle) -> VmResult<()> {
    match handle {
        IoHandle::File(mut file) => {
            file.get_mut()
                .flush()
                .await
                .map_err(|error| VmError::HostError(format!("io_close flush failed: {error}")))?;
        }
        IoHandle::PopenRead { mut child, .. } => {
            let _ = child.start_kill();
            child.wait().await.map_err(|error| {
                VmError::HostError(format!("io_close popen wait failed: {error}"))
            })?;
        }
        IoHandle::PopenWrite { mut child, stdin } => {
            drop(stdin);
            let _ = child.start_kill();
            child.wait().await.map_err(|error| {
                VmError::HostError(format!("io_close popen wait failed: {error}"))
            })?;
        }
    }
    Ok(())
}

fn terminate_process_id(pid: u32, _reason: ResourceCloseReason) {
    if pid == 0 {
        return;
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// The per-call captured policy context.
#[derive(Clone)]
pub(crate) struct IoPolicyContext {
    policy: Option<IoPolicy>,
}

impl CaptureAsyncHostContext for IoPolicyContext {
    fn capture(vm: &mut Vm) -> VmResult<Self> {
        Ok(Self {
            policy: io_policy(vm),
        })
    }
}

/// The per-call captured handle context: shared resource cells plus the
/// policy byte limits, captured before the future is submitted.
pub(crate) struct IoHandleContext {
    handle: ResourceHandle,
    resource: IoResource,
    max_read_bytes: Option<usize>,
    max_write_bytes: Option<usize>,
}

impl CaptureAsyncHostContext for IoHandleContext {
    fn capture(_vm: &mut Vm) -> VmResult<Self> {
        Err(VmError::HostError(
            "io handle context requires call arguments".to_string(),
        ))
    }

    fn capture_with_args(vm: &mut Vm, args: &[Value]) -> VmResult<Self> {
        let handle_id = match args.first() {
            Some(Value::Int(value)) => *value,
            Some(_) => return Err(VmError::TypeMismatch("int")),
            None => return Err(VmError::HostError("missing io handle argument".to_string())),
        };
        let handle = io_parse_handle(handle_id)?;
        let resource = io_resource_for_handle(vm, handle)?;
        Ok(Self {
            handle,
            resource,
            max_read_bytes: io_policy(vm).map(|policy| policy.max_read_bytes),
            max_write_bytes: io_policy(vm).map(|policy| policy.max_write_bytes),
        })
    }
}

/// Opens a file handle for runtime I/O.
#[pd_host_function(name = "io::open")]
pub(crate) async fn builtin_io_open(
    #[pd_host_context] context: IoPolicyContext,
    path: String,
    mode: String,
) -> VmResult<HostFutureOutput<i64>> {
    let writes = match mode.as_str() {
        "r" => false,
        "w" | "a" | "r+" | "w+" | "a+" => true,
        other => {
            return Err(VmError::HostError(format!(
                "io_open unsupported mode '{other}'"
            )));
        }
    };
    let path = authorize_io_path(context.policy.as_ref(), &path, writes).await?;
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
        _ => unreachable!(),
    }
    let file = options
        .open(path)
        .await
        .map_err(|error| VmError::HostError(format!("io_open failed: {error}")))?;
    let handle = IoHandle::File(BufReader::new(file));
    Ok(HostFutureOutput::complete(move |vm| {
        let token = vm
            .execution_scope()
            .push_resource(IoResource::new(handle))
            .map_err(|error| VmError::HostError(format!("io resource insert failed: {error}")))?;
        Ok(token.into_handle().raw() as i64)
    }))
}

/// Starts a child process and returns a process-backed handle.
#[pd_host_function(name = "io::popen")]
pub(crate) async fn builtin_io_popen(
    #[pd_host_context] context: IoPolicyContext,
    command: String,
    mode: String,
) -> VmResult<HostFutureOutput<i64>> {
    if mode != "r" && mode != "w" {
        return Err(VmError::HostError(format!(
            "io_popen unsupported mode '{mode}'"
        )));
    }
    if !context
        .policy
        .as_ref()
        .is_none_or(|policy| policy.allow_process)
    {
        return Err(VmError::HostError(
            "io_popen requires the command capability".to_string(),
        ));
    }
    let handle = spawn_shell_command(&command, &mode)?;
    Ok(HostFutureOutput::complete(move |vm| {
        let token = vm
            .execution_scope()
            .push_resource(IoResource::new(handle))
            .map_err(|error| VmError::HostError(format!("io resource insert failed: {error}")))?;
        Ok(token.into_handle().raw() as i64)
    }))
}

/// Reads all remaining text from an I/O handle.
#[pd_host_function(name = "io::read_all")]
pub(crate) async fn builtin_io_read_all(
    #[pd_host_context] context: IoHandleContext,
    _handle_id: i64,
) -> VmResult<HostFutureOutput<String>> {
    let mut guard = context.resource.handle.lock().await;
    let handle = guard
        .as_mut()
        .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))?;
    let mut out = String::new();
    match handle {
        IoHandle::File(file) => file.read_to_string(&mut out).await,
        IoHandle::PopenRead { stdout, .. } => stdout.read_to_string(&mut out).await,
        IoHandle::PopenWrite { .. } => {
            return Err(VmError::HostError(
                "io_read_all cannot read from a write handle".to_string(),
            ));
        }
    }
    .map_err(|error| VmError::HostError(format!("io_read_all failed: {error}")))?;
    if context
        .max_read_bytes
        .is_some_and(|limit| out.len() > limit)
    {
        return Err(VmError::HostError(
            "io_read_all exceeded read limit".to_string(),
        ));
    }
    Ok(HostFutureOutput::returning(out))
}

/// Reads a single line of text from an I/O handle.
#[pd_host_function(name = "io::read_line")]
pub(crate) async fn builtin_io_read_line(
    #[pd_host_context] context: IoHandleContext,
    _handle_id: i64,
) -> VmResult<HostFutureOutput<String>> {
    let mut guard = context.resource.handle.lock().await;
    let handle = guard
        .as_mut()
        .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))?;
    let mut line = String::new();
    match handle {
        IoHandle::File(file) => file.read_line(&mut line).await,
        IoHandle::PopenRead { stdout, .. } => stdout.read_line(&mut line).await,
        IoHandle::PopenWrite { .. } => {
            return Err(VmError::HostError(
                "io_read_line cannot read from a write handle".to_string(),
            ));
        }
    }
    .map_err(|error| VmError::HostError(format!("io_read_line failed: {error}")))?;
    if context
        .max_read_bytes
        .is_some_and(|limit| line.len() > limit)
    {
        return Err(VmError::HostError(
            "io_read_line exceeded read limit".to_string(),
        ));
    }
    Ok(HostFutureOutput::returning(line))
}

/// Writes text to an I/O handle.
#[pd_host_function(name = "io::write")]
pub(crate) async fn builtin_io_write(
    #[pd_host_context] context: IoHandleContext,
    _handle_id: i64,
    text: String,
) -> VmResult<HostFutureOutput<i64>> {
    if context
        .max_write_bytes
        .is_some_and(|limit| text.len() > limit)
    {
        return Err(VmError::HostError(
            "io_write exceeded write limit".to_string(),
        ));
    }
    let mut guard = context.resource.handle.lock().await;
    let handle = guard
        .as_mut()
        .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))?;
    let written = match handle {
        IoHandle::File(file) => file.get_mut().write(text.as_bytes()).await,
        IoHandle::PopenWrite { stdin, .. } => stdin.write(text.as_bytes()).await,
        IoHandle::PopenRead { .. } => {
            return Err(VmError::HostError(
                "io_write cannot write to a read handle".to_string(),
            ));
        }
    }
    .map_err(|error| VmError::HostError(format!("io_write failed: {error}")))?;
    Ok(HostFutureOutput::returning(written as i64))
}

/// Flushes buffered output for an I/O handle.
#[pd_host_function(name = "io::flush")]
pub(crate) async fn builtin_io_flush(
    #[pd_host_context] context: IoHandleContext,
    _handle_id: i64,
) -> VmResult<HostFutureOutput<bool>> {
    let mut guard = context.resource.handle.lock().await;
    let handle = guard
        .as_mut()
        .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))?;
    match handle {
        IoHandle::File(file) => file.get_mut().flush().await,
        IoHandle::PopenWrite { stdin, .. } => stdin.flush().await,
        IoHandle::PopenRead { .. } => Ok(()),
    }
    .map_err(|error| VmError::HostError(format!("io_flush failed: {error}")))?;
    Ok(HostFutureOutput::returning(true))
}

/// Closes an I/O handle.
#[pd_host_function(name = "io::close")]
pub(crate) async fn builtin_io_close(
    #[pd_host_context] context: IoHandleContext,
    _handle_id: i64,
) -> VmResult<HostFutureOutput<bool>> {
    let resource = IoResource::new_shared(&context.resource);
    let handle = context.handle;
    let close_result = match resource.take_handle().await {
        Ok(handle) => close_io_handle(handle).await,
        Err(_) => Ok(()),
    };
    Ok(HostFutureOutput::complete(move |vm| {
        let _ = vm
            .execution_scope()
            .close_resource::<IoResource>(handle, ResourceCloseReason::Requested);
        close_result?;
        Ok(true)
    }))
}

/// Returns whether a file system path exists.
#[pd_host_function(name = "io::exists")]
pub(crate) async fn builtin_io_exists(
    #[pd_host_context] context: IoPolicyContext,
    path: String,
) -> VmResult<HostFutureOutput<bool>> {
    let path = authorize_io_path(context.policy.as_ref(), &path, false).await?;
    let exists = tokio::fs::try_exists(path)
        .await
        .map_err(|error| VmError::HostError(format!("io_exists failed: {error}")))?;
    Ok(HostFutureOutput::returning(exists))
}

#[allow(dead_code)]
pub(crate) fn cancel_builtin_io_op_with_reason(
    _vm: &mut Vm,
    _op_id: HostOpId,
    _reason: OperationCancelReason,
) {
}

async fn authorize_io_path(
    policy: Option<&IoPolicy>,
    path: &str,
    writes: bool,
) -> VmResult<PathBuf> {
    let requested = PathBuf::from(path);
    let Some(policy) = policy else {
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
    let canonical = canonicalize_io_target(&absolute).await?;
    for root in &policy.allowed_roots {
        let root = tokio::fs::canonicalize(Path::new(root))
            .await
            .map_err(|error| {
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

async fn canonicalize_io_target(path: &Path) -> VmResult<PathBuf> {
    if tokio::fs::try_exists(path)
        .await
        .map_err(|error| VmError::HostError(format!("io path resolution failed: {error}")))?
    {
        return tokio::fs::canonicalize(path)
            .await
            .map_err(|error| VmError::HostError(format!("io path resolution failed: {error}")));
    }
    // The target does not exist yet (e.g. a create-mode open): canonicalize
    // the parent and append the final component.
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let canonical_parent = tokio::fs::canonicalize(parent)
        .await
        .map_err(|error| VmError::HostError(format!("io path resolution failed: {error}")))?;
    let name = path
        .file_name()
        .ok_or_else(|| VmError::HostError("io path has no file name".to_string()))?;
    Ok(canonical_parent.join(name))
}

/// Looks up the shared cells of a live IO handle resource in the execution
/// scope, cloning them so the async builtin can take/restore the handle
/// while the resource stays in the scope table.
fn io_resource_for_handle(vm: &mut Vm, handle: ResourceHandle) -> VmResult<IoResource> {
    let token = vm
        .execution_scope()
        .resources()
        .typed::<IoResource>(handle)
        .map_err(|error| {
            VmError::HostError(format!(
                "io handle {:?} is not a live IO handle: {error}",
                handle.raw()
            ))
        })?;
    let resource = vm
        .execution_scope()
        .resources()
        .get::<IoResource>(&token)
        .map_err(|error| {
            VmError::HostError(format!(
                "io handle {:?} borrow failed: {error}",
                handle.raw()
            ))
        })?;
    Ok(IoResource::new_shared(&resource))
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

fn spawn_shell_command(command: &str, mode: &str) -> VmResult<IoHandle> {
    let mut child = if mode == "r" {
        Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .stdout(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
    } else {
        Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
    }
    .map_err(|error| VmError::HostError(format!("io_popen spawn failed: {error}")))?;

    if mode == "r" {
        let stdout = child.stdout.take().ok_or_else(|| {
            VmError::HostError("io_popen('r') did not provide stdout pipe".to_string())
        })?;
        Ok(IoHandle::PopenRead {
            child,
            stdout: BufReader::new(stdout),
        })
    } else {
        let stdin = child.stdin.take().ok_or_else(|| {
            VmError::HostError("io_popen('w') did not provide stdin pipe".to_string())
        })?;
        Ok(IoHandle::PopenWrite { child, stdin })
    }
}
