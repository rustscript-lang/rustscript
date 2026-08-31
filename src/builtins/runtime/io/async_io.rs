//! Async (tokio-based) IO builtin implementations.
//!
//! This file is a thin wrapper around the canonical implementation in
//! `shared.rs`. The `#[pd_host_function]` attribute generates the VM
//! dispatch wrapper; the actual function bodies live in `shared.rs`.
//!
//! Uses the same concrete [`HostResource`] types as the blocking path:
//! [`IoFileResource`] and aggregate [`IoPipeResource`] values stored in the
//! execution scope via `push_resource_with_key`. Operations use
//! [`HostOperation`] drivers and the scope's [`OperationRegistry`].

use pd_host_function::pd_host_function;

use super::super::HostCallResult;
use super::super::typed::{VmArrayRef, VmMap};
use super::shared::*;

// ---- IO builtin functions (thin wrappers with #[pd_host_function]) ----

/// Opens a file handle for runtime I/O.
/// The actual file open runs on a worker thread; the resource is created
/// by the PendingOpResult provider after the worker completes.
#[pd_host_function(name = "io::open")]
pub(crate) fn builtin_io_open(
    vm: &mut Vm,
    path: &str,
    mode: &str,
) -> VmResult<HostCallResult<i64>> {
    builtin_io_open_body(vm, path, mode)
}

/// Starts a child process and returns a process-backed handle.
/// The process spawn runs on a worker thread.
#[pd_host_function(name = "io::popen")]
pub(crate) fn builtin_io_popen(
    vm: &mut Vm,
    command: &str,
    mode: &str,
) -> VmResult<HostCallResult<i64>> {
    builtin_io_popen_body(vm, command, mode)
}

/// Executes a bounded argv-only process and returns bounded output metadata.
#[pd_host_function(name = "io::exec")]
pub(crate) fn builtin_io_exec(
    vm: &mut Vm,
    argv: VmArrayRef<'_>,
    timeout_ms: i64,
    max_output_bytes: i64,
) -> VmResult<HostCallResult<VmMap>> {
    builtin_io_exec_body(vm, argv, timeout_ms, max_output_bytes)
}

/// Reads all remaining text from an I/O handle.
/// The actual read runs on a worker thread.
#[pd_host_function(name = "io::read_all")]
pub(crate) fn builtin_io_read_all(vm: &mut Vm, handle_id: i64) -> VmResult<HostCallResult<String>> {
    builtin_io_read_all_body(vm, handle_id)
}

/// Reads a single line of text from an I/O handle.
#[pd_host_function(name = "io::read_line")]
pub(crate) fn builtin_io_read_line(
    vm: &mut Vm,
    handle_id: i64,
) -> VmResult<HostCallResult<String>> {
    builtin_io_read_line_body(vm, handle_id)
}

/// Writes text to an I/O handle.
#[pd_host_function(name = "io::write")]
pub(crate) fn builtin_io_write(
    vm: &mut Vm,
    handle_id: i64,
    text: &str,
) -> VmResult<HostCallResult<i64>> {
    builtin_io_write_body(vm, handle_id, text)
}

/// Flushes buffered output for an I/O handle.
#[pd_host_function(name = "io::flush")]
pub(crate) fn builtin_io_flush(vm: &mut Vm, handle_id: i64) -> VmResult<HostCallResult<bool>> {
    builtin_io_flush_body(vm, handle_id)
}

/// Closes an I/O handle.
/// The actual close teardown (flush, process kill) is delegated to the
/// resource's begin_close/poll_close lifecycle, which spawns a worker.
#[pd_host_function(name = "io::close")]
pub(crate) fn builtin_io_close(vm: &mut Vm, handle_id: i64) -> VmResult<HostCallResult<bool>> {
    builtin_io_close_body(vm, handle_id)
}

/// Returns whether a file system path exists.
/// The actual filesystem check runs on a worker thread so the VM thread
/// never blocks on IO.
#[pd_host_function(name = "io::exists")]
pub(crate) fn builtin_io_exists(vm: &mut Vm, path: &str) -> VmResult<HostCallResult<bool>> {
    builtin_io_exists_body(vm, path)
}
