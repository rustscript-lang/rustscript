use super::super::super::{VmError, VmResult};
use std::env;
use std::sync::{Mutex, OnceLock};

#[cfg(all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")))]
mod aarch64;
#[cfg(feature = "cranelift-jit")]
mod cranelift;
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

    fn emit_trace_bytes(trace: &super::JitTrace) -> VmResult<Vec<u8>>;
    fn executable_memory_from_code(code: &[u8]) -> VmResult<Self::ExecutableMemory>;
    fn executable_memory_ptr(memory: &Self::ExecutableMemory) -> *mut u8;
    fn clear_bridge_error();
    fn take_bridge_error() -> Option<VmError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum NativeCodegenBackend {
    Handwritten,
    Cranelift,
}

impl NativeCodegenBackend {
    fn parse(raw: &str) -> Option<Self> {
        if raw.eq_ignore_ascii_case("handwritten") || raw.eq_ignore_ascii_case("native") {
            return Some(Self::Handwritten);
        }
        if raw.eq_ignore_ascii_case("cranelift") {
            return Some(Self::Cranelift);
        }
        None
    }
}

pub(super) fn selected_codegen_backend() -> NativeCodegenBackend {
    let Some(raw) = env::var("PD_VM_JIT_CODEGEN").ok() else {
        return NativeCodegenBackend::Handwritten;
    };
    NativeCodegenBackend::parse(raw.trim()).unwrap_or(NativeCodegenBackend::Handwritten)
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

#[cfg(feature = "cranelift-jit")]
pub(crate) use cranelift::{CraneliftCompiledTrace, CraneliftTraceKeepAlive};

pub(super) enum CompiledNativeTrace {
    Handwritten {
        code: Vec<u8>,
    },
    #[cfg(feature = "cranelift-jit")]
    Cranelift(CraneliftCompiledTrace),
}

pub(super) fn compile_native_trace(
    trace: &super::JitTrace,
    backend: NativeCodegenBackend,
) -> VmResult<CompiledNativeTrace> {
    match backend {
        NativeCodegenBackend::Handwritten => Ok(CompiledNativeTrace::Handwritten {
            code: ActiveBackend::emit_trace_bytes(trace)?,
        }),
        NativeCodegenBackend::Cranelift => compile_native_trace_cranelift(trace),
    }
}

pub(super) fn emit_native_trace_bytes(trace: &super::JitTrace) -> VmResult<Vec<u8>> {
    ActiveBackend::emit_trace_bytes(trace)
}

static GENERIC_BRIDGE_ERROR: OnceLock<Mutex<Option<VmError>>> = OnceLock::new();

fn generic_bridge_error_cell() -> &'static Mutex<Option<VmError>> {
    GENERIC_BRIDGE_ERROR.get_or_init(|| Mutex::new(None))
}

#[cfg(feature = "cranelift-jit")]
pub(super) fn store_bridge_error(error: VmError) {
    if let Ok(mut guard) = generic_bridge_error_cell().lock() {
        *guard = Some(error);
    }
}

pub(super) fn clear_bridge_error() {
    if let Ok(mut guard) = generic_bridge_error_cell().lock() {
        *guard = None;
    }
    ActiveBackend::clear_bridge_error();
}

pub(super) fn take_bridge_error() -> Option<VmError> {
    if let Ok(mut guard) = generic_bridge_error_cell().lock()
        && let Some(error) = guard.take()
    {
        return Some(error);
    }
    ActiveBackend::take_bridge_error()
}

fn compile_native_trace_cranelift(trace: &super::JitTrace) -> VmResult<CompiledNativeTrace> {
    #[cfg(feature = "cranelift-jit")]
    {
        return Ok(CompiledNativeTrace::Cranelift(cranelift::compile_trace(
            trace,
        )?));
    }

    #[cfg(not(feature = "cranelift-jit"))]
    {
        let _ = trace;
        Err(VmError::JitNative(
            "Cranelift backend requested, but pd-vm was built without `cranelift-jit` feature"
                .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::NativeCodegenBackend;

    #[test]
    fn backend_parse_accepts_expected_names() {
        assert_eq!(
            NativeCodegenBackend::parse("handwritten"),
            Some(NativeCodegenBackend::Handwritten)
        );
        assert_eq!(
            NativeCodegenBackend::parse("native"),
            Some(NativeCodegenBackend::Handwritten)
        );
        assert_eq!(
            NativeCodegenBackend::parse("cranelift"),
            Some(NativeCodegenBackend::Cranelift)
        );
        assert_eq!(NativeCodegenBackend::parse("other"), None);
    }
}
