#![allow(unused_imports)]
mod bridge;
mod codegen;
mod exec;
mod layout;
mod offsets;

pub(crate) use bridge::{
    NativeFrameState, NativeInterruptMode, NativeInterruptSettings, OP_BUILTIN_CALL, OP_CALL,
    ROOT_FRAME_KEY, STATUS_CONTINUE, STATUS_ERROR, STATUS_HALTED, STATUS_JIT_TRACE_EXIT_BASE,
    STATUS_LINKED_CONTINUE, STATUS_OUT_OF_FUEL, STATUS_TRACE_EXIT, STATUS_WAITING, STATUS_YIELDED,
    active_local_base_entry_address, active_stack_base_entry_address,
    alloc_byte_buffer_entry_address, alloc_value_buffer_entry_address,
    aot_call_boundary_interrupt_entry_address, array_push_entry_address, array_set_entry_address,
    clear_bridge_error, clear_value_slot_entry_address, clone_value_to_slot_entry_address,
    collection_set_entry_address, copy_bytes_entry_address, decode_jit_trace_exit_status,
    encode_jit_trace_exit_status, enter_call_value_entry_address, frame_state_entry_address,
    helper_entry_address, helper_entry_offset, init_null_value_slot_entry_address,
    interrupt_helper_entry_address, interrupt_helper_entry_offset, leave_frame_entry_address,
    map_get_entry_address, map_has_entry_address, map_iter_next_entry_address,
    map_iter_take_key_entry_address, map_iter_take_value_entry_address, map_set_entry_address,
    non_yielding_host_call_entry_address, non_yielding_i64_host_call_entry_address,
    non_yielding_scalar_host_call_entry_address, regex_match_entry_address,
    regex_replace_entry_address, restore_active_exit_state_entry_address,
    restore_active_sparse_exit_state_entry_address, restore_exit_state_entry_address,
    restore_sparse_exit_state_entry_address, shared_array_from_buffer_entry_address,
    shared_bytes_from_buffer_entry_address, shared_string_from_buffer_entry_address,
    store_bridge_error, string_contains_entry_address, string_lower_ascii_entry_address,
    string_replace_literal_entry_address, string_split_literal_entry_address, take_bridge_error,
    to_string_entry_address, type_of_entry_address, value_eq_entry_address,
    value_len_entry_address, write_heap_value_to_slot_entry_address, zero_bytes_entry_address,
};
#[cfg(feature = "cranelift-jit")]
pub(crate) use codegen::{
    alloc_buffer_signature, array_set_signature, box_heap_value_signature, clone_value_signature,
    collection_get_signature, collection_mutation_signature, collection_predicate_signature,
    copy_bytes_signature, enter_call_value_signature, entry_signature, frame_state_signature,
    free_buffer_signature, helper_signature, jump_with_status, leave_frame_signature,
    map_iter_next_signature, map_iter_take_signature, map_set_signature,
    non_yielding_host_call_signature, non_yielding_i64_host_call_signature,
    non_yielding_scalar_host_call_signature, pack_shared_signature, regex_match_signature,
    regex_replace_signature, restore_exit_signature, sparse_restore_exit_signature,
    string_binary_transform_signature, string_contains_signature, string_replace_signature,
    string_unary_transform_signature, value_eq_signature, value_len_signature,
    value_slot_signature,
};
pub(crate) use exec::{ExecutableBuffer, prepare_for_execution};
pub(crate) use layout::{
    NativeStackLayout, ValueLayout, checked_add_i32, detect_native_stack_layout,
};
#[cfg(feature = "cranelift-jit")]
pub(crate) use offsets::{HeapIntrinsicAddrs, HeapIntrinsicRefs, ResolvedOffsets, resolve_offsets};

pub(crate) const NATIVE_CALLABLE_ABI_VERSION: u16 = 4;

#[cfg(feature = "cranelift-jit")]
pub(crate) fn selected_codegen_backend() -> &'static str {
    "native"
}

#[cfg(not(feature = "cranelift-jit"))]
pub(crate) fn selected_codegen_backend() -> &'static str {
    "native-disabled"
}
