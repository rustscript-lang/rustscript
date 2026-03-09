use std::task::{Context, Poll};

use super::super::{HostOpId, Value, Vm, VmError, VmResult};
use super::BuiltinCallOutcome;

pub(in crate::vm) struct IoState;

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

fn unsupported_io(name: &str) -> VmResult<BuiltinCallOutcome> {
    Err(VmError::HostError(format!(
        "{name} is unsupported on wasm32 runtime",
    )))
}

pub(super) fn builtin_io_open(_vm: &mut Vm, _args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
    unsupported_io("io::open")
}

pub(super) fn builtin_io_popen(_vm: &mut Vm, _args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
    unsupported_io("io::popen")
}

pub(super) fn builtin_io_read_all(_vm: &mut Vm, _args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
    unsupported_io("io::read_all")
}

pub(super) fn builtin_io_read_line(
    _vm: &mut Vm,
    _args: Vec<Value>,
) -> VmResult<BuiltinCallOutcome> {
    unsupported_io("io::read_line")
}

pub(super) fn builtin_io_write(_vm: &mut Vm, _args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
    unsupported_io("io::write")
}

pub(super) fn builtin_io_flush(_vm: &mut Vm, _args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
    unsupported_io("io::flush")
}

pub(super) fn builtin_io_close(_vm: &mut Vm, _args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
    unsupported_io("io::close")
}

pub(super) fn builtin_io_exists(_vm: &mut Vm, _args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
    unsupported_io("io::exists")
}
