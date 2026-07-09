#[cfg(not(feature = "cranelift-jit"))]
use super::super::super::VmError;
use super::super::super::VmResult;

pub(crate) use crate::vm::native::{
    NativeInterruptSettings, STATUS_CONTINUE, STATUS_ERROR, STATUS_HALTED, STATUS_LINKED_CONTINUE,
    STATUS_OUT_OF_FUEL, STATUS_TRACE_EXIT, STATUS_WAITING, STATUS_YIELDED, clear_bridge_error,
    selected_codegen_backend, store_bridge_error, take_bridge_error,
};

#[cfg(feature = "cranelift-jit")]
mod lower;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum NativeCompileProfile {
    Jit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TraceLoweringKind {
    Ssa,
}

impl TraceLoweringKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ssa => "ssa",
        }
    }
}

#[cfg(feature = "cranelift-jit")]
pub(crate) use lower::{CompiledTrace, TraceKeepAlive};

#[cfg(not(feature = "cranelift-jit"))]
pub(crate) struct TraceKeepAlive;

#[cfg(not(feature = "cranelift-jit"))]
pub(crate) struct CompiledTrace {
    pub entry: *const u8,
    pub code: Vec<u8>,
    pub keepalive: TraceKeepAlive,
    pub lowering_kind: TraceLoweringKind,
}

#[cfg(feature = "cranelift-jit")]
pub(super) fn compile_native_trace(
    trace: &super::JitTrace,
    interrupt_settings: Option<NativeInterruptSettings>,
    profile: NativeCompileProfile,
    drop_contract_events_enabled: bool,
) -> VmResult<Box<CompiledTrace>> {
    Ok(Box::new(lower::compile_trace(
        trace,
        interrupt_settings,
        profile,
        drop_contract_events_enabled,
    )?))
}

#[cfg(not(feature = "cranelift-jit"))]
pub(super) fn compile_native_trace(
    _trace: &super::JitTrace,
    _interrupt_settings: Option<NativeInterruptSettings>,
    _profile: NativeCompileProfile,
    _drop_contract_events_enabled: bool,
) -> VmResult<Box<CompiledTrace>> {
    Err(VmError::JitNative(
        "native JIT backend is disabled (feature 'cranelift-jit' is not enabled)".to_string(),
    ))
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
