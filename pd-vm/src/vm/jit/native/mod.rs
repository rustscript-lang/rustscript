use super::super::super::{VmError, VmResult};
use std::sync::{Mutex, OnceLock};

#[cfg(feature = "cranelift-jit")]
mod cranelift;
mod exec;

pub(super) const STATUS_CONTINUE: i32 = 0;
pub(super) const STATUS_HALTED: i32 = 1;
pub(super) const STATUS_TRACE_EXIT: i32 = 2;
pub(super) const STATUS_YIELDED: i32 = 3;
pub(super) const STATUS_WAITING: i32 = 4;
pub(super) const STATUS_OUT_OF_FUEL: i32 = 5;
pub(super) const STATUS_ERROR: i32 = -1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum NativeCompileProfile {
    Jit,
    Aot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum NativeInterruptMode {
    Fuel,
    Epoch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct NativeInterruptSettings {
    pub(super) mode: NativeInterruptMode,
    pub(super) check_interval: u32,
}

impl NativeInterruptSettings {
    pub(super) const fn fuel(check_interval: u32) -> Self {
        Self {
            mode: NativeInterruptMode::Fuel,
            check_interval,
        }
    }

    pub(super) const fn epoch(check_interval: u32) -> Self {
        Self {
            mode: NativeInterruptMode::Epoch,
            check_interval,
        }
    }
}

#[cfg(feature = "cranelift-jit")]
pub(super) fn selected_codegen_backend() -> &'static str {
    "native"
}

#[cfg(not(feature = "cranelift-jit"))]
pub(super) fn selected_codegen_backend() -> &'static str {
    "native-disabled"
}

#[cfg(feature = "cranelift-jit")]
pub(crate) use cranelift::{CompiledTrace, TraceKeepAlive};

#[cfg(feature = "cranelift-jit")]
pub(crate) fn load_compiled_trace(code: &[u8]) -> VmResult<Box<CompiledTrace>> {
    Ok(Box::new(cranelift::load_compiled_trace(code)?))
}

#[cfg(not(feature = "cranelift-jit"))]
pub(crate) struct TraceKeepAlive;

#[cfg(not(feature = "cranelift-jit"))]
pub(crate) struct CompiledTrace {
    pub entry: *const u8,
    pub code: Vec<u8>,
    pub keepalive: TraceKeepAlive,
}

#[cfg(feature = "cranelift-jit")]
pub(super) fn compile_native_trace(
    trace: &super::JitTrace,
    interrupt_settings: Option<NativeInterruptSettings>,
    profile: NativeCompileProfile,
) -> VmResult<Box<CompiledTrace>> {
    Ok(Box::new(cranelift::compile_trace(
        trace,
        interrupt_settings,
        profile,
    )?))
}

#[cfg(not(feature = "cranelift-jit"))]
pub(super) fn compile_native_trace(
    _trace: &super::JitTrace,
    _interrupt_settings: Option<NativeInterruptSettings>,
    _profile: NativeCompileProfile,
) -> VmResult<Box<CompiledTrace>> {
    Err(VmError::JitNative(
        "native JIT backend is disabled (feature 'cranelift-jit' is not enabled)".to_string(),
    ))
}

#[cfg(not(feature = "cranelift-jit"))]
pub(crate) fn load_compiled_trace(_code: &[u8]) -> VmResult<Box<CompiledTrace>> {
    Err(VmError::JitNative(
        "native JIT backend is disabled (feature 'cranelift-jit' is not enabled)".to_string(),
    ))
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn helper_entry_address() -> usize {
    cranelift::helper_entry_address()
}

#[cfg(not(feature = "cranelift-jit"))]
pub(crate) fn helper_entry_address() -> usize {
    0
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn layout_fingerprint() -> VmResult<u64> {
    cranelift::layout_fingerprint()
}

#[cfg(not(feature = "cranelift-jit"))]
pub(crate) fn layout_fingerprint() -> VmResult<u64> {
    Err(VmError::JitNative(
        "native JIT backend is disabled (feature 'cranelift-jit' is not enabled)".to_string(),
    ))
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
    use super::selected_codegen_backend;

    #[test]
    fn selected_backend_is_native() {
        #[cfg(feature = "cranelift-jit")]
        assert_eq!(selected_codegen_backend(), "native");
        #[cfg(not(feature = "cranelift-jit"))]
        assert_eq!(selected_codegen_backend(), "native-disabled");
    }
}
