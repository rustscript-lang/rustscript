mod bridge;
mod codegen;
mod exec;
mod layout;
mod offsets;

pub(crate) use bridge::{
    NativeInterruptMode, NativeInterruptSettings, OP_BUILTIN_CALL, OP_CALL, STATUS_CONTINUE,
    STATUS_ERROR, STATUS_HALTED, STATUS_LINKED_CONTINUE, STATUS_OUT_OF_FUEL, STATUS_TRACE_EXIT,
    STATUS_WAITING, STATUS_YIELDED, alloc_byte_buffer_entry_address,
    alloc_value_buffer_entry_address, aot_call_boundary_interrupt_entry_address,
    clear_bridge_error, clear_value_slot_entry_address, clone_value_to_slot_entry_address,
    copy_bytes_entry_address, helper_entry_address, helper_entry_offset,
    init_null_value_slot_entry_address, interrupt_helper_entry_address,
    interrupt_helper_entry_offset, map_get_entry_address, map_has_entry_address,
    restore_exit_state_entry_address, shared_array_from_buffer_entry_address,
    shared_bytes_from_buffer_entry_address, shared_string_from_buffer_entry_address,
    store_bridge_error, take_bridge_error, value_eq_entry_address,
    write_heap_value_to_slot_entry_address, zero_bytes_entry_address,
};
#[cfg(feature = "cranelift-jit")]
pub(crate) use codegen::{
    alloc_buffer_signature, box_heap_value_signature, clone_value_signature,
    collection_get_signature, collection_predicate_signature, copy_bytes_signature,
    entry_signature, free_buffer_signature, helper_signature, jump_with_status,
    pack_shared_signature, restore_exit_signature, value_eq_signature, value_slot_signature,
};
pub(crate) use exec::{ExecutableBuffer, prepare_for_execution};
pub(crate) use layout::{
    NativeStackLayout, ValueLayout, checked_add_i32, detect_native_stack_layout,
};
#[cfg(feature = "cranelift-jit")]
pub(crate) use offsets::{HeapIntrinsicAddrs, HeapIntrinsicRefs, ResolvedOffsets, resolve_offsets};

#[cfg(feature = "cranelift-jit")]
pub(crate) fn selected_codegen_backend() -> &'static str {
    "native"
}

#[cfg(not(feature = "cranelift-jit"))]
pub(crate) fn selected_codegen_backend() -> &'static str {
    "native-disabled"
}
