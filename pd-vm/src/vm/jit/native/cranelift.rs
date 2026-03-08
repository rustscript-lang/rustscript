use super::super::super::{HostCallExecOutcome, NumericValue, Value, Vm, VmError, VmResult};
use super::super::{JitTrace, TraceStep};
use super::{
    NativeCompileProfile, NativeInterruptSettings, STATUS_CONTINUE, STATUS_ERROR, STATUS_HALTED,
    STATUS_OUT_OF_FUEL, STATUS_TRACE_EXIT, STATUS_WAITING, STATUS_YIELDED, store_bridge_error,
};
use crate::builtins::BuiltinFunction;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    AbiParam, Block, BlockArg, InstBuilder, MemFlags, SigRef, Signature, types,
};
use cranelift_codegen::isa::OwnedTargetIsa;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "bridge.rs"]
mod bridge;
#[path = "codegen.rs"]
mod codegen;
#[path = "layout.rs"]
mod layout;

use super::exec::ExecutableBuffer;
use bridge::pd_vm_cranelift_step;
use codegen::{
    emit_helper_step, emit_inline_ldloc_copy, emit_inline_or_helper_step,
    emit_interrupt_tick_inline, emit_interrupt_tick_inline_guarded, entry_signature,
    helper_signature, jump_with_status, resolve_offsets,
};
use layout::{detect_native_stack_layout, native_layout_fingerprint};

type FuncRef = SigRef;

static CRANELIFT_TRACE_ID: AtomicU64 = AtomicU64::new(1);
static NATIVE_STACK_LAYOUT: OnceLock<Result<NativeStackLayout, String>> = OnceLock::new();
static CRANELIFT_JIT_ISA: OnceLock<Result<OwnedTargetIsa, String>> = OnceLock::new();
static CRANELIFT_AOT_ISA: OnceLock<Result<OwnedTargetIsa, String>> = OnceLock::new();

const OP_LDC: i64 = 1;
const OP_ADD: i64 = 2;
const OP_SUB: i64 = 3;
const OP_MUL: i64 = 4;
const OP_DIV: i64 = 5;
const OP_MOD: i64 = 6;
const OP_SHL: i64 = 7;
const OP_SHR: i64 = 8;
const OP_LSHR: i64 = 9;
const OP_AND: i64 = 10;
const OP_OR: i64 = 11;
const OP_NOT: i64 = 12;
const OP_NEG: i64 = 13;
const OP_CEQ: i64 = 14;
const OP_CLT: i64 = 15;
const OP_CGT: i64 = 16;
const OP_POP: i64 = 17;
const OP_DUP: i64 = 18;
const OP_LDLOC: i64 = 19;
const OP_STLOC: i64 = 20;
const OP_CALL: i64 = 21;
const OP_GUARD_FALSE: i64 = 22;
const OP_JUMP: i64 = 23;
const OP_BUILTIN_CALL: i64 = 24;

fn fused_ldloc_copy_slot(steps: &[TraceStep], index: usize) -> Option<u8> {
    let Some(TraceStep::Ldloc(slot)) = steps.get(index) else {
        return None;
    };
    if !matches!(steps.get(index + 1), Some(TraceStep::Dup)) {
        return None;
    }
    let Some(TraceStep::Stloc(stored_slot)) = steps.get(index + 2) else {
        return None;
    };
    (stored_slot == slot).then_some(*slot)
}

fn fused_ldloc_copy_slot_if_allowed(
    interrupt_settings: Option<NativeInterruptSettings>,
    steps: &[TraceStep],
    index: usize,
) -> Option<u8> {
    if interrupt_settings.is_some() {
        return None;
    }
    fused_ldloc_copy_slot(steps, index)
}

pub(crate) struct CompiledTrace {
    pub(crate) entry: *const u8,
    pub(crate) keepalive: TraceKeepAlive,
    pub(crate) code: Vec<u8>,
}

pub(crate) struct TraceKeepAlive {
    exec: ExecutableBuffer,
}

impl TraceKeepAlive {
    fn from_code(code: &[u8]) -> VmResult<Self> {
        Ok(Self {
            exec: ExecutableBuffer::new(code)?,
        })
    }

    fn entry(&self) -> *const u8 {
        self.exec.entry()
    }
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
    vm_program_constants_ptr_offset: i32,
    vm_program_constants_len_offset: i32,
    vm_ip_offset: i32,
    vm_interrupt_mode_offset: i32,
    vm_fuel_remaining_offset: i32,
    vm_fuel_check_interval_offset: i32,
    vm_fuel_ops_until_check_offset: i32,
    vm_epoch_deadline_offset: i32,
    vm_epoch_counter_ptr_offset: i32,
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
    interrupt_mode: i32,
    fuel_remaining: i32,
    fuel_check_interval: i32,
    fuel_ops_until_check: i32,
    epoch_deadline: i32,
    epoch_counter_ptr: i32,
}

pub(crate) fn helper_entry_address() -> usize {
    pd_vm_cranelift_step as usize
}

pub(crate) fn layout_fingerprint() -> VmResult<u64> {
    native_layout_fingerprint()
}

pub(super) fn native_helper_fn_offset() -> i32 {
    i32::try_from(std::mem::offset_of!(Vm, native_helper_fn))
        .expect("Vm::native_helper_fn offset must fit i32")
}

pub(crate) fn compile_trace(
    trace: &JitTrace,
    interrupt_settings: Option<NativeInterruptSettings>,
    profile: NativeCompileProfile,
) -> VmResult<CompiledTrace> {
    if interrupt_settings.is_some_and(|settings| settings.check_interval == 0) {
        return Err(VmError::InvalidFuelCheckInterval(0));
    }

    let check_runtime_interrupt_settings = trace
        .steps
        .iter()
        .any(|step| matches!(step, TraceStep::Call { .. } | TraceStep::BuiltinCall { .. }));

    let layout = detect_native_stack_layout()?;
    let offsets = resolve_offsets(layout)?;
    let isa = native_isa(profile)?;

    let jit_builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());

    let mut module = JITModule::new(jit_builder);
    let pointer_type = module.target_config().pointer_type();
    let call_conv = module.target_config().default_call_conv;

    let helper_sig = helper_signature(pointer_type, call_conv);

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

        let helper_ref = b.import_signature(helper_sig.clone());

        let mut step_index = 0usize;
        while step_index < trace.steps.len() {
            let step_ip = trace
                .step_ips
                .get(step_index)
                .copied()
                .unwrap_or(trace.root_ip);
            let step_ip_i64 = i64::try_from(step_ip)
                .map_err(|_| VmError::JitNative("step ip out of range for i64".to_string()))?;
            let step_ip_val = b.ins().iconst(pointer_type, step_ip_i64);
            b.ins()
                .store(MemFlags::new(), step_ip_val, vm_ptr, offsets.vm_ip);

            if let Some(settings) = interrupt_settings {
                let stride = settings.check_interval as usize;
                if step_index.is_multiple_of(stride) {
                    let remaining = trace.steps.len().saturating_sub(step_index);
                    let chunk_len = remaining.min(stride) as u32;
                    if check_runtime_interrupt_settings {
                        emit_interrupt_tick_inline_guarded(
                            &mut b, vm_ptr, exit_block, offsets, chunk_len, settings,
                        );
                    } else {
                        emit_interrupt_tick_inline(
                            &mut b, vm_ptr, exit_block, offsets, chunk_len, settings,
                        );
                    }
                }
            }
            if let Some(slot) =
                fused_ldloc_copy_slot_if_allowed(interrupt_settings, &trace.steps, step_index)
            {
                emit_inline_ldloc_copy(
                    &mut b,
                    vm_ptr,
                    helper_ref,
                    exit_block,
                    pointer_type,
                    layout,
                    offsets,
                    slot,
                    trace.root_ip,
                )?;
                step_index += 3;
                continue;
            }

            let step = &trace.steps[step_index];
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
                step_index += 1;
                continue;
            }
            emit_helper_step(&mut b, vm_ptr, helper_ref, exit_block, trace.root_ip, step)?;
            step_index += 1;
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
    let code_len = ctx
        .compiled_code()
        .ok_or_else(|| VmError::JitNative("cranelift trace produced no machine code".to_string()))?
        .code_buffer()
        .len();
    module.clear_context(&mut ctx);
    module
        .finalize_definitions()
        .map_err(|err| VmError::JitNative(format!("finalize cranelift trace failed: {err}")))?;

    let entry = module.get_finalized_function(func_id);
    let code = if code_len == 0 {
        Vec::new()
    } else {
        // SAFETY: `entry` points to a finalized function body and remains valid for the lifetime
        // of `module` until copied out; `code_len` is the exact emitted function size.
        unsafe { std::slice::from_raw_parts(entry, code_len).to_vec() }
    };
    let keepalive = TraceKeepAlive::from_code(&code)?;
    let entry = keepalive.entry();

    Ok(CompiledTrace {
        entry,
        keepalive,
        code,
    })
}

pub(crate) fn load_compiled_trace(code: &[u8]) -> VmResult<CompiledTrace> {
    let keepalive = TraceKeepAlive::from_code(code)?;
    let entry = keepalive.entry();
    Ok(CompiledTrace {
        entry,
        keepalive,
        code: code.to_vec(),
    })
}

fn native_isa(profile: NativeCompileProfile) -> VmResult<OwnedTargetIsa> {
    let (cached, opt_level) = match profile {
        NativeCompileProfile::Jit => (&CRANELIFT_JIT_ISA, "speed"),
        NativeCompileProfile::Aot => (&CRANELIFT_AOT_ISA, "speed_and_size"),
    };
    let cached = cached.get_or_init(|| {
        let mut flag_builder = settings::builder();
        flag_builder
            .set("opt_level", opt_level)
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

    #[test]
    fn fused_ldloc_copy_slot_matches_only_dup_stloc_same_slot() {
        let steps = vec![
            TraceStep::Ldloc(3),
            TraceStep::Dup,
            TraceStep::Stloc(3),
            TraceStep::Add,
        ];
        assert_eq!(fused_ldloc_copy_slot(&steps, 0), Some(3));
        assert_eq!(fused_ldloc_copy_slot(&steps, 1), None);

        let mismatch = vec![TraceStep::Ldloc(3), TraceStep::Dup, TraceStep::Stloc(2)];
        assert_eq!(fused_ldloc_copy_slot(&mismatch, 0), None);

        let wrong_middle = vec![TraceStep::Ldloc(3), TraceStep::Pop, TraceStep::Stloc(3)];
        assert_eq!(fused_ldloc_copy_slot(&wrong_middle, 0), None);
    }

    #[test]
    fn fused_ldloc_copy_slot_is_disabled_when_fuel_metering_is_active() {
        let steps = vec![TraceStep::Ldloc(1), TraceStep::Dup, TraceStep::Stloc(1)];

        assert_eq!(
            fused_ldloc_copy_slot_if_allowed(None, &steps, 0),
            Some(1),
            "fusion should be available without fuel metering"
        );
        assert_eq!(
            fused_ldloc_copy_slot_if_allowed(
                Some(NativeInterruptSettings::fuel(1)),
                &steps,
                0
            ),
            None,
            "fuel metering must disable fused emission"
        );
        assert_eq!(
            fused_ldloc_copy_slot_if_allowed(
                Some(NativeInterruptSettings::epoch(8)),
                &steps,
                0
            ),
            None,
            "all cooperative interruption variants must disable fused emission"
        );
    }
}
