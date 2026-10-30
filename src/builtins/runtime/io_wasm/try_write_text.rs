use pd_host_function::pd_host_function;

use super::super::IoWriteTextResult;
use super::HostCallResult;
use crate::vm::{Value, Vm, VmResult};

/// Whole-file writing is unavailable in the wasm32 backend and reports a typed outcome.
#[pd_host_function(name = "io::try_write_text", contract = super::super::io_try_write_text_contract)]
pub(super) fn builtin_io_try_write_text(
    _vm: &mut Vm,
    _path: &str,
    _text: &str,
) -> VmResult<HostCallResult<Value>> {
    Ok(HostCallResult::Return(
        IoWriteTextResult::failure("unsupported").into_value(),
    ))
}
