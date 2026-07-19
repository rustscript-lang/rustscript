#![allow(dead_code)]
use crate::builtins::BuiltinFunction;
use crate::bytecode::{CallableKind, CallableTarget, Value, ValueType, VmMap};
use crate::vm::{
    CallOutcome, CallReturn, ExecOutcome, ExecutionFrame, FrameContinuation, HostCallExecOutcome,
    NumericValue, Vm, VmError, VmHostFunction, VmResult, logical_shr_i64,
};
use std::cell::RefCell;
use std::sync::Arc;

pub(crate) const STATUS_CONTINUE: i32 = 0;
pub(crate) const STATUS_HALTED: i32 = 1;
pub(crate) const STATUS_TRACE_EXIT: i32 = 2;
pub(crate) const STATUS_YIELDED: i32 = 3;
pub(crate) const STATUS_WAITING: i32 = 4;
pub(crate) const STATUS_OUT_OF_FUEL: i32 = 5;
pub(crate) const STATUS_LINKED_CONTINUE: i32 = 6;
pub(crate) const STATUS_ERROR: i32 = -1;
pub(crate) const STATUS_JIT_TRACE_EXIT_BASE: i32 = 0x100;
pub(crate) const STATUS_JIT_TRACE_EXIT_MAX_ID: u32 = u16::MAX as u32;
pub(crate) const STATUS_JIT_TRACE_EXIT_MAX: i32 =
    STATUS_JIT_TRACE_EXIT_BASE + STATUS_JIT_TRACE_EXIT_MAX_ID as i32;

pub(crate) fn encode_jit_trace_exit_status(exit_id: u32) -> Option<i32> {
    (exit_id <= STATUS_JIT_TRACE_EXIT_MAX_ID).then(|| STATUS_JIT_TRACE_EXIT_BASE + exit_id as i32)
}

pub(crate) fn decode_jit_trace_exit_status(status: i32) -> Option<u32> {
    (STATUS_JIT_TRACE_EXIT_BASE..=STATUS_JIT_TRACE_EXIT_MAX)
        .contains(&status)
        .then(|| (status - STATUS_JIT_TRACE_EXIT_BASE) as u32)
}

pub(crate) const ROOT_FRAME_KEY: u64 = u64::MAX;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct NativeFrameState {
    pub(crate) frame_key: u64,
    pub(crate) operand_stack_base: usize,
    pub(crate) active_stack_len: usize,
    pub(crate) local_base: usize,
    pub(crate) local_count: usize,
    pub(crate) frame_depth: usize,
    pub(crate) continuation_kind: u8,
}

pub(crate) const OP_LDC: i64 = 1;
pub(crate) const OP_ADD: i64 = 2;
pub(crate) const OP_SUB: i64 = 3;
pub(crate) const OP_MUL: i64 = 4;
pub(crate) const OP_DIV: i64 = 5;
pub(crate) const OP_MOD: i64 = 6;
pub(crate) const OP_SHL: i64 = 7;
pub(crate) const OP_SHR: i64 = 8;
pub(crate) const OP_LSHR: i64 = 9;
pub(crate) const OP_AND: i64 = 10;
pub(crate) const OP_OR: i64 = 11;
pub(crate) const OP_NOT: i64 = 12;
pub(crate) const OP_NEG: i64 = 13;
pub(crate) const OP_CEQ: i64 = 14;
pub(crate) const OP_CLT: i64 = 15;
pub(crate) const OP_CGT: i64 = 16;
pub(crate) const OP_POP: i64 = 17;
pub(crate) const OP_DUP: i64 = 18;
pub(crate) const OP_LDLOC: i64 = 19;
pub(crate) const OP_STLOC: i64 = 20;
pub(crate) const OP_CALL: i64 = 21;
pub(crate) const OP_GUARD_FALSE: i64 = 22;
pub(crate) const OP_JUMP: i64 = 23;
pub(crate) const OP_BUILTIN_CALL: i64 = 24;
pub(crate) const OP_GUARD_TRUE: i64 = 25;
pub(crate) const OP_LOOP_IF_FALSE: i64 = 26;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NativeInterruptMode {
    Fuel,
    Epoch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct NativeInterruptSettings {
    pub(crate) mode: NativeInterruptMode,
    pub(crate) check_interval: u32,
}

impl NativeInterruptSettings {
    pub(crate) const fn fuel(check_interval: u32) -> Self {
        Self {
            mode: NativeInterruptMode::Fuel,
            check_interval,
        }
    }

    pub(crate) const fn epoch(check_interval: u32) -> Self {
        Self {
            mode: NativeInterruptMode::Epoch,
            check_interval,
        }
    }
}

thread_local! {
    static GENERIC_BRIDGE_ERROR: RefCell<Option<VmError>> = const { RefCell::new(None) };
}

pub(crate) fn store_bridge_error(error: VmError) {
    GENERIC_BRIDGE_ERROR.with(|slot| *slot.borrow_mut() = Some(error));
}

pub(crate) fn clear_bridge_error() {
    GENERIC_BRIDGE_ERROR.with(|slot| *slot.borrow_mut() = None);
}

pub(crate) fn take_bridge_error() -> Option<VmError> {
    GENERIC_BRIDGE_ERROR.with(|slot| slot.borrow_mut().take())
}

fn arc_repr_word<T>(value: &Arc<T>) -> usize {
    debug_assert_eq!(std::mem::size_of::<Arc<T>>(), std::mem::size_of::<usize>());
    unsafe { *(value as *const Arc<T> as *const usize) }
}

fn arc_into_repr_ptr<T>(value: Arc<T>) -> *mut u8 {
    let ptr = arc_repr_word(&value) as *mut u8;
    std::mem::forget(value);
    ptr
}

unsafe fn arc_from_repr_ptr<T>(ptr: *mut u8) -> Arc<T> {
    debug_assert_eq!(
        std::mem::size_of::<Arc<T>>(),
        std::mem::size_of::<*mut u8>()
    );
    unsafe { std::mem::transmute_copy(&ptr) }
}

fn run_step<F>(vm: *mut Vm, helper_name: &'static str, f: F) -> i32
where
    F: FnOnce(&mut Vm) -> VmResult<i32>,
{
    let Some(vm_ref) = (unsafe { vm.as_mut() }) else {
        store_bridge_error(VmError::JitNative(format!(
            "native {helper_name} helper received null vm pointer"
        )));
        return STATUS_ERROR;
    };

    vm_ref.record_native_bridge_hit(helper_name);
    match f(vm_ref) {
        Ok(status) => status,
        Err(err) => {
            store_bridge_error(err);
            STATUS_ERROR
        }
    }
}

fn bridge_name_for_op(op: i64) -> Option<&'static str> {
    match op {
        OP_LDC => Some("ldc"),
        OP_ADD => Some("add"),
        OP_SUB => Some("sub"),
        OP_MUL => Some("mul"),
        OP_DIV => Some("div"),
        OP_MOD => Some("mod"),
        OP_SHL => Some("shl"),
        OP_SHR => Some("shr"),
        OP_LSHR => Some("lshr"),
        OP_AND => Some("and"),
        OP_OR => Some("or"),
        OP_NOT => Some("not"),
        OP_NEG => Some("neg"),
        OP_CEQ => Some("ceq"),
        OP_CLT => Some("clt"),
        OP_CGT => Some("cgt"),
        OP_POP => Some("pop"),
        OP_DUP => Some("dup"),
        OP_LDLOC => Some("ldloc"),
        OP_STLOC => Some("stloc"),
        OP_CALL => Some("call"),
        OP_BUILTIN_CALL => Some("builtin_call"),
        OP_GUARD_FALSE => Some("guard_false"),
        OP_GUARD_TRUE => Some("guard_true"),
        OP_LOOP_IF_FALSE => Some("loop_if_false"),
        OP_JUMP => Some("jump_ip"),
        _ => None,
    }
}

pub(crate) fn helper_entry_address() -> usize {
    pd_vm_native_step as *const () as usize
}

pub(crate) fn interrupt_helper_entry_address() -> usize {
    pd_vm_native_interrupt_tick as *const () as usize
}

pub(crate) fn aot_call_boundary_interrupt_entry_address() -> usize {
    pd_vm_native_aot_call_boundary_interrupt as *const () as usize
}

pub(crate) fn alloc_byte_buffer_entry_address() -> usize {
    pd_vm_native_alloc_byte_buffer as *const () as usize
}

pub(crate) fn alloc_value_buffer_entry_address() -> usize {
    pd_vm_native_alloc_value_buffer as *const () as usize
}

pub(crate) fn shared_string_from_buffer_entry_address() -> usize {
    pd_vm_native_shared_string_from_buffer as *const () as usize
}

pub(crate) fn shared_bytes_from_buffer_entry_address() -> usize {
    pd_vm_native_shared_bytes_from_buffer as *const () as usize
}

pub(crate) fn shared_array_from_buffer_entry_address() -> usize {
    pd_vm_native_shared_array_from_buffer as *const () as usize
}

pub(crate) fn copy_bytes_entry_address() -> usize {
    pd_vm_native_copy_bytes as *const () as usize
}

pub(crate) fn zero_bytes_entry_address() -> usize {
    pd_vm_native_zero_bytes as *const () as usize
}

pub(crate) fn string_contains_entry_address() -> usize {
    pd_vm_native_string_contains as *const () as usize
}

pub(crate) fn regex_match_entry_address() -> usize {
    pd_vm_native_regex_match as *const () as usize
}

pub(crate) fn regex_replace_entry_address() -> usize {
    pd_vm_native_regex_replace as *const () as usize
}

pub(crate) fn string_replace_literal_entry_address() -> usize {
    pd_vm_native_string_replace_literal as *const () as usize
}

pub(crate) fn string_lower_ascii_entry_address() -> usize {
    pd_vm_native_string_lower_ascii as *const () as usize
}

pub(crate) fn type_of_entry_address() -> usize {
    pd_vm_native_type_of as *const () as usize
}

pub(crate) fn to_string_entry_address() -> usize {
    pd_vm_native_to_string as *const () as usize
}

pub(crate) fn string_split_literal_entry_address() -> usize {
    pd_vm_native_string_split_literal as *const () as usize
}

pub(crate) fn clone_value_to_slot_entry_address() -> usize {
    pd_vm_native_clone_value_to_slot as *const () as usize
}

pub(crate) fn init_null_value_slot_entry_address() -> usize {
    pd_vm_native_init_null_value_slot as *const () as usize
}

pub(crate) fn clear_value_slot_entry_address() -> usize {
    pd_vm_native_clear_value_slot as *const () as usize
}

pub(crate) fn value_eq_entry_address() -> usize {
    pd_vm_native_value_eq as *const () as usize
}

pub(crate) fn value_len_entry_address() -> usize {
    pd_vm_native_value_len as *const () as usize
}

pub(crate) fn write_heap_value_to_slot_entry_address() -> usize {
    pd_vm_native_write_heap_value_to_slot as *const () as usize
}

pub(crate) fn restore_exit_state_entry_address() -> usize {
    pd_vm_native_restore_exit_state as *const () as usize
}

pub(crate) fn active_stack_base_entry_address() -> usize {
    pd_vm_native_active_stack_base as *const () as usize
}

pub(crate) fn frame_state_entry_address() -> usize {
    pd_vm_native_frame_state as *const () as usize
}

pub(crate) fn enter_call_value_entry_address() -> usize {
    pd_vm_native_enter_call_value as *const () as usize
}

pub(crate) fn enter_call_value_inherited_entry_address() -> usize {
    pd_vm_native_enter_call_value_inherited as *const () as usize
}

pub(crate) fn leave_frame_entry_address() -> usize {
    pd_vm_native_leave_frame as *const () as usize
}

pub(crate) fn leave_frame_inherited_entry_address() -> usize {
    pd_vm_native_leave_frame_inherited as *const () as usize
}

pub(crate) fn active_local_base_entry_address() -> usize {
    pd_vm_native_active_local_base as *const () as usize
}

pub(crate) fn restore_active_exit_state_entry_address() -> usize {
    pd_vm_native_restore_active_exit_state as *const () as usize
}

pub(crate) fn restore_sparse_exit_state_entry_address() -> usize {
    pd_vm_native_restore_sparse_exit_state as *const () as usize
}

pub(crate) fn restore_active_sparse_exit_state_entry_address() -> usize {
    pd_vm_native_restore_active_sparse_exit_state as *const () as usize
}

pub(crate) fn restore_virtual_frame_entry_address() -> usize {
    pd_vm_native_restore_virtual_frame as *const () as usize
}

pub(crate) fn map_has_entry_address() -> usize {
    pd_vm_native_map_has as *const () as usize
}

pub(crate) fn map_get_entry_address() -> usize {
    pd_vm_native_map_get as *const () as usize
}

pub(crate) fn map_iter_next_entry_address() -> usize {
    pd_vm_native_map_iter_next as *const () as usize
}

pub(crate) fn map_iter_take_key_entry_address() -> usize {
    pd_vm_native_map_iter_take_key as *const () as usize
}

pub(crate) fn map_iter_take_value_entry_address() -> usize {
    pd_vm_native_map_iter_take_value as *const () as usize
}

pub(crate) fn collection_set_entry_address() -> usize {
    pd_vm_native_collection_set as *const () as usize
}

pub(crate) fn array_set_entry_address() -> usize {
    pd_vm_native_array_set as *const () as usize
}

pub(crate) fn map_set_entry_address() -> usize {
    pd_vm_native_map_set as *const () as usize
}

pub(crate) fn array_push_entry_address() -> usize {
    pd_vm_native_array_push as *const () as usize
}

pub(crate) fn non_yielding_host_call_entry_address() -> usize {
    pd_vm_native_non_yielding_host_call as *const () as usize
}

pub(crate) fn non_yielding_scalar_host_call_entry_address() -> usize {
    pd_vm_native_non_yielding_scalar_host_call as *const () as usize
}

pub(crate) fn non_yielding_i64_host_call_entry_address() -> usize {
    pd_vm_native_non_yielding_i64_host_call as *const () as usize
}

pub(crate) fn helper_entry_offset() -> i32 {
    i32::try_from(std::mem::offset_of!(Vm, native_helper_fn))
        .expect("Vm::native_helper_fn offset must fit i32")
}

pub(crate) fn interrupt_helper_entry_offset() -> i32 {
    i32::try_from(std::mem::offset_of!(Vm, native_interrupt_helper_fn))
        .expect("Vm::native_interrupt_helper_fn offset must fit i32")
}

pub(crate) extern "C" fn pd_vm_native_interrupt_tick(vm: *mut Vm) -> i32 {
    let Some(vm_ref) = (unsafe { vm.as_mut() }) else {
        store_bridge_error(VmError::JitNative(
            "native interrupt helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    };

    match vm_ref.charge_interrupt_tick() {
        Ok(()) => STATUS_CONTINUE,
        Err(VmError::OutOfFuel { .. } | VmError::EpochDeadlineReached { .. }) => STATUS_OUT_OF_FUEL,
        Err(err) => {
            store_bridge_error(err);
            STATUS_ERROR
        }
    }
}

pub(crate) extern "C" fn pd_vm_native_aot_call_boundary_interrupt(vm: *mut Vm) -> i32 {
    let Some(vm_ref) = (unsafe { vm.as_mut() }) else {
        store_bridge_error(VmError::JitNative(
            "native aot call-boundary interrupt helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    };

    match vm_ref.charge_aot_call_boundary_interrupt() {
        Ok(()) => STATUS_CONTINUE,
        Err(VmError::OutOfFuel { .. } | VmError::EpochDeadlineReached { .. }) => STATUS_OUT_OF_FUEL,
        Err(err) => {
            store_bridge_error(err);
            STATUS_ERROR
        }
    }
}

pub(crate) extern "C" fn pd_vm_native_alloc_byte_buffer(cap: usize) -> *mut u8 {
    let mut buffer = Vec::<u8>::with_capacity(cap);
    let ptr = buffer.as_mut_ptr();
    std::mem::forget(buffer);
    ptr
}

pub(crate) extern "C" fn pd_vm_native_alloc_value_buffer(cap: usize) -> *mut Value {
    let mut buffer = Vec::<Value>::with_capacity(cap);
    let ptr = buffer.as_mut_ptr();
    std::mem::forget(buffer);
    ptr
}

pub(crate) extern "C" fn pd_vm_native_shared_string_from_buffer(
    ptr: *mut u8,
    len: usize,
    cap: usize,
) -> *mut u8 {
    let bytes = unsafe { Vec::<u8>::from_raw_parts(ptr, len, cap) };
    let text = unsafe { String::from_utf8_unchecked(bytes) };
    arc_into_repr_ptr(Arc::new(text))
}

pub(crate) extern "C" fn pd_vm_native_shared_bytes_from_buffer(
    ptr: *mut u8,
    len: usize,
    cap: usize,
) -> *mut u8 {
    let bytes = unsafe { Vec::<u8>::from_raw_parts(ptr, len, cap) };
    arc_into_repr_ptr(Arc::new(bytes))
}

pub(crate) extern "C" fn pd_vm_native_shared_array_from_buffer(
    ptr: *mut Value,
    len: usize,
    cap: usize,
) -> *mut u8 {
    let values = unsafe { Vec::<Value>::from_raw_parts(ptr, len, cap) };
    arc_into_repr_ptr(Arc::new(values))
}

pub(crate) extern "C" fn pd_vm_native_copy_bytes(dst: *mut u8, src: *const u8, len: usize) {
    unsafe {
        std::ptr::copy_nonoverlapping(src, dst, len);
    }
}

pub(crate) extern "C" fn pd_vm_native_zero_bytes(dst: *mut u8, len: usize) {
    unsafe {
        std::ptr::write_bytes(dst, 0, len);
    }
}

unsafe fn clone_arc_from_repr_ptr<T>(ptr: *mut u8) -> Arc<T> {
    let arc = unsafe { arc_from_repr_ptr::<T>(ptr) };
    let cloned = arc.clone();
    std::mem::forget(arc);
    cloned
}

pub(crate) extern "C" fn pd_vm_native_string_contains(
    text_ptr: *mut u8,
    needle_ptr: *mut u8,
) -> i32 {
    let text = unsafe { std::mem::ManuallyDrop::new(arc_from_repr_ptr::<String>(text_ptr)) };
    let needle = unsafe { std::mem::ManuallyDrop::new(arc_from_repr_ptr::<String>(needle_ptr)) };
    i32::from(
        crate::builtins::runtime::core::builtin_string_contains_impl(
            text.as_str(),
            needle.as_str(),
        ),
    )
}

pub(crate) extern "C" fn pd_vm_native_regex_match(
    vm: *mut Vm,
    pattern_ptr: *mut u8,
    text_ptr: *mut u8,
) -> i32 {
    let Some(vm_ref) = (unsafe { vm.as_mut() }) else {
        store_bridge_error(VmError::JitNative(
            "native regex-match helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    };
    let pattern = unsafe { std::mem::ManuallyDrop::new(arc_from_repr_ptr::<String>(pattern_ptr)) };
    let text = unsafe { std::mem::ManuallyDrop::new(arc_from_repr_ptr::<String>(text_ptr)) };
    match crate::builtins::runtime::regex::native_re_match(vm_ref, pattern.as_str(), text.as_str())
    {
        Ok(matched) => i32::from(matched),
        Err(err) => {
            store_bridge_error(err);
            STATUS_ERROR
        }
    }
}

pub(crate) extern "C" fn pd_vm_native_regex_replace(
    vm: *mut Vm,
    pattern_ptr: *mut u8,
    text_ptr: *mut u8,
    replacement_ptr: *mut u8,
) -> *mut u8 {
    let Some(vm_ref) = (unsafe { vm.as_mut() }) else {
        store_bridge_error(VmError::JitNative(
            "native regex-replace helper received null vm pointer".to_string(),
        ));
        return std::ptr::null_mut();
    };
    let pattern = unsafe { std::mem::ManuallyDrop::new(arc_from_repr_ptr::<String>(pattern_ptr)) };
    let text = unsafe { std::mem::ManuallyDrop::new(arc_from_repr_ptr::<String>(text_ptr)) };
    let replacement =
        unsafe { std::mem::ManuallyDrop::new(arc_from_repr_ptr::<String>(replacement_ptr)) };
    match crate::builtins::runtime::regex::native_re_replace(
        vm_ref,
        pattern.as_str(),
        text.as_str(),
        replacement.as_str(),
    ) {
        Ok(replaced) => arc_into_repr_ptr(Arc::new(replaced)),
        Err(err) => {
            store_bridge_error(err);
            std::ptr::null_mut()
        }
    }
}

pub(crate) extern "C" fn pd_vm_native_string_replace_literal(
    text_ptr: *mut u8,
    needle_ptr: *mut u8,
    replacement_ptr: *mut u8,
) -> *mut u8 {
    let text = unsafe { std::mem::ManuallyDrop::new(arc_from_repr_ptr::<String>(text_ptr)) };
    let needle = unsafe { std::mem::ManuallyDrop::new(arc_from_repr_ptr::<String>(needle_ptr)) };
    let replacement =
        unsafe { std::mem::ManuallyDrop::new(arc_from_repr_ptr::<String>(replacement_ptr)) };
    if !needle.is_empty() && !text.contains(needle.as_str()) {
        return arc_into_repr_ptr(Arc::clone(&*text));
    }
    arc_into_repr_ptr(Arc::new(
        crate::builtins::runtime::core::builtin_string_replace_literal_impl(
            text.as_str(),
            needle.as_str(),
            replacement.as_str(),
        ),
    ))
}

pub(crate) extern "C" fn pd_vm_native_string_lower_ascii(text_ptr: *mut u8) -> *mut u8 {
    let text = unsafe { std::mem::ManuallyDrop::new(arc_from_repr_ptr::<String>(text_ptr)) };
    arc_into_repr_ptr(Arc::new(
        crate::builtins::runtime::core::builtin_string_lower_ascii_impl(text.as_str()),
    ))
}

pub(crate) extern "C" fn pd_vm_native_type_of(value_ptr: *const Value) -> *mut u8 {
    let name = match unsafe { &*value_ptr } {
        Value::Null => "null",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Bool(_) => "bool",
        Value::String(_) => "string",
        Value::Bytes(_) => "bytes",
        Value::Array(_) => "array",
        Value::Map(_) => "map",
        Value::Callable(_) => "callable",
    };
    arc_into_repr_ptr(Arc::new(name.to_string()))
}

pub(crate) extern "C" fn pd_vm_native_to_string(value_ptr: *const Value) -> *mut u8 {
    let value = unsafe { &*value_ptr };
    arc_into_repr_ptr(Arc::new(
        crate::builtins::runtime::core::builtin_to_string_impl(value),
    ))
}

pub(crate) extern "C" fn pd_vm_native_string_split_literal(
    text_ptr: *mut u8,
    delimiter_ptr: *mut u8,
) -> *mut u8 {
    let text = unsafe { std::mem::ManuallyDrop::new(arc_from_repr_ptr::<String>(text_ptr)) };
    let delimiter =
        unsafe { std::mem::ManuallyDrop::new(arc_from_repr_ptr::<String>(delimiter_ptr)) };
    arc_into_repr_ptr(Arc::new(
        crate::builtins::runtime::core::builtin_string_split_literal_impl(
            text.as_str(),
            delimiter.as_str(),
        ),
    ))
}

pub(crate) extern "C" fn pd_vm_native_clone_value_to_slot(
    dst: *mut Value,
    src: *const Value,
) -> i32 {
    if dst.is_null() || src.is_null() {
        store_bridge_error(VmError::JitNative(
            "native clone-value helper received null slot pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    unsafe {
        std::ptr::write(dst, (*src).clone());
    }
    STATUS_CONTINUE
}

pub(crate) extern "C" fn pd_vm_native_init_null_value_slot(dst: *mut Value) -> i32 {
    if dst.is_null() {
        store_bridge_error(VmError::JitNative(
            "native init-null-slot helper received null pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    unsafe {
        std::ptr::write(dst, Value::Null);
    }
    STATUS_CONTINUE
}

pub(crate) extern "C" fn pd_vm_native_clear_value_slot(dst: *mut Value) -> i32 {
    if dst.is_null() {
        store_bridge_error(VmError::JitNative(
            "native clear-slot helper received null pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    unsafe {
        let old = std::mem::replace(&mut *dst, Value::Null);
        drop(old);
    }
    STATUS_CONTINUE
}

pub(crate) extern "C" fn pd_vm_native_value_eq(lhs: *const Value, rhs: *const Value) -> i32 {
    if lhs.is_null() || rhs.is_null() {
        store_bridge_error(VmError::JitNative(
            "native value-eq helper received null pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    i32::from(unsafe { *lhs == *rhs })
}

pub(crate) extern "C" fn pd_vm_native_value_len(value: *const Value, out: *mut i64) -> i32 {
    if value.is_null() || out.is_null() {
        store_bridge_error(VmError::JitNative(
            "native value-len helper received null pointer".to_string(),
        ));
        return STATUS_ERROR;
    }
    let len = match unsafe { &*value } {
        Value::String(value) => value.chars().count(),
        Value::Bytes(value) => value.len(),
        Value::Array(value) => value.len(),
        Value::Map(value) => value.len(),
        _ => {
            store_bridge_error(VmError::TypeMismatch("string, bytes, array, or map"));
            return STATUS_ERROR;
        }
    };
    let Ok(len) = i64::try_from(len) else {
        store_bridge_error(VmError::IntegerOverflow("len"));
        return STATUS_ERROR;
    };
    unsafe { out.write(len) };
    STATUS_CONTINUE
}

pub(crate) extern "C" fn pd_vm_native_write_heap_value_to_slot(
    dst: *mut Value,
    repr_ptr: *mut u8,
    tag: i64,
) -> i32 {
    if dst.is_null() || repr_ptr.is_null() {
        store_bridge_error(VmError::JitNative(
            "native box-heap helper received null pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let value = match tag {
        x if x == ValueType::String as i64 => {
            Value::String(unsafe { clone_arc_from_repr_ptr::<String>(repr_ptr) })
        }
        x if x == ValueType::Bytes as i64 => {
            Value::Bytes(unsafe { clone_arc_from_repr_ptr::<Vec<u8>>(repr_ptr) })
        }
        x if x == ValueType::Array as i64 => {
            Value::Array(unsafe { clone_arc_from_repr_ptr::<Vec<Value>>(repr_ptr) })
        }
        x if x == ValueType::Map as i64 => {
            Value::Map(unsafe { clone_arc_from_repr_ptr::<VmMap>(repr_ptr) })
        }
        _ => {
            store_bridge_error(VmError::JitNative(format!(
                "native box-heap helper received unsupported ValueType tag {tag}"
            )));
            return STATUS_ERROR;
        }
    };

    unsafe {
        std::ptr::write(dst, value);
    }
    STATUS_CONTINUE
}

pub(crate) extern "C" fn pd_vm_native_restore_exit_state(
    vm: *mut Vm,
    stack_src: *const Value,
    stack_len: usize,
    locals_src: *const Value,
    locals_len: usize,
    ip: usize,
) -> i32 {
    run_step(vm, "restore_exit_state", |vm| {
        if locals_len != vm.locals.len() {
            return Err(VmError::JitNative(format!(
                "native exit restore locals length mismatch: expected {}, got {}",
                vm.locals.len(),
                locals_len
            )));
        }
        if stack_len != 0 && stack_src.is_null() {
            return Err(VmError::JitNative(
                "native exit restore received null stack buffer".to_string(),
            ));
        }
        if locals_len != 0 && locals_src.is_null() {
            return Err(VmError::JitNative(
                "native exit restore received null locals buffer".to_string(),
            ));
        }

        vm.clear_stack_with_drop_contract();
        vm.stack.reserve(stack_len);
        for index in 0..stack_len {
            let value = unsafe { std::ptr::read(stack_src.add(index)) };
            vm.stack.push(value);
        }

        for index in 0..locals_len {
            let local_index = u8::try_from(index).map_err(|_| {
                VmError::JitNative("native exit restore local index out of range".to_string())
            })?;
            let value = unsafe { std::ptr::read(locals_src.add(index)) };
            vm.store_local_with_drop_contract(local_index, value)?;
        }

        vm.jump_to(ip)?;
        Ok(STATUS_CONTINUE)
    })
}

fn native_frame_state(vm: &Vm) -> VmResult<NativeFrameState> {
    let frame = vm.execution_frames.last();
    let operand_stack_base = frame.map(|frame| frame.operand_stack_base).unwrap_or(0);
    let local_base = frame.map(|frame| frame.local_base).unwrap_or(0);
    let local_count = frame
        .map(|frame| frame.local_count)
        .unwrap_or(vm.locals.len());
    let active_stack_len = vm
        .stack
        .len()
        .checked_sub(operand_stack_base)
        .ok_or_else(|| {
            VmError::JitNative("native frame-state stack base exceeds stack length".to_string())
        })?;
    let frame_key = frame
        .and_then(|frame| frame.prototype_id)
        .map(u64::from)
        .unwrap_or(ROOT_FRAME_KEY);
    let continuation_kind = match frame.map(|frame| &frame.continuation) {
        Some(FrameContinuation::Halt) | None => 0,
        Some(FrameContinuation::ResumeBytecode { .. }) => 1,
        Some(FrameContinuation::ReturnToHost) => 2,
    };
    Ok(NativeFrameState {
        frame_key,
        operand_stack_base,
        active_stack_len,
        local_base,
        local_count,
        frame_depth: vm.call_depth,
        continuation_kind,
    })
}

fn write_inherited_state_packet(vm: &Vm, packet: *mut u8) -> VmResult<()> {
    use super::{
        INHERITED_STATE_ACTIVE_OFFSET, INHERITED_STATE_DYNAMIC_TARGET_OFFSET,
        INHERITED_STATE_FRAME_KEY_OFFSET, INHERITED_STATE_LOCAL_BASE_OFFSET,
        INHERITED_STATE_STACK_BASE_OFFSET, INHERITED_STATE_TARGET_IP_OFFSET,
        INHERITED_STATE_VALUE_COUNT_OFFSET, INHERITED_STATE_VALUES_OFFSET,
        MAX_INHERITED_ENTRY_VALUES,
    };

    if packet.is_null() {
        return Err(VmError::JitNative(
            "native inherited-state packet pointer is null".to_string(),
        ));
    }
    let state = native_frame_state(vm)?;
    let value_count = state
        .active_stack_len
        .checked_add(state.local_count)
        .ok_or_else(|| VmError::JitNative("native inherited value count overflow".to_string()))?;
    unsafe {
        packet
            .add(INHERITED_STATE_ACTIVE_OFFSET as usize)
            .cast::<usize>()
            .write(0);
        packet
            .add(INHERITED_STATE_DYNAMIC_TARGET_OFFSET as usize)
            .cast::<usize>()
            .write(0);
    }
    if value_count > MAX_INHERITED_ENTRY_VALUES {
        return Ok(());
    }
    unsafe {
        packet
            .add(INHERITED_STATE_FRAME_KEY_OFFSET as usize)
            .cast::<u64>()
            .write(state.frame_key);
        packet
            .add(INHERITED_STATE_STACK_BASE_OFFSET as usize)
            .cast::<usize>()
            .write(state.operand_stack_base);
        packet
            .add(INHERITED_STATE_LOCAL_BASE_OFFSET as usize)
            .cast::<usize>()
            .write(state.local_base);
        packet
            .add(INHERITED_STATE_TARGET_IP_OFFSET as usize)
            .cast::<usize>()
            .write(vm.ip);
        packet
            .add(INHERITED_STATE_VALUE_COUNT_OFFSET as usize)
            .cast::<usize>()
            .write(value_count);
        let values = packet
            .add(INHERITED_STATE_VALUES_OFFSET as usize)
            .cast::<*const Value>();
        let stack = vm.stack.as_ptr().add(state.operand_stack_base);
        for index in 0..state.active_stack_len {
            values.add(index).write(stack.add(index));
        }
        let locals = vm.locals.as_ptr().add(state.local_base);
        for index in 0..state.local_count {
            values
                .add(state.active_stack_len + index)
                .write(locals.add(index));
        }
        packet
            .add(INHERITED_STATE_DYNAMIC_TARGET_OFFSET as usize)
            .cast::<usize>()
            .write(vm.jit_native_inherited_target());
        packet
            .add(INHERITED_STATE_ACTIVE_OFFSET as usize)
            .cast::<usize>()
            .write(1);
    }
    Ok(())
}

pub(crate) extern "C" fn pd_vm_native_frame_state(vm: *mut Vm, out: *mut NativeFrameState) -> i32 {
    run_step(vm, "frame_state", |vm| {
        if out.is_null() {
            return Err(VmError::JitNative(
                "native frame-state helper received null output".to_string(),
            ));
        }
        unsafe {
            out.write(native_frame_state(vm)?);
        }
        Ok(STATUS_CONTINUE)
    })
}

fn native_enter_call_value(
    vm: &mut Vm,
    argc: i64,
    call_ip: i64,
    resume_ip: i64,
    inherited_state: *mut u8,
) -> VmResult<i32> {
    let argc = u8::try_from(argc)
        .map_err(|_| VmError::InvalidFrameState("native call argc out of range"))?;
    let call_ip = usize::try_from(call_ip)
        .map_err(|_| VmError::InvalidFrameState("native call ip out of range"))?;
    let resume_ip = usize::try_from(resume_ip)
        .map_err(|_| VmError::InvalidFrameState("native resume ip out of range"))?;
    if vm.ip != call_ip {
        vm.jump_to(call_ip)?;
    }
    if resume_ip > vm.program.code.len() {
        return Err(VmError::BytecodeBounds);
    }
    vm.ip = resume_ip;
    let status = match vm.execute_call_value(argc, Some(call_ip))? {
        ExecOutcome::Continue => STATUS_LINKED_CONTINUE,
        ExecOutcome::Halted => STATUS_HALTED,
        ExecOutcome::Yielded => STATUS_YIELDED,
        ExecOutcome::Waiting(_) => STATUS_WAITING,
    };
    if status == STATUS_LINKED_CONTINUE && !inherited_state.is_null() {
        write_inherited_state_packet(vm, inherited_state)?;
    }
    Ok(status)
}

pub(crate) extern "C" fn pd_vm_native_enter_call_value(
    vm: *mut Vm,
    argc: i64,
    call_ip: i64,
    resume_ip: i64,
) -> i32 {
    run_step(vm, "enter_call_value", |vm| {
        native_enter_call_value(vm, argc, call_ip, resume_ip, std::ptr::null_mut())
    })
}

pub(crate) extern "C" fn pd_vm_native_enter_call_value_inherited(
    vm: *mut Vm,
    argc: i64,
    call_ip: i64,
    resume_ip: i64,
    inherited_state: *mut u8,
) -> i32 {
    run_step(vm, "enter_call_value", |vm| {
        native_enter_call_value(vm, argc, call_ip, resume_ip, inherited_state)
    })
}

fn native_leave_frame(vm: &mut Vm, ret_ip: i64, inherited_state: *mut u8) -> VmResult<i32> {
    let ret_ip = usize::try_from(ret_ip)
        .map_err(|_| VmError::InvalidFrameState("native ret ip out of range"))?;
    vm.jump_to(ret_ip)?;
    let status = match vm.complete_active_frame()? {
        ExecOutcome::Continue => STATUS_LINKED_CONTINUE,
        ExecOutcome::Halted => STATUS_HALTED,
        ExecOutcome::Yielded => STATUS_YIELDED,
        ExecOutcome::Waiting(_) => STATUS_WAITING,
    };
    if status == STATUS_LINKED_CONTINUE && !inherited_state.is_null() {
        write_inherited_state_packet(vm, inherited_state)?;
    }
    Ok(status)
}

pub(crate) extern "C" fn pd_vm_native_leave_frame(vm: *mut Vm, ret_ip: i64) -> i32 {
    run_step(vm, "leave_frame", |vm| {
        native_leave_frame(vm, ret_ip, std::ptr::null_mut())
    })
}

pub(crate) extern "C" fn pd_vm_native_leave_frame_inherited(
    vm: *mut Vm,
    ret_ip: i64,
    inherited_state: *mut u8,
) -> i32 {
    run_step(vm, "leave_frame", |vm| {
        native_leave_frame(vm, ret_ip, inherited_state)
    })
}

pub(crate) extern "C" fn pd_vm_native_active_stack_base(vm: *const Vm) -> usize {
    unsafe { vm.as_ref() }
        .map(Vm::active_operand_stack_base)
        .unwrap_or(0)
}

pub(crate) extern "C" fn pd_vm_native_active_local_base(vm: *const Vm) -> usize {
    unsafe { vm.as_ref() }
        .map(Vm::active_local_base)
        .unwrap_or(0)
}

pub(crate) extern "C" fn pd_vm_native_restore_active_exit_state(
    vm: *mut Vm,
    stack_src: *const Value,
    stack_len: usize,
    locals_src: *const Value,
    locals_len: usize,
    ip: usize,
) -> i32 {
    run_step(vm, "restore_active_exit_state", |vm| {
        let stack_base = vm.active_operand_stack_base();
        let local_base = vm.active_local_base();
        let expected_locals_len = local_base
            .checked_add(locals_len)
            .ok_or_else(|| VmError::JitNative("native active local length overflow".to_string()))?;
        if expected_locals_len != vm.locals.len() {
            return Err(VmError::JitNative(format!(
                "native active exit restore locals length mismatch: expected {}, got {}",
                vm.locals.len(),
                expected_locals_len
            )));
        }
        if stack_base > vm.stack.len() {
            return Err(VmError::JitNative(format!(
                "native active stack base {stack_base} exceeds stack length {}",
                vm.stack.len()
            )));
        }
        if stack_len != 0 && stack_src.is_null() {
            return Err(VmError::JitNative(
                "native active exit restore received null stack buffer".to_string(),
            ));
        }
        if locals_len != 0 && locals_src.is_null() {
            return Err(VmError::JitNative(
                "native active exit restore received null locals buffer".to_string(),
            ));
        }

        vm.stack.truncate(stack_base);
        vm.stack.reserve(stack_len);
        for index in 0..stack_len {
            let value = unsafe { std::ptr::read(stack_src.add(index)) };
            vm.stack.push(value);
        }

        for index in 0..locals_len {
            let local_index = u8::try_from(index).map_err(|_| {
                VmError::JitNative("native active local index out of range".to_string())
            })?;
            let value = unsafe { std::ptr::read(locals_src.add(index)) };
            vm.store_local_with_drop_contract(local_index, value)?;
        }

        vm.jump_to(ip)?;
        Ok(STATUS_CONTINUE)
    })
}

pub(crate) extern "C" fn pd_vm_native_restore_sparse_exit_state(
    vm: *mut Vm,
    stack_src: *const Value,
    stack_len: usize,
    dirty_local_indices: *const u32,
    dirty_local_values: *const Value,
    dirty_local_count: usize,
    ip: usize,
) -> i32 {
    run_step(vm, "restore_sparse_exit_state", |vm| {
        if stack_len != 0 && stack_src.is_null() {
            return Err(VmError::JitNative(
                "native sparse exit restore received null stack buffer".to_string(),
            ));
        }
        if dirty_local_count != 0 && dirty_local_indices.is_null() {
            return Err(VmError::JitNative(
                "native sparse exit restore received null local index buffer".to_string(),
            ));
        }
        if dirty_local_count != 0 && dirty_local_values.is_null() {
            return Err(VmError::JitNative(
                "native sparse exit restore received null local value buffer".to_string(),
            ));
        }

        let mut validated_indices = Vec::with_capacity(dirty_local_count);
        for compact_index in 0..dirty_local_count {
            let local_index = unsafe { *dirty_local_indices.add(compact_index) };
            let local_index_usize = usize::try_from(local_index).map_err(|_| {
                VmError::JitNative(
                    "native sparse exit restore local index out of range".to_string(),
                )
            })?;
            if local_index_usize >= vm.locals.len() {
                return Err(VmError::JitNative(format!(
                    "native sparse exit restore local index {local_index} out of range for {} locals",
                    vm.locals.len()
                )));
            }
            let local_index = u8::try_from(local_index).map_err(|_| {
                VmError::JitNative(
                    "native sparse exit restore local index exceeds VM local range".to_string(),
                )
            })?;
            if validated_indices.contains(&local_index) {
                return Err(VmError::JitNative(format!(
                    "native sparse exit restore received duplicate local index {local_index}"
                )));
            }
            validated_indices.push(local_index);
        }

        vm.clear_stack_with_drop_contract();
        vm.stack.reserve(stack_len);
        for index in 0..stack_len {
            let value = unsafe { std::ptr::read(stack_src.add(index)) };
            vm.stack.push(value);
        }

        for (compact_index, local_index) in validated_indices.into_iter().enumerate() {
            let value = unsafe { std::ptr::read(dirty_local_values.add(compact_index)) };
            vm.store_local_with_drop_contract(local_index, value)?;
        }

        vm.jump_to(ip)?;
        Ok(STATUS_CONTINUE)
    })
}

pub(crate) extern "C" fn pd_vm_native_restore_active_sparse_exit_state(
    vm: *mut Vm,
    stack_src: *const Value,
    stack_len: usize,
    dirty_local_indices: *const u32,
    dirty_local_values: *const Value,
    dirty_local_count: usize,
    ip: usize,
) -> i32 {
    run_step(vm, "restore_active_sparse_exit_state", |vm| {
        if stack_len != 0 && stack_src.is_null() {
            return Err(VmError::JitNative(
                "native active sparse exit restore received null stack buffer".to_string(),
            ));
        }
        if dirty_local_count != 0 && dirty_local_indices.is_null() {
            return Err(VmError::JitNative(
                "native active sparse exit restore received null local index buffer".to_string(),
            ));
        }
        if dirty_local_count != 0 && dirty_local_values.is_null() {
            return Err(VmError::JitNative(
                "native active sparse exit restore received null local value buffer".to_string(),
            ));
        }

        // This entry point is emitted only by the native lowering pipeline. Dirty local
        // indices are compile-time metadata whose range and uniqueness are guaranteed
        // while the sparse exit metadata is built.

        let stack_base = vm.active_operand_stack_base();
        if stack_base > vm.stack.len() {
            return Err(VmError::JitNative(format!(
                "native active sparse stack base {stack_base} exceeds stack length {}",
                vm.stack.len()
            )));
        }
        vm.stack.truncate(stack_base);
        vm.stack.reserve(stack_len);
        for index in 0..stack_len {
            let value = unsafe { std::ptr::read(stack_src.add(index)) };
            vm.stack.push(value);
        }

        if vm.capture_cells.is_empty() {
            let local_base = vm.active_local_base();
            let count_drop_events = vm.drop_contract_events_enabled;
            for compact_index in 0..dirty_local_count {
                let local_index = unsafe { *dirty_local_indices.add(compact_index) } as usize;
                debug_assert!(local_index < 256);
                let absolute = local_base + local_index;
                debug_assert!(absolute < vm.locals.len());
                let value = unsafe { std::ptr::read(dirty_local_values.add(compact_index)) };
                let slot = unsafe { vm.locals.get_unchecked_mut(absolute) };
                let previous = std::mem::replace(slot, value);
                if count_drop_events {
                    vm.count_value_drop_contract(&previous);
                }
            }
        } else {
            for compact_index in 0..dirty_local_count {
                let local_index = unsafe { *dirty_local_indices.add(compact_index) };
                debug_assert!(u8::try_from(local_index).is_ok());
                let local_index = local_index as u8;
                let value = unsafe { std::ptr::read(dirty_local_values.add(compact_index)) };
                vm.store_local_with_drop_contract(local_index, value)?;
            }
        }

        if ip >= vm.program.code.len() {
            return Err(VmError::InvalidBranchTarget { target: ip });
        }
        vm.ip = ip;
        Ok(STATUS_CONTINUE)
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) extern "C" fn pd_vm_native_restore_virtual_frame(
    vm: *mut Vm,
    prototype_id: u32,
    call_ip: usize,
    return_ip: usize,
    resume_ip: usize,
    stack_src: *const Value,
    stack_len: usize,
    locals_src: *const Value,
    locals_len: usize,
) -> i32 {
    run_step(vm, "restore_virtual_frame", |vm| {
        if stack_len != 0 && stack_src.is_null() {
            return Err(VmError::JitNative(
                "virtual frame restore received null stack buffer".to_string(),
            ));
        }
        if locals_len != 0 && locals_src.is_null() {
            return Err(VmError::JitNative(
                "virtual frame restore received null locals buffer".to_string(),
            ));
        }
        if vm.call_depth >= vm.max_script_call_depth {
            return Err(VmError::CallStackOverflow {
                limit: vm.max_script_call_depth,
            });
        }
        let prototype = vm
            .program
            .callable_prototypes
            .get(prototype_id as usize)
            .ok_or(VmError::InvalidCallablePrototype(prototype_id))?;
        if prototype.kind != CallableKind::FunctionItem
            || !prototype.capture_slots.is_empty()
            || !prototype.capture_source_slots.is_empty()
            || !prototype.capture_modes.is_empty()
        {
            return Err(VmError::InvalidFrameState(
                "virtual frame prototype is not an environment-free function item",
            ));
        }
        let CallableTarget::ScriptFunction(function_id) = prototype.target else {
            return Err(VmError::InvalidFrameState(
                "virtual frame prototype is not a script function",
            ));
        };
        let function = vm
            .program
            .script_functions
            .get(function_id as usize)
            .ok_or(VmError::InvalidCallablePrototype(prototype_id))?;
        if locals_len != prototype.frame_local_count {
            return Err(VmError::InvalidFrameState(
                "virtual frame local count does not match prototype",
            ));
        }
        if call_ip.saturating_add(2) != return_ip
            || vm.program.code.get(call_ip).copied() != Some(crate::OpCode::CallValue as u8)
            || return_ip > vm.program.code.len()
            || resume_ip < function.entry_ip as usize
            || resume_ip >= function.end_ip as usize
        {
            return Err(VmError::InvalidFrameState(
                "virtual frame continuation metadata is invalid",
            ));
        }

        let operand_stack_base = vm.stack.len();
        let local_base = vm.locals.len();
        vm.stack.reserve(stack_len);
        vm.locals.reserve(locals_len);
        for index in 0..stack_len {
            vm.stack
                .push(unsafe { std::ptr::read(stack_src.add(index)) });
        }
        for index in 0..locals_len {
            vm.locals
                .push(unsafe { std::ptr::read(locals_src.add(index)) });
        }
        vm.execution_frames.push(ExecutionFrame {
            continuation: FrameContinuation::ResumeBytecode { return_ip },
            operand_stack_base,
            local_base,
            local_count: locals_len,
            prototype_id: Some(prototype_id),
        });
        vm.active_local_base_cache = local_base;
        vm.active_operand_stack_base_cache = operand_stack_base;
        vm.call_depth = vm.script_frame_depth();
        vm.ip = resume_ip;
        Ok(STATUS_CONTINUE)
    })
}

pub(crate) extern "C" fn pd_vm_native_map_has(repr_ptr: *mut u8, key: *const Value) -> i32 {
    if repr_ptr.is_null() || key.is_null() {
        store_bridge_error(VmError::JitNative(
            "native map-has helper received null pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let entries = unsafe { arc_from_repr_ptr::<VmMap>(repr_ptr) };
    let present = entries.get(unsafe { &*key }).is_some();
    std::mem::forget(entries);
    i32::from(present)
}

pub(crate) extern "C" fn pd_vm_native_map_get(
    dst: *mut Value,
    repr_ptr: *mut u8,
    key: *const Value,
) -> i32 {
    if dst.is_null() || repr_ptr.is_null() || key.is_null() {
        store_bridge_error(VmError::JitNative(
            "native map-get helper received null pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let entries = unsafe { arc_from_repr_ptr::<VmMap>(repr_ptr) };
    let Some(value) = entries.get(unsafe { &*key }) else {
        std::mem::forget(entries);
        return 0;
    };
    unsafe {
        std::ptr::write(dst, value.clone());
    }
    std::mem::forget(entries);
    1
}

pub(crate) extern "C" fn pd_vm_native_map_iter_next(vm: *mut Vm, slot: i64) -> i32 {
    if vm.is_null() || slot < 0 {
        store_bridge_error(VmError::JitNative(
            "native map-iterator next received invalid arguments".to_string(),
        ));
        return STATUS_ERROR;
    }
    match unsafe { &mut *vm }.advance_map_iterator(slot as usize) {
        Ok(has_next) => i32::from(has_next),
        Err(err) => {
            store_bridge_error(err);
            STATUS_ERROR
        }
    }
}

fn native_map_iter_take(dst: *mut Value, vm: *mut Vm, slot: i64, key: bool) -> i32 {
    if dst.is_null() || vm.is_null() || slot < 0 {
        store_bridge_error(VmError::JitNative(
            "native map-iterator take received invalid arguments".to_string(),
        ));
        return STATUS_ERROR;
    }
    let vm = unsafe { &mut *vm };
    let value = if key {
        vm.take_map_iterator_key(slot as usize)
    } else {
        vm.take_map_iterator_value(slot as usize)
    };
    let value = match value {
        Ok(value) => value,
        Err(err) => {
            store_bridge_error(err);
            return STATUS_ERROR;
        }
    };
    let replaced = unsafe { std::ptr::replace(dst, value) };
    drop(replaced);
    1
}

pub(crate) extern "C" fn pd_vm_native_map_iter_take_key(
    dst: *mut Value,
    vm: *mut Vm,
    slot: i64,
) -> i32 {
    native_map_iter_take(dst, vm, slot, true)
}

pub(crate) extern "C" fn pd_vm_native_map_iter_take_value(
    dst: *mut Value,
    vm: *mut Vm,
    slot: i64,
) -> i32 {
    native_map_iter_take(dst, vm, slot, false)
}

pub(crate) extern "C" fn pd_vm_native_collection_set(
    dst: *mut Value,
    container: *mut Value,
    key: *const Value,
    value: *const Value,
) -> i32 {
    if dst.is_null() || container.is_null() || key.is_null() || value.is_null() {
        store_bridge_error(VmError::JitNative(
            "native collection-set helper received null pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let container = unsafe { std::ptr::replace(container, Value::Null) };
    let key = unsafe { (&*key).clone() };
    let value = unsafe { (&*value).clone() };
    match crate::builtins::runtime::core::builtin_set_owned(container, key, value) {
        Ok(result) => {
            let previous = unsafe { std::ptr::replace(dst, result) };
            drop(previous);
            STATUS_CONTINUE
        }
        Err(err) => {
            store_bridge_error(err);
            STATUS_ERROR
        }
    }
}

pub(crate) extern "C" fn pd_vm_native_array_set(
    dst: *mut Value,
    array: *mut Value,
    index: i64,
    value: *const Value,
) -> i32 {
    if dst.is_null() || array.is_null() || value.is_null() {
        store_bridge_error(VmError::JitNative(
            "native array-set helper received null pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let array = unsafe { std::ptr::replace(array, Value::Null) };
    let value = unsafe { (&*value).clone() };
    let result = match array {
        Value::Array(values) => {
            crate::builtins::runtime::core::builtin_set_array_shared_impl(values, index, value)
                .map(Value::Array)
        }
        _ => Err(VmError::TypeMismatch("array")),
    };
    match result {
        Ok(result) => {
            let previous = unsafe { std::ptr::replace(dst, result) };
            drop(previous);
            STATUS_CONTINUE
        }
        Err(err) => {
            store_bridge_error(err);
            STATUS_ERROR
        }
    }
}

pub(crate) extern "C" fn pd_vm_native_map_set(
    dst: *mut Value,
    map: *mut Value,
    key: *const Value,
    value: *const Value,
) -> i32 {
    if dst.is_null() || map.is_null() || key.is_null() || value.is_null() {
        store_bridge_error(VmError::JitNative(
            "native map-set helper received null pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let map = unsafe { std::ptr::replace(map, Value::Null) };
    let key = unsafe { (&*key).clone() };
    let value = unsafe { (&*value).clone() };
    let result = match map {
        Value::Map(entries) => Ok(Value::Map(
            crate::builtins::runtime::core::builtin_set_map_shared_impl(entries, key, value),
        )),
        _ => Err(VmError::TypeMismatch("map")),
    };
    match result {
        Ok(result) => {
            let previous = unsafe { std::ptr::replace(dst, result) };
            drop(previous);
            STATUS_CONTINUE
        }
        Err(err) => {
            store_bridge_error(err);
            STATUS_ERROR
        }
    }
}

pub(crate) extern "C" fn pd_vm_native_array_push(
    dst: *mut Value,
    array: *mut Value,
    value: *const Value,
) -> i32 {
    if dst.is_null() || array.is_null() || value.is_null() {
        store_bridge_error(VmError::JitNative(
            "native array-push helper received null pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let array = unsafe { std::ptr::replace(array, Value::Null) };
    let value = unsafe { (&*value).clone() };
    let result = match array {
        Value::Array(values) => Ok(Value::Array(
            crate::builtins::runtime::core::builtin_array_push_shared_impl(values, value),
        )),
        _ => Err(VmError::TypeMismatch("array")),
    };
    match result {
        Ok(result) => {
            let previous = unsafe { std::ptr::replace(dst, result) };
            drop(previous);
            STATUS_CONTINUE
        }
        Err(err) => {
            store_bridge_error(err);
            STATUS_ERROR
        }
    }
}

pub(crate) extern "C" fn pd_vm_native_non_yielding_host_call(
    vm: *mut Vm,
    import: usize,
    args: *const Value,
    argc: usize,
    out: *mut Value,
) -> i32 {
    const MAX_ARGS: usize = u8::MAX as usize;
    let Some(vm) = (unsafe { vm.as_mut() }) else {
        store_bridge_error(VmError::JitNative(
            "native host-call helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    };
    if out.is_null() || (argc != 0 && args.is_null()) || argc > MAX_ARGS {
        store_bridge_error(VmError::JitNative(
            "native host-call helper received invalid argument storage".to_string(),
        ));
        return STATUS_ERROR;
    }
    let args = unsafe { std::slice::from_raw_parts(args, argc) };
    let expected_return_type = vm
        .program
        .imports
        .get(import)
        .map(|host_import| host_import.return_type);
    match call_non_yielding_host_value(vm, import, args, expected_return_type) {
        Ok(value) => {
            unsafe { std::ptr::write(out, value) };
            STATUS_CONTINUE
        }
        Err(err) => {
            store_bridge_error(err);
            STATUS_ERROR
        }
    }
}

fn call_non_yielding_host_value(
    vm: &mut Vm,
    import: usize,
    args: &[Value],
    expected_return_type: Option<ValueType>,
) -> VmResult<Value> {
    let resolved = *vm
        .resolved_calls
        .get(import)
        .ok_or(VmError::InvalidCall(import as u16))?;
    let function = match vm.host_functions.get(usize::from(resolved)) {
        Some(VmHostFunction::ArgsStaticNonYielding(function)) => *function,
        _ => {
            return Err(VmError::JitNative(
                "native host-call binding changed after trace compilation".to_string(),
            ));
        }
    };
    vm.call_depth = vm.call_depth.saturating_add(1);
    let outcome = function(args);
    vm.call_depth = vm.call_depth.saturating_sub(1);
    outcome
        .and_then(crate::vm::host::require_non_yielding_host_value)
        .and_then(|value| {
            crate::vm::host::validate_non_yielding_host_value(value, expected_return_type)
        })
}

fn scalar_host_return_type(return_type: i64) -> VmResult<ValueType> {
    if return_type == ValueType::Int as i64 {
        Ok(ValueType::Int)
    } else if return_type == ValueType::Float as i64 {
        Ok(ValueType::Float)
    } else if return_type == ValueType::Bool as i64 {
        Ok(ValueType::Bool)
    } else {
        Err(VmError::JitNative(
            "native scalar host-call return type is unsupported".to_string(),
        ))
    }
}

fn store_scalar_host_result(value: Value, return_type: i64, out: *mut u64) -> VmResult<()> {
    let bits = match (return_type, value) {
        (tag, Value::Int(value)) if tag == ValueType::Int as i64 => value as u64,
        (tag, Value::Float(value)) if tag == ValueType::Float as i64 => value.to_bits(),
        (tag, Value::Bool(value)) if tag == ValueType::Bool as i64 => u64::from(value),
        (tag, _) if tag == ValueType::Int as i64 => return Err(VmError::TypeMismatch("int")),
        (tag, _) if tag == ValueType::Float as i64 => {
            return Err(VmError::TypeMismatch("float"));
        }
        (tag, _) if tag == ValueType::Bool as i64 => return Err(VmError::TypeMismatch("bool")),
        _ => {
            return Err(VmError::JitNative(
                "native scalar host-call return type is unsupported".to_string(),
            ));
        }
    };
    unsafe { out.write(bits) };
    Ok(())
}

pub(crate) extern "C" fn pd_vm_native_non_yielding_scalar_host_call(
    vm: *mut Vm,
    import: usize,
    args: *const Value,
    argc: usize,
    return_type: i64,
    out: *mut u64,
) -> i32 {
    let Some(vm) = (unsafe { vm.as_mut() }) else {
        store_bridge_error(VmError::JitNative(
            "native scalar host-call helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    };
    if out.is_null() || (argc != 0 && args.is_null()) || argc > u8::MAX as usize {
        store_bridge_error(VmError::JitNative(
            "native scalar host-call helper received invalid storage".to_string(),
        ));
        return STATUS_ERROR;
    }
    let args = unsafe { std::slice::from_raw_parts(args, argc) };
    match scalar_host_return_type(return_type)
        .and_then(|expected| call_non_yielding_host_value(vm, import, args, Some(expected)))
        .and_then(|value| store_scalar_host_result(value, return_type, out))
    {
        Ok(()) => STATUS_CONTINUE,
        Err(err) => {
            store_bridge_error(err);
            STATUS_ERROR
        }
    }
}

pub(crate) extern "C" fn pd_vm_native_non_yielding_i64_host_call(
    vm: *mut Vm,
    import: usize,
    arg0: i64,
    arg1: i64,
    argc: usize,
    return_type: i64,
    out: *mut u64,
) -> i32 {
    let Some(vm) = (unsafe { vm.as_mut() }) else {
        store_bridge_error(VmError::JitNative(
            "native i64 host-call helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    };
    if out.is_null() || argc > 2 {
        store_bridge_error(VmError::JitNative(
            "native i64 host-call helper received invalid storage".to_string(),
        ));
        return STATUS_ERROR;
    }
    let storage = [Value::Int(arg0), Value::Int(arg1)];
    match scalar_host_return_type(return_type)
        .and_then(|expected| {
            call_non_yielding_host_value(vm, import, &storage[..argc], Some(expected))
        })
        .and_then(|value| store_scalar_host_result(value, return_type, out))
    {
        Ok(()) => STATUS_CONTINUE,
        Err(err) => {
            store_bridge_error(err);
            STATUS_ERROR
        }
    }
}

pub(crate) extern "C" fn pd_vm_native_step(vm: *mut Vm, op: i64, a: i64, b: i64, c: i64) -> i32 {
    run_step(vm, "step", |vm| {
        if op == OP_BUILTIN_CALL {
            let bridge_name = u16::try_from(a)
                .ok()
                .and_then(BuiltinFunction::from_call_index)
                .map(BuiltinFunction::name)
                .unwrap_or("builtin_call");
            vm.record_native_bridge_hit(bridge_name);
        } else if let Some(name) = bridge_name_for_op(op) {
            vm.record_native_bridge_hit(name);
        }

        match op {
            OP_LDC => {
                let index = u32::try_from(a)
                    .map_err(|_| VmError::JitNative("ldc index out of range".to_string()))?;
                let value = vm
                    .program
                    .constants
                    .get(index as usize)
                    .cloned()
                    .ok_or(VmError::InvalidConstant(index))?;
                vm.stack.push(value);
                Ok(STATUS_CONTINUE)
            }
            OP_ADD => {
                vm.binary_add_op()?;
                Ok(STATUS_CONTINUE)
            }
            OP_SUB => {
                vm.binary_numeric_op(
                    |lhs, rhs| Ok(lhs.wrapping_sub(rhs)),
                    |lhs, rhs| Ok(lhs - rhs),
                )?;
                Ok(STATUS_CONTINUE)
            }
            OP_MUL => {
                vm.binary_numeric_op(
                    |lhs, rhs| Ok(lhs.wrapping_mul(rhs)),
                    |lhs, rhs| Ok(lhs * rhs),
                )?;
                Ok(STATUS_CONTINUE)
            }
            OP_DIV => {
                vm.binary_numeric_op(crate::vm::checked_int_div, |lhs, rhs| Ok(lhs / rhs))?;
                Ok(STATUS_CONTINUE)
            }
            OP_MOD => {
                vm.binary_numeric_op(crate::vm::checked_int_rem, |lhs, rhs| Ok(lhs % rhs))?;
                Ok(STATUS_CONTINUE)
            }
            OP_SHL => {
                let rhs = vm.pop_shift_amount()?;
                let lhs = vm.pop_int()?;
                vm.stack
                    .push(crate::bytecode::Value::Int(lhs.wrapping_shl(rhs)));
                Ok(STATUS_CONTINUE)
            }
            OP_SHR => {
                let rhs = vm.pop_shift_amount()?;
                let lhs = vm.pop_int()?;
                vm.stack
                    .push(crate::bytecode::Value::Int(lhs.wrapping_shr(rhs)));
                Ok(STATUS_CONTINUE)
            }
            OP_LSHR => {
                let rhs = vm.pop_shift_amount()?;
                let lhs = vm.pop_int()?;
                vm.stack
                    .push(crate::bytecode::Value::Int(logical_shr_i64(lhs, rhs)));
                Ok(STATUS_CONTINUE)
            }
            OP_AND => {
                let rhs = vm.pop_bool()?;
                let lhs = vm.pop_bool()?;
                vm.stack.push(crate::bytecode::Value::Bool(lhs && rhs));
                Ok(STATUS_CONTINUE)
            }
            OP_OR => {
                let rhs = vm.pop_bool()?;
                let lhs = vm.pop_bool()?;
                vm.stack.push(crate::bytecode::Value::Bool(lhs || rhs));
                Ok(STATUS_CONTINUE)
            }
            OP_NOT => {
                vm.unary_not_op()?;
                Ok(STATUS_CONTINUE)
            }
            OP_NEG => {
                let value = vm.pop_numeric()?;
                match value {
                    NumericValue::Int(value) => vm
                        .stack
                        .push(crate::bytecode::Value::Int(value.wrapping_neg())),
                    NumericValue::Float(value) => {
                        vm.stack.push(crate::bytecode::Value::Float(-value))
                    }
                }
                Ok(STATUS_CONTINUE)
            }
            OP_CEQ => {
                let rhs = vm.pop_value()?;
                let lhs = vm.pop_value()?;
                vm.stack.push(crate::bytecode::Value::Bool(lhs == rhs));
                Ok(STATUS_CONTINUE)
            }
            OP_CLT => {
                vm.compare_numeric_op(|lhs, rhs| lhs < rhs, |lhs, rhs| lhs < rhs)?;
                Ok(STATUS_CONTINUE)
            }
            OP_CGT => {
                vm.compare_numeric_op(|lhs, rhs| lhs > rhs, |lhs, rhs| lhs > rhs)?;
                Ok(STATUS_CONTINUE)
            }
            OP_POP => {
                vm.pop_value()?;
                Ok(STATUS_CONTINUE)
            }
            OP_DUP => {
                let value = vm.peek_value()?.clone();
                vm.stack.push(value);
                Ok(STATUS_CONTINUE)
            }
            OP_LDLOC => {
                let index = u8::try_from(a)
                    .map_err(|_| VmError::JitNative("ldloc index out of range".to_string()))?;
                let value = vm
                    .locals
                    .get(index as usize)
                    .cloned()
                    .ok_or(VmError::InvalidLocal(index))?;
                vm.stack.push(value);
                Ok(STATUS_CONTINUE)
            }
            OP_STLOC => {
                let index = u8::try_from(a)
                    .map_err(|_| VmError::JitNative("stloc index out of range".to_string()))?;
                let value = vm.pop_value()?;
                vm.store_local_with_drop_contract(index, value)?;
                Ok(STATUS_CONTINUE)
            }
            OP_CALL => {
                let index = u16::try_from(a)
                    .map_err(|_| VmError::JitNative("call index out of range".to_string()))?;
                let argc = u8::try_from(b)
                    .map_err(|_| VmError::JitNative("call argc out of range".to_string()))?;
                let call_ip = usize::try_from(c)
                    .map_err(|_| VmError::JitNative("call ip out of range".to_string()))?;
                match vm.execute_host_call(index, argc, call_ip)? {
                    HostCallExecOutcome::Returned => Ok(STATUS_CONTINUE),
                    HostCallExecOutcome::Halted => Ok(STATUS_HALTED),
                    HostCallExecOutcome::Yielded => Ok(STATUS_YIELDED),
                    HostCallExecOutcome::Pending(_) => Ok(STATUS_WAITING),
                }
            }
            OP_BUILTIN_CALL => {
                let index = u16::try_from(a).map_err(|_| {
                    VmError::JitNative("builtin call index out of range".to_string())
                })?;
                let argc = u8::try_from(b).map_err(|_| {
                    VmError::JitNative("builtin call argc out of range".to_string())
                })?;
                let call_ip = usize::try_from(c)
                    .map_err(|_| VmError::JitNative("builtin call ip out of range".to_string()))?;
                match vm.execute_host_call(index, argc, call_ip)? {
                    HostCallExecOutcome::Returned => Ok(STATUS_CONTINUE),
                    HostCallExecOutcome::Halted => Ok(STATUS_HALTED),
                    HostCallExecOutcome::Yielded => Ok(STATUS_YIELDED),
                    HostCallExecOutcome::Pending(_) => Ok(STATUS_WAITING),
                }
            }
            OP_GUARD_FALSE => {
                let exit_ip = usize::try_from(a)
                    .map_err(|_| VmError::JitNative("guard exit ip out of range".to_string()))?;
                let condition = vm.pop_bool()?;
                if !condition {
                    vm.jump_to(exit_ip)?;
                    return Ok(STATUS_TRACE_EXIT);
                }
                Ok(STATUS_CONTINUE)
            }
            OP_GUARD_TRUE => {
                let exit_ip = usize::try_from(a)
                    .map_err(|_| VmError::JitNative("guard exit ip out of range".to_string()))?;
                let condition = vm.pop_bool()?;
                if condition {
                    vm.jump_to(exit_ip)?;
                    return Ok(STATUS_TRACE_EXIT);
                }
                Ok(STATUS_CONTINUE)
            }
            OP_LOOP_IF_FALSE => {
                let exit_ip = usize::try_from(a)
                    .map_err(|_| VmError::JitNative("guard exit ip out of range".to_string()))?;
                let condition = vm.pop_bool()?;
                if condition {
                    vm.jump_to(exit_ip)?;
                    return Ok(STATUS_TRACE_EXIT);
                }
                Ok(STATUS_CONTINUE)
            }
            OP_JUMP => {
                let target_ip = usize::try_from(a)
                    .map_err(|_| VmError::JitNative("jump target out of range".to_string()))?;
                vm.jump_to(target_ip)?;
                Ok(STATUS_TRACE_EXIT)
            }
            _ => Err(VmError::JitNative(format!(
                "native step helper received unsupported op id {op}"
            ))),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CallablePrototype, FunctionRegion, Program, RootCallableBinding, ScriptFunction};
    use std::mem::{ManuallyDrop, MaybeUninit};

    fn virtual_frame_program() -> Program {
        Program::new(
            Vec::new(),
            vec![crate::OpCode::CallValue as u8, 0, crate::OpCode::Ret as u8],
        )
        .with_callable_metadata(
            vec![ScriptFunction {
                entry_ip: 2,
                end_ip: 3,
            }],
            vec![CallablePrototype {
                kind: CallableKind::FunctionItem,
                target: CallableTarget::ScriptFunction(0),
                arity: 0,
                frame_local_count: 1,
                parameter_slots: Vec::new(),
                capture_source_slots: Vec::new(),
                capture_slots: Vec::new(),
                capture_modes: Vec::new(),
                self_slot: None,
                schema: None,
            }],
            vec![FunctionRegion {
                start_ip: 2,
                end_ip: 3,
                prototype_id: Some(0),
            }],
            vec![RootCallableBinding {
                local_slot: 0,
                prototype_id: 0,
            }],
        )
    }

    #[test]
    fn virtual_frame_restore_is_atomic_for_invalid_metadata() {
        let mut vm = Vm::new(virtual_frame_program());
        let locals = [Value::Int(7)];
        let before = (
            vm.ip,
            vm.stack.len(),
            vm.locals.len(),
            vm.execution_frames.len(),
            vm.call_depth,
        );
        let status = pd_vm_native_restore_virtual_frame(
            &mut vm,
            99,
            0,
            2,
            2,
            std::ptr::null(),
            0,
            locals.as_ptr(),
            locals.len(),
        );
        assert_eq!(status, STATUS_ERROR);
        assert_eq!(
            before,
            (
                vm.ip,
                vm.stack.len(),
                vm.locals.len(),
                vm.execution_frames.len(),
                vm.call_depth,
            )
        );
        let _ = take_bridge_error();
    }

    #[test]
    fn virtual_frame_restore_builds_script_frame_from_materialized_values() {
        let mut vm = Vm::new(virtual_frame_program());
        let mut locals = ManuallyDrop::new(vec![Value::Int(7)]);
        let status = pd_vm_native_restore_virtual_frame(
            &mut vm,
            0,
            0,
            2,
            2,
            std::ptr::null(),
            0,
            locals.as_mut_ptr(),
            locals.len(),
        );
        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(vm.ip, 2);
        assert_eq!(vm.call_depth, 1);
        assert_eq!(vm.execution_frames.len(), 2);
        assert_eq!(vm.locals.last(), Some(&Value::Int(7)));
        let frame = vm.execution_frames.last().unwrap();
        assert_eq!(frame.prototype_id, Some(0));
        assert_eq!(frame.local_count, 1);
        assert_eq!(
            frame.continuation,
            FrameContinuation::ResumeBytecode { return_ip: 2 }
        );
    }

    #[test]
    fn jit_trace_exit_status_round_trips_reserved_range_boundaries() {
        for exit_id in [0, 1, 7, 255, STATUS_JIT_TRACE_EXIT_MAX_ID] {
            let status = encode_jit_trace_exit_status(exit_id).unwrap();
            assert_eq!(decode_jit_trace_exit_status(status), Some(exit_id));
            assert_ne!(status, STATUS_TRACE_EXIT);
            assert_ne!(status, STATUS_YIELDED);
            assert_ne!(status, STATUS_WAITING);
            assert_ne!(status, STATUS_OUT_OF_FUEL);
            assert_ne!(status, STATUS_LINKED_CONTINUE);
        }
        assert_eq!(
            encode_jit_trace_exit_status(STATUS_JIT_TRACE_EXIT_MAX_ID + 1),
            None
        );
        assert_eq!(decode_jit_trace_exit_status(STATUS_TRACE_EXIT), None);
        assert_eq!(decode_jit_trace_exit_status(STATUS_ERROR), None);
        assert_eq!(
            decode_jit_trace_exit_status(STATUS_JIT_TRACE_EXIT_MAX + 1),
            None
        );
        assert_eq!(decode_jit_trace_exit_status(i32::MAX), None);
    }

    #[test]
    fn native_frame_state_and_active_restore_are_frame_relative() {
        let program =
            crate::Program::new(Vec::new(), vec![crate::OpCode::Ret as u8]).with_local_count(2);
        let mut vm = Vm::new(program);
        vm.stack = vec![Value::Int(10), Value::Int(20)];
        vm.locals = vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
            Value::Int(4),
            Value::Int(5),
        ];
        vm.execution_frames.push(crate::vm::ExecutionFrame {
            continuation: FrameContinuation::ResumeBytecode { return_ip: 0 },
            operand_stack_base: 1,
            local_base: 2,
            local_count: 3,
            prototype_id: Some(7),
        });
        vm.active_local_base_cache = 2;
        vm.active_operand_stack_base_cache = 1;
        vm.call_depth = 1;

        let mut state = MaybeUninit::<NativeFrameState>::uninit();
        assert_eq!(
            pd_vm_native_frame_state(&mut vm, state.as_mut_ptr()),
            STATUS_CONTINUE
        );
        let state = unsafe { state.assume_init() };
        assert_eq!(
            state,
            NativeFrameState {
                frame_key: 7,
                operand_stack_base: 1,
                active_stack_len: 1,
                local_base: 2,
                local_count: 3,
                frame_depth: 1,
                continuation_kind: 1,
            }
        );

        let stack = [Value::Int(99)];
        let locals = [Value::Int(30), Value::Int(40), Value::Int(50)];
        assert_eq!(
            pd_vm_native_restore_active_exit_state(
                &mut vm,
                stack.as_ptr(),
                stack.len(),
                locals.as_ptr(),
                locals.len(),
                0,
            ),
            STATUS_CONTINUE
        );
        std::mem::forget(stack);
        std::mem::forget(locals);
        assert_eq!(vm.stack, vec![Value::Int(10), Value::Int(99)]);
        assert_eq!(
            vm.locals,
            vec![
                Value::Int(1),
                Value::Int(2),
                Value::Int(30),
                Value::Int(40),
                Value::Int(50),
            ]
        );

        let sparse_stack = [Value::Int(77), Value::Int(88)];
        let dirty_indices = [1_u32];
        let dirty_values = [Value::Int(44)];
        assert_eq!(
            pd_vm_native_restore_active_sparse_exit_state(
                &mut vm,
                sparse_stack.as_ptr(),
                sparse_stack.len(),
                dirty_indices.as_ptr(),
                dirty_values.as_ptr(),
                dirty_indices.len(),
                0,
            ),
            STATUS_CONTINUE
        );
        std::mem::forget(sparse_stack);
        std::mem::forget(dirty_values);
        assert_eq!(
            vm.stack,
            vec![Value::Int(10), Value::Int(77), Value::Int(88)]
        );
        assert_eq!(
            vm.locals,
            vec![
                Value::Int(1),
                Value::Int(2),
                Value::Int(30),
                Value::Int(44),
                Value::Int(50),
            ]
        );
    }

    #[test]
    fn native_callable_helpers_create_enter_and_leave_frames() {
        let compiled = crate::compile_source_for_repl(
            r#"
                let delta = 1;
                let function = |value| value + delta;
                function(41);
            "#,
        )
        .expect("closure source should compile");
        let call_ip = compiled
            .program
            .code
            .iter()
            .position(|byte| *byte == crate::OpCode::CallValue as u8)
            .expect("callvalue opcode");
        let resume_ip = call_ip + 2;
        let prototype = compiled.program.callable_prototypes[0].clone();
        let function = match prototype.target {
            crate::CallableTarget::ScriptFunction(function_id) => {
                compiled.program.script_functions[function_id as usize].clone()
            }
            _ => panic!("expected script target"),
        };
        let ret_ip = function.end_ip as usize - 1;
        let mut vm = Vm::new(compiled.program);

        let callable = vm
            .bind_callable_value(0, vec![Value::Int(1)])
            .expect("bind callable");
        assert!(matches!(callable, Value::Callable(_)));

        vm.stack.extend([callable, Value::Int(41)]);
        assert_eq!(
            pd_vm_native_enter_call_value(&mut vm, 1, call_ip as i64, resume_ip as i64,),
            STATUS_LINKED_CONTINUE
        );
        assert_eq!(vm.call_depth, 1);
        assert_eq!(vm.ip, function.entry_ip as usize);

        vm.stack.push(Value::Int(42));
        assert_eq!(
            pd_vm_native_leave_frame(&mut vm, ret_ip as i64),
            STATUS_LINKED_CONTINUE
        );
        assert_eq!(vm.call_depth, 0);
        assert_eq!(vm.ip, resume_ip);
        assert_eq!(vm.stack, vec![Value::Int(42)]);
    }

    #[test]
    fn bridge_errors_are_isolated_between_threads() {
        clear_bridge_error();
        store_bridge_error(VmError::HostError("main thread".to_string()));

        std::thread::spawn(|| {
            assert!(take_bridge_error().is_none());
            store_bridge_error(VmError::HostError("worker thread".to_string()));
            assert!(matches!(
                take_bridge_error(),
                Some(VmError::HostError(detail)) if detail == "worker thread"
            ));
        })
        .join()
        .expect("worker should finish");

        assert!(matches!(
            take_bridge_error(),
            Some(VmError::HostError(detail)) if detail == "main thread"
        ));
    }

    #[test]
    fn sparse_exit_restore_accepts_null_buffers_for_zero_dirty_locals() {
        let preserved = Arc::new("preserved".to_string());
        let program =
            crate::Program::new(Vec::new(), vec![crate::OpCode::Ret as u8]).with_local_count(2);
        let mut vm = Vm::new(program);
        vm.set_local(0, Value::Int(17)).expect("scalar local");
        vm.set_local(1, Value::String(preserved.clone()))
            .expect("heap local");
        vm.stack.push(Value::Int(99));

        let status = pd_vm_native_restore_sparse_exit_state(
            &mut vm,
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
            0,
            0,
        );

        assert_eq!(status, STATUS_CONTINUE);
        assert!(vm.stack().is_empty());
        assert_eq!(vm.locals()[0], Value::Int(17));
        let Value::String(local) = &vm.locals()[1] else {
            panic!("expected preserved heap local");
        };
        assert!(Arc::ptr_eq(local, &preserved));
    }

    #[test]
    fn sparse_exit_restore_rejects_invalid_dirty_buffers_before_mutation() {
        clear_bridge_error();
        let program =
            crate::Program::new(Vec::new(), vec![crate::OpCode::Ret as u8]).with_local_count(1);
        let mut vm = Vm::new(program);
        vm.set_local(0, Value::Int(17)).expect("initial local");
        vm.stack.push(Value::Int(23));
        let local_value = Value::Int(99);

        let null_indices = pd_vm_native_restore_sparse_exit_state(
            &mut vm,
            std::ptr::null(),
            0,
            std::ptr::null(),
            &local_value,
            1,
            0,
        );
        assert_eq!(null_indices, STATUS_ERROR);
        assert_eq!(vm.stack(), &[Value::Int(23)]);
        assert_eq!(vm.locals(), &[Value::Int(17)]);
        assert!(take_bridge_error().is_some());

        let invalid_index = [u32::from(u8::MAX) + 1];
        let out_of_range = pd_vm_native_restore_sparse_exit_state(
            &mut vm,
            std::ptr::null(),
            0,
            invalid_index.as_ptr(),
            &local_value,
            1,
            0,
        );
        assert_eq!(out_of_range, STATUS_ERROR);
        assert_eq!(vm.stack(), &[Value::Int(23)]);
        assert_eq!(vm.locals(), &[Value::Int(17)]);
        assert!(take_bridge_error().is_some());
    }

    #[test]
    fn sparse_exit_restore_rejects_duplicate_local_indices_before_mutation() {
        clear_bridge_error();
        let program =
            crate::Program::new(Vec::new(), vec![crate::OpCode::Ret as u8]).with_local_count(2);
        let mut vm = Vm::new(program);
        vm.set_local(0, Value::Int(17)).expect("initial local");
        let local_indices = [0_u32, 0_u32];
        let local_values = [Value::Int(98), Value::Int(99)];

        let status = pd_vm_native_restore_sparse_exit_state(
            &mut vm,
            std::ptr::null(),
            0,
            local_indices.as_ptr(),
            local_values.as_ptr(),
            2,
            0,
        );

        assert_eq!(status, STATUS_ERROR);
        assert_eq!(vm.locals(), &[Value::Int(17), Value::Null]);
        assert!(take_bridge_error().is_some());
    }

    #[test]
    fn sparse_exit_restore_moves_dirty_values_and_drops_replaced_owners_once() {
        let old = Arc::new("old".to_string());
        let replacement = Arc::new("replacement".to_string());
        let program =
            crate::Program::new(Vec::new(), vec![crate::OpCode::Ret as u8]).with_local_count(2);
        let mut vm = Vm::new(program);
        vm.set_drop_contract_events_enabled(true);
        vm.set_local(0, Value::Int(1)).expect("old scalar local");
        vm.set_local(1, Value::String(old.clone()))
            .expect("old heap local");
        let stack = ManuallyDrop::new([Value::Bool(true)]);
        let local_indices = [0_u32, 1_u32];
        let local_values = ManuallyDrop::new([Value::Int(9), Value::String(replacement.clone())]);

        let status = pd_vm_native_restore_sparse_exit_state(
            &mut vm,
            stack.as_ptr(),
            stack.len(),
            local_indices.as_ptr(),
            local_values.as_ptr(),
            local_indices.len(),
            0,
        );

        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(vm.stack(), &[Value::Bool(true)]);
        assert_eq!(vm.locals()[0], Value::Int(9));
        let Value::String(local) = &vm.locals()[1] else {
            panic!("expected replacement heap local");
        };
        assert!(Arc::ptr_eq(local, &replacement));
        assert_eq!(Arc::strong_count(&old), 1);
        assert_eq!(Arc::strong_count(&replacement), 2);
        assert_eq!(vm.drop_contract_event_count(), 2);
    }

    #[test]
    fn typed_array_set_detaches_shared_input() {
        let shared = Arc::new(vec![Value::Int(10), Value::Int(20)]);
        let alias = shared.clone();
        let mut input = Value::Array(shared);
        let replacement = Value::Int(99);
        let mut output = Value::Null;

        let status = pd_vm_native_array_set(&mut output, &mut input, 1, &replacement);

        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(input, Value::Null);
        assert_eq!(alias.as_slice(), &[Value::Int(10), Value::Int(20)]);
        let Value::Array(result) = output else {
            panic!("expected array result");
        };
        assert_eq!(result.as_slice(), &[Value::Int(10), Value::Int(99)]);
        assert!(!Arc::ptr_eq(&result, &alias));
    }

    #[test]
    fn typed_array_push_detaches_shared_input() {
        let shared = Arc::new(vec![Value::Int(10)]);
        let alias = shared.clone();
        let mut input = Value::Array(shared);
        let appended = Value::Int(20);
        let mut output = Value::Null;

        let status = pd_vm_native_array_push(&mut output, &mut input, &appended);

        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(input, Value::Null);
        assert_eq!(alias.as_slice(), &[Value::Int(10)]);
        let Value::Array(result) = output else {
            panic!("expected array result");
        };
        assert_eq!(result.as_slice(), &[Value::Int(10), Value::Int(20)]);
        assert!(!Arc::ptr_eq(&result, &alias));
    }

    #[test]
    fn typed_map_set_detaches_shared_input() {
        let shared = Arc::new(VmMap::from(vec![(Value::Int(1), Value::Int(10))]));
        let alias = shared.clone();
        let mut input = Value::Map(shared);
        let key = Value::Int(2);
        let value = Value::Int(20);
        let mut output = Value::Null;

        let status = pd_vm_native_map_set(&mut output, &mut input, &key, &value);

        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(input, Value::Null);
        assert_eq!(alias.get(&Value::Int(1)), Some(&Value::Int(10)));
        assert_eq!(alias.get(&Value::Int(2)), None);
        let Value::Map(result) = output else {
            panic!("expected map result");
        };
        assert_eq!(result.get(&Value::Int(1)), Some(&Value::Int(10)));
        assert_eq!(result.get(&Value::Int(2)), Some(&Value::Int(20)));
        assert!(!Arc::ptr_eq(&result, &alias));
    }
}
