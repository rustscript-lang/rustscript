use std::fmt;
use std::sync::Arc;

#[cfg(feature = "cranelift-jit")]
use std::collections::HashMap;
#[cfg(feature = "cranelift-jit")]
use std::sync::OnceLock;
#[cfg(feature = "cranelift-jit")]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "cranelift-jit")]
use std::time::Instant;

use crate::vm::native::ExecutableBuffer;
#[cfg(feature = "cranelift-jit")]
use crate::vm::native::{
    HeapIntrinsicAddrs, HeapIntrinsicRefs, OP_BUILTIN_CALL, OP_CALL, STATUS_CONTINUE, STATUS_ERROR,
    alloc_buffer_signature, alloc_byte_buffer_entry_address, alloc_value_buffer_entry_address,
    aot_call_boundary_interrupt_entry_address, box_heap_value_signature,
    clear_value_slot_entry_address, clone_value_signature, clone_value_to_slot_entry_address,
    copy_bytes_entry_address, copy_bytes_signature, detect_native_stack_layout, entry_signature,
    free_buffer_signature, helper_entry_offset, helper_signature,
    init_null_value_slot_entry_address, jump_with_status, pack_shared_signature, resolve_offsets,
    restore_exit_signature, restore_exit_state_entry_address,
    shared_array_from_buffer_entry_address, shared_bytes_from_buffer_entry_address,
    shared_string_from_buffer_entry_address, value_eq_entry_address, value_eq_signature,
    value_slot_signature, write_heap_value_to_slot_entry_address, zero_bytes_entry_address,
};
use crate::vm::{Program, Value, Vm, VmError, VmResult};
#[cfg(feature = "cranelift-jit")]
use cranelift_codegen::ir::condcodes::IntCC;
#[cfg(feature = "cranelift-jit")]
use cranelift_codegen::ir::{
    Block, BlockArg, InstBuilder, MemFlags, StackSlot, StackSlotData, StackSlotKind, types,
};
#[cfg(feature = "cranelift-jit")]
use cranelift_codegen::isa::OwnedTargetIsa;
#[cfg(feature = "cranelift-jit")]
use cranelift_codegen::print_errors::pretty_verifier_error;
#[cfg(feature = "cranelift-jit")]
use cranelift_codegen::settings::{self, Configurable};
#[cfg(feature = "cranelift-jit")]
use cranelift_codegen::verify_function;
#[cfg(feature = "cranelift-jit")]
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Switch};
#[cfg(feature = "cranelift-jit")]
use cranelift_jit::{JITBuilder, JITModule};
#[cfg(feature = "cranelift-jit")]
use cranelift_module::{Linkage, Module};

use super::ir::{AotCallDispatch, AotLowerError};
use super::ssa::{
    AotCheckpoint, AotSsaBuildError, AotSsaInstKind, AotSsaJumpTarget, AotSsaMaterialization,
    AotSsaProgram, AotSsaTerminator, AotSsaValueId, AotSsaValueRepr, build_aot_ssa,
};

#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
pub(crate) type NativeProgramEntry = unsafe extern "C" fn(*mut Vm) -> i32;

#[cfg(not(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
)))]
pub(crate) type NativeProgramEntry = fn(*mut Vm) -> i32;

pub(crate) struct CompiledProgram {
    _keepalive: ProgramKeepAlive,
    pub(crate) entry: NativeProgramEntry,
    pub(crate) code: Arc<[u8]>,
    pub(crate) resume_ips: Arc<[usize]>,
}

struct ProgramKeepAlive {
    exec: ExecutableBuffer,
}

impl ProgramKeepAlive {
    fn from_code(code: &[u8]) -> VmResult<Self> {
        Ok(Self {
            exec: ExecutableBuffer::new(code)?,
        })
    }

    fn entry(&self) -> *const u8 {
        self.exec.entry()
    }
}

impl CompiledProgram {
    pub(crate) fn from_code(code: Vec<u8>, resume_ips: Vec<usize>) -> VmResult<Self> {
        let keepalive = ProgramKeepAlive::from_code(&code)?;
        let entry =
            unsafe { std::mem::transmute::<*const u8, NativeProgramEntry>(keepalive.entry()) };
        Ok(Self {
            _keepalive: keepalive,
            entry,
            code: Arc::<[u8]>::from(code.into_boxed_slice()),
            resume_ips: Arc::<[usize]>::from(resume_ips.into_boxed_slice()),
        })
    }

    pub(crate) fn code_bytes(&self) -> &[u8] {
        self.code.as_ref()
    }
}

#[derive(Debug)]
enum AotCompileError {
    Lower(AotLowerError),
    Ssa(AotSsaBuildError),
    Codegen(String),
}

impl fmt::Display for AotCompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lower(err) => write!(f, "aot lowering failed: {err:?}"),
            Self::Ssa(err) => write!(f, "aot ssa build failed: {err:?}"),
            Self::Codegen(message) => f.write_str(message),
        }
    }
}

impl From<AotLowerError> for AotCompileError {
    fn from(value: AotLowerError) -> Self {
        Self::Lower(value)
    }
}

impl From<AotSsaBuildError> for AotCompileError {
    fn from(value: AotSsaBuildError) -> Self {
        Self::Ssa(value)
    }
}

#[cfg(feature = "cranelift-jit")]
static CRANELIFT_AOT_ID: AtomicU64 = AtomicU64::new(1);
#[cfg(feature = "cranelift-jit")]
static CRANELIFT_AOT_ISA: OnceLock<Result<OwnedTargetIsa, String>> = OnceLock::new();

pub(crate) fn compile_program(program: &Program) -> VmResult<CompiledProgram> {
    compile_program_inner(program).map_err(|err| VmError::JitNative(err.to_string()))
}

#[cfg(feature = "cranelift-jit")]
fn compile_program_inner(program: &Program) -> Result<CompiledProgram, AotCompileError> {
    let trace_enabled = std::env::var_os("PDVM_TRACE_AOT_COMPILE").is_some();
    let build_started = Instant::now();
    let ssa = build_aot_ssa(program)?;
    let build_elapsed = build_started.elapsed();
    if trace_enabled {
        let total_insts = ssa
            .blocks
            .iter()
            .map(|block| block.insts.len())
            .sum::<usize>();
        let external_checkpoints = ssa.checkpoints.iter().filter(|cp| cp.external).count();
        let total_block_params = ssa
            .blocks
            .iter()
            .map(|block| block.params.len())
            .sum::<usize>();
        let max_block_params = ssa
            .blocks
            .iter()
            .map(|block| block.params.len())
            .max()
            .unwrap_or(0);
        let total_checkpoint_values = ssa
            .checkpoints
            .iter()
            .map(|cp| cp.stack.len() + cp.locals.len())
            .sum::<usize>();
        let max_checkpoint_values = ssa
            .checkpoints
            .iter()
            .map(|cp| cp.stack.len() + cp.locals.len())
            .max()
            .unwrap_or(0);
        eprintln!(
            "aot trace: code_bytes={} ssa_blocks={} ssa_insts={} block_params_total={} block_params_max={} checkpoints={} external_checkpoints={} checkpoint_values_total={} checkpoint_values_max={} resume_ips={} ssa_build_us={}",
            program.code.len(),
            ssa.blocks.len(),
            total_insts,
            total_block_params,
            max_block_params,
            ssa.checkpoints.len(),
            external_checkpoints,
            total_checkpoint_values,
            max_checkpoint_values,
            ssa.resume_ips.len(),
            build_elapsed.as_micros(),
        );
    }
    compile_ssa(program, &ssa, trace_enabled)
}

#[cfg(not(feature = "cranelift-jit"))]
fn compile_program_inner(_program: &Program) -> Result<CompiledProgram, AotCompileError> {
    Err(AotCompileError::Codegen(
        "whole-program AOT backend is disabled (feature 'cranelift-jit' is not enabled)"
            .to_string(),
    ))
}

#[cfg(feature = "cranelift-jit")]
#[derive(Clone, Copy)]
struct AotDeoptHelperRefs {
    helper_ref: cranelift_codegen::ir::SigRef,
    vm_status_ref: cranelift_codegen::ir::SigRef,
    interrupt_ref: cranelift_codegen::ir::SigRef,
    clone_value_ref: cranelift_codegen::ir::SigRef,
    value_eq_ref: cranelift_codegen::ir::SigRef,
    init_null_slot_ref: cranelift_codegen::ir::SigRef,
    clear_value_slot_ref: cranelift_codegen::ir::SigRef,
    box_heap_value_ref: cranelift_codegen::ir::SigRef,
    restore_exit_ref: cranelift_codegen::ir::SigRef,
}

#[cfg(feature = "cranelift-jit")]
#[derive(Clone, Copy)]
struct AotDeoptHelperAddrs {
    aot_interrupt: usize,
    clone_value: usize,
    value_eq: usize,
    init_null_slot: usize,
    clear_value_slot: usize,
    box_heap_value: usize,
    restore_exit: usize,
}

#[cfg(feature = "cranelift-jit")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum AotTempValueSlotKey {
    Output(AotSsaValueId),
}

#[cfg(feature = "cranelift-jit")]
struct AotOwnedValueTemps {
    ordered: Vec<StackSlot>,
    slots: HashMap<AotTempValueSlotKey, StackSlot>,
}

#[cfg(feature = "cranelift-jit")]
#[derive(Clone, Copy)]
struct AotLowerCtx<'a> {
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: crate::vm::native::NativeStackLayout,
    offsets: crate::vm::native::ResolvedOffsets,
    heap_refs: HeapIntrinsicRefs,
    heap_addrs: HeapIntrinsicAddrs,
    helper_refs: AotDeoptHelperRefs,
    helper_addrs: AotDeoptHelperAddrs,
    owned_value_temps: &'a AotOwnedValueTemps,
}

#[cfg(feature = "cranelift-jit")]
#[derive(Clone, Copy)]
struct AotTaggedValueOp {
    ip: usize,
    value: cranelift_codegen::ir::Value,
}

#[cfg(feature = "cranelift-jit")]
#[derive(Clone, Copy)]
struct AotBinaryOp {
    ip: usize,
    lhs: cranelift_codegen::ir::Value,
    rhs: cranelift_codegen::ir::Value,
}

#[cfg(feature = "cranelift-jit")]
#[derive(Clone, Copy)]
struct AotBytesIndexOp {
    ip: usize,
    bytes: cranelift_codegen::ir::Value,
    index: cranelift_codegen::ir::Value,
}

#[cfg(feature = "cranelift-jit")]
#[derive(Clone, Copy)]
struct AotStringGetOp {
    output_id: AotSsaValueId,
    ip: usize,
    text: cranelift_codegen::ir::Value,
    index: cranelift_codegen::ir::Value,
}

#[cfg(feature = "cranelift-jit")]
#[derive(Clone, Copy)]
struct AotConcatOp {
    output_id: AotSsaValueId,
    ip: usize,
    lhs: cranelift_codegen::ir::Value,
    rhs: cranelift_codegen::ir::Value,
    result_tag: u32,
    pack_addr: usize,
}

#[cfg(feature = "cranelift-jit")]
#[derive(Clone, Copy)]
struct AotSliceOp {
    output_id: AotSsaValueId,
    ip: usize,
    value: cranelift_codegen::ir::Value,
    start: cranelift_codegen::ir::Value,
    length: cranelift_codegen::ir::Value,
}

#[cfg(feature = "cranelift-jit")]
#[derive(Clone, Copy)]
struct AotArrayConvertOp {
    output_id: AotSsaValueId,
    ip: usize,
    value: cranelift_codegen::ir::Value,
}

#[cfg(feature = "cranelift-jit")]
#[derive(Clone, Copy)]
struct AotStepHelperArgs {
    op: i64,
    a: i64,
    b_arg: i64,
    c: i64,
}

#[cfg(feature = "cranelift-jit")]
#[derive(Clone, Copy)]
struct AotMaterializeCtx<'a> {
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    value_layout: crate::vm::native::ValueLayout,
    helper_refs: AotDeoptHelperRefs,
    helper_addrs: AotDeoptHelperAddrs,
    values: &'a HashMap<AotSsaValueId, cranelift_codegen::ir::Value>,
}

#[cfg(feature = "cranelift-jit")]
fn compile_ssa(
    program: &Program,
    ssa: &AotSsaProgram,
    trace_enabled: bool,
) -> Result<CompiledProgram, AotCompileError> {
    let total_started = Instant::now();
    let isa_started = Instant::now();
    let isa = native_isa()?;
    let isa_elapsed = isa_started.elapsed();
    let module_started = Instant::now();
    let jit_builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    let mut module = JITModule::new(jit_builder);
    let pointer_type = module.target_config().pointer_type();
    let call_conv = module.target_config().default_call_conv;
    let module_elapsed = module_started.elapsed();

    let sigs_started = Instant::now();
    let helper_sig = helper_signature(pointer_type, call_conv);
    let alloc_buffer_sig = alloc_buffer_signature(pointer_type, call_conv);
    let free_buffer_sig = free_buffer_signature(pointer_type, call_conv);
    let pack_shared_sig = pack_shared_signature(pointer_type, call_conv);
    let copy_bytes_sig = copy_bytes_signature(pointer_type, call_conv);
    let vm_status_sig = entry_signature(pointer_type, call_conv);
    let interrupt_sig = entry_signature(pointer_type, call_conv);
    let clone_value_sig = clone_value_signature(pointer_type, call_conv);
    let value_eq_sig = value_eq_signature(pointer_type, call_conv);
    let value_slot_sig = value_slot_signature(pointer_type, call_conv);
    let box_heap_sig = box_heap_value_signature(pointer_type, call_conv);
    let restore_exit_sig = restore_exit_signature(pointer_type, call_conv);
    let sigs_elapsed = sigs_started.elapsed();

    let addr_setup_started = Instant::now();
    let helper_offset = helper_entry_offset();
    let heap_addrs = HeapIntrinsicAddrs {
        alloc_byte_buffer: alloc_byte_buffer_entry_address(),
        alloc_value_buffer: alloc_value_buffer_entry_address(),
        pack_string: shared_string_from_buffer_entry_address(),
        pack_bytes: shared_bytes_from_buffer_entry_address(),
        pack_array: shared_array_from_buffer_entry_address(),
        copy_bytes: copy_bytes_entry_address(),
        zero_bytes: zero_bytes_entry_address(),
    };
    let helper_addrs = AotDeoptHelperAddrs {
        aot_interrupt: aot_call_boundary_interrupt_entry_address(),
        clone_value: clone_value_to_slot_entry_address(),
        value_eq: value_eq_entry_address(),
        init_null_slot: init_null_value_slot_entry_address(),
        clear_value_slot: clear_value_slot_entry_address(),
        box_heap_value: write_heap_value_to_slot_entry_address(),
        restore_exit: restore_exit_state_entry_address(),
    };
    let addr_setup_elapsed = addr_setup_started.elapsed();

    let layout_started = Instant::now();
    let layout = detect_native_stack_layout().map_err(|err| {
        AotCompileError::Codegen(format!("detect native stack layout failed: {err}"))
    })?;
    let offsets = resolve_offsets(layout)
        .map_err(|err| AotCompileError::Codegen(format!("resolve native offsets failed: {err}")))?;
    let layout_elapsed = layout_started.elapsed();

    let ctx_setup_started = Instant::now();
    let mut ctx = module.make_context();
    ctx.func.signature = entry_signature(pointer_type, call_conv);

    let func_id = {
        let id = CRANELIFT_AOT_ID.fetch_add(1, Ordering::Relaxed);
        let name = format!("pd_vm_aot_program_{id}");
        module
            .declare_function(&name, Linkage::Local, &ctx.func.signature)
            .map_err(|err| {
                AotCompileError::Codegen(format!("declare aot function failed: {err}"))
            })?
    };
    let ctx_setup_elapsed = ctx_setup_started.elapsed();

    let vm_ip_offset =
        i32::try_from(std::mem::offset_of!(Vm, ip)).expect("Vm::ip offset must fit i32");
    let code_len_i64 = i64::try_from(program.code.len())
        .map_err(|_| AotCompileError::Codegen("program length does not fit i64".to_string()))?;

    let ir_build_started = Instant::now();
    {
        let mut fb_ctx = FunctionBuilderContext::new();
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fb_ctx);

        let entry_block = b.create_block();
        let dispatch_block = b.create_block();
        let miss_block = b.create_block();
        let exit_block = b.create_block();
        b.append_block_param(exit_block, types::I32);

        let helper_refs = AotDeoptHelperRefs {
            helper_ref: b.import_signature(helper_sig),
            vm_status_ref: b.import_signature(vm_status_sig),
            interrupt_ref: b.import_signature(interrupt_sig),
            clone_value_ref: b.import_signature(clone_value_sig),
            value_eq_ref: b.import_signature(value_eq_sig),
            init_null_slot_ref: b.import_signature(value_slot_sig.clone()),
            clear_value_slot_ref: b.import_signature(value_slot_sig),
            box_heap_value_ref: b.import_signature(box_heap_sig),
            restore_exit_ref: b.import_signature(restore_exit_sig),
        };
        let heap_refs = HeapIntrinsicRefs {
            alloc_buffer_ref: b.import_signature(alloc_buffer_sig),
            free_buffer_ref: b.import_signature(free_buffer_sig),
            pack_shared_ref: b.import_signature(pack_shared_sig),
            copy_bytes_ref: b.import_signature(copy_bytes_sig),
        };

        let mut ssa_blocks = HashMap::new();
        for block in &ssa.blocks {
            let handle = b.create_block();
            for param in &block.params {
                b.append_block_param(handle, ssa_type(pointer_type, param.value.repr)?);
            }
            ssa_blocks.insert(block.id, handle);
        }
        let mut checkpoint_blocks = HashMap::new();
        for checkpoint in &ssa.checkpoints {
            checkpoint_blocks.insert(checkpoint.id, b.create_block());
        }
        let checkpoint_by_id = ssa
            .checkpoints
            .iter()
            .map(|checkpoint| (checkpoint.id, checkpoint))
            .collect::<HashMap<_, _>>();
        let mut value_reprs = HashMap::new();
        for block in &ssa.blocks {
            for param in &block.params {
                value_reprs.insert(param.value.id, param.value.repr);
            }
            for inst in &block.insts {
                value_reprs.insert(inst.output.id, inst.output.repr);
            }
        }
        let owned_value_temps = allocate_owned_value_temps(&mut b, ssa, layout.value.size)?;

        b.switch_to_block(entry_block);
        b.append_block_params_for_function_params(entry_block);
        let vm_ptr = b.block_params(entry_block)[0];
        let lower_ctx = AotLowerCtx {
            vm_ptr,
            exit_block,
            pointer_type,
            layout,
            offsets,
            heap_refs,
            heap_addrs,
            helper_refs,
            helper_addrs,
            owned_value_temps: &owned_value_temps,
        };
        init_owned_value_temps(
            &mut b,
            pointer_type,
            helper_refs,
            helper_addrs,
            &owned_value_temps,
        )?;
        b.ins().jump(dispatch_block, &[]);

        b.switch_to_block(dispatch_block);
        let vm_ip = b
            .ins()
            .load(pointer_type, MemFlags::new(), vm_ptr, vm_ip_offset);
        let mut switch = Switch::new();
        for checkpoint in ssa
            .checkpoints
            .iter()
            .filter(|checkpoint| checkpoint.external)
        {
            switch.set_entry(checkpoint.ip as u128, checkpoint_blocks[&checkpoint.id]);
        }
        switch.emit(&mut b, vm_ip, miss_block);

        b.switch_to_block(miss_block);
        let miss_status = b.ins().iconst(types::I32, STATUS_ERROR as i64);
        jump_with_status(&mut b, exit_block, miss_status);

        for checkpoint in &ssa.checkpoints {
            let handle = checkpoint_blocks[&checkpoint.id];
            b.switch_to_block(handle);
            let args = load_checkpoint_args(
                &mut b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                checkpoint,
            )?;
            b.ins()
                .jump(ssa_blocks[&checkpoint.target], &ssa_block_args(args));
        }

        for block in &ssa.blocks {
            let handle = ssa_blocks[&block.id];
            b.switch_to_block(handle);
            let mut values = HashMap::new();
            for (param, lowered) in block
                .params
                .iter()
                .zip(b.block_params(handle).iter().copied())
            {
                values.insert(param.value.id, lowered);
            }
            for inst in &block.insts {
                let lowered = lower_aot_ssa_inst(&mut b, lower_ctx, inst, &values)?;
                values.insert(inst.output.id, lowered);
            }
            lower_aot_ssa_terminator(
                &mut b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                offsets,
                heap_refs,
                heap_addrs,
                helper_refs,
                helper_addrs,
                helper_offset,
                code_len_i64,
                block.terminator.as_ref().ok_or_else(|| {
                    AotCompileError::Codegen("aot ssa block missing terminator".to_string())
                })?,
                &values,
                &value_reprs,
                &ssa_blocks,
                &ssa.blocks,
                &checkpoint_blocks,
                &checkpoint_by_id,
            )?;
        }

        b.switch_to_block(exit_block);
        let final_status = b.block_params(exit_block)[0];
        clear_owned_value_temps(
            &mut b,
            pointer_type,
            helper_refs,
            helper_addrs,
            &owned_value_temps,
        )?;
        b.ins().return_(&[final_status]);

        b.seal_all_blocks();
        b.finalize();
    }
    let ir_build_elapsed = ir_build_started.elapsed();

    let verify_started = Instant::now();
    if let Err(err) = verify_function(&ctx.func, module.isa()) {
        let pretty = pretty_verifier_error(&ctx.func, None, err);
        return Err(AotCompileError::Codegen(format!(
            "aot ssa verifier failed:\n{pretty}"
        )));
    }
    let verify_elapsed = verify_started.elapsed();
    let define_started = Instant::now();
    module
        .define_function(func_id, &mut ctx)
        .map_err(|err| AotCompileError::Codegen(format!("define aot function failed: {err}")))?;
    let define_elapsed = define_started.elapsed();
    let code_len = ctx
        .compiled_code()
        .ok_or_else(|| {
            AotCompileError::Codegen("aot compile produced no machine code".to_string())
        })?
        .code_buffer()
        .len();
    let clear_ctx_started = Instant::now();
    module.clear_context(&mut ctx);
    let clear_ctx_elapsed = clear_ctx_started.elapsed();
    let finalize_started = Instant::now();
    module.finalize_definitions().map_err(|err| {
        AotCompileError::Codegen(format!("finalize aot definitions failed: {err}"))
    })?;
    let finalize_elapsed = finalize_started.elapsed();

    let copy_started = Instant::now();
    let entry = module.get_finalized_function(func_id);
    let code = if code_len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(entry, code_len).to_vec() }
    };
    let copy_elapsed = copy_started.elapsed();
    let program_wrap_started = Instant::now();
    let compiled = CompiledProgram::from_code(code, ssa.resume_ips.clone())
        .map_err(|err| AotCompileError::Codegen(err.to_string()))?;
    let program_wrap_elapsed = program_wrap_started.elapsed();
    if trace_enabled {
        eprintln!(
            "aot trace: isa_us={} module_us={} sigs_us={} addrs_us={} layout_us={} ctx_us={} ir_build_us={} verify_us={} define_us={} clear_ctx_us={} finalize_us={} copy_us={} wrap_us={} total_codegen_us={} final_code_bytes={}",
            isa_elapsed.as_micros(),
            module_elapsed.as_micros(),
            sigs_elapsed.as_micros(),
            addr_setup_elapsed.as_micros(),
            layout_elapsed.as_micros(),
            ctx_setup_elapsed.as_micros(),
            ir_build_elapsed.as_micros(),
            verify_elapsed.as_micros(),
            define_elapsed.as_micros(),
            clear_ctx_elapsed.as_micros(),
            finalize_elapsed.as_micros(),
            copy_elapsed.as_micros(),
            program_wrap_elapsed.as_micros(),
            total_started.elapsed().as_micros(),
            compiled.code.len(),
        );
    }
    Ok(compiled)
}

#[cfg(feature = "cranelift-jit")]
fn load_checkpoint_args(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: crate::vm::native::NativeStackLayout,
    offsets: crate::vm::native::ResolvedOffsets,
    checkpoint: &AotCheckpoint,
) -> Result<Vec<cranelift_codegen::ir::Value>, AotCompileError> {
    let stack_len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_len);
    let expected_stack_len = b.ins().iconst(
        pointer_type,
        i64::try_from(checkpoint.stack.len()).map_err(|_| {
            AotCompileError::Codegen("checkpoint stack length out of range".to_string())
        })?,
    );
    let stack_ok = b.ins().icmp(IntCC::Equal, stack_len, expected_stack_len);
    let stack_match = b.create_block();
    let stack_error = b.ins().iconst(types::I32, STATUS_ERROR as i64);
    b.ins().brif(
        stack_ok,
        stack_match,
        &[],
        exit_block,
        &[BlockArg::Value(stack_error)],
    );

    b.switch_to_block(stack_match);
    let locals_len = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.locals_len);
    let expected_locals_len = b.ins().iconst(
        pointer_type,
        i64::try_from(checkpoint.locals.len()).map_err(|_| {
            AotCompileError::Codegen("checkpoint locals length out of range".to_string())
        })?,
    );
    let locals_ok = b.ins().icmp(IntCC::Equal, locals_len, expected_locals_len);
    let locals_match = b.create_block();
    let locals_error = b.ins().iconst(types::I32, STATUS_ERROR as i64);
    b.ins().brif(
        locals_ok,
        locals_match,
        &[],
        exit_block,
        &[BlockArg::Value(locals_error)],
    );

    b.switch_to_block(locals_match);
    let stack_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.stack_ptr);
    let locals_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, offsets.locals_ptr);

    let mut args = Vec::with_capacity(checkpoint.stack.len() + checkpoint.locals.len());
    for (index, repr) in checkpoint.stack.iter().copied().enumerate() {
        let index_val = b.ins().iconst(
            pointer_type,
            i64::try_from(index).map_err(|_| {
                AotCompileError::Codegen("checkpoint stack index out of range".to_string())
            })?,
        );
        let addr = ssa_value_addr(b, pointer_type, stack_ptr, index_val, layout.value.size);
        args.push(load_aot_checkpoint_value(
            b,
            pointer_type,
            layout.value,
            addr,
            repr,
        ));
    }
    for (index, repr) in checkpoint.locals.iter().copied().enumerate() {
        let index_val = b.ins().iconst(
            pointer_type,
            i64::try_from(index).map_err(|_| {
                AotCompileError::Codegen("checkpoint local index out of range".to_string())
            })?,
        );
        let addr = ssa_value_addr(b, pointer_type, locals_ptr, index_val, layout.value.size);
        args.push(load_aot_checkpoint_value(
            b,
            pointer_type,
            layout.value,
            addr,
            repr,
        ));
    }
    Ok(args)
}

#[cfg(feature = "cranelift-jit")]
fn load_aot_checkpoint_value(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    layout: crate::vm::native::ValueLayout,
    value_addr: cranelift_codegen::ir::Value,
    repr: AotSsaValueRepr,
) -> cranelift_codegen::ir::Value {
    match repr {
        AotSsaValueRepr::Tagged => value_addr,
        AotSsaValueRepr::I64 => b.ins().load(
            types::I64,
            MemFlags::new(),
            value_addr,
            layout.int_payload_offset,
        ),
        AotSsaValueRepr::F64 => b.ins().load(
            types::F64,
            MemFlags::new(),
            value_addr,
            layout.float_payload_offset,
        ),
        AotSsaValueRepr::Bool => b.ins().load(
            types::I8,
            MemFlags::new(),
            value_addr,
            layout.bool_payload_offset,
        ),
        AotSsaValueRepr::HeapPtr(_) => b.ins().load(
            pointer_type,
            MemFlags::new(),
            value_addr,
            layout.heap_payload_offset,
        ),
    }
}

#[cfg(feature = "cranelift-jit")]
fn lower_aot_ssa_inst(
    b: &mut FunctionBuilder,
    ctx: AotLowerCtx<'_>,
    inst: &super::ssa::AotSsaInst,
    values: &HashMap<AotSsaValueId, cranelift_codegen::ir::Value>,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let AotLowerCtx {
        vm_ptr,
        pointer_type,
        layout,
        offsets,
        heap_addrs,
        ..
    } = ctx;
    let value = match &inst.kind {
        AotSsaInstKind::IntConst(value) => b.ins().iconst(types::I64, *value),
        AotSsaInstKind::FloatConst(value) => b.ins().f64const(*value),
        AotSsaInstKind::BoolConst(value) => b.ins().iconst(types::I8, i64::from(*value as u8)),
        AotSsaInstKind::ConstSlot { index } => {
            let constants_ptr =
                b.ins()
                    .load(pointer_type, MemFlags::new(), vm_ptr, offsets.constants_ptr);
            let idx = b.ins().iconst(pointer_type, i64::from(*index));
            ssa_value_addr(
                b,
                pointer_type,
                constants_ptr,
                idx,
                std::mem::size_of::<Value>() as i32,
            )
        }
        AotSsaInstKind::StringLen { text } => aot_lower_string_len(
            b,
            ctx,
            AotTaggedValueOp {
                ip: inst.ip,
                value: values[text],
            },
        )?,
        AotSsaInstKind::BytesLen { bytes } => aot_lower_bytes_len(
            b,
            ctx,
            AotTaggedValueOp {
                ip: inst.ip,
                value: values[bytes],
            },
        )?,
        AotSsaInstKind::StringSlice {
            text,
            start,
            length,
        } => aot_lower_string_slice(
            b,
            ctx,
            AotSliceOp {
                output_id: inst.output.id,
                ip: inst.ip,
                value: values[text],
                start: values[start],
                length: values[length],
            },
        )?,
        AotSsaInstKind::BytesSlice {
            bytes,
            start,
            length,
        } => aot_lower_bytes_slice(
            b,
            ctx,
            AotSliceOp {
                output_id: inst.output.id,
                ip: inst.ip,
                value: values[bytes],
                start: values[start],
                length: values[length],
            },
        )?,
        AotSsaInstKind::StringGet { text, index } => aot_lower_string_get(
            b,
            ctx,
            AotStringGetOp {
                output_id: inst.output.id,
                ip: inst.ip,
                text: values[text],
                index: values[index],
            },
        )?,
        AotSsaInstKind::BytesGet { bytes, index } => aot_lower_bytes_get(
            b,
            ctx,
            AotBytesIndexOp {
                ip: inst.ip,
                bytes: values[bytes],
                index: values[index],
            },
        )?,
        AotSsaInstKind::BytesHas { bytes, index } => aot_lower_bytes_has(
            b,
            ctx,
            AotBytesIndexOp {
                ip: inst.ip,
                bytes: values[bytes],
                index: values[index],
            },
        )?,
        AotSsaInstKind::StringConcat { lhs, rhs } => aot_lower_concat(
            b,
            ctx,
            AotConcatOp {
                output_id: inst.output.id,
                ip: inst.ip,
                lhs: values[lhs],
                rhs: values[rhs],
                result_tag: layout.value.string_tag,
                pack_addr: heap_addrs.pack_string,
            },
        )?,
        AotSsaInstKind::BytesConcat { lhs, rhs } => aot_lower_concat(
            b,
            ctx,
            AotConcatOp {
                output_id: inst.output.id,
                ip: inst.ip,
                lhs: values[lhs],
                rhs: values[rhs],
                result_tag: layout.value.bytes_tag,
                pack_addr: heap_addrs.pack_bytes,
            },
        )?,
        AotSsaInstKind::BytesFromArrayU8 { array } => aot_lower_bytes_from_array_u8(
            b,
            ctx,
            AotArrayConvertOp {
                output_id: inst.output.id,
                ip: inst.ip,
                value: values[array],
            },
        )?,
        AotSsaInstKind::BytesToArrayU8 { bytes } => aot_lower_bytes_to_array_u8(
            b,
            ctx,
            AotArrayConvertOp {
                output_id: inst.output.id,
                ip: inst.ip,
                value: values[bytes],
            },
        )?,
        AotSsaInstKind::IntAdd { lhs, rhs } => b.ins().iadd(values[lhs], values[rhs]),
        AotSsaInstKind::IntSub { lhs, rhs } => b.ins().isub(values[lhs], values[rhs]),
        AotSsaInstKind::IntMul { lhs, rhs } => b.ins().imul(values[lhs], values[rhs]),
        AotSsaInstKind::IntDiv { lhs, rhs } => aot_lower_int_divrem(
            b,
            ctx,
            AotBinaryOp {
                ip: inst.ip,
                lhs: values[lhs],
                rhs: values[rhs],
            },
            false,
        ),
        AotSsaInstKind::IntMod { lhs, rhs } => aot_lower_int_divrem(
            b,
            ctx,
            AotBinaryOp {
                ip: inst.ip,
                lhs: values[lhs],
                rhs: values[rhs],
            },
            true,
        ),
        AotSsaInstKind::IntShl { lhs, rhs } => b.ins().ishl(values[lhs], values[rhs]),
        AotSsaInstKind::IntShr { lhs, rhs } => b.ins().sshr(values[lhs], values[rhs]),
        AotSsaInstKind::IntLshr { lhs, rhs } => b.ins().ushr(values[lhs], values[rhs]),
        AotSsaInstKind::FloatAdd { lhs, rhs } => b.ins().fadd(values[lhs], values[rhs]),
        AotSsaInstKind::FloatSub { lhs, rhs } => b.ins().fsub(values[lhs], values[rhs]),
        AotSsaInstKind::FloatMul { lhs, rhs } => b.ins().fmul(values[lhs], values[rhs]),
        AotSsaInstKind::FloatDiv { lhs, rhs } => b.ins().fdiv(values[lhs], values[rhs]),
        AotSsaInstKind::FloatMod { lhs, rhs } => {
            let quotient = b.ins().fdiv(values[lhs], values[rhs]);
            let truncated = b.ins().trunc(quotient);
            let product = b.ins().fmul(truncated, values[rhs]);
            b.ins().fsub(values[lhs], product)
        }
        AotSsaInstKind::BoolAnd { lhs, rhs } => b.ins().band(values[lhs], values[rhs]),
        AotSsaInstKind::BoolOr { lhs, rhs } => b.ins().bor(values[lhs], values[rhs]),
        AotSsaInstKind::BoolNot { input } => b.ins().icmp_imm(IntCC::Equal, values[input], 0),
        AotSsaInstKind::TaggedToInt { input } => aot_lower_tagged_to_int(
            b,
            ctx,
            AotTaggedValueOp {
                ip: inst.ip,
                value: values[input],
            },
        )?,
        AotSsaInstKind::TaggedNumberToFloat { input } => aot_lower_tagged_number_to_float(
            b,
            ctx,
            AotTaggedValueOp {
                ip: inst.ip,
                value: values[input],
            },
        )?,
        AotSsaInstKind::IntToFloat { input } => b.ins().fcvt_from_sint(types::F64, values[input]),
        AotSsaInstKind::IntNeg { input } => b.ins().ineg(values[input]),
        AotSsaInstKind::FloatNeg { input } => b.ins().fneg(values[input]),
        AotSsaInstKind::IntCmpEq { lhs, rhs } => {
            b.ins().icmp(IntCC::Equal, values[lhs], values[rhs])
        }
        AotSsaInstKind::IntCmpLt { lhs, rhs } => {
            b.ins()
                .icmp(IntCC::SignedLessThan, values[lhs], values[rhs])
        }
        AotSsaInstKind::IntCmpGt { lhs, rhs } => {
            b.ins()
                .icmp(IntCC::SignedGreaterThan, values[lhs], values[rhs])
        }
        AotSsaInstKind::BoolCmpEq { lhs, rhs } => {
            b.ins().icmp(IntCC::Equal, values[lhs], values[rhs])
        }
        AotSsaInstKind::TaggedCmpEq { lhs, rhs } => aot_lower_tagged_eq(
            b,
            ctx,
            AotBinaryOp {
                ip: inst.ip,
                lhs: values[lhs],
                rhs: values[rhs],
            },
        )?,
        AotSsaInstKind::StringCmpEq { lhs, rhs } => aot_lower_tagged_bytes_eq(
            b,
            ctx,
            AotBinaryOp {
                ip: inst.ip,
                lhs: values[lhs],
                rhs: values[rhs],
            },
            layout.value.string_tag,
        )?,
        AotSsaInstKind::BytesCmpEq { lhs, rhs } => aot_lower_tagged_bytes_eq(
            b,
            ctx,
            AotBinaryOp {
                ip: inst.ip,
                lhs: values[lhs],
                rhs: values[rhs],
            },
            layout.value.bytes_tag,
        )?,
        AotSsaInstKind::NullCmpEq { lhs, rhs } => aot_lower_null_eq(
            b,
            ctx,
            AotBinaryOp {
                ip: inst.ip,
                lhs: values[lhs],
                rhs: values[rhs],
            },
        )?,
        AotSsaInstKind::FloatCmpEq { lhs, rhs } => b.ins().fcmp(
            cranelift_codegen::ir::condcodes::FloatCC::Equal,
            values[lhs],
            values[rhs],
        ),
        AotSsaInstKind::FloatCmpLt { lhs, rhs } => b.ins().fcmp(
            cranelift_codegen::ir::condcodes::FloatCC::LessThan,
            values[lhs],
            values[rhs],
        ),
        AotSsaInstKind::FloatCmpGt { lhs, rhs } => b.ins().fcmp(
            cranelift_codegen::ir::condcodes::FloatCC::GreaterThan,
            values[lhs],
            values[rhs],
        ),
    };
    Ok(value)
}

#[cfg(feature = "cranelift-jit")]
#[allow(clippy::too_many_arguments)]
fn lower_aot_ssa_terminator(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: crate::vm::native::NativeStackLayout,
    offsets: crate::vm::native::ResolvedOffsets,
    _heap_refs: HeapIntrinsicRefs,
    _heap_addrs: HeapIntrinsicAddrs,
    helper_refs: AotDeoptHelperRefs,
    helper_addrs: AotDeoptHelperAddrs,
    helper_offset: i32,
    _code_len_i64: i64,
    terminator: &AotSsaTerminator,
    values: &HashMap<AotSsaValueId, cranelift_codegen::ir::Value>,
    value_reprs: &HashMap<AotSsaValueId, AotSsaValueRepr>,
    ssa_blocks: &HashMap<super::ssa::AotSsaBlockId, Block>,
    ssa_ir_blocks: &[super::ssa::AotSsaBlock],
    _checkpoint_blocks: &HashMap<super::ssa::AotCheckpointId, Block>,
    _checkpoint_by_id: &HashMap<super::ssa::AotCheckpointId, &AotCheckpoint>,
) -> Result<(), AotCompileError> {
    match terminator {
        AotSsaTerminator::Jump(target) => {
            let args = resolve_jump_args(
                b,
                pointer_type,
                layout.value,
                target,
                values,
                value_reprs,
                &ssa_ir_blocks[target.target.index()].params,
            )?;
            b.ins()
                .jump(ssa_blocks[&target.target], &ssa_block_args(args));
        }
        AotSsaTerminator::BranchBool {
            condition,
            if_true,
            if_false,
        } => {
            let raw_condition = *values.get(condition).ok_or_else(|| {
                AotCompileError::Codegen("missing branch condition value".to_string())
            })?;
            let condition = b.ins().icmp_imm(IntCC::NotEqual, raw_condition, 0);
            let true_args = resolve_jump_args(
                b,
                pointer_type,
                layout.value,
                if_true,
                values,
                value_reprs,
                &ssa_ir_blocks[if_true.target.index()].params,
            )?;
            let false_args = resolve_jump_args(
                b,
                pointer_type,
                layout.value,
                if_false,
                values,
                value_reprs,
                &ssa_ir_blocks[if_false.target.index()].params,
            )?;
            b.ins().brif(
                condition,
                ssa_blocks[&if_true.target],
                &ssa_block_args(true_args),
                ssa_blocks[&if_false.target],
                &ssa_block_args(false_args),
            );
        }
        AotSsaTerminator::CallBoundary {
            call,
            stack,
            locals,
        } => {
            materialize_state_to_vm(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                helper_refs,
                helper_addrs,
                stack,
                locals,
                values,
                call.call_ip,
            )?;
            emit_call_boundary_interrupt(
                b,
                vm_ptr,
                helper_refs.interrupt_ref,
                helper_addrs.aot_interrupt,
                pointer_type,
                exit_block,
            )?;
            let op = match call.dispatch {
                AotCallDispatch::Builtin => OP_BUILTIN_CALL,
                AotCallDispatch::HostImport => OP_CALL,
            };
            let status = call_step_helper(
                b,
                vm_ptr,
                helper_refs.helper_ref,
                helper_offset,
                pointer_type,
                AotStepHelperArgs {
                    op,
                    a: i64::from(call.index),
                    b_arg: i64::from(call.argc),
                    c: i64::try_from(call.call_ip).map_err(|_| {
                        AotCompileError::Codegen("call ip does not fit i64".to_string())
                    })?,
                },
            )?;
            let is_continue = b
                .ins()
                .icmp_imm(IntCC::Equal, status, STATUS_CONTINUE as i64);
            let cont = b.create_block();
            b.ins().brif(
                is_continue,
                cont,
                &[],
                exit_block,
                &[BlockArg::Value(status)],
            );
            b.switch_to_block(cont);
            store_vm_ip(
                b,
                vm_ptr,
                pointer_type,
                offsets.vm_ip,
                i64::try_from(call.resume_ip).map_err(|_| {
                    AotCompileError::Codegen("call resume ip does not fit i64".to_string())
                })?,
            );
            jump_with_status(b, exit_block, status);
        }
        AotSsaTerminator::Return { ip, stack, locals } => {
            materialize_state_to_vm(
                b,
                vm_ptr,
                exit_block,
                pointer_type,
                layout,
                helper_refs,
                helper_addrs,
                stack,
                locals,
                values,
                *ip,
            )?;
            let halted = b
                .ins()
                .iconst(types::I32, crate::vm::native::STATUS_HALTED as i64);
            jump_with_status(b, exit_block, halted);
        }
        AotSsaTerminator::Stop { ip } => {
            store_vm_ip(
                b,
                vm_ptr,
                pointer_type,
                offsets.vm_ip,
                i64::try_from(*ip).map_err(|_| {
                    AotCompileError::Codegen("stop ip does not fit i64".to_string())
                })?,
            );
            let status = b.ins().iconst(types::I32, STATUS_ERROR as i64);
            jump_with_status(b, exit_block, status);
        }
    }
    Ok(())
}

#[cfg(feature = "cranelift-jit")]
fn aot_inst_requires_owned_value_slot(kind: &AotSsaInstKind) -> bool {
    matches!(
        kind,
        AotSsaInstKind::StringSlice { .. }
            | AotSsaInstKind::BytesSlice { .. }
            | AotSsaInstKind::StringGet { .. }
            | AotSsaInstKind::StringConcat { .. }
            | AotSsaInstKind::BytesConcat { .. }
            | AotSsaInstKind::BytesFromArrayU8 { .. }
            | AotSsaInstKind::BytesToArrayU8 { .. }
    )
}

#[cfg(feature = "cranelift-jit")]
fn allocate_owned_value_temps(
    b: &mut FunctionBuilder,
    ssa: &AotSsaProgram,
    value_size: i32,
) -> Result<AotOwnedValueTemps, AotCompileError> {
    let mut ordered = Vec::new();
    let mut slots = HashMap::new();
    for block in &ssa.blocks {
        for inst in &block.insts {
            if aot_inst_requires_owned_value_slot(&inst.kind) {
                let slot = aot_create_value_stack_slot(b, value_size)?;
                ordered.push(slot);
                slots.insert(AotTempValueSlotKey::Output(inst.output.id), slot);
            }
        }
    }
    Ok(AotOwnedValueTemps { ordered, slots })
}

#[cfg(feature = "cranelift-jit")]
fn aot_create_value_stack_slot(
    b: &mut FunctionBuilder,
    value_size: i32,
) -> Result<StackSlot, AotCompileError> {
    let bytes = u32::try_from(value_size)
        .map_err(|_| AotCompileError::Codegen("AOT value slot size out of range".to_string()))?;
    let align_shift = std::mem::align_of::<Value>().trailing_zeros() as u8;
    Ok(b.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        bytes,
        align_shift,
    )))
}

#[cfg(feature = "cranelift-jit")]
fn init_owned_value_temps(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    helper_refs: AotDeoptHelperRefs,
    helper_addrs: AotDeoptHelperAddrs,
    temps: &AotOwnedValueTemps,
) -> Result<(), AotCompileError> {
    for slot in &temps.ordered {
        let addr = b.ins().stack_addr(pointer_type, *slot, 0);
        aot_call_infallible_helper(
            b,
            pointer_type,
            helper_refs.init_null_slot_ref,
            helper_addrs.init_null_slot,
            &[addr],
        )?;
    }
    Ok(())
}

#[cfg(feature = "cranelift-jit")]
fn clear_owned_value_temps(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    helper_refs: AotDeoptHelperRefs,
    helper_addrs: AotDeoptHelperAddrs,
    temps: &AotOwnedValueTemps,
) -> Result<(), AotCompileError> {
    for slot in &temps.ordered {
        let addr = b.ins().stack_addr(pointer_type, *slot, 0);
        aot_call_infallible_helper(
            b,
            pointer_type,
            helper_refs.clear_value_slot_ref,
            helper_addrs.clear_value_slot,
            &[addr],
        )?;
    }
    Ok(())
}

#[cfg(feature = "cranelift-jit")]
fn owned_value_temp_slot_addr(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    temps: &AotOwnedValueTemps,
    key: AotTempValueSlotKey,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let slot = temps.slots.get(&key).copied().ok_or_else(|| {
        AotCompileError::Codegen(format!("AOT temp value slot missing for {key:?}"))
    })?;
    Ok(b.ins().stack_addr(pointer_type, slot, 0))
}

#[cfg(feature = "cranelift-jit")]
fn clear_owned_value_temp_slot(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    helper_refs: AotDeoptHelperRefs,
    helper_addrs: AotDeoptHelperAddrs,
    addr: cranelift_codegen::ir::Value,
) -> Result<(), AotCompileError> {
    aot_call_infallible_helper(
        b,
        pointer_type,
        helper_refs.clear_value_slot_ref,
        helper_addrs.clear_value_slot,
        &[addr],
    )
}

#[cfg(feature = "cranelift-jit")]
fn aot_call_infallible_helper(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    helper_ref: cranelift_codegen::ir::SigRef,
    helper_addr: usize,
    args: &[cranelift_codegen::ir::Value],
) -> Result<(), AotCompileError> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, helper_addr)?;
    let _ = b.ins().call_indirect(helper_ref, helper_ptr, args);
    Ok(())
}

#[cfg(feature = "cranelift-jit")]
fn aot_emit_error_status(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    offsets: crate::vm::native::ResolvedOffsets,
    ip: usize,
) -> Result<(), AotCompileError> {
    store_vm_ip(
        b,
        vm_ptr,
        pointer_type,
        offsets.vm_ip,
        i64::try_from(ip)
            .map_err(|_| AotCompileError::Codegen("error ip does not fit i64".to_string()))?,
    );
    let status = b.ins().iconst(types::I32, STATUS_ERROR as i64);
    jump_with_status(b, exit_block, status);
    Ok(())
}

#[cfg(feature = "cranelift-jit")]
fn aot_load_checked_heap_ptr(
    b: &mut FunctionBuilder,
    ctx: AotLowerCtx<'_>,
    operand: AotTaggedValueOp,
    expected_tag: u32,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let AotLowerCtx {
        vm_ptr,
        exit_block,
        pointer_type,
        layout,
        offsets,
        ..
    } = ctx;
    let AotTaggedValueOp {
        ip,
        value: tagged_value,
    } = operand;
    let ok = b.create_block();
    let fail = b.create_block();
    let cont = b.create_block();
    b.append_block_param(cont, pointer_type);

    let tag = ssa_load_tag_i32(b, layout.value, tagged_value);
    let matches = b.ins().icmp_imm(IntCC::Equal, tag, i64::from(expected_tag));
    b.ins().brif(matches, ok, &[], fail, &[]);

    b.switch_to_block(ok);
    let raw = ssa_load_heap_ptr(b, layout.value, tagged_value, pointer_type);
    b.ins().jump(cont, &[BlockArg::Value(raw)]);

    b.switch_to_block(fail);
    aot_emit_error_status(b, vm_ptr, exit_block, pointer_type, offsets, ip)?;

    b.switch_to_block(cont);
    Ok(b.block_params(cont)[0])
}

#[cfg(feature = "cranelift-jit")]
fn aot_lower_tagged_to_int(
    b: &mut FunctionBuilder,
    ctx: AotLowerCtx<'_>,
    operand: AotTaggedValueOp,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let AotLowerCtx {
        vm_ptr,
        exit_block,
        pointer_type,
        layout,
        offsets,
        ..
    } = ctx;
    let AotTaggedValueOp {
        ip,
        value: tagged_value,
    } = operand;
    let ok = b.create_block();
    let fail = b.create_block();
    let cont = b.create_block();
    b.append_block_param(cont, types::I64);

    let tag = ssa_load_tag_i32(b, layout.value, tagged_value);
    let is_int = b
        .ins()
        .icmp_imm(IntCC::Equal, tag, i64::from(layout.value.int_tag));
    b.ins().brif(is_int, ok, &[], fail, &[]);

    b.switch_to_block(ok);
    let value = b.ins().load(
        types::I64,
        MemFlags::new(),
        tagged_value,
        layout.value.int_payload_offset,
    );
    b.ins().jump(cont, &[BlockArg::Value(value)]);

    b.switch_to_block(fail);
    aot_emit_error_status(b, vm_ptr, exit_block, pointer_type, offsets, ip)?;

    b.switch_to_block(cont);
    Ok(b.block_params(cont)[0])
}

#[cfg(feature = "cranelift-jit")]
fn aot_lower_tagged_number_to_float(
    b: &mut FunctionBuilder,
    ctx: AotLowerCtx<'_>,
    operand: AotTaggedValueOp,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let AotLowerCtx {
        vm_ptr,
        exit_block,
        pointer_type,
        layout,
        offsets,
        ..
    } = ctx;
    let AotTaggedValueOp {
        ip,
        value: tagged_value,
    } = operand;
    let check_float = b.create_block();
    let int_ok = b.create_block();
    let float_ok = b.create_block();
    let fail = b.create_block();
    let cont = b.create_block();
    b.append_block_param(cont, types::F64);

    let tag = ssa_load_tag_i32(b, layout.value, tagged_value);
    let is_int = b
        .ins()
        .icmp_imm(IntCC::Equal, tag, i64::from(layout.value.int_tag));
    b.ins().brif(is_int, int_ok, &[], check_float, &[]);

    b.switch_to_block(check_float);
    let is_float = b
        .ins()
        .icmp_imm(IntCC::Equal, tag, i64::from(layout.value.float_tag));
    b.ins().brif(is_float, float_ok, &[], fail, &[]);

    b.switch_to_block(int_ok);
    let int_value = b.ins().load(
        types::I64,
        MemFlags::new(),
        tagged_value,
        layout.value.int_payload_offset,
    );
    let float_from_int = b.ins().fcvt_from_sint(types::F64, int_value);
    b.ins().jump(cont, &[BlockArg::Value(float_from_int)]);

    b.switch_to_block(float_ok);
    let float_value = b.ins().load(
        types::F64,
        MemFlags::new(),
        tagged_value,
        layout.value.float_payload_offset,
    );
    b.ins().jump(cont, &[BlockArg::Value(float_value)]);

    b.switch_to_block(fail);
    aot_emit_error_status(b, vm_ptr, exit_block, pointer_type, offsets, ip)?;

    b.switch_to_block(cont);
    Ok(b.block_params(cont)[0])
}

#[cfg(feature = "cranelift-jit")]
fn aot_lower_string_len(
    b: &mut FunctionBuilder,
    ctx: AotLowerCtx<'_>,
    operand: AotTaggedValueOp,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let AotLowerCtx {
        pointer_type,
        layout,
        ..
    } = ctx;
    let string_raw = aot_load_checked_heap_ptr(b, ctx, operand, layout.value.string_tag)?;
    let string_data = ssa_load_heap_data_ptr(b, layout.value, string_raw);
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
    Ok(b.block_params(done_block)[0])
}

#[cfg(feature = "cranelift-jit")]
fn aot_lower_bytes_len(
    b: &mut FunctionBuilder,
    ctx: AotLowerCtx<'_>,
    operand: AotTaggedValueOp,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let AotLowerCtx {
        pointer_type,
        layout,
        ..
    } = ctx;
    let bytes_raw = aot_load_checked_heap_ptr(b, ctx, operand, layout.value.bytes_tag)?;
    let vec_ptr = ssa_load_heap_data_ptr(b, layout.value, bytes_raw);
    Ok(b.ins().load(
        pointer_type,
        MemFlags::new(),
        vec_ptr,
        layout.stack_vec.len_offset,
    ))
}

#[cfg(feature = "cranelift-jit")]
fn aot_lower_bytes_get(
    b: &mut FunctionBuilder,
    ctx: AotLowerCtx<'_>,
    op: AotBytesIndexOp,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let AotLowerCtx {
        vm_ptr,
        exit_block,
        pointer_type,
        layout,
        offsets,
        ..
    } = ctx;
    let AotBytesIndexOp { ip, bytes, index } = op;
    let bytes_raw = aot_load_checked_heap_ptr(
        b,
        ctx,
        AotTaggedValueOp { ip, value: bytes },
        layout.value.bytes_tag,
    )?;
    let vec_ptr = ssa_load_heap_data_ptr(b, layout.value, bytes_raw);
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
    b.append_block_param(cont, types::I64);
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
    b.ins().jump(cont, &[BlockArg::Value(out)]);

    b.switch_to_block(fail);
    aot_emit_error_status(b, vm_ptr, exit_block, pointer_type, offsets, ip)?;

    b.switch_to_block(cont);
    Ok(b.block_params(cont)[0])
}

#[cfg(feature = "cranelift-jit")]
fn aot_lower_bytes_has(
    b: &mut FunctionBuilder,
    ctx: AotLowerCtx<'_>,
    op: AotBytesIndexOp,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let AotLowerCtx {
        layout,
        pointer_type,
        ..
    } = ctx;
    let AotBytesIndexOp { ip, bytes, index } = op;
    let bytes_raw = aot_load_checked_heap_ptr(
        b,
        ctx,
        AotTaggedValueOp { ip, value: bytes },
        layout.value.bytes_tag,
    )?;
    let vec_ptr = ssa_load_heap_data_ptr(b, layout.value, bytes_raw);
    let len = b.ins().load(
        pointer_type,
        MemFlags::new(),
        vec_ptr,
        layout.stack_vec.len_offset,
    );
    Ok(ssa_index_in_range(b, index, len))
}

#[cfg(feature = "cranelift-jit")]
fn aot_lower_int_divrem(
    b: &mut FunctionBuilder,
    ctx: AotLowerCtx<'_>,
    op: AotBinaryOp,
    is_mod: bool,
) -> cranelift_codegen::ir::Value {
    let AotLowerCtx {
        vm_ptr,
        exit_block,
        pointer_type,
        offsets,
        ..
    } = ctx;
    let AotBinaryOp { ip, lhs, rhs } = op;
    let non_zero = b.ins().icmp_imm(IntCC::NotEqual, rhs, 0);
    let check_overflow = b.create_block();
    let fail = b.create_block();
    let cont = b.create_block();
    b.append_block_param(cont, types::I64);
    b.ins().brif(non_zero, check_overflow, &[], fail, &[]);

    b.switch_to_block(check_overflow);
    let rhs_is_neg_one = b.ins().icmp_imm(IntCC::Equal, rhs, -1);
    let lhs_is_min = b.ins().icmp_imm(IntCC::Equal, lhs, i64::MIN);
    let overflow = b.ins().band(rhs_is_neg_one, lhs_is_min);
    let ok = b.create_block();
    b.ins().brif(overflow, fail, &[], ok, &[]);

    b.switch_to_block(ok);
    let out = if is_mod {
        b.ins().srem(lhs, rhs)
    } else {
        b.ins().sdiv(lhs, rhs)
    };
    b.ins().jump(cont, &[BlockArg::Value(out)]);

    b.switch_to_block(fail);
    let _ = aot_emit_error_status(b, vm_ptr, exit_block, pointer_type, offsets, ip);

    b.switch_to_block(cont);
    b.block_params(cont)[0]
}

#[cfg(feature = "cranelift-jit")]
fn aot_lower_tagged_eq(
    b: &mut FunctionBuilder,
    ctx: AotLowerCtx<'_>,
    op: AotBinaryOp,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let AotLowerCtx {
        vm_ptr,
        exit_block,
        pointer_type,
        offsets,
        helper_refs,
        helper_addrs,
        ..
    } = ctx;
    let AotBinaryOp { ip, lhs, rhs } = op;
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, helper_addrs.value_eq)?;
    let call = b
        .ins()
        .call_indirect(helper_refs.value_eq_ref, helper_ptr, &[lhs, rhs]);
    let result = b.inst_results(call)[0];
    let is_error = b
        .ins()
        .icmp_imm(IntCC::Equal, result, i64::from(STATUS_ERROR));
    let fail = b.create_block();
    let ok = b.create_block();
    let done = b.create_block();
    b.append_block_param(done, types::I8);
    b.ins().brif(is_error, fail, &[], ok, &[]);

    b.switch_to_block(fail);
    aot_emit_error_status(b, vm_ptr, exit_block, pointer_type, offsets, ip)?;

    b.switch_to_block(ok);
    let equal = b.ins().icmp_imm(IntCC::NotEqual, result, 0);
    b.ins().jump(done, &[BlockArg::Value(equal)]);

    b.switch_to_block(done);
    Ok(b.block_params(done)[0])
}

#[cfg(feature = "cranelift-jit")]
fn aot_lower_tagged_bytes_eq(
    b: &mut FunctionBuilder,
    ctx: AotLowerCtx<'_>,
    op: AotBinaryOp,
    expected_tag: u32,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let AotLowerCtx {
        pointer_type,
        layout,
        ..
    } = ctx;
    let AotBinaryOp { ip, lhs, rhs } = op;
    let lhs_raw =
        aot_load_checked_heap_ptr(b, ctx, AotTaggedValueOp { ip, value: lhs }, expected_tag)?;
    let rhs_raw =
        aot_load_checked_heap_ptr(b, ctx, AotTaggedValueOp { ip, value: rhs }, expected_tag)?;
    let lhs_data = ssa_load_heap_data_ptr(b, layout.value, lhs_raw);
    let rhs_data = ssa_load_heap_data_ptr(b, layout.value, rhs_raw);
    let lhs_ptr = b.ins().load(
        pointer_type,
        MemFlags::new(),
        lhs_data,
        layout.stack_vec.ptr_offset,
    );
    let rhs_ptr = b.ins().load(
        pointer_type,
        MemFlags::new(),
        rhs_data,
        layout.stack_vec.ptr_offset,
    );
    let lhs_len = b.ins().load(
        pointer_type,
        MemFlags::new(),
        lhs_data,
        layout.stack_vec.len_offset,
    );
    let rhs_len = b.ins().load(
        pointer_type,
        MemFlags::new(),
        rhs_data,
        layout.stack_vec.len_offset,
    );
    let same_len = b.ins().icmp(IntCC::Equal, lhs_len, rhs_len);
    let loop_block = b.create_block();
    let step_block = b.create_block();
    let false_block = b.create_block();
    let done_block = b.create_block();
    b.append_block_param(loop_block, pointer_type);
    b.append_block_param(done_block, types::I8);

    let zero = b.ins().iconst(pointer_type, 0);
    let one = b.ins().iconst(types::I8, 1);
    let zero_bool = b.ins().iconst(types::I8, 0);
    b.ins().brif(
        same_len,
        loop_block,
        &[BlockArg::Value(zero)],
        done_block,
        &[BlockArg::Value(zero_bool)],
    );

    b.switch_to_block(loop_block);
    let index = b.block_params(loop_block)[0];
    let done = b
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, index, lhs_len);
    b.ins()
        .brif(done, done_block, &[BlockArg::Value(one)], step_block, &[]);

    b.switch_to_block(step_block);
    let lhs_byte_ptr = b.ins().iadd(lhs_ptr, index);
    let rhs_byte_ptr = b.ins().iadd(rhs_ptr, index);
    let lhs_byte = ssa_load_byte(b, lhs_byte_ptr);
    let rhs_byte = ssa_load_byte(b, rhs_byte_ptr);
    let equal = b.ins().icmp(IntCC::Equal, lhs_byte, rhs_byte);
    b.ins().brif(
        equal,
        false_block,
        &[],
        done_block,
        &[BlockArg::Value(zero_bool)],
    );

    b.switch_to_block(false_block);
    let next_index = b.ins().iadd_imm(index, 1);
    b.ins().jump(loop_block, &[BlockArg::Value(next_index)]);

    b.switch_to_block(done_block);
    Ok(b.block_params(done_block)[0])
}

#[cfg(feature = "cranelift-jit")]
fn aot_lower_null_eq(
    b: &mut FunctionBuilder,
    ctx: AotLowerCtx<'_>,
    op: AotBinaryOp,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let AotLowerCtx {
        vm_ptr,
        exit_block,
        pointer_type,
        layout,
        offsets,
        ..
    } = ctx;
    let AotBinaryOp { ip, lhs, rhs } = op;
    let lhs_tag = ssa_load_tag_i32(b, layout.value, lhs);
    let rhs_tag = ssa_load_tag_i32(b, layout.value, rhs);
    let lhs_null = b
        .ins()
        .icmp_imm(IntCC::Equal, lhs_tag, i64::from(layout.value.null_tag));
    let rhs_null = b
        .ins()
        .icmp_imm(IntCC::Equal, rhs_tag, i64::from(layout.value.null_tag));
    let both_null = b.ins().band(lhs_null, rhs_null);
    let ok = b.create_block();
    let fail = b.create_block();
    let cont = b.create_block();
    b.append_block_param(cont, types::I8);
    b.ins().brif(both_null, ok, &[], fail, &[]);

    b.switch_to_block(ok);
    let one = b.ins().iconst(types::I8, 1);
    b.ins().jump(cont, &[BlockArg::Value(one)]);

    b.switch_to_block(fail);
    aot_emit_error_status(b, vm_ptr, exit_block, pointer_type, offsets, ip)?;

    b.switch_to_block(cont);
    Ok(b.block_params(cont)[0])
}

#[cfg(feature = "cranelift-jit")]
fn aot_lower_string_get(
    b: &mut FunctionBuilder,
    ctx: AotLowerCtx<'_>,
    op: AotStringGetOp,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let AotLowerCtx {
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
    } = ctx;
    let AotStringGetOp {
        output_id,
        ip,
        text,
        index,
    } = op;
    let string_raw = aot_load_checked_heap_ptr(
        b,
        ctx,
        AotTaggedValueOp { ip, value: text },
        layout.value.string_tag,
    )?;
    let string_data = ssa_load_heap_data_ptr(b, layout.value, string_raw);
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
        AotTempValueSlotKey::Output(output_id),
    )?;
    let loop_block = b.create_block();
    let scan_block = b.create_block();
    let copy_block = b.create_block();
    let advance_block = b.create_block();
    let fail = b.create_block();
    let cont = b.create_block();
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
    aot_emit_error_status(b, vm_ptr, exit_block, pointer_type, offsets, ip)?;

    b.switch_to_block(cont);
    Ok(out)
}

#[cfg(feature = "cranelift-jit")]
fn aot_lower_concat(
    b: &mut FunctionBuilder,
    ctx: AotLowerCtx<'_>,
    op: AotConcatOp,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let AotLowerCtx {
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
    } = ctx;
    let AotConcatOp {
        output_id,
        ip,
        lhs,
        rhs,
        result_tag,
        pack_addr,
    } = op;
    let lhs_raw =
        aot_load_checked_heap_ptr(b, ctx, AotTaggedValueOp { ip, value: lhs }, result_tag)?;
    let rhs_raw =
        aot_load_checked_heap_ptr(b, ctx, AotTaggedValueOp { ip, value: rhs }, result_tag)?;
    let lhs_data = ssa_load_heap_data_ptr(b, layout.value, lhs_raw);
    let rhs_data = ssa_load_heap_data_ptr(b, layout.value, rhs_raw);
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
        AotTempValueSlotKey::Output(output_id),
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
    aot_emit_error_status(b, vm_ptr, exit_block, pointer_type, offsets, ip)?;

    b.switch_to_block(cont);
    Ok(out)
}

#[cfg(feature = "cranelift-jit")]
fn aot_lower_string_slice(
    b: &mut FunctionBuilder,
    ctx: AotLowerCtx<'_>,
    op: AotSliceOp,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let AotLowerCtx {
        heap_refs,
        heap_addrs,
        helper_refs,
        helper_addrs,
        owned_value_temps,
        ..
    } = ctx;
    let AotSliceOp {
        output_id,
        ip,
        value: text,
        start,
        length,
    } = op;
    let AotLowerCtx {
        pointer_type,
        layout,
        ..
    } = ctx;
    let string_raw = aot_load_checked_heap_ptr(
        b,
        ctx,
        AotTaggedValueOp { ip, value: text },
        layout.value.string_tag,
    )?;
    let string_data = ssa_load_heap_data_ptr(b, layout.value, string_raw);
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
        AotTempValueSlotKey::Output(output_id),
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
    Ok(out)
}

#[cfg(feature = "cranelift-jit")]
fn aot_lower_bytes_slice(
    b: &mut FunctionBuilder,
    ctx: AotLowerCtx<'_>,
    op: AotSliceOp,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let AotLowerCtx {
        heap_refs,
        heap_addrs,
        helper_refs,
        helper_addrs,
        owned_value_temps,
        ..
    } = ctx;
    let AotSliceOp {
        output_id,
        ip,
        value: bytes,
        start,
        length,
    } = op;
    let AotLowerCtx {
        pointer_type,
        layout,
        ..
    } = ctx;
    let bytes_raw = aot_load_checked_heap_ptr(
        b,
        ctx,
        AotTaggedValueOp { ip, value: bytes },
        layout.value.bytes_tag,
    )?;
    let bytes_data = ssa_load_heap_data_ptr(b, layout.value, bytes_raw);
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
        AotTempValueSlotKey::Output(output_id),
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
    Ok(out)
}

#[cfg(feature = "cranelift-jit")]
fn aot_lower_bytes_from_array_u8(
    b: &mut FunctionBuilder,
    ctx: AotLowerCtx<'_>,
    op: AotArrayConvertOp,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let AotLowerCtx {
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
    } = ctx;
    let AotArrayConvertOp {
        output_id,
        ip,
        value: array,
    } = op;
    let array_raw = aot_load_checked_heap_ptr(
        b,
        ctx,
        AotTaggedValueOp { ip, value: array },
        layout.value.array_tag,
    )?;
    let array_data = ssa_load_heap_data_ptr(b, layout.value, array_raw);
    let values_ptr = b.ins().load(
        pointer_type,
        MemFlags::new(),
        array_data,
        layout.stack_vec.ptr_offset,
    );
    let values_len = b.ins().load(
        pointer_type,
        MemFlags::new(),
        array_data,
        layout.stack_vec.len_offset,
    );
    let out = owned_value_temp_slot_addr(
        b,
        pointer_type,
        owned_value_temps,
        AotTempValueSlotKey::Output(output_id),
    )?;
    let zero = b.ins().iconst(pointer_type, 0);
    let validate_loop = b.create_block();
    let copy_loop = b.create_block();
    let finish = b.create_block();
    let fail = b.create_block();
    let cont = b.create_block();
    b.append_block_param(validate_loop, pointer_type);
    b.append_block_param(copy_loop, pointer_type);

    b.ins().jump(validate_loop, &[BlockArg::Value(zero)]);

    b.switch_to_block(validate_loop);
    let validate_index = b.block_params(validate_loop)[0];
    let done = b.ins().icmp(
        IntCC::UnsignedGreaterThanOrEqual,
        validate_index,
        values_len,
    );
    let validated = b.create_block();
    let validate_step = b.create_block();
    b.ins().brif(done, validated, &[], validate_step, &[]);

    b.switch_to_block(validate_step);
    let element_addr = ssa_value_addr(
        b,
        pointer_type,
        values_ptr,
        validate_index,
        layout.value.size,
    );
    let element_tag = ssa_load_tag_i32(b, layout.value, element_addr);
    let is_int = b
        .ins()
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

    b.switch_to_block(validated);
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
    let element_addr = ssa_value_addr(b, pointer_type, values_ptr, copy_index, layout.value.size);
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
    aot_emit_error_status(b, vm_ptr, exit_block, pointer_type, offsets, ip)?;

    b.switch_to_block(cont);
    Ok(out)
}

#[cfg(feature = "cranelift-jit")]
fn aot_lower_bytes_to_array_u8(
    b: &mut FunctionBuilder,
    ctx: AotLowerCtx<'_>,
    op: AotArrayConvertOp,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let AotLowerCtx {
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
    } = ctx;
    let AotArrayConvertOp {
        output_id,
        ip,
        value: bytes,
    } = op;
    let bytes_raw = aot_load_checked_heap_ptr(
        b,
        ctx,
        AotTaggedValueOp { ip, value: bytes },
        layout.value.bytes_tag,
    )?;
    let bytes_data = ssa_load_heap_data_ptr(b, layout.value, bytes_raw);
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
        AotTempValueSlotKey::Output(output_id),
    )?;
    let value_size = i64::from(layout.value.size);
    let max_values = b.ins().iconst(pointer_type, i64::MAX / value_size);
    let too_large = b
        .ins()
        .icmp(IntCC::UnsignedGreaterThan, bytes_len, max_values);
    let cap_ok = b.create_block();
    let fill_loop = b.create_block();
    let finish = b.create_block();
    let fail = b.create_block();
    let cont = b.create_block();
    b.append_block_param(fill_loop, pointer_type);

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
    let element_addr = ssa_value_addr(b, pointer_type, out_ptr, fill_index, layout.value.size);
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
    aot_emit_error_status(b, vm_ptr, exit_block, pointer_type, offsets, ip)?;

    b.switch_to_block(cont);
    Ok(out)
}

#[cfg(feature = "cranelift-jit")]
fn resolve_jump_args(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    value_layout: crate::vm::native::ValueLayout,
    target: &AotSsaJumpTarget,
    values: &HashMap<AotSsaValueId, cranelift_codegen::ir::Value>,
    value_reprs: &HashMap<AotSsaValueId, AotSsaValueRepr>,
    params: &[super::ssa::AotSsaBlockParam],
) -> Result<Vec<cranelift_codegen::ir::Value>, AotCompileError> {
    target
        .args
        .iter()
        .zip(params.iter())
        .map(|arg| {
            let (value_id, param) = arg;
            let value = values
                .get(value_id)
                .copied()
                .ok_or_else(|| AotCompileError::Codegen("missing jump arg value".to_string()))?;
            let src_repr = *value_reprs
                .get(value_id)
                .ok_or_else(|| AotCompileError::Codegen("missing jump arg repr".to_string()))?;
            adapt_jump_arg(
                b,
                pointer_type,
                value_layout,
                value,
                src_repr,
                param.value.repr,
            )
        })
        .collect()
}

#[cfg(feature = "cranelift-jit")]
fn adapt_jump_arg(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    value_layout: crate::vm::native::ValueLayout,
    value: cranelift_codegen::ir::Value,
    src_repr: AotSsaValueRepr,
    dst_repr: AotSsaValueRepr,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    if src_repr == dst_repr {
        return Ok(value);
    }
    match (src_repr, dst_repr) {
        (AotSsaValueRepr::I64, AotSsaValueRepr::Tagged) => {
            let slot = alloc_single_value_slot(b, pointer_type, value_layout.size)?;
            ssa_store_int_in_value(b, value_layout, slot, value);
            Ok(slot)
        }
        (AotSsaValueRepr::F64, AotSsaValueRepr::Tagged) => {
            let slot = alloc_single_value_slot(b, pointer_type, value_layout.size)?;
            ssa_store_float_in_value(b, value_layout, slot, value);
            Ok(slot)
        }
        (AotSsaValueRepr::Bool, AotSsaValueRepr::Tagged) => {
            let slot = alloc_single_value_slot(b, pointer_type, value_layout.size)?;
            ssa_store_bool_in_value(b, value_layout, slot, value);
            Ok(slot)
        }
        (AotSsaValueRepr::Tagged, AotSsaValueRepr::I64) => Ok(b.ins().load(
            types::I64,
            MemFlags::new(),
            value,
            value_layout.int_payload_offset,
        )),
        (AotSsaValueRepr::Tagged, AotSsaValueRepr::F64) => Ok(b.ins().load(
            types::F64,
            MemFlags::new(),
            value,
            value_layout.float_payload_offset,
        )),
        (AotSsaValueRepr::Tagged, AotSsaValueRepr::Bool) => Ok(b.ins().load(
            types::I8,
            MemFlags::new(),
            value,
            value_layout.bool_payload_offset,
        )),
        _ => Err(AotCompileError::Codegen(format!(
            "unsupported jump arg adaptation {src_repr:?} -> {dst_repr:?}"
        ))),
    }
}

#[cfg(feature = "cranelift-jit")]
fn ssa_block_args(values: Vec<cranelift_codegen::ir::Value>) -> Vec<BlockArg> {
    values.into_iter().map(BlockArg::Value).collect()
}

#[cfg(feature = "cranelift-jit")]
#[allow(clippy::too_many_arguments)]
fn materialize_state_to_vm(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    layout: crate::vm::native::NativeStackLayout,
    helper_refs: AotDeoptHelperRefs,
    helper_addrs: AotDeoptHelperAddrs,
    stack: &[AotSsaMaterialization],
    locals: &[AotSsaMaterialization],
    values: &HashMap<AotSsaValueId, cranelift_codegen::ir::Value>,
    ip: usize,
) -> Result<(), AotCompileError> {
    let materialize_ctx = AotMaterializeCtx {
        exit_block,
        pointer_type,
        value_layout: layout.value,
        helper_refs,
        helper_addrs,
        values,
    };
    let stack_ptr = alloc_value_buffer(b, pointer_type, stack.len(), layout.value.size)?;
    for (index, materialization) in stack.iter().enumerate() {
        let dst = value_buffer_slot_addr(
            b,
            pointer_type,
            stack_ptr,
            index,
            layout.value.size,
            "stack",
        )?;
        materialize_slot(b, materialize_ctx, materialization, dst, "stack")?;
    }

    let locals_ptr = alloc_value_buffer(b, pointer_type, locals.len(), layout.value.size)?;
    for (index, materialization) in locals.iter().enumerate() {
        let dst = value_buffer_slot_addr(
            b,
            pointer_type,
            locals_ptr,
            index,
            layout.value.size,
            "local",
        )?;
        materialize_slot(b, materialize_ctx, materialization, dst, "local")?;
    }

    let null_ptr = b.ins().iconst(pointer_type, 0);
    let stack_len = b.ins().iconst(
        pointer_type,
        i64::try_from(stack.len())
            .map_err(|_| AotCompileError::Codegen("stack length out of range".to_string()))?,
    );
    let locals_len = b.ins().iconst(
        pointer_type,
        i64::try_from(locals.len())
            .map_err(|_| AotCompileError::Codegen("locals length out of range".to_string()))?,
    );
    let ip_val = b.ins().iconst(
        pointer_type,
        i64::try_from(ip).map_err(|_| AotCompileError::Codegen("ip out of range".to_string()))?,
    );
    call_status_helper(
        b,
        exit_block,
        pointer_type,
        helper_refs.restore_exit_ref,
        helper_addrs.restore_exit,
        &[
            vm_ptr,
            stack_ptr.unwrap_or(null_ptr),
            stack_len,
            locals_ptr.unwrap_or(null_ptr),
            locals_len,
            ip_val,
        ],
    )
}

#[cfg(feature = "cranelift-jit")]
fn materialize_slot(
    b: &mut FunctionBuilder,
    ctx: AotMaterializeCtx<'_>,
    materialization: &AotSsaMaterialization,
    dst_addr: cranelift_codegen::ir::Value,
    slot_kind: &'static str,
) -> Result<(), AotCompileError> {
    let AotMaterializeCtx {
        exit_block,
        pointer_type,
        value_layout,
        helper_refs,
        helper_addrs,
        values,
    } = ctx;
    match materialization {
        AotSsaMaterialization::Value(value) => {
            let src = *values.get(value).ok_or_else(|| {
                AotCompileError::Codegen(format!("missing tagged {slot_kind} value"))
            })?;
            call_status_helper(
                b,
                exit_block,
                pointer_type,
                helper_refs.clone_value_ref,
                helper_addrs.clone_value,
                &[dst_addr, src],
            )?;
        }
        AotSsaMaterialization::BoxInt(value) => {
            let src = *values.get(value).ok_or_else(|| {
                AotCompileError::Codegen(format!("missing int {slot_kind} value"))
            })?;
            ssa_store_int_in_value(b, value_layout, dst_addr, src);
        }
        AotSsaMaterialization::BoxFloat(value) => {
            let src = *values.get(value).ok_or_else(|| {
                AotCompileError::Codegen(format!("missing float {slot_kind} value"))
            })?;
            ssa_store_float_in_value(b, value_layout, dst_addr, src);
        }
        AotSsaMaterialization::BoxBool(value) => {
            let src = *values.get(value).ok_or_else(|| {
                AotCompileError::Codegen(format!("missing bool {slot_kind} value"))
            })?;
            ssa_store_bool_in_value(b, value_layout, dst_addr, src);
        }
        AotSsaMaterialization::BoxHeapPtr { value, tag } => {
            let src = *values.get(value).ok_or_else(|| {
                AotCompileError::Codegen(format!("missing heap {slot_kind} value"))
            })?;
            let tag = b.ins().iconst(types::I64, *tag as i64);
            call_status_helper(
                b,
                exit_block,
                pointer_type,
                helper_refs.box_heap_value_ref,
                helper_addrs.box_heap_value,
                &[dst_addr, src, tag],
            )?;
        }
    }
    Ok(())
}

#[cfg(feature = "cranelift-jit")]
fn ssa_type(
    pointer_type: cranelift_codegen::ir::Type,
    repr: AotSsaValueRepr,
) -> Result<cranelift_codegen::ir::Type, AotCompileError> {
    Ok(match repr {
        AotSsaValueRepr::Tagged | AotSsaValueRepr::HeapPtr(_) => pointer_type,
        AotSsaValueRepr::I64 => types::I64,
        AotSsaValueRepr::F64 => types::F64,
        AotSsaValueRepr::Bool => types::I8,
    })
}

#[cfg(feature = "cranelift-jit")]
fn alloc_value_buffer(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    slot_count: usize,
    value_size: i32,
) -> Result<Option<cranelift_codegen::ir::Value>, AotCompileError> {
    if slot_count == 0 {
        return Ok(None);
    }
    let value_size = usize::try_from(value_size)
        .map_err(|_| AotCompileError::Codegen("value slot size out of range".to_string()))?;
    let bytes = slot_count
        .checked_mul(value_size)
        .ok_or_else(|| AotCompileError::Codegen("value buffer overflow".to_string()))?;
    let bytes = u32::try_from(bytes)
        .map_err(|_| AotCompileError::Codegen("value buffer too large".to_string()))?;
    let align_shift = std::mem::align_of::<Value>().trailing_zeros() as u8;
    let slot = b.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
        cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
        bytes,
        align_shift,
    ));
    Ok(Some(b.ins().stack_addr(pointer_type, slot, 0)))
}

#[cfg(feature = "cranelift-jit")]
fn alloc_single_value_slot(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    value_size: i32,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    Ok(alloc_value_buffer(b, pointer_type, 1, value_size)?
        .expect("single value slot must allocate"))
}

#[cfg(feature = "cranelift-jit")]
fn value_buffer_slot_addr(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    base_ptr: Option<cranelift_codegen::ir::Value>,
    index: usize,
    value_size: i32,
    slot_kind: &'static str,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let base_ptr = base_ptr
        .ok_or_else(|| AotCompileError::Codegen(format!("missing {slot_kind} value buffer")))?;
    let index = b.ins().iconst(
        pointer_type,
        i64::try_from(index)
            .map_err(|_| AotCompileError::Codegen(format!("{slot_kind} index out of range")))?,
    );
    Ok(ssa_value_addr(b, pointer_type, base_ptr, index, value_size))
}

#[cfg(feature = "cranelift-jit")]
fn call_status_helper(
    b: &mut FunctionBuilder,
    exit_block: Block,
    pointer_type: cranelift_codegen::ir::Type,
    helper_ref: cranelift_codegen::ir::SigRef,
    helper_addr: usize,
    args: &[cranelift_codegen::ir::Value],
) -> Result<(), AotCompileError> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, helper_addr)?;
    let call = b.ins().call_indirect(helper_ref, helper_ptr, args);
    let status = b.inst_results(call)[0];
    let next = b.create_block();
    let is_continue = b
        .ins()
        .icmp_imm(IntCC::Equal, status, STATUS_CONTINUE as i64);
    b.ins().brif(
        is_continue,
        next,
        &[],
        exit_block,
        &[BlockArg::Value(status)],
    );
    b.switch_to_block(next);
    Ok(())
}

#[cfg(feature = "cranelift-jit")]
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

#[cfg(feature = "cranelift-jit")]
fn ssa_tag_type(layout: crate::vm::native::ValueLayout) -> cranelift_codegen::ir::Type {
    match layout.tag_size {
        1 => types::I8,
        2 => types::I16,
        _ => types::I32,
    }
}

#[cfg(feature = "cranelift-jit")]
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

#[cfg(feature = "cranelift-jit")]
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

#[cfg(feature = "cranelift-jit")]
fn ssa_store_bool_in_value(
    b: &mut FunctionBuilder,
    layout: crate::vm::native::ValueLayout,
    value_addr: cranelift_codegen::ir::Value,
    bool_value: cranelift_codegen::ir::Value,
) {
    ssa_store_tag(b, layout, value_addr, layout.bool_tag);
    b.ins().store(
        MemFlags::new(),
        bool_value,
        value_addr,
        layout.bool_payload_offset,
    );
}

#[cfg(feature = "cranelift-jit")]
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

#[cfg(feature = "cranelift-jit")]
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

#[cfg(feature = "cranelift-jit")]
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

#[cfg(feature = "cranelift-jit")]
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

#[cfg(feature = "cranelift-jit")]
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

#[cfg(feature = "cranelift-jit")]
fn ssa_call_alloc_buffer(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    heap_refs: HeapIntrinsicRefs,
    _heap_addrs: HeapIntrinsicAddrs,
    addr: usize,
    cap: cranelift_codegen::ir::Value,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, addr)?;
    let call = b
        .ins()
        .call_indirect(heap_refs.alloc_buffer_ref, helper_ptr, &[cap]);
    Ok(b.inst_results(call)[0])
}

#[cfg(feature = "cranelift-jit")]
fn ssa_call_pack_shared(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    heap_refs: HeapIntrinsicRefs,
    addr: usize,
    ptr: cranelift_codegen::ir::Value,
    len: cranelift_codegen::ir::Value,
    cap: cranelift_codegen::ir::Value,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, addr)?;
    let call = b
        .ins()
        .call_indirect(heap_refs.pack_shared_ref, helper_ptr, &[ptr, len, cap]);
    Ok(b.inst_results(call)[0])
}

#[cfg(feature = "cranelift-jit")]
fn ssa_call_copy_bytes(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    heap_refs: HeapIntrinsicRefs,
    heap_addrs: HeapIntrinsicAddrs,
    dst: cranelift_codegen::ir::Value,
    src: cranelift_codegen::ir::Value,
    len: cranelift_codegen::ir::Value,
) -> Result<(), AotCompileError> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, heap_addrs.copy_bytes)?;
    b.ins()
        .call_indirect(heap_refs.copy_bytes_ref, helper_ptr, &[dst, src, len]);
    Ok(())
}

#[cfg(feature = "cranelift-jit")]
fn ssa_call_zero_bytes(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    heap_refs: HeapIntrinsicRefs,
    heap_addrs: HeapIntrinsicAddrs,
    dst: cranelift_codegen::ir::Value,
    len: cranelift_codegen::ir::Value,
) -> Result<(), AotCompileError> {
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, heap_addrs.zero_bytes)?;
    b.ins()
        .call_indirect(heap_refs.free_buffer_ref, helper_ptr, &[dst, len]);
    Ok(())
}

#[cfg(feature = "cranelift-jit")]
fn ssa_load_byte(
    b: &mut FunctionBuilder,
    ptr: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    let byte = b.ins().load(types::I8, MemFlags::new(), ptr, 0);
    b.ins().uextend(types::I32, byte)
}

#[cfg(feature = "cranelift-jit")]
fn ssa_is_utf8_continuation_byte(
    b: &mut FunctionBuilder,
    byte: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    let mask = b.ins().iconst(types::I32, 0xC0);
    let masked = b.ins().band(byte, mask);
    b.ins().icmp_imm(IntCC::Equal, masked, 0x80)
}

#[cfg(feature = "cranelift-jit")]
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

#[cfg(feature = "cranelift-jit")]
fn ssa_index_in_range(
    b: &mut FunctionBuilder,
    index: cranelift_codegen::ir::Value,
    len: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    let ge_zero = b.ins().icmp_imm(IntCC::SignedGreaterThanOrEqual, index, 0);
    let lt_len = b.ins().icmp(IntCC::UnsignedLessThan, index, len);
    b.ins().band(ge_zero, lt_len)
}

#[cfg(feature = "cranelift-jit")]
fn emit_call_boundary_interrupt(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    interrupt_ref: cranelift_codegen::ir::SigRef,
    helper_addr: usize,
    pointer_type: cranelift_codegen::ir::Type,
    exit_block: Block,
) -> Result<(), AotCompileError> {
    let next = b.create_block();
    let helper_ptr = iconst_ptr_from_addr(b, pointer_type, helper_addr)?;
    let call = b.ins().call_indirect(interrupt_ref, helper_ptr, &[vm_ptr]);
    let status = b.inst_results(call)[0];
    let is_continue = b
        .ins()
        .icmp_imm(IntCC::Equal, status, STATUS_CONTINUE as i64);
    b.ins().brif(
        is_continue,
        next,
        &[],
        exit_block,
        &[BlockArg::Value(status)],
    );
    b.switch_to_block(next);
    Ok(())
}

#[cfg(feature = "cranelift-jit")]
fn iconst_ptr_from_addr(
    b: &mut FunctionBuilder,
    pointer_type: cranelift_codegen::ir::Type,
    addr: usize,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let addr = i64::try_from(addr)
        .map_err(|_| AotCompileError::Codegen("native helper address out of range".to_string()))?;
    Ok(b.ins().iconst(pointer_type, addr))
}

#[cfg(feature = "cranelift-jit")]
fn call_step_helper(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: cranelift_codegen::ir::SigRef,
    helper_offset: i32,
    pointer_type: cranelift_codegen::ir::Type,
    args: AotStepHelperArgs,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let AotStepHelperArgs {
        op,
        a,
        b_arg: arg_b,
        c,
    } = args;
    let helper_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, helper_offset);
    let op_val = b.ins().iconst(types::I64, op);
    let a_val = b.ins().iconst(types::I64, a);
    let b_val = b.ins().iconst(types::I64, arg_b);
    let c_val = b.ins().iconst(types::I64, c);
    let call = b.ins().call_indirect(
        helper_ref,
        helper_ptr,
        &[vm_ptr, op_val, a_val, b_val, c_val],
    );
    Ok(b.inst_results(call)[0])
}

#[cfg(feature = "cranelift-jit")]
fn store_vm_ip(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    pointer_type: cranelift_codegen::ir::Type,
    vm_ip_offset: i32,
    ip: i64,
) {
    let ip_val = b.ins().iconst(pointer_type, ip);
    b.ins().store(MemFlags::new(), ip_val, vm_ptr, vm_ip_offset);
}

#[cfg(feature = "cranelift-jit")]
fn native_isa() -> Result<OwnedTargetIsa, AotCompileError> {
    let cached = CRANELIFT_AOT_ISA.get_or_init(|| {
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
    cached.clone().map_err(AotCompileError::Codegen)
}
