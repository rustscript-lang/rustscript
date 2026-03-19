use super::super::super::{Vm, VmError, VmResult};
use super::super::{JitTrace, TraceBytesCodecKind, TraceConcatKind, TraceStep, TraceTextBytesKind};
use super::NativeCompileProfile;
use crate::vm::native::{
    ExecutableBuffer, NativeInterruptSettings, OP_ADD, OP_AND, OP_BUILTIN_CALL, OP_CALL, OP_CEQ,
    OP_CGT, OP_CLT, OP_DIV, OP_DUP, OP_GUARD_FALSE, OP_GUARD_TRUE, OP_JUMP, OP_LDC, OP_LDLOC,
    OP_LOOP_IF_FALSE, OP_LSHR, OP_MOD, OP_MUL, OP_NEG, OP_NOT, OP_OR, OP_POP, OP_SHL, OP_SHR,
    OP_STLOC, OP_SUB, STATUS_CONTINUE, STATUS_HALTED, STATUS_OUT_OF_FUEL, STATUS_TRACE_EXIT,
    alloc_buffer_signature, alloc_byte_buffer_entry_address, alloc_value_buffer_entry_address,
    copy_bytes_entry_address, copy_bytes_signature, detect_native_stack_layout,
    drop_shared_array_entry_address, drop_shared_bytes_entry_address, drop_shared_signature,
    drop_shared_string_entry_address, entry_signature, free_buffer_signature, helper_signature,
    jump_with_status, pack_shared_signature, shared_array_from_buffer_entry_address,
    shared_bytes_from_buffer_entry_address, shared_string_from_buffer_entry_address,
    zero_bytes_entry_address,
};
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{Block, BlockArg, InstBuilder, MemFlags, SigRef, types};
use cranelift_codegen::isa::OwnedTargetIsa;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module};
use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "codegen.rs"]
mod codegen;

use codegen::{
    HelperEmitCtx, emit_helper_step, emit_inline_or_helper_step, emit_interrupt_tick_inline,
    emit_interrupt_tick_inline_guarded, resolve_offsets,
};

type FuncRef = SigRef;

static CRANELIFT_TRACE_ID: AtomicU64 = AtomicU64::new(1);
static CRANELIFT_JIT_ISA: OnceLock<Result<OwnedTargetIsa, String>> = OnceLock::new();

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
    drop_contract_events: i32,
}

pub(super) fn native_helper_fn_offset() -> i32 {
    i32::try_from(std::mem::offset_of!(Vm, native_helper_fn))
        .expect("Vm::native_helper_fn offset must fit i32")
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
    let alloc_buffer_sig = alloc_buffer_signature(pointer_type, call_conv);
    let free_buffer_sig = free_buffer_signature(pointer_type, call_conv);
    let pack_shared_sig = pack_shared_signature(pointer_type, call_conv);
    let drop_shared_sig = drop_shared_signature(pointer_type, call_conv);
    let copy_bytes_sig = copy_bytes_signature(pointer_type, call_conv);

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
        let root_block = b.create_block();
        let exit_block = b.create_block();
        b.append_block_param(exit_block, types::I32);
        let loop_target_indices_by_step = resolve_loop_target_indices(trace)?;
        let mut step_blocks = HashMap::new();
        step_blocks.insert(0usize, root_block);
        for target_step_index in loop_target_indices_by_step
            .values()
            .copied()
            .collect::<BTreeSet<_>>()
        {
            step_blocks
                .entry(target_step_index)
                .or_insert_with(|| b.create_block());
        }

        b.switch_to_block(entry_block);
        b.append_block_params_for_function_params(entry_block);
        let vm_ptr = b.block_params(entry_block)[0];
        b.ins().jump(root_block, &[]);
        b.switch_to_block(root_block);

        let helper_ref = b.import_signature(helper_sig.clone());
        let vm_status_helper_ref = b.import_signature(entry_signature(pointer_type, call_conv));
        let heap_refs = crate::vm::native::HeapIntrinsicRefs {
            alloc_buffer_ref: b.import_signature(alloc_buffer_sig.clone()),
            free_buffer_ref: b.import_signature(free_buffer_sig.clone()),
            pack_shared_ref: b.import_signature(pack_shared_sig.clone()),
            drop_shared_ref: b.import_signature(drop_shared_sig.clone()),
            copy_bytes_ref: b.import_signature(copy_bytes_sig.clone()),
        };
        let heap_addrs = crate::vm::native::HeapIntrinsicAddrs {
            alloc_byte_buffer: alloc_byte_buffer_entry_address(),
            alloc_value_buffer: alloc_value_buffer_entry_address(),
            pack_string: shared_string_from_buffer_entry_address(),
            pack_bytes: shared_bytes_from_buffer_entry_address(),
            pack_array: shared_array_from_buffer_entry_address(),
            copy_bytes: copy_bytes_entry_address(),
            zero_bytes: zero_bytes_entry_address(),
            drop_string: drop_shared_string_entry_address(),
            drop_bytes: drop_shared_bytes_entry_address(),
            drop_array: drop_shared_array_entry_address(),
        };

        let mut step_index = 0usize;
        while step_index < trace.steps.len() {
            if step_index != 0
                && let Some(&block) = step_blocks.get(&step_index)
            {
                b.ins().jump(block, &[]);
                b.switch_to_block(block);
            }
            let step_ip = trace
                .step_ips
                .get(step_index)
                .copied()
                .unwrap_or(trace.root_ip);

            if let Some(settings) = interrupt_settings {
                let stride = settings.check_interval as usize;
                if step_index.is_multiple_of(stride) {
                    let step_ip_i64 = i64::try_from(step_ip).map_err(|_| {
                        VmError::JitNative("step ip out of range for i64".to_string())
                    })?;
                    let step_ip_val = b.ins().iconst(pointer_type, step_ip_i64);
                    b.ins()
                        .store(MemFlags::new(), step_ip_val, vm_ptr, offsets.vm_ip);
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
            let step = &trace.steps[step_index];
            let loop_target_block = loop_target_indices_by_step
                .get(&step_index)
                .and_then(|target_step_index| step_blocks.get(target_step_index))
                .copied();
            if emit_inline_or_helper_step(
                &mut b,
                vm_ptr,
                helper_ref,
                vm_status_helper_ref,
                exit_block,
                pointer_type,
                layout,
                offsets,
                heap_refs,
                heap_addrs,
                trace.root_ip,
                step_ip,
                step,
                loop_target_block,
                drop_contract_events_enabled,
            )? {
                step_index += 1;
                continue;
            }
            emit_helper_step(
                &mut b,
                HelperEmitCtx {
                    vm_ptr,
                    helper_ref,
                    exit_block,
                    offsets,
                },
                step_ip,
                trace.root_ip,
                step,
            )?;
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

fn resolve_loop_target_indices(trace: &JitTrace) -> VmResult<HashMap<usize, usize>> {
    let step_indices_by_ip = trace
        .step_ips
        .iter()
        .copied()
        .enumerate()
        .map(|(step_index, step_ip)| (step_ip, step_index))
        .collect::<HashMap<_, _>>();
    let mut targets = HashMap::new();
    for (step_index, step) in trace.steps.iter().enumerate() {
        let TraceStep::LoopIfFalse { target_ip, .. } = step else {
            continue;
        };
        let Some(&target_step_index) = step_indices_by_ip.get(target_ip) else {
            return Err(VmError::JitNative(format!(
                "trace {} loop target step_ip {} is missing",
                trace.id, target_ip
            )));
        };
        if target_step_index >= step_index {
            return Err(VmError::JitNative(format!(
                "trace {} loop target step_ip {} must resolve to an earlier step",
                trace.id, target_ip
            )));
        }
        targets.insert(step_index, target_step_index);
    }
    Ok(targets)
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

    #[test]
    fn native_opcodes_remain_stable() {
        assert_eq!(OP_LDLOC, 19);
        assert_eq!(OP_STLOC, 20);
        assert_eq!(OP_LOOP_IF_FALSE, 26);
    }
}
