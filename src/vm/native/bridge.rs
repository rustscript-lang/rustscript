#![allow(dead_code)]
use crate::builtins::BuiltinFunction;
use crate::bytecode::{Value, ValueType, VmMap};
use crate::vm::{
    CallOutcome, CallReturn, HostCallExecOutcome, NumericValue, Vm, VmError, VmHostFunction,
    VmResult, logical_shr_i64,
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

fn run_step<F>(vm: *mut Vm, helper_name: &str, f: F) -> i32
where
    F: FnOnce(&mut Vm) -> VmResult<i32>,
{
    let Some(vm_ref) = (unsafe { vm.as_mut() }) else {
        store_bridge_error(VmError::JitNative(format!(
            "native {helper_name} helper received null vm pointer"
        )));
        return STATUS_ERROR;
    };

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

pub(crate) fn string_replace_literal_many_entry_address() -> usize {
    pd_vm_native_string_replace_literal_many as *const () as usize
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

pub(crate) fn restore_sparse_exit_state_entry_address() -> usize {
    pd_vm_native_restore_sparse_exit_state as *const () as usize
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

pub(crate) extern "C" fn pd_vm_native_string_replace_literal_many(
    text_ptr: *mut u8, needles_ptr: *mut u8, replacements_ptr: *mut u8,
) -> *mut u8 {
    let text = unsafe { std::mem::ManuallyDrop::new(arc_from_repr_ptr::<String>(text_ptr)) };
    let needles = unsafe { std::mem::ManuallyDrop::new(arc_from_repr_ptr::<Vec<Value>>(needles_ptr)) };
    let replacements = unsafe { std::mem::ManuallyDrop::new(arc_from_repr_ptr::<Vec<Value>>(replacements_ptr)) };
    match crate::builtins::runtime::core::builtin_string_replace_literal_many_impl(
        text.as_str(), needles.as_slice(), replacements.as_slice(),
    ) {
        Ok(value) => arc_into_repr_ptr(Arc::new(value)),
        Err(error) => { store_bridge_error(error); std::ptr::null_mut() }
    }
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
    let Some(&resolved) = vm.resolved_calls.get(import) else {
        store_bridge_error(VmError::InvalidCall(import as u16));
        return STATUS_ERROR;
    };
    let Some(VmHostFunction::ArgsStaticNonYielding(function)) =
        vm.host_functions.get(usize::from(resolved))
    else {
        store_bridge_error(VmError::JitNative(
            "native host-call binding changed after trace compilation".to_string(),
        ));
        return STATUS_ERROR;
    };

    let args = unsafe { std::slice::from_raw_parts(args, argc) };
    vm.call_depth = vm.call_depth.saturating_add(1);
    let outcome = function(args);
    vm.call_depth = vm.call_depth.saturating_sub(1);
    match outcome.and_then(crate::vm::host::require_non_yielding_host_value) {
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
    use std::mem::ManuallyDrop;

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
