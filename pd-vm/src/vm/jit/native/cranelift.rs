use super::super::super::{
    HostCallExecOutcome, NumericValue, Program, Value, Vm, VmError, VmResult,
};
use super::super::{JitTrace, TraceStep};
use super::{
    STATUS_CONTINUE, STATUS_ERROR, STATUS_HALTED, STATUS_TRACE_EXIT, STATUS_WAITING,
    STATUS_YIELDED, store_bridge_error,
};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    AbiParam, Block, BlockArg, FuncRef, InstBuilder, MemFlags, Signature, types,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

static CRANELIFT_TRACE_ID: AtomicU64 = AtomicU64::new(1);
static NATIVE_STACK_LAYOUT: OnceLock<Result<NativeStackLayout, String>> = OnceLock::new();

const OP_LDC: i64 = 1;
const OP_ADD: i64 = 2;
const OP_SUB: i64 = 3;
const OP_MUL: i64 = 4;
const OP_DIV: i64 = 5;
const OP_MOD: i64 = 6;
const OP_SHL: i64 = 7;
const OP_SHR: i64 = 8;
const OP_AND: i64 = 9;
const OP_OR: i64 = 10;
const OP_NEG: i64 = 11;
const OP_CEQ: i64 = 12;
const OP_CLT: i64 = 13;
const OP_CGT: i64 = 14;
const OP_POP: i64 = 15;
const OP_DUP: i64 = 16;
const OP_LDLOC: i64 = 17;
const OP_STLOC: i64 = 18;
const OP_CALL: i64 = 19;
const OP_GUARD_FALSE: i64 = 20;
const OP_JUMP: i64 = 21;

pub(crate) struct CraneliftCompiledTrace {
    pub(crate) entry: *const u8,
    pub(crate) keepalive: CraneliftTraceKeepAlive,
    pub(crate) code: Vec<u8>,
}

pub(crate) struct CraneliftTraceKeepAlive {
    _module: JITModule,
}

#[derive(Clone, Copy)]
struct VecLayout {
    ptr_offset: i32,
    len_offset: i32,
    cap_offset: i32,
}

#[derive(Clone, Copy)]
struct ValueLayout {
    size: i32,
    tag_offset: i32,
    tag_size: u8,
    null_tag: u32,
    int_tag: u32,
    float_tag: u32,
    bool_tag: u32,
    int_payload_offset: i32,
    bool_payload_offset: i32,
}

#[derive(Clone, Copy)]
struct NativeStackLayout {
    vm_stack_offset: i32,
    vm_locals_offset: i32,
    vm_program_offset: i32,
    vm_ip_offset: i32,
    program_constants_offset: i32,
    stack_vec: VecLayout,
    value: ValueLayout,
}

#[derive(Clone, Copy)]
struct ResolvedOffsets {
    stack_ptr: i32,
    stack_len: i32,
    stack_cap: i32,
    locals_ptr: i32,
    locals_len: i32,
    constants_ptr: i32,
    constants_len: i32,
    vm_ip: i32,
}

pub(crate) fn compile_trace(trace: &JitTrace) -> VmResult<CraneliftCompiledTrace> {
    let layout = detect_native_stack_layout()?;
    let offsets = resolve_offsets(layout)?;

    let mut flag_builder = settings::builder();
    flag_builder
        .set("opt_level", "speed")
        .map_err(|err| VmError::JitNative(format!("failed to set cranelift opt_level: {err}")))?;
    let isa_builder = cranelift_native::builder()
        .map_err(|err| VmError::JitNative(format!("failed to build native ISA: {err}")))?;
    let isa = isa_builder
        .finish(settings::Flags::new(flag_builder))
        .map_err(|err| VmError::JitNative(format!("failed to finalize cranelift ISA: {err}")))?;

    let mut jit_builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    jit_builder.symbol("pd_vm_cranelift_step", pd_vm_cranelift_step as *const u8);

    let mut module = JITModule::new(jit_builder);
    let pointer_type = module.target_config().pointer_type();
    let call_conv = module.target_config().default_call_conv;

    let helper_sig = helper_signature(pointer_type, call_conv);
    let helper_id = module
        .declare_function("pd_vm_cranelift_step", Linkage::Import, &helper_sig)
        .map_err(|err| VmError::JitNative(format!("declare import failed: {err}")))?;

    let mut ctx = module.make_context();
    ctx.func.signature = entry_signature(pointer_type, call_conv);

    let trace_id = CRANELIFT_TRACE_ID.fetch_add(1, Ordering::Relaxed);
    let func_name = format!("pd_vm_trace_cranelift_{trace_id}");
    let func_id = module
        .declare_function(&func_name, Linkage::Local, &ctx.func.signature)
        .map_err(|err| VmError::JitNative(format!("declare trace function failed: {err}")))?;

    {
        let mut fb_ctx = FunctionBuilderContext::new();
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fb_ctx);

        let entry_block = b.create_block();
        let exit_block = b.create_block();
        b.append_block_param(exit_block, types::I32);

        b.switch_to_block(entry_block);
        b.append_block_params_for_function_params(entry_block);
        let vm_ptr = b.block_params(entry_block)[0];

        let helper_ref = module.declare_func_in_func(helper_id, b.func);

        for step in &trace.steps {
            if emit_inline_or_helper_step(
                &mut b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                trace.root_ip,
                step,
            )? {
                continue;
            }
            emit_helper_step(&mut b, vm_ptr, helper_ref, exit_block, trace.root_ip, step)?;
        }

        let continue_status = b.ins().iconst(types::I32, STATUS_CONTINUE as i64);
        jump_with_status(&mut b, exit_block, continue_status);

        b.switch_to_block(exit_block);
        let final_status = b.block_params(exit_block)[0];
        b.ins().return_(&[final_status]);

        b.seal_all_blocks();
        b.finalize();
    }

    module
        .define_function(func_id, &mut ctx)
        .map_err(|err| VmError::JitNative(format!("define cranelift trace failed: {err}")))?;
    module.clear_context(&mut ctx);
    module
        .finalize_definitions()
        .map_err(|err| VmError::JitNative(format!("finalize cranelift trace failed: {err}")))?;

    let entry = module.get_finalized_function(func_id);
    let mut code = Vec::with_capacity(8);
    code.extend_from_slice(&(entry as usize as u64).to_le_bytes());

    Ok(CraneliftCompiledTrace {
        entry,
        keepalive: CraneliftTraceKeepAlive { _module: module },
        code,
    })
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_or_helper_step(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    step: &TraceStep,
) -> VmResult<bool> {
    match step {
        TraceStep::Nop => Ok(true),
        TraceStep::Ret => {
            let halted = b.ins().iconst(types::I32, STATUS_HALTED as i64);
            jump_with_status(b, exit_block, halted);
            let dead = b.create_block();
            b.switch_to_block(dead);
            Ok(true)
        }
        TraceStep::JumpToIp { target_ip } => {
            let ip = i64::try_from(*target_ip)
                .map_err(|_| VmError::JitNative("jump target out of range for i64".to_string()))?;
            let ip_val = b.ins().iconst(pointer_type, ip);
            b.ins()
                .store(MemFlags::new(), ip_val, vm_ptr, offsets.vm_ip);
            let status = b.ins().iconst(types::I32, STATUS_TRACE_EXIT as i64);
            jump_with_status(b, exit_block, status);
            let dead = b.create_block();
            b.switch_to_block(dead);
            Ok(true)
        }
        TraceStep::JumpToRoot => {
            let ip = i64::try_from(root_ip)
                .map_err(|_| VmError::JitNative("root ip out of range for i64".to_string()))?;
            let ip_val = b.ins().iconst(pointer_type, ip);
            b.ins()
                .store(MemFlags::new(), ip_val, vm_ptr, offsets.vm_ip);
            let status = b.ins().iconst(types::I32, STATUS_TRACE_EXIT as i64);
            jump_with_status(b, exit_block, status);
            let dead = b.create_block();
            b.switch_to_block(dead);
            Ok(true)
        }
        TraceStep::Ldc(index) => {
            emit_inline_ldc(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *index,
                root_ip,
            )?;
            Ok(true)
        }
        TraceStep::Ldloc(index) => {
            emit_inline_ldloc(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *index,
                root_ip,
            )?;
            Ok(true)
        }
        TraceStep::Stloc(index) => {
            emit_inline_stloc(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *index,
                root_ip,
            )?;
            Ok(true)
        }
        TraceStep::Pop => {
            emit_inline_pop(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
            )?;
            Ok(true)
        }
        TraceStep::Dup => {
            emit_inline_dup(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
            )?;
            Ok(true)
        }
        TraceStep::Add => {
            emit_inline_int_binop(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                IntBinopKind::Add,
            )?;
            Ok(true)
        }
        TraceStep::Sub => {
            emit_inline_int_binop(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                IntBinopKind::Sub,
            )?;
            Ok(true)
        }
        TraceStep::Mul => {
            emit_inline_int_binop(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                IntBinopKind::Mul,
            )?;
            Ok(true)
        }
        TraceStep::Div => {
            emit_inline_int_divrem(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                false,
            )?;
            Ok(true)
        }
        TraceStep::Mod => {
            emit_inline_int_divrem(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                true,
            )?;
            Ok(true)
        }
        TraceStep::Shl => {
            emit_inline_shift(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                true,
            )?;
            Ok(true)
        }
        TraceStep::Shr => {
            emit_inline_shift(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                false,
            )?;
            Ok(true)
        }
        TraceStep::And => {
            emit_inline_bool_logic(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                true,
            )?;
            Ok(true)
        }
        TraceStep::Or => {
            emit_inline_bool_logic(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                false,
            )?;
            Ok(true)
        }
        TraceStep::Neg => {
            emit_inline_neg(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
            )?;
            Ok(true)
        }
        TraceStep::Clt => {
            emit_inline_int_compare(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                true,
            )?;
            Ok(true)
        }
        TraceStep::Cgt => {
            emit_inline_int_compare(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                false,
            )?;
            Ok(true)
        }
        TraceStep::Ceq => {
            emit_inline_int_eq(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
            )?;
            Ok(true)
        }
        TraceStep::GuardFalse { exit_ip } => {
            emit_inline_guard_false(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                *exit_ip,
            )?;
            Ok(true)
        }
        TraceStep::Call { .. } => Ok(false),
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_ldc(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    index: u32,
    root_ip: usize,
) -> VmResult<()> {
    let slow = b.create_block();
    let fast = b.create_block();
    let next = b.create_block();

    let constants_len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.constants_len);
    let idx = b.ins().iconst(pointer_type, i64::from(index));
    let in_bounds = b.ins().icmp(IntCC::UnsignedLessThan, idx, constants_len);
    b.ins().brif(in_bounds, fast, &[], slow, &[]);

    b.switch_to_block(fast);
    let stack_len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_len);
    let stack_cap = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_cap);
    let has_capacity = b.ins().icmp(IntCC::UnsignedLessThan, stack_len, stack_cap);
    let cap_ok = b.create_block();
    b.ins().brif(has_capacity, cap_ok, &[], slow, &[]);

    b.switch_to_block(cap_ok);
    let constants_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.constants_ptr);
    let src_addr = value_addr(b, pointer_type, constants_ptr, idx, layout.value.size);
    let src_tag = load_tag_i32(b, layout.value, src_addr);
    let scalar = is_scalar_tag(b, layout.value, src_tag);
    let scalar_ok = b.create_block();
    b.ins().brif(scalar, scalar_ok, &[], slow, &[]);

    b.switch_to_block(scalar_ok);
    let stack_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_ptr);
    let dst_addr = value_addr(b, pointer_type, stack_ptr, stack_len, layout.value.size);
    copy_value_bytes(b, src_addr, dst_addr, layout.value.size);
    let one = b.ins().iconst(pointer_type, 1);
    let new_len = b.ins().iadd(stack_len, one);
    b.ins()
        .store(MemFlags::new(), new_len, vm_ptr, offsets.stack_len);
    b.ins().jump(next, &[]);

    b.switch_to_block(slow);
    emit_helper_step_from_call_tuple(
        b,
        vm_ptr,
        helper_ref,
        exit_block,
        next,
        (OP_LDC, i64::from(index), 0, 0),
    );

    b.switch_to_block(next);
    let _ = root_ip;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_ldloc(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    index: u8,
    root_ip: usize,
) -> VmResult<()> {
    let slow = b.create_block();
    let fast = b.create_block();
    let next = b.create_block();

    let locals_len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.locals_len);
    let idx = b.ins().iconst(pointer_type, i64::from(index));
    let in_bounds = b.ins().icmp(IntCC::UnsignedLessThan, idx, locals_len);
    b.ins().brif(in_bounds, fast, &[], slow, &[]);

    b.switch_to_block(fast);
    let stack_len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_len);
    let stack_cap = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_cap);
    let has_capacity = b.ins().icmp(IntCC::UnsignedLessThan, stack_len, stack_cap);
    let cap_ok = b.create_block();
    b.ins().brif(has_capacity, cap_ok, &[], slow, &[]);

    b.switch_to_block(cap_ok);
    let locals_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.locals_ptr);
    let src_addr = value_addr(b, pointer_type, locals_ptr, idx, layout.value.size);
    let src_tag = load_tag_i32(b, layout.value, src_addr);
    let scalar = is_scalar_tag(b, layout.value, src_tag);
    let scalar_ok = b.create_block();
    b.ins().brif(scalar, scalar_ok, &[], slow, &[]);

    b.switch_to_block(scalar_ok);
    let stack_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_ptr);
    let dst_addr = value_addr(b, pointer_type, stack_ptr, stack_len, layout.value.size);
    copy_value_bytes(b, src_addr, dst_addr, layout.value.size);
    let one = b.ins().iconst(pointer_type, 1);
    let new_len = b.ins().iadd(stack_len, one);
    b.ins()
        .store(MemFlags::new(), new_len, vm_ptr, offsets.stack_len);
    b.ins().jump(next, &[]);

    b.switch_to_block(slow);
    emit_helper_step_from_call_tuple(
        b,
        vm_ptr,
        helper_ref,
        exit_block,
        next,
        (OP_LDLOC, i64::from(index), 0, 0),
    );

    b.switch_to_block(next);
    let _ = root_ip;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_stloc(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    index: u8,
    root_ip: usize,
) -> VmResult<()> {
    let slow = b.create_block();
    let fast = b.create_block();
    let next = b.create_block();

    let stack_len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_len);
    let has_stack = b
        .ins()
        .icmp_imm(IntCC::UnsignedGreaterThanOrEqual, stack_len, 1);
    b.ins().brif(has_stack, fast, &[], slow, &[]);

    b.switch_to_block(fast);
    let locals_len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.locals_len);
    let idx = b.ins().iconst(pointer_type, i64::from(index));
    let in_bounds = b.ins().icmp(IntCC::UnsignedLessThan, idx, locals_len);
    let bounds_ok = b.create_block();
    b.ins().brif(in_bounds, bounds_ok, &[], slow, &[]);

    b.switch_to_block(bounds_ok);
    let one = b.ins().iconst(pointer_type, 1);
    let src_index = b.ins().isub(stack_len, one);
    let stack_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_ptr);
    let src_addr = value_addr(b, pointer_type, stack_ptr, src_index, layout.value.size);
    let src_tag = load_tag_i32(b, layout.value, src_addr);

    let locals_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.locals_ptr);
    let dst_addr = value_addr(b, pointer_type, locals_ptr, idx, layout.value.size);
    let dst_tag = load_tag_i32(b, layout.value, dst_addr);

    let src_scalar = is_scalar_tag(b, layout.value, src_tag);
    let dst_scalar = is_scalar_tag(b, layout.value, dst_tag);
    let both_scalar = b.ins().band(src_scalar, dst_scalar);
    let scalar_ok = b.create_block();
    b.ins().brif(both_scalar, scalar_ok, &[], slow, &[]);

    b.switch_to_block(scalar_ok);
    copy_value_bytes(b, src_addr, dst_addr, layout.value.size);
    let new_len = b.ins().isub(stack_len, one);
    b.ins()
        .store(MemFlags::new(), new_len, vm_ptr, offsets.stack_len);
    b.ins().jump(next, &[]);

    b.switch_to_block(slow);
    emit_helper_step_from_call_tuple(
        b,
        vm_ptr,
        helper_ref,
        exit_block,
        next,
        (OP_STLOC, i64::from(index), 0, 0),
    );

    b.switch_to_block(next);
    let _ = root_ip;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_pop(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
) -> VmResult<()> {
    let slow = b.create_block();
    let fast = b.create_block();
    let next = b.create_block();

    let len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_len);
    let has_stack = b.ins().icmp_imm(IntCC::UnsignedGreaterThanOrEqual, len, 1);
    b.ins().brif(has_stack, fast, &[], slow, &[]);

    b.switch_to_block(fast);
    let one = b.ins().iconst(pointer_type, 1);
    let top_index = b.ins().isub(len, one);
    let stack_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_ptr);
    let top_addr = value_addr(b, pointer_type, stack_ptr, top_index, layout.value.size);
    let top_tag = load_tag_i32(b, layout.value, top_addr);
    let scalar = is_scalar_tag(b, layout.value, top_tag);
    let scalar_ok = b.create_block();
    b.ins().brif(scalar, scalar_ok, &[], slow, &[]);

    b.switch_to_block(scalar_ok);
    let new_len = b.ins().isub(len, one);
    b.ins()
        .store(MemFlags::new(), new_len, vm_ptr, offsets.stack_len);
    b.ins().jump(next, &[]);

    b.switch_to_block(slow);
    emit_helper_step_from_call_tuple(b, vm_ptr, helper_ref, exit_block, next, (OP_POP, 0, 0, 0));

    b.switch_to_block(next);
    let _ = root_ip;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_dup(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
) -> VmResult<()> {
    let slow = b.create_block();
    let fast = b.create_block();
    let next = b.create_block();

    let len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_len);
    let has_stack = b.ins().icmp_imm(IntCC::UnsignedGreaterThanOrEqual, len, 1);
    b.ins().brif(has_stack, fast, &[], slow, &[]);

    b.switch_to_block(fast);
    let cap = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_cap);
    let has_capacity = b.ins().icmp(IntCC::UnsignedLessThan, len, cap);
    let cap_ok = b.create_block();
    b.ins().brif(has_capacity, cap_ok, &[], slow, &[]);

    b.switch_to_block(cap_ok);
    let one = b.ins().iconst(pointer_type, 1);
    let src_index = b.ins().isub(len, one);
    let stack_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_ptr);
    let src_addr = value_addr(b, pointer_type, stack_ptr, src_index, layout.value.size);
    let src_tag = load_tag_i32(b, layout.value, src_addr);
    let scalar = is_scalar_tag(b, layout.value, src_tag);
    let scalar_ok = b.create_block();
    b.ins().brif(scalar, scalar_ok, &[], slow, &[]);

    b.switch_to_block(scalar_ok);
    let dst_addr = value_addr(b, pointer_type, stack_ptr, len, layout.value.size);
    copy_value_bytes(b, src_addr, dst_addr, layout.value.size);
    let new_len = b.ins().iadd(len, one);
    b.ins()
        .store(MemFlags::new(), new_len, vm_ptr, offsets.stack_len);
    b.ins().jump(next, &[]);

    b.switch_to_block(slow);
    emit_helper_step_from_call_tuple(b, vm_ptr, helper_ref, exit_block, next, (OP_DUP, 0, 0, 0));

    b.switch_to_block(next);
    let _ = root_ip;
    Ok(())
}

enum IntBinopKind {
    Add,
    Sub,
    Mul,
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_int_binop(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    kind: IntBinopKind,
) -> VmResult<()> {
    let slow = b.create_block();
    let len_ok = b.create_block();
    let fast = b.create_block();
    let next = b.create_block();

    let len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_len);
    let enough = b.ins().icmp_imm(IntCC::UnsignedGreaterThanOrEqual, len, 2);
    b.ins().brif(enough, len_ok, &[], slow, &[]);

    b.switch_to_block(len_ok);
    let one = b.ins().iconst(pointer_type, 1);
    let two = b.ins().iconst(pointer_type, 2);
    let rhs_index = b.ins().isub(len, one);
    let lhs_index = b.ins().isub(len, two);
    let stack_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_ptr);
    let lhs_addr = value_addr(b, pointer_type, stack_ptr, lhs_index, layout.value.size);
    let rhs_addr = value_addr(b, pointer_type, stack_ptr, rhs_index, layout.value.size);
    let lhs_tag = load_tag_i32(b, layout.value, lhs_addr);
    let rhs_tag = load_tag_i32(b, layout.value, rhs_addr);
    let lhs_int = b
        .ins()
        .icmp_imm(IntCC::Equal, lhs_tag, i64::from(layout.value.int_tag));
    let rhs_int = b
        .ins()
        .icmp_imm(IntCC::Equal, rhs_tag, i64::from(layout.value.int_tag));
    let both_int = b.ins().band(lhs_int, rhs_int);
    b.ins().brif(both_int, fast, &[], slow, &[]);

    b.switch_to_block(fast);
    let lhs = b.ins().load(
        types::I64,
        MemFlags::new(),
        lhs_addr,
        layout.value.int_payload_offset,
    );
    let rhs = b.ins().load(
        types::I64,
        MemFlags::new(),
        rhs_addr,
        layout.value.int_payload_offset,
    );
    let out = match kind {
        IntBinopKind::Add => b.ins().iadd(lhs, rhs),
        IntBinopKind::Sub => b.ins().isub(lhs, rhs),
        IntBinopKind::Mul => b.ins().imul(lhs, rhs),
    };
    b.ins().store(
        MemFlags::new(),
        out,
        lhs_addr,
        layout.value.int_payload_offset,
    );
    let new_len = b.ins().isub(len, one);
    b.ins()
        .store(MemFlags::new(), new_len, vm_ptr, offsets.stack_len);
    b.ins().jump(next, &[]);

    b.switch_to_block(slow);
    let op = match kind {
        IntBinopKind::Add => OP_ADD,
        IntBinopKind::Sub => OP_SUB,
        IntBinopKind::Mul => OP_MUL,
    };
    emit_helper_step_from_call_tuple(b, vm_ptr, helper_ref, exit_block, next, (op, 0, 0, 0));

    b.switch_to_block(next);
    let _ = root_ip;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_int_divrem(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    is_mod: bool,
) -> VmResult<()> {
    let slow = b.create_block();
    let len_ok = b.create_block();
    let type_ok = b.create_block();
    let non_zero = b.create_block();
    let next = b.create_block();

    let len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_len);
    let enough = b.ins().icmp_imm(IntCC::UnsignedGreaterThanOrEqual, len, 2);
    b.ins().brif(enough, len_ok, &[], slow, &[]);

    b.switch_to_block(len_ok);
    let one = b.ins().iconst(pointer_type, 1);
    let two = b.ins().iconst(pointer_type, 2);
    let rhs_index = b.ins().isub(len, one);
    let lhs_index = b.ins().isub(len, two);
    let stack_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_ptr);
    let lhs_addr = value_addr(b, pointer_type, stack_ptr, lhs_index, layout.value.size);
    let rhs_addr = value_addr(b, pointer_type, stack_ptr, rhs_index, layout.value.size);
    let lhs_tag = load_tag_i32(b, layout.value, lhs_addr);
    let rhs_tag = load_tag_i32(b, layout.value, rhs_addr);
    let lhs_int = b
        .ins()
        .icmp_imm(IntCC::Equal, lhs_tag, i64::from(layout.value.int_tag));
    let rhs_int = b
        .ins()
        .icmp_imm(IntCC::Equal, rhs_tag, i64::from(layout.value.int_tag));
    let both_int = b.ins().band(lhs_int, rhs_int);
    b.ins().brif(both_int, type_ok, &[], slow, &[]);

    b.switch_to_block(type_ok);
    let lhs = b.ins().load(
        types::I64,
        MemFlags::new(),
        lhs_addr,
        layout.value.int_payload_offset,
    );
    let rhs = b.ins().load(
        types::I64,
        MemFlags::new(),
        rhs_addr,
        layout.value.int_payload_offset,
    );
    let rhs_not_zero = b.ins().icmp_imm(IntCC::NotEqual, rhs, 0);
    b.ins().brif(rhs_not_zero, non_zero, &[], slow, &[]);

    b.switch_to_block(non_zero);
    let min_i64 = b.ins().iconst(types::I64, i64::MIN);
    let neg_one = b.ins().iconst(types::I64, -1);
    let lhs_is_min = b.ins().icmp(IntCC::Equal, lhs, min_i64);
    let rhs_is_neg_one = b.ins().icmp(IntCC::Equal, rhs, neg_one);
    let overflow_case = b.ins().band(lhs_is_min, rhs_is_neg_one);

    let overflow_block = b.create_block();
    let normal_block = b.create_block();
    b.ins()
        .brif(overflow_case, overflow_block, &[], normal_block, &[]);

    b.switch_to_block(overflow_block);
    let overflow_out = if is_mod {
        b.ins().iconst(types::I64, 0)
    } else {
        min_i64
    };
    b.ins().store(
        MemFlags::new(),
        overflow_out,
        lhs_addr,
        layout.value.int_payload_offset,
    );
    let new_len_overflow = b.ins().isub(len, one);
    b.ins()
        .store(MemFlags::new(), new_len_overflow, vm_ptr, offsets.stack_len);
    b.ins().jump(next, &[]);

    b.switch_to_block(normal_block);
    let out = if is_mod {
        b.ins().srem(lhs, rhs)
    } else {
        b.ins().sdiv(lhs, rhs)
    };
    b.ins().store(
        MemFlags::new(),
        out,
        lhs_addr,
        layout.value.int_payload_offset,
    );
    let new_len = b.ins().isub(len, one);
    b.ins()
        .store(MemFlags::new(), new_len, vm_ptr, offsets.stack_len);
    b.ins().jump(next, &[]);

    b.switch_to_block(slow);
    emit_helper_step_from_call_tuple(
        b,
        vm_ptr,
        helper_ref,
        exit_block,
        next,
        (if is_mod { OP_MOD } else { OP_DIV }, 0, 0, 0),
    );

    b.switch_to_block(next);
    let _ = root_ip;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_shift(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    is_shl: bool,
) -> VmResult<()> {
    let slow = b.create_block();
    let len_ok = b.create_block();
    let type_ok = b.create_block();
    let shift_ok = b.create_block();
    let next = b.create_block();

    let len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_len);
    let enough = b.ins().icmp_imm(IntCC::UnsignedGreaterThanOrEqual, len, 2);
    b.ins().brif(enough, len_ok, &[], slow, &[]);

    b.switch_to_block(len_ok);
    let one = b.ins().iconst(pointer_type, 1);
    let two = b.ins().iconst(pointer_type, 2);
    let rhs_index = b.ins().isub(len, one);
    let lhs_index = b.ins().isub(len, two);
    let stack_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_ptr);
    let lhs_addr = value_addr(b, pointer_type, stack_ptr, lhs_index, layout.value.size);
    let rhs_addr = value_addr(b, pointer_type, stack_ptr, rhs_index, layout.value.size);
    let lhs_tag = load_tag_i32(b, layout.value, lhs_addr);
    let rhs_tag = load_tag_i32(b, layout.value, rhs_addr);
    let lhs_int = b
        .ins()
        .icmp_imm(IntCC::Equal, lhs_tag, i64::from(layout.value.int_tag));
    let rhs_int = b
        .ins()
        .icmp_imm(IntCC::Equal, rhs_tag, i64::from(layout.value.int_tag));
    let both_int = b.ins().band(lhs_int, rhs_int);
    b.ins().brif(both_int, type_ok, &[], slow, &[]);

    b.switch_to_block(type_ok);
    let lhs = b.ins().load(
        types::I64,
        MemFlags::new(),
        lhs_addr,
        layout.value.int_payload_offset,
    );
    let rhs = b.ins().load(
        types::I64,
        MemFlags::new(),
        rhs_addr,
        layout.value.int_payload_offset,
    );
    let shift_ge_zero = b.ins().icmp_imm(IntCC::SignedGreaterThanOrEqual, rhs, 0);
    let shift_le_63 = b.ins().icmp_imm(IntCC::SignedLessThanOrEqual, rhs, 63);
    let shift_in_range = b.ins().band(shift_ge_zero, shift_le_63);
    b.ins().brif(shift_in_range, shift_ok, &[], slow, &[]);

    b.switch_to_block(shift_ok);
    let out = if is_shl {
        b.ins().ishl(lhs, rhs)
    } else {
        b.ins().sshr(lhs, rhs)
    };
    b.ins().store(
        MemFlags::new(),
        out,
        lhs_addr,
        layout.value.int_payload_offset,
    );
    let new_len = b.ins().isub(len, one);
    b.ins()
        .store(MemFlags::new(), new_len, vm_ptr, offsets.stack_len);
    b.ins().jump(next, &[]);

    b.switch_to_block(slow);
    emit_helper_step_from_call_tuple(
        b,
        vm_ptr,
        helper_ref,
        exit_block,
        next,
        (if is_shl { OP_SHL } else { OP_SHR }, 0, 0, 0),
    );

    b.switch_to_block(next);
    let _ = root_ip;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_bool_logic(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    is_and: bool,
) -> VmResult<()> {
    let slow = b.create_block();
    let len_ok = b.create_block();
    let type_ok = b.create_block();
    let next = b.create_block();

    let len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_len);
    let enough = b.ins().icmp_imm(IntCC::UnsignedGreaterThanOrEqual, len, 2);
    b.ins().brif(enough, len_ok, &[], slow, &[]);

    b.switch_to_block(len_ok);
    let one = b.ins().iconst(pointer_type, 1);
    let two = b.ins().iconst(pointer_type, 2);
    let rhs_index = b.ins().isub(len, one);
    let lhs_index = b.ins().isub(len, two);
    let stack_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_ptr);
    let lhs_addr = value_addr(b, pointer_type, stack_ptr, lhs_index, layout.value.size);
    let rhs_addr = value_addr(b, pointer_type, stack_ptr, rhs_index, layout.value.size);
    let lhs_tag = load_tag_i32(b, layout.value, lhs_addr);
    let rhs_tag = load_tag_i32(b, layout.value, rhs_addr);
    let lhs_bool = b
        .ins()
        .icmp_imm(IntCC::Equal, lhs_tag, i64::from(layout.value.bool_tag));
    let rhs_bool = b
        .ins()
        .icmp_imm(IntCC::Equal, rhs_tag, i64::from(layout.value.bool_tag));
    let both_bool = b.ins().band(lhs_bool, rhs_bool);
    b.ins().brif(both_bool, type_ok, &[], slow, &[]);

    b.switch_to_block(type_ok);
    let lhs = b.ins().load(
        types::I8,
        MemFlags::new(),
        lhs_addr,
        layout.value.bool_payload_offset,
    );
    let rhs = b.ins().load(
        types::I8,
        MemFlags::new(),
        rhs_addr,
        layout.value.bool_payload_offset,
    );
    let lhs_non_zero = b.ins().icmp_imm(IntCC::NotEqual, lhs, 0);
    let rhs_non_zero = b.ins().icmp_imm(IntCC::NotEqual, rhs, 0);
    let out_bool = if is_and {
        b.ins().band(lhs_non_zero, rhs_non_zero)
    } else {
        b.ins().bor(lhs_non_zero, rhs_non_zero)
    };
    store_bool_in_value(b, layout.value, lhs_addr, out_bool);
    let new_len = b.ins().isub(len, one);
    b.ins()
        .store(MemFlags::new(), new_len, vm_ptr, offsets.stack_len);
    b.ins().jump(next, &[]);

    b.switch_to_block(slow);
    emit_helper_step_from_call_tuple(
        b,
        vm_ptr,
        helper_ref,
        exit_block,
        next,
        (if is_and { OP_AND } else { OP_OR }, 0, 0, 0),
    );

    b.switch_to_block(next);
    let _ = root_ip;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_neg(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
) -> VmResult<()> {
    let slow = b.create_block();
    let fast = b.create_block();
    let next = b.create_block();

    let len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_len);
    let has_stack = b.ins().icmp_imm(IntCC::UnsignedGreaterThanOrEqual, len, 1);
    b.ins().brif(has_stack, fast, &[], slow, &[]);

    b.switch_to_block(fast);
    let one = b.ins().iconst(pointer_type, 1);
    let idx = b.ins().isub(len, one);
    let stack_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_ptr);
    let addr = value_addr(b, pointer_type, stack_ptr, idx, layout.value.size);
    let tag = load_tag_i32(b, layout.value, addr);
    let is_int = b
        .ins()
        .icmp_imm(IntCC::Equal, tag, i64::from(layout.value.int_tag));
    let int_ok = b.create_block();
    b.ins().brif(is_int, int_ok, &[], slow, &[]);

    b.switch_to_block(int_ok);
    let value = b.ins().load(
        types::I64,
        MemFlags::new(),
        addr,
        layout.value.int_payload_offset,
    );
    let neg = b.ins().irsub_imm(value, 0);
    b.ins()
        .store(MemFlags::new(), neg, addr, layout.value.int_payload_offset);
    b.ins().jump(next, &[]);

    b.switch_to_block(slow);
    emit_helper_step_from_call_tuple(b, vm_ptr, helper_ref, exit_block, next, (OP_NEG, 0, 0, 0));

    b.switch_to_block(next);
    let _ = root_ip;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_int_compare(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    less_than: bool,
) -> VmResult<()> {
    let slow = b.create_block();
    let len_ok = b.create_block();
    let fast = b.create_block();
    let next = b.create_block();

    let len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_len);
    let enough = b.ins().icmp_imm(IntCC::UnsignedGreaterThanOrEqual, len, 2);
    b.ins().brif(enough, len_ok, &[], slow, &[]);

    b.switch_to_block(len_ok);
    let one = b.ins().iconst(pointer_type, 1);
    let two = b.ins().iconst(pointer_type, 2);
    let rhs_index = b.ins().isub(len, one);
    let lhs_index = b.ins().isub(len, two);
    let stack_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_ptr);
    let lhs_addr = value_addr(b, pointer_type, stack_ptr, lhs_index, layout.value.size);
    let rhs_addr = value_addr(b, pointer_type, stack_ptr, rhs_index, layout.value.size);
    let lhs_tag = load_tag_i32(b, layout.value, lhs_addr);
    let rhs_tag = load_tag_i32(b, layout.value, rhs_addr);
    let lhs_int = b
        .ins()
        .icmp_imm(IntCC::Equal, lhs_tag, i64::from(layout.value.int_tag));
    let rhs_int = b
        .ins()
        .icmp_imm(IntCC::Equal, rhs_tag, i64::from(layout.value.int_tag));
    let both_int = b.ins().band(lhs_int, rhs_int);
    b.ins().brif(both_int, fast, &[], slow, &[]);

    b.switch_to_block(fast);
    let lhs = b.ins().load(
        types::I64,
        MemFlags::new(),
        lhs_addr,
        layout.value.int_payload_offset,
    );
    let rhs = b.ins().load(
        types::I64,
        MemFlags::new(),
        rhs_addr,
        layout.value.int_payload_offset,
    );
    let cmp = if less_than {
        b.ins().icmp(IntCC::SignedLessThan, lhs, rhs)
    } else {
        b.ins().icmp(IntCC::SignedGreaterThan, lhs, rhs)
    };
    store_bool_in_value(b, layout.value, lhs_addr, cmp);
    let new_len = b.ins().isub(len, one);
    b.ins()
        .store(MemFlags::new(), new_len, vm_ptr, offsets.stack_len);
    b.ins().jump(next, &[]);

    b.switch_to_block(slow);
    emit_helper_step_from_call_tuple(
        b,
        vm_ptr,
        helper_ref,
        exit_block,
        next,
        (if less_than { OP_CLT } else { OP_CGT }, 0, 0, 0),
    );

    b.switch_to_block(next);
    let _ = root_ip;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_int_eq(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
) -> VmResult<()> {
    let slow = b.create_block();
    let len_ok = b.create_block();
    let fast = b.create_block();
    let next = b.create_block();

    let len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_len);
    let enough = b.ins().icmp_imm(IntCC::UnsignedGreaterThanOrEqual, len, 2);
    b.ins().brif(enough, len_ok, &[], slow, &[]);

    b.switch_to_block(len_ok);
    let one = b.ins().iconst(pointer_type, 1);
    let two = b.ins().iconst(pointer_type, 2);
    let rhs_index = b.ins().isub(len, one);
    let lhs_index = b.ins().isub(len, two);
    let stack_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_ptr);
    let lhs_addr = value_addr(b, pointer_type, stack_ptr, lhs_index, layout.value.size);
    let rhs_addr = value_addr(b, pointer_type, stack_ptr, rhs_index, layout.value.size);
    let lhs_tag = load_tag_i32(b, layout.value, lhs_addr);
    let rhs_tag = load_tag_i32(b, layout.value, rhs_addr);
    let lhs_int = b
        .ins()
        .icmp_imm(IntCC::Equal, lhs_tag, i64::from(layout.value.int_tag));
    let rhs_int = b
        .ins()
        .icmp_imm(IntCC::Equal, rhs_tag, i64::from(layout.value.int_tag));
    let both_int = b.ins().band(lhs_int, rhs_int);
    b.ins().brif(both_int, fast, &[], slow, &[]);

    b.switch_to_block(fast);
    let lhs = b.ins().load(
        types::I64,
        MemFlags::new(),
        lhs_addr,
        layout.value.int_payload_offset,
    );
    let rhs = b.ins().load(
        types::I64,
        MemFlags::new(),
        rhs_addr,
        layout.value.int_payload_offset,
    );
    let cmp = b.ins().icmp(IntCC::Equal, lhs, rhs);
    store_bool_in_value(b, layout.value, lhs_addr, cmp);
    let new_len = b.ins().isub(len, one);
    b.ins()
        .store(MemFlags::new(), new_len, vm_ptr, offsets.stack_len);
    b.ins().jump(next, &[]);

    b.switch_to_block(slow);
    emit_helper_step_from_call_tuple(b, vm_ptr, helper_ref, exit_block, next, (OP_CEQ, 0, 0, 0));

    b.switch_to_block(next);
    let _ = root_ip;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_guard_false(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    exit_ip: usize,
) -> VmResult<()> {
    let slow = b.create_block();
    let len_ok = b.create_block();
    let bool_ok = b.create_block();
    let branch_false = b.create_block();
    let next = b.create_block();

    let len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_len);
    let has_stack = b.ins().icmp_imm(IntCC::UnsignedGreaterThanOrEqual, len, 1);
    b.ins().brif(has_stack, len_ok, &[], slow, &[]);

    b.switch_to_block(len_ok);
    let one = b.ins().iconst(pointer_type, 1);
    let idx = b.ins().isub(len, one);
    let stack_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_ptr);
    let top_addr = value_addr(b, pointer_type, stack_ptr, idx, layout.value.size);
    let top_tag = load_tag_i32(b, layout.value, top_addr);
    let is_bool = b
        .ins()
        .icmp_imm(IntCC::Equal, top_tag, i64::from(layout.value.bool_tag));
    b.ins().brif(is_bool, bool_ok, &[], slow, &[]);

    b.switch_to_block(bool_ok);
    let new_len = b.ins().isub(len, one);
    b.ins()
        .store(MemFlags::new(), new_len, vm_ptr, offsets.stack_len);
    let cond = b.ins().load(
        types::I8,
        MemFlags::new(),
        top_addr,
        layout.value.bool_payload_offset,
    );
    let cond_true = b.ins().icmp_imm(IntCC::NotEqual, cond, 0);
    b.ins().brif(cond_true, next, &[], branch_false, &[]);

    b.switch_to_block(branch_false);
    let exit_ip = i64::try_from(exit_ip)
        .map_err(|_| VmError::JitNative("guard exit ip out of range".to_string()))?;
    let exit_ip_val = b.ins().iconst(pointer_type, exit_ip);
    b.ins()
        .store(MemFlags::new(), exit_ip_val, vm_ptr, offsets.vm_ip);
    let status = b.ins().iconst(types::I32, STATUS_TRACE_EXIT as i64);
    jump_with_status(b, exit_block, status);

    b.switch_to_block(slow);
    emit_helper_step_from_call_tuple(
        b,
        vm_ptr,
        helper_ref,
        exit_block,
        next,
        (OP_GUARD_FALSE, exit_ip, 0, 0),
    );

    b.switch_to_block(next);
    let _ = root_ip;
    Ok(())
}

fn emit_helper_step(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    exit_block: Block,
    root_ip: usize,
    step: &TraceStep,
) -> VmResult<()> {
    let tuple = step_to_call(step, root_ip)?;
    let next = b.create_block();
    emit_helper_step_from_call_tuple(b, vm_ptr, helper_ref, exit_block, next, tuple);
    b.switch_to_block(next);
    Ok(())
}

fn emit_helper_step_from_call_tuple(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    exit_block: Block,
    next_block: Block,
    tuple: (i64, i64, i64, i64),
) {
    let (op, a, b_arg, c) = tuple;
    let op_val = b.ins().iconst(types::I64, op);
    let a_val = b.ins().iconst(types::I64, a);
    let b_val = b.ins().iconst(types::I64, b_arg);
    let c_val = b.ins().iconst(types::I64, c);

    let call = b
        .ins()
        .call(helper_ref, &[vm_ptr, op_val, a_val, b_val, c_val]);
    let status = b.inst_results(call)[0];

    let is_continue = b
        .ins()
        .icmp_imm(IntCC::Equal, status, STATUS_CONTINUE as i64);
    let else_args = [BlockArg::Value(status)];
    b.ins()
        .brif(is_continue, next_block, &[], exit_block, &else_args);
}

fn jump_with_status(b: &mut FunctionBuilder, block: Block, status: cranelift_codegen::ir::Value) {
    let args = [BlockArg::Value(status)];
    b.ins().jump(block, &args);
}

fn value_addr(
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

fn tag_type(layout: ValueLayout) -> cranelift_codegen::ir::Type {
    match layout.tag_size {
        1 => types::I8,
        2 => types::I16,
        4 => types::I32,
        _ => types::I32,
    }
}

fn load_tag_i32(
    b: &mut FunctionBuilder,
    layout: ValueLayout,
    value_addr: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    let raw = b.ins().load(
        tag_type(layout),
        MemFlags::new(),
        value_addr,
        layout.tag_offset,
    );
    match layout.tag_size {
        1 | 2 => b.ins().uextend(types::I32, raw),
        _ => raw,
    }
}

fn store_tag(
    b: &mut FunctionBuilder,
    layout: ValueLayout,
    value_addr: cranelift_codegen::ir::Value,
    tag: u32,
) {
    let ty = tag_type(layout);
    let raw = b.ins().iconst(ty, i64::from(tag));
    b.ins()
        .store(MemFlags::new(), raw, value_addr, layout.tag_offset);
}

fn is_scalar_tag(
    b: &mut FunctionBuilder,
    layout: ValueLayout,
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

fn store_bool_in_value(
    b: &mut FunctionBuilder,
    layout: ValueLayout,
    value_addr: cranelift_codegen::ir::Value,
    bool_value: cranelift_codegen::ir::Value,
) {
    store_tag(b, layout, value_addr, layout.bool_tag);
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

fn copy_value_bytes(
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
    let stack_cap = checked_add_i32(
        layout.vm_stack_offset,
        layout.stack_vec.cap_offset,
        "stack cap offset overflow",
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

    let constants_vec_base = checked_add_i32(
        layout.vm_program_offset,
        layout.program_constants_offset,
        "constants vec base offset overflow",
    )?;
    let constants_ptr = checked_add_i32(
        constants_vec_base,
        layout.stack_vec.ptr_offset,
        "constants ptr offset overflow",
    )?;
    let constants_len = checked_add_i32(
        constants_vec_base,
        layout.stack_vec.len_offset,
        "constants len offset overflow",
    )?;

    Ok(ResolvedOffsets {
        stack_ptr,
        stack_len,
        stack_cap,
        locals_ptr,
        locals_len,
        constants_ptr,
        constants_len,
        vm_ip: layout.vm_ip_offset,
    })
}

fn helper_signature(
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

fn entry_signature(
    pointer_type: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    sig.returns.push(AbiParam::new(types::I32));
    sig
}

fn step_to_call(step: &TraceStep, root_ip: usize) -> VmResult<(i64, i64, i64, i64)> {
    Ok(match step {
        TraceStep::Nop => (0, 0, 0, 0),
        TraceStep::Ldc(index) => (OP_LDC, i64::from(*index), 0, 0),
        TraceStep::Add => (OP_ADD, 0, 0, 0),
        TraceStep::Sub => (OP_SUB, 0, 0, 0),
        TraceStep::Mul => (OP_MUL, 0, 0, 0),
        TraceStep::Div => (OP_DIV, 0, 0, 0),
        TraceStep::Mod => (OP_MOD, 0, 0, 0),
        TraceStep::Shl => (OP_SHL, 0, 0, 0),
        TraceStep::Shr => (OP_SHR, 0, 0, 0),
        TraceStep::And => (OP_AND, 0, 0, 0),
        TraceStep::Or => (OP_OR, 0, 0, 0),
        TraceStep::Neg => (OP_NEG, 0, 0, 0),
        TraceStep::Ceq => (OP_CEQ, 0, 0, 0),
        TraceStep::Clt => (OP_CLT, 0, 0, 0),
        TraceStep::Cgt => (OP_CGT, 0, 0, 0),
        TraceStep::Pop => (OP_POP, 0, 0, 0),
        TraceStep::Dup => (OP_DUP, 0, 0, 0),
        TraceStep::Ldloc(index) => (OP_LDLOC, i64::from(*index), 0, 0),
        TraceStep::Stloc(index) => (OP_STLOC, i64::from(*index), 0, 0),
        TraceStep::Call {
            index,
            argc,
            call_ip,
        } => {
            let call_ip = i64::try_from(*call_ip)
                .map_err(|_| VmError::JitNative("call ip out of range for i64".to_string()))?;
            (OP_CALL, i64::from(*index), i64::from(*argc), call_ip)
        }
        TraceStep::GuardFalse { exit_ip } => {
            let exit_ip = i64::try_from(*exit_ip).map_err(|_| {
                VmError::JitNative("guard exit ip out of range for i64".to_string())
            })?;
            (OP_GUARD_FALSE, exit_ip, 0, 0)
        }
        TraceStep::JumpToIp { target_ip } => {
            let target_ip = i64::try_from(*target_ip)
                .map_err(|_| VmError::JitNative("jump target out of range for i64".to_string()))?;
            (OP_JUMP, target_ip, 0, 0)
        }
        TraceStep::JumpToRoot => {
            let root = i64::try_from(root_ip)
                .map_err(|_| VmError::JitNative("root ip out of range for i64".to_string()))?;
            (OP_JUMP, root, 0, 0)
        }
        TraceStep::Ret => (0, 0, 0, 0),
    })
}

fn detect_native_stack_layout() -> VmResult<NativeStackLayout> {
    let cached = NATIVE_STACK_LAYOUT
        .get_or_init(|| detect_native_stack_layout_uncached().map_err(layout_probe_error_message));
    match cached {
        Ok(layout) => Ok(*layout),
        Err(message) => Err(VmError::JitNative(message.clone())),
    }
}

fn detect_native_stack_layout_uncached() -> VmResult<NativeStackLayout> {
    let vm_stack_offset = usize_to_i32(std::mem::offset_of!(Vm, stack), "Vm::stack offset")?;
    let vm_locals_offset = usize_to_i32(std::mem::offset_of!(Vm, locals), "Vm::locals offset")?;
    let vm_program_offset = usize_to_i32(std::mem::offset_of!(Vm, program), "Vm::program offset")?;
    let vm_ip_offset = usize_to_i32(std::mem::offset_of!(Vm, ip), "Vm::ip offset")?;
    let program_constants_offset = usize_to_i32(
        std::mem::offset_of!(Program, constants),
        "Program::constants offset",
    )?;
    let stack_vec = detect_vec_layout()?;
    let value = detect_value_layout()?;
    Ok(NativeStackLayout {
        vm_stack_offset,
        vm_locals_offset,
        vm_program_offset,
        vm_ip_offset,
        program_constants_offset,
        stack_vec,
        value,
    })
}

fn layout_probe_error_message(error: VmError) -> String {
    match error {
        VmError::JitNative(message) => message,
        other => other.to_string(),
    }
}

fn detect_vec_layout() -> VmResult<VecLayout> {
    let expected_size = std::mem::size_of::<[usize; 3]>();
    if std::mem::size_of::<Vec<Value>>() != expected_size {
        return Err(VmError::JitNative(format!(
            "unsupported Vec<Value> size {} for native emission",
            std::mem::size_of::<Vec<Value>>()
        )));
    }

    let mut sample = Vec::with_capacity(11);
    sample.push(Value::Int(1));
    sample.push(Value::Int(2));
    let ptr_value = sample.as_ptr() as usize;
    let len_value = sample.len();
    let cap_value = sample.capacity();

    let words = unsafe { &*((&sample as *const Vec<Value>) as *const [usize; 3]) };
    let ptr_index = find_unique_word_index(words, ptr_value, "Vec<Value> ptr field")?;
    let len_index = find_unique_word_index(words, len_value, "Vec<Value> len field")?;
    let cap_index = find_unique_word_index(words, cap_value, "Vec<Value> cap field")?;

    Ok(VecLayout {
        ptr_offset: usize_to_i32(
            ptr_index * std::mem::size_of::<usize>(),
            "Vec<Value>::ptr offset",
        )?,
        len_offset: usize_to_i32(
            len_index * std::mem::size_of::<usize>(),
            "Vec<Value>::len offset",
        )?,
        cap_offset: usize_to_i32(
            cap_index * std::mem::size_of::<usize>(),
            "Vec<Value>::cap offset",
        )?,
    })
}

fn find_unique_word_index(words: &[usize; 3], needle: usize, label: &str) -> VmResult<usize> {
    let mut match_index = None;
    for (index, value) in words.iter().enumerate() {
        if *value == needle {
            if match_index.is_some() {
                return Err(VmError::JitNative(format!(
                    "ambiguous {} while probing native layout",
                    label
                )));
            }
            match_index = Some(index);
        }
    }
    match_index.ok_or_else(|| {
        VmError::JitNative(format!(
            "failed to locate {} while probing native layout",
            label
        ))
    })
}

fn detect_value_layout() -> VmResult<ValueLayout> {
    let value_size = std::mem::size_of::<Value>();
    let int_a = 0x0102_0304_0506_0708_i64;
    let int_b = 0x1112_1314_1516_1718_i64;
    let float_a = 3.25_f64;
    let float_b = -11.5_f64;
    let null_a_bytes = encode_value_bytes(Value::Null);
    let null_b_bytes = encode_value_bytes(Value::Null);
    let int_a_bytes = encode_value_bytes(Value::Int(int_a));
    let int_b_bytes = encode_value_bytes(Value::Int(int_b));
    let float_a_bytes = encode_value_bytes(Value::Float(float_a));
    let float_b_bytes = encode_value_bytes(Value::Float(float_b));
    let bool_false_bytes = encode_value_bytes(Value::Bool(false));
    let bool_true_bytes = encode_value_bytes(Value::Bool(true));
    let string_a_bytes = encode_value_bytes(Value::String("a".to_string()));
    let string_b_bytes = encode_value_bytes(Value::String("b".to_string()));

    let stable_tag_pairs = [
        (&null_a_bytes[..], &null_b_bytes[..]),
        (&int_a_bytes[..], &int_b_bytes[..]),
        (&float_a_bytes[..], &float_b_bytes[..]),
        (&bool_false_bytes[..], &bool_true_bytes[..]),
        (&string_a_bytes[..], &string_b_bytes[..]),
    ];
    let (tag_offset, tag_size) = detect_tag_layout(&stable_tag_pairs)?;
    let null_tag = decode_tag(&null_a_bytes, tag_offset, tag_size);
    let int_tag = decode_tag(&int_a_bytes, tag_offset, tag_size);
    let float_tag = decode_tag(&float_a_bytes, tag_offset, tag_size);
    let bool_tag = decode_tag(&bool_false_bytes, tag_offset, tag_size);

    let payload_match_a = int_a.to_le_bytes();
    let payload_match_b = int_b.to_le_bytes();
    let mut int_payload_offset = None;
    for offset in 0..=value_size.saturating_sub(8) {
        if int_a_bytes[offset..offset + 8] == payload_match_a
            && int_b_bytes[offset..offset + 8] == payload_match_b
        {
            if int_payload_offset.is_some() {
                return Err(VmError::JitNative(
                    "ambiguous Value::Int payload offset for native emission".to_string(),
                ));
            }
            int_payload_offset = Some(offset);
        }
    }
    let int_payload_offset = int_payload_offset.ok_or_else(|| {
        VmError::JitNative(
            "unable to find Value::Int payload offset for native emission".to_string(),
        )
    })?;

    let mut bool_payload_offset = None;
    for offset in 0..value_size {
        if bool_false_bytes[offset] == bool_true_bytes[offset] {
            continue;
        }
        if offset >= tag_offset && offset < tag_offset + tag_size {
            continue;
        }
        bool_payload_offset = Some(offset);
        break;
    }
    let bool_payload_offset = bool_payload_offset.ok_or_else(|| {
        VmError::JitNative(
            "unable to find Value::Bool payload offset for native emission".to_string(),
        )
    })?;
    let false_byte = bool_false_bytes[bool_payload_offset];
    let true_byte = bool_true_bytes[bool_payload_offset];
    if false_byte != 0 || true_byte != 1 {
        return Err(VmError::JitNative(
            "unsupported Value::Bool byte encoding for native emission".to_string(),
        ));
    }

    Ok(ValueLayout {
        size: usize_to_i32(value_size, "Value size")?,
        tag_offset: usize_to_i32(tag_offset, "Value tag offset")?,
        tag_size: tag_size as u8,
        null_tag,
        int_tag,
        float_tag,
        bool_tag,
        int_payload_offset: usize_to_i32(int_payload_offset, "Value::Int payload offset")?,
        bool_payload_offset: usize_to_i32(bool_payload_offset, "Value::Bool payload offset")?,
    })
}

fn detect_tag_layout(stable_pairs: &[(&[u8], &[u8])]) -> VmResult<(usize, usize)> {
    if stable_pairs.len() < 2 {
        return Err(VmError::JitNative(
            "need at least two value variants to detect native tag layout".to_string(),
        ));
    }
    let size = stable_pairs[0].0.len();
    for (lhs, rhs) in stable_pairs {
        if lhs.len() != size || rhs.len() != size {
            return Err(VmError::JitNative(
                "value byte probes must all have matching lengths".to_string(),
            ));
        }
    }

    for tag_size in [1usize, 2, 4] {
        if tag_size > size {
            continue;
        }
        for offset in 0..=size - tag_size {
            let mut all_stable = true;
            let mut first_tag_slice: Option<&[u8]> = None;
            let mut all_equal_across_variants = true;
            for (lhs, rhs) in stable_pairs {
                let lhs_slice = &lhs[offset..offset + tag_size];
                let rhs_slice = &rhs[offset..offset + tag_size];
                if lhs_slice != rhs_slice {
                    all_stable = false;
                    break;
                }
                if let Some(first) = first_tag_slice {
                    if lhs_slice != first {
                        all_equal_across_variants = false;
                    }
                } else {
                    first_tag_slice = Some(lhs_slice);
                }
            }
            if !all_stable || all_equal_across_variants {
                continue;
            }
            return Ok((offset, tag_size));
        }
    }
    Err(VmError::JitNative(
        "unable to find Value discriminant bytes for native emission".to_string(),
    ))
}

fn decode_tag(bytes: &[u8], offset: usize, size: usize) -> u32 {
    let mut out = 0u32;
    for index in 0..size {
        out |= (bytes[offset + index] as u32) << (index * 8);
    }
    out
}

fn encode_value_bytes(value: Value) -> Vec<u8> {
    let size = std::mem::size_of::<Value>();
    let mut bytes = vec![0u8; size];
    let mut slot = std::mem::MaybeUninit::<Value>::zeroed();
    unsafe {
        slot.as_mut_ptr().write(value);
        std::ptr::copy_nonoverlapping(slot.as_ptr() as *const u8, bytes.as_mut_ptr(), size);
        std::ptr::drop_in_place(slot.as_mut_ptr());
    }
    bytes
}

fn checked_add_i32(lhs: i32, rhs: i32, context: &str) -> VmResult<i32> {
    lhs.checked_add(rhs)
        .ok_or_else(|| VmError::JitNative(context.to_string()))
}

fn usize_to_i32(value: usize, context: &str) -> VmResult<i32> {
    i32::try_from(value)
        .map_err(|_| VmError::JitNative(format!("{} exceeds 32-bit displacement range", context)))
}

fn run_step<F>(vm: *mut Vm, helper_name: &str, f: F) -> i32
where
    F: FnOnce(&mut Vm) -> VmResult<i32>,
{
    let Some(vm_ref) = (unsafe { vm.as_mut() }) else {
        store_bridge_error(VmError::JitNative(format!(
            "cranelift trace {helper_name} helper received null vm pointer"
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

extern "C" fn pd_vm_cranelift_step(vm: *mut Vm, op: i64, a: i64, b: i64, c: i64) -> i32 {
    run_step(vm, "step", |vm| match op {
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
            vm.binary_numeric_op(
                |lhs, rhs| {
                    if rhs == 0 {
                        return Err(VmError::DivisionByZero);
                    }
                    Ok(lhs.wrapping_div(rhs))
                },
                |lhs, rhs| {
                    if rhs == 0.0 {
                        return Err(VmError::DivisionByZero);
                    }
                    Ok(lhs / rhs)
                },
            )?;
            Ok(STATUS_CONTINUE)
        }
        OP_MOD => {
            vm.binary_numeric_op(
                |lhs, rhs| {
                    if rhs == 0 {
                        return Err(VmError::DivisionByZero);
                    }
                    Ok(lhs.wrapping_rem(rhs))
                },
                |lhs, rhs| {
                    if rhs == 0.0 {
                        return Err(VmError::DivisionByZero);
                    }
                    Ok(lhs % rhs)
                },
            )?;
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
        OP_NEG => {
            let value = vm.pop_numeric()?;
            match value {
                NumericValue::Int(value) => vm
                    .stack
                    .push(crate::bytecode::Value::Int(value.wrapping_neg())),
                NumericValue::Float(value) => vm.stack.push(crate::bytecode::Value::Float(-value)),
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
            let slot = vm
                .locals
                .get_mut(index as usize)
                .ok_or(VmError::InvalidLocal(index))?;
            *slot = value;
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
        OP_JUMP => {
            let target_ip = usize::try_from(a)
                .map_err(|_| VmError::JitNative("jump target out of range".to_string()))?;
            vm.jump_to(target_ip)?;
            Ok(STATUS_TRACE_EXIT)
        }
        _ => Err(VmError::JitNative(format!(
            "cranelift step helper received unsupported op id {op}"
        ))),
    })
}
