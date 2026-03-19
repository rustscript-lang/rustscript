use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

#[cfg(feature = "cranelift-jit")]
use std::collections::HashMap;
#[cfg(feature = "cranelift-jit")]
use std::sync::OnceLock;
#[cfg(feature = "cranelift-jit")]
use std::sync::atomic::{AtomicU64, Ordering};

use crate::vm::native::ExecutableBuffer;
#[cfg(feature = "cranelift-jit")]
use crate::vm::native::{
    HeapIntrinsicAddrs, HeapIntrinsicRefs, InlineEmitCtx, NativeInlineStep, OP_ADD, OP_AND,
    OP_BUILTIN_CALL, OP_CALL, OP_CEQ, OP_CGT, OP_CLT, OP_DIV, OP_DUP, OP_GUARD_FALSE, OP_JUMP,
    OP_LDC, OP_LDLOC, OP_LSHR, OP_MOD, OP_MUL, OP_NEG, OP_NOT, OP_OR, OP_POP, OP_SHL, OP_SHR,
    OP_STLOC, OP_SUB, STATUS_CONTINUE, STATUS_ERROR, STATUS_TRACE_EXIT, alloc_buffer_signature,
    alloc_byte_buffer_entry_address, alloc_value_buffer_entry_address, copy_bytes_entry_address,
    copy_bytes_signature, detect_native_stack_layout, drop_shared_array_entry_address,
    drop_shared_bytes_entry_address, drop_shared_signature, drop_shared_string_entry_address,
    emit_native_inline_step, entry_signature, free_buffer_signature, helper_entry_offset,
    helper_signature, interrupt_helper_entry_offset, jump_with_status, pack_shared_signature,
    resolve_offsets, shared_array_from_buffer_entry_address,
    shared_bytes_from_buffer_entry_address, shared_string_from_buffer_entry_address,
    zero_bytes_entry_address,
};
use crate::vm::{OpCode, Program, Vm, VmError, VmResult};
#[cfg(feature = "cranelift-jit")]
use cranelift_codegen::ir::condcodes::IntCC;
#[cfg(feature = "cranelift-jit")]
use cranelift_codegen::ir::{Block, BlockArg, InstBuilder, MemFlags, types};
#[cfg(feature = "cranelift-jit")]
use cranelift_codegen::isa::OwnedTargetIsa;
#[cfg(feature = "cranelift-jit")]
use cranelift_codegen::settings::{self, Configurable};
#[cfg(feature = "cranelift-jit")]
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Switch};
#[cfg(feature = "cranelift-jit")]
use cranelift_jit::{JITBuilder, JITModule};
#[cfg(feature = "cranelift-jit")]
use cranelift_module::{Linkage, Module};

use super::cfg::AotBlockTerminal;
#[cfg(any(feature = "cranelift-jit", test))]
use super::ir::lower_program;
use super::ir::{
    AotBytesCodecKind, AotCallDispatch, AotConcatKind, AotInstruction, AotIrBlock, AotLowerError,
    AotProgram, AotTextBytesKind,
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

#[derive(Clone, Debug)]
struct SegmentStep {
    ip: usize,
    instruction: AotInstruction,
}

#[derive(Clone, Debug)]
enum SegmentContinuation {
    Direct(usize),
    Terminal {
        terminal: AotBlockTerminal,
        terminal_ip: Option<usize>,
    },
}

#[derive(Clone, Debug)]
struct AotSegment {
    entry_ip: usize,
    steps: Vec<SegmentStep>,
    continuation: SegmentContinuation,
}

#[derive(Debug)]
enum AotCompileError {
    Lower(AotLowerError),
    InvalidInstructionIp {
        block_start: usize,
        ip: usize,
    },
    MissingSegment {
        source_ip: usize,
        target_ip: usize,
    },
    DuplicateSegmentEntry {
        entry_ip: usize,
    },
    InvalidTerminal {
        block_start: usize,
        terminal: &'static str,
    },
    Codegen(String),
}

impl fmt::Display for AotCompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lower(err) => write!(f, "aot lowering failed: {err:?}"),
            Self::InvalidInstructionIp { block_start, ip } => write!(
                f,
                "aot block starting at ip {} could not decode instruction ip {}",
                block_start, ip
            ),
            Self::MissingSegment {
                source_ip,
                target_ip,
            } => write!(
                f,
                "aot segment at ip {} references missing target ip {}",
                source_ip, target_ip
            ),
            Self::DuplicateSegmentEntry { entry_ip } => {
                write!(
                    f,
                    "aot segment entry ip {} was emitted more than once",
                    entry_ip
                )
            }
            Self::InvalidTerminal {
                block_start,
                terminal,
            } => write!(
                f,
                "aot block starting at ip {} has invalid terminal {}",
                block_start, terminal
            ),
            Self::Codegen(message) => f.write_str(message),
        }
    }
}

impl From<AotLowerError> for AotCompileError {
    fn from(value: AotLowerError) -> Self {
        Self::Lower(value)
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
    let lowered = lower_program(program)?;
    let segments = build_segments(program, &lowered)?;
    let entry = compile_segments(program, &lowered, &segments)?;
    Ok(entry)
}

#[cfg(not(feature = "cranelift-jit"))]
fn compile_program_inner(_program: &Program) -> Result<CompiledProgram, AotCompileError> {
    Err(AotCompileError::Codegen(
        "whole-program AOT backend is disabled (feature 'cranelift-jit' is not enabled)"
            .to_string(),
    ))
}

#[cfg(feature = "cranelift-jit")]
fn compile_segments(
    program: &Program,
    _lowered: &AotProgram,
    segments: &[AotSegment],
) -> Result<CompiledProgram, AotCompileError> {
    let isa = native_isa()?;
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
    let interrupt_sig = entry_signature(pointer_type, call_conv);
    let vm_status_sig = entry_signature(pointer_type, call_conv);
    let helper_offset = helper_entry_offset();
    let interrupt_helper_offset = interrupt_helper_entry_offset();
    let heap_addrs = HeapIntrinsicAddrs {
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
    let layout = detect_native_stack_layout().map_err(|err| {
        AotCompileError::Codegen(format!("detect native stack layout failed: {err}"))
    })?;
    let offsets = resolve_offsets(layout)
        .map_err(|err| AotCompileError::Codegen(format!("resolve native offsets failed: {err}")))?;

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

    let vm_ip_offset =
        i32::try_from(std::mem::offset_of!(Vm, ip)).expect("Vm::ip offset must fit i32");
    let code_len_i64 = i64::try_from(program.code.len())
        .map_err(|_| AotCompileError::Codegen("program length does not fit i64".to_string()))?;

    {
        let mut fb_ctx = FunctionBuilderContext::new();
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fb_ctx);

        let entry_block = b.create_block();
        let dispatch_block = b.create_block();
        let miss_block = b.create_block();
        let exit_block = b.create_block();
        b.append_block_param(exit_block, types::I32);

        let mut segment_blocks = HashMap::with_capacity(segments.len());
        for segment in segments {
            segment_blocks.insert(segment.entry_ip, b.create_block());
        }

        b.switch_to_block(entry_block);
        b.append_block_params_for_function_params(entry_block);
        let vm_ptr = b.block_params(entry_block)[0];
        b.ins().jump(dispatch_block, &[]);

        b.switch_to_block(dispatch_block);
        let vm_ip = b
            .ins()
            .load(pointer_type, MemFlags::new(), vm_ptr, vm_ip_offset);
        let mut switch = Switch::new();
        for segment in segments {
            switch.set_entry(segment.entry_ip as u128, segment_blocks[&segment.entry_ip]);
        }
        switch.emit(&mut b, vm_ip, miss_block);

        b.switch_to_block(miss_block);
        let miss_status = b.ins().iconst(types::I32, STATUS_ERROR as i64);
        jump_with_status(&mut b, exit_block, miss_status);

        let helper_ref = b.import_signature(helper_sig);
        let vm_status_helper_ref = b.import_signature(vm_status_sig);
        let interrupt_ref = b.import_signature(interrupt_sig);
        let heap_refs = HeapIntrinsicRefs {
            alloc_buffer_ref: b.import_signature(alloc_buffer_sig),
            free_buffer_ref: b.import_signature(free_buffer_sig),
            pack_shared_ref: b.import_signature(pack_shared_sig),
            drop_shared_ref: b.import_signature(drop_shared_sig),
            copy_bytes_ref: b.import_signature(copy_bytes_sig),
        };

        for segment in segments {
            let segment_block = segment_blocks[&segment.entry_ip];
            b.switch_to_block(segment_block);
            store_vm_ip(
                &mut b,
                vm_ptr,
                pointer_type,
                vm_ip_offset,
                i64::try_from(segment.entry_ip).map_err(|_| {
                    AotCompileError::Codegen("segment entry ip does not fit i64".to_string())
                })?,
            );

            for step in &segment.steps {
                store_vm_ip(
                    &mut b,
                    vm_ptr,
                    pointer_type,
                    vm_ip_offset,
                    i64::try_from(step.ip).map_err(|_| {
                        AotCompileError::Codegen("segment step ip does not fit i64".to_string())
                    })?,
                );
                emit_interrupt_tick(
                    &mut b,
                    vm_ptr,
                    interrupt_ref,
                    interrupt_helper_offset,
                    pointer_type,
                    exit_block,
                )?;
                emit_step(
                    &mut b,
                    vm_ptr,
                    helper_ref,
                    vm_status_helper_ref,
                    helper_offset,
                    pointer_type,
                    layout,
                    offsets,
                    heap_refs,
                    heap_addrs,
                    exit_block,
                    step,
                )?;
            }

            match &segment.continuation {
                SegmentContinuation::Direct(target_ip) => {
                    let block =
                        *segment_blocks
                            .get(target_ip)
                            .ok_or(AotCompileError::MissingSegment {
                                source_ip: segment.entry_ip,
                                target_ip: *target_ip,
                            })?;
                    b.ins().jump(block, &[]);
                }
                SegmentContinuation::Terminal {
                    terminal,
                    terminal_ip,
                } => {
                    emit_terminal(
                        &mut b,
                        vm_ptr,
                        helper_ref,
                        helper_offset,
                        interrupt_ref,
                        interrupt_helper_offset,
                        pointer_type,
                        vm_ip_offset,
                        exit_block,
                        code_len_i64,
                        segment.entry_ip,
                        terminal,
                        *terminal_ip,
                        &segment_blocks,
                    )?;
                }
            }
        }

        b.switch_to_block(exit_block);
        let final_status = b.block_params(exit_block)[0];
        b.ins().return_(&[final_status]);

        b.seal_all_blocks();
        b.finalize();
    }

    module
        .define_function(func_id, &mut ctx)
        .map_err(|err| AotCompileError::Codegen(format!("define aot function failed: {err}")))?;
    let code_len = ctx
        .compiled_code()
        .ok_or_else(|| {
            AotCompileError::Codegen("aot compile produced no machine code".to_string())
        })?
        .code_buffer()
        .len();
    module.clear_context(&mut ctx);
    module.finalize_definitions().map_err(|err| {
        AotCompileError::Codegen(format!("finalize aot definitions failed: {err}"))
    })?;

    let entry = module.get_finalized_function(func_id);
    let code = if code_len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(entry, code_len).to_vec() }
    };
    CompiledProgram::from_code(
        code,
        segments
            .iter()
            .map(|segment| segment.entry_ip)
            .collect::<Vec<_>>(),
    )
    .map_err(|err| AotCompileError::Codegen(err.to_string()))
}

#[cfg(feature = "cranelift-jit")]
#[allow(clippy::too_many_arguments)]
fn emit_terminal(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: cranelift_codegen::ir::SigRef,
    helper_offset: i32,
    interrupt_ref: cranelift_codegen::ir::SigRef,
    interrupt_helper_offset: i32,
    pointer_type: cranelift_codegen::ir::Type,
    vm_ip_offset: i32,
    exit_block: Block,
    code_len_i64: i64,
    segment_ip: usize,
    terminal: &AotBlockTerminal,
    terminal_ip: Option<usize>,
    segment_blocks: &HashMap<usize, Block>,
) -> Result<(), AotCompileError> {
    match terminal {
        AotBlockTerminal::Return => {
            let terminal_ip = terminal_ip.ok_or(AotCompileError::InvalidTerminal {
                block_start: segment_ip,
                terminal: "return",
            })?;
            store_vm_ip(
                b,
                vm_ptr,
                pointer_type,
                vm_ip_offset,
                i64::try_from(terminal_ip).map_err(|_| {
                    AotCompileError::Codegen("terminal ip does not fit i64".to_string())
                })?,
            );
            emit_interrupt_tick(
                b,
                vm_ptr,
                interrupt_ref,
                interrupt_helper_offset,
                pointer_type,
                exit_block,
            )?;
            let halted = b
                .ins()
                .iconst(types::I32, crate::vm::native::STATUS_HALTED as i64);
            jump_with_status(b, exit_block, halted);
        }
        AotBlockTerminal::Jump { target_ip } => {
            let terminal_ip = terminal_ip.ok_or(AotCompileError::InvalidTerminal {
                block_start: segment_ip,
                terminal: "jump",
            })?;
            let target_block =
                *segment_blocks
                    .get(target_ip)
                    .ok_or(AotCompileError::MissingSegment {
                        source_ip: segment_ip,
                        target_ip: *target_ip,
                    })?;
            store_vm_ip(
                b,
                vm_ptr,
                pointer_type,
                vm_ip_offset,
                i64::try_from(terminal_ip).map_err(|_| {
                    AotCompileError::Codegen("terminal ip does not fit i64".to_string())
                })?,
            );
            emit_interrupt_tick(
                b,
                vm_ptr,
                interrupt_ref,
                interrupt_helper_offset,
                pointer_type,
                exit_block,
            )?;
            let status = call_step_helper(
                b,
                vm_ptr,
                helper_ref,
                helper_offset,
                pointer_type,
                OP_JUMP,
                i64::try_from(*target_ip).map_err(|_| {
                    AotCompileError::Codegen("jump target ip does not fit i64".to_string())
                })?,
                0,
                0,
            )?;
            let trace_exit = b
                .ins()
                .icmp_imm(IntCC::Equal, status, STATUS_TRACE_EXIT as i64);
            let next = b.create_block();
            b.ins().brif(trace_exit, target_block, &[], next, &[]);
            b.switch_to_block(next);
            let ok = b
                .ins()
                .icmp_imm(IntCC::Equal, status, STATUS_CONTINUE as i64);
            let dead = b.create_block();
            b.ins()
                .brif(ok, dead, &[], exit_block, &[BlockArg::Value(status)]);
            b.switch_to_block(dead);
            let unexpected = b.ins().iconst(types::I32, STATUS_ERROR as i64);
            jump_with_status(b, exit_block, unexpected);
        }
        AotBlockTerminal::ConditionalJump {
            target_ip,
            fallthrough_ip,
        } => {
            let terminal_ip = terminal_ip.ok_or(AotCompileError::InvalidTerminal {
                block_start: segment_ip,
                terminal: "conditional jump",
            })?;
            let target_block =
                *segment_blocks
                    .get(target_ip)
                    .ok_or(AotCompileError::MissingSegment {
                        source_ip: segment_ip,
                        target_ip: *target_ip,
                    })?;
            let fallthrough_block =
                *segment_blocks
                    .get(fallthrough_ip)
                    .ok_or(AotCompileError::MissingSegment {
                        source_ip: segment_ip,
                        target_ip: *fallthrough_ip,
                    })?;
            store_vm_ip(
                b,
                vm_ptr,
                pointer_type,
                vm_ip_offset,
                i64::try_from(terminal_ip).map_err(|_| {
                    AotCompileError::Codegen("terminal ip does not fit i64".to_string())
                })?,
            );
            emit_interrupt_tick(
                b,
                vm_ptr,
                interrupt_ref,
                interrupt_helper_offset,
                pointer_type,
                exit_block,
            )?;
            let status = call_step_helper(
                b,
                vm_ptr,
                helper_ref,
                helper_offset,
                pointer_type,
                OP_GUARD_FALSE,
                i64::try_from(*target_ip).map_err(|_| {
                    AotCompileError::Codegen("branch target ip does not fit i64".to_string())
                })?,
                0,
                0,
            )?;
            let fallthrough = b
                .ins()
                .icmp_imm(IntCC::Equal, status, STATUS_CONTINUE as i64);
            let trace_exit = b
                .ins()
                .icmp_imm(IntCC::Equal, status, STATUS_TRACE_EXIT as i64);
            let check_trace_exit = b.create_block();
            b.ins()
                .brif(fallthrough, fallthrough_block, &[], check_trace_exit, &[]);
            b.switch_to_block(check_trace_exit);
            b.ins().brif(
                trace_exit,
                target_block,
                &[],
                exit_block,
                &[BlockArg::Value(status)],
            );
        }
        AotBlockTerminal::Fallthrough { next_ip } => {
            let next_block =
                *segment_blocks
                    .get(next_ip)
                    .ok_or(AotCompileError::MissingSegment {
                        source_ip: segment_ip,
                        target_ip: *next_ip,
                    })?;
            b.ins().jump(next_block, &[]);
        }
        AotBlockTerminal::Stop => {
            store_vm_ip(b, vm_ptr, pointer_type, vm_ip_offset, code_len_i64);
            let status = b.ins().iconst(types::I32, STATUS_ERROR as i64);
            jump_with_status(b, exit_block, status);
        }
    }
    Ok(())
}

#[cfg(feature = "cranelift-jit")]
fn emit_step(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: cranelift_codegen::ir::SigRef,
    vm_status_helper_ref: cranelift_codegen::ir::SigRef,
    helper_offset: i32,
    pointer_type: cranelift_codegen::ir::Type,
    layout: crate::vm::native::NativeStackLayout,
    offsets: crate::vm::native::ResolvedOffsets,
    heap_refs: HeapIntrinsicRefs,
    heap_addrs: HeapIntrinsicAddrs,
    exit_block: Block,
    step: &SegmentStep,
) -> Result<(), AotCompileError> {
    match &step.instruction {
        AotInstruction::Nop => {
            let next = b.create_block();
            b.ins().jump(next, &[]);
            b.switch_to_block(next);
        }
        AotInstruction::Call(call) => {
            let next = b.create_block();
            let (op, a, b_arg, c) = step_to_call_tuple(&AotInstruction::Call(call.clone()))?;
            let status = call_step_helper(
                b,
                vm_ptr,
                helper_ref,
                helper_offset,
                pointer_type,
                op,
                a,
                b_arg,
                c,
            )?;
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
        }
        instruction => {
            let inline_step = inline_step_for_instruction(instruction).ok_or_else(|| {
                AotCompileError::Codegen(format!(
                    "aot instruction {instruction:?} has no inline lowering or helper mapping"
                ))
            })?;
            emit_native_inline_step(
                b,
                InlineEmitCtx {
                    vm_ptr,
                    helper_ref,
                    _vm_status_helper_ref: vm_status_helper_ref,
                    exit_block,
                    pointer_type,
                    layout,
                    offsets,
                    heap_refs,
                    heap_addrs,
                },
                step.ip,
                inline_step,
            )
            .map_err(|err| AotCompileError::Codegen(err.to_string()))?;
        }
    }
    Ok(())
}

#[cfg(feature = "cranelift-jit")]
fn emit_interrupt_tick(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    interrupt_ref: cranelift_codegen::ir::SigRef,
    interrupt_helper_offset: i32,
    pointer_type: cranelift_codegen::ir::Type,
    exit_block: Block,
) -> Result<(), AotCompileError> {
    let next = b.create_block();
    let helper_ptr = b.ins().load(
        pointer_type,
        MemFlags::new(),
        vm_ptr,
        interrupt_helper_offset,
    );
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
fn call_step_helper(
    b: &mut FunctionBuilder,
    vm_ptr: cranelift_codegen::ir::Value,
    helper_ref: cranelift_codegen::ir::SigRef,
    helper_offset: i32,
    pointer_type: cranelift_codegen::ir::Type,
    op: i64,
    a: i64,
    b_arg: i64,
    c: i64,
) -> Result<cranelift_codegen::ir::Value, AotCompileError> {
    let helper_ptr = b
        .ins()
        .load(pointer_type, MemFlags::new(), vm_ptr, helper_offset);
    let op_val = b.ins().iconst(types::I64, op);
    let a_val = b.ins().iconst(types::I64, a);
    let b_val = b.ins().iconst(types::I64, b_arg);
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
    cached.clone().map_err(|err| AotCompileError::Codegen(err))
}

fn build_segments(
    program: &Program,
    lowered: &AotProgram,
) -> Result<Vec<AotSegment>, AotCompileError> {
    let mut segments = Vec::new();
    let mut seen = BTreeSet::new();

    for block in &lowered.blocks {
        let decoded_steps = decode_block_steps(program, block)?;
        let explicit_terminal_ip = terminal_ip(block);

        for (index, step) in decoded_steps.iter().enumerate() {
            let continuation = if let Some(next_step) = decoded_steps.get(index + 1) {
                SegmentContinuation::Direct(next_step.ip)
            } else {
                match &block.terminal {
                    AotBlockTerminal::Return
                    | AotBlockTerminal::Jump { .. }
                    | AotBlockTerminal::ConditionalJump { .. } => {
                        let terminal_ip =
                            explicit_terminal_ip.ok_or(AotCompileError::InvalidTerminal {
                                block_start: block.start_ip,
                                terminal: "explicit terminal ip",
                            })?;
                        SegmentContinuation::Direct(terminal_ip)
                    }
                    AotBlockTerminal::Fallthrough { next_ip } => {
                        SegmentContinuation::Direct(*next_ip)
                    }
                    AotBlockTerminal::Stop => SegmentContinuation::Terminal {
                        terminal: AotBlockTerminal::Stop,
                        terminal_ip: None,
                    },
                }
            };

            push_segment(
                &mut segments,
                &mut seen,
                AotSegment {
                    entry_ip: step.ip,
                    steps: vec![step.clone()],
                    continuation,
                },
            )?;
        }

        if decoded_steps.is_empty()
            || matches!(
                block.terminal,
                AotBlockTerminal::Return
                    | AotBlockTerminal::Jump { .. }
                    | AotBlockTerminal::ConditionalJump { .. }
            )
        {
            let entry_ip = explicit_terminal_ip.unwrap_or(block.start_ip);
            push_segment(
                &mut segments,
                &mut seen,
                AotSegment {
                    entry_ip,
                    steps: Vec::new(),
                    continuation: match &block.terminal {
                        AotBlockTerminal::Fallthrough { next_ip } => {
                            SegmentContinuation::Direct(*next_ip)
                        }
                        terminal => SegmentContinuation::Terminal {
                            terminal: terminal.clone(),
                            terminal_ip: explicit_terminal_ip,
                        },
                    },
                },
            )?;
        }
    }

    segments.sort_unstable_by_key(|segment| segment.entry_ip);
    Ok(segments)
}

fn push_segment(
    segments: &mut Vec<AotSegment>,
    seen: &mut BTreeSet<usize>,
    segment: AotSegment,
) -> Result<(), AotCompileError> {
    if !seen.insert(segment.entry_ip) {
        return Err(AotCompileError::DuplicateSegmentEntry {
            entry_ip: segment.entry_ip,
        });
    }
    segments.push(segment);
    Ok(())
}

fn decode_block_steps(
    program: &Program,
    block: &AotIrBlock,
) -> Result<Vec<SegmentStep>, AotCompileError> {
    let mut steps = Vec::with_capacity(block.instructions.len());
    let mut ip = block.start_ip;

    for instruction in &block.instructions {
        let opcode_byte = *program
            .code
            .get(ip)
            .ok_or(AotCompileError::InvalidInstructionIp {
                block_start: block.start_ip,
                ip,
            })?;
        let opcode =
            OpCode::try_from(opcode_byte).map_err(|_| AotCompileError::InvalidInstructionIp {
                block_start: block.start_ip,
                ip,
            })?;
        if matches!(opcode, OpCode::Ret | OpCode::Br | OpCode::Brfalse) {
            return Err(AotCompileError::InvalidInstructionIp {
                block_start: block.start_ip,
                ip,
            });
        }
        let next_ip = ip.checked_add(1 + opcode.operand_len()).ok_or(
            AotCompileError::InvalidInstructionIp {
                block_start: block.start_ip,
                ip,
            },
        )?;
        steps.push(SegmentStep {
            ip,
            instruction: instruction.clone(),
        });
        ip = next_ip;
    }

    Ok(steps)
}

fn terminal_ip(block: &AotIrBlock) -> Option<usize> {
    match block.terminal {
        AotBlockTerminal::Return => block.end_ip.checked_sub(1),
        AotBlockTerminal::Jump { .. } | AotBlockTerminal::ConditionalJump { .. } => {
            block.end_ip.checked_sub(5)
        }
        AotBlockTerminal::Fallthrough { .. } | AotBlockTerminal::Stop => None,
    }
}

#[cfg(feature = "cranelift-jit")]
fn inline_step_for_instruction(instruction: &AotInstruction) -> Option<NativeInlineStep> {
    Some(match instruction {
        AotInstruction::Nop | AotInstruction::Call(_) => return None,
        AotInstruction::Ldc { const_index } => NativeInlineStep::Ldc(*const_index),
        AotInstruction::Add => NativeInlineStep::Add,
        AotInstruction::IAdd => NativeInlineStep::IAdd,
        AotInstruction::FAdd => NativeInlineStep::FAdd,
        AotInstruction::Concat(AotConcatKind::String) => NativeInlineStep::StringConcat,
        AotInstruction::Concat(AotConcatKind::Bytes) => NativeInlineStep::BytesConcat,
        AotInstruction::Len(AotTextBytesKind::String) => NativeInlineStep::StringLen,
        AotInstruction::Len(AotTextBytesKind::Bytes) => NativeInlineStep::BytesLen,
        AotInstruction::Slice(AotTextBytesKind::String) => NativeInlineStep::StringSlice,
        AotInstruction::Slice(AotTextBytesKind::Bytes) => NativeInlineStep::BytesSlice,
        AotInstruction::Get(AotTextBytesKind::String) => NativeInlineStep::StringGet,
        AotInstruction::Get(AotTextBytesKind::Bytes) => NativeInlineStep::BytesGet,
        AotInstruction::HasBytes => NativeInlineStep::BytesHas,
        AotInstruction::BytesCodec(AotBytesCodecKind::FromArrayU8) => {
            NativeInlineStep::BytesFromArrayU8
        }
        AotInstruction::BytesCodec(AotBytesCodecKind::ToArrayU8) => {
            NativeInlineStep::BytesToArrayU8
        }
        AotInstruction::Sub => NativeInlineStep::Sub,
        AotInstruction::ISub => NativeInlineStep::ISub,
        AotInstruction::FSub => NativeInlineStep::FSub,
        AotInstruction::Mul => NativeInlineStep::Mul,
        AotInstruction::IMul => NativeInlineStep::IMul,
        AotInstruction::FMul => NativeInlineStep::FMul,
        AotInstruction::Div => NativeInlineStep::Div,
        AotInstruction::IDiv => NativeInlineStep::IDiv,
        AotInstruction::FDiv => NativeInlineStep::FDiv,
        AotInstruction::Mod => NativeInlineStep::Mod,
        AotInstruction::IMod => NativeInlineStep::IMod,
        AotInstruction::FMod => NativeInlineStep::FMod,
        AotInstruction::Shl => NativeInlineStep::Shl,
        AotInstruction::Shr => NativeInlineStep::Shr,
        AotInstruction::Lshr => NativeInlineStep::Lshr,
        AotInstruction::And => NativeInlineStep::And,
        AotInstruction::Or => NativeInlineStep::Or,
        AotInstruction::Not => NativeInlineStep::Not,
        AotInstruction::Neg => NativeInlineStep::Neg,
        AotInstruction::INeg => NativeInlineStep::INeg,
        AotInstruction::FNeg => NativeInlineStep::FNeg,
        AotInstruction::Ceq => NativeInlineStep::Ceq,
        AotInstruction::FCeq => NativeInlineStep::FCeq,
        AotInstruction::Clt => NativeInlineStep::Clt,
        AotInstruction::FClt => NativeInlineStep::FClt,
        AotInstruction::Cgt => NativeInlineStep::Cgt,
        AotInstruction::FCgt => NativeInlineStep::FCgt,
        AotInstruction::Pop => NativeInlineStep::Pop,
        AotInstruction::Dup => NativeInlineStep::Dup,
        AotInstruction::Ldloc { index } => NativeInlineStep::Ldloc(*index),
        AotInstruction::Stloc { index } => NativeInlineStep::Stloc(*index),
    })
}

#[cfg(feature = "cranelift-jit")]
fn step_to_call_tuple(
    instruction: &AotInstruction,
) -> Result<(i64, i64, i64, i64), AotCompileError> {
    Ok(match instruction {
        AotInstruction::Nop => (0, 0, 0, 0),
        AotInstruction::Ldc { const_index } => (OP_LDC, i64::from(*const_index), 0, 0),
        AotInstruction::Add | AotInstruction::IAdd | AotInstruction::FAdd => (OP_ADD, 0, 0, 0),
        AotInstruction::Sub | AotInstruction::ISub | AotInstruction::FSub => (OP_SUB, 0, 0, 0),
        AotInstruction::Mul | AotInstruction::IMul | AotInstruction::FMul => (OP_MUL, 0, 0, 0),
        AotInstruction::Div | AotInstruction::IDiv | AotInstruction::FDiv => (OP_DIV, 0, 0, 0),
        AotInstruction::Mod | AotInstruction::IMod | AotInstruction::FMod => (OP_MOD, 0, 0, 0),
        AotInstruction::Shl => (OP_SHL, 0, 0, 0),
        AotInstruction::Shr => (OP_SHR, 0, 0, 0),
        AotInstruction::Lshr => (OP_LSHR, 0, 0, 0),
        AotInstruction::And => (OP_AND, 0, 0, 0),
        AotInstruction::Or => (OP_OR, 0, 0, 0),
        AotInstruction::Not => (OP_NOT, 0, 0, 0),
        AotInstruction::Neg | AotInstruction::INeg | AotInstruction::FNeg => (OP_NEG, 0, 0, 0),
        AotInstruction::Ceq | AotInstruction::FCeq => (OP_CEQ, 0, 0, 0),
        AotInstruction::Clt | AotInstruction::FClt => (OP_CLT, 0, 0, 0),
        AotInstruction::Cgt | AotInstruction::FCgt => (OP_CGT, 0, 0, 0),
        AotInstruction::Pop => (OP_POP, 0, 0, 0),
        AotInstruction::Dup => (OP_DUP, 0, 0, 0),
        AotInstruction::Ldloc { index } => (OP_LDLOC, i64::from(*index), 0, 0),
        AotInstruction::Stloc { index } => (OP_STLOC, i64::from(*index), 0, 0),
        AotInstruction::Concat(_)
        | AotInstruction::Len(_)
        | AotInstruction::Slice(_)
        | AotInstruction::Get(_)
        | AotInstruction::HasBytes
        | AotInstruction::BytesCodec(_) => {
            return Err(AotCompileError::Codegen(
                "typed string/bytes aot instruction must lower natively".to_string(),
            ));
        }
        AotInstruction::Call(call) => {
            let op = match call.dispatch {
                AotCallDispatch::Builtin => OP_BUILTIN_CALL,
                AotCallDispatch::HostImport => OP_CALL,
            };
            (
                op,
                i64::from(call.index),
                i64::from(call.argc),
                i64::try_from(call.call_ip).map_err(|_| {
                    AotCompileError::Codegen("call ip does not fit i64".to_string())
                })?,
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BytecodeBuilder, Value};

    fn patch_branch_target(code: &mut [u8], instr_ip: u32, target: u32) {
        let start = instr_ip as usize + 1;
        code[start..start + 4].copy_from_slice(&target.to_le_bytes());
    }

    #[test]
    fn aot_segments_split_around_call_replay_and_resume_points() {
        let mut bc = BytecodeBuilder::new();
        bc.ldc(0);
        let call_ip = bc.position();
        bc.call(1024, 1);
        let resume_ip = bc.position();
        bc.ldc(1);
        bc.ret();

        let program = Program::new(vec![Value::Int(10), Value::Int(20)], bc.finish());
        let lowered = lower_program(&program).expect("lowering should succeed");
        let segments = build_segments(&program, &lowered).expect("segments should build");

        assert_eq!(
            segments
                .iter()
                .map(|segment| segment.entry_ip)
                .collect::<Vec<_>>(),
            vec![
                0,
                call_ip as usize,
                resume_ip as usize,
                resume_ip as usize + 5
            ]
        );
    }

    #[test]
    fn aot_segments_skip_empty_fallthrough_resume_segments() {
        let mut bc = BytecodeBuilder::new();
        bc.ldc(0);
        let branch_ip = bc.position();
        bc.brfalse(0);
        let true_ip = bc.position();
        let call_ip = bc.position();
        bc.call(1024, 0);
        let false_ip = bc.position();
        bc.ret();

        let mut code = bc.finish();
        patch_branch_target(&mut code, branch_ip, false_ip);
        let program = Program::new(vec![Value::Bool(true), Value::Int(7)], code);
        let lowered = lower_program(&program).expect("lowering should succeed");
        let segments = build_segments(&program, &lowered).expect("segments should build");

        assert!(
            segments
                .iter()
                .any(|segment| segment.entry_ip == true_ip as usize)
        );
        assert!(
            segments
                .iter()
                .any(|segment| segment.entry_ip == call_ip as usize)
        );
        assert!(
            segments
                .iter()
                .any(|segment| segment.entry_ip == false_ip as usize)
        );
    }
}
