use super::super::super::{VmError, VmResult};
use std::sync::{Mutex, OnceLock};

mod cranelift;

pub(super) const STATUS_CONTINUE: i32 = 0;
pub(super) const STATUS_HALTED: i32 = 1;
pub(super) const STATUS_TRACE_EXIT: i32 = 2;
pub(super) const STATUS_YIELDED: i32 = 3;
pub(super) const STATUS_WAITING: i32 = 4;
pub(super) const STATUS_OUT_OF_FUEL: i32 = 5;
pub(super) const STATUS_ERROR: i32 = -1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum NativeCodegenBackend {
    Cranelift,
}

pub(super) fn selected_codegen_backend() -> NativeCodegenBackend {
    NativeCodegenBackend::Cranelift
}

pub(crate) use cranelift::{CraneliftCompiledTrace, CraneliftTraceKeepAlive};

pub(super) enum CompiledNativeTrace {
    Cranelift(Box<CraneliftCompiledTrace>),
}

pub(super) fn compile_native_trace(
    trace: &super::JitTrace,
    fuel_check_interval: Option<u32>,
) -> VmResult<CompiledNativeTrace> {
    Ok(CompiledNativeTrace::Cranelift(Box::new(
        cranelift::compile_trace(trace, fuel_check_interval)?,
    )))
}

static GENERIC_BRIDGE_ERROR: OnceLock<Mutex<Option<VmError>>> = OnceLock::new();

fn generic_bridge_error_cell() -> &'static Mutex<Option<VmError>> {
    GENERIC_BRIDGE_ERROR.get_or_init(|| Mutex::new(None))
}

pub(super) fn store_bridge_error(error: VmError) {
    if let Ok(mut guard) = generic_bridge_error_cell().lock() {
        *guard = Some(error);
    }
}

pub(super) fn clear_bridge_error() {
    if let Ok(mut guard) = generic_bridge_error_cell().lock() {
        *guard = None;
    }
}

pub(super) fn take_bridge_error() -> Option<VmError> {
    if let Ok(mut guard) = generic_bridge_error_cell().lock() {
        return guard.take();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{NativeCodegenBackend, selected_codegen_backend};

    #[test]
    fn selected_backend_is_cranelift() {
        assert_eq!(selected_codegen_backend(), NativeCodegenBackend::Cranelift);
    }
}
