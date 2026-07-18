#![allow(dead_code)]
#[cfg(not(feature = "cranelift-jit"))]
use super::super::super::VmError;
use super::super::super::VmResult;
use super::ir::{SsaMaterialization, SsaValueRepr};
use crate::ValueType;
use std::sync::atomic::{AtomicPtr, Ordering};

pub(crate) use crate::vm::native::{
    NativeInterruptSettings, STATUS_CONTINUE, STATUS_ERROR, STATUS_HALTED, STATUS_LINKED_CONTINUE,
    STATUS_OUT_OF_FUEL, STATUS_TRACE_EXIT, STATUS_WAITING, STATUS_YIELDED, clear_bridge_error,
    decode_jit_trace_exit_status, selected_codegen_backend, store_bridge_error, take_bridge_error,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SideEntryOwnership {
    Borrowed,
    Owned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InheritedStateAbiClass {
    ScalarInt,
    ScalarFloat,
    ScalarBool,
    HeapPointer {
        tag: ValueType,
        ownership: SideEntryOwnership,
    },
    Tagged(SideEntryOwnership),
}

pub(crate) fn classify_side_entry_repr(
    repr: SsaValueRepr,
    ownership: SideEntryOwnership,
) -> InheritedStateAbiClass {
    match repr {
        SsaValueRepr::I64 => InheritedStateAbiClass::ScalarInt,
        SsaValueRepr::F64 => InheritedStateAbiClass::ScalarFloat,
        SsaValueRepr::Bool => InheritedStateAbiClass::ScalarBool,
        SsaValueRepr::HeapPtr(tag) => InheritedStateAbiClass::HeapPointer { tag, ownership },
        SsaValueRepr::Tagged => InheritedStateAbiClass::Tagged(ownership),
    }
}

pub(crate) fn classify_side_entry_materialization(
    materialization: &SsaMaterialization,
) -> InheritedStateAbiClass {
    match materialization {
        SsaMaterialization::Value(_) => {
            InheritedStateAbiClass::Tagged(SideEntryOwnership::Borrowed)
        }
        SsaMaterialization::BoxInt(_) => InheritedStateAbiClass::ScalarInt,
        SsaMaterialization::BoxFloat(_) => InheritedStateAbiClass::ScalarFloat,
        SsaMaterialization::BoxBool(_) => InheritedStateAbiClass::ScalarBool,
        SsaMaterialization::BoxHeapPtr { tag, .. } => InheritedStateAbiClass::HeapPointer {
            tag: *tag,
            ownership: SideEntryOwnership::Owned,
        },
    }
}

pub(crate) struct NativeSideLinkSlot {
    entry: AtomicPtr<u8>,
}

impl NativeSideLinkSlot {
    pub(crate) const fn new() -> Self {
        Self {
            entry: AtomicPtr::new(std::ptr::null_mut()),
        }
    }

    pub(crate) fn target(&self) -> *mut u8 {
        self.entry.load(Ordering::Acquire)
    }

    pub(crate) fn publish(&self, entry: *const u8) {
        self.entry.store(entry.cast_mut(), Ordering::Release);
    }

    pub(crate) fn clear(&self) {
        self.entry.store(std::ptr::null_mut(), Ordering::Release);
    }

    pub(crate) fn address(&self) -> *mut *mut u8 {
        self.entry.as_ptr()
    }
}

impl Default for NativeSideLinkSlot {
    fn default() -> Self {
        Self::new()
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
        &[],
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

#[cfg(feature = "cranelift-jit")]
pub(super) fn compile_native_region(
    region: &super::region::FusedRegion,
    interrupt_settings: Option<NativeInterruptSettings>,
    profile: NativeCompileProfile,
    drop_contract_events_enabled: bool,
) -> VmResult<Box<CompiledTrace>> {
    Ok(Box::new(lower::compile_trace(
        &region.trace,
        &region.links,
        interrupt_settings,
        profile,
        drop_contract_events_enabled,
    )?))
}

#[cfg(not(feature = "cranelift-jit"))]
pub(super) fn compile_native_region(
    _region: &super::region::FusedRegion,
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
    use super::lower::{
        compile_system_owned_tail_wrapper, compile_system_tail_wrapper,
        compile_tail_owned_clear_body, compile_tail_owned_side_link_body,
        compile_tail_side_link_body, compile_tail_status_body,
    };
    use super::{
        InheritedStateAbiClass, NativeSideLinkSlot, SideEntryOwnership,
        classify_side_entry_materialization, classify_side_entry_repr, selected_codegen_backend,
    };
    use crate::vm::jit::ir::{SsaMaterialization, SsaValueId, SsaValueRepr};
    use crate::{Value, ValueType};
    use std::sync::Arc;

    #[cfg(target_os = "linux")]
    fn assert_executable_mapping_is_not_writable(entry: *const u8) {
        let address = entry as usize;
        let maps = std::fs::read_to_string("/proc/self/maps").expect("process maps should read");
        let mapping = maps
            .lines()
            .find(|line| {
                let Some((range, _)) = line.split_once(' ') else {
                    return false;
                };
                let Some((start, end)) = range.split_once('-') else {
                    return false;
                };
                let Ok(start) = usize::from_str_radix(start, 16) else {
                    return false;
                };
                let Ok(end) = usize::from_str_radix(end, 16) else {
                    return false;
                };
                start <= address && address < end
            })
            .expect("entry mapping should exist");
        let permissions = mapping
            .split_whitespace()
            .nth(1)
            .expect("mapping permissions should exist");
        assert!(permissions.contains('x'), "{mapping}");
        assert!(!permissions.contains('w'), "{mapping}");
    }

    #[test]
    fn side_entry_abi_classifies_scalar_pointer_tagged_borrowed_and_owned_values() {
        let value = SsaValueId::new(7);
        assert_eq!(
            classify_side_entry_materialization(&SsaMaterialization::BoxInt(value)),
            InheritedStateAbiClass::ScalarInt
        );
        assert_eq!(
            classify_side_entry_materialization(&SsaMaterialization::BoxFloat(value)),
            InheritedStateAbiClass::ScalarFloat
        );
        assert_eq!(
            classify_side_entry_materialization(&SsaMaterialization::BoxBool(value)),
            InheritedStateAbiClass::ScalarBool
        );
        assert_eq!(
            classify_side_entry_materialization(&SsaMaterialization::Value(value)),
            InheritedStateAbiClass::Tagged(SideEntryOwnership::Borrowed)
        );
        assert_eq!(
            classify_side_entry_materialization(&SsaMaterialization::BoxHeapPtr {
                value,
                tag: ValueType::String,
            }),
            InheritedStateAbiClass::HeapPointer {
                tag: ValueType::String,
                ownership: SideEntryOwnership::Owned,
            }
        );
        assert_eq!(
            classify_side_entry_repr(
                SsaValueRepr::HeapPtr(ValueType::Array),
                SideEntryOwnership::Borrowed,
            ),
            InheritedStateAbiClass::HeapPointer {
                tag: ValueType::Array,
                ownership: SideEntryOwnership::Borrowed,
            }
        );
    }

    #[cfg(feature = "cranelift-jit")]
    #[test]
    fn trace_jit_side_link_slot_switches_between_deopt_and_child() {
        if selected_codegen_backend() != "native" {
            return;
        }
        const DEOPT_STATUS: i32 = 17;
        const CHILD_STATUS: i32 = 23;
        let slot = Box::new(NativeSideLinkSlot::new());
        let child = compile_tail_status_body(CHILD_STATUS).expect("tail child should compile");
        let root = compile_tail_side_link_body(slot.address() as usize, DEOPT_STATUS)
            .expect("tail root should compile");
        let wrapper =
            compile_system_tail_wrapper(root.entry()).expect("system wrapper should compile");
        #[cfg(target_os = "linux")]
        assert_executable_mapping_is_not_writable(wrapper.entry());
        assert!(child.code_len() > 0);
        assert!(root.code_len() > 0);
        assert!(wrapper.code_len() > 0);
        let entry = unsafe {
            std::mem::transmute::<*const u8, unsafe extern "C" fn(*mut crate::Vm) -> i32>(
                wrapper.entry(),
            )
        };

        assert!(slot.target().is_null());
        assert_eq!(unsafe { entry(std::ptr::null_mut()) }, DEOPT_STATUS);
        slot.publish(child.entry());
        assert_eq!(slot.target().cast_const(), child.entry());
        assert_eq!(unsafe { entry(std::ptr::null_mut()) }, CHILD_STATUS);
        slot.clear();
        assert!(slot.target().is_null());
        assert_eq!(unsafe { entry(std::ptr::null_mut()) }, DEOPT_STATUS);
    }

    #[cfg(feature = "cranelift-jit")]
    #[test]
    fn trace_jit_side_entry_transfers_owned_values_once() {
        if selected_codegen_backend() != "native" {
            return;
        }
        const DEOPT_STATUS: i32 = 29;
        const CHILD_STATUS: i32 = 31;
        let slot = Box::new(NativeSideLinkSlot::new());
        let child =
            compile_tail_owned_clear_body(CHILD_STATUS).expect("owned child should compile");
        let root = compile_tail_owned_side_link_body(slot.address() as usize, DEOPT_STATUS)
            .expect("owned root should compile");
        let wrapper = compile_system_owned_tail_wrapper(root.entry())
            .expect("owned system wrapper should compile");
        let entry = unsafe {
            std::mem::transmute::<*const u8, unsafe extern "C" fn(*mut crate::Vm, *mut Value) -> i32>(
                wrapper.entry(),
            )
        };
        let backing = Arc::new("owned".to_string());
        let mut owned = Value::String(backing.clone());
        assert_eq!(Arc::strong_count(&backing), 2);

        assert_eq!(
            unsafe { entry(std::ptr::null_mut(), &mut owned) },
            DEOPT_STATUS
        );
        assert!(matches!(owned, Value::String(_)));
        assert_eq!(Arc::strong_count(&backing), 2);

        slot.publish(child.entry());
        assert_eq!(
            unsafe { entry(std::ptr::null_mut(), &mut owned) },
            CHILD_STATUS
        );
        assert!(matches!(owned, Value::Null));
        assert_eq!(Arc::strong_count(&backing), 1);

        assert_eq!(
            unsafe { entry(std::ptr::null_mut(), &mut owned) },
            CHILD_STATUS
        );
        assert!(matches!(owned, Value::Null));
        assert_eq!(Arc::strong_count(&backing), 1);
    }

    #[test]
    fn selected_backend_is_native() {
        #[cfg(feature = "cranelift-jit")]
        assert_eq!(selected_codegen_backend(), "native");
        #[cfg(not(feature = "cranelift-jit"))]
        assert_eq!(selected_codegen_backend(), "native-disabled");
    }
}
