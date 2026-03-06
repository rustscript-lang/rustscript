use super::layout::checked_add_i32;
use super::*;

pub(super) fn emit_fuel_tick_inline(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    offsets: ResolvedOffsets,
    steps_to_advance: u32,
    fuel_check_interval: u32,
) {
    if steps_to_advance == 0 {
        return;
    }

    let continue_block = b.create_block();
    emit_fuel_tick_inline_core(
        b,
        vm_ptr,
        exit_block,
        offsets,
        steps_to_advance,
        fuel_check_interval,
        continue_block,
    );
    b.switch_to_block(continue_block);
}

pub(super) fn emit_fuel_tick_inline_guarded(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    offsets: ResolvedOffsets,
    steps_to_advance: u32,
    fuel_check_interval: u32,
) {
    if steps_to_advance == 0 {
        return;
    }

    let metering_enabled_block = b.create_block();
    let continue_block = b.create_block();
    let fuel_enabled = b
        .ins()
        .load(types::I8, MemFlags::new(), vm_ptr, offsets.fuel_enabled);
    let metering_enabled = b.ins().icmp_imm(IntCC::NotEqual, fuel_enabled, 0);
    b.ins().brif(
        metering_enabled,
        metering_enabled_block,
        &[],
        continue_block,
        &[],
    );
    b.switch_to_block(metering_enabled_block);
    emit_fuel_tick_inline_core(
        b,
        vm_ptr,
        exit_block,
        offsets,
        steps_to_advance,
        fuel_check_interval,
        continue_block,
    );
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
    let charge_block = b.create_block();

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
        .brif(no_charge, countdown_block, &[], charge_block, &[]);

    b.switch_to_block(countdown_block);
    let new_ops = b.ins().isub(ops_until_check, chunk_steps);
    b.ins().store(
        MemFlags::new(),
        new_ops,
        vm_ptr,
        offsets.fuel_ops_until_check,
    );
    b.ins().jump(continue_block, &[]);

    b.switch_to_block(charge_block);
    let remaining = b
        .ins()
        .load(types::I64, MemFlags::new(), vm_ptr, offsets.fuel_remaining);
    let interval_i32 = b.ins().iconst(types::I32, i64::from(fuel_check_interval));
    let charge_amount = b.ins().uextend(types::I64, interval_i32);
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
    let overrun = b.ins().isub(chunk_steps, ops_until_check);
    let next_ops = b.ins().isub(interval_i32, overrun);
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

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_inline_or_helper_step(
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
        TraceStep::Call { .. } | TraceStep::BuiltinCall { .. } => Ok(false),
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

    let locals_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.locals_ptr);
    let dst_addr = value_addr(b, pointer_type, locals_ptr, idx, layout.value.size);
    let dst_tag = load_tag_i32(b, layout.value, dst_addr);

    let dst_scalar = is_scalar_tag(b, layout.value, dst_tag);
    let scalar_ok = b.create_block();
    b.ins().brif(dst_scalar, scalar_ok, &[], slow, &[]);

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
    let int_fast = b.create_block();
    let bool_check = b.create_block();
    let bool_fast = b.create_block();
    let null_check = b.create_block();
    let null_fast = b.create_block();
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
    let tags_equal = b.ins().icmp(IntCC::Equal, lhs_tag, rhs_tag);
    let tag_eq = b.create_block();
    b.ins().brif(tags_equal, tag_eq, &[], slow, &[]);

    b.switch_to_block(tag_eq);
    let lhs_int = b
        .ins()
        .icmp_imm(IntCC::Equal, lhs_tag, i64::from(layout.value.int_tag));
    b.ins().brif(lhs_int, int_fast, &[], bool_check, &[]);

    b.switch_to_block(bool_check);
    let lhs_bool = b
        .ins()
        .icmp_imm(IntCC::Equal, lhs_tag, i64::from(layout.value.bool_tag));
    b.ins().brif(lhs_bool, bool_fast, &[], null_check, &[]);

    b.switch_to_block(null_check);
    let lhs_null = b
        .ins()
        .icmp_imm(IntCC::Equal, lhs_tag, i64::from(layout.value.null_tag));
    b.ins().brif(lhs_null, null_fast, &[], slow, &[]);

    b.switch_to_block(int_fast);
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

    b.switch_to_block(bool_fast);
    let lhs_bool_value = b.ins().load(
        types::I8,
        MemFlags::new(),
        lhs_addr,
        layout.value.bool_payload_offset,
    );
    let rhs_bool_value = b.ins().load(
        types::I8,
        MemFlags::new(),
        rhs_addr,
        layout.value.bool_payload_offset,
    );
    let bool_eq = b.ins().icmp(IntCC::Equal, lhs_bool_value, rhs_bool_value);
    store_bool_in_value(b, layout.value, lhs_addr, bool_eq);
    let new_len_bool = b.ins().isub(len, one);
    b.ins()
        .store(MemFlags::new(), new_len_bool, vm_ptr, offsets.stack_len);
    b.ins().jump(next, &[]);

    b.switch_to_block(null_fast);
    let null_eq = b
        .ins()
        .icmp_imm(IntCC::Equal, lhs_tag, i64::from(layout.value.null_tag));
    store_bool_in_value(b, layout.value, lhs_addr, null_eq);
    let new_len_null = b.ins().isub(len, one);
    b.ins()
        .store(MemFlags::new(), new_len_null, vm_ptr, offsets.stack_len);
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

pub(super) fn emit_helper_step(
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

pub(super) fn jump_with_status(
    b: &mut FunctionBuilder,
    block: Block,
    status: cranelift_codegen::ir::Value,
) {
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

pub(super) fn resolve_offsets(layout: NativeStackLayout) -> VmResult<ResolvedOffsets> {
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

    let constants_ptr = layout.vm_program_constants_ptr_offset;
    let constants_len = layout.vm_program_constants_len_offset;

    Ok(ResolvedOffsets {
        stack_ptr,
        stack_len,
        stack_cap,
        locals_ptr,
        locals_len,
        constants_ptr,
        constants_len,
        vm_ip: layout.vm_ip_offset,
        fuel_enabled: layout.vm_fuel_enabled_offset,
        fuel_remaining: layout.vm_fuel_remaining_offset,
        fuel_ops_until_check: layout.vm_fuel_ops_until_check_offset,
    })
}

pub(super) fn helper_signature(
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

pub(super) fn entry_signature(
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
        TraceStep::BuiltinCall {
            index,
            argc,
            call_ip,
        } => {
            let call_ip = i64::try_from(*call_ip)
                .map_err(|_| VmError::JitNative("call ip out of range for i64".to_string()))?;
            (
                OP_BUILTIN_CALL,
                i64::from(*index),
                i64::from(*argc),
                call_ip,
            )
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
