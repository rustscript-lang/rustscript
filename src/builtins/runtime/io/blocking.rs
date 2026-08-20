use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[cfg(unix)]
use libc;

use pd_host_function::pd_host_function;

use super::super::HostCallResult;
use crate::host_api::ResourceTypeKey;
use crate::vm::resource::{
    CloseProgress, HostResource, ResourceCloseReason, ResourceHandle, ResourceResult,
};
use crate::vm::{Value, Vm, VmError, VmResult};

// ---- HostResource implementations for IO resources ----

/// A file handle stored as a concrete HostResource.
pub(crate) struct IoFileResource {
    handle: Mutex<Option<std::fs::File>>,
    closed: AtomicBool,
}

impl HostResource for IoFileResource {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(super::io_file_key())
    }

    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(mut file) = self.handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = file.flush();
        }
        Ok(CloseProgress::Ready)
    }

    fn poll_close(&mut self, _cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        Poll::Ready(Ok(()))
    }
}

impl IoFileResource {
    fn new(file: std::fs::File) -> Self {
        Self {
            handle: Mutex::new(Some(file)),
            closed: AtomicBool::new(false),
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
    process_id: u32,
    closed: AtomicBool,
}

impl HostResource for IoProcessResource {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(super::io_process_key())
    }

    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.closed.store(true, Ordering::SeqCst);
        let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut child) = guard.take() {
            // Kill the process group (which includes the child and its descendants)
            // since we set process_group(0) on spawn.
            terminate_process_group(self.process_id);
            let _ = child.kill();
            let _ = child.wait();
        }
        Ok(CloseProgress::Ready)
    }

    fn poll_close(&mut self, _cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        Poll::Ready(Ok(()))
    }
}

impl IoProcessResource {
    fn new(child: std::process::Child) -> Self {
        let process_id = child.id();
        Self {
            child: Mutex::new(Some(child)),
            process_id,
            closed: AtomicBool::new(false),
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

// ---- IO builtin functions ----

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
    let mut options = OpenOptions::new();
    match mode {
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
            return Err(VmError::HostError(format!(
                "unsupported io_open mode '{other}', expected r/w/a/r+/w+/a+"
            )));
        }
    }
    let file = options
        .open(path)
        .map_err(|err| VmError::HostError(format!("io_open failed: {err}")))?;
    let resource = IoFileResource::new(file);
    let handle = insert_io_file_resource(vm, resource)?;
    Ok(HostCallResult::Return(handle))
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
    if let Some(policy) = io_policy(vm) {
        if !policy.allow_process {
            return Err(VmError::HostError(
                "io_popen requires the process capability".to_string(),
            ));
        }
    }
    let mut child = spawn_shell_command(command, mode)?;
    match mode {
        "r" => {
            let stdout = child.stdout.take().ok_or_else(|| {
                VmError::HostError("io_popen('r') did not provide stdout pipe".to_string())
            })?;
            let process_resource = IoProcessResource::new(child);
            let process_token = insert_io_process_resource(vm, process_resource)?;
            let pipe_resource = IoPipeResource::new_read(stdout);
            let pipe_token = insert_io_pipe_child_resource(vm, pipe_resource, &process_token)?;
            let handle = pipe_token.handle().as_value();
            match handle {
                Value::Int(value) => Ok(HostCallResult::Return(value)),
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
            let pipe_token = insert_io_pipe_child_resource(vm, pipe_resource, &process_token)?;
            let handle = pipe_token.handle().as_value();
            match handle {
                Value::Int(value) => Ok(HostCallResult::Return(value)),
                _ => unreachable!(),
            }
        }
        _ => unreachable!("mode validated above"),
    }
}

/// Reads all remaining text from an I/O handle.
#[pd_host_function(name = "io::read_all")]
pub(crate) fn builtin_io_read_all(vm: &mut Vm, handle_id: i64) -> VmResult<HostCallResult<String>> {
    let max_read_bytes = io_policy(vm).map(|policy| policy.max_read_bytes);
    let handle = resource_handle(handle_id)?;
    let mut ctx = vm.host_context();
    let token = ctx.typed_resource::<IoFileResource>(handle);
    let out = if let Ok(token) = token {
        let mut resource = ctx
            .resource_mut(&token)
            .map_err(|error| VmError::HostError(format!("io handle lookup failed: {error}")))?;
        let mut out = String::new();
        resource
            .get()
            .with_handle_mut(|f| read_to_string_with_limit(f, max_read_bytes, &mut out))?;
        out
    } else {
        let token = ctx
            .typed_resource::<IoPipeResource>(handle)
            .map_err(|error| VmError::HostError(format!("io handle lookup failed: {error}")))?;
        let mut resource = ctx
            .resource_mut(&token)
            .map_err(|error| VmError::HostError(format!("io handle lookup failed: {error}")))?;
        let mut out = String::new();
        resource
            .get()
            .with_reader(|r| read_to_string_with_limit(r, max_read_bytes, &mut out))?;
        out
    };
    Ok(HostCallResult::Return(out))
}

/// Reads a single line of text from an I/O handle.
#[pd_host_function(name = "io::read_line")]
pub(crate) fn builtin_io_read_line(
    vm: &mut Vm,
    handle_id: i64,
) -> VmResult<HostCallResult<String>> {
    let max_read_bytes = io_policy(vm).map(|policy| policy.max_read_bytes);
    let handle = resource_handle(handle_id)?;
    let line = with_file_or_pipe_mut(
        vm,
        handle,
        |file| file.with_handle_mut(|f| read_line_from_reader(f, max_read_bytes)),
        |pipe| pipe.with_reader(|r| read_line_from_reader(r, max_read_bytes)),
    )?;
    Ok(HostCallResult::Return(line))
}

/// Writes text to an I/O handle.
#[pd_host_function(name = "io::write")]
pub(crate) fn builtin_io_write(
    vm: &mut Vm,
    handle_id: i64,
    text: &str,
) -> VmResult<HostCallResult<i64>> {
    if let Some(policy) = io_policy(vm) {
        if text.len() > policy.max_write_bytes {
            return Err(VmError::HostError(format!(
                "io_write exceeds the configured write limit of {} bytes",
                policy.max_write_bytes
            )));
        }
    }
    let bytes = text.as_bytes().to_vec();
    let handle = resource_handle(handle_id)?;
    let written = with_file_or_pipe_mut(
        vm,
        handle,
        |file| {
            file.with_handle_mut(|f| {
                f.write(&bytes)
                    .map_err(|err| VmError::HostError(format!("io_write failed: {err}")))
                    .map(|n| n as i64)
            })
        },
        |pipe| {
            pipe.with_writer(|w| {
                w.write(&bytes)
                    .map_err(|err| VmError::HostError(format!("io_write failed: {err}")))
                    .map(|n| n as i64)
            })
        },
    )?;
    Ok(HostCallResult::Return(written))
}

/// Flushes buffered output for an I/O handle.
#[pd_host_function(name = "io::flush")]
pub(crate) fn builtin_io_flush(vm: &mut Vm, handle_id: i64) -> VmResult<HostCallResult<bool>> {
    let handle = resource_handle(handle_id)?;
    let mut ctx = vm.host_context();
    let token = ctx.typed_resource::<IoFileResource>(handle);
    if let Ok(token) = token {
        let mut resource = ctx
            .resource_mut(&token)
            .map_err(|error| VmError::HostError(format!("io handle lookup failed: {error}")))?;
        resource.get().with_handle_mut(|f| {
            f.flush()
                .map_err(|err| VmError::HostError(format!("io_flush failed: {err}")))?;
            Ok(true)
        })?;
    } else {
        // For pipe resources, flush is a no-op for read handles.
        let token = ctx
            .typed_resource::<IoPipeResource>(handle)
            .map_err(|error| VmError::HostError(format!("io handle lookup failed: {error}")))?;
        let mut resource = ctx
            .resource_mut(&token)
            .map_err(|error| VmError::HostError(format!("io handle lookup failed: {error}")))?;
        let _ = resource.get().with_writer(|w| {
            w.flush()
                .map_err(|err| VmError::HostError(format!("io_flush failed: {err}")))?;
            Ok(true)
        });
    }
    Ok(HostCallResult::Return(true))
}

/// Closes an I/O handle.
#[pd_host_function(name = "io::close")]
pub(crate) fn builtin_io_close(vm: &mut Vm, handle_id: i64) -> VmResult<HostCallResult<bool>> {
    let handle = resource_handle(handle_id)?;
    // Try file resource first, then pipe resource (popen returns a pipe handle).
    let mut ctx = vm.host_context();
    let result = ctx.close_resource::<IoFileResource>(handle, ResourceCloseReason::Requested);
    match result {
        Ok(_) => Ok(HostCallResult::Return(true)),
        Err(ref error) if error.message().contains("resource_type_mismatch") => {
            ctx.close_resource::<IoPipeResource>(handle, ResourceCloseReason::Requested)
                .map_err(|error| VmError::HostError(format!("io_close failed: {error}")))?;
            Ok(HostCallResult::Return(true))
        }
        Err(error) => Err(VmError::HostError(format!("io_close failed: {error}"))),
    }
}

/// Returns whether a file system path exists.
#[pd_host_function(name = "io::exists")]
pub(crate) fn builtin_io_exists(vm: &mut Vm, path: &str) -> VmResult<HostCallResult<bool>> {
    let path = authorize_io_path(vm, path, false)?;
    Ok(HostCallResult::Return(path.exists()))
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

// ---- Policy helpers ----

fn io_policy(vm: &Vm) -> Option<super::IoPolicy> {
    vm.host
        .get_module_state::<super::IoHostState>()
        .map(|state| state.policy().clone())
        .or_else(|| {
            (!vm.host.default_builtin_capabilities_enabled()).then(super::IoPolicy::default)
        })
}

// ---- Path helpers ----

fn authorize_io_path(vm: &Vm, path: &str, writes: bool) -> VmResult<PathBuf> {
    let requested = PathBuf::from(path);
    let Some(policy) = io_policy(vm) else {
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
