use super::{VmError, VmResult};

#[cfg(all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")))]
mod aarch64;
#[cfg(all(
    target_arch = "x86_64",
    any(target_os = "windows", all(unix, not(target_os = "macos")))
))]
mod x86_64;

pub(super) const STATUS_CONTINUE: i32 = 0;
pub(super) const STATUS_HALTED: i32 = 1;
pub(super) const STATUS_TRACE_EXIT: i32 = 2;
pub(super) const STATUS_YIELDED: i32 = 3;
pub(super) const STATUS_WAITING: i32 = 4;
pub(super) const STATUS_ERROR: i32 = -1;

pub(super) trait NativeBackend {
    type ExecutableMemory;

    fn emit_trace_bytes(trace: &crate::jit::JitTrace) -> VmResult<Vec<u8>>;
    fn executable_memory_from_code(code: &[u8]) -> VmResult<Self::ExecutableMemory>;
    fn executable_memory_ptr(memory: &Self::ExecutableMemory) -> *mut u8;
    fn clear_bridge_error();
    fn take_bridge_error() -> Option<VmError>;
}

#[cfg(all(
    target_arch = "x86_64",
    any(target_os = "windows", all(unix, not(target_os = "macos")))
))]
type ActiveBackend = x86_64::X86_64Backend;
#[cfg(all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")))]
type ActiveBackend = aarch64::AArch64Backend;

pub(super) struct ExecutableMemory {
    pub(super) ptr: *mut u8,
    _inner: <ActiveBackend as NativeBackend>::ExecutableMemory,
}

// Executable memory is immutable after publication and managed via backend-owned lifetime.
// Sharing handles across threads is safe.
unsafe impl Send for ExecutableMemory {}
unsafe impl Sync for ExecutableMemory {}

impl ExecutableMemory {
    pub(super) fn from_code(code: &[u8]) -> VmResult<Self> {
        let inner = ActiveBackend::executable_memory_from_code(code)?;
        let ptr = ActiveBackend::executable_memory_ptr(&inner);
        Ok(Self { ptr, _inner: inner })
    }
}

pub(super) fn emit_native_trace_bytes(trace: &crate::jit::JitTrace) -> VmResult<Vec<u8>> {
    ActiveBackend::emit_trace_bytes(trace)
}

pub(super) fn clear_bridge_error() {
    ActiveBackend::clear_bridge_error();
}

pub(super) fn take_bridge_error() -> Option<VmError> {
    ActiveBackend::take_bridge_error()
}
