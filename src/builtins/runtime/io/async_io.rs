//! Tokio-backed IO hosts for builds with the `async` feature.
//!
//! Each call is an ordinary annotated async function. Open file and process
//! handles remain typed execution-scope resources because they span guest
//! calls; transient reads, writes, flushes, and closes rely on the generic
//! submitted-future lifecycle.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
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
                let _ = start_terminate_child_tree(child);
            }
            Self::File(_) => {}
        }
    }
}

/// The typed resource stored in the execution scope for one async IO handle.
struct IoResource {
    handle: Arc<Mutex<Option<IoHandle>>>,
    process_tree_terminator: fn(u32) -> io::Result<()>,
    deferred_process_cleanup_error: Option<io::Error>,
}

impl IoResource {
    fn new(handle: IoHandle) -> Self {
        Self {
            handle: Arc::new(Mutex::new(Some(handle))),
            process_tree_terminator: terminate_process_id,
            deferred_process_cleanup_error: None,
        }
    }

    #[cfg(test)]
    fn new_with_process_tree_terminator(
        handle: IoHandle,
        process_tree_terminator: fn(u32) -> io::Result<()>,
    ) -> Self {
        Self {
            handle: Arc::new(Mutex::new(Some(handle))),
            process_tree_terminator,
            deferred_process_cleanup_error: None,
        }
    }

    fn exclusive_handle_slot(&mut self) -> ResourceResult<&mut Option<IoHandle>> {
        Arc::get_mut(&mut self.handle)
            .map(Mutex::get_mut)
            .ok_or_else(|| {
                ResourceError::new(
                    ResourceErrorCode::ResourceCleanupFailed,
                    "io::resource",
                    "async IO handle remained borrowed after host operations quiesced",
                )
            })
    }

    fn begin_close_after_operations_quiesce(&mut self) -> ResourceResult<CloseProgress> {
        let process_tree_terminator = self.process_tree_terminator;
        let slot = self.exclusive_handle_slot()?;
        let (progress, cleanup_error) = match slot.as_mut() {
            None => (CloseProgress::Ready, None),
            Some(IoHandle::File(_)) => {
                slot.take();
                (CloseProgress::Ready, None)
            }
            Some(IoHandle::PopenRead { child, .. }) | Some(IoHandle::PopenWrite { child, .. }) => {
                let cleanup_error =
                    start_terminate_child_tree_with(child, process_tree_terminator).err();
                (CloseProgress::Pending, cleanup_error)
            }
        };
        self.deferred_process_cleanup_error = cleanup_error;
        Ok(progress)
    }

    fn poll_process_close(&mut self, cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        let Self {
            handle,
            deferred_process_cleanup_error,
            ..
        } = self;
        let slot = match Arc::get_mut(handle).map(Mutex::get_mut) {
            Some(slot) => slot,
            None => {
                return Poll::Ready(Err(ResourceError::new(
                    ResourceErrorCode::ResourceCleanupFailed,
                    "io::resource",
                    "async IO handle remained borrowed after host operations quiesced",
                )));
            }
        };
        let poll = match slot.as_mut() {
            None | Some(IoHandle::File(_)) => Poll::Ready(Ok(())),
            Some(IoHandle::PopenRead { child, .. }) | Some(IoHandle::PopenWrite { child, .. }) => {
                match child.try_wait() {
                    Ok(Some(_)) => Poll::Ready(Ok(())),
                    Err(error) => Poll::Ready(Err(error)),
                    Ok(None) if tokio::runtime::Handle::try_current().is_err() => Poll::Pending,
                    Ok(None) => {
                        let mut wait = Box::pin(child.wait());
                        match wait.as_mut().poll(cx) {
                            Poll::Pending => Poll::Pending,
                            Poll::Ready(result) => Poll::Ready(result.map(|_| ())),
                        }
                    }
                }
            }
        };
        match poll {
            Poll::Pending => Poll::Pending,
            Poll::Ready(leader_result) => {
                slot.take();
                let prior_result = match deferred_process_cleanup_error.take() {
                    Some(error) => Err(error),
                    None => Ok(()),
                };
                Poll::Ready(
                    combine_process_cleanup_results(prior_result, leader_result)
                        .map_err(process_resource_error),
                )
            }
        }
    }
}

impl Drop for IoResource {
    fn drop(&mut self) {
        if let Some(mutex) = Arc::get_mut(&mut self.handle)
            && let Some(mut handle) = mutex.get_mut().take()
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
        self.begin_close_after_operations_quiesce()
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        self.poll_process_close(cx)
    }
}

fn process_resource_error(error: io::Error) -> ResourceError {
    ResourceError::new(
        ResourceErrorCode::ResourceCleanupFailed,
        "io::resource",
        format!("io_close popen terminate failed: {error}"),
    )
}

fn start_close_io_handle(handle: &mut IoHandle) -> ResourceResult<()> {
    match handle {
        IoHandle::File(_) => Ok(()),
        IoHandle::PopenRead { child, .. } | IoHandle::PopenWrite { child, .. } => {
            start_terminate_child_tree(child).map_err(process_resource_error)
        }
    }
}

type CloseIoFuture<'a> = Pin<Box<dyn Future<Output = VmResult<()>> + Send + 'a>>;

fn close_io_handle_future(handle: &mut IoHandle) -> CloseIoFuture<'_> {
    Box::pin(close_io_handle(handle))
}

async fn close_shared_io_handle_with(
    shared: Arc<Mutex<Option<IoHandle>>>,
    close: impl for<'a> FnOnce(&'a mut IoHandle) -> CloseIoFuture<'a>,
) -> VmResult<()> {
    let mut slot = shared.lock().await;
    let handle = slot
        .as_mut()
        .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))?;
    close(handle).await?;
    slot.take();
    Ok(())
}

async fn close_io_handle(handle: &mut IoHandle) -> VmResult<()> {
    match handle {
        IoHandle::File(file) => {
            file.get_mut()
                .flush()
                .await
                .map_err(|error| VmError::HostError(format!("io_close flush failed: {error}")))?;
        }
        IoHandle::PopenRead { child, .. } => {
            terminate_child_tree(child).await?;
        }
        IoHandle::PopenWrite { child, stdin } => {
            let _ = stdin.shutdown().await;
            terminate_child_tree(child).await?;
        }
    }
    Ok(())
}

async fn terminate_child_tree(child: &mut Child) -> VmResult<()> {
    let pid = child.id().unwrap_or(0);
    terminate_process_tree_and_leader(|| terminate_process_id(pid), || kill_and_reap_child(child))
        .await
        .map_err(|error| VmError::HostError(format!("io_close popen terminate failed: {error}")))
}

async fn kill_and_reap_child(child: &mut Child) -> io::Result<()> {
    match child.kill().await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => child.wait().await.map(|_| ()),
        Err(error) => Err(error),
    }
}

async fn terminate_process_tree_and_leader<Tree, Leader, LeaderFuture>(
    terminate_tree: Tree,
    terminate_leader: Leader,
) -> io::Result<()>
where
    Tree: FnOnce() -> io::Result<()>,
    Leader: FnOnce() -> LeaderFuture,
    LeaderFuture: Future<Output = io::Result<()>>,
{
    let tree_result = terminate_tree();
    let leader_result = terminate_leader().await;
    combine_process_cleanup_results(tree_result, leader_result)
}

fn combine_process_cleanup_results(
    tree_result: io::Result<()>,
    leader_result: io::Result<()>,
) -> io::Result<()> {
    match (tree_result, leader_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(tree_error), Ok(())) => Err(tree_error),
        (Ok(()), Err(leader_error)) => Err(leader_error),
        (Err(tree_error), Err(leader_error)) => Err(io::Error::new(
            tree_error.kind(),
            format!(
                "process-tree termination failed: {tree_error}; direct leader cleanup also failed: {leader_error}"
            ),
        )),
    }
}

fn start_terminate_child_tree(child: &mut Child) -> io::Result<()> {
    start_terminate_child_tree_with(child, terminate_process_id)
}

fn start_terminate_child_tree_with(
    child: &mut Child,
    terminate_tree: impl FnOnce(u32) -> io::Result<()>,
) -> io::Result<()> {
    let pid = child.id().unwrap_or(0);
    let tree_result = terminate_tree(pid);
    let leader_result = child.start_kill().or_else(ignore_already_exited);
    combine_process_cleanup_results(tree_result, leader_result)
}

fn ignore_already_exited(error: io::Error) -> io::Result<()> {
    if error.kind() == io::ErrorKind::InvalidInput {
        Ok(())
    } else {
        Err(error)
    }
}

fn terminate_process_id(pid: u32) -> io::Result<()> {
    if pid == 0 {
        return Ok(());
    }
    #[cfg(unix)]
    {
        terminate_unix_process_group_with(
            pid,
            |process_group, signal| unsafe { libc::kill(process_group, signal) },
            io::Error::last_os_error,
        )
    }
    #[cfg(windows)]
    {
        run_taskkill_with(pid, std::process::Command::status)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        Ok(())
    }
}

#[cfg(unix)]
fn terminate_unix_process_group_with<Kill, LastError>(
    pid: u32,
    kill: Kill,
    last_error: LastError,
) -> io::Result<()>
where
    Kill: FnOnce(libc::pid_t, libc::c_int) -> libc::c_int,
    LastError: FnOnce() -> io::Error,
{
    let pid = libc::pid_t::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "process id exceeds pid_t"))?;
    if kill(-pid, libc::SIGKILL) == 0 {
        return Ok(());
    }
    let error = last_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(any(windows, test))]
fn run_taskkill_with<Run>(pid: u32, run: Run) -> io::Result<()>
where
    Run: FnOnce(&mut std::process::Command) -> io::Result<std::process::ExitStatus>,
{
    let mut command = std::process::Command::new("taskkill");
    command.args(["/T", "/F", "/PID", &pid.to_string()]);
    let status = run(&mut command)?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "taskkill exited with status {status}"
        )))
    }
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

/// Shared handle ownership and byte limits captured before an async call starts.
pub(crate) struct IoHandleContext {
    resource: ResourceHandle,
    handle: Arc<Mutex<Option<IoHandle>>>,
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
        let resource = io_parse_handle(handle_id)?;
        let handle = io_handle_for_resource(vm, resource)?;
        Ok(Self {
            resource,
            handle,
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
    let mut slot = context.handle.lock().await;
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
    let mut slot = context.handle.lock().await;
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
    let mut slot = context.handle.lock().await;
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
    let mut slot = context.handle.lock().await;
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
    close_shared_io_handle_with(Arc::clone(&context.handle), close_io_handle_future).await?;
    let resource = context.resource;
    Ok(HostFutureOutput::complete(move |vm| {
        let progress = vm
            .execution_scope()
            .close_resource::<IoResource>(resource, ResourceCloseReason::Requested)
            .map_err(|error| {
                VmError::HostError(format!("io_close scope retirement failed: {error}"))
            })?;
        if progress != CloseProgress::Ready {
            return Err(VmError::HostError(
                "io_close scope retirement is still pending".to_string(),
            ));
        }
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

fn io_handle_for_resource(
    vm: &mut Vm,
    handle: ResourceHandle,
) -> VmResult<Arc<Mutex<Option<IoHandle>>>> {
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
    Ok(Arc::clone(&resource.handle))
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
            let cleanup = start_terminate_child_tree(&mut child);
            return Err(VmError::HostError(match cleanup {
                Ok(()) => "io_popen('r') did not provide stdout pipe".to_string(),
                Err(error) => format!(
                    "io_popen('r') did not provide stdout pipe; process cleanup failed: {error}"
                ),
            }));
        };
        Ok(IoHandle::PopenRead {
            child,
            stdout: BufReader::new(stdout),
        })
    } else {
        let Some(stdin) = child.stdin.take() else {
            let cleanup = start_terminate_child_tree(&mut child);
            return Err(VmError::HostError(match cleanup {
                Ok(()) => "io_popen('w') did not provide stdin pipe".to_string(),
                Err(error) => format!(
                    "io_popen('w') did not provide stdin pipe; process cleanup failed: {error}"
                ),
            }));
        };
        Ok(IoHandle::PopenWrite { child, stdin })
    }
}

#[cfg(test)]
mod tests {
    use std::future::{Future, pending};
    use std::io;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::task::{Context, Poll, Waker};

    use super::*;

    fn file_resource() -> IoResource {
        let file = std::fs::File::open("Cargo.toml").expect("test fixture should exist");
        IoResource::new(IoHandle::File(BufReader::new(File::from_std(file))))
    }

    fn pending_close<'a>(
        _handle: &'a mut IoHandle,
        started: Arc<StdMutex<bool>>,
    ) -> Pin<Box<dyn Future<Output = VmResult<()>> + Send + 'a>> {
        Box::pin(async move {
            *started.lock().expect("started lock") = true;
            pending().await
        })
    }

    #[tokio::test]
    async fn cancelling_explicit_close_retains_the_handle_in_the_resource() {
        let resource = file_resource();
        let shared = Arc::clone(&resource.handle);
        let started = Arc::new(StdMutex::new(false));
        let close_started = Arc::clone(&started);
        let mut future = Box::pin(close_shared_io_handle_with(
            Arc::clone(&shared),
            move |handle| pending_close(handle, close_started),
        ));
        let mut cx = Context::from_waker(Waker::noop());

        assert!(matches!(future.as_mut().poll(&mut cx), Poll::Pending));
        assert!(*started.lock().expect("started lock"));
        drop(future);

        assert!(
            shared.lock().await.is_some(),
            "cancelling io::close must leave the real handle owned by the resource"
        );
    }

    #[tokio::test]
    async fn process_resource_close_polls_until_the_leader_is_reaped() {
        let mut resource =
            IoResource::new(spawn_shell_command("sleep 30", "r").expect("process should spawn"));
        let pid = {
            let slot = resource.handle.lock().await;
            match slot.as_ref().expect("resource handle") {
                IoHandle::PopenRead { child, .. } => child.id().expect("child pid"),
                other => panic!("expected popen read handle, got {other:?}"),
            }
        };

        assert_eq!(
            resource
                .begin_close(ResourceCloseReason::VmReset)
                .expect("close should start"),
            CloseProgress::Pending
        );
        std::future::poll_fn(|cx| resource.poll_close(cx))
            .await
            .expect("leader cleanup should complete");
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "resource close must reap the direct child"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resource_shutdown_reaps_leader_before_reporting_tree_failure() {
        let handle = spawn_shell_command("sleep 30", "r").expect("process should spawn");
        let pid = match &handle {
            IoHandle::PopenRead { child, .. } => child.id().expect("child pid"),
            other => panic!("expected popen read handle, got {other:?}"),
        };
        let resource = IoResource::new_with_process_tree_terminator(handle, |_| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected tree failure",
            ))
        });
        let mut resources = crate::vm::resource::ResourceTable::new().expect("resource table");
        resources.push(resource).expect("resource insert");

        let error =
            std::future::poll_fn(|cx| resources.poll_close_all(ResourceCloseReason::VmReset, cx))
                .await
                .expect_err("tree failure must propagate after shutdown");

        assert!(
            resources.is_empty(),
            "resource must be reclaimed after reap"
        );
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "resource shutdown must reap the direct child before returning the tree error"
        );
        assert_eq!(error.code(), ResourceErrorCode::ResourceCleanupFailed);
        assert!(error.to_string().contains("injected tree failure"));
    }

    #[tokio::test]
    async fn process_tree_failure_still_attempts_direct_async_leader_cleanup() {
        let leader_attempted = Arc::new(StdMutex::new(false));
        let attempted = Arc::clone(&leader_attempted);
        let error = terminate_process_tree_and_leader(
            || {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "tree denied",
                ))
            },
            move || async move {
                *attempted.lock().expect("attempt lock") = true;
                Ok(())
            },
        )
        .await
        .expect_err("tree failure must propagate");

        assert!(*leader_attempted.lock().expect("attempt lock"));
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("tree denied"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_process_group_signal_treats_esrch_as_success() {
        terminate_unix_process_group_with(
            42,
            |_, _| -1,
            || io::Error::from_raw_os_error(libc::ESRCH),
        )
        .expect("an already absent process group is successfully terminated");

        let error = terminate_unix_process_group_with(
            42,
            |_, _| -1,
            || io::Error::from_raw_os_error(libc::EPERM),
        )
        .expect_err("other process group failures must propagate");
        assert_eq!(error.raw_os_error(), Some(libc::EPERM));
    }

    #[test]
    fn taskkill_launch_failure_is_an_error() {
        let launch = run_taskkill_with(42, |_| {
            Err(io::Error::new(io::ErrorKind::NotFound, "taskkill missing"))
        })
        .expect_err("taskkill launch failure must propagate");
        assert_eq!(launch.kind(), io::ErrorKind::NotFound);
    }

    #[cfg(unix)]
    #[test]
    fn unsuccessful_taskkill_status_is_an_error() {
        let status = run_taskkill_with(42, |_| {
            std::process::Command::new("sh")
                .args(["-c", "exit 7"])
                .status()
        })
        .expect_err("unsuccessful taskkill status must propagate");
        assert!(status.to_string().contains("status"));
    }

    #[cfg(windows)]
    #[test]
    fn unsuccessful_taskkill_status_is_an_error() {
        let status = run_taskkill_with(42, |_| {
            std::process::Command::new("cmd")
                .args(["/C", "exit", "7"])
                .status()
        })
        .expect_err("unsuccessful taskkill status must propagate");
        assert!(status.to_string().contains("status"));
    }
}
