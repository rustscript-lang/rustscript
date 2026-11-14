//! Tokio-backed IO hosts for builds with the `async` feature.
//!
//! Each call is an ordinary annotated async function. Open file and process
//! handles remain typed execution-scope resources because they span guest
//! calls; transient reads, writes, flushes, and closes rely on the generic
//! submitted-future lifecycle.
//!
//! Process resources borrow the embedding's Tokio runtime as a runtime lease:
//! the runtime that spawns them must remain alive and actively driven until an
//! explicit close or reset reaches quiescence. Abrupt Drop only sends immediate
//! best-effort kill signals; it never waits and provides no eventual-reap
//! guarantee.

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
use crate::{GuestOption, GuestResult, ResourceOwned};

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
        start_close_io_handle_for_drop(self);
    }
}

type ProcessTreeCleanupFuture = Pin<Box<dyn Future<Output = io::Result<()>> + Send + 'static>>;
type PreparedProcessCleanup = (
    Option<ProcessTreeCleanupFuture>,
    bool,
    bool,
    Option<io::Error>,
);

#[cfg(windows)]
fn prepare_process_cleanup(
    child: &mut Child,
    _terminate_tree: fn(u32) -> io::Result<()>,
) -> PreparedProcessCleanup {
    let pid = child.id().unwrap_or(0);
    (
        Some(Box::pin(terminate_process_id(pid))),
        false,
        false,
        None,
    )
}

#[cfg(not(windows))]
fn prepare_process_cleanup(
    child: &mut Child,
    terminate_tree: fn(u32) -> io::Result<()>,
) -> PreparedProcessCleanup {
    (
        None,
        true,
        true,
        start_terminate_child_tree_with(child, terminate_tree).err(),
    )
}

/// The typed resource stored in the execution scope for one async IO handle.
struct IoResource {
    handle: Arc<Mutex<Option<IoHandle>>>,
    process_tree_terminator: fn(u32) -> io::Result<()>,
    drop_process_tree_signaler: fn(u32) -> io::Result<()>,
    process_tree_cleanup: Option<ProcessTreeCleanupFuture>,
    process_tree_cleanup_complete: bool,
    leader_kill_started: bool,
    deferred_process_cleanup_error: Option<io::Error>,
}

impl IoResource {
    fn new(handle: IoHandle) -> Self {
        Self {
            handle: Arc::new(Mutex::new(Some(handle))),
            process_tree_terminator: signal_process_tree_for_drop,
            drop_process_tree_signaler: signal_process_tree_for_drop,
            process_tree_cleanup: None,
            process_tree_cleanup_complete: false,
            leader_kill_started: false,
            deferred_process_cleanup_error: None,
        }
    }

    #[cfg(all(test, unix))]
    fn new_with_process_tree_terminator(
        handle: IoHandle,
        process_tree_terminator: fn(u32) -> io::Result<()>,
    ) -> Self {
        Self {
            handle: Arc::new(Mutex::new(Some(handle))),
            process_tree_terminator,
            drop_process_tree_signaler: signal_process_tree_for_drop,
            process_tree_cleanup: None,
            process_tree_cleanup_complete: false,
            leader_kill_started: false,
            deferred_process_cleanup_error: None,
        }
    }

    #[cfg(all(test, windows))]
    fn new_with_process_terminators(
        handle: IoHandle,
        process_tree_terminator: fn(u32) -> io::Result<()>,
        drop_process_tree_signaler: fn(u32) -> io::Result<()>,
    ) -> Self {
        Self {
            handle: Arc::new(Mutex::new(Some(handle))),
            process_tree_terminator,
            drop_process_tree_signaler,
            process_tree_cleanup: None,
            process_tree_cleanup_complete: false,
            leader_kill_started: false,
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
        let (progress, process_cleanup) = match slot.as_mut() {
            None => (CloseProgress::Ready, None),
            Some(IoHandle::File(_)) => {
                slot.take();
                (CloseProgress::Ready, None)
            }
            Some(IoHandle::PopenRead { child, .. }) | Some(IoHandle::PopenWrite { child, .. }) => (
                CloseProgress::Pending,
                Some(prepare_process_cleanup(child, process_tree_terminator)),
            ),
        };
        let (
            process_tree_cleanup,
            process_tree_cleanup_complete,
            leader_kill_started,
            cleanup_error,
        ) = process_cleanup.unwrap_or((None, true, true, None));
        self.process_tree_cleanup = process_tree_cleanup;
        self.process_tree_cleanup_complete = process_tree_cleanup_complete;
        self.leader_kill_started = leader_kill_started;
        self.deferred_process_cleanup_error = cleanup_error;
        Ok(progress)
    }

    fn begin_close_for_drop_nonblocking(&mut self) -> CloseProgress {
        let Some(slot) = Arc::get_mut(&mut self.handle).map(Mutex::get_mut) else {
            return CloseProgress::Pending;
        };
        let Some(handle) = slot.as_mut() else {
            return CloseProgress::Ready;
        };
        if matches!(handle, IoHandle::File(_)) {
            slot.take();
            return CloseProgress::Ready;
        }
        let _ = start_close_io_handle_for_drop_with(handle, self.drop_process_tree_signaler);
        CloseProgress::Pending
    }

    fn poll_process_close(&mut self, cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        let Self {
            handle,
            process_tree_cleanup,
            process_tree_cleanup_complete,
            leader_kill_started,
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
                if tokio::runtime::Handle::try_current().is_err() {
                    return Poll::Ready(Err(runtime_required_resource_error()));
                }
                if !*process_tree_cleanup_complete {
                    let cleanup = process_tree_cleanup
                        .as_mut()
                        .expect("pending process tree cleanup retains its future");
                    let tree_result = match cleanup.as_mut().poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(result) => result,
                    };
                    process_tree_cleanup.take();
                    *process_tree_cleanup_complete = true;
                    let leader_result = child.start_kill().or_else(ignore_already_exited);
                    *leader_kill_started = true;
                    *deferred_process_cleanup_error =
                        combine_process_cleanup_results(tree_result, leader_result).err();
                }
                debug_assert!(*leader_kill_started);
                let mut wait = Box::pin(child.wait());
                match wait.as_mut().poll(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(result) => Poll::Ready(result.map(|_| ())),
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
        let _ = self.begin_close_for_drop_nonblocking();
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
    fn resource_type_key() -> Option<crate::host_api::ResourceTypeKey> {
        Some(
            crate::host_api::ResourceTypeKey::new(super::IO_FILE_KEY)
                .expect("the io.file resource key is a valid static key"),
        )
    }

    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.begin_close_after_operations_quiesce()
    }

    fn begin_close_for_drop(
        &mut self,
        _reason: ResourceCloseReason,
    ) -> ResourceResult<CloseProgress> {
        Ok(self.begin_close_for_drop_nonblocking())
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

fn runtime_required_resource_error() -> ResourceError {
    ResourceError::new(
        ResourceErrorCode::RuntimeRequired,
        "io::resource",
        "process cleanup requires polling from a live Tokio runtime",
    )
    .with_retryable_disposition()
}

fn start_close_io_handle_for_drop(handle: &mut IoHandle) {
    let _ = start_close_io_handle_for_drop_with(handle, signal_process_tree_for_drop);
}

fn start_close_io_handle_for_drop_with(
    handle: &mut IoHandle,
    signal_tree: fn(u32) -> io::Result<()>,
) -> io::Result<()> {
    match handle {
        IoHandle::File(_) => Ok(()),
        IoHandle::PopenRead { child, .. } | IoHandle::PopenWrite { child, .. } => {
            start_terminate_child_tree_with(child, signal_tree)
        }
    }
}

/// Why one `io::close` did not reach completion.
///
/// The variants are classified where they are produced so the caller can keep
/// the two guest-visible channels apart: a cleanup failure is a guest `Err`,
/// while a broken runtime lease is a fatal `VmError`.
#[derive(Debug)]
enum CloseFailure {
    /// The handle could not be flushed, or its process tree could not be
    /// terminated: a recoverable guest error.
    Cleanup(String),
    /// Process cleanup was polled without a live Tokio runtime, so the
    /// embedding's runtime lease is broken: fatal.
    RuntimeRequired,
}

/// The outcome of a close whose handle was still present in the shared slot.
#[derive(Debug)]
enum SharedCloseOutcome {
    /// The handle was flushed/terminated and detached from the slot.
    Closed,
    /// Flush or process termination failed. The handle was detached so `Drop`
    /// still signals the process tree and the scope can retire, and the guest
    /// receives the message.
    CleanupFailed(String),
}

type CloseIoFuture<'a> = Pin<Box<dyn Future<Output = Result<(), CloseFailure>> + Send + 'a>>;

fn close_io_handle_future(handle: &mut IoHandle) -> CloseIoFuture<'_> {
    Box::pin(close_io_handle(handle))
}

/// Closes the handle owned by `shared`, if it still owns one.
///
/// An already-closed slot is fatal and mutates nothing: the resource has
/// already retired its handle, so there is nothing left to flush, signal, or
/// retire again. Only a flush or process-termination failure detaches the
/// handle and becomes the guest `Err` branch, mirroring the blocking backend's
/// order of "fatal empty slot first, then cleanup outcome".
async fn close_shared_io_handle_with(
    shared: Arc<Mutex<Option<IoHandle>>>,
    close: impl for<'a> FnOnce(&'a mut IoHandle) -> CloseIoFuture<'a>,
) -> VmResult<SharedCloseOutcome> {
    let mut slot = shared.lock().await;
    let handle = slot
        .as_mut()
        .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))?;
    match close(handle).await {
        Ok(()) => {
            slot.take();
            Ok(SharedCloseOutcome::Closed)
        }
        // The handle stays owned: the caller must not retire the scope.
        Err(CloseFailure::RuntimeRequired) => Err(VmError::HostError(
            "RuntimeRequired: io::close process cleanup must be polled from a live Tokio runtime"
                .to_string(),
        )),
        // The handle survives a failed flush or a process that will not
        // terminate. Detach it so the scope retirement can complete while
        // `Drop` still signals the process tree, and report the failure to the
        // guest instead of aborting the VM. The handle is gone afterwards, so a
        // retried close reports a closed slot instead of repeating the cleanup.
        Err(CloseFailure::Cleanup(message)) => {
            slot.take();
            Ok(SharedCloseOutcome::CleanupFailed(message))
        }
    }
}

async fn close_io_handle(handle: &mut IoHandle) -> Result<(), CloseFailure> {
    if matches!(
        handle,
        IoHandle::PopenRead { .. } | IoHandle::PopenWrite { .. }
    ) && tokio::runtime::Handle::try_current().is_err()
    {
        return Err(CloseFailure::RuntimeRequired);
    }
    match handle {
        IoHandle::File(file) => {
            file.get_mut().flush().await.map_err(|error| {
                CloseFailure::Cleanup(format!("io_close flush failed: {error}"))
            })?;
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

async fn terminate_child_tree(child: &mut Child) -> Result<(), CloseFailure> {
    let pid = child.id().unwrap_or(0);
    terminate_process_tree_and_leader(|| terminate_process_id(pid), || kill_and_reap_child(child))
        .await
        .map_err(|error| CloseFailure::Cleanup(format!("io_close popen terminate failed: {error}")))
}

async fn kill_and_reap_child(child: &mut Child) -> io::Result<()> {
    match child.kill().await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => child.wait().await.map(|_| ()),
        Err(error) => Err(error),
    }
}

async fn terminate_process_tree_and_leader<Tree, TreeFuture, Leader, LeaderFuture>(
    terminate_tree: Tree,
    terminate_leader: Leader,
) -> io::Result<()>
where
    Tree: FnOnce() -> TreeFuture,
    TreeFuture: Future<Output = io::Result<()>>,
    Leader: FnOnce() -> LeaderFuture,
    LeaderFuture: Future<Output = io::Result<()>>,
{
    let tree_result = terminate_tree().await;
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

fn start_terminate_child_tree_for_drop(child: &mut Child) -> io::Result<()> {
    start_terminate_child_tree_with(child, signal_process_tree_for_drop)
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

async fn terminate_process_id(pid: u32) -> io::Result<()> {
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
        run_taskkill_with(pid, |mut command| async move { command.status().await }).await
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        Ok(())
    }
}

fn signal_process_tree_for_drop(pid: u32) -> io::Result<()> {
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
    #[cfg(not(unix))]
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
async fn run_taskkill_with<Run, RunFuture>(pid: u32, run: Run) -> io::Result<()>
where
    Run: FnOnce(Command) -> RunFuture,
    RunFuture: Future<Output = io::Result<std::process::ExitStatus>>,
{
    let mut command = Command::new("taskkill");
    command.args(["/T", "/F", "/PID", &pid.to_string()]);
    let status = run(command).await?;
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
///
/// Only the `OpenOptions` failure is a guest error; an unsupported mode and a
/// policy denial stay fatal.
///
/// The async adapter returns the guest `Value` carrier instead of
/// `HostFutureOutput<GuestResult<ResourceOwned<IoResource>, String>>` because
/// the host SDK rejects owned resources crossing a future publication. The
/// declared contract is unchanged, and [`publish_open_handle`] reuses
/// `publish_owned`, so ownership, schema validation and rollback stay
/// identical to the blocking backend.
#[pd_host_function(name = "io::open", contract = super::io_open_contract)]
pub(crate) async fn builtin_io_open(
    #[pd_host_context] context: IoPolicyContext,
    path: String,
    mode: String,
) -> VmResult<HostFutureOutput<Value>> {
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
    let file = match options.open(path).await {
        Ok(file) => file,
        Err(error) => return Ok(io_error_output("io_open", error)),
    };
    let handle = IoHandle::File(BufReader::new(file));
    Ok(HostFutureOutput::complete(move |vm| {
        publish_open_handle(vm, handle, super::io_open_contract().return_type)
    }))
}

/// Starts a child process and returns a process-backed handle.
///
/// An unsupported mode and a disabled process capability stay fatal; a process
/// that cannot be started or wired up is a guest error.
#[pd_host_function(name = "io::popen", contract = super::io_popen_contract)]
pub(crate) async fn builtin_io_popen(
    #[pd_host_context] context: IoPolicyContext,
    command: String,
    mode: String,
) -> VmResult<HostFutureOutput<Value>> {
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
    let handle = match spawn_shell_command(&command, &mode) {
        Ok(handle) => handle,
        Err(message) => {
            return Ok(HostFutureOutput::returning(crate::result::carrier(
                false,
                Value::string(&message),
            )));
        }
    };
    Ok(HostFutureOutput::complete(move |vm| {
        publish_open_handle(vm, handle, super::io_popen_contract().return_type)
    }))
}

/// Publishes an opened handle as the `Ok` branch of `io.open` / `io.popen`.
///
/// `publish_owned` validates the assembled carrier against `schema`, inserts
/// the resource into the execution scope and rolls the insertion back when
/// assembly fails, so the async adapter keeps the blocking backend's ownership
/// guarantees despite crossing the future boundary with a plain `Value`.
fn publish_open_handle(
    vm: &mut Vm,
    handle: IoHandle,
    schema: crate::HostTypeSchema,
) -> VmResult<Value> {
    crate::result::publish_owned::<IoResource, _>(
        vm,
        ResourceOwned::new(IoResource::new(handle)),
        &schema,
        |token| Ok(crate::result::carrier(true, token)),
    )
}

/// The guest `Err` carrier of a recoverable IO failure.
fn io_error_output(operation: &str, error: io::Error) -> HostFutureOutput<Value> {
    HostFutureOutput::returning(crate::result::carrier(
        false,
        Value::string(super::io_failure(operation, error)),
    ))
}

/// Reads all remaining text from an I/O handle.
///
/// A read failure is a guest error; an unusable handle kind and the read limit
/// stay fatal.
#[pd_host_function(name = "io::read_all", contract = super::io_read_all_contract)]
pub(crate) async fn builtin_io_read_all(
    #[pd_host_context] context: IoHandleContext,
    _handle_id: i64,
) -> VmResult<HostFutureOutput<GuestResult<String, String>>> {
    let mut slot = context.handle.lock().await;
    let handle = slot
        .as_mut()
        .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))?;
    let mut out = String::new();
    let read = match handle {
        IoHandle::File(file) => file.read_to_string(&mut out).await,
        IoHandle::PopenRead { stdout, .. } => stdout.read_to_string(&mut out).await,
        IoHandle::PopenWrite { .. } => {
            return Err(VmError::HostError(
                "io_read_all requires a readable handle".to_string(),
            ));
        }
    };
    if let Err(error) = read {
        return Ok(HostFutureOutput::returning(GuestResult::Err(
            super::io_failure("io_read_all", error),
        )));
    }
    if context
        .max_read_bytes
        .is_some_and(|limit| out.len() > limit)
    {
        return Err(VmError::HostError(
            "io_read_all exceeded read limit".to_string(),
        ));
    }
    Ok(HostFutureOutput::returning(GuestResult::Ok(out)))
}

/// Reads a single line of text from an I/O handle.
///
/// End of file is `Ok(None)` and an empty line is `Ok(Some(""))`: the returned
/// line has its single trailing `\n` removed. A read failure is a guest error;
/// an unusable handle kind and the read limit stay fatal.
#[pd_host_function(name = "io::read_line", contract = super::io_read_line_contract)]
pub(crate) async fn builtin_io_read_line(
    #[pd_host_context] context: IoHandleContext,
    _handle_id: i64,
) -> VmResult<HostFutureOutput<GuestResult<GuestOption<String>, String>>> {
    let mut slot = context.handle.lock().await;
    let handle = slot
        .as_mut()
        .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))?;
    let mut line = String::new();
    let read = match handle {
        IoHandle::File(file) => file.read_line(&mut line).await,
        IoHandle::PopenRead { stdout, .. } => stdout.read_line(&mut line).await,
        IoHandle::PopenWrite { .. } => {
            return Err(VmError::HostError(
                "io_read_line requires a readable handle".to_string(),
            ));
        }
    };
    let read = match read {
        Ok(read) => read,
        Err(error) => {
            return Ok(HostFutureOutput::returning(GuestResult::Err(
                super::io_failure("io_read_line", error),
            )));
        }
    };
    if read == 0 {
        return Ok(HostFutureOutput::returning(GuestResult::Ok(
            GuestOption::None,
        )));
    }
    if line.ends_with('\n') {
        line.pop();
    }
    if context
        .max_read_bytes
        .is_some_and(|limit| line.len() > limit)
    {
        return Err(VmError::HostError(
            "io_read_line exceeded read limit".to_string(),
        ));
    }
    Ok(HostFutureOutput::returning(GuestResult::Ok(
        GuestOption::Some(line),
    )))
}

/// Writes text to an I/O handle.
///
/// A write failure is a guest error; an unusable handle kind and the write
/// limit stay fatal.
#[pd_host_function(name = "io::write", contract = super::io_write_contract)]
pub(crate) async fn builtin_io_write(
    #[pd_host_context] context: IoHandleContext,
    _handle_id: i64,
    text: String,
) -> VmResult<HostFutureOutput<GuestResult<i64, String>>> {
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
    };
    match written {
        Ok(written) => Ok(HostFutureOutput::returning(GuestResult::Ok(written as i64))),
        Err(error) => Ok(HostFutureOutput::returning(GuestResult::Err(
            super::io_failure("io_write", error),
        ))),
    }
}

/// Flushes buffered output for an I/O handle.
///
/// A flush failure is a guest error; an unusable handle kind stays fatal.
#[pd_host_function(name = "io::flush", contract = super::io_flush_contract)]
pub(crate) async fn builtin_io_flush(
    #[pd_host_context] context: IoHandleContext,
    _handle_id: i64,
) -> VmResult<HostFutureOutput<GuestResult<(), String>>> {
    let mut slot = context.handle.lock().await;
    let handle = slot
        .as_mut()
        .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))?;
    let flushed = match handle {
        IoHandle::File(file) => file.get_mut().flush().await,
        IoHandle::PopenWrite { stdin, .. } => stdin.flush().await,
        IoHandle::PopenRead { .. } => Ok(()),
    };
    match flushed {
        Ok(()) => Ok(HostFutureOutput::returning(GuestResult::Ok(()))),
        Err(error) => Ok(HostFutureOutput::returning(GuestResult::Err(
            super::io_failure("io_flush", error),
        ))),
    }
}

/// Closes an I/O handle.
///
/// A cleanup (flush or process termination) failure is a guest error and still
/// retires the scope resource; an already-closed handle and a pending scope
/// retirement stay fatal.
#[pd_host_function(name = "io::close", contract = super::io_close_contract)]
pub(crate) async fn builtin_io_close(
    #[pd_host_context] context: IoHandleContext,
    _handle_id: i64,
) -> VmResult<HostFutureOutput<GuestResult<(), String>>> {
    // A closed slot returns before any retirement: that handle is already gone,
    // so retiring the scope again would corrupt the accounting the fatal error
    // is reporting.
    let close_failure =
        match close_shared_io_handle_with(Arc::clone(&context.handle), close_io_handle_future)
            .await?
        {
            SharedCloseOutcome::Closed => None,
            SharedCloseOutcome::CleanupFailed(message) => Some(message),
        };
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
        Ok(match close_failure {
            Some(message) => GuestResult::Err(message),
            None => GuestResult::Ok(()),
        })
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

/// Starts the shell for a `popen` handle.
///
/// Failures are formatted for the guest error channel of [`builtin_io_popen`].
fn spawn_shell_command(shell_command: &str, mode: &str) -> Result<IoHandle, String> {
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
        .map_err(|error| super::io_failure("io_popen", error))?;
    if mode == "r" {
        let Some(stdout) = child.stdout.take() else {
            let cleanup = start_terminate_child_tree_for_drop(&mut child);
            return Err(match cleanup {
                Ok(()) => "io_popen('r') did not provide stdout pipe".to_string(),
                Err(error) => format!(
                    "io_popen('r') did not provide stdout pipe; process cleanup failed: {error}"
                ),
            });
        };
        Ok(IoHandle::PopenRead {
            child,
            stdout: BufReader::new(stdout),
        })
    } else {
        let Some(stdin) = child.stdin.take() else {
            let cleanup = start_terminate_child_tree_for_drop(&mut child);
            return Err(match cleanup {
                Ok(()) => "io_popen('w') did not provide stdin pipe".to_string(),
                Err(error) => format!(
                    "io_popen('w') did not provide stdin pipe; process cleanup failed: {error}"
                ),
            });
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

    #[cfg(windows)]
    static DROP_TREE_SIGNAL_CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    #[cfg(windows)]
    fn record_drop_tree_signal(_pid: u32) -> io::Result<()> {
        DROP_TREE_SIGNAL_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn file_resource() -> IoResource {
        let file = std::fs::File::open("Cargo.toml").expect("test fixture should exist");
        IoResource::new(IoHandle::File(BufReader::new(File::from_std(file))))
    }

    fn pending_close<'a>(
        _handle: &'a mut IoHandle,
        started: Arc<StdMutex<bool>>,
    ) -> Pin<Box<dyn Future<Output = Result<(), CloseFailure>> + Send + 'a>> {
        Box::pin(async move {
            *started.lock().expect("started lock") = true;
            pending().await
        })
    }

    /// Fails the close the way a real flush or process termination would.
    fn failing_close<'a>(
        _handle: &'a mut IoHandle,
        message: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<(), CloseFailure>> + Send + 'a>> {
        Box::pin(async move { Err(CloseFailure::Cleanup(message.to_string())) })
    }

    #[cfg(unix)]
    fn start_process_kill_then_pending<'a>(
        handle: &'a mut IoHandle,
        started: Arc<StdMutex<bool>>,
    ) -> Pin<Box<dyn Future<Output = Result<(), CloseFailure>> + Send + 'a>> {
        Box::pin(async move {
            match handle {
                IoHandle::PopenRead { child, .. } | IoHandle::PopenWrite { child, .. } => {
                    child.start_kill().map_err(|error| {
                        CloseFailure::Cleanup(format!("test process kill failed: {error}"))
                    })?;
                }
                IoHandle::File(_) => panic!("expected process handle"),
            }
            *started.lock().expect("started lock") = true;
            pending().await
        })
    }

    #[tokio::test]
    async fn closed_slot_close_is_fatal_and_touches_nothing() {
        let resource = file_resource();
        let shared = Arc::clone(&resource.handle);
        // The resource already retired its handle; the slot is empty.
        shared.lock().await.take();
        let ran = Arc::new(StdMutex::new(false));
        let close_ran = Arc::clone(&ran);

        let error = close_shared_io_handle_with(Arc::clone(&shared), move |handle| {
            *close_ran.lock().expect("ran lock") = true;
            failing_close(handle, "must not run")
        })
        .await
        .expect_err("a closed slot must stay fatal instead of becoming a guest error");

        assert!(
            matches!(&error, VmError::HostError(message) if message == "io handle is closed"),
            "unexpected error: {error:?}"
        );
        assert!(
            !*ran.lock().expect("ran lock"),
            "a closed slot must not reach the cleanup closure"
        );
        assert!(
            shared.lock().await.is_none(),
            "a closed slot must not take or restore a handle"
        );
    }

    /// Regression for the call site, not just the helper: a resource entry that
    /// is still registered while the handle it owns is already gone must stay a
    /// fatal `VmError` out of `builtin_io_close`'s body, before any retirement.
    ///
    /// `_impl` is the name `#[pd_host_function]` gives the authored body; the
    /// generated wrapper only captures the context and submits the future, so
    /// this drives the code that owns the `?`-before-retirement ordering.
    ///
    /// The state itself is not reachable from guest code in a correct build
    /// (only the pre-fix double-retirement left an entry behind with an empty
    /// slot), so the test installs it directly and asserts on the scope: no
    /// `close_resource`, no entry dropped, no scope teardown.
    #[tokio::test]
    async fn builtin_close_keeps_an_empty_slot_resource_registered_and_fatal() {
        let mut vm = Vm::new(crate::vm::Program::new(
            Vec::new(),
            vec![crate::vm::OpCode::Ret as u8],
        ));
        let resource = file_resource();
        let shared = Arc::clone(&resource.handle);
        let token = vm
            .execution_scope()
            .push_resource(resource)
            .expect("resource insert");
        let handle = token.handle();
        // The handle is gone; the entry and its accounting are not.
        shared.lock().await.take();

        let close = builtin_io_close_impl(
            IoHandleContext {
                resource: handle,
                handle: Arc::clone(&shared),
                max_read_bytes: None,
                max_write_bytes: None,
            },
            handle.raw() as i64,
        )
        .await;

        let error = match close {
            Err(error) => error,
            Ok(_) => {
                panic!("a closed slot must stay fatal at the call site, not become a guest error")
            }
        };

        assert!(
            matches!(&error, VmError::HostError(message) if message == "io handle is closed"),
            "unexpected error: {error:?}"
        );
        assert_eq!(
            vm.execution_scope().resources().len(),
            1,
            "the fatal close must not call close_resource on the scope"
        );
        assert!(
            vm.execution_scope()
                .resources()
                .get::<IoResource>(&token)
                .is_ok(),
            "the resource must still be registered, so its Drop never ran"
        );
        assert!(
            !vm.execution_scope().is_closing(),
            "the fatal close must not start scope teardown"
        );
        assert!(
            shared.lock().await.is_none(),
            "the fatal close must not take or restore a handle"
        );
    }

    /// `CloseFailure::RuntimeRequired` is the one failure that keeps the handle:
    /// the runtime lease broke before the close started, so nothing was flushed
    /// or signalled and the caller must not retire its scope either.
    #[cfg(target_os = "linux")]
    #[test]
    fn runtime_required_close_retains_the_process_handle() {
        // Popens need a reactor, so spawn under one and then drop out of it:
        // `Handle::try_current` is what decides the branch under test.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("spawn runtime should build");
        let resource = runtime.block_on(async {
            IoResource::new(spawn_shell_command("sleep 30", "r").expect("process should spawn"))
        });
        let shared = Arc::clone(&resource.handle);
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "the close must be polled outside a runtime to reach RuntimeRequired"
        );

        let mut vm = Vm::new(crate::vm::Program::new(
            Vec::new(),
            vec![crate::vm::OpCode::Ret as u8],
        ));
        let token = vm
            .execution_scope()
            .push_resource(resource)
            .expect("resource insert");
        let handle = token.handle();
        let mut close = Box::pin(builtin_io_close_impl(
            IoHandleContext {
                resource: handle,
                handle: Arc::clone(&shared),
                max_read_bytes: None,
                max_write_bytes: None,
            },
            handle.raw() as i64,
        ));
        let mut cx = Context::from_waker(Waker::noop());

        let error = match close.as_mut().poll(&mut cx) {
            Poll::Ready(Err(error)) => error,
            Poll::Ready(Ok(_)) => panic!("an off-runtime close must not report a guest outcome"),
            Poll::Pending => panic!("an off-runtime close must fail instead of waiting"),
        };
        assert!(
            matches!(&error, VmError::HostError(message) if message.contains("RuntimeRequired")),
            "unexpected error: {error:?}"
        );
        assert!(
            shared.blocking_lock().is_some(),
            "a broken runtime lease must leave the handle owned for a retry"
        );
        assert_eq!(
            vm.execution_scope().resources().len(),
            1,
            "the fatal close must not call close_resource on the scope"
        );

        // The retained handle is still closable once a runtime is driven again.
        drop(close);
        runtime
            .block_on(close_shared_io_handle_with(
                Arc::clone(&shared),
                close_io_handle_future,
            ))
            .expect("retry under a live runtime should close the retained process");
        assert!(
            shared.blocking_lock().is_none(),
            "the retried close must detach the handle"
        );
    }

    #[tokio::test]
    async fn cleanup_failure_detaches_the_handle_and_cannot_be_retried() {
        let resource = file_resource();
        let shared = Arc::clone(&resource.handle);

        let outcome = close_shared_io_handle_with(Arc::clone(&shared), |handle| {
            failing_close(handle, "io_close flush failed: fixture")
        })
        .await
        .expect("a flush failure is a guest error, not a VM fault");
        assert!(
            matches!(
                &outcome,
                SharedCloseOutcome::CleanupFailed(message)
                    if message == "io_close flush failed: fixture"
            ),
            "unexpected close outcome"
        );
        assert!(
            shared.lock().await.is_none(),
            "the failed handle must be detached so Drop signals and the scope can retire"
        );

        let error = close_shared_io_handle_with(Arc::clone(&shared), close_io_handle_future)
            .await
            .expect_err("a detached handle must not be closable twice");
        assert!(
            matches!(&error, VmError::HostError(message) if message == "io handle is closed"),
            "the retry must report the closed slot, not repeat the cleanup: {error:?}"
        );
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

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_process_close_retains_child_and_allows_retry_to_reap() {
        let resource =
            IoResource::new(spawn_shell_command("sleep 30", "r").expect("process should spawn"));
        let shared = Arc::clone(&resource.handle);
        let pid = {
            let slot = shared.lock().await;
            match slot.as_ref().expect("resource handle") {
                IoHandle::PopenRead { child, .. } => child.id().expect("child pid"),
                other => panic!("expected popen read handle, got {other:?}"),
            }
        };
        let started = Arc::new(StdMutex::new(false));
        let close_started = Arc::clone(&started);
        let mut cancelled = Box::pin(close_shared_io_handle_with(
            Arc::clone(&shared),
            move |handle| start_process_kill_then_pending(handle, close_started),
        ));
        let mut cx = Context::from_waker(Waker::noop());

        assert!(matches!(cancelled.as_mut().poll(&mut cx), Poll::Pending));
        assert!(*started.lock().expect("started lock"));
        drop(cancelled);
        assert!(
            shared.lock().await.is_some(),
            "cancelled close must preserve the child and its pipe"
        );

        close_shared_io_handle_with(Arc::clone(&shared), close_io_handle_future)
            .await
            .expect("retry should terminate and reap the retained child");
        assert!(shared.lock().await.is_none());
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "retry must reap the direct child"
        );
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "current_thread")]
    async fn dropping_process_resource_exercises_only_the_drop_signal_helper() {
        DROP_TREE_SIGNAL_CALLS.store(0, std::sync::atomic::Ordering::SeqCst);
        let resource = IoResource::new_with_process_terminators(
            spawn_shell_command("ping -n 30 127.0.0.1 >NUL", "r").expect("process should spawn"),
            |_| panic!("Drop must not invoke taskkill or explicit-close waiting"),
            record_drop_tree_signal,
        );

        drop(resource);

        assert_eq!(
            DROP_TREE_SIGNAL_CALLS.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "Drop must invoke the immediate signal helper"
        );
    }

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
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
            || async {
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

    #[tokio::test]
    async fn taskkill_launch_failure_is_an_error() {
        let launch = run_taskkill_with(42, |_| async {
            Err(io::Error::new(io::ErrorKind::NotFound, "taskkill missing"))
        })
        .await
        .expect_err("taskkill launch failure must propagate");
        assert_eq!(launch.kind(), io::ErrorKind::NotFound);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unsuccessful_taskkill_status_is_an_error() {
        let status = run_taskkill_with(42, |_| async {
            Command::new("sh").args(["-c", "exit 7"]).status().await
        })
        .await
        .expect_err("unsuccessful taskkill status must propagate");
        assert!(status.to_string().contains("status"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn unsuccessful_taskkill_status_is_an_error() {
        let status = run_taskkill_with(42, |_| async {
            Command::new("cmd").args(["/C", "exit", "7"]).status().await
        })
        .await
        .expect_err("unsuccessful taskkill status must propagate");
        assert!(status.to_string().contains("status"));
    }
}
