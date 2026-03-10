use std::task::{Context, Poll};

use pd_host_function::pd_host_function;

use super::BuiltinResult;
use crate::vm::{HostOpId, Value, Vm, VmError, VmResult};

pub(crate) struct IoState;

impl Default for IoState {
    fn default() -> Self {
        Self
    }
}

pub(super) fn poll_builtin_io_op(
    _vm: &mut Vm,
    op_id: HostOpId,
    _cx: &mut Context<'_>,
) -> Poll<VmResult<Vec<Value>>> {
    Poll::Ready(Err(VmError::HostError(format!(
        "builtin io op {op_id} is unsupported on wasm32 runtime",
    ))))
}

pub(super) fn close_all_handles(_vm: &mut Vm) {}

#[pd_host_function(name = "io::open")]
pub(super) fn builtin_io_open(
    _vm: &mut Vm,
    _path: &str,
    _mode: &str,
) -> VmResult<BuiltinResult<i64>> {
    Err(VmError::HostError(
        "io::open is unsupported on wasm32 runtime".to_string(),
    ))
}

#[pd_host_function(name = "io::popen")]
pub(super) fn builtin_io_popen(
    _vm: &mut Vm,
    _command: &str,
    _mode: &str,
) -> VmResult<BuiltinResult<i64>> {
    Err(VmError::HostError(
        "io::popen is unsupported on wasm32 runtime".to_string(),
    ))
}

#[pd_host_function(name = "io::read_all")]
pub(super) fn builtin_io_read_all(
    _vm: &mut Vm,
    _handle_id: i64,
) -> VmResult<BuiltinResult<String>> {
    Err(VmError::HostError(
        "io::read_all is unsupported on wasm32 runtime".to_string(),
    ))
}

#[pd_host_function(name = "io::read_line")]
pub(super) fn builtin_io_read_line(
    _vm: &mut Vm,
    _handle_id: i64,
) -> VmResult<BuiltinResult<String>> {
    Err(VmError::HostError(
        "io::read_line is unsupported on wasm32 runtime".to_string(),
    ))
}

#[pd_host_function(name = "io::write")]
pub(super) fn builtin_io_write(
    _vm: &mut Vm,
    _handle_id: i64,
    _text: &str,
) -> VmResult<BuiltinResult<i64>> {
    Err(VmError::HostError(
        "io::write is unsupported on wasm32 runtime".to_string(),
    ))
}

#[pd_host_function(name = "io::flush")]
pub(super) fn builtin_io_flush(_vm: &mut Vm, _handle_id: i64) -> VmResult<BuiltinResult<bool>> {
    Err(VmError::HostError(
        "io::flush is unsupported on wasm32 runtime".to_string(),
    ))
}

#[pd_host_function(name = "io::close")]
pub(super) fn builtin_io_close(_vm: &mut Vm, _handle_id: i64) -> VmResult<BuiltinResult<bool>> {
    Err(VmError::HostError(
        "io::close is unsupported on wasm32 runtime".to_string(),
    ))
}

#[pd_host_function(name = "io::exists")]
pub(super) fn builtin_io_exists(_vm: &mut Vm, _path: &str) -> VmResult<BuiltinResult<bool>> {
    Err(VmError::HostError(
        "io::exists is unsupported on wasm32 runtime".to_string(),
    ))
}
