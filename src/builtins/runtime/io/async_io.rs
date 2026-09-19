//! Tokio-backed IO hosts for builds with the `async` feature.
//!
//! Each call is an ordinary annotated async function. Open file and process
//! handles remain typed execution-scope resources because they span guest
//! calls; transient reads, writes, flushes, and closes rely on the generic
//! submitted-future lifecycle.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::task::{Context, Poll};

use pd_host_function::pd_host_function;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use super::{IoPolicy, io_policy};
use crate::vm::resource::close::{CloseProgress, HostResource};
use crate::vm::resource::error::{ResourceError, ResourceErrorCode, ResourceResult};
use crate::vm::resource::{ResourceCloseReason, ResourceHandle};
use crate::vm::{CaptureAsyncHostContext, HostFutureOutput, Value, Vm, VmError, VmResult};

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

impl Drop for IoHandle {
    fn drop(&mut self) {
        match self {
            Self::PopenRead { child, .. } | Self::PopenWrite { child, .. } => {
                terminate_process_id(child.id().unwrap_or(0));
                let _ = child.start_kill();
            }
            Self::File(_) => {}
        }
    }
}

/// Shared handle state captured by async calls.
struct IoResourceState {
    handle: Mutex<Option<IoHandle>>,
    closed: AtomicBool,
    process_id: AtomicU32,
}

impl IoResourceState {
    fn new(handle: IoHandle) -> Self {
        let process_id = process_id(&handle);
        Self {
            handle: Mutex::new(Some(handle)),
            closed: AtomicBool::new(false),
            process_id: AtomicU32::new(process_id),
        }
    }

    fn ensure_open(&self, operation: &str) -> VmResult<()> {
        if self.closed.load(Ordering::Acquire) {
            Err(VmError::HostError(format!("{operation} handle is closed")))
        } else {
            Ok(())
        }
    }
}

/// The typed resource stored in the execution scope for one async IO handle.
struct IoResource {
    state: Arc<IoResourceState>,
}

impl IoResource {
    fn new(handle: IoHandle) -> Self {
        Self {
            state: Arc::new(IoResourceState::new(handle)),
        }
    }

    fn close_nonblocking(&mut self) -> ResourceResult<CloseProgress> {
        self.state.closed.store(true, Ordering::Release);
        terminate_process_id(self.state.process_id.load(Ordering::Acquire));
        let Ok(mut slot) = self.state.handle.try_lock() else {
            return Ok(CloseProgress::Pending);
        };
        if let Some(mut handle) = slot.take() {
            start_close_io_handle(&mut handle)?;
        }
        self.state.process_id.store(0, Ordering::Release);
        Ok(CloseProgress::Ready)
    }
}

impl Drop for IoResource {
    fn drop(&mut self) {
        self.state.closed.store(true, Ordering::Release);
        terminate_process_id(self.state.process_id.swap(0, Ordering::AcqRel));
        if let Ok(mut slot) = self.state.handle.try_lock()
            && let Some(mut handle) = slot.take()
        {
            let _ = start_close_io_handle(&mut handle);
        }
    }
}

impl crate::host_extension::HostResourceType for IoResource {
    const KEY: &'static str = super::IO_FILE_KEY;
    const DESCRIPTION: &'static str = super::IO_FILE_DESCRIPTION;
}

/// The canonical declaration for the `io.file` resource type.
pub(crate) fn io_file_resource() -> crate::host_extension::HostResourceTypeMeta {
    crate::host_extension::HostResourceTypeMeta::of::<IoResource>()
}

impl HostResource for IoResource {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.close_nonblocking()
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        match self.close_nonblocking() {
            Ok(CloseProgress::Ready) => Poll::Ready(Ok(())),
            Ok(CloseProgress::Pending) => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(error) => Poll::Ready(Err(error)),
        }
    }
}

fn start_close_io_handle(handle: &mut IoHandle) -> ResourceResult<()> {
    match handle {
        IoHandle::File(_) => Ok(()),
        IoHandle::PopenRead { child, .. } | IoHandle::PopenWrite { child, .. } => {
            terminate_process_id(child.id().unwrap_or(0));
            match child.start_kill() {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
                Err(error) => Err(ResourceError::new(
                    ResourceErrorCode::ResourceCleanupFailed,
                    "io::resource",
                    format!("io_close popen terminate failed: {error}"),
                )),
            }
        }
    }
}

async fn close_io_handle(mut handle: IoHandle) -> VmResult<()> {
    match &mut handle {
        IoHandle::File(file) => {
            file.get_mut()
                .flush()
                .await
                .map_err(|error| VmError::HostError(format!("io_close flush failed: {error}")))?;
        }
        IoHandle::PopenRead { child, .. } => {
            terminate_process_id(child.id().unwrap_or(0));
            kill_and_reap_child(child).await?;
        }
        IoHandle::PopenWrite { child, stdin } => {
            let _ = stdin.shutdown().await;
            terminate_process_id(child.id().unwrap_or(0));
            kill_and_reap_child(child).await?;
        }
    }
    Ok(())
}

async fn kill_and_reap_child(child: &mut Child) -> VmResult<()> {
    match child.kill().await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
            child.wait().await.map(|_| ()).map_err(|wait_error| {
                VmError::HostError(format!("io_close popen wait failed: {wait_error}"))
            })
        }
        Err(error) => Err(VmError::HostError(format!(
            "io_close popen wait failed: {error}"
        ))),
    }
}

fn process_id(handle: &IoHandle) -> u32 {
    match handle {
        IoHandle::PopenRead { child, .. } | IoHandle::PopenWrite { child, .. } => {
            child.id().unwrap_or(0)
        }
        IoHandle::File(_) => 0,
    }
}

fn terminate_process_id(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(unix)]
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .status();
    }
    #[cfg(not(any(unix, windows)))]
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

/// Shared handle state and byte limits captured before an async call starts.
pub(crate) struct IoHandleContext {
    handle: ResourceHandle,
    state: Arc<IoResourceState>,
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
        let state = io_state_for_handle(vm, handle)?;
        Ok(Self {
            handle,
            state,
            max_read_bytes: io_policy(vm).map(|policy| policy.max_read_bytes),
            max_write_bytes: io_policy(vm).map(|policy| policy.max_write_bytes),
        })
    }
}

/// Opens a file handle for runtime I/O.
#[pd_host_function(name = "io::open", contract = super::io_open_contract)]
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
                "unsupported io_open mode '{other}', expected r/w/a/r+/w+/a+"
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
        _ => unreachable!("mode validated above"),
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
            "unsupported io_popen mode '{mode}', expected r or w"
        )));
    }
    if context
        .policy
        .as_ref()
        .is_some_and(|policy| !policy.allow_process)
    {
        return Err(VmError::HostError(
            "io_popen requires the process capability".to_string(),
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
#[pd_host_function(name = "io::read_all", contract = super::io_read_all_contract)]
pub(crate) async fn builtin_io_read_all(
    #[pd_host_context] context: IoHandleContext,
    _handle_id: i64,
) -> VmResult<HostFutureOutput<String>> {
    context.state.ensure_open("io_read_all")?;
    let mut slot = context.state.handle.lock().await;
    context.state.ensure_open("io_read_all")?;
    let handle = slot
        .as_mut()
        .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))?;
    let mut out = String::new();
    match handle {
        IoHandle::File(file) => file.read_to_string(&mut out).await,
        IoHandle::PopenRead { stdout, .. } => stdout.read_to_string(&mut out).await,
        IoHandle::PopenWrite { .. } => {
            return Err(VmError::HostError(
                "io_read_all requires a readable handle".to_string(),
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
    context.state.ensure_open("io_read_line")?;
    let mut slot = context.state.handle.lock().await;
    context.state.ensure_open("io_read_line")?;
    let handle = slot
        .as_mut()
        .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))?;
    let mut line = String::new();
    match handle {
        IoHandle::File(file) => file.read_line(&mut line).await,
        IoHandle::PopenRead { stdout, .. } => stdout.read_line(&mut line).await,
        IoHandle::PopenWrite { .. } => {
            return Err(VmError::HostError(
                "io_read_line requires a readable handle".to_string(),
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
    context.state.ensure_open("io_write")?;
    let mut slot = context.state.handle.lock().await;
    context.state.ensure_open("io_write")?;
    let handle = slot
        .as_mut()
        .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))?;
    let written = match handle {
        IoHandle::File(file) => file.get_mut().write(text.as_bytes()).await,
        IoHandle::PopenWrite { stdin, .. } => stdin.write(text.as_bytes()).await,
        IoHandle::PopenRead { .. } => {
            return Err(VmError::HostError(
                "io_write requires a writable handle".to_string(),
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
    context.state.ensure_open("io_flush")?;
    let mut slot = context.state.handle.lock().await;
    context.state.ensure_open("io_flush")?;
    let handle = slot
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
#[pd_host_function(name = "io::close", contract = super::io_close_contract)]
pub(crate) async fn builtin_io_close(
    #[pd_host_context] context: IoHandleContext,
    _handle_id: i64,
) -> VmResult<HostFutureOutput<bool>> {
    if context.state.closed.swap(true, Ordering::AcqRel) {
        return Err(VmError::HostError("io_close handle is closed".to_string()));
    }
    let owned = context
        .state
        .handle
        .lock()
        .await
        .take()
        .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))?;
    let close_result = close_io_handle(owned).await;
    if close_result.is_ok() {
        context.state.process_id.store(0, Ordering::Release);
    }
    let handle = context.handle;
    Ok(HostFutureOutput::complete(move |vm| {
        let progress = vm
            .execution_scope()
            .close_resource::<IoResource>(handle, ResourceCloseReason::Requested)
            .map_err(|error| {
                VmError::HostError(format!("io_close scope retirement failed: {error}"))
            })?;
        if progress != CloseProgress::Ready {
            return Err(VmError::HostError(
                "io_close scope retirement is still pending".to_string(),
            ));
        }
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
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let canonical_parent = tokio::fs::canonicalize(parent)
        .await
        .map_err(|error| VmError::HostError(format!("io path resolution failed: {error}")))?;
    let name = path
        .file_name()
        .ok_or_else(|| VmError::HostError("io path has no file name".to_string()))?;
    Ok(canonical_parent.join(name))
}

fn io_state_for_handle(vm: &mut Vm, handle: ResourceHandle) -> VmResult<Arc<IoResourceState>> {
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
    Ok(Arc::clone(&resource.state))
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

fn spawn_shell_command(shell_command: &str, mode: &str) -> VmResult<IoHandle> {
    let mut process = if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.arg("/C").arg(shell_command);
        command
    } else {
        let mut command = Command::new("sh");
        command.arg("-c").arg(shell_command);
        command
    };
    #[cfg(unix)]
    process.process_group(0);
    process.kill_on_drop(true);
    match mode {
        "r" => {
            process.stdout(Stdio::piped()).stdin(Stdio::null());
        }
        "w" => {
            process.stdin(Stdio::piped()).stdout(Stdio::null());
        }
        _ => unreachable!("mode validated above"),
    }
    let mut child = process
        .spawn()
        .map_err(|error| VmError::HostError(format!("io_popen failed: {error}")))?;
    if mode == "r" {
        let Some(stdout) = child.stdout.take() else {
            terminate_process_id(child.id().unwrap_or(0));
            let _ = child.start_kill();
            return Err(VmError::HostError(
                "io_popen('r') did not provide stdout pipe".to_string(),
            ));
        };
        Ok(IoHandle::PopenRead {
            child,
            stdout: BufReader::new(stdout),
        })
    } else {
        let Some(stdin) = child.stdin.take() else {
            terminate_process_id(child.id().unwrap_or(0));
            let _ = child.start_kill();
            return Err(VmError::HostError(
                "io_popen('w') did not provide stdin pipe".to_string(),
            ));
        };
        Ok(IoHandle::PopenWrite { child, stdin })
    }
}
