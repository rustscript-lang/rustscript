use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use pd_host_function::pd_host_function;

use crate::vm::resource::close::{CloseProgress, HostResource};
use crate::vm::resource::error::{ResourceError, ResourceErrorCode, ResourceResult};
use crate::vm::resource::{Resource, ResourceCloseReason, ResourceHandle};
use crate::vm::{Vm, VmError, VmResult};

/// A file / child-process backed IO handle.
pub(super) enum IoHandle {
    File(std::fs::File),
    PopenRead { child: Child },
    PopenWrite { child: Child },
}

/// The typed resource stored in the execution scope for one IO handle.
struct IoResource {
    handle: Option<IoHandle>,
}

impl IoResource {
    fn new(handle: IoHandle) -> Self {
        Self {
            handle: Some(handle),
        }
    }
}

impl Drop for IoResource {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = close_io_handle(handle);
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
        if let Some(handle) = self.handle.take() {
            close_io_handle(handle).map_err(|error| {
                ResourceError::new(
                    ResourceErrorCode::ResourceCleanupFailed,
                    "io::resource",
                    error.to_string(),
                )
            })?;
        }
        Ok(CloseProgress::Ready)
    }
}

/// Opens a file handle for runtime I/O inline on non-async builds.
#[pd_host_function(name = "io::open", contract = super::io_open_contract)]
pub(super) fn builtin_io_open(vm: &mut Vm, path: &str, mode: &str) -> VmResult<i64> {
    let writes = match mode {
        "r" => false,
        "w" | "a" | "r+" | "w+" | "a+" => true,
        other => {
            return Err(VmError::HostError(format!(
                "unsupported io_open mode '{other}', expected r/w/a/r+/w+/a+"
            )));
        }
    };
    let path = authorize_blocking_io_path(vm, path, writes)?;
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
        _ => unreachable!("mode validated above"),
    }
    let file = options
        .open(path)
        .map_err(|error| VmError::HostError(format!("io_open failed: {error}")))?;
    let token = vm
        .execution_scope()
        .push_resource(IoResource::new(IoHandle::File(file)))
        .map_err(|error| VmError::HostError(format!("io resource insert failed: {error}")))?;
    Ok(token.into_handle().raw() as i64)
}

/// Starts a child process and returns a process-backed handle.
#[pd_host_function(name = "io::popen")]
pub(super) fn builtin_io_popen(vm: &mut Vm, command: &str, mode: &str) -> VmResult<i64> {
    if mode != "r" && mode != "w" {
        return Err(VmError::HostError(format!(
            "unsupported io_popen mode '{mode}', expected r or w"
        )));
    }
    if super::io_policy(vm)
        .as_ref()
        .is_some_and(|policy| !policy.allow_process)
    {
        return Err(VmError::HostError(
            "io_popen requires the process capability".to_string(),
        ));
    }
    let handle = spawn_shell_command(command, mode)?;
    let token = vm
        .execution_scope()
        .push_resource(IoResource::new(handle))
        .map_err(|error| VmError::HostError(format!("io resource insert failed: {error}")))?;
    Ok(token.into_handle().raw() as i64)
}

/// Reads all remaining text from an I/O handle.
#[pd_host_function(name = "io::read_all", contract = super::io_read_all_contract)]
pub(super) fn builtin_io_read_all(vm: &mut Vm, handle_id: i64) -> VmResult<String> {
    let limit = super::io_policy(vm).map(|policy| policy.max_read_bytes);
    let token = io_resource_for_handle(vm, handle_id)?;
    let mut resource = vm
        .execution_scope()
        .resources_mut()
        .get_mut(&token)
        .map_err(|error| io_borrow_error(handle_id, error))?;
    let handle = resource
        .handle
        .as_mut()
        .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))?;
    let mut out = String::new();
    match handle {
        IoHandle::File(file) => file.read_to_string(&mut out),
        IoHandle::PopenRead { child } => child
            .stdout
            .as_mut()
            .ok_or_else(|| {
                VmError::HostError("io_read_all popen handle missing stdout".to_string())
            })?
            .read_to_string(&mut out),
        IoHandle::PopenWrite { .. } => {
            return Err(VmError::HostError(
                "io_read_all requires a readable handle".to_string(),
            ));
        }
    }
    .map_err(|error| VmError::HostError(format!("io_read_all failed: {error}")))?;
    if limit.is_some_and(|limit| out.len() > limit) {
        return Err(VmError::HostError(
            "io_read_all exceeded read limit".to_string(),
        ));
    }
    Ok(out)
}

/// Reads a single line of text from an I/O handle.
#[pd_host_function(name = "io::read_line")]
pub(super) fn builtin_io_read_line(vm: &mut Vm, handle_id: i64) -> VmResult<String> {
    let limit = super::io_policy(vm).map(|policy| policy.max_read_bytes);
    let token = io_resource_for_handle(vm, handle_id)?;
    let mut resource = vm
        .execution_scope()
        .resources_mut()
        .get_mut(&token)
        .map_err(|error| io_borrow_error(handle_id, error))?;
    let handle = resource
        .handle
        .as_mut()
        .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))?;
    let line = match handle {
        IoHandle::File(file) => read_line_from_reader(file)?,
        IoHandle::PopenRead { child } => {
            read_line_from_reader(child.stdout.as_mut().ok_or_else(|| {
                VmError::HostError("io_read_line popen handle missing stdout".to_string())
            })?)?
        }
        IoHandle::PopenWrite { .. } => {
            return Err(VmError::HostError(
                "io_read_line requires a readable handle".to_string(),
            ));
        }
    };
    if limit.is_some_and(|limit| line.len() > limit) {
        return Err(VmError::HostError(
            "io_read_line exceeded read limit".to_string(),
        ));
    }
    Ok(line)
}

/// Writes text to an I/O handle.
#[pd_host_function(name = "io::write")]
pub(super) fn builtin_io_write(vm: &mut Vm, handle_id: i64, text: &str) -> VmResult<i64> {
    if super::io_policy(vm)
        .as_ref()
        .is_some_and(|policy| text.len() > policy.max_write_bytes)
    {
        return Err(VmError::HostError(
            "io_write exceeded write limit".to_string(),
        ));
    }
    let token = io_resource_for_handle(vm, handle_id)?;
    let mut resource = vm
        .execution_scope()
        .resources_mut()
        .get_mut(&token)
        .map_err(|error| io_borrow_error(handle_id, error))?;
    let handle = resource
        .handle
        .as_mut()
        .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))?;
    let written = match handle {
        IoHandle::File(file) => file.write(text.as_bytes()),
        IoHandle::PopenWrite { child } => child
            .stdin
            .as_mut()
            .ok_or_else(|| VmError::HostError("io_write popen handle missing stdin".to_string()))?
            .write(text.as_bytes()),
        IoHandle::PopenRead { .. } => {
            return Err(VmError::HostError(
                "io_write requires a writable handle".to_string(),
            ));
        }
    }
    .map_err(|error| VmError::HostError(format!("io_write failed: {error}")))?;
    Ok(written as i64)
}

/// Flushes buffered output for an I/O handle.
#[pd_host_function(name = "io::flush")]
pub(super) fn builtin_io_flush(vm: &mut Vm, handle_id: i64) -> VmResult<bool> {
    let token = io_resource_for_handle(vm, handle_id)?;
    let mut resource = vm
        .execution_scope()
        .resources_mut()
        .get_mut(&token)
        .map_err(|error| io_borrow_error(handle_id, error))?;
    let handle = resource
        .handle
        .as_mut()
        .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))?;
    match handle {
        IoHandle::File(file) => file.flush(),
        IoHandle::PopenWrite { child } => child
            .stdin
            .as_mut()
            .ok_or_else(|| VmError::HostError("io_flush popen handle missing stdin".to_string()))?
            .flush(),
        IoHandle::PopenRead { .. } => Ok(()),
    }
    .map_err(|error| VmError::HostError(format!("io_flush failed: {error}")))?;
    Ok(true)
}

/// Closes an I/O handle.
#[pd_host_function(name = "io::close", contract = super::io_close_contract)]
pub(super) fn builtin_io_close(vm: &mut Vm, handle_id: i64) -> VmResult<bool> {
    let token = io_resource_for_handle(vm, handle_id)?;
    let handle = token.handle();
    let owned = {
        let mut resource = vm
            .execution_scope()
            .resources_mut()
            .get_mut(&token)
            .map_err(|error| io_borrow_error(handle_id, error))?;
        resource
            .handle
            .take()
            .ok_or_else(|| VmError::HostError("io handle is closed".to_string()))?
    };
    let close_result = close_io_handle(owned);
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
}

/// Returns whether a file system path exists.
#[pd_host_function(name = "io::exists")]
pub(super) fn builtin_io_exists(vm: &mut Vm, path: &str) -> VmResult<bool> {
    Ok(authorize_blocking_io_path(vm, path, false)?.exists())
}

fn io_resource_for_handle(vm: &mut Vm, handle_id: i64) -> VmResult<Resource<IoResource>> {
    let handle = io_parse_handle(handle_id)?;
    vm.execution_scope()
        .resources()
        .typed::<IoResource>(handle)
        .map_err(|error| {
            VmError::HostError(format!(
                "io handle {handle_id} is not a live IO handle: {error}"
            ))
        })
}

fn io_borrow_error(handle_id: i64, error: impl std::fmt::Display) -> VmError {
    VmError::HostError(format!("io handle {handle_id} borrow failed: {error}"))
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

fn authorize_blocking_io_path(vm: &Vm, path: &str, writes: bool) -> VmResult<PathBuf> {
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
    let canonical = canonicalize_blocking_target(&absolute)?;
    for root in &policy.allowed_roots {
        let root = std::fs::canonicalize(Path::new(root)).map_err(|error| {
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

fn canonicalize_blocking_target(path: &Path) -> VmResult<PathBuf> {
    if path.exists() {
        return std::fs::canonicalize(path)
            .map_err(|error| VmError::HostError(format!("io path resolution failed: {error}")));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let canonical_parent = std::fs::canonicalize(parent)
        .map_err(|error| VmError::HostError(format!("io path resolution failed: {error}")))?;
    let name = path
        .file_name()
        .ok_or_else(|| VmError::HostError("io path has no file name".to_string()))?;
    Ok(canonical_parent.join(name))
}

fn spawn_shell_command(command: &str, mode: &str) -> VmResult<IoHandle> {
    let mut process = if cfg!(windows) {
        let mut shell = Command::new("cmd");
        shell.arg("/C").arg(command);
        shell
    } else {
        let mut shell = Command::new("sh");
        shell.arg("-c").arg(command);
        shell
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        process.process_group(0);
    }
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
    match mode {
        "r" if child.stdout.is_some() => Ok(IoHandle::PopenRead { child }),
        "w" if child.stdin.is_some() => Ok(IoHandle::PopenWrite { child }),
        "r" => {
            let cleanup = terminate_child_tree(&mut child);
            Err(VmError::HostError(match cleanup {
                Ok(()) => "io_popen('r') did not provide stdout pipe".to_string(),
                Err(error) => format!(
                    "io_popen('r') did not provide stdout pipe; process cleanup failed: {error}"
                ),
            }))
        }
        "w" => {
            let cleanup = terminate_child_tree(&mut child);
            Err(VmError::HostError(match cleanup {
                Ok(()) => "io_popen('w') did not provide stdin pipe".to_string(),
                Err(error) => format!(
                    "io_popen('w') did not provide stdin pipe; process cleanup failed: {error}"
                ),
            }))
        }
        _ => unreachable!("mode validated above"),
    }
}

fn close_io_handle(mut handle: IoHandle) -> VmResult<()> {
    match &mut handle {
        IoHandle::File(file) => file
            .flush()
            .map_err(|error| VmError::HostError(format!("io_close flush failed: {error}"))),
        IoHandle::PopenRead { child } => terminate_child_tree(child),
        IoHandle::PopenWrite { child } => {
            let _ = child.stdin.take();
            terminate_child_tree(child)
        }
    }
}

fn terminate_child_tree(child: &mut Child) -> VmResult<()> {
    let pid = child.id();
    terminate_process_tree_and_leader(
        || terminate_process_tree(pid),
        || kill_and_reap_child(child),
    )
    .map_err(|error| VmError::HostError(format!("io_close popen terminate failed: {error}")))
}

fn kill_and_reap_child(child: &mut Child) -> io::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    match child.kill() {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => {}
        Err(error) => return Err(error),
    }
    child.wait().map(|_| ())
}

fn terminate_process_tree_and_leader<Tree, Leader>(
    terminate_tree: Tree,
    terminate_leader: Leader,
) -> io::Result<()>
where
    Tree: FnOnce() -> io::Result<()>,
    Leader: FnOnce() -> io::Result<()>,
{
    let tree_result = terminate_tree();
    let leader_result = terminate_leader();
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

fn terminate_process_tree(pid: u32) -> io::Result<()> {
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
        run_taskkill_with(pid, Command::status)
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
    Run: FnOnce(&mut Command) -> io::Result<std::process::ExitStatus>,
{
    let mut command = Command::new("taskkill");
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

fn read_line_from_reader(reader: &mut impl Read) -> VmResult<String> {
    let mut bytes = Vec::new();
    let mut one = [0u8; 1];
    loop {
        let read = reader
            .read(&mut one)
            .map_err(|error| VmError::HostError(format!("io_read_line failed: {error}")))?;
        if read == 0 {
            break;
        }
        bytes.push(one[0]);
        if one[0] == b'\n' {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io;

    use super::*;

    #[test]
    fn process_tree_failure_still_attempts_direct_blocking_leader_cleanup() {
        let leader_attempted = Cell::new(false);
        let error = terminate_process_tree_and_leader(
            || {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "tree denied",
                ))
            },
            || {
                leader_attempted.set(true);
                Ok(())
            },
        )
        .expect_err("tree failure must propagate");

        assert!(leader_attempted.get());
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
