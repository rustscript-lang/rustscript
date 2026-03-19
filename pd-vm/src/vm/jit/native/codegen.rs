use super::*;
use crate::vm::native::{
    HeapIntrinsicAddrs, HeapIntrinsicRefs, InlineEmitCtx as SharedInlineEmitCtx,
    NativeInlineStep as SharedNativeInlineStep, NativeStackLayout,
    ResolvedOffsets as SharedResolvedOffsets, ValueLayout, checked_add_i32,
    emit_native_inline_step,
};

#[derive(Clone, Copy)]
pub(super) struct HelperEmitCtx {
    pub(super) vm_ptr: cranelift_codegen::ir::Value,
    pub(super) helper_ref: FuncRef,
    pub(super) exit_block: Block,
    pub(super) offsets: ResolvedOffsets,
}

fn shared_inline_offsets(
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
) -> SharedResolvedOffsets {
    SharedResolvedOffsets {
        stack_ptr: offsets.stack_ptr,
        stack_len: offsets.stack_len,
        stack_cap: offsets.stack_cap,
        locals_ptr: offsets.locals_ptr,
        locals_len: offsets.locals_len,
        constants_ptr: offsets.constants_ptr,
        constants_len: offsets.constants_len,
        vm_ip: offsets.vm_ip,
        drop_contract_events_enabled: layout.vm_drop_contract_events_enabled_offset,
        drop_contract_events: offsets.drop_contract_events,
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_shared_inline_trace_step(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    vm_status_helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    step_ip: usize,
    step: SharedNativeInlineStep,
) -> VmResult<()> {
    emit_native_inline_step(
        b,
        SharedInlineEmitCtx {
            vm_ptr,
            helper_ref,
            _vm_status_helper_ref: vm_status_helper_ref,
            exit_block,
            pointer_type,
            layout,
            offsets: shared_inline_offsets(layout, offsets),
            heap_refs: HeapIntrinsicRefs {
                alloc_buffer_ref: helper_ref,
                free_buffer_ref: helper_ref,
                pack_shared_ref: helper_ref,
                drop_shared_ref: helper_ref,
                copy_bytes_ref: helper_ref,
            },
            heap_addrs: HeapIntrinsicAddrs {
                alloc_byte_buffer: 0,
                alloc_value_buffer: 0,
                pack_string: 0,
                pack_bytes: 0,
                pack_array: 0,
                copy_bytes: 0,
                zero_bytes: 0,
                drop_string: 0,
                drop_bytes: 0,
                drop_array: 0,
            },
        },
        step_ip,
        step,
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_shared_inline_trace_step_heap(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    vm_status_helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    heap_refs: HeapIntrinsicRefs,
    heap_addrs: HeapIntrinsicAddrs,
    step_ip: usize,
    step: SharedNativeInlineStep,
) -> VmResult<()> {
    emit_native_inline_step(
        b,
        SharedInlineEmitCtx {
            vm_ptr,
            helper_ref,
            _vm_status_helper_ref: vm_status_helper_ref,
            exit_block,
            pointer_type,
            layout,
            offsets: shared_inline_offsets(layout, offsets),
            heap_refs,
            heap_addrs,
        },
        step_ip,
        step,
    )
}

pub(super) fn emit_interrupt_tick_inline(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    offsets: ResolvedOffsets,
    steps_to_advance: u32,
    interrupt_settings: super::super::NativeInterruptSettings,
) {
    if steps_to_advance == 0 {
        return;
    }

    let continue_block = b.create_block();
    match interrupt_settings.mode {
        super::super::NativeInterruptMode::Fuel => emit_fuel_tick_inline_core(
            b,
            vm_ptr,
            exit_block,
            offsets,
            steps_to_advance,
            interrupt_settings.check_interval,
            continue_block,
        ),
        super::super::NativeInterruptMode::Epoch => emit_epoch_tick_inline_core(
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

pub(super) fn emit_interrupt_tick_inline_guarded(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    offsets: ResolvedOffsets,
    steps_to_advance: u32,
    interrupt_settings: super::super::NativeInterruptSettings,
) {
    if steps_to_advance == 0 {
        return;
    }

    let settings_match_block = b.create_block();
    let mode_match_block = b.create_block();
    let mismatch_block = b.create_block();
    let continue_block = b.create_block();
    let interrupt_mode = b
        .ins()
        .load(types::I8, MemFlags::new(), vm_ptr, offsets.interrupt_mode);
    let metering_enabled = b.ins().icmp_imm(IntCC::NotEqual, interrupt_mode, 0);
    b.ins()
        .brif(metering_enabled, mode_match_block, &[], continue_block, &[]);

    b.switch_to_block(mode_match_block);
    let expected_mode = match interrupt_settings.mode {
        super::super::NativeInterruptMode::Fuel => crate::vm::InterruptMode::Fuel as u8,
        super::super::NativeInterruptMode::Epoch => crate::vm::InterruptMode::Epoch as u8,
    };
    let mode_matches = b
        .ins()
        .icmp_imm(IntCC::Equal, interrupt_mode, i64::from(expected_mode));
    b.ins()
        .brif(mode_matches, settings_match_block, &[], mismatch_block, &[]);

    b.switch_to_block(settings_match_block);
    let live_interval = b.ins().load(
        types::I32,
        MemFlags::new(),
        vm_ptr,
        offsets.fuel_check_interval,
    );
    let expected_interval = b
        .ins()
        .iconst(types::I32, i64::from(interrupt_settings.check_interval));
    let interval_matches = b.ins().icmp(IntCC::Equal, live_interval, expected_interval);
    let specialized_block = b.create_block();
    b.ins().brif(
        interval_matches,
        specialized_block,
        &[],
        mismatch_block,
        &[],
    );

    b.switch_to_block(specialized_block);
    match interrupt_settings.mode {
        super::super::NativeInterruptMode::Fuel => emit_fuel_tick_inline_core(
            b,
            vm_ptr,
            exit_block,
            offsets,
            steps_to_advance,
            interrupt_settings.check_interval,
            continue_block,
        ),
        super::super::NativeInterruptMode::Epoch => emit_epoch_tick_inline_core(
            b,
            vm_ptr,
            exit_block,
            offsets,
            steps_to_advance,
            interrupt_settings.check_interval,
            continue_block,
        ),
    }

    b.switch_to_block(mismatch_block);
    let status = b.ins().iconst(types::I32, STATUS_TRACE_EXIT as i64);
    jump_with_status(b, exit_block, status);

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
    let remaining = b
        .ins()
        .load(types::I64, MemFlags::new(), vm_ptr, offsets.fuel_remaining);
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
    let epoch_overrun = b.ins().isub(chunk_steps, ops_until_check);
    let epoch_next_ops = b.ins().isub(interval_i32, epoch_overrun);
    b.ins().store(
        MemFlags::new(),
        epoch_next_ops,
        vm_ptr,
        offsets.fuel_ops_until_check,
    );
    b.ins().jump(continue_block, &[]);

    b.switch_to_block(epoch_tripped);
    let epoch_status = b.ins().iconst(types::I32, STATUS_OUT_OF_FUEL as i64);
    jump_with_status(b, exit_block, epoch_status);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_inline_or_helper_step(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    vm_status_helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    heap_refs: HeapIntrinsicRefs,
    heap_addrs: HeapIntrinsicAddrs,
    root_ip: usize,
    step_ip: usize,
    step: &TraceStep,
    loop_target_block: Option<Block>,
    drop_contract_events_enabled: bool,
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
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *index,
                root_ip,
                step_ip,
            )?;
            Ok(true)
        }
        TraceStep::Ldloc(index) => {
            emit_inline_ldloc(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *index,
                root_ip,
                step_ip,
            )?;
            Ok(true)
        }
        TraceStep::Stloc(index) => {
            emit_inline_stloc(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *index,
                root_ip,
                step_ip,
                drop_contract_events_enabled,
            )?;
            Ok(true)
        }
        TraceStep::Pop => {
            emit_inline_pop(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
            )?;
            Ok(true)
        }
        TraceStep::Dup => {
            emit_inline_dup(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
            )?;
            Ok(true)
        }
        TraceStep::Add | TraceStep::IAdd => {
            emit_inline_int_binop(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
                IntBinopKind::Add,
            )?;
            Ok(true)
        }
        TraceStep::IAddImm(imm) => {
            emit_inline_stack_int_imm_binop(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *imm,
                step_ip,
                IntImmBinopKind::Add,
            )?;
            Ok(true)
        }
        TraceStep::ILocalAddImm { local, imm } => {
            emit_inline_local_numeric_imm_op(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *local,
                NumberImmediate::Int(*imm),
                step_ip,
                LocalNumericImmOpKind::Add,
            )?;
            Ok(true)
        }
        TraceStep::FAdd => {
            emit_inline_float_binop(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
                FloatBinopKind::Add,
            )?;
            Ok(true)
        }
        TraceStep::FAddImm(imm_bits) => {
            emit_inline_stack_float_imm_binop(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *imm_bits,
                step_ip,
                FloatImmBinopKind::Add,
            )?;
            Ok(true)
        }
        TraceStep::FLocalAddImm { local, imm_bits } => {
            emit_inline_local_numeric_imm_op(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *local,
                NumberImmediate::Float(*imm_bits),
                step_ip,
                LocalNumericImmOpKind::Add,
            )?;
            Ok(true)
        }
        TraceStep::Concat(TraceConcatKind::String) => {
            emit_shared_inline_trace_step_heap(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                heap_refs,
                heap_addrs,
                step_ip,
                SharedNativeInlineStep::StringConcat,
            )?;
            Ok(true)
        }
        TraceStep::Concat(TraceConcatKind::Bytes) => {
            emit_shared_inline_trace_step_heap(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                heap_refs,
                heap_addrs,
                step_ip,
                SharedNativeInlineStep::BytesConcat,
            )?;
            Ok(true)
        }
        TraceStep::Len(TraceTextBytesKind::String) => {
            emit_shared_inline_trace_step_heap(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                heap_refs,
                heap_addrs,
                step_ip,
                SharedNativeInlineStep::StringLen,
            )?;
            Ok(true)
        }
        TraceStep::Len(TraceTextBytesKind::Bytes) => {
            emit_shared_inline_trace_step_heap(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                heap_refs,
                heap_addrs,
                step_ip,
                SharedNativeInlineStep::BytesLen,
            )?;
            Ok(true)
        }
        TraceStep::Slice(TraceTextBytesKind::String) => {
            emit_shared_inline_trace_step_heap(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                heap_refs,
                heap_addrs,
                step_ip,
                SharedNativeInlineStep::StringSlice,
            )?;
            Ok(true)
        }
        TraceStep::Slice(TraceTextBytesKind::Bytes) => {
            emit_shared_inline_trace_step_heap(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                heap_refs,
                heap_addrs,
                step_ip,
                SharedNativeInlineStep::BytesSlice,
            )?;
            Ok(true)
        }
        TraceStep::Get(TraceTextBytesKind::String) => {
            emit_shared_inline_trace_step_heap(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                heap_refs,
                heap_addrs,
                step_ip,
                SharedNativeInlineStep::StringGet,
            )?;
            Ok(true)
        }
        TraceStep::Get(TraceTextBytesKind::Bytes) => {
            emit_shared_inline_trace_step_heap(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                heap_refs,
                heap_addrs,
                step_ip,
                SharedNativeInlineStep::BytesGet,
            )?;
            Ok(true)
        }
        TraceStep::HasBytes => {
            emit_shared_inline_trace_step_heap(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                heap_refs,
                heap_addrs,
                step_ip,
                SharedNativeInlineStep::BytesHas,
            )?;
            Ok(true)
        }
        TraceStep::BytesCodec(TraceBytesCodecKind::FromArrayU8) => {
            emit_shared_inline_trace_step_heap(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                heap_refs,
                heap_addrs,
                step_ip,
                SharedNativeInlineStep::BytesFromArrayU8,
            )?;
            Ok(true)
        }
        TraceStep::BytesCodec(TraceBytesCodecKind::ToArrayU8) => {
            emit_shared_inline_trace_step_heap(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                heap_refs,
                heap_addrs,
                step_ip,
                SharedNativeInlineStep::BytesToArrayU8,
            )?;
            Ok(true)
        }
        TraceStep::BytesCodec(
            TraceBytesCodecKind::FromUtf8
            | TraceBytesCodecKind::ToUtf8
            | TraceBytesCodecKind::ToUtf8Lossy
            | TraceBytesCodecKind::FromHex
            | TraceBytesCodecKind::ToHex
            | TraceBytesCodecKind::FromBase64
            | TraceBytesCodecKind::ToBase64,
        ) => Err(VmError::JitNative(
            "utf8/hex/base64 bytes codecs should stay on the builtin-call path".to_string(),
        )),
        TraceStep::Sub | TraceStep::ISub => {
            emit_inline_int_binop(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
                IntBinopKind::Sub,
            )?;
            Ok(true)
        }
        TraceStep::ISubImm(imm) => {
            emit_inline_stack_int_imm_binop(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *imm,
                step_ip,
                IntImmBinopKind::Sub,
            )?;
            Ok(true)
        }
        TraceStep::ILocalSubImm { local, imm } => {
            emit_inline_local_numeric_imm_op(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *local,
                NumberImmediate::Int(*imm),
                step_ip,
                LocalNumericImmOpKind::Sub,
            )?;
            Ok(true)
        }
        TraceStep::FSub => {
            emit_inline_float_binop(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
                FloatBinopKind::Sub,
            )?;
            Ok(true)
        }
        TraceStep::FSubImm(imm_bits) => {
            emit_inline_stack_float_imm_binop(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *imm_bits,
                step_ip,
                FloatImmBinopKind::Sub,
            )?;
            Ok(true)
        }
        TraceStep::FLocalSubImm { local, imm_bits } => {
            emit_inline_local_numeric_imm_op(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *local,
                NumberImmediate::Float(*imm_bits),
                step_ip,
                LocalNumericImmOpKind::Sub,
            )?;
            Ok(true)
        }
        TraceStep::Mul | TraceStep::IMul => {
            emit_inline_int_binop(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
                IntBinopKind::Mul,
            )?;
            Ok(true)
        }
        TraceStep::IMulImm(imm) => {
            emit_inline_stack_int_imm_binop(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *imm,
                step_ip,
                IntImmBinopKind::Mul,
            )?;
            Ok(true)
        }
        TraceStep::ILocalMulImm { local, imm } => {
            emit_inline_local_numeric_imm_op(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *local,
                NumberImmediate::Int(*imm),
                step_ip,
                LocalNumericImmOpKind::Mul,
            )?;
            Ok(true)
        }
        TraceStep::FMul => {
            emit_inline_float_binop(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
                FloatBinopKind::Mul,
            )?;
            Ok(true)
        }
        TraceStep::FMulImm(imm_bits) => {
            emit_inline_stack_float_imm_binop(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *imm_bits,
                step_ip,
                FloatImmBinopKind::Mul,
            )?;
            Ok(true)
        }
        TraceStep::FLocalMulImm { local, imm_bits } => {
            emit_inline_local_numeric_imm_op(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *local,
                NumberImmediate::Float(*imm_bits),
                step_ip,
                LocalNumericImmOpKind::Mul,
            )?;
            Ok(true)
        }
        TraceStep::Div | TraceStep::IDiv => {
            emit_inline_int_divrem(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
                false,
            )?;
            Ok(true)
        }
        TraceStep::IDivImm(imm) => {
            emit_inline_stack_int_imm_binop(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *imm,
                step_ip,
                IntImmBinopKind::Div,
            )?;
            Ok(true)
        }
        TraceStep::ILocalDivImm { local, imm } => {
            emit_inline_local_numeric_imm_op(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *local,
                NumberImmediate::Int(*imm),
                step_ip,
                LocalNumericImmOpKind::Div,
            )?;
            Ok(true)
        }
        TraceStep::FDiv => {
            emit_inline_float_binop(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
                FloatBinopKind::Div,
            )?;
            Ok(true)
        }
        TraceStep::FDivImm(imm_bits) => {
            emit_inline_stack_float_imm_binop(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *imm_bits,
                step_ip,
                FloatImmBinopKind::Div,
            )?;
            Ok(true)
        }
        TraceStep::FLocalDivImm { local, imm_bits } => {
            emit_inline_local_numeric_imm_op(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *local,
                NumberImmediate::Float(*imm_bits),
                step_ip,
                LocalNumericImmOpKind::Div,
            )?;
            Ok(true)
        }
        TraceStep::Mod | TraceStep::IMod => {
            emit_inline_int_divrem(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
                true,
            )?;
            Ok(true)
        }
        TraceStep::IModImm(imm) => {
            emit_inline_stack_int_imm_binop(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *imm,
                step_ip,
                IntImmBinopKind::Mod,
            )?;
            Ok(true)
        }
        TraceStep::ILocalModImm { local, imm } => {
            emit_inline_local_numeric_imm_op(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *local,
                NumberImmediate::Int(*imm),
                step_ip,
                LocalNumericImmOpKind::Mod,
            )?;
            Ok(true)
        }
        TraceStep::FMod => {
            emit_inline_float_binop(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
                FloatBinopKind::Mod,
            )?;
            Ok(true)
        }
        TraceStep::FModImm(imm_bits) => {
            emit_inline_stack_float_imm_binop(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *imm_bits,
                step_ip,
                FloatImmBinopKind::Mod,
            )?;
            Ok(true)
        }
        TraceStep::FLocalModImm { local, imm_bits } => {
            emit_inline_local_numeric_imm_op(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *local,
                NumberImmediate::Float(*imm_bits),
                step_ip,
                LocalNumericImmOpKind::Mod,
            )?;
            Ok(true)
        }
        TraceStep::Shl => {
            emit_inline_shift(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
                ShiftKind::Left,
            )?;
            Ok(true)
        }
        TraceStep::ILocalShlImm { local, amount } => {
            emit_inline_local_numeric_imm_op(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *local,
                NumberImmediate::Int(i64::from(*amount)),
                step_ip,
                LocalNumericImmOpKind::Shl,
            )?;
            Ok(true)
        }
        TraceStep::Shr => {
            emit_inline_shift(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
                ShiftKind::ArithmeticRight,
            )?;
            Ok(true)
        }
        TraceStep::Lshr => {
            emit_inline_shift(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
                ShiftKind::LogicalRight,
            )?;
            Ok(true)
        }
        TraceStep::And => {
            emit_inline_bool_logic(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
                true,
            )?;
            Ok(true)
        }
        TraceStep::Or => {
            emit_inline_bool_logic(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
                false,
            )?;
            Ok(true)
        }
        TraceStep::Not => {
            emit_inline_not(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
            )?;
            Ok(true)
        }
        TraceStep::Neg | TraceStep::INeg => {
            emit_inline_neg(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
            )?;
            Ok(true)
        }
        TraceStep::FNeg => {
            emit_inline_float_neg(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
            )?;
            Ok(true)
        }
        TraceStep::Clt => {
            emit_inline_int_compare(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
                true,
            )?;
            Ok(true)
        }
        TraceStep::ILocalCltImm { local, imm } => {
            emit_inline_local_numeric_imm_op(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *local,
                NumberImmediate::Int(*imm),
                step_ip,
                LocalNumericImmOpKind::Clt,
            )?;
            Ok(true)
        }
        TraceStep::FClt => {
            emit_inline_float_compare(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
                true,
            )?;
            Ok(true)
        }
        TraceStep::FLocalCltImm { local, imm_bits } => {
            emit_inline_local_numeric_imm_op(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *local,
                NumberImmediate::Float(*imm_bits),
                step_ip,
                LocalNumericImmOpKind::Clt,
            )?;
            Ok(true)
        }
        TraceStep::Cgt => {
            emit_inline_int_compare(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
                false,
            )?;
            Ok(true)
        }
        TraceStep::ILocalCgtImm { local, imm } => {
            emit_inline_local_numeric_imm_op(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *local,
                NumberImmediate::Int(*imm),
                step_ip,
                LocalNumericImmOpKind::Cgt,
            )?;
            Ok(true)
        }
        TraceStep::FCgt => {
            emit_inline_float_compare(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
                false,
            )?;
            Ok(true)
        }
        TraceStep::FLocalCgtImm { local, imm_bits } => {
            emit_inline_local_numeric_imm_op(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *local,
                NumberImmediate::Float(*imm_bits),
                step_ip,
                LocalNumericImmOpKind::Cgt,
            )?;
            Ok(true)
        }
        TraceStep::Ceq => {
            emit_inline_int_eq(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
            )?;
            Ok(true)
        }
        TraceStep::FCeq => {
            emit_inline_float_eq(
                b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                step_ip,
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
                step_ip,
            )?;
            Ok(true)
        }
        TraceStep::GuardTrue { exit_ip } => {
            emit_inline_guard_true(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                root_ip,
                *exit_ip,
                step_ip,
            )?;
            Ok(true)
        }
        TraceStep::LoopIfFalse { exit_ip, .. } => {
            let loop_target_block = loop_target_block.ok_or_else(|| {
                VmError::JitNative("loop_if_false is missing a target block".to_string())
            })?;
            emit_inline_loop_if_false(
                b,
                vm_ptr,
                helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                *exit_ip,
                step_ip,
                loop_target_block,
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
    vm_status_helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    index: u32,
    root_ip: usize,
    step_ip: usize,
) -> VmResult<()> {
    let _ = root_ip;
    emit_shared_inline_trace_step(
        b,
        vm_ptr,
        helper_ref,
        vm_status_helper_ref,
        exit_block,
        pointer_type,
        layout,
        offsets,
        step_ip,
        SharedNativeInlineStep::Ldc(index),
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_ldloc(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    vm_status_helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    index: u8,
    root_ip: usize,
    step_ip: usize,
) -> VmResult<()> {
    let _ = root_ip;
    emit_shared_inline_trace_step(
        b,
        vm_ptr,
        helper_ref,
        vm_status_helper_ref,
        exit_block,
        pointer_type,
        layout,
        offsets,
        step_ip,
        SharedNativeInlineStep::Ldloc(index),
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_stloc(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    vm_status_helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    index: u8,
    root_ip: usize,
    step_ip: usize,
    drop_contract_events_enabled: bool,
) -> VmResult<()> {
    let _ = (root_ip, drop_contract_events_enabled);
    emit_shared_inline_trace_step(
        b,
        vm_ptr,
        helper_ref,
        vm_status_helper_ref,
        exit_block,
        pointer_type,
        layout,
        offsets,
        step_ip,
        SharedNativeInlineStep::Stloc(index),
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_pop(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    vm_status_helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    step_ip: usize,
) -> VmResult<()> {
    let _ = root_ip;
    emit_shared_inline_trace_step(
        b,
        vm_ptr,
        helper_ref,
        vm_status_helper_ref,
        exit_block,
        pointer_type,
        layout,
        offsets,
        step_ip,
        SharedNativeInlineStep::Pop,
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_dup(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    vm_status_helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    step_ip: usize,
) -> VmResult<()> {
    let _ = root_ip;
    emit_shared_inline_trace_step(
        b,
        vm_ptr,
        helper_ref,
        vm_status_helper_ref,
        exit_block,
        pointer_type,
        layout,
        offsets,
        step_ip,
        SharedNativeInlineStep::Dup,
    )
}

enum IntBinopKind {
    Add,
    Sub,
    Mul,
}

enum IntImmBinopKind {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

enum FloatBinopKind {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

enum FloatImmBinopKind {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

enum LocalNumericImmOpKind {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Clt,
    Cgt,
    Shl,
}

enum NumberImmediate {
    Int(i64),
    Float(u64),
}

#[derive(Clone, Copy)]
enum ShiftKind {
    Left,
    ArithmeticRight,
    LogicalRight,
}

fn emit_trace_exit_to_step_ip(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    offsets: ResolvedOffsets,
    step_ip: usize,
) -> VmResult<()> {
    let step_ip = i64::try_from(step_ip)
        .map_err(|_| VmError::JitNative("step ip out of range for i64".to_string()))?;
    let step_ip_val = b.ins().iconst(pointer_type, step_ip);
    b.ins()
        .store(MemFlags::new(), step_ip_val, vm_ptr, offsets.vm_ip);
    let status = b.ins().iconst(types::I32, STATUS_TRACE_EXIT as i64);
    jump_with_status(b, exit_block, status);
    let dead = b.create_block();
    b.switch_to_block(dead);
    Ok(())
}

fn store_int_in_value(
    b: &mut FunctionBuilder,
    layout: ValueLayout,
    value_addr: cranelift_codegen::ir::Value,
    int_value: cranelift_codegen::ir::Value,
) {
    store_tag(b, layout, value_addr, layout.int_tag);
    b.ins().store(
        MemFlags::new(),
        int_value,
        value_addr,
        layout.int_payload_offset,
    );
}

fn store_float_in_value(
    b: &mut FunctionBuilder,
    layout: ValueLayout,
    value_addr: cranelift_codegen::ir::Value,
    float_value: cranelift_codegen::ir::Value,
) {
    store_tag(b, layout, value_addr, layout.float_tag);
    b.ins().store(
        MemFlags::new(),
        float_value,
        value_addr,
        layout.float_payload_offset,
    );
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_int_binop(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    vm_status_helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    step_ip: usize,
    kind: IntBinopKind,
) -> VmResult<()> {
    let _ = root_ip;
    emit_shared_inline_trace_step(
        b,
        vm_ptr,
        helper_ref,
        vm_status_helper_ref,
        exit_block,
        pointer_type,
        layout,
        offsets,
        step_ip,
        match kind {
            IntBinopKind::Add => SharedNativeInlineStep::Add,
            IntBinopKind::Sub => SharedNativeInlineStep::Sub,
            IntBinopKind::Mul => SharedNativeInlineStep::Mul,
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_float_binop(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    vm_status_helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    step_ip: usize,
    kind: FloatBinopKind,
) -> VmResult<()> {
    let _ = root_ip;
    emit_shared_inline_trace_step(
        b,
        vm_ptr,
        helper_ref,
        vm_status_helper_ref,
        exit_block,
        pointer_type,
        layout,
        offsets,
        step_ip,
        match kind {
            FloatBinopKind::Add => SharedNativeInlineStep::FAdd,
            FloatBinopKind::Sub => SharedNativeInlineStep::FSub,
            FloatBinopKind::Mul => SharedNativeInlineStep::FMul,
            FloatBinopKind::Div => SharedNativeInlineStep::FDiv,
            FloatBinopKind::Mod => SharedNativeInlineStep::FMod,
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_stack_int_imm_binop(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    imm: i64,
    step_ip: usize,
    kind: IntImmBinopKind,
) -> VmResult<()> {
    let exit = b.create_block();
    let len_ok = b.create_block();
    let type_ok = b.create_block();
    let next = b.create_block();

    let len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_len);
    let enough = b.ins().icmp_imm(IntCC::UnsignedGreaterThanOrEqual, len, 1);
    b.ins().brif(enough, len_ok, &[], exit, &[]);

    b.switch_to_block(len_ok);
    let one = b.ins().iconst(pointer_type, 1);
    let index = b.ins().isub(len, one);
    let stack_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_ptr);
    let top_addr = value_addr(b, pointer_type, stack_ptr, index, layout.value.size);
    let top_tag = load_tag_i32(b, layout.value, top_addr);
    let is_int = b
        .ins()
        .icmp_imm(IntCC::Equal, top_tag, i64::from(layout.value.int_tag));
    b.ins().brif(is_int, type_ok, &[], exit, &[]);

    b.switch_to_block(type_ok);
    let lhs = b.ins().load(
        types::I64,
        MemFlags::new(),
        top_addr,
        layout.value.int_payload_offset,
    );
    let imm_val = b.ins().iconst(types::I64, imm);
    let out = match kind {
        IntImmBinopKind::Add => b.ins().iadd(lhs, imm_val),
        IntImmBinopKind::Sub => b.ins().isub(lhs, imm_val),
        IntImmBinopKind::Mul => b.ins().imul(lhs, imm_val),
        IntImmBinopKind::Div | IntImmBinopKind::Mod => {
            if imm == 0 {
                b.ins().jump(exit, &[]);
                b.switch_to_block(exit);
                emit_trace_exit_to_step_ip(b, vm_ptr, exit_block, pointer_type, offsets, step_ip)?;
                b.switch_to_block(next);
                return Ok(());
            }
            let overflow_ok = b.create_block();
            if imm == -1 {
                let min_i64 = b.ins().iconst(types::I64, i64::MIN);
                let lhs_is_min = b.ins().icmp(IntCC::Equal, lhs, min_i64);
                b.ins().brif(lhs_is_min, exit, &[], overflow_ok, &[]);
                b.switch_to_block(overflow_ok);
            }
            if matches!(kind, IntImmBinopKind::Div) {
                b.ins().sdiv(lhs, imm_val)
            } else {
                b.ins().srem(lhs, imm_val)
            }
        }
    };
    store_int_in_value(b, layout.value, top_addr, out);
    b.ins().jump(next, &[]);

    b.switch_to_block(exit);
    emit_trace_exit_to_step_ip(b, vm_ptr, exit_block, pointer_type, offsets, step_ip)?;

    b.switch_to_block(next);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_stack_float_imm_binop(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    imm_bits: u64,
    step_ip: usize,
    kind: FloatImmBinopKind,
) -> VmResult<()> {
    let exit = b.create_block();
    let len_ok = b.create_block();
    let type_ok = b.create_block();
    let next = b.create_block();

    let len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_len);
    let enough = b.ins().icmp_imm(IntCC::UnsignedGreaterThanOrEqual, len, 1);
    b.ins().brif(enough, len_ok, &[], exit, &[]);

    b.switch_to_block(len_ok);
    let one = b.ins().iconst(pointer_type, 1);
    let index = b.ins().isub(len, one);
    let stack_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_ptr);
    let top_addr = value_addr(b, pointer_type, stack_ptr, index, layout.value.size);
    let top_tag = load_tag_i32(b, layout.value, top_addr);
    let is_float = b
        .ins()
        .icmp_imm(IntCC::Equal, top_tag, i64::from(layout.value.float_tag));
    b.ins().brif(is_float, type_ok, &[], exit, &[]);

    b.switch_to_block(type_ok);
    let lhs = b.ins().load(
        types::F64,
        MemFlags::new(),
        top_addr,
        layout.value.float_payload_offset,
    );
    let imm_val = b
        .ins()
        .f64const(cranelift_codegen::ir::immediates::Ieee64::with_bits(
            imm_bits,
        ));
    let out = match kind {
        FloatImmBinopKind::Add => b.ins().fadd(lhs, imm_val),
        FloatImmBinopKind::Sub => b.ins().fsub(lhs, imm_val),
        FloatImmBinopKind::Mul => b.ins().fmul(lhs, imm_val),
        FloatImmBinopKind::Div => b.ins().fdiv(lhs, imm_val),
        FloatImmBinopKind::Mod => {
            let quotient = b.ins().fdiv(lhs, imm_val);
            let truncated = b.ins().trunc(quotient);
            let product = b.ins().fmul(truncated, imm_val);
            b.ins().fsub(lhs, product)
        }
    };
    store_float_in_value(b, layout.value, top_addr, out);
    b.ins().jump(next, &[]);

    b.switch_to_block(exit);
    emit_trace_exit_to_step_ip(b, vm_ptr, exit_block, pointer_type, offsets, step_ip)?;

    b.switch_to_block(next);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_local_numeric_imm_op(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    local: u8,
    imm: NumberImmediate,
    step_ip: usize,
    kind: LocalNumericImmOpKind,
) -> VmResult<()> {
    let exit = b.create_block();
    let local_ok = b.create_block();
    let stack_ok = b.create_block();
    let type_ok = b.create_block();
    let next = b.create_block();

    let locals_len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.locals_len);
    let local_index = b.ins().iconst(pointer_type, i64::from(local));
    let local_in_bounds = b
        .ins()
        .icmp(IntCC::UnsignedLessThan, local_index, locals_len);
    b.ins().brif(local_in_bounds, local_ok, &[], exit, &[]);

    b.switch_to_block(local_ok);
    let stack_len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_len);
    let stack_cap = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_cap);
    let has_capacity = b.ins().icmp(IntCC::UnsignedLessThan, stack_len, stack_cap);
    b.ins().brif(has_capacity, stack_ok, &[], exit, &[]);

    b.switch_to_block(stack_ok);
    let locals_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.locals_ptr);
    let local_addr = value_addr(b, pointer_type, locals_ptr, local_index, layout.value.size);
    let local_tag = load_tag_i32(b, layout.value, local_addr);
    let type_matches = match imm {
        NumberImmediate::Int(_) => {
            b.ins()
                .icmp_imm(IntCC::Equal, local_tag, i64::from(layout.value.int_tag))
        }
        NumberImmediate::Float(_) => {
            b.ins()
                .icmp_imm(IntCC::Equal, local_tag, i64::from(layout.value.float_tag))
        }
    };
    b.ins().brif(type_matches, type_ok, &[], exit, &[]);

    b.switch_to_block(type_ok);
    let stack_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_ptr);
    let dst_addr = value_addr(b, pointer_type, stack_ptr, stack_len, layout.value.size);
    match imm {
        NumberImmediate::Int(imm) => {
            let lhs = b.ins().load(
                types::I64,
                MemFlags::new(),
                local_addr,
                layout.value.int_payload_offset,
            );
            match kind {
                LocalNumericImmOpKind::Add => {
                    let out = b.ins().iadd_imm(lhs, imm);
                    store_int_in_value(b, layout.value, dst_addr, out);
                }
                LocalNumericImmOpKind::Sub => {
                    let rhs = b.ins().iconst(types::I64, imm);
                    let out = b.ins().isub(lhs, rhs);
                    store_int_in_value(b, layout.value, dst_addr, out);
                }
                LocalNumericImmOpKind::Mul => {
                    let rhs = b.ins().iconst(types::I64, imm);
                    let out = b.ins().imul(lhs, rhs);
                    store_int_in_value(b, layout.value, dst_addr, out);
                }
                LocalNumericImmOpKind::Div | LocalNumericImmOpKind::Mod => {
                    if imm == 0 {
                        b.ins().jump(exit, &[]);
                        b.switch_to_block(exit);
                        emit_trace_exit_to_step_ip(
                            b,
                            vm_ptr,
                            exit_block,
                            pointer_type,
                            offsets,
                            step_ip,
                        )?;
                        b.switch_to_block(next);
                        return Ok(());
                    }
                    if imm == -1 {
                        let min_i64 = b.ins().iconst(types::I64, i64::MIN);
                        let lhs_is_min = b.ins().icmp(IntCC::Equal, lhs, min_i64);
                        let op_ok = b.create_block();
                        b.ins().brif(lhs_is_min, exit, &[], op_ok, &[]);
                        b.switch_to_block(op_ok);
                    }
                    let rhs = b.ins().iconst(types::I64, imm);
                    let out = if matches!(kind, LocalNumericImmOpKind::Div) {
                        b.ins().sdiv(lhs, rhs)
                    } else {
                        b.ins().srem(lhs, rhs)
                    };
                    store_int_in_value(b, layout.value, dst_addr, out);
                }
                LocalNumericImmOpKind::Clt => {
                    let cmp = b.ins().icmp_imm(IntCC::SignedLessThan, lhs, imm);
                    store_bool_in_value(b, layout.value, dst_addr, cmp);
                }
                LocalNumericImmOpKind::Cgt => {
                    let cmp = b.ins().icmp_imm(IntCC::SignedGreaterThan, lhs, imm);
                    store_bool_in_value(b, layout.value, dst_addr, cmp);
                }
                LocalNumericImmOpKind::Shl => {
                    let amount = u32::try_from(imm).map_err(|_| {
                        VmError::JitNative("shift immediate out of range".to_string())
                    })?;
                    let rhs = b.ins().iconst(types::I64, i64::from(amount));
                    let out = b.ins().ishl(lhs, rhs);
                    store_int_in_value(b, layout.value, dst_addr, out);
                }
            }
        }
        NumberImmediate::Float(imm_bits) => {
            let lhs = b.ins().load(
                types::F64,
                MemFlags::new(),
                local_addr,
                layout.value.float_payload_offset,
            );
            let rhs = b
                .ins()
                .f64const(cranelift_codegen::ir::immediates::Ieee64::with_bits(
                    imm_bits,
                ));
            match kind {
                LocalNumericImmOpKind::Add => {
                    let out = b.ins().fadd(lhs, rhs);
                    store_float_in_value(b, layout.value, dst_addr, out);
                }
                LocalNumericImmOpKind::Sub => {
                    let out = b.ins().fsub(lhs, rhs);
                    store_float_in_value(b, layout.value, dst_addr, out);
                }
                LocalNumericImmOpKind::Mul => {
                    let out = b.ins().fmul(lhs, rhs);
                    store_float_in_value(b, layout.value, dst_addr, out);
                }
                LocalNumericImmOpKind::Div => {
                    let out = b.ins().fdiv(lhs, rhs);
                    store_float_in_value(b, layout.value, dst_addr, out);
                }
                LocalNumericImmOpKind::Mod => {
                    let quotient = b.ins().fdiv(lhs, rhs);
                    let truncated = b.ins().trunc(quotient);
                    let product = b.ins().fmul(truncated, rhs);
                    let out = b.ins().fsub(lhs, product);
                    store_float_in_value(b, layout.value, dst_addr, out);
                }
                LocalNumericImmOpKind::Clt => {
                    let cmp = b.ins().fcmp(FloatCC::LessThan, lhs, rhs);
                    store_bool_in_value(b, layout.value, dst_addr, cmp);
                }
                LocalNumericImmOpKind::Cgt => {
                    let cmp = b.ins().fcmp(FloatCC::GreaterThan, lhs, rhs);
                    store_bool_in_value(b, layout.value, dst_addr, cmp);
                }
                LocalNumericImmOpKind::Shl => {
                    return Err(VmError::JitNative(
                        "float local immediate shift is invalid".to_string(),
                    ));
                }
            }
        }
    }

    let new_len = b.ins().iadd_imm(stack_len, 1);
    b.ins()
        .store(MemFlags::new(), new_len, vm_ptr, offsets.stack_len);
    b.ins().jump(next, &[]);

    b.switch_to_block(exit);
    emit_trace_exit_to_step_ip(b, vm_ptr, exit_block, pointer_type, offsets, step_ip)?;

    b.switch_to_block(next);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_int_divrem(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    vm_status_helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    step_ip: usize,
    is_mod: bool,
) -> VmResult<()> {
    let _ = root_ip;
    emit_shared_inline_trace_step(
        b,
        vm_ptr,
        helper_ref,
        vm_status_helper_ref,
        exit_block,
        pointer_type,
        layout,
        offsets,
        step_ip,
        if is_mod {
            SharedNativeInlineStep::Mod
        } else {
            SharedNativeInlineStep::Div
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_shift(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    vm_status_helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    step_ip: usize,
    kind: ShiftKind,
) -> VmResult<()> {
    let _ = root_ip;
    emit_shared_inline_trace_step(
        b,
        vm_ptr,
        helper_ref,
        vm_status_helper_ref,
        exit_block,
        pointer_type,
        layout,
        offsets,
        step_ip,
        match kind {
            ShiftKind::Left => SharedNativeInlineStep::Shl,
            ShiftKind::ArithmeticRight => SharedNativeInlineStep::Shr,
            ShiftKind::LogicalRight => SharedNativeInlineStep::Lshr,
        },
    )
}

fn emit_inline_not(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    vm_status_helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    step_ip: usize,
) -> VmResult<()> {
    let _ = root_ip;
    emit_shared_inline_trace_step(
        b,
        vm_ptr,
        helper_ref,
        vm_status_helper_ref,
        exit_block,
        pointer_type,
        layout,
        offsets,
        step_ip,
        SharedNativeInlineStep::Not,
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_bool_logic(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    vm_status_helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    step_ip: usize,
    is_and: bool,
) -> VmResult<()> {
    let _ = root_ip;
    emit_shared_inline_trace_step(
        b,
        vm_ptr,
        helper_ref,
        vm_status_helper_ref,
        exit_block,
        pointer_type,
        layout,
        offsets,
        step_ip,
        if is_and {
            SharedNativeInlineStep::And
        } else {
            SharedNativeInlineStep::Or
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_neg(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    vm_status_helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    step_ip: usize,
) -> VmResult<()> {
    let _ = root_ip;
    emit_shared_inline_trace_step(
        b,
        vm_ptr,
        helper_ref,
        vm_status_helper_ref,
        exit_block,
        pointer_type,
        layout,
        offsets,
        step_ip,
        SharedNativeInlineStep::Neg,
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_float_neg(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    vm_status_helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    step_ip: usize,
) -> VmResult<()> {
    let _ = root_ip;
    emit_shared_inline_trace_step(
        b,
        vm_ptr,
        helper_ref,
        vm_status_helper_ref,
        exit_block,
        pointer_type,
        layout,
        offsets,
        step_ip,
        SharedNativeInlineStep::FNeg,
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_int_compare(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    vm_status_helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    step_ip: usize,
    less_than: bool,
) -> VmResult<()> {
    let _ = root_ip;
    emit_shared_inline_trace_step(
        b,
        vm_ptr,
        helper_ref,
        vm_status_helper_ref,
        exit_block,
        pointer_type,
        layout,
        offsets,
        step_ip,
        if less_than {
            SharedNativeInlineStep::Clt
        } else {
            SharedNativeInlineStep::Cgt
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_float_compare(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    vm_status_helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    step_ip: usize,
    less_than: bool,
) -> VmResult<()> {
    let _ = root_ip;
    emit_shared_inline_trace_step(
        b,
        vm_ptr,
        helper_ref,
        vm_status_helper_ref,
        exit_block,
        pointer_type,
        layout,
        offsets,
        step_ip,
        if less_than {
            SharedNativeInlineStep::FClt
        } else {
            SharedNativeInlineStep::FCgt
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_int_eq(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    vm_status_helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    step_ip: usize,
) -> VmResult<()> {
    let _ = root_ip;
    emit_shared_inline_trace_step(
        b,
        vm_ptr,
        helper_ref,
        vm_status_helper_ref,
        exit_block,
        pointer_type,
        layout,
        offsets,
        step_ip,
        SharedNativeInlineStep::Ceq,
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_float_eq(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    vm_status_helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    step_ip: usize,
) -> VmResult<()> {
    let _ = root_ip;
    emit_shared_inline_trace_step(
        b,
        vm_ptr,
        helper_ref,
        vm_status_helper_ref,
        exit_block,
        pointer_type,
        layout,
        offsets,
        step_ip,
        SharedNativeInlineStep::FCeq,
    )
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
    step_ip: usize,
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
        offsets,
        step_ip,
        (OP_GUARD_FALSE, exit_ip, 0, 0),
    );

    b.switch_to_block(next);
    let _ = root_ip;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_loop_if_false(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    exit_ip: usize,
    step_ip: usize,
    loop_target_block: Block,
) -> VmResult<()> {
    let slow = b.create_block();
    let len_ok = b.create_block();
    let bool_ok = b.create_block();
    let branch_true = b.create_block();

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
    b.ins()
        .brif(cond_true, branch_true, &[], loop_target_block, &[]);

    b.switch_to_block(branch_true);
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
        loop_target_block,
        offsets,
        step_ip,
        (OP_LOOP_IF_FALSE, exit_ip, 0, 0),
    );

    let dead = b.create_block();
    b.switch_to_block(dead);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_guard_true(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: NativeStackLayout,
    offsets: ResolvedOffsets,
    root_ip: usize,
    exit_ip: usize,
    step_ip: usize,
) -> VmResult<()> {
    let slow = b.create_block();
    let len_ok = b.create_block();
    let bool_ok = b.create_block();
    let branch_true = b.create_block();
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
    b.ins().brif(cond_true, branch_true, &[], next, &[]);

    b.switch_to_block(branch_true);
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
        offsets,
        step_ip,
        (OP_GUARD_TRUE, exit_ip, 0, 0),
    );

    b.switch_to_block(next);
    let _ = root_ip;
    Ok(())
}

pub(super) fn emit_helper_step(
    b: &mut FunctionBuilder,
    ctx: HelperEmitCtx,
    step_ip: usize,
    root_ip: usize,
    step: &TraceStep,
) -> VmResult<()> {
    let tuple = step_to_call(step, root_ip)?;
    let next = b.create_block();
    emit_helper_step_from_call_tuple(
        b,
        ctx.vm_ptr,
        ctx.helper_ref,
        ctx.exit_block,
        next,
        ctx.offsets,
        step_ip,
        tuple,
    );
    b.switch_to_block(next);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_helper_step_from_call_tuple(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: FuncRef,
    exit_block: Block,
    next_block: Block,
    offsets: ResolvedOffsets,
    step_ip: usize,
    tuple: (i64, i64, i64, i64),
) {
    let (op, a, b_arg, c) = tuple;
    let op_val = b.ins().iconst(types::I64, op);
    let a_val = b.ins().iconst(types::I64, a);
    let b_val = b.ins().iconst(types::I64, b_arg);
    let c_val = b.ins().iconst(types::I64, c);
    let pointer_type = b.func.signature.params[0].value_type;
    let step_ip = i64::try_from(step_ip).expect("step ip must fit i64");
    let step_ip_val = b.ins().iconst(pointer_type, step_ip);
    b.ins()
        .store(MemFlags::new(), step_ip_val, vm_ptr, offsets.vm_ip);
    let helper_ptr = b.ins().load(
        pointer_type,
        MemFlags::new(),
        vm_ptr,
        native_helper_fn_offset(),
    );

    let call = b.ins().call_indirect(
        helper_ref,
        helper_ptr,
        &[vm_ptr, op_val, a_val, b_val, c_val],
    );
    let status = b.inst_results(call)[0];

    let is_continue = b
        .ins()
        .icmp_imm(IntCC::Equal, status, STATUS_CONTINUE as i64);
    let else_args = [BlockArg::Value(status)];
    b.ins()
        .brif(is_continue, next_block, &[], exit_block, &else_args);
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
        interrupt_mode: layout.vm_interrupt_mode_offset,
        fuel_remaining: layout.vm_fuel_remaining_offset,
        fuel_check_interval: layout.vm_fuel_check_interval_offset,
        fuel_ops_until_check: layout.vm_fuel_ops_until_check_offset,
        epoch_deadline: layout.vm_epoch_deadline_offset,
        epoch_counter_ptr: layout.vm_epoch_counter_ptr_offset,
        drop_contract_events: layout.vm_drop_contract_events_offset,
    })
}

fn step_to_call(step: &TraceStep, root_ip: usize) -> VmResult<(i64, i64, i64, i64)> {
    Ok(match step {
        TraceStep::Nop => (0, 0, 0, 0),
        TraceStep::Ldc(index) => (OP_LDC, i64::from(*index), 0, 0),
        TraceStep::IAddImm(_)
        | TraceStep::ILocalAddImm { .. }
        | TraceStep::FAddImm(_)
        | TraceStep::FLocalAddImm { .. }
        | TraceStep::ISubImm(_)
        | TraceStep::ILocalSubImm { .. }
        | TraceStep::FSubImm(_)
        | TraceStep::FLocalSubImm { .. }
        | TraceStep::IMulImm(_)
        | TraceStep::ILocalMulImm { .. }
        | TraceStep::FMulImm(_)
        | TraceStep::FLocalMulImm { .. }
        | TraceStep::IDivImm(_)
        | TraceStep::ILocalDivImm { .. }
        | TraceStep::FDivImm(_)
        | TraceStep::FLocalDivImm { .. }
        | TraceStep::IModImm(_)
        | TraceStep::ILocalModImm { .. }
        | TraceStep::FModImm(_)
        | TraceStep::FLocalModImm { .. }
        | TraceStep::ILocalCltImm { .. }
        | TraceStep::FLocalCltImm { .. }
        | TraceStep::ILocalCgtImm { .. }
        | TraceStep::FLocalCgtImm { .. }
        | TraceStep::ILocalShlImm { .. } => {
            return Err(VmError::JitNative(
                "fused immediate trace step must inline natively".to_string(),
            ));
        }
        TraceStep::Add | TraceStep::IAdd | TraceStep::FAdd => (OP_ADD, 0, 0, 0),
        TraceStep::Concat(_)
        | TraceStep::Len(_)
        | TraceStep::Slice(_)
        | TraceStep::Get(_)
        | TraceStep::HasBytes
        | TraceStep::BytesCodec(_) => {
            return Err(VmError::JitNative(
                "typed string/bytes trace step must lower natively".to_string(),
            ));
        }
        TraceStep::Sub | TraceStep::ISub | TraceStep::FSub => (OP_SUB, 0, 0, 0),
        TraceStep::Mul | TraceStep::IMul | TraceStep::FMul => (OP_MUL, 0, 0, 0),
        TraceStep::Div | TraceStep::IDiv | TraceStep::FDiv => (OP_DIV, 0, 0, 0),
        TraceStep::Mod | TraceStep::IMod | TraceStep::FMod => (OP_MOD, 0, 0, 0),
        TraceStep::Shl => (OP_SHL, 0, 0, 0),
        TraceStep::Shr => (OP_SHR, 0, 0, 0),
        TraceStep::Lshr => (OP_LSHR, 0, 0, 0),
        TraceStep::And => (OP_AND, 0, 0, 0),
        TraceStep::Or => (OP_OR, 0, 0, 0),
        TraceStep::Not => (OP_NOT, 0, 0, 0),
        TraceStep::Neg | TraceStep::INeg | TraceStep::FNeg => (OP_NEG, 0, 0, 0),
        TraceStep::Ceq | TraceStep::FCeq => (OP_CEQ, 0, 0, 0),
        TraceStep::Clt | TraceStep::FClt => (OP_CLT, 0, 0, 0),
        TraceStep::Cgt | TraceStep::FCgt => (OP_CGT, 0, 0, 0),
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
        TraceStep::LoopIfFalse { exit_ip, .. } => {
            let exit_ip = i64::try_from(*exit_ip).map_err(|_| {
                VmError::JitNative("guard exit ip out of range for i64".to_string())
            })?;
            (OP_LOOP_IF_FALSE, exit_ip, 0, 0)
        }
        TraceStep::GuardTrue { exit_ip } => {
            let exit_ip = i64::try_from(*exit_ip).map_err(|_| {
                VmError::JitNative("guard exit ip out of range for i64".to_string())
            })?;
            (OP_GUARD_TRUE, exit_ip, 0, 0)
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
