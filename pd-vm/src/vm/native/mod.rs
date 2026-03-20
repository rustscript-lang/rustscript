mod bridge;
mod codegen;
mod exec;
mod inline;
mod layout;

pub(crate) use bridge::{
    NativeInterruptMode, NativeInterruptSettings, OP_ADD, OP_AND, OP_BUILTIN_CALL, OP_CALL, OP_CEQ,
    OP_CGT, OP_CLT, OP_DIV, OP_DUP, OP_GUARD_FALSE, OP_JUMP, OP_LDC, OP_LDLOC, OP_LSHR, OP_MOD,
    OP_MUL, OP_NEG, OP_NOT, OP_OR, OP_POP, OP_SHL, OP_SHR, OP_STLOC, OP_SUB, STATUS_CONTINUE,
    STATUS_ERROR, STATUS_HALTED, STATUS_OUT_OF_FUEL, STATUS_TRACE_EXIT, STATUS_WAITING,
    STATUS_YIELDED, alloc_byte_buffer_entry_address, alloc_value_buffer_entry_address,
    clear_bridge_error, clone_value_to_slot_entry_address, copy_bytes_entry_address,
    drop_shared_array_entry_address, drop_shared_bytes_entry_address,
    drop_shared_string_entry_address, helper_entry_address, helper_entry_offset,
    interrupt_helper_entry_address, interrupt_helper_entry_offset,
    restore_exit_state_entry_address, shared_array_from_buffer_entry_address,
    shared_bytes_from_buffer_entry_address, shared_string_from_buffer_entry_address,
    take_bridge_error, write_heap_value_to_slot_entry_address, zero_bytes_entry_address,
};
#[cfg(feature = "cranelift-jit")]
pub(crate) use codegen::{
    alloc_buffer_signature, box_heap_value_signature, clone_value_signature, copy_bytes_signature,
    drop_shared_signature, entry_signature, free_buffer_signature, helper_signature,
    jump_with_status, pack_shared_signature, restore_exit_signature,
};
pub(crate) use exec::ExecutableBuffer;
#[cfg(feature = "cranelift-jit")]
pub(crate) use inline::{
    HeapIntrinsicAddrs, HeapIntrinsicRefs, InlineEmitCtx, NativeInlineStep, ResolvedOffsets,
    emit_native_inline_step, resolve_offsets,
};
pub(crate) use layout::{
    NativeStackLayout, ValueLayout, checked_add_i32, detect_native_stack_layout,
};

#[cfg(feature = "cranelift-jit")]
pub(crate) fn selected_codegen_backend() -> &'static str {
    "native"
}

#[cfg(not(feature = "cranelift-jit"))]
pub(crate) fn selected_codegen_backend() -> &'static str {
    "native-disabled"
}
