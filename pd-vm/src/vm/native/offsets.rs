#[cfg(feature = "cranelift-jit")]
use crate::vm::VmResult;
#[cfg(feature = "cranelift-jit")]
use cranelift_codegen::ir::SigRef;

#[cfg(feature = "cranelift-jit")]
use super::{NativeStackLayout, checked_add_i32};

#[cfg(feature = "cranelift-jit")]
#[derive(Clone, Copy)]
pub(crate) struct ResolvedOffsets {
    pub(crate) stack_ptr: i32,
    pub(crate) stack_len: i32,
    pub(crate) locals_ptr: i32,
    pub(crate) locals_len: i32,
    pub(crate) constants_ptr: i32,
    pub(crate) vm_ip: i32,
}

#[cfg(feature = "cranelift-jit")]
#[derive(Clone, Copy)]
pub(crate) struct HeapIntrinsicRefs {
    pub(crate) alloc_buffer_ref: SigRef,
    pub(crate) free_buffer_ref: SigRef,
    pub(crate) pack_shared_ref: SigRef,
    pub(crate) copy_bytes_ref: SigRef,
}

#[cfg(feature = "cranelift-jit")]
#[derive(Clone, Copy)]
pub(crate) struct HeapIntrinsicAddrs {
    pub(crate) alloc_byte_buffer: usize,
    pub(crate) alloc_value_buffer: usize,
    pub(crate) pack_string: usize,
    pub(crate) pack_bytes: usize,
    pub(crate) pack_array: usize,
    pub(crate) copy_bytes: usize,
    pub(crate) zero_bytes: usize,
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn resolve_offsets(layout: NativeStackLayout) -> VmResult<ResolvedOffsets> {
    let stack_ptr = checked_add_i32(
        layout.vm_stack_offset,
        layout.stack_vec.ptr_offset,
        "stack ptr offset overflow",
    )?;
    let stack_len = checked_add_i32(
        layout.vm_stack_offset,
        layout.stack_vec.len_offset,
        "stack len offset overflow",
    )?;
    let locals_ptr = checked_add_i32(
        layout.vm_locals_offset,
        layout.stack_vec.ptr_offset,
        "locals ptr offset overflow",
    )?;
    let locals_len = checked_add_i32(
        layout.vm_locals_offset,
        layout.stack_vec.len_offset,
        "locals len offset overflow",
    )?;

    Ok(ResolvedOffsets {
        stack_ptr,
        stack_len,
        locals_ptr,
        locals_len,
        constants_ptr: layout.vm_program_constants_ptr_offset,
        vm_ip: layout.vm_ip_offset,
    })
}
