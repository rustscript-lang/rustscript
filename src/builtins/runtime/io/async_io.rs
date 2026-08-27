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

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll, Waker};

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

type CloseFuture = Pin<Box<dyn Future<Output = VmResult<()>> + Send + 'static>>;

/// The typed resource stored in the execution scope for one async IO handle.
///
/// Mirrors the blocking path: the handle lives behind an `Arc<Mutex<...>>`
/// so the async builtin can take/restore it while the resource stays in the
/// scope table. Closing is exact-once.
struct IoResource {
    handle: Arc<Mutex<Option<IoHandle>>>,
    closed: Arc<AtomicBool>,
    process_id: Arc<AtomicU32>,
    active_operations: Arc<AtomicUsize>,
    close_waker: Arc<StdMutex<Option<Waker>>>,
    close_scheduled: Arc<AtomicBool>,
    close_future: Option<CloseFuture>,
    owner: bool,
    owner_alive: Arc<AtomicBool>,
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
            active_operations: Arc::new(AtomicUsize::new(0)),
            close_waker: Arc::new(StdMutex::new(None)),
            close_scheduled: Arc::new(AtomicBool::new(false)),
            close_future: None,
            owner: true,
            owner_alive: Arc::new(AtomicBool::new(true)),
        }
    }

    fn new_shared(cells: &IoResource) -> Self {
        Self {
            handle: Arc::clone(&cells.handle),
            closed: Arc::clone(&cells.closed),
            process_id: Arc::clone(&cells.process_id),
            active_operations: Arc::clone(&cells.active_operations),
            close_waker: Arc::clone(&cells.close_waker),
            close_scheduled: Arc::clone(&cells.close_scheduled),
            close_future: None,
            owner: false,
            owner_alive: Arc::clone(&cells.owner_alive),
        }
    }

    fn begin_operation(&self, operation: &'static str) -> VmResult<IoOperationLease> {
        if self.closed.load(Ordering::Acquire) {
            return Err(VmError::HostError(format!("{operation} handle is closed")));
        }
        self.active_operations.fetch_add(1, Ordering::AcqRel);
        if self.closed.load(Ordering::Acquire) {
            self.active_operations.fetch_sub(1, Ordering::AcqRel);
            wake_close_waker(&self.close_waker);
            return Err(VmError::HostError(format!("{operation} handle is closed")));
        }
        Ok(IoOperationLease {
            active_operations: Arc::clone(&self.active_operations),
            close_waker: Arc::clone(&self.close_waker),
            handle: Arc::clone(&self.handle),
            closed: Arc::clone(&self.closed),
            owner_alive: Arc::clone(&self.owner_alive),
            close_scheduled: Arc::clone(&self.close_scheduled),
            process_id: Arc::clone(&self.process_id),
            completed: false,
        })
    }

    fn schedule_close(&mut self, reason: ResourceCloseReason) {
        if self.close_future.is_some() {
            return;
        }
        self.close_scheduled.store(true, Ordering::Release);
        let handle = Arc::clone(&self.handle);
        let process_id = Arc::clone(&self.process_id);
        self.close_future = Some(Box::pin(async move {
            let handle = handle.lock().await.take();
            let result = match handle {
                Some(handle) => close_io_handle(handle, reason).await,
                None => Ok(()),
            };
            if result.is_ok() {
                process_id.store(0, Ordering::Release);
            }
            result
        }));
    }

    fn wait_for_operations(&self, cx: &Context<'_>) -> bool {
        if self.active_operations.load(Ordering::Acquire) == 0 {
            return false;
        }
        let mut wake = None;
        let pending = {
            let mut slot = self
                .close_waker
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if self.active_operations.load(Ordering::Acquire) == 0 {
                false
            } else {
                *slot = Some(cx.waker().clone());
                if self.active_operations.load(Ordering::Acquire) == 0 {
                    wake = slot.take();
                    false
                } else {
                    true
                }
            }
        };
        if let Some(waker) = wake {
            waker.wake();
        }
        pending
    }

    fn take_handle(&self) -> impl Future<Output = VmResult<IoHandle>> + Send + 'static {
        let handle = Arc::clone(&self.handle);
        async move {
            handle
                .lock()
                .await
                .take()
                .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))
        }
    }
}

struct IoOperationLease {
    active_operations: Arc<AtomicUsize>,
    close_waker: Arc<StdMutex<Option<Waker>>>,
    handle: Arc<Mutex<Option<IoHandle>>>,
    closed: Arc<AtomicBool>,
    owner_alive: Arc<AtomicBool>,
    close_scheduled: Arc<AtomicBool>,
    process_id: Arc<AtomicU32>,
    completed: bool,
}

impl Drop for IoOperationLease {
    fn drop(&mut self) {
        if !self.completed {
            self.closed.store(true, Ordering::Release);
            terminate_process_id(
                self.process_id.load(Ordering::Acquire),
                ResourceCloseReason::ResourceClosed,
            );
            if !self.close_scheduled.load(Ordering::Acquire) {
                self.process_id.store(0, Ordering::Release);
            }
        }
        let previous = self.active_operations.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "IO operation lease count underflowed");
        if previous != 1 {
            return;
        }
        if self.closed.load(Ordering::Acquire)
            && (!self.owner_alive.load(Ordering::Acquire)
                || !self.close_scheduled.load(Ordering::Acquire))
            && let Ok(mut guard) = self.handle.try_lock()
        {
            drop(guard.take());
        }
        wake_close_waker(&self.close_waker);
    }
}

impl IoOperationLease {
    fn complete(&mut self) {
        self.completed = true;
    }
}

fn wake_close_waker(close_waker: &StdMutex<Option<Waker>>) {
    if let Some(waker) = close_waker
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        waker.wake();
    }
}

impl Drop for IoHandle {
    fn drop(&mut self) {
        match self {
            Self::PopenRead { child, .. } | Self::PopenWrite { child, .. } => {
                reap_child_now(child, ResourceCloseReason::VmDrop);
            }
            Self::File(_) => {}
        }
    }
}

fn reap_child_now(child: &mut Child, reason: ResourceCloseReason) {
    let Some(pid) = child.id() else {
        return;
    };
    terminate_process_id(pid, reason);
    let _ = child.start_kill();
    for _ in 0..200 {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(1)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => return,
        }
    }
}

impl Drop for IoResource {
    fn drop(&mut self) {
        if !self.owner {
            return;
        }
        self.owner_alive.store(false, Ordering::Release);
        self.closed.store(true, Ordering::Release);
        let pid = self.process_id.load(Ordering::Acquire);
        terminate_process_id(pid, ResourceCloseReason::VmDrop);
        if let Ok(mut guard) = self.handle.try_lock() {
            drop(guard.take());
        }
        self.close_waker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }
}

impl HostResource for IoResource {
    fn begin_close(&mut self, reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.closed.store(true, Ordering::Release);
        let pid = self.process_id.load(Ordering::Acquire);
        if pid != 0 {
            terminate_process_id(pid, reason);
        }
        self.schedule_close(reason);
        if self.active_operations.load(Ordering::Acquire) != 0 {
            return Ok(CloseProgress::Pending);
        }
        match self.handle.try_lock() {
            Ok(guard) if guard.is_none() => {
                self.close_future = None;
                self.process_id.store(0, Ordering::Release);
                Ok(CloseProgress::Ready)
            }
            Ok(_) | Err(_) => Ok(CloseProgress::Pending),
        }
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        if self.wait_for_operations(cx) {
            return Poll::Pending;
        }
        let Some(close_future) = self.close_future.as_mut() else {
            return Poll::Ready(Ok(()));
        };
        match close_future.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(())) => {
                self.close_future = None;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => {
                self.close_future = None;
                Poll::Ready(Err(ResourceError::new(
                    ResourceErrorCode::ResourceCleanupFailed,
                    "io::resource",
                    error.to_string(),
                )))
            }
        }
    }
}

async fn close_io_handle(mut handle: IoHandle, reason: ResourceCloseReason) -> VmResult<()> {
    match &mut handle {
        IoHandle::File(file) => {
            file.get_mut()
                .flush()
                .await
                .map_err(|error| VmError::HostError(format!("io_close flush failed: {error}")))?;
        }
        IoHandle::PopenRead { child, .. } => {
            terminate_process_id(child.id().unwrap_or(0), reason);
            child.kill().await.map_err(|error| {
                VmError::HostError(format!("io_close popen wait failed: {error}"))
            })?;
        }
        IoHandle::PopenWrite { child, stdin } => {
            let _ = stdin.shutdown().await;
            terminate_process_id(child.id().unwrap_or(0), reason);
            child.kill().await.map_err(|error| {
                VmError::HostError(format!("io_close popen wait failed: {error}"))
            })?;
        }
    }
    Ok(())
}

fn terminate_process_id(pid: u32, reason: ResourceCloseReason) {
    if pid == 0 {
        return;
    }
    let _ = reason;
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return;
        };
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
    let mut lease = context.resource.begin_operation("io_read_all")?;
    let mut guard = context.resource.handle.lock().await;
    if context.resource.closed.load(Ordering::Acquire) {
        return Err(VmError::HostError(
            "io_read_all handle is closed".to_string(),
        ));
    }
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
    if context.resource.closed.load(Ordering::Acquire) {
        return Err(VmError::HostError(
            "io_read_all handle is closed".to_string(),
        ));
    }
    if context
        .max_read_bytes
        .is_some_and(|limit| out.len() > limit)
    {
        return Err(VmError::HostError(
            "io_read_all exceeded read limit".to_string(),
        ));
    }
    lease.complete();
    Ok(HostFutureOutput::returning(out))
}

/// Reads a single line of text from an I/O handle.
#[pd_host_function(name = "io::read_line")]
pub(crate) async fn builtin_io_read_line(
    #[pd_host_context] context: IoHandleContext,
    _handle_id: i64,
) -> VmResult<HostFutureOutput<String>> {
    let mut lease = context.resource.begin_operation("io_read_line")?;
    let mut guard = context.resource.handle.lock().await;
    if context.resource.closed.load(Ordering::Acquire) {
        return Err(VmError::HostError(
            "io_read_line handle is closed".to_string(),
        ));
    }
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
    if context.resource.closed.load(Ordering::Acquire) {
        return Err(VmError::HostError(
            "io_read_line handle is closed".to_string(),
        ));
    }
    if context
        .max_read_bytes
        .is_some_and(|limit| line.len() > limit)
    {
        return Err(VmError::HostError(
            "io_read_line exceeded read limit".to_string(),
        ));
    }
    lease.complete();
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
    let mut lease = context.resource.begin_operation("io_write")?;
    let mut guard = context.resource.handle.lock().await;
    if context.resource.closed.load(Ordering::Acquire) {
        return Err(VmError::HostError("io_write handle is closed".to_string()));
    }
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
    if context.resource.closed.load(Ordering::Acquire) {
        return Err(VmError::HostError("io_write handle is closed".to_string()));
    }
    lease.complete();
    Ok(HostFutureOutput::returning(written as i64))
}

/// Flushes buffered output for an I/O handle.
#[pd_host_function(name = "io::flush")]
pub(crate) async fn builtin_io_flush(
    #[pd_host_context] context: IoHandleContext,
    _handle_id: i64,
) -> VmResult<HostFutureOutput<bool>> {
    let mut lease = context.resource.begin_operation("io_flush")?;
    let mut guard = context.resource.handle.lock().await;
    if context.resource.closed.load(Ordering::Acquire) {
        return Err(VmError::HostError("io_flush handle is closed".to_string()));
    }
    let handle = guard
        .as_mut()
        .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))?;
    match handle {
        IoHandle::File(file) => file.get_mut().flush().await,
        IoHandle::PopenWrite { stdin, .. } => stdin.flush().await,
        IoHandle::PopenRead { .. } => Ok(()),
    }
    .map_err(|error| VmError::HostError(format!("io_flush failed: {error}")))?;
    if context.resource.closed.load(Ordering::Acquire) {
        return Err(VmError::HostError("io_flush handle is closed".to_string()));
    }
    lease.complete();
    Ok(HostFutureOutput::returning(true))
}

/// Closes an I/O handle.
#[pd_host_function(name = "io::close")]
pub(crate) async fn builtin_io_close(
    #[pd_host_context] context: IoHandleContext,
    _handle_id: i64,
) -> VmResult<HostFutureOutput<bool>> {
    let mut lease = context.resource.begin_operation("io_close")?;
    let resource = IoResource::new_shared(&context.resource);
    let handle = context.handle;
    let owned_handle = resource.take_handle().await?;
    let close_result = close_io_handle(owned_handle, ResourceCloseReason::Requested).await;
    if close_result.is_ok() {
        context.resource.process_id.store(0, Ordering::Release);
    }
    lease.complete();
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
        _ => {}
    }

    let mut child = process
        .spawn()
        .map_err(|error| VmError::HostError(format!("io_popen spawn failed: {error}")))?;

    if mode == "r" {
        let Some(stdout) = child.stdout.take() else {
            terminate_process_id(child.id().unwrap_or(0), ResourceCloseReason::VmDrop);
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
            terminate_process_id(child.id().unwrap_or(0), ResourceCloseReason::VmDrop);
            let _ = child.start_kill();
            return Err(VmError::HostError(
                "io_popen('w') did not provide stdin pipe".to_string(),
            ));
        };
        Ok(IoHandle::PopenWrite { child, stdin })
    }
}

#[cfg(test)]
mod tests {
    use std::task::{Context, Poll, Waker};

    use super::*;
    use crate::return_one;

    fn file_resource() -> IoResource {
        let file = std::fs::File::open("Cargo.toml").expect("test fixture should exist");
        IoResource::new(IoHandle::File(BufReader::new(File::from_std(file))))
    }

    async fn assert_close_waits_for_busy_handle_lock() {
        let mut resource = file_resource();
        let handle = Arc::clone(&resource.handle);
        let guard = handle.lock().await;
        let lease = resource
            .begin_operation("test")
            .expect("test operation should start");
        let reason = ResourceCloseReason::Requested;

        assert_eq!(
            resource.begin_close(reason).expect("close should start"),
            CloseProgress::Pending,
            "close must stay pending while an async operation owns the handle lock"
        );

        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(resource.poll_close(&mut cx), Poll::Pending));

        drop(guard);
        drop(lease);
        assert!(matches!(resource.poll_close(&mut cx), Poll::Ready(Ok(()))));
    }

    #[tokio::test]
    async fn async_io_close_while_read_lock_is_busy_stays_pending() {
        assert_close_waits_for_busy_handle_lock().await;
    }

    #[tokio::test]
    async fn async_io_close_while_write_lock_is_busy_stays_pending() {
        assert_close_waits_for_busy_handle_lock().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn async_io_child_close_polls_until_child_is_reaped() {
        let mut resource = IoResource::new(spawn_shell_command("sleep 30", "r").expect("spawn"));
        let pid = resource.process_id.load(Ordering::Acquire);
        assert_ne!(pid, 0);

        assert_eq!(
            resource
                .begin_close(ResourceCloseReason::Requested)
                .expect("close should start"),
            CloseProgress::Pending
        );

        std::future::poll_fn(|cx| resource.poll_close(cx))
            .await
            .expect("child close should succeed");
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "poll_close must wait for the child to be reaped"
        );
    }

    #[tokio::test]
    async fn async_io_close_propagates_scope_retirement_errors() {
        let compiled = crate::compile_source("0;").expect("test program should compile");
        let mut vm = Vm::new(compiled.program);
        let resource = IoResource::new(spawn_shell_command("sleep 30", "r").expect("spawn"));
        let shared = IoResource::new_shared(&resource);
        let token = vm
            .execution_scope()
            .push_resource(resource)
            .expect("resource should insert");
        let context = IoHandleContext {
            handle: token.handle(),
            resource: shared,
            max_read_bytes: None,
            max_write_bytes: None,
        };
        let mut close_future =
            Box::pin(builtin_io_close_impl(context, token.handle().raw() as i64));
        let mut cx = Context::from_waker(Waker::noop());

        assert!(matches!(close_future.as_mut().poll(&mut cx), Poll::Pending));
        assert_eq!(
            vm.execution_scope()
                .close_resource::<IoResource>(token.handle(), ResourceCloseReason::Requested)
                .expect("concurrent close should start"),
            CloseProgress::Pending
        );

        let output = close_future
            .await
            .expect("close future should complete")
            .map(return_one);
        let error = output
            .finish(&mut vm)
            .expect_err("scope retirement failure must reach the guest");
        assert!(
            error.to_string().contains("already closed")
                || error.to_string().contains("closing")
                || error.to_string().contains("resource"),
            "unexpected scope retirement error: {error}"
        );
    }
}
