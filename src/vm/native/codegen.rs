#[cfg(feature = "cranelift-jit")]
use cranelift_codegen::ir::{AbiParam, Block, BlockArg, InstBuilder, Signature, types};
#[cfg(feature = "cranelift-jit")]
use cranelift_frontend::FunctionBuilder;

#[cfg(feature = "cranelift-jit")]
pub(crate) fn jump_with_status(
    b: &mut FunctionBuilder,
    block: Block,
    status: cranelift_codegen::ir::Value,
) {
    b.ins().jump(block, &[BlockArg::Value(status)]);
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn helper_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::I32));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn alloc_buffer_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(pointer_type));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn free_buffer_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn pack_shared_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(pointer_type));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn string_contains_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(types::I32));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn regex_match_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(types::I32));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn regex_replace_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(pointer_type));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn string_unary_transform_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(pointer_type));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn string_binary_transform_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(pointer_type));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn string_replace_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(pointer_type));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn copy_bytes_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn clone_value_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(types::I32));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn non_yielding_host_call_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(types::I32));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn collection_predicate_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(types::I32));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn collection_get_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(types::I32));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn map_iter_next_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::I32));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn map_iter_take_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::I32));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn collection_mutation_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(types::I32));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn map_set_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(types::I32));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn array_set_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(types::I32));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn value_slot_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(types::I32));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn value_eq_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(types::I32));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn value_len_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(types::I32));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn box_heap_value_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::I32));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn restore_exit_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(types::I32));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn sparse_restore_exit_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    for _ in 0..7 {
        sig.params.push(AbiParam::new(pointer_type));
    }
    sig.returns.push(AbiParam::new(types::I32));
    sig
}

#[cfg(feature = "cranelift-jit")]
pub(crate) fn entry_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(types::I32));
    sig
}
