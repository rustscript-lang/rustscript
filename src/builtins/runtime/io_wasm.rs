use pd_host_function::pd_host_function;

use super::HostCallResult;
use crate::vm::{Vm, VmError, VmResult};

/// The wasm32 IO backend keeps no scope resource, but the guest-visible
/// `io.file` resource type is part of the standard catalog on every target, so
/// the declaration lives here for this build.
pub(crate) struct WasmIoFileHandle;

impl crate::vm::resource::HostResource for WasmIoFileHandle {}

impl crate::host_extension::HostResourceType for WasmIoFileHandle {
    const KEY: &'static str = "io.file";
    const DESCRIPTION: &'static str = "An open file handle";
}

/// The canonical declaration for the `io.file` resource type.
pub(crate) fn io_file_resource() -> crate::host_extension::HostResourceTypeMeta {
    crate::host_extension::HostResourceTypeMeta::of::<WasmIoFileHandle>()
}

/// Opens a file handle for runtime I/O.
#[pd_host_function(name = "io::open", contract = super::io_open_contract)]
pub(super) fn builtin_io_open(
    _vm: &mut Vm,
    _path: &str,
    _mode: &str,
) -> VmResult<HostCallResult<i64>> {
    Err(VmError::HostError(
        "io::open is unsupported on wasm32 runtime".to_string(),
    ))
}

/// Starts a child process and returns a process-backed handle.
#[pd_host_function(name = "io::popen")]
pub(super) fn builtin_io_popen(
    _vm: &mut Vm,
    _command: &str,
    _mode: &str,
) -> VmResult<HostCallResult<i64>> {
    Err(VmError::HostError(
        "io::popen is unsupported on wasm32 runtime".to_string(),
    ))
}

/// Reads all remaining text from an I/O handle.
#[pd_host_function(name = "io::read_all", contract = super::io_read_all_contract)]
pub(super) fn builtin_io_read_all(
    _vm: &mut Vm,
    _handle_id: i64,
) -> VmResult<HostCallResult<String>> {
    Err(VmError::HostError(
        "io::read_all is unsupported on wasm32 runtime".to_string(),
    ))
}

/// Reads a single line of text from an I/O handle.
#[pd_host_function(name = "io::read_line")]
pub(super) fn builtin_io_read_line(
    _vm: &mut Vm,
    _handle_id: i64,
) -> VmResult<HostCallResult<String>> {
    Err(VmError::HostError(
        "io::read_line is unsupported on wasm32 runtime".to_string(),
    ))
}

/// Writes text to an I/O handle.
#[pd_host_function(name = "io::write")]
pub(super) fn builtin_io_write(
    _vm: &mut Vm,
    _handle_id: i64,
    _text: &str,
) -> VmResult<HostCallResult<i64>> {
    Err(VmError::HostError(
        "io::write is unsupported on wasm32 runtime".to_string(),
    ))
}

/// Flushes buffered output for an I/O handle.
#[pd_host_function(name = "io::flush")]
pub(super) fn builtin_io_flush(_vm: &mut Vm, _handle_id: i64) -> VmResult<HostCallResult<bool>> {
    Err(VmError::HostError(
        "io::flush is unsupported on wasm32 runtime".to_string(),
    ))
}

/// Closes an I/O handle.
#[pd_host_function(name = "io::close", contract = super::io_close_contract)]
pub(super) fn builtin_io_close(_vm: &mut Vm, _handle_id: i64) -> VmResult<HostCallResult<bool>> {
    Err(VmError::HostError(
        "io::close is unsupported on wasm32 runtime".to_string(),
    ))
}

/// Returns whether a file system path exists.
#[pd_host_function(name = "io::exists")]
pub(super) fn builtin_io_exists(_vm: &mut Vm, _path: &str) -> VmResult<HostCallResult<bool>> {
    Err(VmError::HostError(
        "io::exists is unsupported on wasm32 runtime".to_string(),
    ))
}
