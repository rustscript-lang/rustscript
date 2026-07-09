use super::super::super::{Value, VmError, VmResult};
use super::super::JitTrace;
use super::super::runtime::resume_linked_trace_entry_address;
use super::{NativeCompileProfile, TraceLoweringKind};
use crate::vm::jit::deopt::exit_inputs;
use crate::vm::jit::ir::{
    SsaBranchTarget, SsaExitId, SsaInstKind, SsaMaterialization, SsaTerminator, SsaTrace,
    SsaValueId, SsaValueRepr,
};
use crate::vm::native::{
    ExecutableBuffer, HeapIntrinsicAddrs, HeapIntrinsicRefs, NativeInterruptMode,
    NativeInterruptSettings, NativeStackLayout, STATUS_CONTINUE, STATUS_ERROR, STATUS_HALTED,
    STATUS_OUT_OF_FUEL, STATUS_TRACE_EXIT, alloc_buffer_signature, alloc_byte_buffer_entry_address,
    alloc_value_buffer_entry_address, box_heap_value_signature, checked_add_i32,
    clear_value_slot_entry_address, clone_value_signature, clone_value_to_slot_entry_address,
    collection_get_signature, collection_predicate_signature, copy_bytes_entry_address,
    copy_bytes_signature, detect_native_stack_layout, entry_signature, free_buffer_signature,
    init_null_value_slot_entry_address, jump_with_status, map_get_entry_address,
    map_has_entry_address, pack_shared_signature, restore_exit_signature,
    restore_exit_state_entry_address, shared_array_from_buffer_entry_address,
    shared_bytes_from_buffer_entry_address, shared_string_from_buffer_entry_address,
    value_slot_signature, write_heap_value_to_slot_entry_address, zero_bytes_entry_address,
};
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::immediates::Ieee64;
use cranelift_codegen::ir::{
    Block, BlockArg, InstBuilder, MemFlags, StackSlot, StackSlotData, StackSlotKind, types,
};
use cranelift_codegen::isa::OwnedTargetIsa;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module};
use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

static CRANELIFT_TRACE_ID: AtomicU64 = AtomicU64::new(1);
static CRANELIFT_JIT_ISA: OnceLock<Result<OwnedTargetIsa, String>> = OnceLock::new();

type TaggedConstants = (Box<[Value]>, HashMap<SsaValueId, usize>);

pub(crate) struct CompiledTrace {
    pub(crate) entry: *const u8,
    pub(crate) keepalive: TraceKeepAlive,
    pub(crate) code: Vec<u8>,
    pub(crate) lowering_kind: TraceLoweringKind,
}

pub(crate) struct TraceKeepAlive {
    exec: ExecutableBuffer,
    _tagged_constants: Box<[Value]>,
}

impl TraceKeepAlive {
    fn from_code(code: &[u8], tagged_constants: Box<[Value]>) -> VmResult<Self> {
        Ok(Self {
            exec: ExecutableBuffer::new(code)?,
            _tagged_constants: tagged_constants,
        })
    }

    fn entry(&self) -> *const u8 {
        self.exec.entry()
    }
}

fn try_compile_ssa_trace(
    trace: &JitTrace,
    ssa: &SsaTrace,
    interrupt_settings: Option<NativeInterruptSettings>,
    profile: NativeCompileProfile,
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
    let isa = native_isa(profile)?;

    let jit_builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    let mut module = JITModule::new(jit_builder);
    let pointer_type = module.target_config().pointer_type();
    let call_conv = module.target_config().default_call_conv;

    let mut ctx = module.make_context();
    ctx.func.signature = entry_signature(pointer_type, call_conv);
    let clone_value_sig = clone_value_signature(pointer_type, call_conv);
    let value_slot_sig = value_slot_signature(pointer_type, call_conv);
    let box_heap_value_sig = box_heap_value_signature(pointer_type, call_conv);
    let alloc_buffer_sig = alloc_buffer_signature(pointer_type, call_conv);
    let free_buffer_sig = free_buffer_signature(pointer_type, call_conv);
    let pack_shared_sig = pack_shared_signature(pointer_type, call_conv);
    let copy_bytes_sig = copy_bytes_signature(pointer_type, call_conv);
    let map_has_sig = collection_predicate_signature(pointer_type, call_conv);
    let map_get_sig = collection_get_signature(pointer_type, call_conv);
    let restore_exit_sig = restore_exit_signature(pointer_type, call_conv);
    let resume_linked_trace_sig = entry_signature(pointer_type, call_conv);

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
        let deopt_refs = SsaDeoptHelperRefs {
            clone_value_ref: b.import_signature(clone_value_sig),
            init_null_slot_ref: b.import_signature(value_slot_sig.clone()),
            clear_value_slot_ref: b.import_signature(value_slot_sig),
            box_heap_value_ref: b.import_signature(box_heap_value_sig),
            map_has_ref: b.import_signature(map_has_sig),
            map_get_ref: b.import_signature(map_get_sig),
            restore_exit_ref: b.import_signature(restore_exit_sig),
            resume_linked_trace_ref: b.import_signature(resume_linked_trace_sig),
        };
        let deopt_addrs = SsaDeoptHelperAddrs {
            clone_value: clone_value_to_slot_entry_address(),
            init_null_slot: init_null_value_slot_entry_address(),
            clear_value_slot: clear_value_slot_entry_address(),
            box_heap_value: write_heap_value_to_slot_entry_address(),
            map_has: map_has_entry_address(),
            map_get: map_get_entry_address(),
            restore_exit: restore_exit_state_entry_address(),
            resume_linked_trace: resume_linked_trace_entry_address(),
        };
        let allow_exit_link_handoff = !trace.has_call && !trace.has_yielding_call;

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
        let owned_value_temps = allocate_owned_value_temps(&mut b, ssa, layout.value.size)?;

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

        let mut exit_specs = HashMap::new();
        for exit in &ssa.exits {
            let inputs = exit_inputs(exit);
            let trace_exit_block = b.create_block();
            let halted_block = b.create_block();
            for value in &inputs {
                let Some(repr) = value_reprs.get(value).copied() else {
                    return Ok(None);
                };
                let Some(ty) = ssa_type(pointer_type, repr) else {
                    return Ok(None);
                };
                b.append_block_param(trace_exit_block, ty);
                b.append_block_param(halted_block, ty);
            }
            exit_specs.insert(
                exit.id,
                SsaExitLowering {
                    trace_exit_block,
                    halted_block,
                    inputs,
                },
            );
        }

        b.switch_to_block(entry_block);
        b.append_block_params_for_function_params(entry_block);
        let vm_ptr = b.block_params(entry_block)[0];
        let lower_ctx = SsaLowerCtx {
            vm_ptr,
            exit_block,
            pointer_type,
            layout,
            offsets,
            heap_refs,
            heap_addrs,
            helper_refs: deopt_refs,
            helper_addrs: deopt_addrs,
            owned_value_temps: &owned_value_temps,
            value_reprs: &value_reprs,
            tagged_constant_addrs: &tagged_constant_addrs,
        };
        let root_ip = b.ins().iconst(
            pointer_type,
            i64::try_from(trace.root_ip)
                .map_err(|_| VmError::JitNative("trace root ip out of range".to_string()))?,
        );
        b.ins()
            .store(MemFlags::new(), root_ip, vm_ptr, offsets.vm_ip);

        let entry_ssa_block = ssa
            .blocks
            .get(ssa.entry.index())
            .ok_or_else(|| VmError::JitNative("SSA entry block missing".to_string()))?;
        let entry_handle = *block_handles
            .get(&ssa.entry)
            .ok_or_else(|| VmError::JitNative("SSA entry block handle missing".to_string()))?;
        let entry_args = build_entry_args(
            &mut b,
            vm_ptr,
            pointer_type,
            layout,
            offsets,
            entry_ssa_block.params.len(),
        )?;
        init_owned_value_temps(
            &mut b,
            exit_block,
            pointer_type,
            deopt_refs,
            deopt_addrs,
            &owned_value_temps,
        )?;
        let entry_args = ssa_block_args(entry_args);
        b.ins().jump(entry_handle, &entry_args);

        let charge_blocks = ssa_interrupt_charge_blocks(ssa);
        let ops_to_advance = u32::try_from(trace.op_names.len().max(1))
            .map_err(|_| VmError::JitNative("trace op count exceeds u32".to_string()))?;
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
            if charge_blocks.contains(&block.id)
                && let Some(settings) = interrupt_settings
            {
                emit_interrupt_tick_inline(
                    &mut b,
                    vm_ptr,
                    exit_block,
                    offsets,
                    ops_to_advance,
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
                false,
                allow_exit_link_handoff,
            )?;
            lower_ssa_exit_block(&mut b, lower_ctx, exit, spec, true, false)?;
        }

        b.switch_to_block(exit_block);
        let final_status = b.block_params(exit_block)[0];
        clear_owned_value_temps(
            &mut b,
            exit_block,
            pointer_type,
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
        keepalive,
        code,
        lowering_kind: TraceLoweringKind::Ssa,
    }))
}

#[derive(Clone)]
struct SsaExitLowering {
    trace_exit_block: Block,
    halted_block: Block,
    inputs: Vec<SsaValueId>,
}

#[derive(Clone, Copy)]
struct SsaDeoptHelperRefs {
    clone_value_ref: cranelift_codegen::ir::SigRef,
    init_null_slot_ref: cranelift_codegen::ir::SigRef,
    clear_value_slot_ref: cranelift_codegen::ir::SigRef,
    box_heap_value_ref: cranelift_codegen::ir::SigRef,
    map_has_ref: cranelift_codegen::ir::SigRef,
    map_get_ref: cranelift_codegen::ir::SigRef,
    restore_exit_ref: cranelift_codegen::ir::SigRef,
    resume_linked_trace_ref: cranelift_codegen::ir::SigRef,
}

#[derive(Clone, Copy)]
struct SsaDeoptHelperAddrs {
    clone_value: usize,
    init_null_slot: usize,
    clear_value_slot: usize,
    box_heap_value: usize,
    map_has: usize,
    map_get: usize,
    restore_exit: usize,
    resume_linked_trace: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SsaTempValueSlotKey {
    Output(SsaValueId),
    MapKeyBox(SsaValueId),
}

#[derive(Clone)]
struct SsaOwnedValueTemps {
    ordered: Vec<StackSlot>,
    slots: HashMap<SsaTempValueSlotKey, StackSlot>,
}

#[derive(Clone, Copy)]
struct SsaLowerCtx<'a> {
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: crate::vm::native::NativeStackLayout,
    offsets: ResolvedOffsets,
    heap_refs: HeapIntrinsicRefs,
    heap_addrs: HeapIntrinsicAddrs,
    helper_refs: SsaDeoptHelperRefs,
    helper_addrs: SsaDeoptHelperAddrs,
    owned_value_temps: &'a SsaOwnedValueTemps,
    value_reprs: &'a HashMap<SsaValueId, SsaValueRepr>,
    tagged_constant_addrs: &'a HashMap<SsaValueId, usize>,
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
                    | SsaInstKind::UnboxHeapPtr { .. }
                    | SsaInstKind::UnboxInt { .. }
                    | SsaInstKind::UnboxFloat { .. }
                    | SsaInstKind::UnboxBool { .. }
                    | SsaInstKind::StringLen { .. }
                    | SsaInstKind::BytesLen { .. }
                    | SsaInstKind::StringSlice { .. }
                    | SsaInstKind::BytesSlice { .. }
                    | SsaInstKind::StringGet { .. }
                    | SsaInstKind::BytesGet { .. }
                    | SsaInstKind::BytesHas { .. }
                    | SsaInstKind::StringConcat { .. }
                    | SsaInstKind::BytesConcat { .. }
                    | SsaInstKind::BytesFromArrayU8 { .. }
                    | SsaInstKind::BytesToArrayU8 { .. }
                    | SsaInstKind::ArrayLen { .. }
                    | SsaInstKind::ArrayGet { .. }
                    | SsaInstKind::ArrayHas { .. }
                    | SsaInstKind::MapLen { .. }
                    | SsaInstKind::MapGet { .. }
                    | SsaInstKind::MapHas { .. }
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
                    | SsaInstKind::IntCmpLt { .. }
                    | SsaInstKind::IntCmpLtImm { .. }
                    | SsaInstKind::IntCmpGt { .. }
                    | SsaInstKind::IntCmpGtImm { .. }
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
        for materialization in &exit.locals {
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

fn allocate_owned_value_temps(
    b: &mut FunctionBuilder,
    ssa: &SsaTrace,
    value_size: i32,
) -> VmResult<SsaOwnedValueTemps> {
    let mut ordered = Vec::new();
    let mut slots = HashMap::new();
    for block in &ssa.blocks {
        for inst in &block.insts {
            let Some(output) = inst.output else {
                continue;
            };
            if ssa_inst_requires_owned_value_slot(&inst.kind) {
                let slot = ssa_create_value_stack_slot(b, value_size)?;
                ordered.push(slot);
                slots.insert(SsaTempValueSlotKey::Output(output.id), slot);
            }
            if matches!(
                inst.kind,
                SsaInstKind::MapGet { .. } | SsaInstKind::MapHas { .. }
            ) {
                let slot = ssa_create_value_stack_slot(b, value_size)?;
                ordered.push(slot);
                slots.insert(SsaTempValueSlotKey::MapKeyBox(output.id), slot);
            }
        }
    }
    Ok(SsaOwnedValueTemps { ordered, slots })
}

fn ssa_inst_requires_owned_value_slot(kind: &SsaInstKind) -> bool {
    matches!(
        kind,
        SsaInstKind::ArrayGet { .. }
            | SsaInstKind::MapGet { .. }
            | SsaInstKind::StringSlice { .. }
            | SsaInstKind::BytesSlice { .. }
            | SsaInstKind::StringGet { .. }
            | SsaInstKind::BytesFromArrayU8 { .. }
            | SsaInstKind::BytesToArrayU8 { .. }
            | SsaInstKind::StringConcat { .. }
            | SsaInstKind::BytesConcat { .. }
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
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    helper_refs: SsaDeoptHelperRefs,
    helper_addrs: SsaDeoptHelperAddrs,
    temps: &SsaOwnedValueTemps,
) -> VmResult<()> {
    let _ = exit_block;
    for slot in &temps.ordered {
        let addr = b.ins().stack_addr(pointer_type, *slot, 0);
        ssa_call_infallible_helper(
            b,
            pointer_type,
            helper_refs.init_null_slot_ref,
            helper_addrs.init_null_slot,
            &[addr],
        )?;
    }
    Ok(())
}

fn clear_owned_value_temps(
    b: &mut FunctionBuilder,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    helper_refs: SsaDeoptHelperRefs,
    helper_addrs: SsaDeoptHelperAddrs,
    temps: &SsaOwnedValueTemps,
) -> VmResult<()> {
    let _ = exit_block;
    for slot in &temps.ordered {
        let addr = b.ins().stack_addr(pointer_type, *slot, 0);
        ssa_call_infallible_helper(
            b,
            pointer_type,
            helper_refs.clear_value_slot_ref,
            helper_addrs.clear_value_slot,
            &[addr],
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
        SsaTerminator::Exit { .. } | SsaTerminator::Return { .. } => {}
    }
    targets
}

fn build_entry_args(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    pointer_type: cranelift_codegen::ir::Type,
    layout: crate::vm::native::NativeStackLayout,
    offsets: ResolvedOffsets,
    local_count: usize,
) -> VmResult<Vec<cranelift_codegen::ir::Value>> {
    let locals_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.locals_ptr);
    let mut args = Vec::with_capacity(local_count);
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
        helper_refs,
        helper_addrs,
        owned_value_temps,
        value_reprs,
        tagged_constant_addrs,
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
            Value::Null | Value::String(_) | Value::Bytes(_) | Value::Array(_) | Value::Map(_),
        ) => {
            let addr = tagged_constant_addrs
                .get(&output.id)
                .copied()
                .ok_or_else(|| {
                    VmError::JitNative("SSA tagged constant lowering address missing".to_string())
                })?;
            iconst_ptr_from_addr(b, pointer_type, addr)?
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

fn lower_ssa_terminator(
    b: &mut FunctionBuilder,
    _ctx: SsaLowerCtx<'_>,
    terminator: &SsaTerminator,
    values: &HashMap<SsaValueId, cranelift_codegen::ir::Value>,
    block_handles: &HashMap<crate::vm::jit::ir::SsaBlockId, Block>,
    exit_specs: &HashMap<SsaExitId, SsaExitLowering>,
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
            let true_target =
                ssa_branch_target_block(if_true, values, block_handles, exit_specs, false)?;
            let false_target =
                ssa_branch_target_block(if_false, values, block_handles, exit_specs, false)?;
            let true_args = ssa_block_args(true_target.1);
            let false_args = ssa_block_args(false_target.1);
            b.ins().brif(
                condition,
                true_target.0,
                &true_args,
                false_target.0,
                &false_args,
            );
        }
        SsaTerminator::Exit { exit } => {
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
    }
    Ok(())
}

fn ssa_branch_target_block(
    target: &SsaBranchTarget,
    values: &HashMap<SsaValueId, cranelift_codegen::ir::Value>,
    block_handles: &HashMap<crate::vm::jit::ir::SsaBlockId, Block>,
    exit_specs: &HashMap<SsaExitId, SsaExitLowering>,
    halted: bool,
) -> VmResult<(Block, Vec<cranelift_codegen::ir::Value>)> {
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
            Ok((handle, lowered_args))
        }
        SsaBranchTarget::Exit(exit) => {
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
            Ok((
                if halted {
                    spec.halted_block
                } else {
                    spec.trace_exit_block
                },
                lowered_args,
            ))
        }
    }
}

fn lower_ssa_exit_block(
    b: &mut FunctionBuilder,
    ctx: SsaLowerCtx<'_>,
    exit: &crate::vm::jit::ir::SsaExit,
    spec: &SsaExitLowering,
    halted: bool,
    allow_link_handoff: bool,
) -> VmResult<()> {
    let SsaLowerCtx {
        vm_ptr,
        exit_block,
        pointer_type,
        layout,
        helper_refs: deopt_refs,
        helper_addrs: deopt_addrs,
        ..
    } = ctx;
    let block = if halted {
        spec.halted_block
    } else {
        spec.trace_exit_block
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

    let locals_ptr = ssa_alloc_value_buffer(b, pointer_type, exit.locals.len(), layout.value.size)?;
    for (local_index, materialization) in exit.locals.iter().enumerate() {
        let dst_addr = ssa_value_buffer_slot_addr(
            b,
            pointer_type,
            locals_ptr,
            local_index,
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
    let locals_len = b.ins().iconst(
        pointer_type,
        i64::try_from(exit.locals.len())
            .map_err(|_| VmError::JitNative("SSA exit locals length out of range".to_string()))?,
    );
    let ip_val = b.ins().iconst(
        pointer_type,
        i64::try_from(exit.exit_ip)
            .map_err(|_| VmError::JitNative("SSA exit ip out of range".to_string()))?,
    );
    let null_ptr = b.ins().iconst(pointer_type, 0);
    let stack_ptr = stack_ptr.unwrap_or(null_ptr);
    let locals_ptr = locals_ptr.unwrap_or(null_ptr);
    ssa_call_status_helper(
        b,
        exit_block,
        pointer_type,
        deopt_refs.restore_exit_ref,
        deopt_addrs.restore_exit,
        &[vm_ptr, stack_ptr, stack_len, locals_ptr, locals_len, ip_val],
    )?;
    let status = if halted {
        b.ins().iconst(types::I32, STATUS_HALTED as i64)
    } else if allow_link_handoff {
        let helper_ptr = iconst_ptr_from_addr(b, pointer_type, deopt_addrs.resume_linked_trace)?;
        let call = b
            .ins()
            .call_indirect(deopt_refs.resume_linked_trace_ref, helper_ptr, &[vm_ptr]);
        b.inst_results(call)[0]
    } else {
        b.ins().iconst(types::I32, STATUS_TRACE_EXIT as i64)
    };
    jump_with_status(b, exit_block, status);
    Ok(())
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
    locals_ptr: i32,
    vm_ip: i32,
    fuel_remaining: i32,
    fuel_ops_until_check: i32,
    epoch_deadline: i32,
    epoch_counter_ptr: i32,
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
    let locals_ptr = checked_add_i32(
        layout.vm_locals_offset,
        layout.stack_vec.ptr_offset,
        "locals ptr offset overflow",
    )?;

    Ok(ResolvedOffsets {
        locals_ptr,
        vm_ip: layout.vm_ip_offset,
        fuel_remaining: layout.vm_fuel_remaining_offset,
        fuel_ops_until_check: layout.vm_fuel_ops_until_check_offset,
        epoch_deadline: layout.vm_epoch_deadline_offset,
        epoch_counter_ptr: layout.vm_epoch_counter_ptr_offset,
    })
}

pub(crate) fn compile_trace(
    trace: &JitTrace,
    interrupt_settings: Option<NativeInterruptSettings>,
    profile: NativeCompileProfile,
    drop_contract_events_enabled: bool,
) -> VmResult<CompiledTrace> {
    if interrupt_settings.is_some_and(|settings| settings.check_interval == 0) {
        return Err(VmError::InvalidFuelCheckInterval(0));
    }

    try_compile_ssa_trace(
        trace,
        &trace.ssa,
        interrupt_settings,
        profile,
        drop_contract_events_enabled,
    )?
    .ok_or_else(|| {
        VmError::JitNative(format!(
            "SSA native lowering does not support trace {} at root_ip {}",
            trace.id, trace.root_ip
        ))
    })
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
