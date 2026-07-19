use super::super::super::{Value, ValueType, VmError, VmResult};
use super::super::JitTrace;
use super::super::region::FusedRegionLink;
use super::super::runtime::resume_linked_trace_entry_address;
use super::{NativeCompileProfile, TraceLoweringKind};
use crate::vm::jit::deopt::exit_inputs;
use crate::vm::jit::ir::{
    SsaBlockId, SsaBranchTarget, SsaExitId, SsaInstKind, SsaMaterialization, SsaTerminator,
    SsaTrace, SsaValueId, SsaValueRepr,
};
use crate::vm::native::{
    ExecutableBuffer, HeapIntrinsicAddrs, HeapIntrinsicRefs, NativeFrameState, NativeInterruptMode,
    NativeInterruptSettings, NativeStackLayout, STATUS_CONTINUE, STATUS_ERROR, STATUS_OUT_OF_FUEL,
    STATUS_TRACE_EXIT, alloc_buffer_signature, alloc_byte_buffer_entry_address,
    alloc_value_buffer_entry_address, array_push_entry_address, array_set_entry_address,
    array_set_signature, box_heap_value_signature, checked_add_i32, clear_value_slot_entry_address,
    clone_value_signature, clone_value_to_slot_entry_address, collection_get_signature,
    collection_predicate_signature, copy_bytes_entry_address, copy_bytes_signature,
    detect_native_stack_layout, enter_call_value_inherited_entry_address,
    enter_call_value_inherited_signature, entry_signature, frame_state_entry_address,
    frame_state_signature, free_buffer_signature, jump_with_status,
    leave_frame_inherited_entry_address, leave_frame_inherited_signature, map_get_entry_address,
    map_has_entry_address, map_iter_next_entry_address, map_iter_next_signature,
    map_iter_take_key_entry_address, map_iter_take_signature, map_iter_take_value_entry_address,
    map_set_entry_address, map_set_signature, non_yielding_host_call_entry_address,
    non_yielding_host_call_signature, non_yielding_i64_host_call_entry_address,
    non_yielding_i64_host_call_signature, non_yielding_scalar_host_call_entry_address,
    non_yielding_scalar_host_call_signature, pack_shared_signature, regex_match_entry_address,
    regex_match_signature, regex_replace_entry_address, regex_replace_signature,
    replace_value_in_slot_entry_address, restore_active_sparse_exit_state_entry_address,
    restore_virtual_frame_entry_address, restore_virtual_frame_signature,
    shared_array_from_buffer_entry_address, shared_bytes_from_buffer_entry_address,
    shared_string_from_buffer_entry_address, sparse_restore_exit_signature,
    string_binary_transform_signature, string_contains_entry_address, string_contains_signature,
    string_lower_ascii_entry_address, string_replace_literal_entry_address,
    string_replace_signature, string_split_literal_entry_address, string_unary_transform_signature,
    to_string_entry_address, type_of_entry_address, value_eq_entry_address, value_eq_signature,
    value_len_entry_address, value_len_signature, value_slot_signature,
    write_heap_value_to_slot_entry_address, zero_bytes_entry_address,
};
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::immediates::Ieee64;
use cranelift_codegen::ir::{
    AbiParam, Block, BlockArg, InstBuilder, MemFlags, Signature, StackSlot, StackSlotData,
    StackSlotKind, types,
};
use cranelift_codegen::isa::{CallConv, OwnedTargetIsa};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module};
use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

static CRANELIFT_TRACE_ID: AtomicU64 = AtomicU64::new(1);
static CRANELIFT_JIT_ISA: OnceLock<Result<OwnedTargetIsa, String>> = OnceLock::new();
static CRANELIFT_TAIL_ISA: OnceLock<Result<OwnedTargetIsa, String>> = OnceLock::new();

const MAX_INHERITED_ENTRY_VALUES: usize = 256;
const INHERITED_STATE_ACTIVE_OFFSET: i32 = 0;
const INHERITED_STATE_FRAME_KEY_OFFSET: i32 = 8;
const INHERITED_STATE_STACK_BASE_OFFSET: i32 = 16;
const INHERITED_STATE_LOCAL_BASE_OFFSET: i32 = 24;
const INHERITED_STATE_DYNAMIC_TARGET_OFFSET: i32 = 32;
const INHERITED_STATE_TARGET_IP_OFFSET: i32 = 40;
const INHERITED_STATE_VALUE_COUNT_OFFSET: i32 = 48;
const INHERITED_STATE_VALUES_OFFSET: i32 = 56;

type TaggedConstants = (Box<[Value]>, HashMap<SsaValueId, usize>);

pub(crate) struct CompiledTrace {
    pub(crate) entry: *const u8,
    pub(crate) tail_entry: *const u8,
    pub(crate) keepalive: TraceKeepAlive,
    pub(crate) code: Vec<u8>,
    pub(crate) lowering_kind: TraceLoweringKind,
}

pub(crate) struct TraceKeepAlive {
    exec: ExecutableBuffer,
    _tagged_constants: Box<[Value]>,
    dependencies: Vec<TraceKeepAlive>,
}

impl TraceKeepAlive {
    fn from_code(code: &[u8], tagged_constants: Box<[Value]>) -> VmResult<Self> {
        Ok(Self {
            exec: ExecutableBuffer::new(code)?,
            _tagged_constants: tagged_constants,
            dependencies: Vec::new(),
        })
    }

    fn entry(&self) -> *const u8 {
        self.exec.entry()
    }
}

pub(crate) struct CompiledTailFunction {
    entry: *const u8,
    _keepalive: TraceKeepAlive,
    code: Vec<u8>,
}

impl CompiledTailFunction {
    pub(crate) fn entry(&self) -> *const u8 {
        self.entry
    }

    pub(crate) fn code_len(&self) -> usize {
        self.code.len()
    }

    pub(crate) fn into_parts(self) -> (*const u8, TraceKeepAlive, Vec<u8>) {
        (self.entry, self._keepalive, self.code)
    }
}

fn tail_entry_signature(pointer_type: cranelift_codegen::ir::Type) -> Signature {
    let mut signature = Signature::new(CallConv::Tail);
    signature.params.push(AbiParam::new(pointer_type));
    signature.returns.push(AbiParam::new(types::I32));
    signature
}

fn inherited_tail_entry_signature(pointer_type: cranelift_codegen::ir::Type) -> Signature {
    let mut signature = tail_entry_signature(pointer_type);
    signature.params.push(AbiParam::new(pointer_type));
    signature
}

fn compile_standalone_native_function(
    prefix: &str,
    signature: impl FnOnce(cranelift_codegen::ir::Type, CallConv) -> Signature,
    lower: impl FnOnce(&mut FunctionBuilder<'_>, cranelift_codegen::ir::Type, CallConv) -> VmResult<()>,
) -> VmResult<CompiledTailFunction> {
    let isa = native_tail_isa()?;
    let jit_builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    let mut module = JITModule::new(jit_builder);
    let pointer_type = module.target_config().pointer_type();
    let default_call_conv = module.target_config().default_call_conv;
    let mut ctx = module.make_context();
    ctx.func.signature = signature(pointer_type, default_call_conv);
    let function_id = CRANELIFT_TRACE_ID.fetch_add(1, Ordering::Relaxed);
    let function_name = format!("{prefix}_{function_id}");
    let func_id = module
        .declare_function(&function_name, Linkage::Local, &ctx.func.signature)
        .map_err(|err| VmError::JitNative(format!("declare {prefix} failed: {err}")))?;
    {
        let mut fb_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fb_ctx);
        lower(&mut builder, pointer_type, default_call_conv)?;
        builder.seal_all_blocks();
        builder.finalize();
    }
    module
        .define_function(func_id, &mut ctx)
        .map_err(|err| VmError::JitNative(format!("define {prefix} failed: {err}")))?;
    let code_len = ctx
        .compiled_code()
        .ok_or_else(|| VmError::JitNative(format!("{prefix} produced no machine code")))?
        .code_buffer()
        .len();
    module.clear_context(&mut ctx);
    module
        .finalize_definitions()
        .map_err(|err| VmError::JitNative(format!("finalize {prefix} failed: {err}")))?;
    let module_entry = module.get_finalized_function(func_id);
    let code = unsafe { std::slice::from_raw_parts(module_entry, code_len).to_vec() };
    let keepalive = TraceKeepAlive::from_code(&code, Box::new([]))?;
    let entry = keepalive.entry();
    Ok(CompiledTailFunction {
        entry,
        _keepalive: keepalive,
        code,
    })
}

pub(crate) fn compile_tail_status_body(status: i32) -> VmResult<CompiledTailFunction> {
    compile_standalone_native_function(
        "pd_vm_tail_status",
        |pointer_type, _| tail_entry_signature(pointer_type),
        move |builder, _, _| {
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            let status = builder.ins().iconst(types::I32, i64::from(status));
            builder.ins().return_(&[status]);
            Ok(())
        },
    )
}

pub(crate) fn compile_tail_side_link_body(
    slot_address: usize,
    deopt_status: i32,
) -> VmResult<CompiledTailFunction> {
    compile_standalone_native_function(
        "pd_vm_tail_side_link",
        |pointer_type, _| tail_entry_signature(pointer_type),
        move |builder, pointer_type, _| {
            let entry = builder.create_block();
            let deopt = builder.create_block();
            let linked = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            let vm_ptr = builder.block_params(entry)[0];
            let slot_address = iconst_ptr_from_addr(builder, pointer_type, slot_address)?;
            let target = builder
                .ins()
                .atomic_load(pointer_type, MemFlags::new(), slot_address);
            let is_null = builder.ins().icmp_imm(IntCC::Equal, target, 0);
            builder.ins().brif(is_null, deopt, &[], linked, &[]);

            builder.switch_to_block(deopt);
            let status = builder.ins().iconst(types::I32, i64::from(deopt_status));
            builder.ins().return_(&[status]);

            builder.switch_to_block(linked);
            let signature = builder.import_signature(tail_entry_signature(pointer_type));
            builder
                .ins()
                .return_call_indirect(signature, target, &[vm_ptr]);
            Ok(())
        },
    )
}

pub(crate) fn compile_system_tail_wrapper(root_entry: *const u8) -> VmResult<CompiledTailFunction> {
    let root_entry = root_entry as usize;
    compile_standalone_native_function(
        "pd_vm_tail_wrapper",
        entry_signature,
        move |builder, pointer_type, _| {
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            let vm_ptr = builder.block_params(entry)[0];
            let root_entry = iconst_ptr_from_addr(builder, pointer_type, root_entry)?;
            let signature = builder.import_signature(tail_entry_signature(pointer_type));
            let call = builder
                .ins()
                .call_indirect(signature, root_entry, &[vm_ptr]);
            let status = builder.inst_results(call)[0];
            builder.ins().return_(&[status]);
            Ok(())
        },
    )
}

pub(crate) fn compile_system_inherited_tail_wrapper(
    root_entry: *const u8,
) -> VmResult<CompiledTailFunction> {
    let root_entry = root_entry as usize;
    compile_standalone_native_function(
        "pd_vm_inherited_tail_wrapper",
        entry_signature,
        move |builder, pointer_type, _| {
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            let vm_ptr = builder.block_params(entry)[0];
            let pointer_bytes = pointer_type.bits() / 8;
            let packet_bytes = pointer_bytes
                .checked_mul((MAX_INHERITED_ENTRY_VALUES + 7) as u32)
                .ok_or_else(|| {
                    VmError::JitNative("native inherited-state packet is too large".to_string())
                })?;
            let packet = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                packet_bytes,
                pointer_bytes.trailing_zeros() as u8,
            ));
            let packet_ptr = builder.ins().stack_addr(pointer_type, packet, 0);
            let inactive = builder.ins().iconst(pointer_type, 0);
            builder.ins().store(
                MemFlags::new(),
                inactive,
                packet_ptr,
                INHERITED_STATE_ACTIVE_OFFSET,
            );
            builder.ins().store(
                MemFlags::new(),
                inactive,
                packet_ptr,
                INHERITED_STATE_DYNAMIC_TARGET_OFFSET,
            );
            let root_entry = iconst_ptr_from_addr(builder, pointer_type, root_entry)?;
            let signature = builder.import_signature(inherited_tail_entry_signature(pointer_type));
            let call = builder
                .ins()
                .call_indirect(signature, root_entry, &[vm_ptr, packet_ptr]);
            let status = builder.inst_results(call)[0];
            builder.ins().return_(&[status]);
            Ok(())
        },
    )
}

pub(crate) fn compile_tail_trace_dispatcher(
    trace_entry: *const u8,
    trace_id: usize,
    links: &[(i32, usize)],
) -> VmResult<CompiledTailFunction> {
    let trace_entry = trace_entry as usize;
    let links = links.to_vec();
    let direct_link_offset = detect_native_stack_layout()?.vm_jit_native_direct_link_count_offset;
    let active_trace_offset =
        detect_native_stack_layout()?.vm_jit_native_active_direct_trace_id_offset;
    compile_standalone_native_function(
        "pd_vm_tail_trace_dispatch",
        |pointer_type, _| inherited_tail_entry_signature(pointer_type),
        move |builder, pointer_type, _default_call_conv| {
            let entry = builder.create_block();
            let return_status = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.append_block_param(return_status, types::I32);
            builder.switch_to_block(entry);
            let vm_ptr = builder.block_params(entry)[0];
            let inherited_state_ptr = builder.block_params(entry)[1];
            let trace_id = i64::try_from(trace_id)
                .map_err(|_| VmError::JitNative("native trace id exceeds i64".to_string()))?;
            let trace_id = builder.ins().iconst(pointer_type, trace_id);
            builder
                .ins()
                .store(MemFlags::new(), trace_id, vm_ptr, active_trace_offset);
            let trace_entry = iconst_ptr_from_addr(builder, pointer_type, trace_entry)?;
            let trace_signature =
                builder.import_signature(inherited_tail_entry_signature(pointer_type));
            let call = builder.ins().call_indirect(
                trace_signature,
                trace_entry,
                &[vm_ptr, inherited_state_ptr],
            );
            let status = builder.inst_results(call)[0];

            let static_dispatch = builder.create_block();
            let dynamic_check = builder.create_block();
            let dynamic_linked = builder.create_block();
            builder.append_block_param(static_dispatch, types::I32);
            let is_linked_continue = builder.ins().icmp_imm(
                IntCC::Equal,
                status,
                i64::from(crate::vm::native::STATUS_LINKED_CONTINUE),
            );
            builder.ins().brif(
                is_linked_continue,
                dynamic_check,
                &[],
                static_dispatch,
                &[status.into()],
            );

            builder.switch_to_block(dynamic_check);
            let dynamic_target = builder.ins().load(
                pointer_type,
                MemFlags::new(),
                inherited_state_ptr,
                INHERITED_STATE_DYNAMIC_TARGET_OFFSET,
            );
            let dynamic_target_is_null = builder.ins().icmp_imm(IntCC::Equal, dynamic_target, 0);
            builder.ins().brif(
                dynamic_target_is_null,
                return_status,
                &[status.into()],
                dynamic_linked,
                &[],
            );

            builder.switch_to_block(dynamic_linked);
            let direct_count =
                builder
                    .ins()
                    .load(types::I64, MemFlags::new(), vm_ptr, direct_link_offset);
            let direct_count = builder.ins().iadd_imm(direct_count, 1);
            builder
                .ins()
                .store(MemFlags::new(), direct_count, vm_ptr, direct_link_offset);
            let dynamic_tail_signature =
                builder.import_signature(inherited_tail_entry_signature(pointer_type));
            builder.ins().return_call_indirect(
                dynamic_tail_signature,
                dynamic_target,
                &[vm_ptr, inherited_state_ptr],
            );

            builder.switch_to_block(static_dispatch);
            let status = builder.block_params(static_dispatch)[0];
            for (linked_status, slot_address) in links {
                let slot_check = builder.create_block();
                let next = builder.create_block();
                let linked = builder.create_block();
                builder.append_block_param(next, types::I32);
                let matches =
                    builder
                        .ins()
                        .icmp_imm(IntCC::Equal, status, i64::from(linked_status));
                builder
                    .ins()
                    .brif(matches, slot_check, &[], next, &[status.into()]);

                builder.switch_to_block(slot_check);
                let slot_address = iconst_ptr_from_addr(builder, pointer_type, slot_address)?;
                let target = builder
                    .ins()
                    .atomic_load(pointer_type, MemFlags::new(), slot_address);
                let is_null = builder.ins().icmp_imm(IntCC::Equal, target, 0);
                builder
                    .ins()
                    .brif(is_null, return_status, &[status.into()], linked, &[]);

                builder.switch_to_block(linked);
                let direct_count =
                    builder
                        .ins()
                        .load(types::I64, MemFlags::new(), vm_ptr, direct_link_offset);
                let direct_count = builder.ins().iadd_imm(direct_count, 1);
                builder
                    .ins()
                    .store(MemFlags::new(), direct_count, vm_ptr, direct_link_offset);
                let tail_signature =
                    builder.import_signature(inherited_tail_entry_signature(pointer_type));
                builder.ins().return_call_indirect(
                    tail_signature,
                    target,
                    &[vm_ptr, inherited_state_ptr],
                );

                builder.switch_to_block(next);
            }

            builder.ins().jump(return_status, &[status.into()]);
            builder.switch_to_block(return_status);
            let status = builder.block_params(return_status)[0];
            builder.ins().return_(&[status]);
            Ok(())
        },
    )
}

fn tail_owned_entry_signature(pointer_type: cranelift_codegen::ir::Type) -> Signature {
    let mut signature = tail_entry_signature(pointer_type);
    signature.params.push(AbiParam::new(pointer_type));
    signature
}

fn system_owned_entry_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: CallConv,
) -> Signature {
    let mut signature = entry_signature(pointer_type, call_conv);
    signature.params.push(AbiParam::new(pointer_type));
    signature
}

pub(crate) fn compile_tail_owned_side_link_body(
    slot_address: usize,
    deopt_status: i32,
) -> VmResult<CompiledTailFunction> {
    compile_standalone_native_function(
        "pd_vm_tail_owned_side_link",
        |pointer_type, _| tail_owned_entry_signature(pointer_type),
        move |builder, pointer_type, _| {
            let entry = builder.create_block();
            let deopt = builder.create_block();
            let linked = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            let vm_ptr = builder.block_params(entry)[0];
            let owned_slot = builder.block_params(entry)[1];
            let slot_address = iconst_ptr_from_addr(builder, pointer_type, slot_address)?;
            let target = builder
                .ins()
                .atomic_load(pointer_type, MemFlags::new(), slot_address);
            let is_null = builder.ins().icmp_imm(IntCC::Equal, target, 0);
            builder.ins().brif(is_null, deopt, &[], linked, &[]);

            builder.switch_to_block(deopt);
            let status = builder.ins().iconst(types::I32, i64::from(deopt_status));
            builder.ins().return_(&[status]);

            builder.switch_to_block(linked);
            let signature = builder.import_signature(tail_owned_entry_signature(pointer_type));
            builder
                .ins()
                .return_call_indirect(signature, target, &[vm_ptr, owned_slot]);
            Ok(())
        },
    )
}

pub(crate) fn compile_tail_owned_clear_body(success_status: i32) -> VmResult<CompiledTailFunction> {
    compile_standalone_native_function(
        "pd_vm_tail_owned_clear",
        |pointer_type, _| tail_owned_entry_signature(pointer_type),
        move |builder, pointer_type, call_conv| {
            let entry = builder.create_block();
            let success = builder.create_block();
            let failure = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.append_block_param(failure, types::I32);
            builder.switch_to_block(entry);
            let owned_slot = builder.block_params(entry)[1];
            let helper =
                iconst_ptr_from_addr(builder, pointer_type, clear_value_slot_entry_address())?;
            let helper_signature =
                builder.import_signature(value_slot_signature(pointer_type, call_conv));
            let call = builder
                .ins()
                .call_indirect(helper_signature, helper, &[owned_slot]);
            let helper_status = builder.inst_results(call)[0];
            let succeeded =
                builder
                    .ins()
                    .icmp_imm(IntCC::Equal, helper_status, i64::from(STATUS_CONTINUE));
            builder
                .ins()
                .brif(succeeded, success, &[], failure, &[helper_status.into()]);

            builder.switch_to_block(success);
            let status = builder.ins().iconst(types::I32, i64::from(success_status));
            builder.ins().return_(&[status]);

            builder.switch_to_block(failure);
            let status = builder.block_params(failure)[0];
            builder.ins().return_(&[status]);
            Ok(())
        },
    )
}

pub(crate) fn compile_system_owned_tail_wrapper(
    root_entry: *const u8,
) -> VmResult<CompiledTailFunction> {
    let root_entry = root_entry as usize;
    compile_standalone_native_function(
        "pd_vm_tail_owned_wrapper",
        system_owned_entry_signature,
        move |builder, pointer_type, _| {
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            let vm_ptr = builder.block_params(entry)[0];
            let owned_slot = builder.block_params(entry)[1];
            let root_entry = iconst_ptr_from_addr(builder, pointer_type, root_entry)?;
            let signature = builder.import_signature(tail_owned_entry_signature(pointer_type));
            let call = builder
                .ins()
                .call_indirect(signature, root_entry, &[vm_ptr, owned_slot]);
            let status = builder.inst_results(call)[0];
            builder.ins().return_(&[status]);
            Ok(())
        },
    )
}

fn try_compile_ssa_trace(
    trace: &JitTrace,
    ssa: &SsaTrace,
    internal_links: &[FusedRegionLink],
    interrupt_settings: Option<NativeInterruptSettings>,
    _profile: NativeCompileProfile,
    drop_contract_events_enabled: bool,
) -> VmResult<Option<CompiledTrace>> {
    if drop_contract_events_enabled {
        return Ok(None);
    }
    if !ssa_trace_supported(ssa) {
        return Ok(None);
    }

    let layout = detect_native_stack_layout()?;
    let offsets = resolve_offsets(layout)?;
    let isa = native_tail_isa()?;

    let jit_builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    let mut module = JITModule::new(jit_builder);
    let pointer_type = module.target_config().pointer_type();
    let call_conv = module.target_config().default_call_conv;

    let mut ctx = module.make_context();
    ctx.func.signature = inherited_tail_entry_signature(pointer_type);
    let clone_value_sig = clone_value_signature(pointer_type, call_conv);
    let non_yielding_host_call_sig = non_yielding_host_call_signature(pointer_type, call_conv);
    let non_yielding_scalar_host_call_sig =
        non_yielding_scalar_host_call_signature(pointer_type, call_conv);
    let non_yielding_i64_host_call_sig =
        non_yielding_i64_host_call_signature(pointer_type, call_conv);
    let value_slot_sig = value_slot_signature(pointer_type, call_conv);
    let value_eq_sig = value_eq_signature(pointer_type, call_conv);
    let value_len_sig = value_len_signature(pointer_type, call_conv);
    let box_heap_value_sig = box_heap_value_signature(pointer_type, call_conv);
    let alloc_buffer_sig = alloc_buffer_signature(pointer_type, call_conv);
    let free_buffer_sig = free_buffer_signature(pointer_type, call_conv);
    let pack_shared_sig = pack_shared_signature(pointer_type, call_conv);
    let copy_bytes_sig = copy_bytes_signature(pointer_type, call_conv);
    let map_has_sig = collection_predicate_signature(pointer_type, call_conv);
    let map_get_sig = collection_get_signature(pointer_type, call_conv);
    let map_iter_next_sig = map_iter_next_signature(pointer_type, call_conv);
    let map_iter_take_sig = map_iter_take_signature(pointer_type, call_conv);
    let array_push_sig = collection_get_signature(pointer_type, call_conv);
    let array_set_sig = array_set_signature(pointer_type, call_conv);
    let map_set_sig = map_set_signature(pointer_type, call_conv);
    let sparse_restore_exit_sig = sparse_restore_exit_signature(pointer_type, call_conv);
    let restore_virtual_frame_sig = restore_virtual_frame_signature(pointer_type, call_conv);
    let frame_state_sig = frame_state_signature(pointer_type, call_conv);
    let leave_frame_sig = leave_frame_inherited_signature(pointer_type, call_conv);
    let enter_call_value_sig = enter_call_value_inherited_signature(pointer_type, call_conv);

    let resume_linked_trace_sig = entry_signature(pointer_type, call_conv);
    let string_contains_sig = string_contains_signature(pointer_type, call_conv);
    let regex_match_sig = regex_match_signature(pointer_type, call_conv);
    let regex_replace_sig = regex_replace_signature(pointer_type, call_conv);
    let string_lower_sig = string_unary_transform_signature(pointer_type, call_conv);
    let string_replace_sig = string_replace_signature(pointer_type, call_conv);
    let string_split_sig = string_binary_transform_signature(pointer_type, call_conv);

    let trace_id = CRANELIFT_TRACE_ID.fetch_add(1, Ordering::Relaxed);
    let func_name = format!("pd_vm_trace_ssa_{trace_id}");
    let func_id = module
        .declare_function(&func_name, Linkage::Local, &ctx.func.signature)
        .map_err(|err| VmError::JitNative(format!("declare SSA trace function failed: {err}")))?;

    let (tagged_constants, tagged_constant_addrs) = prepare_tagged_constants(ssa)?;

    {
        let mut fb_ctx = FunctionBuilderContext::new();
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fb_ctx);
        let entry_block = b.create_block();
        let exit_block = b.create_block();
        b.append_block_param(exit_block, types::I32);
        let heap_refs = HeapIntrinsicRefs {
            alloc_buffer_ref: b.import_signature(alloc_buffer_sig),
            free_buffer_ref: b.import_signature(free_buffer_sig),
            pack_shared_ref: b.import_signature(pack_shared_sig),
            copy_bytes_ref: b.import_signature(copy_bytes_sig),
        };
        let heap_addrs = HeapIntrinsicAddrs {
            alloc_byte_buffer: alloc_byte_buffer_entry_address(),
            alloc_value_buffer: alloc_value_buffer_entry_address(),
            pack_string: shared_string_from_buffer_entry_address(),
            pack_bytes: shared_bytes_from_buffer_entry_address(),
            pack_array: shared_array_from_buffer_entry_address(),
            copy_bytes: copy_bytes_entry_address(),
            zero_bytes: zero_bytes_entry_address(),
        };
        let string_refs = SsaStringHelperRefs {
            contains_ref: b.import_signature(string_contains_sig),
            regex_match_ref: b.import_signature(regex_match_sig),
            regex_replace_ref: b.import_signature(regex_replace_sig),
            replace_ref: b.import_signature(string_replace_sig),
            lower_ascii_ref: b.import_signature(string_lower_sig.clone()),
            type_of_ref: b.import_signature(string_lower_sig.clone()),
            to_string_ref: b.import_signature(string_lower_sig),
            split_literal_ref: b.import_signature(string_split_sig),
        };
        let string_addrs = SsaStringHelperAddrs {
            contains: string_contains_entry_address(),
            regex_match: regex_match_entry_address(),
            regex_replace: regex_replace_entry_address(),
            replace_literal: string_replace_literal_entry_address(),
            lower_ascii: string_lower_ascii_entry_address(),
            type_of: type_of_entry_address(),
            to_string: to_string_entry_address(),
            split_literal: string_split_literal_entry_address(),
        };
        let frame_state_ref = b.import_signature(frame_state_sig);
        let deopt_refs = SsaDeoptHelperRefs {
            clone_value_ref: b.import_signature(clone_value_sig.clone()),
            replace_value_ref: b.import_signature(clone_value_sig),
            value_eq_ref: b.import_signature(value_eq_sig),
            value_len_ref: b.import_signature(value_len_sig),
            non_yielding_host_call_ref: b.import_signature(non_yielding_host_call_sig),
            non_yielding_scalar_host_call_ref: b
                .import_signature(non_yielding_scalar_host_call_sig),
            non_yielding_i64_host_call_ref: b.import_signature(non_yielding_i64_host_call_sig),
            clear_value_slot_ref: b.import_signature(value_slot_sig),
            box_heap_value_ref: b.import_signature(box_heap_value_sig),
            map_has_ref: b.import_signature(map_has_sig),
            map_get_ref: b.import_signature(map_get_sig),
            map_iter_next_ref: b.import_signature(map_iter_next_sig),
            map_iter_take_key_ref: b.import_signature(map_iter_take_sig.clone()),
            map_iter_take_value_ref: b.import_signature(map_iter_take_sig),
            array_push_ref: b.import_signature(array_push_sig),
            array_set_ref: b.import_signature(array_set_sig),
            map_set_ref: b.import_signature(map_set_sig),
            sparse_restore_exit_ref: b.import_signature(sparse_restore_exit_sig),
            restore_virtual_frame_ref: b.import_signature(restore_virtual_frame_sig),
            leave_frame_ref: b.import_signature(leave_frame_sig),
            enter_call_value_ref: b.import_signature(enter_call_value_sig),

            resume_linked_trace_ref: b.import_signature(resume_linked_trace_sig),
        };
        let deopt_addrs = SsaDeoptHelperAddrs {
            clone_value: clone_value_to_slot_entry_address(),
            replace_value: replace_value_in_slot_entry_address(),
            value_eq: value_eq_entry_address(),
            value_len: value_len_entry_address(),
            non_yielding_host_call: non_yielding_host_call_entry_address(),
            non_yielding_scalar_host_call: non_yielding_scalar_host_call_entry_address(),
            non_yielding_i64_host_call: non_yielding_i64_host_call_entry_address(),
            clear_value_slot: clear_value_slot_entry_address(),
            box_heap_value: write_heap_value_to_slot_entry_address(),
            map_has: map_has_entry_address(),
            map_get: map_get_entry_address(),
            map_iter_next: map_iter_next_entry_address(),
            map_iter_take_key: map_iter_take_key_entry_address(),
            map_iter_take_value: map_iter_take_value_entry_address(),
            array_push: array_push_entry_address(),
            array_set: array_set_entry_address(),
            map_set: map_set_entry_address(),
            sparse_restore_exit: restore_active_sparse_exit_state_entry_address(),
            restore_virtual_frame: restore_virtual_frame_entry_address(),
            leave_frame: leave_frame_inherited_entry_address(),
            enter_call_value: enter_call_value_inherited_entry_address(),

            resume_linked_trace: resume_linked_trace_entry_address(),
        };
        // The outer native dispatch loop links trace exits directly. Re-entering it through the
        // native bridge adds a redundant depth check to every subsequent linked trace.
        let allow_exit_link_handoff = false;

        let mut value_reprs = HashMap::new();
        for block in &ssa.blocks {
            for param in &block.params {
                value_reprs.insert(param.value.id, param.value.repr);
            }
            for inst in &block.insts {
                if let Some(output) = inst.output {
                    value_reprs.insert(output.id, output.repr);
                }
            }
        }
        let borrowed_array_gets = borrowed_array_get_outputs(ssa);
        let owned_value_temps = allocate_owned_value_temps(
            &mut b,
            ssa,
            layout.value.size,
            &borrowed_array_gets,
            &value_reprs,
        )?;
        let internal_link_slots = internal_links
            .iter()
            .map(|link| allocate_internal_link_slots(&mut b, link, layout.value.size))
            .collect::<VmResult<Vec<_>>>()?;

        let mut block_handles = HashMap::new();
        for block in &ssa.blocks {
            let handle = b.create_block();
            for param in &block.params {
                let Some(ty) = ssa_type(pointer_type, param.value.repr) else {
                    return Ok(None);
                };
                b.append_block_param(handle, ty);
            }
            block_handles.insert(block.id, handle);
        }

        let call_value_exits = ssa
            .blocks
            .iter()
            .filter_map(|block| match block.terminator.as_ref() {
                Some(SsaTerminator::CallValue {
                    argc,
                    call_ip,
                    resume_ip,
                    exit,
                }) => Some((*exit, (*argc, *call_ip, *resume_ip))),
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        let mut exit_specs = HashMap::new();
        for exit in &ssa.exits {
            let inputs = exit_inputs(exit);
            let trace_exit_block = b.create_block();
            let halted_block = b.create_block();
            let call_value_block = call_value_exits
                .contains_key(&exit.id)
                .then(|| b.create_block());
            let interrupt_block = (interrupt_settings.is_some()
                && internal_links.iter().any(|link| link.exit == exit.id))
            .then(|| b.create_block());
            for value in &inputs {
                let Some(repr) = value_reprs.get(value).copied() else {
                    return Ok(None);
                };
                let Some(ty) = ssa_type(pointer_type, repr) else {
                    return Ok(None);
                };
                b.append_block_param(trace_exit_block, ty);
                b.append_block_param(halted_block, ty);
                if let Some(call_value_block) = call_value_block {
                    b.append_block_param(call_value_block, ty);
                }
                if let Some(interrupt_block) = interrupt_block {
                    b.append_block_param(interrupt_block, ty);
                }
            }
            exit_specs.insert(
                exit.id,
                SsaExitLowering {
                    trace_exit_block,
                    halted_block,
                    call_value_block,
                    interrupt_block,
                    inputs,
                },
            );
        }

        b.switch_to_block(entry_block);
        b.append_block_params_for_function_params(entry_block);
        let vm_ptr = b.block_params(entry_block)[0];
        let inherited_state_ptr = b.block_params(entry_block)[1];
        let entry_ssa_block = ssa
            .blocks
            .get(ssa.entry.index())
            .ok_or_else(|| VmError::JitNative("SSA entry block is missing".to_string()))?;
        let entry_local_count = entry_ssa_block
            .params
            .len()
            .checked_sub(ssa.entry_stack_depth)
            .ok_or_else(|| {
                VmError::JitNative(
                    "SSA entry stack depth exceeds entry parameter count".to_string(),
                )
            })?;
        let entry_ready = b.create_block();
        b.append_block_param(entry_ready, pointer_type);
        b.append_block_param(entry_ready, pointer_type);
        for param in &entry_ssa_block.params {
            let ty = ssa_type(pointer_type, param.value.repr).ok_or_else(|| {
                VmError::JitNative("SSA inherited entry parameter type is unsupported".to_string())
            })?;
            b.append_block_param(entry_ready, ty);
        }

        let normal_entry = b.create_block();
        if entry_ssa_block.params.len() <= MAX_INHERITED_ENTRY_VALUES {
            let inherited_entry = b.create_block();
            let active = b.ins().load(
                pointer_type,
                MemFlags::new(),
                inherited_state_ptr,
                INHERITED_STATE_ACTIVE_OFFSET,
            );
            let has_inherited_state = b.ins().icmp_imm(IntCC::NotEqual, active, 0);
            // Active inherited packets are created only by admitted native links. Link
            // publication already validates frame, target, and entry shape, and invalidation
            // clears the slot before the packet can be reused.
            b.ins()
                .brif(has_inherited_state, inherited_entry, &[], normal_entry, &[]);

            b.switch_to_block(inherited_entry);
            let active_stack_base = b.ins().load(
                pointer_type,
                MemFlags::new(),
                inherited_state_ptr,
                INHERITED_STATE_STACK_BASE_OFFSET,
            );
            let active_local_base = b.ins().load(
                pointer_type,
                MemFlags::new(),
                inherited_state_ptr,
                INHERITED_STATE_LOCAL_BASE_OFFSET,
            );
            let inherited_args = build_inherited_entry_args(
                &mut b,
                inherited_state_ptr,
                pointer_type,
                entry_ssa_block.params.len(),
            )?;
            let inactive = b.ins().iconst(pointer_type, 0);
            b.ins().store(
                MemFlags::new(),
                inactive,
                inherited_state_ptr,
                INHERITED_STATE_ACTIVE_OFFSET,
            );
            let mut ready_args = vec![active_stack_base.into(), active_local_base.into()];
            ready_args.extend(ssa_block_args(inherited_args));
            b.ins().jump(entry_ready, &ready_args);
        } else {
            b.ins().jump(normal_entry, &[]);
        }

        b.switch_to_block(normal_entry);
        let (active_stack_base, active_local_base) = emit_frame_state_entry_guard(
            &mut b,
            vm_ptr,
            exit_block,
            pointer_type,
            offsets,
            frame_state_ref,
            frame_state_entry_address(),
            trace.frame_key,
            ssa.entry_stack_depth,
            entry_local_count,
        )?;
        let normal_args = build_entry_args(
            &mut b,
            vm_ptr,
            pointer_type,
            layout,
            offsets,
            active_stack_base,
            active_local_base,
            ssa.entry_stack_depth,
            entry_local_count,
        )?;
        let mut ready_args = vec![active_stack_base.into(), active_local_base.into()];
        ready_args.extend(ssa_block_args(normal_args));
        b.ins().jump(entry_ready, &ready_args);

        b.switch_to_block(entry_ready);
        let ready_params = b.block_params(entry_ready).to_vec();
        let active_stack_base = ready_params[0];
        let active_local_base = ready_params[1];
        let interrupt_ops_to_advance = u32::try_from(trace.op_names.len().max(1))
            .map_err(|_| VmError::JitNative("trace op count exceeds u32".to_string()))?;
        let lower_ctx = SsaLowerCtx {
            vm_ptr,
            inherited_state_ptr,
            frame_key: trace.frame_key,
            active_stack_base,
            active_local_base,
            exit_block,
            pointer_type,
            layout,
            offsets,
            entry_stack_depth: ssa.entry_stack_depth,
            heap_refs,
            heap_addrs,
            string_refs,
            string_addrs,
            helper_refs: deopt_refs,
            helper_addrs: deopt_addrs,
            owned_value_temps: &owned_value_temps,
            borrowed_array_gets: &borrowed_array_gets,
            value_reprs: &value_reprs,
            tagged_constant_addrs: &tagged_constant_addrs,
            interrupt_settings,
            interrupt_ops_to_advance,
        };
        let root_ip = b.ins().iconst(
            pointer_type,
            i64::try_from(trace.root_ip)
                .map_err(|_| VmError::JitNative("trace root ip out of range".to_string()))?,
        );
        b.ins()
            .store(MemFlags::new(), root_ip, vm_ptr, offsets.vm_ip);
        let entry_handle = *block_handles
            .get(&ssa.entry)
            .ok_or_else(|| VmError::JitNative("SSA entry block handle missing".to_string()))?;
        init_owned_value_temps(&mut b, pointer_type, layout.value, &owned_value_temps)?;
        let entry_args = ssa_block_args(ready_params[2..].to_vec());
        b.ins().jump(entry_handle, &entry_args);

        let charge_blocks = ssa_interrupt_charge_blocks(ssa);
        for block in &ssa.blocks {
            let handle = *block_handles
                .get(&block.id)
                .ok_or_else(|| VmError::JitNative("SSA block handle missing".to_string()))?;
            b.switch_to_block(handle);
            let mut values = HashMap::new();
            let block_params = b.block_params(handle).to_vec();
            for (param, lowered) in block.params.iter().zip(block_params.iter().copied()) {
                values.insert(param.value.id, lowered);
            }
            if internal_links.is_empty()
                && charge_blocks.contains(&block.id)
                && let Some(settings) = interrupt_settings
            {
                emit_interrupt_tick_inline(
                    &mut b,
                    vm_ptr,
                    exit_block,
                    offsets,
                    lower_ctx.interrupt_ops_to_advance,
                    settings,
                );
            }
            for inst in &block.insts {
                lower_ssa_inst(&mut b, lower_ctx, inst, &mut values)?;
            }
            lower_ssa_terminator(
                &mut b,
                lower_ctx,
                block.terminator.as_ref().ok_or_else(|| {
                    VmError::JitNative("SSA block missing terminator".to_string())
                })?,
                &values,
                &block_handles,
                &exit_specs,
                InternalRegionLinks {
                    links: internal_links,
                    slots: &internal_link_slots,
                },
            )?;
        }

        for exit in &ssa.exits {
            let spec = exit_specs
                .get(&exit.id)
                .ok_or_else(|| VmError::JitNative("SSA exit lowering missing".to_string()))?;
            lower_ssa_exit_block(
                &mut b,
                lower_ctx,
                exit,
                spec,
                SsaExitAction::TraceExit {
                    allow_link_handoff: allow_exit_link_handoff,
                },
            )?;
            lower_ssa_exit_block(&mut b, lower_ctx, exit, spec, SsaExitAction::Return)?;
            if let Some((argc, call_ip, resume_ip)) = call_value_exits.get(&exit.id).copied() {
                lower_ssa_exit_block(
                    &mut b,
                    lower_ctx,
                    exit,
                    spec,
                    SsaExitAction::CallValue {
                        argc,
                        call_ip,
                        resume_ip,
                    },
                )?;
            }
            if spec.interrupt_block.is_some() {
                lower_ssa_exit_block(&mut b, lower_ctx, exit, spec, SsaExitAction::InterruptYield)?;
            }
        }

        b.switch_to_block(exit_block);
        let final_status = b.block_params(exit_block)[0];
        clear_owned_value_temps(
            &mut b,
            exit_block,
            pointer_type,
            layout.value,
            deopt_refs,
            deopt_addrs,
            &owned_value_temps,
        )?;
        b.ins().return_(&[final_status]);

        b.seal_all_blocks();
        b.finalize();
    }

    module
        .define_function(func_id, &mut ctx)
        .map_err(|err| VmError::JitNative(format!("define SSA trace failed: {err}")))?;
    let code_len = ctx
        .compiled_code()
        .ok_or_else(|| VmError::JitNative("SSA trace produced no machine code".to_string()))?
        .code_buffer()
        .len();
    module.clear_context(&mut ctx);
    module
        .finalize_definitions()
        .map_err(|err| VmError::JitNative(format!("finalize SSA trace failed: {err}")))?;

    let entry = module.get_finalized_function(func_id);
    let code = if code_len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(entry, code_len).to_vec() }
    };
    let keepalive = TraceKeepAlive::from_code(&code, tagged_constants)?;
    let entry = keepalive.entry();

    Ok(Some(CompiledTrace {
        entry,
        tail_entry: entry,
        keepalive,
        code,
        lowering_kind: TraceLoweringKind::Ssa,
    }))
}

#[derive(Clone)]
struct SsaExitLowering {
    trace_exit_block: Block,
    halted_block: Block,
    call_value_block: Option<Block>,
    interrupt_block: Option<Block>,
    inputs: Vec<SsaValueId>,
}

#[derive(Clone, Copy)]
enum SsaExitAction {
    TraceExit {
        allow_link_handoff: bool,
    },
    Return,
    CallValue {
        argc: u8,
        call_ip: usize,
        resume_ip: usize,
    },
    InterruptYield,
}

#[derive(Clone, Copy)]
struct SsaDeoptHelperRefs {
    clone_value_ref: cranelift_codegen::ir::SigRef,
    replace_value_ref: cranelift_codegen::ir::SigRef,
    value_eq_ref: cranelift_codegen::ir::SigRef,
    value_len_ref: cranelift_codegen::ir::SigRef,
    non_yielding_host_call_ref: cranelift_codegen::ir::SigRef,
    non_yielding_scalar_host_call_ref: cranelift_codegen::ir::SigRef,
    non_yielding_i64_host_call_ref: cranelift_codegen::ir::SigRef,
    clear_value_slot_ref: cranelift_codegen::ir::SigRef,
    box_heap_value_ref: cranelift_codegen::ir::SigRef,
    map_has_ref: cranelift_codegen::ir::SigRef,
    map_get_ref: cranelift_codegen::ir::SigRef,
    map_iter_next_ref: cranelift_codegen::ir::SigRef,
    map_iter_take_key_ref: cranelift_codegen::ir::SigRef,
    map_iter_take_value_ref: cranelift_codegen::ir::SigRef,
    array_push_ref: cranelift_codegen::ir::SigRef,
    array_set_ref: cranelift_codegen::ir::SigRef,
    map_set_ref: cranelift_codegen::ir::SigRef,
    sparse_restore_exit_ref: cranelift_codegen::ir::SigRef,
    restore_virtual_frame_ref: cranelift_codegen::ir::SigRef,
    leave_frame_ref: cranelift_codegen::ir::SigRef,
    enter_call_value_ref: cranelift_codegen::ir::SigRef,

    resume_linked_trace_ref: cranelift_codegen::ir::SigRef,
}

#[derive(Clone, Copy)]
struct SsaDeoptHelperAddrs {
    clone_value: usize,
    replace_value: usize,
    value_eq: usize,
    value_len: usize,
    non_yielding_host_call: usize,
    non_yielding_scalar_host_call: usize,
    non_yielding_i64_host_call: usize,
    clear_value_slot: usize,
    box_heap_value: usize,
    map_has: usize,
    map_get: usize,
    map_iter_next: usize,
    map_iter_take_key: usize,
    map_iter_take_value: usize,
    array_push: usize,
    array_set: usize,
    map_set: usize,
    sparse_restore_exit: usize,
    restore_virtual_frame: usize,
    leave_frame: usize,
    enter_call_value: usize,

    resume_linked_trace: usize,
}

#[derive(Clone, Copy)]
struct SsaStringHelperRefs {
    contains_ref: cranelift_codegen::ir::SigRef,
    regex_match_ref: cranelift_codegen::ir::SigRef,
    regex_replace_ref: cranelift_codegen::ir::SigRef,
    replace_ref: cranelift_codegen::ir::SigRef,
    lower_ascii_ref: cranelift_codegen::ir::SigRef,
    type_of_ref: cranelift_codegen::ir::SigRef,
    to_string_ref: cranelift_codegen::ir::SigRef,
    split_literal_ref: cranelift_codegen::ir::SigRef,
}

#[derive(Clone, Copy)]
struct SsaStringHelperAddrs {
    contains: usize,
    regex_match: usize,
    regex_replace: usize,
    replace_literal: usize,
    lower_ascii: usize,
    type_of: usize,
    to_string: usize,
    split_literal: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SsaTempValueSlotKey {
    Output(SsaValueId),
    HostArgs(SsaValueId),
    HostScalarResult(SsaValueId),
    MapKeyBox(SsaValueId),
    MutationArgBox(SsaValueId, u8),
}

#[derive(Clone)]
struct SsaOwnedValueTemps {
    ordered: Vec<StackSlot>,
    slots: HashMap<SsaTempValueSlotKey, StackSlot>,
}

#[derive(Clone, Copy)]
struct InternalRegionLinks<'a> {
    links: &'a [FusedRegionLink],
    slots: &'a [Vec<Option<StackSlot>>],
}

impl<'a> InternalRegionLinks<'a> {
    fn find(self, exit: SsaExitId) -> Option<(&'a FusedRegionLink, &'a [Option<StackSlot>])> {
        self.links
            .iter()
            .zip(self.slots)
            .find(|(link, _)| link.exit == exit)
            .map(|(link, slots)| (link, slots.as_slice()))
    }
}

#[derive(Clone, Copy)]
struct SsaLowerCtx<'a> {
    vm_ptr: cranelift_codegen::ir::Value,
    inherited_state_ptr: cranelift_codegen::ir::Value,
    frame_key: u64,
    active_stack_base: cranelift_codegen::ir::Value,
    active_local_base: cranelift_codegen::ir::Value,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: crate::vm::native::NativeStackLayout,
    offsets: ResolvedOffsets,
    entry_stack_depth: usize,
    heap_refs: HeapIntrinsicRefs,
    heap_addrs: HeapIntrinsicAddrs,
    string_refs: SsaStringHelperRefs,
    string_addrs: SsaStringHelperAddrs,
    helper_refs: SsaDeoptHelperRefs,
    helper_addrs: SsaDeoptHelperAddrs,
    owned_value_temps: &'a SsaOwnedValueTemps,
    borrowed_array_gets: &'a BTreeSet<SsaValueId>,
    value_reprs: &'a HashMap<SsaValueId, SsaValueRepr>,
    tagged_constant_addrs: &'a HashMap<SsaValueId, usize>,
    interrupt_settings: Option<NativeInterruptSettings>,
    interrupt_ops_to_advance: u32,
}

#[derive(Clone, Copy)]
struct SsaMaterializeCtx<'a> {
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    value_layout: crate::vm::native::ValueLayout,
    exit_values: &'a HashMap<SsaValueId, cranelift_codegen::ir::Value>,
    deopt_refs: SsaDeoptHelperRefs,
    deopt_addrs: SsaDeoptHelperAddrs,
}

#[derive(Clone, Copy)]
struct SsaBoxCtx {
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    value_layout: crate::vm::native::ValueLayout,
    helper_refs: SsaDeoptHelperRefs,
    helper_addrs: SsaDeoptHelperAddrs,
}

#[derive(Clone, Copy)]
struct SsaConcatOp {
    output_id: SsaValueId,
    ip: usize,
    lhs: cranelift_codegen::ir::Value,
    rhs: cranelift_codegen::ir::Value,
    result_tag: u32,
    pack_addr: usize,
}

fn ssa_trace_supported(ssa: &SsaTrace) -> bool {
    for block in &ssa.blocks {
        for inst in &block.insts {
            if !matches!(
                inst.kind,
                SsaInstKind::Constant(_)
                    | SsaInstKind::CloneTagged { .. }
                    | SsaInstKind::ValueIsType { .. }
                    | SsaInstKind::UnboxHeapPtr { .. }
                    | SsaInstKind::UnboxInt { .. }
                    | SsaInstKind::UnboxFloat { .. }
                    | SsaInstKind::UnboxBool { .. }
                    | SsaInstKind::ValueLen { .. }
                    | SsaInstKind::StringLen { .. }
                    | SsaInstKind::BytesLen { .. }
                    | SsaInstKind::StringSlice { .. }
                    | SsaInstKind::BytesSlice { .. }
                    | SsaInstKind::StringGet { .. }
                    | SsaInstKind::BytesGet { .. }
                    | SsaInstKind::BytesHas { .. }
                    | SsaInstKind::StringContains { .. }
                    | SsaInstKind::RegexMatch { .. }
                    | SsaInstKind::RegexReplace { .. }
                    | SsaInstKind::StringReplaceLiteral { .. }
                    | SsaInstKind::StringLowerAscii { .. }
                    | SsaInstKind::TypeOf { .. }
                    | SsaInstKind::ToString { .. }
                    | SsaInstKind::StringSplitLiteral { .. }
                    | SsaInstKind::StringConcat { .. }
                    | SsaInstKind::BytesConcat { .. }
                    | SsaInstKind::BytesFromArrayU8 { .. }
                    | SsaInstKind::BytesToUtf8Ascii { .. }
                    | SsaInstKind::BytesToArrayU8 { .. }
                    | SsaInstKind::ArrayNew
                    | SsaInstKind::ArrayLen { .. }
                    | SsaInstKind::ArrayGet { .. }
                    | SsaInstKind::ArrayHas { .. }
                    | SsaInstKind::ArraySet { .. }
                    | SsaInstKind::ArrayPush { .. }
                    | SsaInstKind::MapLen { .. }
                    | SsaInstKind::MapGet { .. }
                    | SsaInstKind::MapHas { .. }
                    | SsaInstKind::MapSet { .. }
                    | SsaInstKind::MapIterNext { .. }
                    | SsaInstKind::MapIterTakeKey { .. }
                    | SsaInstKind::MapIterTakeValue { .. }
                    | SsaInstKind::IntNeg { .. }
                    | SsaInstKind::IntAdd { .. }
                    | SsaInstKind::IntAddImm { .. }
                    | SsaInstKind::IntSub { .. }
                    | SsaInstKind::IntSubImm { .. }
                    | SsaInstKind::IntMul { .. }
                    | SsaInstKind::IntMulImm { .. }
                    | SsaInstKind::IntDiv { .. }
                    | SsaInstKind::IntDivImm { .. }
                    | SsaInstKind::IntMod { .. }
                    | SsaInstKind::IntModImm { .. }
                    | SsaInstKind::IntShl { .. }
                    | SsaInstKind::IntShlImm { .. }
                    | SsaInstKind::IntShr { .. }
                    | SsaInstKind::IntShrImm { .. }
                    | SsaInstKind::IntLshr { .. }
                    | SsaInstKind::IntLshrImm { .. }
                    | SsaInstKind::BoolAnd { .. }
                    | SsaInstKind::BoolOr { .. }
                    | SsaInstKind::BoolNot { .. }
                    | SsaInstKind::FloatNeg { .. }
                    | SsaInstKind::FloatAdd { .. }
                    | SsaInstKind::FloatSub { .. }
                    | SsaInstKind::FloatMul { .. }
                    | SsaInstKind::FloatDiv { .. }
                    | SsaInstKind::FloatMod { .. }
                    | SsaInstKind::FloatCmpEq { .. }
                    | SsaInstKind::FloatCmpLt { .. }
                    | SsaInstKind::FloatCmpGt { .. }
                    | SsaInstKind::IntCmpEq { .. }
                    | SsaInstKind::ValueCmpEq { .. }
                    | SsaInstKind::IntCmpLt { .. }
                    | SsaInstKind::IntCmpLtImm { .. }
                    | SsaInstKind::IntCmpGt { .. }
                    | SsaInstKind::IntCmpGtImm { .. }
                    | SsaInstKind::HostCall { .. }
            ) {
                return false;
            }
        }
    }
    for exit in &ssa.exits {
        for materialization in &exit.stack {
            match materialization {
                SsaMaterialization::BoxInt(_)
                | SsaMaterialization::BoxFloat(_)
                | SsaMaterialization::BoxBool(_)
                | SsaMaterialization::Value(_)
                | SsaMaterialization::BoxHeapPtr { .. } => {}
            }
        }
        for materialization in exit
            .locals
            .iter()
            .zip(&exit.dirty_locals)
            .filter_map(|(materialization, dirty)| dirty.then_some(materialization))
        {
            match materialization {
                SsaMaterialization::Value(_)
                | SsaMaterialization::BoxInt(_)
                | SsaMaterialization::BoxFloat(_)
                | SsaMaterialization::BoxBool(_)
                | SsaMaterialization::BoxHeapPtr { .. } => {}
            }
        }
    }
    true
}

fn prepare_tagged_constants(ssa: &SsaTrace) -> VmResult<TaggedConstants> {
    let mut entries = Vec::new();
    for block in &ssa.blocks {
        for inst in &block.insts {
            let Some(output) = inst.output else {
                continue;
            };
            if output.repr != SsaValueRepr::Tagged {
                continue;
            }
            if let SsaInstKind::Constant(value) = &inst.kind {
                entries.push((output.id, value.clone()));
            }
        }
    }
    let values = entries
        .iter()
        .map(|(_, value)| value.clone())
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let mut out = HashMap::new();
    let base = values.as_ptr();
    for (index, (value_id, _)) in entries.iter().enumerate() {
        let addr = unsafe { base.add(index) as usize };
        if out.insert(*value_id, addr).is_some() {
            return Err(VmError::JitNative(
                "duplicate SSA tagged constant value id".to_string(),
            ));
        }
    }
    Ok((values, out))
}

fn scalar_host_return_type(repr: SsaValueRepr) -> VmResult<ValueType> {
    match repr {
        SsaValueRepr::I64 => Ok(ValueType::Int),
        SsaValueRepr::F64 => Ok(ValueType::Float),
        SsaValueRepr::Bool => Ok(ValueType::Bool),
        _ => Err(VmError::JitNative(
            "SSA scalar host-call result representation is unsupported".to_string(),
        )),
    }
}

fn ssa_load_scalar_host_result(
    b: &mut FunctionBuilder,
    repr: SsaValueRepr,
    out: cranelift_codegen::ir::Value,
) -> VmResult<cranelift_codegen::ir::Value> {
    match repr {
        SsaValueRepr::I64 => Ok(b.ins().load(types::I64, MemFlags::new(), out, 0)),
        SsaValueRepr::F64 => Ok(b.ins().load(types::F64, MemFlags::new(), out, 0)),
        SsaValueRepr::Bool => {
            let bits = b.ins().load(types::I64, MemFlags::new(), out, 0);
            let raw = b.ins().ireduce(types::I8, bits);
            Ok(b.ins().icmp_imm(IntCC::NotEqual, raw, 0))
        }
        _ => Err(VmError::JitNative(
            "SSA scalar host-call result representation is unsupported".to_string(),
        )),
    }
}

fn host_call_has_direct_i64_args(
    args: &[SsaValueId],
    value_reprs: &HashMap<SsaValueId, SsaValueRepr>,
) -> bool {
    args.len() <= 2
        && args
            .iter()
            .all(|arg| value_reprs.get(arg) == Some(&SsaValueRepr::I64))
}

fn allocate_owned_value_temps(
    b: &mut FunctionBuilder,
    ssa: &SsaTrace,
    value_size: i32,
    borrowed_array_gets: &BTreeSet<SsaValueId>,
    value_reprs: &HashMap<SsaValueId, SsaValueRepr>,
) -> VmResult<SsaOwnedValueTemps> {
    let mut ordered = Vec::new();
    let mut slots = HashMap::new();

    for block in &ssa.blocks {
        for inst in &block.insts {
            let Some(output) = inst.output else {
                continue;
            };
            let scalar_host_call = matches!(inst.kind, SsaInstKind::HostCall { .. })
                && output.repr != SsaValueRepr::Tagged;
            if ssa_inst_requires_owned_value_slot(&inst.kind)
                && !scalar_host_call
                && !borrowed_array_gets.contains(&output.id)
            {
                let slot = ssa_create_value_stack_slot(b, value_size)?;
                ordered.push(slot);
                slots.insert(SsaTempValueSlotKey::Output(output.id), slot);
            }
            if let SsaInstKind::HostCall { args, .. } = &inst.kind {
                if scalar_host_call {
                    let scalar_slot = b.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        std::mem::size_of::<u64>() as u32,
                        std::mem::align_of::<u64>().trailing_zeros() as u8,
                    ));
                    slots.insert(
                        SsaTempValueSlotKey::HostScalarResult(output.id),
                        scalar_slot,
                    );
                }
                if scalar_host_call && host_call_has_direct_i64_args(args, value_reprs) {
                    continue;
                }
                let arg_bytes = usize::try_from(value_size)
                    .ok()
                    .and_then(|value_size| value_size.checked_mul(args.len().max(1)))
                    .and_then(|bytes| u32::try_from(bytes).ok())
                    .ok_or_else(|| {
                        VmError::JitNative("SSA host-call argument storage too large".to_string())
                    })?;
                let align_shift = std::mem::align_of::<Value>().trailing_zeros() as u8;
                let args_slot = b.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    arg_bytes,
                    align_shift,
                ));
                slots.insert(SsaTempValueSlotKey::HostArgs(output.id), args_slot);
            }

            if matches!(
                inst.kind,
                SsaInstKind::MapGet { .. } | SsaInstKind::MapHas { .. }
            ) {
                let slot = ssa_create_value_stack_slot(b, value_size)?;
                ordered.push(slot);
                slots.insert(SsaTempValueSlotKey::MapKeyBox(output.id), slot);
            }
            let mutation_args: &[u8] = match inst.kind {
                SsaInstKind::ArraySet { .. } => &[1],
                SsaInstKind::ArrayPush { .. } => &[0],
                SsaInstKind::MapSet { .. } => &[0, 1],
                _ => &[],
            };
            for arg in mutation_args {
                let slot = ssa_create_value_stack_slot(b, value_size)?;
                ordered.push(slot);
                slots.insert(SsaTempValueSlotKey::MutationArgBox(output.id, *arg), slot);
            }
        }
    }
    Ok(SsaOwnedValueTemps { ordered, slots })
}

fn borrowed_array_get_outputs(ssa: &SsaTrace) -> BTreeSet<SsaValueId> {
    let mut instruction_uses: HashMap<SsaValueId, Vec<(SsaBlockId, usize, bool)>> = HashMap::new();
    let mut non_instruction_uses = BTreeSet::new();

    for block in &ssa.blocks {
        for (index, inst) in block.insts.iter().enumerate() {
            for input in inst.kind.inputs() {
                let borrows_input = match &inst.kind {
                    SsaInstKind::HostCall { .. } => true,
                    SsaInstKind::ArraySet { value, .. } | SsaInstKind::ArrayPush { value, .. } => {
                        *value == input
                    }
                    SsaInstKind::MapGet { key, .. } | SsaInstKind::MapHas { key, .. } => {
                        *key == input
                    }
                    SsaInstKind::MapSet { key, value, .. } => *key == input || *value == input,
                    _ => false,
                };
                instruction_uses
                    .entry(input)
                    .or_default()
                    .push((block.id, index, borrows_input));
            }
        }
        let Some(terminator) = &block.terminator else {
            continue;
        };
        match terminator {
            SsaTerminator::Jump { args, .. } => non_instruction_uses.extend(args.iter().copied()),
            SsaTerminator::BranchBool {
                condition,
                if_true,
                if_false,
            } => {
                non_instruction_uses.insert(*condition);
                for target in [if_true, if_false] {
                    if let SsaBranchTarget::Block { args, .. } = target {
                        non_instruction_uses.extend(args.iter().copied());
                    }
                }
            }
            SsaTerminator::Exit { .. }
            | SsaTerminator::Return { .. }
            | SsaTerminator::CallValue { .. } => {}
        }
    }
    for exit in &ssa.exits {
        non_instruction_uses.extend(exit_inputs(exit));
    }

    let mut borrowed = BTreeSet::new();
    for block in &ssa.blocks {
        for (definition_index, inst) in block.insts.iter().enumerate() {
            let Some(output) = inst.output else {
                continue;
            };
            if !matches!(inst.kind, SsaInstKind::ArrayGet { .. })
                || non_instruction_uses.contains(&output.id)
            {
                continue;
            }
            let Some([(use_block, use_index, true)]) =
                instruction_uses.get(&output.id).map(Vec::as_slice)
            else {
                continue;
            };
            if *use_block != block.id || *use_index <= definition_index {
                continue;
            }
            let borrow_stays_valid =
                block.insts[definition_index + 1..*use_index]
                    .iter()
                    .all(|between| {
                        !matches!(
                            between.kind,
                            SsaInstKind::ArraySet { .. }
                                | SsaInstKind::ArrayPush { .. }
                                | SsaInstKind::HostCall { .. }
                        )
                    });
            if borrow_stays_valid {
                borrowed.insert(output.id);
            }
        }
    }
    borrowed
}

fn ssa_inst_requires_owned_value_slot(kind: &SsaInstKind) -> bool {
    matches!(
        kind,
        SsaInstKind::CloneTagged { .. }
            | SsaInstKind::ArrayGet { .. }
            | SsaInstKind::ArraySet { .. }
            | SsaInstKind::ArrayPush { .. }
            | SsaInstKind::MapGet { .. }
            | SsaInstKind::MapSet { .. }
            | SsaInstKind::MapIterTakeKey { .. }
            | SsaInstKind::MapIterTakeValue { .. }
            | SsaInstKind::StringSlice { .. }
            | SsaInstKind::BytesSlice { .. }
            | SsaInstKind::StringGet { .. }
            | SsaInstKind::RegexReplace { .. }
            | SsaInstKind::StringReplaceLiteral { .. }
            | SsaInstKind::StringLowerAscii { .. }
            | SsaInstKind::TypeOf { .. }
            | SsaInstKind::ToString { .. }
            | SsaInstKind::StringSplitLiteral { .. }
            | SsaInstKind::BytesFromArrayU8 { .. }
            | SsaInstKind::BytesToUtf8Ascii { .. }
            | SsaInstKind::BytesToArrayU8 { .. }
            | SsaInstKind::ArrayNew
            | SsaInstKind::StringConcat { .. }
            | SsaInstKind::BytesConcat { .. }
            | SsaInstKind::HostCall { .. }
    )
}

fn ssa_create_value_stack_slot(b: &mut FunctionBuilder, value_size: i32) -> VmResult<StackSlot> {
    let bytes = u32::try_from(value_size)
        .map_err(|_| VmError::JitNative("SSA value slot size out of range".to_string()))?;
    let align_shift = std::mem::align_of::<Value>().trailing_zeros() as u8;
    Ok(b.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        bytes,
        align_shift,
    )))
}

fn init_owned_value_temps(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    value_layout: crate::vm::native::ValueLayout,
    temps: &SsaOwnedValueTemps,
) -> VmResult<()> {
    for slot in &temps.ordered {
        let addr = b.ins().stack_addr(pointer_type, *slot, 0);
        ssa_store_tag(b, value_layout, addr, value_layout.null_tag);
    }
    Ok(())
}

fn clear_owned_value_temps(
    b: &mut FunctionBuilder,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    value_layout: crate::vm::native::ValueLayout,
    helper_refs: SsaDeoptHelperRefs,
    helper_addrs: SsaDeoptHelperAddrs,
    temps: &SsaOwnedValueTemps,
) -> VmResult<()> {
    let _ = exit_block;
    for slot in &temps.ordered {
        let addr = b.ins().stack_addr(pointer_type, *slot, 0);
        clear_value_slot_if_heap(
            b,
            pointer_type,
            value_layout,
            helper_refs,
            helper_addrs,
            addr,
        )?;
    }
    Ok(())
}

fn owned_value_temp_slot_addr(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    temps: &SsaOwnedValueTemps,
    key: SsaTempValueSlotKey,
) -> VmResult<cranelift_codegen::ir::Value> {
    let slot =
        temps.slots.get(&key).copied().ok_or_else(|| {
            VmError::JitNative(format!("SSA temp value slot missing for {key:?}"))
        })?;
    Ok(b.ins().stack_addr(pointer_type, slot, 0))
}

fn clear_owned_value_temp_slot(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    helper_refs: SsaDeoptHelperRefs,
    helper_addrs: SsaDeoptHelperAddrs,
    addr: cranelift_codegen::ir::Value,
) -> VmResult<()> {
    ssa_call_infallible_helper(
        b,
        pointer_type,
        helper_refs.clear_value_slot_ref,
        helper_addrs.clear_value_slot,
        &[addr],
    )
}

fn clear_value_slot_if_heap(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    layout: crate::vm::native::ValueLayout,
    helper_refs: SsaDeoptHelperRefs,
    helper_addrs: SsaDeoptHelperAddrs,
    addr: cranelift_codegen::ir::Value,
) -> VmResult<()> {
    let tag = ssa_load_tag_i32(b, layout, addr);
    let scalar = ssa_is_scalar_tag(b, layout, tag);
    let done = b.create_block();
    let clear = b.create_block();
    b.ins().brif(scalar, done, &[], clear, &[]);

    b.switch_to_block(clear);
    clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, addr)?;
    b.ins().jump(done, &[]);

    b.switch_to_block(done);
    Ok(())
}

fn ssa_interrupt_charge_blocks(ssa: &SsaTrace) -> BTreeSet<crate::vm::jit::ir::SsaBlockId> {
    let mut blocks = BTreeSet::new();
    for block in &ssa.blocks {
        let Some(terminator) = &block.terminator else {
            continue;
        };
        for target in ssa_backedge_targets(block.id, terminator) {
            blocks.insert(target);
        }
    }
    if blocks.is_empty() {
        blocks.insert(ssa.entry);
    }
    blocks
}

fn ssa_backedge_targets(
    block: crate::vm::jit::ir::SsaBlockId,
    terminator: &SsaTerminator,
) -> Vec<crate::vm::jit::ir::SsaBlockId> {
    let mut targets = Vec::new();
    let mut push_target = |target: crate::vm::jit::ir::SsaBlockId| {
        if target.index() <= block.index() && !targets.contains(&target) {
            targets.push(target);
        }
    };
    match terminator {
        SsaTerminator::Jump { target, .. } => push_target(*target),
        SsaTerminator::BranchBool {
            if_true, if_false, ..
        } => {
            if let SsaBranchTarget::Block { target, .. } = if_true {
                push_target(*target);
            }
            if let SsaBranchTarget::Block { target, .. } = if_false {
                push_target(*target);
            }
        }
        SsaTerminator::Exit { .. }
        | SsaTerminator::Return { .. }
        | SsaTerminator::CallValue { .. } => {}
    }
    targets
}

fn build_inherited_entry_args(
    b: &mut FunctionBuilder,
    inherited_state_ptr: cranelift_codegen::ir::Value,
    pointer_type: cranelift_codegen::ir::Type,
    entry_value_count: usize,
) -> VmResult<Vec<cranelift_codegen::ir::Value>> {
    if entry_value_count > MAX_INHERITED_ENTRY_VALUES {
        return Err(VmError::JitNative(
            "SSA inherited entry value count exceeds packet capacity".to_string(),
        ));
    }
    (0..entry_value_count)
        .map(|index| {
            let byte_offset = index
                .checked_mul(std::mem::size_of::<usize>())
                .and_then(|offset| {
                    usize::try_from(INHERITED_STATE_VALUES_OFFSET)
                        .ok()
                        .and_then(|base| base.checked_add(offset))
                })
                .and_then(|offset| i32::try_from(offset).ok())
                .ok_or_else(|| {
                    VmError::JitNative("SSA inherited entry packet offset exceeds i32".to_string())
                })?;
            Ok(b.ins().load(
                pointer_type,
                MemFlags::new(),
                inherited_state_ptr,
                byte_offset,
            ))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn build_entry_args(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    pointer_type: cranelift_codegen::ir::Type,
    layout: crate::vm::native::NativeStackLayout,
    offsets: ResolvedOffsets,
    active_stack_base: cranelift_codegen::ir::Value,
    active_local_base: cranelift_codegen::ir::Value,
    stack_depth: usize,
    local_count: usize,
) -> VmResult<Vec<cranelift_codegen::ir::Value>> {
    let stack_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_ptr);
    let stack_ptr = ssa_value_addr(
        b,
        pointer_type,
        stack_ptr,
        active_stack_base,
        layout.value.size,
    );
    let locals_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.locals_ptr);
    let locals_ptr = ssa_value_addr(
        b,
        pointer_type,
        locals_ptr,
        active_local_base,
        layout.value.size,
    );
    let mut args = Vec::with_capacity(stack_depth + local_count);
    for stack_index in 0..stack_depth {
        let index = b.ins().iconst(
            pointer_type,
            i64::try_from(stack_index)
                .map_err(|_| VmError::JitNative("SSA stack index out of range".to_string()))?,
        );
        args.push(ssa_value_addr(
            b,
            pointer_type,
            stack_ptr,
            index,
            layout.value.size,
        ));
    }
    for local in 0..local_count {
        let index = b.ins().iconst(
            pointer_type,
            i64::try_from(local)
                .map_err(|_| VmError::JitNative("SSA local index out of range".to_string()))?,
        );
        args.push(ssa_value_addr(
            b,
            pointer_type,
            locals_ptr,
            index,
            layout.value.size,
        ));
    }
    Ok(args)
}

#[allow(clippy::too_many_arguments)]
fn emit_frame_state_entry_guard(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    offsets: ResolvedOffsets,
    frame_state_ref: cranelift_codegen::ir::SigRef,
    frame_state_addr: usize,
    expected_frame_key: u64,
    expected_stack_depth: usize,
    expected_local_count: usize,
) -> VmResult<(cranelift_codegen::ir::Value, cranelift_codegen::ir::Value)> {
    let frame_state_size = u32::try_from(std::mem::size_of::<NativeFrameState>())
        .map_err(|_| VmError::JitNative("native frame state size out of range".to_string()))?;
    let frame_state_align = std::mem::align_of::<NativeFrameState>().trailing_zeros() as u8;
    let frame_state_slot = b.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        frame_state_size,
        frame_state_align,
    ));
    let frame_state_ptr = b.ins().stack_addr(pointer_type, frame_state_slot, 0);
    ssa_call_status_helper(
        b,
        exit_block,
        pointer_type,
        frame_state_ref,
        frame_state_addr,
        &[vm_ptr, frame_state_ptr],
    )?;

    let frame_key_offset = i32::try_from(std::mem::offset_of!(NativeFrameState, frame_key))
        .map_err(|_| VmError::JitNative("native frame key offset out of range".to_string()))?;
    let stack_base_offset =
        i32::try_from(std::mem::offset_of!(NativeFrameState, operand_stack_base)).map_err(
            |_| VmError::JitNative("native frame stack-base offset out of range".to_string()),
        )?;
    let local_base_offset = i32::try_from(std::mem::offset_of!(NativeFrameState, local_base))
        .map_err(|_| {
            VmError::JitNative("native frame local-base offset out of range".to_string())
        })?;
    let frame_key = b.ins().load(
        types::I64,
        MemFlags::new(),
        frame_state_ptr,
        frame_key_offset,
    );
    let active_stack_base = b.ins().load(
        pointer_type,
        MemFlags::new(),
        frame_state_ptr,
        stack_base_offset,
    );
    let active_local_base = b.ins().load(
        pointer_type,
        MemFlags::new(),
        frame_state_ptr,
        local_base_offset,
    );

    let expected_frame_key = b.ins().iconst(types::I64, expected_frame_key as i64);
    let frame_matches = b.ins().icmp(IntCC::Equal, frame_key, expected_frame_key);
    let frame_match = b.create_block();
    let mismatch = b.create_block();
    b.ins().brif(frame_matches, frame_match, &[], mismatch, &[]);

    b.switch_to_block(mismatch);
    let status = b.ins().iconst(types::I32, STATUS_CONTINUE as i64);
    jump_with_status(b, exit_block, status);

    b.switch_to_block(frame_match);
    let actual_stack_len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_len);
    let expected_stack_depth = b.ins().iconst(
        pointer_type,
        i64::try_from(expected_stack_depth)
            .map_err(|_| VmError::JitNative("SSA entry stack depth out of range".to_string()))?,
    );
    let expected_stack_len = b.ins().iadd(active_stack_base, expected_stack_depth);
    let stack_matches = b
        .ins()
        .icmp(IntCC::Equal, actual_stack_len, expected_stack_len);
    let stack_match = b.create_block();
    let stack_mismatch = b.create_block();
    b.ins()
        .brif(stack_matches, stack_match, &[], stack_mismatch, &[]);

    b.switch_to_block(stack_mismatch);
    let status = b.ins().iconst(types::I32, STATUS_CONTINUE as i64);
    jump_with_status(b, exit_block, status);

    b.switch_to_block(stack_match);
    let actual_local_len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.locals_len);
    let expected_local_count = b.ins().iconst(
        pointer_type,
        i64::try_from(expected_local_count)
            .map_err(|_| VmError::JitNative("SSA entry local count out of range".to_string()))?,
    );
    let expected_local_len = b.ins().iadd(active_local_base, expected_local_count);
    let locals_match = b
        .ins()
        .icmp(IntCC::Equal, actual_local_len, expected_local_len);
    let locals_matched = b.create_block();
    let locals_mismatch = b.create_block();
    b.ins()
        .brif(locals_match, locals_matched, &[], locals_mismatch, &[]);

    b.switch_to_block(locals_mismatch);
    let status = b.ins().iconst(types::I32, STATUS_CONTINUE as i64);
    jump_with_status(b, exit_block, status);

    b.switch_to_block(locals_matched);
    Ok((active_stack_base, active_local_base))
}

fn ssa_block_args(values: impl IntoIterator<Item = cranelift_codegen::ir::Value>) -> Vec<BlockArg> {
    values.into_iter().map(BlockArg::Value).collect()
}

fn lower_ssa_inst(
    b: &mut FunctionBuilder,
    ctx: SsaLowerCtx<'_>,
    inst: &crate::vm::jit::ir::SsaInst,
    values: &mut HashMap<SsaValueId, cranelift_codegen::ir::Value>,
) -> VmResult<()> {
    let SsaLowerCtx {
        vm_ptr,
        exit_block,
        pointer_type,
        layout,
        offsets,
        heap_refs,
        heap_addrs,
        string_refs,
        string_addrs,
        helper_refs,
        helper_addrs,
        owned_value_temps,
        borrowed_array_gets,
        value_reprs,
        tagged_constant_addrs,
        ..
    } = ctx;
    let Some(output) = inst.output else {
        return Err(VmError::JitNative(
            "SSA effect-only inst not supported".to_string(),
        ));
    };
    let lowered = match &inst.kind {
        SsaInstKind::Constant(Value::Int(value)) => b.ins().iconst(types::I64, *value),
        SsaInstKind::Constant(Value::Float(value)) => b.ins().f64const(Ieee64::with_float(*value)),
        SsaInstKind::Constant(Value::Bool(value)) => {
            let raw = b.ins().iconst(types::I8, if *value { 1 } else { 0 });
            b.ins().icmp_imm(IntCC::NotEqual, raw, 0)
        }
        SsaInstKind::Constant(
            Value::Null
            | Value::String(_)
            | Value::Bytes(_)
            | Value::Array(_)
            | Value::Map(_)
            | Value::Callable(_),
        ) => {
            let addr = tagged_constant_addrs
                .get(&output.id)
                .copied()
                .ok_or_else(|| {
                    VmError::JitNative("SSA tagged constant lowering address missing".to_string())
                })?;
            iconst_ptr_from_addr(b, pointer_type, addr)?
        }
        SsaInstKind::CloneTagged { input } => {
            if value_reprs.get(input) != Some(&SsaValueRepr::Tagged) {
                return Err(VmError::JitNative(
                    "SSA clone-tagged input must be tagged".to_string(),
                ));
            }
            let out = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::Output(output.id),
            )?;
            ssa_call_status_helper(
                b,
                exit_block,
                pointer_type,
                helper_refs.replace_value_ref,
                helper_addrs.replace_value,
                &[out, values[input]],
            )?;
            out
        }
        SsaInstKind::ValueIsType { input, tag } => {
            let input = *values.get(input).ok_or_else(|| {
                VmError::JitNative("SSA type predicate input missing".to_string())
            })?;
            let expected_tag = match tag {
                ValueType::Null => layout.value.null_tag,
                ValueType::Int => layout.value.int_tag,
                ValueType::Float => layout.value.float_tag,
                ValueType::Bool => layout.value.bool_tag,
                ValueType::String => layout.value.string_tag,
                ValueType::Bytes => layout.value.bytes_tag,
                ValueType::Array => layout.value.array_tag,
                ValueType::Map => layout.value.map_tag,
                ValueType::Callable | ValueType::Unknown => {
                    return Err(VmError::JitNative(format!(
                        "unsupported SSA type predicate tag {tag:?}"
                    )));
                }
            };
            let actual_tag = ssa_load_tag_i32(b, layout.value, input);
            b.ins()
                .icmp_imm(IntCC::Equal, actual_tag, i64::from(expected_tag))
        }
        SsaInstKind::UnboxInt { input } => {
            let input = *values
                .get(input)
                .ok_or_else(|| VmError::JitNative("SSA int unbox input missing".to_string()))?;
            let type_ok = b.create_block();
            let fail = b.create_block();
            let cont = b.create_block();
            let tag = ssa_load_tag_i32(b, layout.value, input);
            let is_int = b
                .ins()
                .icmp_imm(IntCC::Equal, tag, i64::from(layout.value.int_tag));
            b.ins().brif(is_int, type_ok, &[], fail, &[]);

            b.switch_to_block(type_ok);
            let out = b.ins().load(
                types::I64,
                MemFlags::new(),
                input,
                layout.value.int_payload_offset,
            );
            b.ins().jump(cont, &[]);

            b.switch_to_block(fail);
            ssa_emit_trace_exit_status(b, vm_ptr, exit_block, pointer_type, offsets, inst.ip)?;

            b.switch_to_block(cont);
            out
        }
        SsaInstKind::UnboxFloat { input } => {
            let input = *values
                .get(input)
                .ok_or_else(|| VmError::JitNative("SSA float unbox input missing".to_string()))?;
            let type_ok = b.create_block();
            let fail = b.create_block();
            let cont = b.create_block();
            let tag = ssa_load_tag_i32(b, layout.value, input);
            let is_float = b
                .ins()
                .icmp_imm(IntCC::Equal, tag, i64::from(layout.value.float_tag));
            b.ins().brif(is_float, type_ok, &[], fail, &[]);

            b.switch_to_block(type_ok);
            let out = b.ins().load(
                types::F64,
                MemFlags::new(),
                input,
                layout.value.float_payload_offset,
            );
            b.ins().jump(cont, &[]);

            b.switch_to_block(fail);
            ssa_emit_trace_exit_status(b, vm_ptr, exit_block, pointer_type, offsets, inst.ip)?;

            b.switch_to_block(cont);
            out
        }
        SsaInstKind::UnboxBool { input } => {
            let input = *values
                .get(input)
                .ok_or_else(|| VmError::JitNative("SSA bool unbox input missing".to_string()))?;
            let type_ok = b.create_block();
            let fail = b.create_block();
            let cont = b.create_block();
            let tag = ssa_load_tag_i32(b, layout.value, input);
            let is_bool = b
                .ins()
                .icmp_imm(IntCC::Equal, tag, i64::from(layout.value.bool_tag));
            b.ins().brif(is_bool, type_ok, &[], fail, &[]);

            b.switch_to_block(type_ok);
            let raw = b.ins().load(
                types::I8,
                MemFlags::new(),
                input,
                layout.value.bool_payload_offset,
            );
            let out = b.ins().icmp_imm(IntCC::NotEqual, raw, 0);
            b.ins().jump(cont, &[]);

            b.switch_to_block(fail);
            ssa_emit_trace_exit_status(b, vm_ptr, exit_block, pointer_type, offsets, inst.ip)?;

            b.switch_to_block(cont);
            out
        }
        SsaInstKind::UnboxHeapPtr { input, tag } => {
            let input = *values
                .get(input)
                .ok_or_else(|| VmError::JitNative("SSA heap unbox input missing".to_string()))?;
            let expected_tag = ssa_heap_tag(layout.value, *tag)?;
            let type_ok = b.create_block();
            let fail = b.create_block();
            let cont = b.create_block();
            let tag = ssa_load_tag_i32(b, layout.value, input);
            let is_heap = b.ins().icmp_imm(IntCC::Equal, tag, i64::from(expected_tag));
            b.ins().brif(is_heap, type_ok, &[], fail, &[]);

            b.switch_to_block(type_ok);
            let out = ssa_load_heap_ptr(b, layout.value, input, pointer_type);
            b.ins().jump(cont, &[]);

            b.switch_to_block(fail);
            ssa_emit_trace_exit_status(b, vm_ptr, exit_block, pointer_type, offsets, inst.ip)?;

            b.switch_to_block(cont);
            out
        }
        SsaInstKind::ValueLen { value } => {
            let value = values[value];
            let out_slot = b.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                std::mem::size_of::<i64>() as u32,
                std::mem::align_of::<i64>().trailing_zeros() as u8,
            ));
            let out = b.ins().stack_addr(pointer_type, out_slot, 0);
            let helper_ptr = iconst_ptr_from_addr(b, pointer_type, helper_addrs.value_len)?;
            let call = b
                .ins()
                .call_indirect(helper_refs.value_len_ref, helper_ptr, &[value, out]);
            let status = b.inst_results(call)[0];
            let success = b.create_block();
            let fail = b.create_block();
            let ok = b
                .ins()
                .icmp_imm(IntCC::Equal, status, i64::from(STATUS_CONTINUE));
            b.ins().brif(ok, success, &[], fail, &[]);

            b.switch_to_block(fail);
            jump_with_status(b, exit_block, status);

            b.switch_to_block(success);
            b.ins().stack_load(types::I64, out_slot, 0)
        }
        SsaInstKind::StringLen { text } => {
            let string_data = ssa_load_heap_data_ptr(b, layout.value, values[text]);
            let bytes_ptr = b.ins().load(
                pointer_type,
                MemFlags::new(),
                string_data,
                layout.stack_vec.ptr_offset,
            );
            let bytes_len = b.ins().load(
                pointer_type,
                MemFlags::new(),
                string_data,
                layout.stack_vec.len_offset,
            );
            let loop_block = b.create_block();
            let step_block = b.create_block();
            let done_block = b.create_block();
            b.append_block_param(loop_block, pointer_type);
            b.append_block_param(loop_block, pointer_type);
            b.append_block_param(done_block, pointer_type);

            let zero = b.ins().iconst(pointer_type, 0);
            b.ins()
                .jump(loop_block, &[BlockArg::Value(zero), BlockArg::Value(zero)]);

            b.switch_to_block(loop_block);
            let byte_index = b.block_params(loop_block)[0];
            let char_count = b.block_params(loop_block)[1];
            let done = b
                .ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, byte_index, bytes_len);
            b.ins().brif(
                done,
                done_block,
                &[BlockArg::Value(char_count)],
                step_block,
                &[],
            );

            b.switch_to_block(step_block);
            let byte_ptr = b.ins().iadd(bytes_ptr, byte_index);
            let byte = ssa_load_byte(b, byte_ptr);
            let cont = ssa_is_utf8_continuation_byte(b, byte);
            let advanced_count = b.ins().iadd_imm(char_count, 1);
            let next_count = b.ins().select(cont, char_count, advanced_count);
            let next_index = b.ins().iadd_imm(byte_index, 1);
            b.ins().jump(
                loop_block,
                &[BlockArg::Value(next_index), BlockArg::Value(next_count)],
            );

            b.switch_to_block(done_block);
            b.block_params(done_block)[0]
        }
        SsaInstKind::BytesLen { bytes } => {
            let bytes = values[bytes];
            let vec_ptr = ssa_load_heap_data_ptr(b, layout.value, bytes);
            b.ins().load(
                pointer_type,
                MemFlags::new(),
                vec_ptr,
                layout.stack_vec.len_offset,
            )
        }
        SsaInstKind::StringSlice {
            text,
            start,
            length,
        } => {
            let text = values[text];
            let start = values[start];
            let length = values[length];
            let string_data = ssa_load_heap_data_ptr(b, layout.value, text);
            let bytes_ptr = b.ins().load(
                pointer_type,
                MemFlags::new(),
                string_data,
                layout.stack_vec.ptr_offset,
            );
            let bytes_len = b.ins().load(
                pointer_type,
                MemFlags::new(),
                string_data,
                layout.stack_vec.len_offset,
            );
            let out = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::Output(output.id),
            )?;
            let seek_start = b.create_block();
            let seek_end = b.create_block();
            let copy_block = b.create_block();
            let empty_block = b.create_block();
            let cont = b.create_block();
            b.append_block_param(seek_start, pointer_type);
            b.append_block_param(seek_start, pointer_type);
            b.append_block_param(seek_end, pointer_type);
            b.append_block_param(seek_end, pointer_type);
            b.append_block_param(seek_end, pointer_type);
            b.append_block_param(copy_block, pointer_type);
            b.append_block_param(copy_block, pointer_type);

            let zero = b.ins().iconst(pointer_type, 0);
            let start_negative = b.ins().icmp_imm(IntCC::SignedLessThan, start, 0);
            let length_positive = b.ins().icmp_imm(IntCC::SignedGreaterThan, length, 0);
            let start_non_negative = b.ins().bnot(start_negative);
            let positive = b.ins().band(start_non_negative, length_positive);
            b.ins().brif(
                positive,
                seek_start,
                &[BlockArg::Value(zero), BlockArg::Value(zero)],
                empty_block,
                &[],
            );

            b.switch_to_block(seek_start);
            let byte_index = b.block_params(seek_start)[0];
            let char_index = b.block_params(seek_start)[1];
            let reached_start = b.ins().icmp(IntCC::Equal, char_index, start);
            let scan_more = b.create_block();
            b.ins().brif(
                reached_start,
                seek_end,
                &[
                    BlockArg::Value(byte_index),
                    BlockArg::Value(byte_index),
                    BlockArg::Value(length),
                ],
                scan_more,
                &[],
            );

            b.switch_to_block(scan_more);
            let at_end = b
                .ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, byte_index, bytes_len);
            let start_found = b.create_block();
            b.ins().brif(at_end, empty_block, &[], start_found, &[]);

            b.switch_to_block(start_found);
            let current_ptr = b.ins().iadd(bytes_ptr, byte_index);
            let current_byte = ssa_load_byte(b, current_ptr);
            let current_width = ssa_utf8_char_width(b, pointer_type, current_byte);
            let next_byte = b.ins().iadd(byte_index, current_width);
            let next_char = b.ins().iadd_imm(char_index, 1);
            b.ins().jump(
                seek_start,
                &[BlockArg::Value(next_byte), BlockArg::Value(next_char)],
            );

            b.switch_to_block(seek_end);
            let slice_start = b.block_params(seek_end)[0];
            let end_byte = b.block_params(seek_end)[1];
            let remaining_chars = b.block_params(seek_end)[2];
            let no_chars_left = b.ins().icmp_imm(IntCC::Equal, remaining_chars, 0);
            let reached_end = b
                .ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, end_byte, bytes_len);
            let finish_now = b.ins().bor(no_chars_left, reached_end);
            let finish_block = b.create_block();
            let advance_block = b.create_block();
            b.ins()
                .brif(finish_now, finish_block, &[], advance_block, &[]);

            b.switch_to_block(advance_block);
            let end_ptr = b.ins().iadd(bytes_ptr, end_byte);
            let end_byte_value = ssa_load_byte(b, end_ptr);
            let end_width = ssa_utf8_char_width(b, pointer_type, end_byte_value);
            let next_end = b.ins().iadd(end_byte, end_width);
            let one = b.ins().iconst(pointer_type, 1);
            let remaining_next = b.ins().isub(remaining_chars, one);
            b.ins().jump(
                seek_end,
                &[
                    BlockArg::Value(slice_start),
                    BlockArg::Value(next_end),
                    BlockArg::Value(remaining_next),
                ],
            );

            b.switch_to_block(finish_block);
            let slice_len = b.ins().isub(end_byte, slice_start);
            b.ins().jump(
                copy_block,
                &[BlockArg::Value(slice_start), BlockArg::Value(slice_len)],
            );

            b.switch_to_block(empty_block);
            b.ins()
                .jump(copy_block, &[BlockArg::Value(zero), BlockArg::Value(zero)]);

            b.switch_to_block(copy_block);
            let slice_start = b.block_params(copy_block)[0];
            let slice_len = b.block_params(copy_block)[1];
            let source_ptr = b.ins().iadd(bytes_ptr, slice_start);
            let out_ptr = ssa_call_alloc_buffer(
                b,
                pointer_type,
                heap_refs,
                heap_addrs,
                heap_addrs.alloc_byte_buffer,
                slice_len,
            )?;
            ssa_call_copy_bytes(
                b,
                pointer_type,
                heap_refs,
                heap_addrs,
                out_ptr,
                source_ptr,
                slice_len,
            )?;
            let out_raw = ssa_call_pack_shared(
                b,
                pointer_type,
                heap_refs,
                heap_addrs.pack_string,
                out_ptr,
                slice_len,
                slice_len,
            )?;
            clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, out)?;
            ssa_store_heap_ptr_in_value(b, layout.value, out, layout.value.string_tag, out_raw);
            b.ins().jump(cont, &[]);

            b.switch_to_block(cont);
            out
        }
        SsaInstKind::BytesSlice {
            bytes,
            start,
            length,
        } => {
            let bytes = values[bytes];
            let start = values[start];
            let length = values[length];
            let bytes_data = ssa_load_heap_data_ptr(b, layout.value, bytes);
            let bytes_ptr = b.ins().load(
                pointer_type,
                MemFlags::new(),
                bytes_data,
                layout.stack_vec.ptr_offset,
            );
            let bytes_len = b.ins().load(
                pointer_type,
                MemFlags::new(),
                bytes_data,
                layout.stack_vec.len_offset,
            );
            let out = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::Output(output.id),
            )?;
            let copy_block = b.create_block();
            let cont = b.create_block();
            b.append_block_param(copy_block, pointer_type);
            b.append_block_param(copy_block, pointer_type);
            let zero = b.ins().iconst(pointer_type, 0);
            let start_negative = b.ins().icmp_imm(IntCC::SignedLessThan, start, 0);
            let length_positive = b.ins().icmp_imm(IntCC::SignedGreaterThan, length, 0);
            let start_non_negative = b.ins().bnot(start_negative);
            let positive = b.ins().band(start_non_negative, length_positive);
            let positive_block = b.create_block();
            let empty_block = b.create_block();
            b.ins()
                .brif(positive, positive_block, &[], empty_block, &[]);

            b.switch_to_block(positive_block);
            let start_in_bounds = b.ins().icmp(IntCC::UnsignedLessThan, start, bytes_len);
            let in_bounds_block = b.create_block();
            b.ins()
                .brif(start_in_bounds, in_bounds_block, &[], empty_block, &[]);

            b.switch_to_block(in_bounds_block);
            let available = b.ins().isub(bytes_len, start);
            let take_full = b.ins().icmp(IntCC::UnsignedGreaterThan, length, available);
            let actual_len = b.ins().select(take_full, available, length);
            let slice_ptr = b.ins().iadd(bytes_ptr, start);
            b.ins().jump(
                copy_block,
                &[BlockArg::Value(slice_ptr), BlockArg::Value(actual_len)],
            );

            b.switch_to_block(empty_block);
            b.ins().jump(
                copy_block,
                &[BlockArg::Value(bytes_ptr), BlockArg::Value(zero)],
            );

            b.switch_to_block(copy_block);
            let slice_ptr = b.block_params(copy_block)[0];
            let actual_len = b.block_params(copy_block)[1];
            let out_ptr = ssa_call_alloc_buffer(
                b,
                pointer_type,
                heap_refs,
                heap_addrs,
                heap_addrs.alloc_byte_buffer,
                actual_len,
            )?;
            ssa_call_copy_bytes(
                b,
                pointer_type,
                heap_refs,
                heap_addrs,
                out_ptr,
                slice_ptr,
                actual_len,
            )?;
            let out_raw = ssa_call_pack_shared(
                b,
                pointer_type,
                heap_refs,
                heap_addrs.pack_bytes,
                out_ptr,
                actual_len,
                actual_len,
            )?;
            clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, out)?;
            ssa_store_heap_ptr_in_value(b, layout.value, out, layout.value.bytes_tag, out_raw);
            b.ins().jump(cont, &[]);

            b.switch_to_block(cont);
            out
        }
        SsaInstKind::StringGet { text, index } => {
            let text = values[text];
            let index = values[index];
            let string_data = ssa_load_heap_data_ptr(b, layout.value, text);
            let bytes_ptr = b.ins().load(
                pointer_type,
                MemFlags::new(),
                string_data,
                layout.stack_vec.ptr_offset,
            );
            let bytes_len = b.ins().load(
                pointer_type,
                MemFlags::new(),
                string_data,
                layout.stack_vec.len_offset,
            );
            let out = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::Output(output.id),
            )?;
            let loop_block = b.create_block();
            let scan_block = b.create_block();
            let copy_block = b.create_block();
            let advance_block = b.create_block();
            let cont = b.create_block();
            let fail = b.create_block();
            b.append_block_param(loop_block, pointer_type);
            b.append_block_param(loop_block, pointer_type);
            let negative = b.ins().icmp_imm(IntCC::SignedLessThan, index, 0);
            let loop_entry = b.create_block();
            b.ins().brif(negative, fail, &[], loop_entry, &[]);

            b.switch_to_block(loop_entry);
            let zero = b.ins().iconst(pointer_type, 0);
            b.ins()
                .jump(loop_block, &[BlockArg::Value(zero), BlockArg::Value(zero)]);

            b.switch_to_block(loop_block);
            let byte_index = b.block_params(loop_block)[0];
            let char_index = b.block_params(loop_block)[1];
            let past_end = b
                .ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, byte_index, bytes_len);
            b.ins().brif(past_end, fail, &[], scan_block, &[]);

            b.switch_to_block(scan_block);
            let byte_ptr = b.ins().iadd(bytes_ptr, byte_index);
            let byte = ssa_load_byte(b, byte_ptr);
            let width = ssa_utf8_char_width(b, pointer_type, byte);
            let at_target = b.ins().icmp(IntCC::Equal, char_index, index);
            b.ins().brif(at_target, copy_block, &[], advance_block, &[]);

            b.switch_to_block(copy_block);
            let out_ptr = ssa_call_alloc_buffer(
                b,
                pointer_type,
                heap_refs,
                heap_addrs,
                heap_addrs.alloc_byte_buffer,
                width,
            )?;
            ssa_call_copy_bytes(
                b,
                pointer_type,
                heap_refs,
                heap_addrs,
                out_ptr,
                byte_ptr,
                width,
            )?;
            let out_raw = ssa_call_pack_shared(
                b,
                pointer_type,
                heap_refs,
                heap_addrs.pack_string,
                out_ptr,
                width,
                width,
            )?;
            clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, out)?;
            ssa_store_heap_ptr_in_value(b, layout.value, out, layout.value.string_tag, out_raw);
            b.ins().jump(cont, &[]);

            b.switch_to_block(advance_block);
            let next_byte = b.ins().iadd(byte_index, width);
            let next_char = b.ins().iadd_imm(char_index, 1);
            b.ins().jump(
                loop_block,
                &[BlockArg::Value(next_byte), BlockArg::Value(next_char)],
            );

            b.switch_to_block(fail);
            ssa_emit_trace_exit_status(b, vm_ptr, exit_block, pointer_type, offsets, inst.ip)?;

            b.switch_to_block(cont);
            out
        }
        SsaInstKind::BytesGet { bytes, index } => {
            let bytes = values[bytes];
            let index = values[index];
            let vec_ptr = ssa_load_heap_data_ptr(b, layout.value, bytes);
            let len = b.ins().load(
                pointer_type,
                MemFlags::new(),
                vec_ptr,
                layout.stack_vec.len_offset,
            );
            let in_range = ssa_index_in_range(b, index, len);
            let ok = b.create_block();
            let fail = b.create_block();
            let cont = b.create_block();
            b.ins().brif(in_range, ok, &[], fail, &[]);

            b.switch_to_block(ok);
            let data_ptr = b.ins().load(
                pointer_type,
                MemFlags::new(),
                vec_ptr,
                layout.stack_vec.ptr_offset,
            );
            let byte_addr = b.ins().iadd(data_ptr, index);
            let raw = b.ins().load(types::I8, MemFlags::new(), byte_addr, 0);
            let out = b.ins().uextend(types::I64, raw);
            b.ins().jump(cont, &[]);

            b.switch_to_block(fail);
            ssa_emit_trace_exit_status(b, vm_ptr, exit_block, pointer_type, offsets, inst.ip)?;

            b.switch_to_block(cont);
            out
        }
        SsaInstKind::BytesHas { bytes, index } => {
            let bytes = values[bytes];
            let index = values[index];
            let vec_ptr = ssa_load_heap_data_ptr(b, layout.value, bytes);
            let len = b.ins().load(
                pointer_type,
                MemFlags::new(),
                vec_ptr,
                layout.stack_vec.len_offset,
            );
            ssa_index_in_range(b, index, len)
        }
        SsaInstKind::StringContains { text, needle } => {
            let text = values[text];
            let needle = values[needle];
            let raw =
                ssa_call_string_contains(b, pointer_type, string_refs, string_addrs, text, needle)?;
            b.ins().icmp_imm(IntCC::NotEqual, raw, 0)
        }
        SsaInstKind::RegexMatch { pattern, text } => {
            let pattern = values[pattern];
            let text = values[text];
            let raw = ssa_call_regex_match(
                b,
                pointer_type,
                string_refs,
                string_addrs,
                vm_ptr,
                pattern,
                text,
            )?;
            let error = b.ins().icmp_imm(IntCC::SignedLessThan, raw, 0);
            let failed = b.create_block();
            let matched = b.create_block();
            b.ins().brif(error, failed, &[], matched, &[]);
            b.switch_to_block(failed);
            let status = b.ins().iconst(types::I32, STATUS_ERROR as i64);
            jump_with_status(b, exit_block, status);
            b.switch_to_block(matched);
            b.ins().icmp_imm(IntCC::NotEqual, raw, 0)
        }
        SsaInstKind::RegexReplace {
            pattern,
            text,
            replacement,
        } => {
            let pattern = values[pattern];
            let text = values[text];
            let replacement = values[replacement];
            let out_raw = ssa_call_regex_replace(
                b,
                pointer_type,
                string_refs,
                string_addrs,
                vm_ptr,
                pattern,
                text,
                replacement,
            )?;
            let error = b.ins().icmp_imm(IntCC::Equal, out_raw, 0);
            let failed = b.create_block();
            let replaced = b.create_block();
            b.ins().brif(error, failed, &[], replaced, &[]);
            b.switch_to_block(failed);
            let status = b.ins().iconst(types::I32, STATUS_ERROR as i64);
            jump_with_status(b, exit_block, status);
            b.switch_to_block(replaced);
            let out = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::Output(output.id),
            )?;
            clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, out)?;
            ssa_store_heap_ptr_in_value(b, layout.value, out, layout.value.string_tag, out_raw);
            out
        }
        SsaInstKind::StringReplaceLiteral {
            text,
            needle,
            replacement,
        } => {
            let text = values[text];
            let needle = values[needle];
            let replacement = values[replacement];
            let out = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::Output(output.id),
            )?;
            let out_raw = ssa_call_string_replace_literal(
                b,
                pointer_type,
                string_refs,
                string_addrs,
                text,
                needle,
                replacement,
            )?;
            clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, out)?;
            ssa_store_heap_ptr_in_value(b, layout.value, out, layout.value.string_tag, out_raw);
            out
        }
        SsaInstKind::StringLowerAscii { text } => {
            let text = values[text];
            let out = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::Output(output.id),
            )?;
            let out_raw =
                ssa_call_string_lower_ascii(b, pointer_type, string_refs, string_addrs, text)?;
            clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, out)?;
            ssa_store_heap_ptr_in_value(b, layout.value, out, layout.value.string_tag, out_raw);
            out
        }
        SsaInstKind::TypeOf { value } => {
            let value = values[value];
            let out = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::Output(output.id),
            )?;
            let out_raw = ssa_call_type_of(b, pointer_type, string_refs, string_addrs, value)?;
            clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, out)?;
            ssa_store_heap_ptr_in_value(b, layout.value, out, layout.value.string_tag, out_raw);
            out
        }
        SsaInstKind::ToString { value } => {
            let value = values[value];
            let out = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::Output(output.id),
            )?;
            let out_raw = ssa_call_to_string(b, pointer_type, string_refs, string_addrs, value)?;
            clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, out)?;
            ssa_store_heap_ptr_in_value(b, layout.value, out, layout.value.string_tag, out_raw);
            out
        }
        SsaInstKind::StringSplitLiteral { text, delimiter } => {
            let text = values[text];
            let delimiter = values[delimiter];
            let out = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::Output(output.id),
            )?;
            let out_raw = ssa_call_string_split_literal(
                b,
                pointer_type,
                string_refs,
                string_addrs,
                text,
                delimiter,
            )?;
            clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, out)?;
            ssa_store_heap_ptr_in_value(b, layout.value, out, layout.value.array_tag, out_raw);
            out
        }
        SsaInstKind::StringConcat { lhs, rhs } => ssa_inline_concat(
            b,
            ctx,
            SsaConcatOp {
                output_id: output.id,
                ip: inst.ip,
                lhs: values[lhs],
                rhs: values[rhs],
                result_tag: layout.value.string_tag,
                pack_addr: heap_addrs.pack_string,
            },
        )?,
        SsaInstKind::BytesConcat { lhs, rhs } => ssa_inline_concat(
            b,
            ctx,
            SsaConcatOp {
                output_id: output.id,
                ip: inst.ip,
                lhs: values[lhs],
                rhs: values[rhs],
                result_tag: layout.value.bytes_tag,
                pack_addr: heap_addrs.pack_bytes,
            },
        )?,
        SsaInstKind::BytesFromArrayU8 { array } => {
            let array = values[array];
            let vec_ptr = ssa_load_heap_data_ptr(b, layout.value, array);
            let values_ptr = b.ins().load(
                pointer_type,
                MemFlags::new(),
                vec_ptr,
                layout.stack_vec.ptr_offset,
            );
            let values_len = b.ins().load(
                pointer_type,
                MemFlags::new(),
                vec_ptr,
                layout.stack_vec.len_offset,
            );
            let out = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::Output(output.id),
            )?;
            let validate_loop = b.create_block();
            let copy_loop = b.create_block();
            let finish = b.create_block();
            let fail = b.create_block();
            let cont = b.create_block();
            b.append_block_param(validate_loop, pointer_type);
            b.append_block_param(copy_loop, pointer_type);

            let zero = b.ins().iconst(pointer_type, 0);
            b.ins().jump(validate_loop, &[BlockArg::Value(zero)]);

            b.switch_to_block(validate_loop);
            let validate_index = b.block_params(validate_loop)[0];
            let validated = b.ins().icmp(
                IntCC::UnsignedGreaterThanOrEqual,
                validate_index,
                values_len,
            );
            let validate_step = b.create_block();
            let allocate = b.create_block();
            b.ins().brif(validated, allocate, &[], validate_step, &[]);

            b.switch_to_block(validate_step);
            let element_addr = ssa_value_addr(
                b,
                pointer_type,
                values_ptr,
                validate_index,
                layout.value.size,
            );
            let element_tag = ssa_load_tag_i32(b, layout.value, element_addr);
            let is_int =
                b.ins()
                    .icmp_imm(IntCC::Equal, element_tag, i64::from(layout.value.int_tag));
            let int_ok = b.create_block();
            b.ins().brif(is_int, int_ok, &[], fail, &[]);

            b.switch_to_block(int_ok);
            let value = b.ins().load(
                types::I64,
                MemFlags::new(),
                element_addr,
                layout.value.int_payload_offset,
            );
            let non_negative = b.ins().icmp_imm(IntCC::SignedGreaterThanOrEqual, value, 0);
            let le_255 = b.ins().icmp_imm(IntCC::SignedLessThanOrEqual, value, 255);
            let valid_byte = b.ins().band(non_negative, le_255);
            let validate_next = b.create_block();
            b.ins().brif(valid_byte, validate_next, &[], fail, &[]);

            b.switch_to_block(validate_next);
            let next_index = b.ins().iadd_imm(validate_index, 1);
            b.ins().jump(validate_loop, &[BlockArg::Value(next_index)]);

            b.switch_to_block(allocate);
            let out_ptr = ssa_call_alloc_buffer(
                b,
                pointer_type,
                heap_refs,
                heap_addrs,
                heap_addrs.alloc_byte_buffer,
                values_len,
            )?;
            b.ins().jump(copy_loop, &[BlockArg::Value(zero)]);

            b.switch_to_block(copy_loop);
            let copy_index = b.block_params(copy_loop)[0];
            let copy_done = b
                .ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, copy_index, values_len);
            let copy_step = b.create_block();
            b.ins().brif(copy_done, finish, &[], copy_step, &[]);

            b.switch_to_block(copy_step);
            let element_addr =
                ssa_value_addr(b, pointer_type, values_ptr, copy_index, layout.value.size);
            let value = b.ins().load(
                types::I64,
                MemFlags::new(),
                element_addr,
                layout.value.int_payload_offset,
            );
            let byte = b.ins().ireduce(types::I8, value);
            let dst = b.ins().iadd(out_ptr, copy_index);
            b.ins().store(MemFlags::new(), byte, dst, 0);
            let next_index = b.ins().iadd_imm(copy_index, 1);
            b.ins().jump(copy_loop, &[BlockArg::Value(next_index)]);

            b.switch_to_block(finish);
            let out_raw = ssa_call_pack_shared(
                b,
                pointer_type,
                heap_refs,
                heap_addrs.pack_bytes,
                out_ptr,
                values_len,
                values_len,
            )?;
            clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, out)?;
            ssa_store_heap_ptr_in_value(b, layout.value, out, layout.value.bytes_tag, out_raw);
            b.ins().jump(cont, &[]);

            b.switch_to_block(fail);
            ssa_emit_trace_exit_status(b, vm_ptr, exit_block, pointer_type, offsets, inst.ip)?;

            b.switch_to_block(cont);
            out
        }
        SsaInstKind::BytesToUtf8Ascii { bytes } => {
            let bytes = values[bytes];
            let bytes_data = ssa_load_heap_data_ptr(b, layout.value, bytes);
            let bytes_ptr = b.ins().load(
                pointer_type,
                MemFlags::new(),
                bytes_data,
                layout.stack_vec.ptr_offset,
            );
            let bytes_len = b.ins().load(
                pointer_type,
                MemFlags::new(),
                bytes_data,
                layout.stack_vec.len_offset,
            );
            let out = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::Output(output.id),
            )?;
            let validate_loop = b.create_block();
            let validate_step = b.create_block();
            let finish = b.create_block();
            let fail = b.create_block();
            let cont = b.create_block();
            b.append_block_param(validate_loop, pointer_type);

            let zero = b.ins().iconst(pointer_type, 0);
            b.ins().jump(validate_loop, &[BlockArg::Value(zero)]);

            b.switch_to_block(validate_loop);
            let index = b.block_params(validate_loop)[0];
            let done = b
                .ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, index, bytes_len);
            b.ins().brif(done, finish, &[], validate_step, &[]);

            b.switch_to_block(validate_step);
            let byte_addr = b.ins().iadd(bytes_ptr, index);
            let byte = b.ins().load(types::I8, MemFlags::new(), byte_addr, 0);
            let is_ascii = b.ins().icmp_imm(IntCC::UnsignedLessThan, byte, 128);
            let validate_next = b.create_block();
            b.ins().brif(is_ascii, validate_next, &[], fail, &[]);

            b.switch_to_block(validate_next);
            let next_index = b.ins().iadd_imm(index, 1);
            b.ins().jump(validate_loop, &[BlockArg::Value(next_index)]);

            b.switch_to_block(finish);
            let out_ptr = ssa_call_alloc_buffer(
                b,
                pointer_type,
                heap_refs,
                heap_addrs,
                heap_addrs.alloc_byte_buffer,
                bytes_len,
            )?;
            ssa_call_copy_bytes(
                b,
                pointer_type,
                heap_refs,
                heap_addrs,
                out_ptr,
                bytes_ptr,
                bytes_len,
            )?;
            let out_raw = ssa_call_pack_shared(
                b,
                pointer_type,
                heap_refs,
                heap_addrs.pack_string,
                out_ptr,
                bytes_len,
                bytes_len,
            )?;
            clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, out)?;
            ssa_store_heap_ptr_in_value(b, layout.value, out, layout.value.string_tag, out_raw);
            b.ins().jump(cont, &[]);

            b.switch_to_block(fail);
            ssa_emit_trace_exit_status(b, vm_ptr, exit_block, pointer_type, offsets, inst.ip)?;

            b.switch_to_block(cont);
            out
        }
        SsaInstKind::BytesToArrayU8 { bytes } => {
            let bytes = values[bytes];
            let bytes_data = ssa_load_heap_data_ptr(b, layout.value, bytes);
            let bytes_ptr = b.ins().load(
                pointer_type,
                MemFlags::new(),
                bytes_data,
                layout.stack_vec.ptr_offset,
            );
            let bytes_len = b.ins().load(
                pointer_type,
                MemFlags::new(),
                bytes_data,
                layout.stack_vec.len_offset,
            );
            let out = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::Output(output.id),
            )?;
            let fill_loop = b.create_block();
            let finish = b.create_block();
            let fail = b.create_block();
            let cont = b.create_block();
            b.append_block_param(fill_loop, pointer_type);

            let value_size = i64::from(layout.value.size);
            let max_values = b.ins().iconst(pointer_type, i64::MAX / value_size);
            let too_large = b
                .ins()
                .icmp(IntCC::UnsignedGreaterThan, bytes_len, max_values);
            let cap_ok = b.create_block();
            b.ins().brif(too_large, fail, &[], cap_ok, &[]);

            b.switch_to_block(cap_ok);
            let out_ptr = ssa_call_alloc_buffer(
                b,
                pointer_type,
                heap_refs,
                heap_addrs,
                heap_addrs.alloc_value_buffer,
                bytes_len,
            )?;
            let total_bytes = b.ins().imul_imm(bytes_len, value_size);
            ssa_call_zero_bytes(b, pointer_type, heap_refs, heap_addrs, out_ptr, total_bytes)?;
            let zero = b.ins().iconst(pointer_type, 0);
            b.ins().jump(fill_loop, &[BlockArg::Value(zero)]);

            b.switch_to_block(fill_loop);
            let fill_index = b.block_params(fill_loop)[0];
            let done = b
                .ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, fill_index, bytes_len);
            let fill_step = b.create_block();
            b.ins().brif(done, finish, &[], fill_step, &[]);

            b.switch_to_block(fill_step);
            let src_ptr = b.ins().iadd(bytes_ptr, fill_index);
            let byte = ssa_load_byte(b, src_ptr);
            let byte_i64 = b.ins().uextend(types::I64, byte);
            let element_addr =
                ssa_value_addr(b, pointer_type, out_ptr, fill_index, layout.value.size);
            ssa_store_int_in_value(b, layout.value, element_addr, byte_i64);
            let next_index = b.ins().iadd_imm(fill_index, 1);
            b.ins().jump(fill_loop, &[BlockArg::Value(next_index)]);

            b.switch_to_block(finish);
            let out_raw = ssa_call_pack_shared(
                b,
                pointer_type,
                heap_refs,
                heap_addrs.pack_array,
                out_ptr,
                bytes_len,
                bytes_len,
            )?;
            clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, out)?;
            ssa_store_heap_ptr_in_value(b, layout.value, out, layout.value.array_tag, out_raw);
            b.ins().jump(cont, &[]);

            b.switch_to_block(fail);
            ssa_emit_trace_exit_status(b, vm_ptr, exit_block, pointer_type, offsets, inst.ip)?;

            b.switch_to_block(cont);
            out
        }
        SsaInstKind::ArrayNew => {
            let out = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::Output(output.id),
            )?;
            let zero = b.ins().iconst(pointer_type, 0);
            let out_ptr = ssa_call_alloc_buffer(
                b,
                pointer_type,
                heap_refs,
                heap_addrs,
                heap_addrs.alloc_value_buffer,
                zero,
            )?;
            let out_raw = ssa_call_pack_shared(
                b,
                pointer_type,
                heap_refs,
                heap_addrs.pack_array,
                out_ptr,
                zero,
                zero,
            )?;
            clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, out)?;
            ssa_store_heap_ptr_in_value(b, layout.value, out, layout.value.array_tag, out_raw);
            out
        }
        SsaInstKind::ArrayLen { array } => {
            let array = values[array];
            let vec_ptr = ssa_load_heap_data_ptr(b, layout.value, array);
            b.ins().load(
                pointer_type,
                MemFlags::new(),
                vec_ptr,
                layout.stack_vec.len_offset,
            )
        }
        SsaInstKind::ArrayGet { array, index } => {
            let array = values[array];
            let index = values[index];
            let vec_ptr = ssa_load_heap_data_ptr(b, layout.value, array);
            let len = b.ins().load(
                pointer_type,
                MemFlags::new(),
                vec_ptr,
                layout.stack_vec.len_offset,
            );
            let in_range = ssa_index_in_range(b, index, len);
            let ok = b.create_block();
            let fail = b.create_block();
            let done = b.create_block();
            b.ins().brif(in_range, ok, &[], fail, &[]);

            b.switch_to_block(ok);
            let data_ptr = b.ins().load(
                pointer_type,
                MemFlags::new(),
                vec_ptr,
                layout.stack_vec.ptr_offset,
            );
            let element_addr = ssa_value_addr(b, pointer_type, data_ptr, index, layout.value.size);
            let out = if borrowed_array_gets.contains(&output.id) {
                element_addr
            } else {
                let out = owned_value_temp_slot_addr(
                    b,
                    pointer_type,
                    owned_value_temps,
                    SsaTempValueSlotKey::Output(output.id),
                )?;
                let tag = ssa_load_tag_i32(b, layout.value, element_addr);
                let scalar = ssa_is_scalar_tag(b, layout.value, tag);
                let fast = b.create_block();
                let slow = b.create_block();
                let clone_done = b.create_block();
                b.ins().brif(scalar, fast, &[], slow, &[]);

                b.switch_to_block(fast);
                clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, out)?;
                ssa_copy_value_bytes(b, element_addr, out, layout.value.size);
                b.ins().jump(clone_done, &[]);

                b.switch_to_block(slow);
                clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, out)?;
                ssa_call_status_helper(
                    b,
                    exit_block,
                    pointer_type,
                    helper_refs.clone_value_ref,
                    helper_addrs.clone_value,
                    &[out, element_addr],
                )?;
                b.ins().jump(clone_done, &[]);

                b.switch_to_block(clone_done);
                out
            };
            b.ins().jump(done, &[]);

            b.switch_to_block(fail);
            ssa_emit_trace_exit_status(b, vm_ptr, exit_block, pointer_type, offsets, inst.ip)?;

            b.switch_to_block(done);
            out
        }
        SsaInstKind::ArrayHas { array, index } => {
            let array = values[array];
            let index = values[index];
            let vec_ptr = ssa_load_heap_data_ptr(b, layout.value, array);
            let len = b.ins().load(
                pointer_type,
                MemFlags::new(),
                vec_ptr,
                layout.stack_vec.len_offset,
            );
            ssa_index_in_range(b, index, len)
        }
        SsaInstKind::ArraySet {
            array,
            index,
            value,
        } => {
            let out = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::Output(output.id),
            )?;
            if value_reprs.get(index) != Some(&SsaValueRepr::I64) {
                return Err(VmError::JitNative(
                    "SSA array-set index must be lowered as i64".to_string(),
                ));
            }
            let value_box = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::MutationArgBox(output.id, 1),
            )?;
            let value_addr = ssa_ensure_boxed_value_addr(
                b,
                SsaBoxCtx {
                    exit_block,
                    pointer_type,
                    value_layout: layout.value,
                    helper_refs,
                    helper_addrs,
                },
                Some(value_box),
                *value_reprs.get(value).ok_or_else(|| {
                    VmError::JitNative("SSA array-set value representation missing".to_string())
                })?,
                values[value],
            )?;
            let helper_ptr = iconst_ptr_from_addr(b, pointer_type, helper_addrs.array_set)?;
            let call = b.ins().call_indirect(
                helper_refs.array_set_ref,
                helper_ptr,
                &[out, values[array], values[index], value_addr],
            );
            let status = b.inst_results(call)[0];
            let fail = b.create_block();
            let cont = b.create_block();
            let is_error = b
                .ins()
                .icmp_imm(IntCC::Equal, status, i64::from(STATUS_ERROR));
            b.ins().brif(is_error, fail, &[], cont, &[]);

            b.switch_to_block(fail);
            jump_with_status(b, exit_block, status);

            b.switch_to_block(cont);
            out
        }
        SsaInstKind::ArrayPush { array, value } => {
            let out = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::Output(output.id),
            )?;
            let value_box = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::MutationArgBox(output.id, 0),
            )?;
            let value_addr = ssa_ensure_boxed_value_addr(
                b,
                SsaBoxCtx {
                    exit_block,
                    pointer_type,
                    value_layout: layout.value,
                    helper_refs,
                    helper_addrs,
                },
                Some(value_box),
                *value_reprs.get(value).ok_or_else(|| {
                    VmError::JitNative("SSA array-push value representation missing".to_string())
                })?,
                values[value],
            )?;
            let helper_ptr = iconst_ptr_from_addr(b, pointer_type, helper_addrs.array_push)?;
            let call = b.ins().call_indirect(
                helper_refs.array_push_ref,
                helper_ptr,
                &[out, values[array], value_addr],
            );
            let status = b.inst_results(call)[0];
            let fail = b.create_block();
            let cont = b.create_block();
            let is_error = b
                .ins()
                .icmp_imm(IntCC::Equal, status, i64::from(STATUS_ERROR));
            b.ins().brif(is_error, fail, &[], cont, &[]);

            b.switch_to_block(fail);
            jump_with_status(b, exit_block, status);

            b.switch_to_block(cont);
            out
        }
        SsaInstKind::MapLen { map } => {
            let map_ptr = ssa_load_heap_data_ptr(b, layout.value, values[map]);
            b.ins().load(
                pointer_type,
                MemFlags::new(),
                map_ptr,
                layout.map.len_offset,
            )
        }
        SsaInstKind::MapGet { map, key } => {
            let key_box_slot = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::MapKeyBox(output.id),
            )?;
            let key_addr = ssa_ensure_boxed_value_addr(
                b,
                SsaBoxCtx {
                    exit_block,
                    pointer_type,
                    value_layout: layout.value,
                    helper_refs,
                    helper_addrs,
                },
                Some(key_box_slot),
                *value_reprs.get(key).ok_or_else(|| {
                    VmError::JitNative("SSA map-get key representation missing".to_string())
                })?,
                values[key],
            )?;
            let out = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::Output(output.id),
            )?;
            clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, out)?;
            let helper_ptr = iconst_ptr_from_addr(b, pointer_type, helper_addrs.map_get)?;
            let call = b.ins().call_indirect(
                helper_refs.map_get_ref,
                helper_ptr,
                &[out, values[map], key_addr],
            );
            let status = b.inst_results(call)[0];
            let ok = b.create_block();
            let fail = b.create_block();
            let cont = b.create_block();
            let is_error = b
                .ins()
                .icmp_imm(IntCC::Equal, status, i64::from(STATUS_ERROR));
            let is_found = b.ins().icmp_imm(IntCC::Equal, status, 1);
            b.ins().brif(is_error, fail, &[], ok, &[]);

            b.switch_to_block(ok);
            b.ins().brif(is_found, cont, &[], fail, &[]);

            b.switch_to_block(fail);
            let is_status_error = b
                .ins()
                .icmp_imm(IntCC::Equal, status, i64::from(STATUS_ERROR));
            let error_block = b.create_block();
            let miss_block = b.create_block();
            b.ins()
                .brif(is_status_error, error_block, &[], miss_block, &[]);

            b.switch_to_block(error_block);
            jump_with_status(b, exit_block, status);

            b.switch_to_block(miss_block);
            ssa_emit_trace_exit_status(b, vm_ptr, exit_block, pointer_type, offsets, inst.ip)?;

            b.switch_to_block(cont);
            out
        }
        SsaInstKind::MapHas { map, key } => {
            let key_box_slot = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::MapKeyBox(output.id),
            )?;
            let key_addr = ssa_ensure_boxed_value_addr(
                b,
                SsaBoxCtx {
                    exit_block,
                    pointer_type,
                    value_layout: layout.value,
                    helper_refs,
                    helper_addrs,
                },
                Some(key_box_slot),
                *value_reprs.get(key).ok_or_else(|| {
                    VmError::JitNative("SSA map-has key representation missing".to_string())
                })?,
                values[key],
            )?;
            let helper_ptr = iconst_ptr_from_addr(b, pointer_type, helper_addrs.map_has)?;
            let call = b.ins().call_indirect(
                helper_refs.map_has_ref,
                helper_ptr,
                &[values[map], key_addr],
            );
            let status = b.inst_results(call)[0];
            let fail = b.create_block();
            let cont = b.create_block();
            let is_error = b
                .ins()
                .icmp_imm(IntCC::Equal, status, i64::from(STATUS_ERROR));
            b.ins().brif(is_error, fail, &[], cont, &[]);

            b.switch_to_block(fail);
            jump_with_status(b, exit_block, status);

            b.switch_to_block(cont);
            b.ins().icmp_imm(IntCC::NotEqual, status, 0)
        }
        SsaInstKind::MapIterNext { slot } => {
            let helper_ptr = iconst_ptr_from_addr(b, pointer_type, helper_addrs.map_iter_next)?;
            let call = b.ins().call_indirect(
                helper_refs.map_iter_next_ref,
                helper_ptr,
                &[vm_ptr, values[slot]],
            );
            let status = b.inst_results(call)[0];
            let fail = b.create_block();
            let cont = b.create_block();
            let is_error = b
                .ins()
                .icmp_imm(IntCC::Equal, status, i64::from(STATUS_ERROR));
            b.ins().brif(is_error, fail, &[], cont, &[]);
            b.switch_to_block(fail);
            jump_with_status(b, exit_block, status);
            b.switch_to_block(cont);
            b.ins().icmp_imm(IntCC::NotEqual, status, 0)
        }
        SsaInstKind::MapIterTakeKey { slot } | SsaInstKind::MapIterTakeValue { slot } => {
            let out = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::Output(output.id),
            )?;
            clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, out)?;
            let (helper_ref, helper_addr) = match inst.kind {
                SsaInstKind::MapIterTakeKey { .. } => (
                    helper_refs.map_iter_take_key_ref,
                    helper_addrs.map_iter_take_key,
                ),
                _ => (
                    helper_refs.map_iter_take_value_ref,
                    helper_addrs.map_iter_take_value,
                ),
            };
            let helper_ptr = iconst_ptr_from_addr(b, pointer_type, helper_addr)?;
            let call = b
                .ins()
                .call_indirect(helper_ref, helper_ptr, &[out, vm_ptr, values[slot]]);
            let status = b.inst_results(call)[0];
            let fail = b.create_block();
            let cont = b.create_block();
            let is_error = b
                .ins()
                .icmp_imm(IntCC::Equal, status, i64::from(STATUS_ERROR));
            b.ins().brif(is_error, fail, &[], cont, &[]);
            b.switch_to_block(fail);
            jump_with_status(b, exit_block, status);
            b.switch_to_block(cont);
            out
        }
        SsaInstKind::MapSet { map, key, value } => {
            let out = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::Output(output.id),
            )?;
            let key_box = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::MutationArgBox(output.id, 0),
            )?;
            let key_addr = ssa_ensure_boxed_value_addr(
                b,
                SsaBoxCtx {
                    exit_block,
                    pointer_type,
                    value_layout: layout.value,
                    helper_refs,
                    helper_addrs,
                },
                Some(key_box),
                *value_reprs.get(key).ok_or_else(|| {
                    VmError::JitNative("SSA map-set key representation missing".to_string())
                })?,
                values[key],
            )?;
            let value_box = owned_value_temp_slot_addr(
                b,
                pointer_type,
                owned_value_temps,
                SsaTempValueSlotKey::MutationArgBox(output.id, 1),
            )?;
            let value_addr = ssa_ensure_boxed_value_addr(
                b,
                SsaBoxCtx {
                    exit_block,
                    pointer_type,
                    value_layout: layout.value,
                    helper_refs,
                    helper_addrs,
                },
                Some(value_box),
                *value_reprs.get(value).ok_or_else(|| {
                    VmError::JitNative("SSA map-set value representation missing".to_string())
                })?,
                values[value],
            )?;
            let helper_ptr = iconst_ptr_from_addr(b, pointer_type, helper_addrs.map_set)?;
            let call = b.ins().call_indirect(
                helper_refs.map_set_ref,
                helper_ptr,
                &[out, values[map], key_addr, value_addr],
            );
            let status = b.inst_results(call)[0];
            let fail = b.create_block();
            let cont = b.create_block();
            let is_error = b
                .ins()
                .icmp_imm(IntCC::Equal, status, i64::from(STATUS_ERROR));
            b.ins().brif(is_error, fail, &[], cont, &[]);

            b.switch_to_block(fail);
            jump_with_status(b, exit_block, status);

            b.switch_to_block(cont);
            out
        }
        SsaInstKind::HostCall { import, args } => {
            let scalar_result = (output.repr != SsaValueRepr::Tagged)
                .then(|| {
                    owned_value_temp_slot_addr(
                        b,
                        pointer_type,
                        owned_value_temps,
                        SsaTempValueSlotKey::HostScalarResult(output.id),
                    )
                })
                .transpose()?;
            let import = b.ins().iconst(pointer_type, i64::from(*import));
            let argc = b.ins().iconst(
                pointer_type,
                i64::try_from(args.len()).map_err(|_| {
                    VmError::JitNative("SSA host-call argument count out of range".to_string())
                })?,
            );

            if let Some(out) = scalar_result
                && host_call_has_direct_i64_args(args, value_reprs)
            {
                let zero = b.ins().iconst(types::I64, 0);
                let arg0 = args.first().map_or(zero, |arg| values[arg]);
                let arg1 = args.get(1).map_or(zero, |arg| values[arg]);
                let return_type = b
                    .ins()
                    .iconst(types::I64, scalar_host_return_type(output.repr)? as i64);
                ssa_call_status_helper(
                    b,
                    exit_block,
                    pointer_type,
                    helper_refs.non_yielding_i64_host_call_ref,
                    helper_addrs.non_yielding_i64_host_call,
                    &[vm_ptr, import, arg0, arg1, argc, return_type, out],
                )?;
                ssa_load_scalar_host_result(b, output.repr, out)?
            } else {
                let arg_values = owned_value_temp_slot_addr(
                    b,
                    pointer_type,
                    owned_value_temps,
                    SsaTempValueSlotKey::HostArgs(output.id),
                )?;
                for (index, arg) in args.iter().copied().enumerate() {
                    let repr = *value_reprs.get(&arg).ok_or_else(|| {
                        VmError::JitNative(
                            "SSA host-call argument representation missing".to_string(),
                        )
                    })?;
                    let value = values[&arg];
                    let index_value = b.ins().iconst(
                        pointer_type,
                        i64::try_from(index).map_err(|_| {
                            VmError::JitNative(
                                "SSA host-call argument index out of range".to_string(),
                            )
                        })?,
                    );
                    let addr =
                        ssa_value_addr(b, pointer_type, arg_values, index_value, layout.value.size);
                    match repr {
                        SsaValueRepr::Tagged => {
                            ssa_copy_value_bytes(b, value, addr, layout.value.size)
                        }
                        SsaValueRepr::I64 => ssa_store_int_in_value(b, layout.value, addr, value),
                        SsaValueRepr::F64 => ssa_store_float_in_value(b, layout.value, addr, value),
                        SsaValueRepr::Bool => ssa_store_bool_in_value(b, layout.value, addr, value),
                        SsaValueRepr::HeapPtr(tag) => {
                            let tag = ssa_heap_tag(layout.value, tag)?;
                            ssa_store_heap_ptr_in_value(b, layout.value, addr, tag, value);
                        }
                    }
                }
                if let Some(out) = scalar_result {
                    let return_type = b
                        .ins()
                        .iconst(pointer_type, scalar_host_return_type(output.repr)? as i64);
                    ssa_call_status_helper(
                        b,
                        exit_block,
                        pointer_type,
                        helper_refs.non_yielding_scalar_host_call_ref,
                        helper_addrs.non_yielding_scalar_host_call,
                        &[vm_ptr, import, arg_values, argc, return_type, out],
                    )?;
                    ssa_load_scalar_host_result(b, output.repr, out)?
                } else {
                    let out = owned_value_temp_slot_addr(
                        b,
                        pointer_type,
                        owned_value_temps,
                        SsaTempValueSlotKey::Output(output.id),
                    )?;
                    clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, out)?;
                    ssa_call_status_helper(
                        b,
                        exit_block,
                        pointer_type,
                        helper_refs.non_yielding_host_call_ref,
                        helper_addrs.non_yielding_host_call,
                        &[vm_ptr, import, arg_values, argc, out],
                    )?;
                    out
                }
            }
        }
        SsaInstKind::IntNeg { input } => b.ins().ineg(values[input]),
        SsaInstKind::IntAdd { lhs, rhs } => b.ins().iadd(values[lhs], values[rhs]),
        SsaInstKind::IntAddImm { lhs, imm } => b.ins().iadd_imm(values[lhs], *imm),
        SsaInstKind::IntSub { lhs, rhs } => b.ins().isub(values[lhs], values[rhs]),
        SsaInstKind::IntSubImm { lhs, imm } => {
            let rhs = b.ins().iconst(types::I64, *imm);
            b.ins().isub(values[lhs], rhs)
        }
        SsaInstKind::IntMul { lhs, rhs } => b.ins().imul(values[lhs], values[rhs]),
        SsaInstKind::IntMulImm { lhs, imm } => {
            let rhs = b.ins().iconst(types::I64, *imm);
            b.ins().imul(values[lhs], rhs)
        }
        SsaInstKind::IntDiv { lhs, rhs } => {
            let lhs_value = values[lhs];
            let rhs_value = values[rhs];
            let non_zero = b.ins().icmp_imm(IntCC::NotEqual, rhs_value, 0);
            let check_overflow = b.create_block();
            let fail = b.create_block();
            let cont = b.create_block();
            b.ins().brif(non_zero, check_overflow, &[], fail, &[]);

            b.switch_to_block(check_overflow);
            let rhs_is_neg_one = b.ins().icmp_imm(IntCC::Equal, rhs_value, -1);
            let lhs_is_min = b.ins().icmp_imm(IntCC::Equal, lhs_value, i64::MIN);
            let overflow = b.ins().band(rhs_is_neg_one, lhs_is_min);
            let div_ok = b.create_block();
            b.ins().brif(overflow, fail, &[], div_ok, &[]);

            b.switch_to_block(div_ok);
            let out = b.ins().sdiv(lhs_value, rhs_value);
            b.ins().jump(cont, &[]);

            b.switch_to_block(fail);
            ssa_emit_trace_exit_status(b, vm_ptr, exit_block, pointer_type, offsets, inst.ip)?;

            b.switch_to_block(cont);
            out
        }
        SsaInstKind::IntDivImm { lhs, imm } => {
            if *imm == 0 || *imm == -1 {
                return Err(VmError::JitNative(
                    "SSA native lowering does not support unsafe integer div immediates"
                        .to_string(),
                ));
            }
            let rhs = b.ins().iconst(types::I64, *imm);
            b.ins().sdiv(values[lhs], rhs)
        }
        SsaInstKind::IntMod { lhs, rhs } => {
            let lhs_value = values[lhs];
            let rhs_value = values[rhs];
            let non_zero = b.ins().icmp_imm(IntCC::NotEqual, rhs_value, 0);
            let check_overflow = b.create_block();
            let fail = b.create_block();
            let cont = b.create_block();
            b.ins().brif(non_zero, check_overflow, &[], fail, &[]);

            b.switch_to_block(check_overflow);
            let rhs_is_neg_one = b.ins().icmp_imm(IntCC::Equal, rhs_value, -1);
            let lhs_is_min = b.ins().icmp_imm(IntCC::Equal, lhs_value, i64::MIN);
            let overflow = b.ins().band(rhs_is_neg_one, lhs_is_min);
            let mod_ok = b.create_block();
            b.ins().brif(overflow, fail, &[], mod_ok, &[]);

            b.switch_to_block(mod_ok);
            let out = b.ins().srem(lhs_value, rhs_value);
            b.ins().jump(cont, &[]);

            b.switch_to_block(fail);
            ssa_emit_trace_exit_status(b, vm_ptr, exit_block, pointer_type, offsets, inst.ip)?;

            b.switch_to_block(cont);
            out
        }
        SsaInstKind::IntModImm { lhs, imm } => {
            if *imm == 0 {
                return Err(VmError::JitNative(
                    "SSA native lowering does not support modulo-by-zero immediates".to_string(),
                ));
            }
            let rhs = b.ins().iconst(types::I64, *imm);
            b.ins().srem(values[lhs], rhs)
        }
        SsaInstKind::IntShl { lhs, rhs } => {
            let rhs_value = values[rhs];
            let shift_ge_zero = b
                .ins()
                .icmp_imm(IntCC::SignedGreaterThanOrEqual, rhs_value, 0);
            let shift_le_63 = b
                .ins()
                .icmp_imm(IntCC::SignedLessThanOrEqual, rhs_value, 63);
            let shift_in_range = b.ins().band(shift_ge_zero, shift_le_63);
            let shift_ok = b.create_block();
            let fail = b.create_block();
            let cont = b.create_block();
            b.ins().brif(shift_in_range, shift_ok, &[], fail, &[]);

            b.switch_to_block(shift_ok);
            let out = b.ins().ishl(values[lhs], rhs_value);
            b.ins().jump(cont, &[]);

            b.switch_to_block(fail);
            ssa_emit_trace_exit_status(b, vm_ptr, exit_block, pointer_type, offsets, inst.ip)?;

            b.switch_to_block(cont);
            out
        }
        SsaInstKind::IntShlImm { lhs, amount } => {
            let rhs = b.ins().iconst(types::I64, i64::from(*amount));
            b.ins().ishl(values[lhs], rhs)
        }
        SsaInstKind::IntShr { lhs, rhs } => {
            let rhs_value = values[rhs];
            let shift_ge_zero = b
                .ins()
                .icmp_imm(IntCC::SignedGreaterThanOrEqual, rhs_value, 0);
            let shift_le_63 = b
                .ins()
                .icmp_imm(IntCC::SignedLessThanOrEqual, rhs_value, 63);
            let shift_in_range = b.ins().band(shift_ge_zero, shift_le_63);
            let shift_ok = b.create_block();
            let fail = b.create_block();
            let cont = b.create_block();
            b.ins().brif(shift_in_range, shift_ok, &[], fail, &[]);

            b.switch_to_block(shift_ok);
            let out = b.ins().sshr(values[lhs], rhs_value);
            b.ins().jump(cont, &[]);

            b.switch_to_block(fail);
            ssa_emit_trace_exit_status(b, vm_ptr, exit_block, pointer_type, offsets, inst.ip)?;

            b.switch_to_block(cont);
            out
        }
        SsaInstKind::IntShrImm { lhs, amount } => {
            let rhs = b.ins().iconst(types::I64, i64::from(*amount));
            b.ins().sshr(values[lhs], rhs)
        }
        SsaInstKind::IntLshr { lhs, rhs } => {
            let rhs_value = values[rhs];
            let shift_ge_zero = b
                .ins()
                .icmp_imm(IntCC::SignedGreaterThanOrEqual, rhs_value, 0);
            let shift_le_63 = b
                .ins()
                .icmp_imm(IntCC::SignedLessThanOrEqual, rhs_value, 63);
            let shift_in_range = b.ins().band(shift_ge_zero, shift_le_63);
            let shift_ok = b.create_block();
            let fail = b.create_block();
            let cont = b.create_block();
            b.ins().brif(shift_in_range, shift_ok, &[], fail, &[]);

            b.switch_to_block(shift_ok);
            let out = b.ins().ushr(values[lhs], rhs_value);
            b.ins().jump(cont, &[]);

            b.switch_to_block(fail);
            ssa_emit_trace_exit_status(b, vm_ptr, exit_block, pointer_type, offsets, inst.ip)?;

            b.switch_to_block(cont);
            out
        }
        SsaInstKind::IntLshrImm { lhs, amount } => {
            let rhs = b.ins().iconst(types::I64, i64::from(*amount));
            b.ins().ushr(values[lhs], rhs)
        }
        SsaInstKind::BoolAnd { lhs, rhs } => b.ins().band(values[lhs], values[rhs]),
        SsaInstKind::BoolOr { lhs, rhs } => b.ins().bor(values[lhs], values[rhs]),
        SsaInstKind::BoolNot { input } => b.ins().icmp_imm(IntCC::Equal, values[input], 0),
        SsaInstKind::FloatNeg { input } => {
            let zero = b.ins().f64const(Ieee64::with_float(0.0));
            b.ins().fsub(zero, values[input])
        }
        SsaInstKind::FloatAdd { lhs, rhs } => b.ins().fadd(values[lhs], values[rhs]),
        SsaInstKind::FloatSub { lhs, rhs } => b.ins().fsub(values[lhs], values[rhs]),
        SsaInstKind::FloatMul { lhs, rhs } => b.ins().fmul(values[lhs], values[rhs]),
        SsaInstKind::FloatDiv { lhs, rhs } => b.ins().fdiv(values[lhs], values[rhs]),
        SsaInstKind::FloatMod { lhs, rhs } => {
            let quotient = b.ins().fdiv(values[lhs], values[rhs]);
            let truncated = b.ins().trunc(quotient);
            let product = b.ins().fmul(truncated, values[rhs]);
            b.ins().fsub(values[lhs], product)
        }
        SsaInstKind::FloatCmpEq { lhs, rhs } => {
            b.ins().fcmp(FloatCC::Equal, values[lhs], values[rhs])
        }
        SsaInstKind::FloatCmpLt { lhs, rhs } => {
            b.ins().fcmp(FloatCC::LessThan, values[lhs], values[rhs])
        }
        SsaInstKind::FloatCmpGt { lhs, rhs } => {
            b.ins().fcmp(FloatCC::GreaterThan, values[lhs], values[rhs])
        }
        SsaInstKind::IntCmpEq { lhs, rhs } => b.ins().icmp(IntCC::Equal, values[lhs], values[rhs]),
        SsaInstKind::ValueCmpEq { lhs, rhs } => {
            let raw = ssa_call_value_eq(
                b,
                pointer_type,
                helper_refs,
                helper_addrs,
                values[lhs],
                values[rhs],
            )?;
            b.ins().icmp_imm(IntCC::NotEqual, raw, 0)
        }
        SsaInstKind::IntCmpLt { lhs, rhs } => {
            b.ins()
                .icmp(IntCC::SignedLessThan, values[lhs], values[rhs])
        }
        SsaInstKind::IntCmpLtImm { lhs, imm } => {
            b.ins().icmp_imm(IntCC::SignedLessThan, values[lhs], *imm)
        }
        SsaInstKind::IntCmpGt { lhs, rhs } => {
            b.ins()
                .icmp(IntCC::SignedGreaterThan, values[lhs], values[rhs])
        }
        SsaInstKind::IntCmpGtImm { lhs, imm } => {
            b.ins()
                .icmp_imm(IntCC::SignedGreaterThan, values[lhs], *imm)
        }
    };
    values.insert(output.id, lowered);
    Ok(())
}

fn allocate_internal_link_slots(
    b: &mut FunctionBuilder,
    link: &FusedRegionLink,
    value_size: i32,
) -> VmResult<Vec<Option<StackSlot>>> {
    link.args
        .iter()
        .map(|materialization| match materialization {
            SsaMaterialization::Value(_) => Ok(None),
            SsaMaterialization::BoxInt(_)
            | SsaMaterialization::BoxFloat(_)
            | SsaMaterialization::BoxBool(_) => {
                ssa_create_value_stack_slot(b, value_size).map(Some)
            }
            SsaMaterialization::BoxHeapPtr { .. } => Err(VmError::JitNative(
                "fused SSA link does not support heap ownership transfer".to_string(),
            )),
        })
        .collect()
}

fn lower_internal_link_args(
    b: &mut FunctionBuilder,
    ctx: SsaLowerCtx<'_>,
    link: &FusedRegionLink,
    slots: &[Option<StackSlot>],
    values: &HashMap<SsaValueId, cranelift_codegen::ir::Value>,
) -> VmResult<Vec<cranelift_codegen::ir::Value>> {
    if slots.len() != link.args.len() {
        return Err(VmError::JitNative(
            "fused SSA link slot count mismatch".to_string(),
        ));
    }
    link.args
        .iter()
        .zip(slots)
        .map(|(materialization, slot)| match materialization {
            SsaMaterialization::Value(value) => values
                .get(value)
                .copied()
                .ok_or_else(|| VmError::JitNative("fused SSA link value missing".to_string())),
            SsaMaterialization::BoxInt(value) => {
                let src = *values.get(value).ok_or_else(|| {
                    VmError::JitNative("fused SSA link int value missing".to_string())
                })?;
                let addr = b.ins().stack_addr(
                    ctx.pointer_type,
                    slot.ok_or_else(|| {
                        VmError::JitNative("fused SSA link int slot missing".to_string())
                    })?,
                    0,
                );
                ssa_store_int_in_value(b, ctx.layout.value, addr, src);
                Ok(addr)
            }
            SsaMaterialization::BoxFloat(value) => {
                let src = *values.get(value).ok_or_else(|| {
                    VmError::JitNative("fused SSA link float value missing".to_string())
                })?;
                let addr = b.ins().stack_addr(
                    ctx.pointer_type,
                    slot.ok_or_else(|| {
                        VmError::JitNative("fused SSA link float slot missing".to_string())
                    })?,
                    0,
                );
                ssa_store_float_in_value(b, ctx.layout.value, addr, src);
                Ok(addr)
            }
            SsaMaterialization::BoxBool(value) => {
                let src = *values.get(value).ok_or_else(|| {
                    VmError::JitNative("fused SSA link bool value missing".to_string())
                })?;
                let addr = b.ins().stack_addr(
                    ctx.pointer_type,
                    slot.ok_or_else(|| {
                        VmError::JitNative("fused SSA link bool slot missing".to_string())
                    })?,
                    0,
                );
                ssa_store_bool_in_value(b, ctx.layout.value, addr, src);
                Ok(addr)
            }
            SsaMaterialization::BoxHeapPtr { .. } => Err(VmError::JitNative(
                "fused SSA link does not support heap ownership transfer".to_string(),
            )),
        })
        .collect()
}

fn lower_ssa_terminator(
    b: &mut FunctionBuilder,
    ctx: SsaLowerCtx<'_>,
    terminator: &SsaTerminator,
    values: &HashMap<SsaValueId, cranelift_codegen::ir::Value>,
    block_handles: &HashMap<crate::vm::jit::ir::SsaBlockId, Block>,
    exit_specs: &HashMap<SsaExitId, SsaExitLowering>,
    internal_links: InternalRegionLinks<'_>,
) -> VmResult<()> {
    match terminator {
        SsaTerminator::Jump { target, args } => {
            let handle = *block_handles
                .get(target)
                .ok_or_else(|| VmError::JitNative("SSA jump target block missing".to_string()))?;
            let lowered_args = args
                .iter()
                .map(|value| {
                    values
                        .get(value)
                        .copied()
                        .ok_or_else(|| VmError::JitNative("SSA jump value missing".to_string()))
                })
                .collect::<VmResult<Vec<_>>>()?;
            let lowered_args = ssa_block_args(lowered_args);
            b.ins().jump(handle, &lowered_args);
        }
        SsaTerminator::BranchBool {
            condition,
            if_true,
            if_false,
        } => {
            let condition = *values
                .get(condition)
                .ok_or_else(|| VmError::JitNative("SSA branch condition missing".to_string()))?;
            let (true_block, true_args, true_pending) = ssa_branch_target_block(
                b,
                ctx,
                if_true,
                values,
                block_handles,
                exit_specs,
                internal_links,
            )?;
            let (false_block, false_args, false_pending) = ssa_branch_target_block(
                b,
                ctx,
                if_false,
                values,
                block_handles,
                exit_specs,
                internal_links,
            )?;
            let true_args = ssa_block_args(true_args);
            let false_args = ssa_block_args(false_args);
            b.ins()
                .brif(condition, true_block, &true_args, false_block, &false_args);
            if let Some(pending) = true_pending {
                lower_pending_internal_link_gate(b, ctx, pending);
            }
            if let Some(pending) = false_pending {
                lower_pending_internal_link_gate(b, ctx, pending);
            }
        }
        SsaTerminator::Exit { exit } => {
            if let Some((link, slots)) = internal_links.find(*exit) {
                let handle = *block_handles.get(&link.child_entry).ok_or_else(|| {
                    VmError::JitNative("fused SSA child entry block missing".to_string())
                })?;
                let child_args = lower_internal_link_args(b, ctx, link, slots, values)?;
                let spec = exit_specs.get(exit).ok_or_else(|| {
                    VmError::JitNative("SSA internal exit lowering missing".to_string())
                })?;
                let restore_args = spec
                    .inputs
                    .iter()
                    .map(|value| {
                        values.get(value).copied().ok_or_else(|| {
                            VmError::JitNative("SSA internal restore arg missing".to_string())
                        })
                    })
                    .collect::<VmResult<Vec<_>>>()?;
                let (target, args, pending) =
                    lower_internal_link_target(b, ctx, handle, child_args, spec, restore_args)?;
                let args = ssa_block_args(args);
                b.ins().jump(target, &args);
                if let Some(pending) = pending {
                    lower_pending_internal_link_gate(b, ctx, pending);
                }
                return Ok(());
            }
            let spec = exit_specs
                .get(exit)
                .ok_or_else(|| VmError::JitNative("SSA exit lowering missing".to_string()))?;
            let args = spec
                .inputs
                .iter()
                .map(|value| {
                    values
                        .get(value)
                        .copied()
                        .ok_or_else(|| VmError::JitNative("SSA exit value missing".to_string()))
                })
                .collect::<VmResult<Vec<_>>>()?;
            let args = ssa_block_args(args);
            b.ins().jump(spec.trace_exit_block, &args);
        }
        SsaTerminator::Return { exit } => {
            let spec = exit_specs.get(exit).ok_or_else(|| {
                VmError::JitNative("SSA return exit lowering missing".to_string())
            })?;
            let args = spec
                .inputs
                .iter()
                .map(|value| {
                    values.get(value).copied().ok_or_else(|| {
                        VmError::JitNative("SSA return exit value missing".to_string())
                    })
                })
                .collect::<VmResult<Vec<_>>>()?;
            let args = ssa_block_args(args);
            b.ins().jump(spec.halted_block, &args);
        }
        SsaTerminator::CallValue { exit, .. } => {
            let spec = exit_specs.get(exit).ok_or_else(|| {
                VmError::JitNative("SSA call-value exit lowering missing".to_string())
            })?;
            let args = spec
                .inputs
                .iter()
                .map(|value| {
                    values.get(value).copied().ok_or_else(|| {
                        VmError::JitNative("SSA call-value exit value missing".to_string())
                    })
                })
                .collect::<VmResult<Vec<_>>>()?;
            let args = ssa_block_args(args);
            let call_value_block = spec
                .call_value_block
                .ok_or_else(|| VmError::JitNative("SSA call-value block missing".to_string()))?;
            b.ins().jump(call_value_block, &args);
        }
    }
    Ok(())
}

fn ssa_branch_target_block(
    b: &mut FunctionBuilder,
    ctx: SsaLowerCtx<'_>,
    target: &SsaBranchTarget,
    values: &HashMap<SsaValueId, cranelift_codegen::ir::Value>,
    block_handles: &HashMap<crate::vm::jit::ir::SsaBlockId, Block>,
    exit_specs: &HashMap<SsaExitId, SsaExitLowering>,
    internal_links: InternalRegionLinks<'_>,
) -> VmResult<(
    Block,
    Vec<cranelift_codegen::ir::Value>,
    Option<PendingInternalLinkGate>,
)> {
    match target {
        SsaBranchTarget::Block { target, args } => {
            let handle = *block_handles
                .get(target)
                .ok_or_else(|| VmError::JitNative("SSA branch target block missing".to_string()))?;
            let lowered_args = args
                .iter()
                .map(|value| {
                    values
                        .get(value)
                        .copied()
                        .ok_or_else(|| VmError::JitNative("SSA branch arg missing".to_string()))
                })
                .collect::<VmResult<Vec<_>>>()?;
            Ok((handle, lowered_args, None))
        }
        SsaBranchTarget::Exit(exit) => {
            if let Some((link, slots)) = internal_links.find(*exit) {
                let handle = *block_handles.get(&link.child_entry).ok_or_else(|| {
                    VmError::JitNative("fused SSA child entry block missing".to_string())
                })?;
                let child_args = lower_internal_link_args(b, ctx, link, slots, values)?;
                let spec = exit_specs.get(exit).ok_or_else(|| {
                    VmError::JitNative("SSA internal branch exit lowering missing".to_string())
                })?;
                let restore_args = spec
                    .inputs
                    .iter()
                    .map(|value| {
                        values.get(value).copied().ok_or_else(|| {
                            VmError::JitNative(
                                "SSA internal branch restore arg missing".to_string(),
                            )
                        })
                    })
                    .collect::<VmResult<Vec<_>>>()?;
                return lower_internal_link_target(b, ctx, handle, child_args, spec, restore_args);
            }
            let spec = exit_specs.get(exit).ok_or_else(|| {
                VmError::JitNative("SSA branch exit lowering missing".to_string())
            })?;
            let lowered_args = spec
                .inputs
                .iter()
                .map(|value| {
                    values
                        .get(value)
                        .copied()
                        .ok_or_else(|| VmError::JitNative("SSA exit arg missing".to_string()))
                })
                .collect::<VmResult<Vec<_>>>()?;
            Ok((spec.trace_exit_block, lowered_args, None))
        }
    }
}

struct PendingInternalLinkGate {
    gate_block: Block,
    child_handle: Block,
    child_arg_count: usize,
    interrupt_block: Block,
}

fn lower_internal_link_target(
    b: &mut FunctionBuilder,
    ctx: SsaLowerCtx<'_>,
    child_handle: Block,
    child_args: Vec<cranelift_codegen::ir::Value>,
    exit_spec: &SsaExitLowering,
    restore_args: Vec<cranelift_codegen::ir::Value>,
) -> VmResult<(
    Block,
    Vec<cranelift_codegen::ir::Value>,
    Option<PendingInternalLinkGate>,
)> {
    let Some(_settings) = ctx.interrupt_settings else {
        increment_native_region_edge_count(b, ctx);
        return Ok((child_handle, child_args, None));
    };
    let gate_block = b.create_block();
    for value in child_args.iter().chain(&restore_args) {
        b.append_block_param(gate_block, b.func.dfg.value_type(*value));
    }
    let child_arg_count = child_args.len();
    let mut gate_args = child_args;
    gate_args.extend(restore_args);
    let interrupt_block = exit_spec
        .interrupt_block
        .ok_or_else(|| VmError::JitNative("SSA internal interrupt block missing".to_string()))?;
    Ok((
        gate_block,
        gate_args,
        Some(PendingInternalLinkGate {
            gate_block,
            child_handle,
            child_arg_count,
            interrupt_block,
        }),
    ))
}

fn lower_pending_internal_link_gate(
    b: &mut FunctionBuilder,
    ctx: SsaLowerCtx<'_>,
    pending: PendingInternalLinkGate,
) {
    let settings = ctx
        .interrupt_settings
        .expect("pending internal link gate requires interrupt settings");
    b.switch_to_block(pending.gate_block);
    let gate_params = b.block_params(pending.gate_block).to_vec();
    let interrupt_status_block = b.create_block();
    b.append_block_param(interrupt_status_block, types::I32);
    emit_interrupt_tick_inline(
        b,
        ctx.vm_ptr,
        interrupt_status_block,
        ctx.offsets,
        ctx.interrupt_ops_to_advance,
        settings,
    );
    increment_native_region_edge_count(b, ctx);
    let child_params = ssa_block_args(gate_params[..pending.child_arg_count].to_vec());
    b.ins().jump(pending.child_handle, &child_params);

    b.switch_to_block(interrupt_status_block);
    let restore_params = ssa_block_args(gate_params[pending.child_arg_count..].to_vec());
    b.ins().jump(pending.interrupt_block, &restore_params);
}

fn increment_native_region_edge_count(b: &mut FunctionBuilder, ctx: SsaLowerCtx<'_>) {
    let count = b.ins().load(
        types::I64,
        MemFlags::new(),
        ctx.vm_ptr,
        ctx.offsets.jit_native_region_edge_count,
    );
    let next = b.ins().iadd_imm(count, 1);
    b.ins().store(
        MemFlags::new(),
        next,
        ctx.vm_ptr,
        ctx.offsets.jit_native_region_edge_count,
    );
}

fn ssa_leave_frame_status(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    helper_refs: SsaDeoptHelperRefs,
    helper_addrs: SsaDeoptHelperAddrs,
    vm_ptr: cranelift_codegen::ir::Value,
    inherited_state_ptr: cranelift_codegen::ir::Value,
    ret_ip: usize,
) -> VmResult<cranelift_codegen::ir::Value> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, helper_addrs.leave_frame)?;
    let ret_ip = b.ins().iconst(
        types::I64,
        i64::try_from(ret_ip)
            .map_err(|_| VmError::JitNative("SSA return ip out of range".to_string()))?,
    );
    let call = b.ins().call_indirect(
        helper_refs.leave_frame_ref,
        helper_ptr,
        &[vm_ptr, ret_ip, inherited_state_ptr],
    );
    Ok(b.inst_results(call)[0])
}

#[allow(clippy::too_many_arguments)]
fn ssa_exit_action_status(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    helper_refs: SsaDeoptHelperRefs,
    helper_addrs: SsaDeoptHelperAddrs,
    vm_ptr: cranelift_codegen::ir::Value,
    inherited_state_ptr: cranelift_codegen::ir::Value,
    exit: &crate::vm::jit::ir::SsaExit,
    action: SsaExitAction,
) -> VmResult<cranelift_codegen::ir::Value> {
    match action {
        SsaExitAction::InterruptYield => Ok(b.ins().iconst(types::I32, STATUS_OUT_OF_FUEL as i64)),
        SsaExitAction::Return => ssa_leave_frame_status(
            b,
            pointer_type,
            helper_refs,
            helper_addrs,
            vm_ptr,
            inherited_state_ptr,
            exit.exit_ip,
        ),
        SsaExitAction::CallValue {
            argc,
            call_ip,
            resume_ip,
        } => {
            let helper_ptr = iconst_ptr_from_addr(b, pointer_type, helper_addrs.enter_call_value)?;
            let argc = b.ins().iconst(types::I64, i64::from(argc));
            let call_ip = b.ins().iconst(
                types::I64,
                i64::try_from(call_ip).map_err(|_| {
                    VmError::JitNative("SSA call-value ip out of range".to_string())
                })?,
            );
            let resume_ip = b.ins().iconst(
                types::I64,
                i64::try_from(resume_ip).map_err(|_| {
                    VmError::JitNative("SSA call-value resume ip out of range".to_string())
                })?,
            );
            let call = b.ins().call_indirect(
                helper_refs.enter_call_value_ref,
                helper_ptr,
                &[vm_ptr, argc, call_ip, resume_ip, inherited_state_ptr],
            );
            Ok(b.inst_results(call)[0])
        }
        SsaExitAction::TraceExit { allow_link_handoff } => {
            if allow_link_handoff {
                let helper_ptr =
                    iconst_ptr_from_addr(b, pointer_type, helper_addrs.resume_linked_trace)?;
                let call = b.ins().call_indirect(
                    helper_refs.resume_linked_trace_ref,
                    helper_ptr,
                    &[vm_ptr],
                );
                Ok(b.inst_results(call)[0])
            } else {
                let status = crate::vm::native::encode_jit_trace_exit_status(exit.id.raw())
                    .ok_or_else(|| {
                        VmError::JitNative("SSA exit id exceeds native status range".to_string())
                    })?;
                Ok(b.ins().iconst(types::I32, i64::from(status)))
            }
        }
    }
}

fn write_inherited_state_packet(
    b: &mut FunctionBuilder,
    ctx: SsaLowerCtx<'_>,
    exit: &crate::vm::jit::ir::SsaExit,
) -> VmResult<()> {
    let inactive = b.ins().iconst(ctx.pointer_type, 0);
    b.ins().store(
        MemFlags::new(),
        inactive,
        ctx.inherited_state_ptr,
        INHERITED_STATE_ACTIVE_OFFSET,
    );
    b.ins().store(
        MemFlags::new(),
        inactive,
        ctx.inherited_state_ptr,
        INHERITED_STATE_DYNAMIC_TARGET_OFFSET,
    );
    let entry_value_count = exit
        .stack
        .len()
        .checked_add(exit.locals.len())
        .ok_or_else(|| VmError::JitNative("SSA inherited exit value count overflow".to_string()))?;
    if entry_value_count > MAX_INHERITED_ENTRY_VALUES {
        return Ok(());
    }

    let frame_key = b.ins().iconst(ctx.pointer_type, ctx.frame_key as i64);
    b.ins().store(
        MemFlags::new(),
        frame_key,
        ctx.inherited_state_ptr,
        INHERITED_STATE_FRAME_KEY_OFFSET,
    );
    b.ins().store(
        MemFlags::new(),
        ctx.active_stack_base,
        ctx.inherited_state_ptr,
        INHERITED_STATE_STACK_BASE_OFFSET,
    );
    b.ins().store(
        MemFlags::new(),
        ctx.active_local_base,
        ctx.inherited_state_ptr,
        INHERITED_STATE_LOCAL_BASE_OFFSET,
    );
    let target_ip = b.ins().iconst(
        ctx.pointer_type,
        i64::try_from(exit.exit_ip)
            .map_err(|_| VmError::JitNative("SSA inherited target ip exceeds i64".to_string()))?,
    );
    b.ins().store(
        MemFlags::new(),
        target_ip,
        ctx.inherited_state_ptr,
        INHERITED_STATE_TARGET_IP_OFFSET,
    );
    let value_count = b.ins().iconst(
        ctx.pointer_type,
        i64::try_from(entry_value_count)
            .map_err(|_| VmError::JitNative("SSA inherited value count exceeds i64".to_string()))?,
    );
    b.ins().store(
        MemFlags::new(),
        value_count,
        ctx.inherited_state_ptr,
        INHERITED_STATE_VALUE_COUNT_OFFSET,
    );
    let stack_ptr = b.ins().load(
        ctx.pointer_type,
        MemFlags::new(),
        ctx.vm_ptr,
        ctx.offsets.stack_ptr,
    );
    let stack_base_ptr = ssa_value_addr(
        b,
        ctx.pointer_type,
        stack_ptr,
        ctx.active_stack_base,
        ctx.layout.value.size,
    );
    let locals_ptr = b.ins().load(
        ctx.pointer_type,
        MemFlags::new(),
        ctx.vm_ptr,
        ctx.offsets.locals_ptr,
    );
    let locals_base_ptr = ssa_value_addr(
        b,
        ctx.pointer_type,
        locals_ptr,
        ctx.active_local_base,
        ctx.layout.value.size,
    );
    for index in 0..entry_value_count {
        let value_addr = if index < exit.stack.len() {
            let stack_index = b.ins().iconst(
                ctx.pointer_type,
                i64::try_from(index).map_err(|_| {
                    VmError::JitNative("SSA inherited stack index exceeds i64".to_string())
                })?,
            );
            ssa_value_addr(
                b,
                ctx.pointer_type,
                stack_base_ptr,
                stack_index,
                ctx.layout.value.size,
            )
        } else {
            let local_index = index - exit.stack.len();
            let local_index = b.ins().iconst(
                ctx.pointer_type,
                i64::try_from(local_index).map_err(|_| {
                    VmError::JitNative("SSA inherited local index exceeds i64".to_string())
                })?,
            );
            ssa_value_addr(
                b,
                ctx.pointer_type,
                locals_base_ptr,
                local_index,
                ctx.layout.value.size,
            )
        };
        let packet_offset = index
            .checked_mul(std::mem::size_of::<usize>())
            .and_then(|offset| {
                usize::try_from(INHERITED_STATE_VALUES_OFFSET)
                    .ok()
                    .and_then(|base| base.checked_add(offset))
            })
            .and_then(|offset| i32::try_from(offset).ok())
            .ok_or_else(|| {
                VmError::JitNative("SSA inherited packet offset exceeds i32".to_string())
            })?;
        b.ins().store(
            MemFlags::new(),
            value_addr,
            ctx.inherited_state_ptr,
            packet_offset,
        );
    }
    let active = b.ins().iconst(ctx.pointer_type, 1);
    b.ins().store(
        MemFlags::new(),
        active,
        ctx.inherited_state_ptr,
        INHERITED_STATE_ACTIVE_OFFSET,
    );
    Ok(())
}

fn lower_ssa_exit_block(
    b: &mut FunctionBuilder,
    ctx: SsaLowerCtx<'_>,
    exit: &crate::vm::jit::ir::SsaExit,
    spec: &SsaExitLowering,
    action: SsaExitAction,
) -> VmResult<()> {
    let SsaLowerCtx {
        vm_ptr,
        inherited_state_ptr,
        active_local_base,
        exit_block,
        pointer_type,
        layout,
        offsets,
        entry_stack_depth,
        owned_value_temps,
        helper_refs: deopt_refs,
        helper_addrs: deopt_addrs,
        ..
    } = ctx;
    let block = match action {
        SsaExitAction::TraceExit { .. } => spec.trace_exit_block,
        SsaExitAction::Return => spec.halted_block,
        SsaExitAction::CallValue { .. } => spec
            .call_value_block
            .ok_or_else(|| VmError::JitNative("SSA call-value exit block missing".to_string()))?,
        SsaExitAction::InterruptYield => spec
            .interrupt_block
            .ok_or_else(|| VmError::JitNative("SSA interrupt exit block missing".to_string()))?,
    };
    b.switch_to_block(block);
    let block_params = b.block_params(block).to_vec();
    let exit_values = spec
        .inputs
        .iter()
        .copied()
        .zip(block_params.iter().copied())
        .collect::<HashMap<_, _>>();
    let materialize_ctx = SsaMaterializeCtx {
        exit_block,
        pointer_type,
        value_layout: layout.value,
        exit_values: &exit_values,
        deopt_refs,
        deopt_addrs,
    };

    let mut tagged_local_counts = HashMap::<SsaValueId, usize>::new();
    for (materialization, dirty) in exit.locals.iter().zip(&exit.dirty_locals) {
        if *dirty && let SsaMaterialization::Value(value) = materialization {
            *tagged_local_counts.entry(*value).or_default() += 1;
        }
    }
    let mut moved_owned_values = BTreeSet::new();
    let inline_owned_restore = exit.virtual_frames.is_empty()
        && entry_stack_depth == 0
        && exit.stack.is_empty()
        && exit
            .locals
            .iter()
            .zip(&exit.dirty_locals)
            .all(|(materialization, dirty)| {
                if !*dirty {
                    return true;
                }
                match materialization {
                    SsaMaterialization::BoxInt(_)
                    | SsaMaterialization::BoxBool(_)
                    | SsaMaterialization::BoxFloat(_) => true,
                    SsaMaterialization::Value(value) => {
                        if tagged_local_counts.get(value) == Some(&1)
                            && owned_value_temps
                                .slots
                                .contains_key(&SsaTempValueSlotKey::Output(*value))
                        {
                            moved_owned_values.insert(*value);
                        }
                        true
                    }
                    SsaMaterialization::BoxHeapPtr { .. } => true,
                }
            });
    if inline_owned_restore {
        let vm_locals_ptr = b
            .ins()
            .load(pointer_type, MemFlags::new(), vm_ptr, offsets.locals_ptr);
        let vm_locals_ptr = ssa_value_addr(
            b,
            pointer_type,
            vm_locals_ptr,
            active_local_base,
            layout.value.size,
        );
        let mut cloned_local_addrs = HashMap::new();
        for (local_index, materialization) in
            exit.locals
                .iter()
                .enumerate()
                .filter_map(|(local_index, materialization)| {
                    exit.dirty_locals[local_index].then_some((local_index, materialization))
                })
        {
            let clone_before_clear = match materialization {
                SsaMaterialization::BoxHeapPtr { .. } => true,
                SsaMaterialization::Value(value) => !moved_owned_values.contains(value),
                _ => false,
            };
            if clone_before_clear {
                let temp_slot = ssa_create_value_stack_slot(b, layout.value.size)?;
                let temp_addr = b.ins().stack_addr(pointer_type, temp_slot, 0);
                ssa_materialize_slot(b, materialize_ctx, materialization, temp_addr, "local")?;
                cloned_local_addrs.insert(local_index, temp_addr);
            }
        }
        for (local_index, materialization) in
            exit.locals
                .iter()
                .enumerate()
                .filter_map(|(local_index, materialization)| {
                    exit.dirty_locals[local_index].then_some((local_index, materialization))
                })
        {
            let index = b.ins().iconst(
                pointer_type,
                i64::try_from(local_index).map_err(|_| {
                    VmError::JitNative("SSA dirty local index out of range".to_string())
                })?,
            );
            let dst_addr = ssa_value_addr(b, pointer_type, vm_locals_ptr, index, layout.value.size);
            if let Some(temp_addr) = cloned_local_addrs.get(&local_index).copied() {
                clear_owned_value_temp_slot(b, pointer_type, deopt_refs, deopt_addrs, dst_addr)?;
                ssa_copy_value_bytes(b, temp_addr, dst_addr, layout.value.size);
                ssa_store_tag(b, layout.value, temp_addr, layout.value.null_tag);
            } else {
                clear_owned_value_temp_slot(b, pointer_type, deopt_refs, deopt_addrs, dst_addr)?;
                if let SsaMaterialization::Value(value) = materialization {
                    let src = *exit_values.get(value).ok_or_else(|| {
                        VmError::JitNative("SSA exit tagged local value missing".to_string())
                    })?;
                    ssa_copy_value_bytes(b, src, dst_addr, layout.value.size);
                    ssa_store_tag(b, layout.value, src, layout.value.null_tag);
                } else {
                    ssa_materialize_slot(b, materialize_ctx, materialization, dst_addr, "local")?;
                }
            }
        }
        let ip_val = b.ins().iconst(
            pointer_type,
            i64::try_from(exit.exit_ip)
                .map_err(|_| VmError::JitNative("SSA exit ip out of range".to_string()))?,
        );
        b.ins()
            .store(MemFlags::new(), ip_val, vm_ptr, offsets.vm_ip);
        if matches!(action, SsaExitAction::TraceExit { .. }) {
            write_inherited_state_packet(b, ctx, exit)?;
        }
        let status = ssa_exit_action_status(
            b,
            pointer_type,
            deopt_refs,
            deopt_addrs,
            vm_ptr,
            inherited_state_ptr,
            exit,
            action,
        )?;
        jump_with_status(b, exit_block, status);
        return Ok(());
    }

    let stack_ptr = ssa_alloc_value_buffer(b, pointer_type, exit.stack.len(), layout.value.size)?;
    for (stack_index, materialization) in exit.stack.iter().enumerate() {
        let dst_addr = ssa_value_buffer_slot_addr(
            b,
            pointer_type,
            stack_ptr,
            stack_index,
            layout.value.size,
            "stack",
        )?;
        ssa_materialize_slot(b, materialize_ctx, materialization, dst_addr, "stack")?;
    }

    let dirty_local_count = exit.dirty_locals.iter().filter(|dirty| **dirty).count();
    let local_indices_ptr = ssa_alloc_u32_buffer(b, pointer_type, dirty_local_count)?;
    let locals_ptr = ssa_alloc_value_buffer(b, pointer_type, dirty_local_count, layout.value.size)?;
    for (compact_index, (local_index, materialization)) in exit
        .locals
        .iter()
        .enumerate()
        .zip(&exit.dirty_locals)
        .filter_map(|((local_index, materialization), dirty)| {
            dirty.then_some((local_index, materialization))
        })
        .enumerate()
    {
        ssa_store_u32_buffer_slot(
            b,
            local_indices_ptr,
            compact_index,
            u32::try_from(local_index).map_err(|_| {
                VmError::JitNative("SSA dirty local index out of range".to_string())
            })?,
        )?;
        let dst_addr = ssa_value_buffer_slot_addr(
            b,
            pointer_type,
            locals_ptr,
            compact_index,
            layout.value.size,
            "local",
        )?;
        ssa_materialize_slot(b, materialize_ctx, materialization, dst_addr, "local")?;
    }
    let stack_len = b.ins().iconst(
        pointer_type,
        i64::try_from(exit.stack.len())
            .map_err(|_| VmError::JitNative("SSA exit stack length out of range".to_string()))?,
    );
    let dirty_local_count = b.ins().iconst(
        pointer_type,
        i64::try_from(dirty_local_count)
            .map_err(|_| VmError::JitNative("SSA dirty local count out of range".to_string()))?,
    );
    let ip_val = b.ins().iconst(
        pointer_type,
        i64::try_from(exit.exit_ip)
            .map_err(|_| VmError::JitNative("SSA exit ip out of range".to_string()))?,
    );
    let null_ptr = b.ins().iconst(pointer_type, 0);
    let stack_ptr = stack_ptr.unwrap_or(null_ptr);
    let local_indices_ptr = local_indices_ptr.unwrap_or(null_ptr);
    let locals_ptr = locals_ptr.unwrap_or(null_ptr);
    ssa_call_status_helper(
        b,
        exit_block,
        pointer_type,
        deopt_refs.sparse_restore_exit_ref,
        deopt_addrs.sparse_restore_exit,
        &[
            vm_ptr,
            stack_ptr,
            stack_len,
            local_indices_ptr,
            locals_ptr,
            dirty_local_count,
            ip_val,
        ],
    )?;
    restore_virtual_frames(b, materialize_ctx, vm_ptr, &exit.virtual_frames)?;
    if matches!(action, SsaExitAction::TraceExit { .. }) && exit.virtual_frames.is_empty() {
        write_inherited_state_packet(b, ctx, exit)?;
    }
    let status = ssa_exit_action_status(
        b,
        pointer_type,
        deopt_refs,
        deopt_addrs,
        vm_ptr,
        inherited_state_ptr,
        exit,
        action,
    )?;
    jump_with_status(b, exit_block, status);
    Ok(())
}

fn restore_virtual_frames(
    b: &mut FunctionBuilder,
    materialize_ctx: SsaMaterializeCtx<'_>,
    vm_ptr: cranelift_codegen::ir::Value,
    frames: &[crate::vm::jit::ir::VirtualFrameSnapshot],
) -> VmResult<()> {
    let pointer_type = materialize_ctx.pointer_type;
    let value_size = materialize_ctx.value_layout.size;
    let null_ptr = b.ins().iconst(pointer_type, 0);
    for frame in frames {
        let stack_ptr =
            ssa_alloc_value_buffer(b, pointer_type, frame.operand_stack.len(), value_size)?;
        for (index, materialization) in frame.operand_stack.iter().enumerate() {
            let dst = ssa_value_buffer_slot_addr(
                b,
                pointer_type,
                stack_ptr,
                index,
                value_size,
                "virtual stack",
            )?;
            ssa_materialize_slot(b, materialize_ctx, materialization, dst, "virtual stack")?;
        }
        let locals_ptr = ssa_alloc_value_buffer(b, pointer_type, frame.locals.len(), value_size)?;
        for (index, materialization) in frame.locals.iter().enumerate() {
            let dst = ssa_value_buffer_slot_addr(
                b,
                pointer_type,
                locals_ptr,
                index,
                value_size,
                "virtual local",
            )?;
            ssa_materialize_slot(b, materialize_ctx, materialization, dst, "virtual local")?;
        }
        let prototype_id = b.ins().iconst(types::I32, i64::from(frame.prototype_id));
        let call_ip = iconst_usize(b, pointer_type, frame.call_ip, "virtual call ip")?;
        let return_ip = iconst_usize(b, pointer_type, frame.return_ip, "virtual return ip")?;
        let resume_ip = iconst_usize(b, pointer_type, frame.resume_ip, "virtual resume ip")?;
        let stack_len = iconst_usize(
            b,
            pointer_type,
            frame.operand_stack.len(),
            "virtual stack length",
        )?;
        let locals_len =
            iconst_usize(b, pointer_type, frame.locals.len(), "virtual locals length")?;
        ssa_call_status_helper(
            b,
            materialize_ctx.exit_block,
            pointer_type,
            materialize_ctx.deopt_refs.restore_virtual_frame_ref,
            materialize_ctx.deopt_addrs.restore_virtual_frame,
            &[
                vm_ptr,
                prototype_id,
                call_ip,
                return_ip,
                resume_ip,
                stack_ptr.unwrap_or(null_ptr),
                stack_len,
                locals_ptr.unwrap_or(null_ptr),
                locals_len,
            ],
        )?;
    }
    Ok(())
}

fn iconst_usize(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    value: usize,
    label: &'static str,
) -> VmResult<cranelift_codegen::ir::Value> {
    Ok(b.ins().iconst(
        pointer_type,
        i64::try_from(value)
            .map_err(|_| VmError::JitNative(format!("SSA {label} out of range")))?,
    ))
}

fn ssa_materialize_slot(
    b: &mut FunctionBuilder,
    ctx: SsaMaterializeCtx<'_>,
    materialization: &SsaMaterialization,
    dst_addr: cranelift_codegen::ir::Value,
    slot_kind: &'static str,
) -> VmResult<()> {
    let SsaMaterializeCtx {
        exit_block,
        pointer_type,
        value_layout,
        exit_values,
        deopt_refs,
        deopt_addrs,
    } = ctx;
    match materialization {
        SsaMaterialization::Value(value) => {
            let src = *exit_values.get(value).ok_or_else(|| {
                VmError::JitNative(format!("SSA exit tagged {slot_kind} value missing"))
            })?;
            ssa_call_status_helper(
                b,
                exit_block,
                pointer_type,
                deopt_refs.clone_value_ref,
                deopt_addrs.clone_value,
                &[dst_addr, src],
            )?;
        }
        SsaMaterialization::BoxInt(value) => {
            let src = *exit_values.get(value).ok_or_else(|| {
                VmError::JitNative(format!("SSA exit int {slot_kind} value missing"))
            })?;
            ssa_store_int_in_value(b, value_layout, dst_addr, src);
        }
        SsaMaterialization::BoxBool(value) => {
            let src = *exit_values.get(value).ok_or_else(|| {
                VmError::JitNative(format!("SSA exit bool {slot_kind} value missing"))
            })?;
            ssa_store_bool_in_value(b, value_layout, dst_addr, src);
        }
        SsaMaterialization::BoxFloat(value) => {
            let src = *exit_values.get(value).ok_or_else(|| {
                VmError::JitNative(format!("SSA exit float {slot_kind} value missing"))
            })?;
            ssa_store_float_in_value(b, value_layout, dst_addr, src);
        }
        SsaMaterialization::BoxHeapPtr { value, tag } => {
            let src = *exit_values.get(value).ok_or_else(|| {
                VmError::JitNative(format!("SSA exit heap {slot_kind} value missing"))
            })?;
            let tag = b.ins().iconst(types::I64, *tag as i64);
            ssa_call_status_helper(
                b,
                exit_block,
                pointer_type,
                deopt_refs.box_heap_value_ref,
                deopt_addrs.box_heap_value,
                &[dst_addr, src, tag],
            )?;
        }
    }
    Ok(())
}

fn ssa_type(
    pointer_type: cranelift_codegen::ir::Type,
    repr: SsaValueRepr,
) -> Option<cranelift_codegen::ir::Type> {
    match repr {
        SsaValueRepr::Tagged | SsaValueRepr::HeapPtr(_) => Some(pointer_type),
        SsaValueRepr::I64 => Some(types::I64),
        SsaValueRepr::F64 => Some(types::F64),
        SsaValueRepr::Bool => Some(types::I8),
    }
}

fn ssa_alloc_value_buffer(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    slot_count: usize,
    value_size: i32,
) -> VmResult<Option<cranelift_codegen::ir::Value>> {
    if slot_count == 0 {
        return Ok(None);
    }
    let value_size = usize::try_from(value_size)
        .map_err(|_| VmError::JitNative("SSA value slot size out of range".to_string()))?;
    let bytes = slot_count
        .checked_mul(value_size)
        .ok_or_else(|| VmError::JitNative("SSA temp value buffer overflow".to_string()))?;
    let bytes = u32::try_from(bytes)
        .map_err(|_| VmError::JitNative("SSA temp value buffer too large".to_string()))?;
    let align_shift = std::mem::align_of::<Value>().trailing_zeros() as u8;
    let slot = b.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        bytes,
        align_shift,
    ));
    Ok(Some(b.ins().stack_addr(pointer_type, slot, 0)))
}

fn ssa_alloc_u32_buffer(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    slot_count: usize,
) -> VmResult<Option<cranelift_codegen::ir::Value>> {
    if slot_count == 0 {
        return Ok(None);
    }
    let bytes = slot_count
        .checked_mul(std::mem::size_of::<u32>())
        .ok_or_else(|| VmError::JitNative("SSA local index buffer overflow".to_string()))?;
    let bytes = u32::try_from(bytes)
        .map_err(|_| VmError::JitNative("SSA local index buffer too large".to_string()))?;
    let align_shift = std::mem::align_of::<u32>().trailing_zeros() as u8;
    let slot = b.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        bytes,
        align_shift,
    ));
    Ok(Some(b.ins().stack_addr(pointer_type, slot, 0)))
}

fn ssa_store_u32_buffer_slot(
    b: &mut FunctionBuilder,
    base_ptr: Option<cranelift_codegen::ir::Value>,
    index: usize,
    value: u32,
) -> VmResult<()> {
    let base_ptr = base_ptr.ok_or_else(|| {
        VmError::JitNative("SSA local index buffer missing during exit lowering".to_string())
    })?;
    let offset = index
        .checked_mul(std::mem::size_of::<u32>())
        .and_then(|offset| i32::try_from(offset).ok())
        .ok_or_else(|| VmError::JitNative("SSA local index offset out of range".to_string()))?;
    let value = b.ins().iconst(types::I32, i64::from(value));
    b.ins().store(MemFlags::new(), value, base_ptr, offset);
    Ok(())
}

fn ssa_value_buffer_slot_addr(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    base_ptr: Option<cranelift_codegen::ir::Value>,
    index: usize,
    value_size: i32,
    slot_kind: &'static str,
) -> VmResult<cranelift_codegen::ir::Value> {
    let base_ptr = base_ptr.ok_or_else(|| {
        VmError::JitNative(format!(
            "SSA {slot_kind} buffer missing during exit lowering"
        ))
    })?;
    let index = b.ins().iconst(
        pointer_type,
        i64::try_from(index)
            .map_err(|_| VmError::JitNative(format!("SSA {slot_kind} index out of range")))?,
    );
    Ok(ssa_value_addr(b, pointer_type, base_ptr, index, value_size))
}

fn ssa_call_value_eq(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    helper_refs: SsaDeoptHelperRefs,
    helper_addrs: SsaDeoptHelperAddrs,
    lhs: cranelift_codegen::ir::Value,
    rhs: cranelift_codegen::ir::Value,
) -> VmResult<cranelift_codegen::ir::Value> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, helper_addrs.value_eq)?;
    let call = b
        .ins()
        .call_indirect(helper_refs.value_eq_ref, helper_ptr, &[lhs, rhs]);
    Ok(b.inst_results(call)[0])
}

fn ssa_call_status_helper(
    b: &mut FunctionBuilder,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    helper_ref: cranelift_codegen::ir::SigRef,
    helper_addr: usize,
    args: &[cranelift_codegen::ir::Value],
) -> VmResult<()> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, helper_addr)?;
    let call = b.ins().call_indirect(helper_ref, helper_ptr, args);
    let status = b.inst_results(call)[0];
    let cont = b.create_block();
    let is_continue = b
        .ins()
        .icmp_imm(IntCC::Equal, status, STATUS_CONTINUE as i64);
    let else_args = [BlockArg::Value(status)];
    b.ins().brif(is_continue, cont, &[], exit_block, &else_args);
    b.switch_to_block(cont);
    Ok(())
}

fn ssa_call_infallible_helper(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    helper_ref: cranelift_codegen::ir::SigRef,
    helper_addr: usize,
    args: &[cranelift_codegen::ir::Value],
) -> VmResult<()> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, helper_addr)?;
    let _ = b.ins().call_indirect(helper_ref, helper_ptr, args);
    Ok(())
}

fn ssa_heap_tag(layout: crate::vm::native::ValueLayout, tag: crate::ValueType) -> VmResult<u32> {
    match tag {
        crate::ValueType::String => Ok(layout.string_tag),
        crate::ValueType::Bytes => Ok(layout.bytes_tag),
        crate::ValueType::Array => Ok(layout.array_tag),
        crate::ValueType::Map => Ok(layout.map_tag),
        other => Err(VmError::JitNative(format!(
            "unsupported SSA heap unbox tag {other:?}"
        ))),
    }
}

fn ssa_load_heap_ptr(
    b: &mut FunctionBuilder,
    layout: crate::vm::native::ValueLayout,
    value_addr: cranelift_codegen::ir::Value,
    pointer_type: cranelift_codegen::ir::Type,
) -> cranelift_codegen::ir::Value {
    b.ins().load(
        pointer_type,
        MemFlags::new(),
        value_addr,
        layout.heap_payload_offset,
    )
}

fn ssa_load_heap_data_ptr(
    b: &mut FunctionBuilder,
    layout: crate::vm::native::ValueLayout,
    heap_ptr: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    if layout.arc_data_offset == 0 {
        heap_ptr
    } else {
        b.ins()
            .iadd_imm(heap_ptr, i64::from(layout.arc_data_offset))
    }
}

fn ssa_index_in_range(
    b: &mut FunctionBuilder,
    index: cranelift_codegen::ir::Value,
    len: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    let ge_zero = b.ins().icmp_imm(IntCC::SignedGreaterThanOrEqual, index, 0);
    let lt_len = b.ins().icmp(IntCC::UnsignedLessThan, index, len);
    b.ins().band(ge_zero, lt_len)
}

fn ssa_inline_concat(
    b: &mut FunctionBuilder,
    ctx: SsaLowerCtx<'_>,
    op: SsaConcatOp,
) -> VmResult<cranelift_codegen::ir::Value> {
    let SsaLowerCtx {
        vm_ptr,
        exit_block,
        pointer_type,
        layout,
        offsets,
        heap_refs,
        heap_addrs,
        helper_refs,
        helper_addrs,
        owned_value_temps,
        ..
    } = ctx;
    let SsaConcatOp {
        output_id,
        ip,
        lhs,
        rhs,
        result_tag,
        pack_addr,
    } = op;
    let lhs_data = ssa_load_heap_data_ptr(b, layout.value, lhs);
    let rhs_data = ssa_load_heap_data_ptr(b, layout.value, rhs);
    let lhs_bytes_ptr = b.ins().load(
        pointer_type,
        MemFlags::new(),
        lhs_data,
        layout.stack_vec.ptr_offset,
    );
    let lhs_bytes_len = b.ins().load(
        pointer_type,
        MemFlags::new(),
        lhs_data,
        layout.stack_vec.len_offset,
    );
    let rhs_bytes_ptr = b.ins().load(
        pointer_type,
        MemFlags::new(),
        rhs_data,
        layout.stack_vec.ptr_offset,
    );
    let rhs_bytes_len = b.ins().load(
        pointer_type,
        MemFlags::new(),
        rhs_data,
        layout.stack_vec.len_offset,
    );
    let max_usize = b.ins().iconst(pointer_type, -1);
    let remaining = b.ins().isub(max_usize, lhs_bytes_len);
    let overflow = b
        .ins()
        .icmp(IntCC::UnsignedGreaterThan, rhs_bytes_len, remaining);
    let add_ok = b.create_block();
    let fail = b.create_block();
    let cont = b.create_block();
    let out = owned_value_temp_slot_addr(
        b,
        pointer_type,
        owned_value_temps,
        SsaTempValueSlotKey::Output(output_id),
    )?;
    b.ins().brif(overflow, fail, &[], add_ok, &[]);

    b.switch_to_block(add_ok);
    let total_len = b.ins().iadd(lhs_bytes_len, rhs_bytes_len);
    let exceeds_isize = b
        .ins()
        .icmp_imm(IntCC::UnsignedGreaterThan, total_len, i64::MAX);
    let cap_ok = b.create_block();
    b.ins().brif(exceeds_isize, fail, &[], cap_ok, &[]);

    b.switch_to_block(cap_ok);
    let out_ptr = ssa_call_alloc_buffer(
        b,
        pointer_type,
        heap_refs,
        heap_addrs,
        heap_addrs.alloc_byte_buffer,
        total_len,
    )?;
    ssa_call_copy_bytes(
        b,
        pointer_type,
        heap_refs,
        heap_addrs,
        out_ptr,
        lhs_bytes_ptr,
        lhs_bytes_len,
    )?;
    let rhs_dst = b.ins().iadd(out_ptr, lhs_bytes_len);
    ssa_call_copy_bytes(
        b,
        pointer_type,
        heap_refs,
        heap_addrs,
        rhs_dst,
        rhs_bytes_ptr,
        rhs_bytes_len,
    )?;
    let out_raw = ssa_call_pack_shared(
        b,
        pointer_type,
        heap_refs,
        pack_addr,
        out_ptr,
        total_len,
        total_len,
    )?;
    clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, out)?;
    ssa_store_heap_ptr_in_value(b, layout.value, out, result_tag, out_raw);
    b.ins().jump(cont, &[]);

    b.switch_to_block(fail);
    ssa_emit_trace_exit_status(b, vm_ptr, exit_block, pointer_type, offsets, ip)?;

    b.switch_to_block(cont);
    Ok(out)
}

fn ssa_alloc_single_value_slot(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    value_size: i32,
) -> VmResult<cranelift_codegen::ir::Value> {
    ssa_alloc_value_buffer(b, pointer_type, 1, value_size)?.ok_or_else(|| {
        VmError::JitNative("SSA single-value temp slot allocation failed".to_string())
    })
}

fn ssa_materialize_runtime_value_to_slot(
    b: &mut FunctionBuilder,
    ctx: SsaBoxCtx,
    repr: SsaValueRepr,
    value: cranelift_codegen::ir::Value,
    dst_addr: cranelift_codegen::ir::Value,
) -> VmResult<()> {
    let SsaBoxCtx {
        exit_block,
        pointer_type,
        value_layout,
        helper_refs,
        helper_addrs,
    } = ctx;
    match repr {
        SsaValueRepr::Tagged => ssa_call_status_helper(
            b,
            exit_block,
            pointer_type,
            helper_refs.clone_value_ref,
            helper_addrs.clone_value,
            &[dst_addr, value],
        ),
        SsaValueRepr::I64 => {
            ssa_store_int_in_value(b, value_layout, dst_addr, value);
            Ok(())
        }
        SsaValueRepr::F64 => {
            ssa_store_float_in_value(b, value_layout, dst_addr, value);
            Ok(())
        }
        SsaValueRepr::Bool => {
            ssa_store_bool_in_value(b, value_layout, dst_addr, value);
            Ok(())
        }
        SsaValueRepr::HeapPtr(tag) => {
            let tag = b.ins().iconst(types::I64, tag as i64);
            ssa_call_status_helper(
                b,
                exit_block,
                pointer_type,
                helper_refs.box_heap_value_ref,
                helper_addrs.box_heap_value,
                &[dst_addr, value, tag],
            )
        }
    }
}

fn ssa_ensure_boxed_value_addr(
    b: &mut FunctionBuilder,
    ctx: SsaBoxCtx,
    temp_slot: Option<cranelift_codegen::ir::Value>,
    repr: SsaValueRepr,
    value: cranelift_codegen::ir::Value,
) -> VmResult<cranelift_codegen::ir::Value> {
    let SsaBoxCtx {
        exit_block: _,
        pointer_type,
        value_layout,
        helper_refs,
        helper_addrs,
    } = ctx;
    if repr == SsaValueRepr::Tagged {
        return Ok(value);
    }
    let slot = if let Some(slot) = temp_slot {
        clear_owned_value_temp_slot(b, pointer_type, helper_refs, helper_addrs, slot)?;
        slot
    } else {
        ssa_alloc_single_value_slot(b, pointer_type, value_layout.size)?
    };
    ssa_materialize_runtime_value_to_slot(b, ctx, repr, value, slot)?;
    Ok(slot)
}

fn ssa_value_addr(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    base_ptr: cranelift_codegen::ir::Value,
    index: cranelift_codegen::ir::Value,
    value_size: i32,
) -> cranelift_codegen::ir::Value {
    let stride = b.ins().iconst(pointer_type, i64::from(value_size));
    let offset = b.ins().imul(index, stride);
    b.ins().iadd(base_ptr, offset)
}

fn iconst_ptr_from_addr(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    addr: usize,
) -> VmResult<cranelift_codegen::ir::Value> {
    let addr = i64::try_from(addr)
        .map_err(|_| VmError::JitNative("native helper address out of range".to_string()))?;
    Ok(b.ins().iconst(pointer_type, addr))
}

fn ssa_tag_type(layout: crate::vm::native::ValueLayout) -> cranelift_codegen::ir::Type {
    match layout.tag_size {
        1 => types::I8,
        2 => types::I16,
        _ => types::I32,
    }
}

fn ssa_load_tag_i32(
    b: &mut FunctionBuilder,
    layout: crate::vm::native::ValueLayout,
    value_addr: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    let raw = b.ins().load(
        ssa_tag_type(layout),
        MemFlags::new(),
        value_addr,
        layout.tag_offset,
    );
    match layout.tag_size {
        1 | 2 => b.ins().uextend(types::I32, raw),
        _ => raw,
    }
}

fn ssa_store_tag(
    b: &mut FunctionBuilder,
    layout: crate::vm::native::ValueLayout,
    value_addr: cranelift_codegen::ir::Value,
    tag: u32,
) {
    let ty = ssa_tag_type(layout);
    let raw = b.ins().iconst(ty, i64::from(tag));
    b.ins()
        .store(MemFlags::new(), raw, value_addr, layout.tag_offset);
}

fn ssa_store_int_in_value(
    b: &mut FunctionBuilder,
    layout: crate::vm::native::ValueLayout,
    value_addr: cranelift_codegen::ir::Value,
    int_value: cranelift_codegen::ir::Value,
) {
    ssa_store_tag(b, layout, value_addr, layout.int_tag);
    b.ins().store(
        MemFlags::new(),
        int_value,
        value_addr,
        layout.int_payload_offset,
    );
}

fn ssa_store_bool_in_value(
    b: &mut FunctionBuilder,
    layout: crate::vm::native::ValueLayout,
    value_addr: cranelift_codegen::ir::Value,
    bool_value: cranelift_codegen::ir::Value,
) {
    ssa_store_tag(b, layout, value_addr, layout.bool_tag);
    let one = b.ins().iconst(types::I8, 1);
    let zero = b.ins().iconst(types::I8, 0);
    let byte_value = b.ins().select(bool_value, one, zero);
    b.ins().store(
        MemFlags::new(),
        byte_value,
        value_addr,
        layout.bool_payload_offset,
    );
}

fn ssa_store_float_in_value(
    b: &mut FunctionBuilder,
    layout: crate::vm::native::ValueLayout,
    value_addr: cranelift_codegen::ir::Value,
    float_value: cranelift_codegen::ir::Value,
) {
    ssa_store_tag(b, layout, value_addr, layout.float_tag);
    b.ins().store(
        MemFlags::new(),
        float_value,
        value_addr,
        layout.float_payload_offset,
    );
}

fn ssa_is_scalar_tag(
    b: &mut FunctionBuilder,
    layout: crate::vm::native::ValueLayout,
    tag: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    let is_null = b
        .ins()
        .icmp_imm(IntCC::Equal, tag, i64::from(layout.null_tag));
    let is_int = b
        .ins()
        .icmp_imm(IntCC::Equal, tag, i64::from(layout.int_tag));
    let is_float = b
        .ins()
        .icmp_imm(IntCC::Equal, tag, i64::from(layout.float_tag));
    let is_bool = b
        .ins()
        .icmp_imm(IntCC::Equal, tag, i64::from(layout.bool_tag));
    let scalar_a = b.ins().bor(is_null, is_int);
    let scalar_b = b.ins().bor(is_float, is_bool);
    b.ins().bor(scalar_a, scalar_b)
}

fn ssa_copy_value_bytes(
    b: &mut FunctionBuilder,
    src_addr: cranelift_codegen::ir::Value,
    dst_addr: cranelift_codegen::ir::Value,
    size: i32,
) {
    let mut offset = 0i32;
    while offset + 8 <= size {
        let chunk = b.ins().load(types::I64, MemFlags::new(), src_addr, offset);
        b.ins().store(MemFlags::new(), chunk, dst_addr, offset);
        offset += 8;
    }
    if offset + 4 <= size {
        let chunk = b.ins().load(types::I32, MemFlags::new(), src_addr, offset);
        b.ins().store(MemFlags::new(), chunk, dst_addr, offset);
        offset += 4;
    }
    while offset < size {
        let chunk = b.ins().load(types::I8, MemFlags::new(), src_addr, offset);
        b.ins().store(MemFlags::new(), chunk, dst_addr, offset);
        offset += 1;
    }
}

fn ssa_store_heap_ptr_in_value(
    b: &mut FunctionBuilder,
    layout: crate::vm::native::ValueLayout,
    value_addr: cranelift_codegen::ir::Value,
    tag: u32,
    heap_ptr: cranelift_codegen::ir::Value,
) {
    ssa_store_tag(b, layout, value_addr, tag);
    b.ins().store(
        MemFlags::new(),
        heap_ptr,
        value_addr,
        layout.heap_payload_offset,
    );
}

fn ssa_call_alloc_buffer(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    heap_refs: HeapIntrinsicRefs,
    _heap_addrs: HeapIntrinsicAddrs,
    addr: usize,
    cap: cranelift_codegen::ir::Value,
) -> VmResult<cranelift_codegen::ir::Value> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, addr)?;
    let call = b
        .ins()
        .call_indirect(heap_refs.alloc_buffer_ref, helper_ptr, &[cap]);
    Ok(b.inst_results(call)[0])
}

fn ssa_call_pack_shared(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    heap_refs: HeapIntrinsicRefs,
    addr: usize,
    ptr: cranelift_codegen::ir::Value,
    len: cranelift_codegen::ir::Value,
    cap: cranelift_codegen::ir::Value,
) -> VmResult<cranelift_codegen::ir::Value> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, addr)?;
    let call = b
        .ins()
        .call_indirect(heap_refs.pack_shared_ref, helper_ptr, &[ptr, len, cap]);
    Ok(b.inst_results(call)[0])
}

fn ssa_call_copy_bytes(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    heap_refs: HeapIntrinsicRefs,
    heap_addrs: HeapIntrinsicAddrs,
    dst: cranelift_codegen::ir::Value,
    src: cranelift_codegen::ir::Value,
    len: cranelift_codegen::ir::Value,
) -> VmResult<()> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, heap_addrs.copy_bytes)?;
    b.ins()
        .call_indirect(heap_refs.copy_bytes_ref, helper_ptr, &[dst, src, len]);
    Ok(())
}

fn ssa_call_zero_bytes(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    heap_refs: HeapIntrinsicRefs,
    heap_addrs: HeapIntrinsicAddrs,
    dst: cranelift_codegen::ir::Value,
    len: cranelift_codegen::ir::Value,
) -> VmResult<()> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, heap_addrs.zero_bytes)?;
    b.ins()
        .call_indirect(heap_refs.free_buffer_ref, helper_ptr, &[dst, len]);
    Ok(())
}

fn ssa_call_string_contains(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    string_refs: SsaStringHelperRefs,
    string_addrs: SsaStringHelperAddrs,
    text: cranelift_codegen::ir::Value,
    needle: cranelift_codegen::ir::Value,
) -> VmResult<cranelift_codegen::ir::Value> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, string_addrs.contains)?;
    let call = b
        .ins()
        .call_indirect(string_refs.contains_ref, helper_ptr, &[text, needle]);
    Ok(b.inst_results(call)[0])
}

fn ssa_call_regex_match(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    string_refs: SsaStringHelperRefs,
    string_addrs: SsaStringHelperAddrs,
    vm_ptr: cranelift_codegen::ir::Value,
    pattern: cranelift_codegen::ir::Value,
    text: cranelift_codegen::ir::Value,
) -> VmResult<cranelift_codegen::ir::Value> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, string_addrs.regex_match)?;
    let call = b.ins().call_indirect(
        string_refs.regex_match_ref,
        helper_ptr,
        &[vm_ptr, pattern, text],
    );
    Ok(b.inst_results(call)[0])
}

#[allow(clippy::too_many_arguments)]
fn ssa_call_regex_replace(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    string_refs: SsaStringHelperRefs,
    string_addrs: SsaStringHelperAddrs,
    vm_ptr: cranelift_codegen::ir::Value,
    pattern: cranelift_codegen::ir::Value,
    text: cranelift_codegen::ir::Value,
    replacement: cranelift_codegen::ir::Value,
) -> VmResult<cranelift_codegen::ir::Value> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, string_addrs.regex_replace)?;
    let call = b.ins().call_indirect(
        string_refs.regex_replace_ref,
        helper_ptr,
        &[vm_ptr, pattern, text, replacement],
    );
    Ok(b.inst_results(call)[0])
}

fn ssa_call_string_replace_literal(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    string_refs: SsaStringHelperRefs,
    string_addrs: SsaStringHelperAddrs,
    text: cranelift_codegen::ir::Value,
    needle: cranelift_codegen::ir::Value,
    replacement: cranelift_codegen::ir::Value,
) -> VmResult<cranelift_codegen::ir::Value> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, string_addrs.replace_literal)?;
    let call = b.ins().call_indirect(
        string_refs.replace_ref,
        helper_ptr,
        &[text, needle, replacement],
    );
    Ok(b.inst_results(call)[0])
}

fn ssa_call_string_lower_ascii(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    string_refs: SsaStringHelperRefs,
    string_addrs: SsaStringHelperAddrs,
    text: cranelift_codegen::ir::Value,
) -> VmResult<cranelift_codegen::ir::Value> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, string_addrs.lower_ascii)?;
    let call = b
        .ins()
        .call_indirect(string_refs.lower_ascii_ref, helper_ptr, &[text]);
    Ok(b.inst_results(call)[0])
}

fn ssa_call_type_of(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    string_refs: SsaStringHelperRefs,
    string_addrs: SsaStringHelperAddrs,
    value: cranelift_codegen::ir::Value,
) -> VmResult<cranelift_codegen::ir::Value> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, string_addrs.type_of)?;
    let call = b
        .ins()
        .call_indirect(string_refs.type_of_ref, helper_ptr, &[value]);
    Ok(b.inst_results(call)[0])
}

fn ssa_call_to_string(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    string_refs: SsaStringHelperRefs,
    string_addrs: SsaStringHelperAddrs,
    value: cranelift_codegen::ir::Value,
) -> VmResult<cranelift_codegen::ir::Value> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, string_addrs.to_string)?;
    let call = b
        .ins()
        .call_indirect(string_refs.to_string_ref, helper_ptr, &[value]);
    Ok(b.inst_results(call)[0])
}

fn ssa_call_string_split_literal(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    string_refs: SsaStringHelperRefs,
    string_addrs: SsaStringHelperAddrs,
    text: cranelift_codegen::ir::Value,
    delimiter: cranelift_codegen::ir::Value,
) -> VmResult<cranelift_codegen::ir::Value> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, string_addrs.split_literal)?;
    let call = b.ins().call_indirect(
        string_refs.split_literal_ref,
        helper_ptr,
        &[text, delimiter],
    );
    Ok(b.inst_results(call)[0])
}

fn ssa_load_byte(
    b: &mut FunctionBuilder,
    ptr: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    let byte = b.ins().load(types::I8, MemFlags::new(), ptr, 0);
    b.ins().uextend(types::I32, byte)
}

fn ssa_is_utf8_continuation_byte(
    b: &mut FunctionBuilder,
    byte: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    let mask = b.ins().iconst(types::I32, 0xC0);
    let masked = b.ins().band(byte, mask);
    b.ins().icmp_imm(IntCC::Equal, masked, 0x80)
}

fn ssa_utf8_char_width(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    byte: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    let one = b.ins().iconst(pointer_type, 1);
    let two = b.ins().iconst(pointer_type, 2);
    let three = b.ins().iconst(pointer_type, 3);
    let four = b.ins().iconst(pointer_type, 4);
    let lt_80 = b.ins().icmp_imm(IntCC::UnsignedLessThan, byte, 0x80);
    let lt_e0 = b.ins().icmp_imm(IntCC::UnsignedLessThan, byte, 0xE0);
    let lt_f0 = b.ins().icmp_imm(IntCC::UnsignedLessThan, byte, 0xF0);
    let tail = b.ins().select(lt_f0, three, four);
    let wide = b.ins().select(lt_e0, two, tail);
    b.ins().select(lt_80, one, wide)
}

fn ssa_emit_trace_exit_status(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    offsets: ResolvedOffsets,
    ip: usize,
) -> VmResult<()> {
    let ip_val = b.ins().iconst(
        pointer_type,
        i64::try_from(ip)
            .map_err(|_| VmError::JitNative("SSA guard ip out of range".to_string()))?,
    );
    b.ins()
        .store(MemFlags::new(), ip_val, vm_ptr, offsets.vm_ip);
    let status = b.ins().iconst(types::I32, STATUS_TRACE_EXIT as i64);
    jump_with_status(b, exit_block, status);
    Ok(())
}

#[derive(Clone, Copy)]
struct ResolvedOffsets {
    stack_ptr: i32,
    stack_len: i32,
    locals_ptr: i32,
    locals_len: i32,
    vm_ip: i32,
    fuel_remaining: i32,
    fuel_ops_until_check: i32,
    epoch_deadline: i32,
    epoch_counter_ptr: i32,
    jit_native_region_edge_count: i32,
}

fn emit_interrupt_tick_inline(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    offsets: ResolvedOffsets,
    steps_to_advance: u32,
    interrupt_settings: NativeInterruptSettings,
) {
    if steps_to_advance == 0 {
        return;
    }

    let continue_block = b.create_block();
    match interrupt_settings.mode {
        NativeInterruptMode::Fuel => emit_fuel_tick_inline_core(
            b,
            vm_ptr,
            exit_block,
            offsets,
            steps_to_advance,
            interrupt_settings.check_interval,
            continue_block,
        ),
        NativeInterruptMode::Epoch => emit_epoch_tick_inline_core(
            b,
            vm_ptr,
            exit_block,
            offsets,
            steps_to_advance,
            interrupt_settings.check_interval,
            continue_block,
        ),
    }
    b.switch_to_block(continue_block);
}

fn emit_fuel_tick_inline_core(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    offsets: ResolvedOffsets,
    steps_to_advance: u32,
    fuel_check_interval: u32,
    continue_block: Block,
) {
    let countdown_block = b.create_block();
    let check_block = b.create_block();

    let ops_until_check = b.ins().load(
        types::I32,
        MemFlags::new(),
        vm_ptr,
        offsets.fuel_ops_until_check,
    );
    let chunk_steps = b.ins().iconst(types::I32, i64::from(steps_to_advance));
    let no_charge = b
        .ins()
        .icmp(IntCC::UnsignedGreaterThan, ops_until_check, chunk_steps);
    b.ins()
        .brif(no_charge, countdown_block, &[], check_block, &[]);

    b.switch_to_block(countdown_block);
    let new_ops = b.ins().isub(ops_until_check, chunk_steps);
    b.ins().store(
        MemFlags::new(),
        new_ops,
        vm_ptr,
        offsets.fuel_ops_until_check,
    );
    b.ins().jump(continue_block, &[]);

    b.switch_to_block(check_block);
    let interval_i32 = b.ins().iconst(types::I32, i64::from(fuel_check_interval));
    let consumed_after_first = b.ins().isub(chunk_steps, ops_until_check);
    let extra_checks = b.ins().udiv(consumed_after_first, interval_i32);
    let checks_i32 = b.ins().iadd_imm(extra_checks, 1);
    let remainder = b.ins().urem(consumed_after_first, interval_i32);
    let next_ops = b.ins().isub(interval_i32, remainder);
    let remaining = b
        .ins()
        .load(types::I64, MemFlags::new(), vm_ptr, offsets.fuel_remaining);
    let total_charge_i32 = b.ins().imul(checks_i32, interval_i32);
    let charge_amount = b.ins().uextend(types::I64, total_charge_i32);
    let enough_fuel = b
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, remaining, charge_amount);

    let charge_ok = b.create_block();
    let out_of_fuel = b.create_block();
    b.ins().brif(enough_fuel, charge_ok, &[], out_of_fuel, &[]);

    b.switch_to_block(charge_ok);
    let new_remaining = b.ins().isub(remaining, charge_amount);
    b.ins().store(
        MemFlags::new(),
        new_remaining,
        vm_ptr,
        offsets.fuel_remaining,
    );
    b.ins().store(
        MemFlags::new(),
        next_ops,
        vm_ptr,
        offsets.fuel_ops_until_check,
    );
    b.ins().jump(continue_block, &[]);

    b.switch_to_block(out_of_fuel);
    let status = b.ins().iconst(types::I32, STATUS_OUT_OF_FUEL as i64);
    jump_with_status(b, exit_block, status);
}

fn emit_epoch_tick_inline_core(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    offsets: ResolvedOffsets,
    steps_to_advance: u32,
    fuel_check_interval: u32,
    continue_block: Block,
) {
    let countdown_block = b.create_block();
    let check_block = b.create_block();

    let ops_until_check = b.ins().load(
        types::I32,
        MemFlags::new(),
        vm_ptr,
        offsets.fuel_ops_until_check,
    );
    let chunk_steps = b.ins().iconst(types::I32, i64::from(steps_to_advance));
    let no_charge = b
        .ins()
        .icmp(IntCC::UnsignedGreaterThan, ops_until_check, chunk_steps);
    b.ins()
        .brif(no_charge, countdown_block, &[], check_block, &[]);

    b.switch_to_block(countdown_block);
    let new_ops = b.ins().isub(ops_until_check, chunk_steps);
    b.ins().store(
        MemFlags::new(),
        new_ops,
        vm_ptr,
        offsets.fuel_ops_until_check,
    );
    b.ins().jump(continue_block, &[]);

    b.switch_to_block(check_block);
    let interval_i32 = b.ins().iconst(types::I32, i64::from(fuel_check_interval));
    let consumed_after_first = b.ins().isub(chunk_steps, ops_until_check);
    let remainder = b.ins().urem(consumed_after_first, interval_i32);
    let epoch_next_ops = b.ins().isub(interval_i32, remainder);
    let epoch_counter_ptr = b.ins().load(
        types::I64,
        MemFlags::new(),
        vm_ptr,
        offsets.epoch_counter_ptr,
    );
    let current_epoch = b
        .ins()
        .atomic_load(types::I64, MemFlags::new(), epoch_counter_ptr);
    let epoch_deadline = b
        .ins()
        .load(types::I64, MemFlags::new(), vm_ptr, offsets.epoch_deadline);
    let reached_deadline = b.ins().icmp(
        IntCC::UnsignedGreaterThanOrEqual,
        current_epoch,
        epoch_deadline,
    );
    let epoch_ok = b.create_block();
    let epoch_tripped = b.create_block();
    b.ins()
        .brif(reached_deadline, epoch_tripped, &[], epoch_ok, &[]);

    b.switch_to_block(epoch_ok);
    b.ins().store(
        MemFlags::new(),
        epoch_next_ops,
        vm_ptr,
        offsets.fuel_ops_until_check,
    );
    b.ins().jump(continue_block, &[]);

    b.switch_to_block(epoch_tripped);
    let status = b.ins().iconst(types::I32, STATUS_OUT_OF_FUEL as i64);
    jump_with_status(b, exit_block, status);
}

fn resolve_offsets(layout: NativeStackLayout) -> VmResult<ResolvedOffsets> {
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
        vm_ip: layout.vm_ip_offset,
        fuel_remaining: layout.vm_fuel_remaining_offset,
        fuel_ops_until_check: layout.vm_fuel_ops_until_check_offset,
        epoch_deadline: layout.vm_epoch_deadline_offset,
        epoch_counter_ptr: layout.vm_epoch_counter_ptr_offset,
        jit_native_region_edge_count: layout.vm_jit_native_region_edge_count_offset,
    })
}

pub(crate) fn compile_trace(
    trace: &JitTrace,
    internal_links: &[FusedRegionLink],
    interrupt_settings: Option<NativeInterruptSettings>,
    profile: NativeCompileProfile,
    drop_contract_events_enabled: bool,
) -> VmResult<CompiledTrace> {
    if interrupt_settings.is_some_and(|settings| settings.check_interval == 0) {
        return Err(VmError::InvalidFuelCheckInterval(0));
    }

    let body = try_compile_ssa_trace(
        trace,
        &trace.ssa,
        internal_links,
        interrupt_settings,
        profile,
        drop_contract_events_enabled,
    )?
    .ok_or_else(|| {
        VmError::JitNative(format!(
            "SSA native lowering does not support trace {} at root_ip {}",
            trace.id, trace.root_ip
        ))
    })?;
    let tail_entry = body.tail_entry;
    let lowering_kind = body.lowering_kind;
    let mut code = body.code;
    let mut wrapper = compile_system_inherited_tail_wrapper(tail_entry)?;
    code.extend_from_slice(&wrapper.code);
    wrapper._keepalive.dependencies.push(body.keepalive);
    Ok(CompiledTrace {
        entry: wrapper.entry,
        tail_entry,
        keepalive: wrapper._keepalive,
        code,
        lowering_kind,
    })
}

fn native_tail_isa() -> VmResult<OwnedTargetIsa> {
    let cached = CRANELIFT_TAIL_ISA.get_or_init(|| {
        let mut flag_builder = settings::builder();
        flag_builder
            .set("opt_level", "speed")
            .map_err(|err| format!("failed to set cranelift opt_level: {err}"))?;
        flag_builder
            .set("preserve_frame_pointers", "true")
            .map_err(|err| format!("failed to preserve tail-call frame pointers: {err}"))?;
        let isa_builder = cranelift_native::builder()
            .map_err(|err| format!("failed to build native tail ISA: {err}"))?;
        isa_builder
            .finish(settings::Flags::new(flag_builder))
            .map_err(|err| format!("failed to finalize cranelift tail ISA: {err}"))
    });
    cached.clone().map_err(VmError::JitNative)
}

fn native_isa(profile: NativeCompileProfile) -> VmResult<OwnedTargetIsa> {
    let cached = match profile {
        NativeCompileProfile::Jit => &CRANELIFT_JIT_ISA,
    };
    let cached = cached.get_or_init(|| {
        let mut flag_builder = settings::builder();
        flag_builder
            .set("opt_level", "speed")
            .map_err(|err| format!("failed to set cranelift opt_level: {err}"))?;
        let isa_builder = cranelift_native::builder()
            .map_err(|err| format!("failed to build native ISA: {err}"))?;
        isa_builder
            .finish(settings::Flags::new(flag_builder))
            .map_err(|err| format!("failed to finalize cranelift ISA: {err}"))
    });
    cached.clone().map_err(VmError::JitNative)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ValueType;
    use crate::vm::jit::ir::SsaTraceBuilder;

    #[derive(Clone, Copy)]
    enum BorrowingCollectionConsumer {
        ArraySet,
        ArrayPush,
        MapGet,
        MapHas,
        MapSet,
    }

    fn array_get_host_call_trace(
        mutate_before_call: bool,
        materialize_get_on_exit: bool,
    ) -> (SsaTrace, SsaValueId) {
        let mut builder = SsaTraceBuilder::new(0, 0);
        let entry = builder.entry();
        let array = builder
            .append_param(entry, SsaValueRepr::HeapPtr(ValueType::Array), "array")
            .unwrap();
        let index = builder
            .append_param(entry, SsaValueRepr::I64, "index")
            .unwrap();
        let mutation_value = builder
            .append_param(entry, SsaValueRepr::Tagged, "mutation_value")
            .unwrap();
        let get = builder
            .append_value_inst(
                entry,
                1,
                SsaValueRepr::Tagged,
                SsaInstKind::ArrayGet {
                    array: array.id,
                    index: index.id,
                },
            )
            .unwrap();
        if mutate_before_call {
            builder
                .append_value_inst(
                    entry,
                    2,
                    SsaValueRepr::Tagged,
                    SsaInstKind::ArraySet {
                        array: array.id,
                        index: index.id,
                        value: mutation_value.id,
                    },
                )
                .unwrap();
        }
        let call = builder
            .append_value_inst(
                entry,
                3,
                SsaValueRepr::Tagged,
                SsaInstKind::HostCall {
                    import: 0,
                    args: vec![get.id],
                },
            )
            .unwrap();
        let stack_value = if materialize_get_on_exit { get } else { call };
        let exit = builder.add_exit(
            4,
            vec![SsaMaterialization::Value(stack_value.id)],
            Vec::new(),
            Vec::new(),
        );
        builder
            .set_terminator(entry, SsaTerminator::Return { exit })
            .unwrap();
        (builder.finish(), get.id)
    }

    fn array_get_collection_consumer_trace(
        consumer: BorrowingCollectionConsumer,
    ) -> (SsaTrace, SsaValueId) {
        let mut builder = SsaTraceBuilder::new(0, 0);
        let entry = builder.entry();
        let array = builder
            .append_param(entry, SsaValueRepr::HeapPtr(ValueType::Array), "array")
            .unwrap();
        let index = builder
            .append_param(entry, SsaValueRepr::I64, "index")
            .unwrap();
        let map = builder
            .append_param(entry, SsaValueRepr::HeapPtr(ValueType::Map), "map")
            .unwrap();
        let map_key = builder
            .append_param(entry, SsaValueRepr::Tagged, "map_key")
            .unwrap();
        let get = builder
            .append_value_inst(
                entry,
                1,
                SsaValueRepr::Tagged,
                SsaInstKind::ArrayGet {
                    array: array.id,
                    index: index.id,
                },
            )
            .unwrap();
        let (repr, kind) = match consumer {
            BorrowingCollectionConsumer::ArraySet => (
                SsaValueRepr::Tagged,
                SsaInstKind::ArraySet {
                    array: array.id,
                    index: index.id,
                    value: get.id,
                },
            ),
            BorrowingCollectionConsumer::ArrayPush => (
                SsaValueRepr::Tagged,
                SsaInstKind::ArrayPush {
                    array: array.id,
                    value: get.id,
                },
            ),
            BorrowingCollectionConsumer::MapGet => (
                SsaValueRepr::Tagged,
                SsaInstKind::MapGet {
                    map: map.id,
                    key: get.id,
                },
            ),
            BorrowingCollectionConsumer::MapHas => (
                SsaValueRepr::Bool,
                SsaInstKind::MapHas {
                    map: map.id,
                    key: get.id,
                },
            ),
            BorrowingCollectionConsumer::MapSet => (
                SsaValueRepr::Tagged,
                SsaInstKind::MapSet {
                    map: map.id,
                    key: map_key.id,
                    value: get.id,
                },
            ),
        };
        let result = builder.append_value_inst(entry, 2, repr, kind).unwrap();
        let exit = builder.add_exit(
            3,
            vec![SsaMaterialization::Value(result.id)],
            Vec::new(),
            Vec::new(),
        );
        builder
            .set_terminator(entry, SsaTerminator::Return { exit })
            .unwrap();
        (builder.finish(), get.id)
    }

    #[test]
    fn borrows_single_use_array_get_for_immediate_host_call() {
        let (trace, get) = array_get_host_call_trace(false, false);
        assert_eq!(borrowed_array_get_outputs(&trace), BTreeSet::from([get]));
    }

    #[test]
    fn borrows_single_use_array_get_for_typed_collection_consumers() {
        for consumer in [
            BorrowingCollectionConsumer::ArraySet,
            BorrowingCollectionConsumer::ArrayPush,
            BorrowingCollectionConsumer::MapGet,
            BorrowingCollectionConsumer::MapHas,
            BorrowingCollectionConsumer::MapSet,
        ] {
            let (trace, get) = array_get_collection_consumer_trace(consumer);
            assert_eq!(borrowed_array_get_outputs(&trace), BTreeSet::from([get]));
        }
    }

    #[test]
    fn does_not_borrow_array_get_across_array_mutation() {
        let (trace, _) = array_get_host_call_trace(true, false);
        assert!(borrowed_array_get_outputs(&trace).is_empty());
    }

    #[test]
    fn does_not_borrow_array_get_that_escapes_to_an_exit() {
        let (trace, _) = array_get_host_call_trace(false, true);
        assert!(borrowed_array_get_outputs(&trace).is_empty());
    }
}
