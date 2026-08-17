use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use pd_host_function::pd_host_function;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use super::super::resource::ResourceTypeId;
use super::super::{
    CancellationReason, CaptureAsyncHostContext, HostFutureOutput, HostOpId, ResourceHandle,
    RuntimeError, RuntimeErrorCode, Value, Vm, VmError, VmResult,
};
use super::{IoPolicy, io_policy};

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

struct IoResource {
    handle: Mutex<Option<IoHandle>>,
    process_id: AtomicU32,
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
            handle: Mutex::new(Some(handle)),
            process_id: AtomicU32::new(process_id),
        }
    }

    async fn take_handle(&self) -> VmResult<IoHandle> {
        self.handle
            .lock()
            .await
            .take()
            .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))
    }

    fn close(&self, reason: CancellationReason) -> VmResult<()> {
        if let Ok(mut handle) = self.handle.try_lock()
            && let Some(handle) = handle.take()
        {
            start_close_io_handle(handle, reason)?;
        }
        terminate_process_id(self.process_id.load(Ordering::Acquire), reason)?;
        self.process_id.store(0, Ordering::Release);
        Ok(())
    }
}

impl Drop for IoResource {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.get_mut().take() {
            let _ = start_close_io_handle(handle, CancellationReason::VmReset);
        }
        let _ = terminate_process_id(
            self.process_id.load(Ordering::Acquire),
            CancellationReason::VmReset,
        );
    }
}

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

pub(crate) struct IoHandleContext {
    handle: ResourceHandle,
    resource: Arc<IoResource>,
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
        let handle = resource_handle(handle_id)?;
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
            options.append(true).create(true);
        }
        "r+" => {
            options.read(true).write(true);
        }
        "w+" => {
            options.read(true).write(true).create(true).truncate(true);
        }
        "a+" => {
            options.read(true).append(true).create(true);
        }
        _ => unreachable!(),
    }
    let file = options
        .open(path)
        .await
        .map_err(|error| VmError::HostError(format!("io_open failed: {error}")))?;
    let handle = IoHandle::File(BufReader::new(file));
    Ok(HostFutureOutput::complete(move |vm| {
        let handle = insert_io_resource(vm, handle)?;
        match handle.as_value() {
            Value::Int(value) => Ok(value),
            _ => unreachable!(),
        }
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
        let handle = insert_io_resource(vm, handle)?;
        match handle.as_value() {
            Value::Int(value) => Ok(value),
            _ => unreachable!(),
        }
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
    let resource = context.resource;
    let handle = context.handle;
    let resource_handle = resource.take_handle().await?;
    let close_result = close_io_handle(resource_handle, CancellationReason::ResourceClosed).await;
    Ok(HostFutureOutput::complete(move |vm| {
        super::super::close_runtime_resource(vm, handle, CancellationReason::ResourceClosed)
            .map_err(runtime_host_error)?;
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
    _reason: CancellationReason,
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
    let parent = path
        .parent()
        .ok_or_else(|| VmError::HostError(format!("io path '{}' has no parent", path.display())))?;
    let file_name = path.file_name().ok_or_else(|| {
        VmError::HostError(format!("io path '{}' has no file name", path.display()))
    })?;
    tokio::fs::canonicalize(parent)
        .await
        .map(|parent| parent.join(file_name))
        .map_err(|error| VmError::HostError(format!("io path resolution failed: {error}")))
}

fn spawn_shell_command(command: &str, mode: &str) -> VmResult<IoHandle> {
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
    process.as_std_mut().process_group(0);
    process.kill_on_drop(true);
    match mode {
        "r" => {
            process.stdout(Stdio::piped()).stdin(Stdio::null());
        }
        "w" => {
            process.stdin(Stdio::piped()).stdout(Stdio::null());
        }
        _ => {}
    }
    let mut child = process
        .spawn()
        .map_err(|error| VmError::HostError(format!("io_popen failed: {error}")))?;
    match mode {
        "r" => {
            let stdout = child.stdout.take().ok_or_else(|| {
                VmError::HostError("io_popen failed to capture stdout".to_string())
            })?;
            Ok(IoHandle::PopenRead {
                child,
                stdout: BufReader::new(stdout),
            })
        }
        "w" => {
            let stdin = child.stdin.take().ok_or_else(|| {
                VmError::HostError("io_popen failed to capture stdin".to_string())
            })?;
            Ok(IoHandle::PopenWrite { child, stdin })
        }
        _ => unreachable!(),
    }
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

async fn close_io_handle(mut handle: IoHandle, reason: CancellationReason) -> VmResult<()> {
    match &mut handle {
        IoHandle::File(file) => {
            file.get_mut()
                .flush()
                .await
                .map_err(|error| VmError::HostError(format!("io close failed: {error}")))?;
        }
        IoHandle::PopenRead { child, .. } => wait_for_child(child, reason).await?,
        IoHandle::PopenWrite { child, stdin } => {
            stdin
                .shutdown()
                .await
                .map_err(|error| VmError::HostError(format!("io close failed: {error}")))?;
            wait_for_child(child, reason).await?;
        }
    }
    Ok(())
}

async fn wait_for_child(child: &mut Child, reason: CancellationReason) -> VmResult<()> {
    if !matches!(reason, CancellationReason::ResourceClosed) {
        let _ = child.start_kill();
    }
    match tokio::time::timeout(Duration::from_secs(1), child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(VmError::HostError(format!(
            "io process cleanup failed: {error}"
        ))),
        Err(_) => {
            let _ = child.start_kill();
            child
                .wait()
                .await
                .map(|_| ())
                .map_err(|error| VmError::HostError(format!("io process cleanup failed: {error}")))
        }
    }
}

fn start_close_io_handle(mut handle: IoHandle, _reason: CancellationReason) -> VmResult<()> {
    match &mut handle {
        IoHandle::File(_) => {}
        IoHandle::PopenRead { child, .. } | IoHandle::PopenWrite { child, .. } => {
            child.start_kill().map_err(|error| {
                VmError::HostError(format!("io process cleanup failed: {error}"))
            })?;
        }
    }
    Ok(())
}

fn terminate_process_id(process_id: u32, reason: CancellationReason) -> VmResult<()> {
    if process_id == 0 || matches!(reason, CancellationReason::ResourceClosed) {
        return Ok(());
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(-(process_id as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = process_id;
    }
    Ok(())
}
