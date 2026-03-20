use super::super::super::{Value, VmError, VmResult};
use super::super::JitTrace;
use super::{NativeCompileProfile, TraceLoweringKind};
use crate::vm::jit::deopt::exit_inputs;
use crate::vm::jit::ir::{
    SsaBranchTarget, SsaExitId, SsaInstKind, SsaMaterialization, SsaTerminator, SsaTrace,
    SsaValueId, SsaValueRepr,
};
use crate::vm::native::{
    ExecutableBuffer, NativeInterruptMode, NativeInterruptSettings, NativeStackLayout,
    STATUS_CONTINUE, STATUS_HALTED, STATUS_OUT_OF_FUEL, STATUS_TRACE_EXIT,
    box_heap_value_signature, checked_add_i32, clone_value_signature,
    clone_value_to_slot_entry_address, detect_native_stack_layout, entry_signature,
    jump_with_status, restore_exit_signature, restore_exit_state_entry_address,
    write_heap_value_to_slot_entry_address,
};
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::immediates::Ieee64;
use cranelift_codegen::ir::{
    Block, BlockArg, InstBuilder, MemFlags, StackSlotData, StackSlotKind, types,
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
    let box_heap_value_sig = box_heap_value_signature(pointer_type, call_conv);
    let restore_exit_sig = restore_exit_signature(pointer_type, call_conv);

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
        let deopt_refs = SsaDeoptHelperRefs {
            clone_value_ref: b.import_signature(clone_value_sig),
            box_heap_value_ref: b.import_signature(box_heap_value_sig),
            restore_exit_ref: b.import_signature(restore_exit_sig),
        };
        let deopt_addrs = SsaDeoptHelperAddrs {
            clone_value: clone_value_to_slot_entry_address(),
            box_heap_value: write_heap_value_to_slot_entry_address(),
            restore_exit: restore_exit_state_entry_address(),
        };

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
                lower_ssa_inst(
                    &mut b,
                    vm_ptr,
                    exit_block,
                    pointer_type,
                    layout,
                    offsets,
                    block.id == ssa.entry,
                    inst,
                    &tagged_constant_addrs,
                    &mut values,
                )?;
            }
            lower_ssa_terminator(
                &mut b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
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
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                deopt_refs,
                deopt_addrs,
                exit,
                spec,
                false,
            )?;
            lower_ssa_exit_block(
                &mut b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                deopt_refs,
                deopt_addrs,
                exit,
                spec,
                true,
            )?;
        }

        b.switch_to_block(exit_block);
        let final_status = b.block_params(exit_block)[0];
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
    box_heap_value_ref: cranelift_codegen::ir::SigRef,
    restore_exit_ref: cranelift_codegen::ir::SigRef,
}

#[derive(Clone, Copy)]
struct SsaDeoptHelperAddrs {
    clone_value: usize,
    box_heap_value: usize,
    restore_exit: usize,
}

fn ssa_trace_supported(ssa: &SsaTrace) -> bool {
    for block in &ssa.blocks {
        for inst in &block.insts {
            if !matches!(
                inst.kind,
                SsaInstKind::Constant(_)
                    | SsaInstKind::UnboxInt { .. }
                    | SsaInstKind::UnboxFloat { .. }
                    | SsaInstKind::UnboxBool { .. }
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
            if matches!(
                inst.kind,
                SsaInstKind::UnboxInt { .. }
                    | SsaInstKind::UnboxFloat { .. }
                    | SsaInstKind::UnboxBool { .. }
            ) && block.id != ssa.entry
            {
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

fn prepare_tagged_constants(
    ssa: &SsaTrace,
) -> VmResult<(Box<[Value]>, HashMap<SsaValueId, usize>)> {
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
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: crate::vm::native::NativeStackLayout,
    offsets: ResolvedOffsets,
    is_entry_block: bool,
    inst: &crate::vm::jit::ir::SsaInst,
    tagged_constant_addrs: &HashMap<SsaValueId, usize>,
    values: &mut HashMap<SsaValueId, cranelift_codegen::ir::Value>,
) -> VmResult<()> {
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
            if !is_entry_block {
                return Err(VmError::JitNative(
                    "SSA int unbox outside entry block requires snapshots".to_string(),
                ));
            }
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
            if !is_entry_block {
                return Err(VmError::JitNative(
                    "SSA float unbox outside entry block requires snapshots".to_string(),
                ));
            }
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
            if !is_entry_block {
                return Err(VmError::JitNative(
                    "SSA bool unbox outside entry block requires snapshots".to_string(),
                ));
            }
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
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: crate::vm::native::NativeStackLayout,
    offsets: ResolvedOffsets,
    terminator: &SsaTerminator,
    values: &HashMap<SsaValueId, cranelift_codegen::ir::Value>,
    block_handles: &HashMap<crate::vm::jit::ir::SsaBlockId, Block>,
    exit_specs: &HashMap<SsaExitId, SsaExitLowering>,
) -> VmResult<()> {
    let _ = (vm_ptr, exit_block, pointer_type, layout, offsets);
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
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: crate::vm::native::NativeStackLayout,
    deopt_refs: SsaDeoptHelperRefs,
    deopt_addrs: SsaDeoptHelperAddrs,
    exit: &crate::vm::jit::ir::SsaExit,
    spec: &SsaExitLowering,
    halted: bool,
) -> VmResult<()> {
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
        ssa_materialize_slot(
            b,
            exit_block,
            pointer_type,
            layout.value,
            &exit_values,
            deopt_refs,
            deopt_addrs,
            materialization,
            dst_addr,
            "stack",
        )?;
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
        ssa_materialize_slot(
            b,
            exit_block,
            pointer_type,
            layout.value,
            &exit_values,
            deopt_refs,
            deopt_addrs,
            materialization,
            dst_addr,
            "local",
        )?;
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
    } else {
        b.ins().iconst(types::I32, STATUS_TRACE_EXIT as i64)
    };
    jump_with_status(b, exit_block, status);
    Ok(())
}

fn ssa_materialize_slot(
    b: &mut FunctionBuilder,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    value_layout: crate::vm::native::ValueLayout,
    exit_values: &HashMap<SsaValueId, cranelift_codegen::ir::Value>,
    deopt_refs: SsaDeoptHelperRefs,
    deopt_addrs: SsaDeoptHelperAddrs,
    materialization: &SsaMaterialization,
    dst_addr: cranelift_codegen::ir::Value,
    slot_kind: &'static str,
) -> VmResult<()> {
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
