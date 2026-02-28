use super::super::{Program, Value, Vm, VmError, VmResult};
use super::{
    NativeBackend, STATUS_CONTINUE, STATUS_ERROR, STATUS_HALTED, STATUS_TRACE_EXIT, STATUS_YIELDED,
};
use crate::builtins::BuiltinFunction;
use std::sync::OnceLock;

pub(super) struct X86_64Backend;

impl NativeBackend for X86_64Backend {
    type ExecutableMemory = BackendExecutableMemory;

    fn emit_trace_bytes(trace: &crate::jit::JitTrace) -> VmResult<Vec<u8>> {
        emit_native_trace_bytes(trace)
    }

    fn executable_memory_from_code(code: &[u8]) -> VmResult<Self::ExecutableMemory> {
        BackendExecutableMemory::from_code(code)
    }

    fn executable_memory_ptr(memory: &Self::ExecutableMemory) -> *mut u8 {
        memory.ptr
    }

    fn clear_bridge_error() {
        clear_bridge_error();
    }

    fn take_bridge_error() -> Option<VmError> {
        take_bridge_error()
    }
}

pub(super) struct BackendExecutableMemory {
    pub(super) ptr: *mut u8,
    len: usize,
}

impl BackendExecutableMemory {
    fn from_code(code: &[u8]) -> VmResult<Self> {
        let len = code.len();
        if len == 0 {
            return Err(VmError::JitNative(
                "cannot create executable region for empty code".to_string(),
            ));
        }
        let ptr = alloc_executable_region(len)?;
        write_machine_code(ptr, code)?;
        finalize_executable_region(ptr, len)?;
        Ok(Self { ptr, len })
    }
}

impl Drop for BackendExecutableMemory {
    fn drop(&mut self) {
        let _ = free_executable_region(self.ptr, self.len);
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
    int_tag: u32,
    float_tag: u32,
    bool_tag: u32,
    string_tag: u32,
    array_tag: u32,
    int_payload_offset: i32,
    float_payload_offset: i32,
    bool_payload_offset: i32,
    array_ptr_offset: i32,
    array_len_offset: i32,
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

static NATIVE_STACK_LAYOUT: OnceLock<Result<NativeStackLayout, String>> = OnceLock::new();

fn emit_native_trace_bytes(trace: &crate::jit::JitTrace) -> VmResult<Vec<u8>> {
    let mut code = Vec::with_capacity(512);
    let mut jump_patches: Vec<usize> = Vec::new();
    let layout = detect_native_stack_layout()?;

    emit_native_prologue(&mut code);

    let steps = &trace.steps;
    let mut step_index = 0usize;
    while step_index < steps.len() {
        match &steps[step_index] {
            crate::jit::TraceStep::Nop => {}
            crate::jit::TraceStep::Ldc(index) => {
                emit_native_step_ldc_inline(&mut code, layout, *index)?;
                emit_native_status_check(&mut code, &mut jump_patches);
            }
            crate::jit::TraceStep::Add => {
                emit_native_step_binary_numeric_inline(
                    &mut code,
                    layout,
                    NativeBinaryNumericOp::Add,
                )?;
                emit_native_status_check(&mut code, &mut jump_patches);
            }
            crate::jit::TraceStep::Sub => {
                emit_native_step_binary_numeric_inline(
                    &mut code,
                    layout,
                    NativeBinaryNumericOp::Sub,
                )?;
                emit_native_status_check(&mut code, &mut jump_patches);
            }
            crate::jit::TraceStep::Mul => {
                emit_native_step_binary_numeric_inline(
                    &mut code,
                    layout,
                    NativeBinaryNumericOp::Mul,
                )?;
                emit_native_status_check(&mut code, &mut jump_patches);
            }
            crate::jit::TraceStep::Div => {
                emit_native_step_binary_numeric_inline(
                    &mut code,
                    layout,
                    NativeBinaryNumericOp::Div,
                )?;
                emit_native_status_check(&mut code, &mut jump_patches);
            }
            crate::jit::TraceStep::Shl => {
                emit_native_step_shift_inline(&mut code, layout, true)?;
                emit_native_status_check(&mut code, &mut jump_patches);
            }
            crate::jit::TraceStep::Shr => {
                emit_native_step_shift_inline(&mut code, layout, false)?;
                emit_native_status_check(&mut code, &mut jump_patches);
            }
            crate::jit::TraceStep::Neg => {
                emit_native_step_neg_inline(&mut code, layout)?;
                emit_native_status_check(&mut code, &mut jump_patches);
            }
            crate::jit::TraceStep::Ceq => {
                emit_native_step_ceq_inline(&mut code, layout)?;
                emit_native_status_check(&mut code, &mut jump_patches);
            }
            crate::jit::TraceStep::Clt => {
                emit_native_step_binary_numeric_inline(
                    &mut code,
                    layout,
                    NativeBinaryNumericOp::Clt,
                )?;
                emit_native_status_check(&mut code, &mut jump_patches);
            }
            crate::jit::TraceStep::Cgt => {
                emit_native_step_binary_numeric_inline(
                    &mut code,
                    layout,
                    NativeBinaryNumericOp::Cgt,
                )?;
                emit_native_status_check(&mut code, &mut jump_patches);
            }
            crate::jit::TraceStep::Pop => {
                emit_native_step_pop_inline(&mut code, layout)?;
                emit_native_status_check(&mut code, &mut jump_patches);
            }
            crate::jit::TraceStep::Dup => {
                emit_native_step_dup_inline(&mut code, layout)?;
                emit_native_status_check(&mut code, &mut jump_patches);
            }
            crate::jit::TraceStep::Ldloc(index) => {
                emit_native_step_ldloc_inline(&mut code, layout, *index)?;
                emit_native_status_check(&mut code, &mut jump_patches);
            }
            crate::jit::TraceStep::Stloc(index) => {
                emit_native_step_stloc_inline(&mut code, layout, *index)?;
                emit_native_status_check(&mut code, &mut jump_patches);
            }
            crate::jit::TraceStep::Call {
                index,
                argc,
                call_ip,
            } => {
                emit_native_step_call_inline(&mut code, layout, *index, *argc, *call_ip)?;
                emit_native_status_check(&mut code, &mut jump_patches);
            }
            crate::jit::TraceStep::GuardFalse { exit_ip } => {
                let exit_ip = u32::try_from(*exit_ip).map_err(|_| {
                    VmError::JitNative(format!(
                        "guard exit ip {} exceeds u32 immediate range",
                        exit_ip
                    ))
                })?;
                emit_native_step_guard_false_inline(&mut code, layout, exit_ip)?;
                emit_native_status_check(&mut code, &mut jump_patches);
            }
            crate::jit::TraceStep::JumpToIp { target_ip } => {
                let target_ip = u32::try_from(*target_ip).map_err(|_| {
                    VmError::JitNative(format!(
                        "trace jump target ip {} exceeds u32 immediate range",
                        target_ip
                    ))
                })?;
                emit_native_step_jump_to_ip_inline(&mut code, layout, target_ip)?;
                emit_native_status_check(&mut code, &mut jump_patches);
            }
            crate::jit::TraceStep::JumpToRoot => {
                let root_ip = u32::try_from(trace.root_ip).map_err(|_| {
                    VmError::JitNative(format!(
                        "trace root ip {} exceeds u32 immediate range",
                        trace.root_ip
                    ))
                })?;
                emit_native_step_jump_to_ip_inline(&mut code, layout, root_ip)?;
                emit_native_status_check(&mut code, &mut jump_patches);
            }
            crate::jit::TraceStep::Ret => {
                emit_native_step_ret_inline(&mut code);
                emit_native_status_check(&mut code, &mut jump_patches);
            }
        }
        step_index += 1;
    }

    code.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax

    let return_label = code.len();
    for disp_offset in jump_patches {
        let rel = (return_label as i64) - ((disp_offset + 4) as i64);
        let rel = i32::try_from(rel)
            .map_err(|_| VmError::JitNative("native patch displacement overflow".to_string()))?;
        code[disp_offset..disp_offset + 4].copy_from_slice(&rel.to_le_bytes());
    }

    emit_native_epilogue(&mut code);
    Ok(code)
}

fn emit_native_prologue(code: &mut Vec<u8>) {
    code.push(0x53); // push rbx
    #[cfg(target_os = "windows")]
    {
        code.push(0x56); // push rsi
        code.push(0x57); // push rdi
        code.extend_from_slice(&[0x48, 0x89, 0xCB]); // mov rbx, rcx
        code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x20]); // sub rsp, 32
    }
    #[cfg(not(target_os = "windows"))]
    {
        code.extend_from_slice(&[0x48, 0x89, 0xFB]); // mov rbx, rdi
    }
}

fn emit_native_epilogue(code: &mut Vec<u8>) {
    #[cfg(target_os = "windows")]
    {
        code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x20]); // add rsp, 32
        code.push(0x5F); // pop rdi
        code.push(0x5E); // pop rsi
    }
    code.push(0x5B); // pop rbx
    code.push(0xC3); // ret
}

#[derive(Clone, Copy)]
enum NativeBinaryNumericOp {
    Add,
    Sub,
    Mul,
    Div,
    Clt,
    Cgt,
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

fn emit_jcc_rel32(code: &mut Vec<u8>, opcodes: [u8; 2]) -> usize {
    code.extend_from_slice(&opcodes);
    let disp = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    disp
}

fn emit_jmp_rel32(code: &mut Vec<u8>) -> usize {
    code.push(0xE9);
    let disp = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    disp
}

fn patch_rel32(code: &mut [u8], disp_offset: usize, target: usize) -> VmResult<()> {
    let rel = (target as i64) - ((disp_offset + 4) as i64);
    let rel = i32::try_from(rel)
        .map_err(|_| VmError::JitNative("native patch displacement overflow".to_string()))?;
    code[disp_offset..disp_offset + 4].copy_from_slice(&rel.to_le_bytes());
    Ok(())
}

fn emit_status_continue(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax
}

fn emit_status_error(code: &mut Vec<u8>) {
    code.push(0xB8); // mov eax, imm32
    code.extend_from_slice(&STATUS_ERROR.to_le_bytes());
}

fn emit_stack_binary_setup(
    code: &mut Vec<u8>,
    layout: NativeStackLayout,
    min_len: u8,
) -> VmResult<()> {
    let stack_len_offset = checked_add_i32(
        layout.vm_stack_offset,
        layout.stack_vec.len_offset,
        "stack len offset overflow",
    )?;
    let stack_ptr_offset = checked_add_i32(
        layout.vm_stack_offset,
        layout.stack_vec.ptr_offset,
        "stack ptr offset overflow",
    )?;
    code.extend_from_slice(&[0x48, 0x8B, 0x8B]); // mov rcx, [rbx+disp32]
    code.extend_from_slice(&stack_len_offset.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x83, 0xF9, min_len]); // cmp rcx, min_len
    let short_stack = emit_jcc_rel32(code, [0x0F, 0x82]); // jb
    code.extend_from_slice(&[0x48, 0x8B, 0x93]); // mov rdx, [rbx+disp32]
    code.extend_from_slice(&stack_ptr_offset.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8D, 0x41, 0xFF]); // lea rax, [rcx-1]
    code.extend_from_slice(&[0x48, 0x69, 0xC0]); // imul rax, rax, imm32
    code.extend_from_slice(&layout.value.size.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8D, 0x34, 0x02]); // lea rsi, [rdx+rax]
    code.extend_from_slice(&[0x48, 0x2D]); // sub rax, imm32
    code.extend_from_slice(&layout.value.size.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8D, 0x3C, 0x02]); // lea rdi, [rdx+rax]
    let ready = emit_jmp_rel32(code);
    let short_stack_label = code.len();
    emit_status_error(code);
    let short_stack_done = emit_jmp_rel32(code);
    let end = code.len();
    patch_rel32(code, short_stack, short_stack_label)?;
    patch_rel32(code, ready, end)?;
    patch_rel32(code, short_stack_done, end)?;
    Ok(())
}

fn emit_stack_top_setup(
    code: &mut Vec<u8>,
    layout: NativeStackLayout,
    min_len: u8,
) -> VmResult<()> {
    let stack_len_offset = checked_add_i32(
        layout.vm_stack_offset,
        layout.stack_vec.len_offset,
        "stack len offset overflow",
    )?;
    let stack_ptr_offset = checked_add_i32(
        layout.vm_stack_offset,
        layout.stack_vec.ptr_offset,
        "stack ptr offset overflow",
    )?;
    code.extend_from_slice(&[0x48, 0x8B, 0x8B]); // mov rcx, [rbx+disp32]
    code.extend_from_slice(&stack_len_offset.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x83, 0xF9, min_len]); // cmp rcx, min_len
    let short_stack = emit_jcc_rel32(code, [0x0F, 0x82]); // jb
    code.extend_from_slice(&[0x48, 0x8B, 0x93]); // mov rdx, [rbx+disp32]
    code.extend_from_slice(&stack_ptr_offset.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8D, 0x41, 0xFF]); // lea rax, [rcx-1]
    code.extend_from_slice(&[0x48, 0x69, 0xC0]); // imul rax, rax, imm32
    code.extend_from_slice(&layout.value.size.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8D, 0x3C, 0x02]); // lea rdi, [rdx+rax]
    let ready = emit_jmp_rel32(code);
    let short_stack_label = code.len();
    emit_status_error(code);
    let short_stack_done = emit_jmp_rel32(code);
    let end = code.len();
    patch_rel32(code, short_stack, short_stack_label)?;
    patch_rel32(code, ready, end)?;
    patch_rel32(code, short_stack_done, end)?;
    Ok(())
}

fn emit_adjust_stack_len_minus_one(code: &mut Vec<u8>, stack_len_offset: i32) {
    code.extend_from_slice(&[0x48, 0x8B, 0x83]); // mov rax, [rbx+disp32]
    code.extend_from_slice(&stack_len_offset.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xFF, 0xC8]); // dec rax
    code.extend_from_slice(&[0x48, 0x89, 0x83]); // mov [rbx+disp32], rax
    code.extend_from_slice(&stack_len_offset.to_le_bytes());
}

fn emit_load_tag_eax_from_rdi(code: &mut Vec<u8>, layout: ValueLayout) -> VmResult<()> {
    match layout.tag_size {
        1 => {
            code.extend_from_slice(&[0x0F, 0xB6, 0x87]); // movzx eax, byte [rdi+disp32]
            code.extend_from_slice(&layout.tag_offset.to_le_bytes());
        }
        2 => {
            code.extend_from_slice(&[0x0F, 0xB7, 0x87]); // movzx eax, word [rdi+disp32]
            code.extend_from_slice(&layout.tag_offset.to_le_bytes());
        }
        4 => {
            code.extend_from_slice(&[0x8B, 0x87]); // mov eax, dword [rdi+disp32]
            code.extend_from_slice(&layout.tag_offset.to_le_bytes());
        }
        other => {
            return Err(VmError::JitNative(format!(
                "unsupported native tag width {}",
                other
            )));
        }
    }
    Ok(())
}

fn emit_load_tag_edx_from_rsi(code: &mut Vec<u8>, layout: ValueLayout) -> VmResult<()> {
    match layout.tag_size {
        1 => {
            code.extend_from_slice(&[0x0F, 0xB6, 0x96]); // movzx edx, byte [rsi+disp32]
            code.extend_from_slice(&layout.tag_offset.to_le_bytes());
        }
        2 => {
            code.extend_from_slice(&[0x0F, 0xB7, 0x96]); // movzx edx, word [rsi+disp32]
            code.extend_from_slice(&layout.tag_offset.to_le_bytes());
        }
        4 => {
            code.extend_from_slice(&[0x8B, 0x96]); // mov edx, dword [rsi+disp32]
            code.extend_from_slice(&layout.tag_offset.to_le_bytes());
        }
        other => {
            return Err(VmError::JitNative(format!(
                "unsupported native tag width {}",
                other
            )));
        }
    }
    Ok(())
}

fn emit_store_tag_rdi(code: &mut Vec<u8>, layout: ValueLayout, tag: u32) -> VmResult<()> {
    match layout.tag_size {
        1 => {
            let tag = u8::try_from(tag).map_err(|_| {
                VmError::JitNative("native value tag out of byte range".to_string())
            })?;
            code.extend_from_slice(&[0xC6, 0x87]); // mov byte [rdi+disp32], imm8
            code.extend_from_slice(&layout.tag_offset.to_le_bytes());
            code.push(tag);
        }
        2 => {
            let tag = u16::try_from(tag).map_err(|_| {
                VmError::JitNative("native value tag out of word range".to_string())
            })?;
            code.extend_from_slice(&[0x66, 0xC7, 0x87]); // mov word [rdi+disp32], imm16
            code.extend_from_slice(&layout.tag_offset.to_le_bytes());
            code.extend_from_slice(&tag.to_le_bytes());
        }
        4 => {
            code.extend_from_slice(&[0xC7, 0x87]); // mov dword [rdi+disp32], imm32
            code.extend_from_slice(&layout.tag_offset.to_le_bytes());
            code.extend_from_slice(&tag.to_le_bytes());
        }
        other => {
            return Err(VmError::JitNative(format!(
                "unsupported native tag width {}",
                other
            )));
        }
    }
    Ok(())
}

fn emit_store_bool_from_al(code: &mut Vec<u8>, layout: ValueLayout) -> VmResult<()> {
    emit_store_tag_rdi(code, layout, layout.bool_tag)?;
    code.extend_from_slice(&[0x88, 0x87]); // mov [rdi+disp32], al
    code.extend_from_slice(&layout.bool_payload_offset.to_le_bytes());
    Ok(())
}

fn vec_ptr_disp(vec_base_offset: i32, vec_layout: VecLayout) -> VmResult<i32> {
    checked_add_i32(
        vec_base_offset,
        vec_layout.ptr_offset,
        "vec ptr offset overflow",
    )
}

fn vec_len_disp(vec_base_offset: i32, vec_layout: VecLayout) -> VmResult<i32> {
    checked_add_i32(
        vec_base_offset,
        vec_layout.len_offset,
        "vec len offset overflow",
    )
}

fn vec_cap_disp(vec_base_offset: i32, vec_layout: VecLayout) -> VmResult<i32> {
    checked_add_i32(
        vec_base_offset,
        vec_layout.cap_offset,
        "vec cap offset overflow",
    )
}

fn emit_copy_value_rsi_to_rdi(code: &mut Vec<u8>, value_layout: ValueLayout) -> VmResult<()> {
    let mut copied = 0i32;
    while copied + 8 <= value_layout.size {
        code.extend_from_slice(&[0x48, 0x8B, 0x86]); // mov rax, [rsi+disp32]
        code.extend_from_slice(&copied.to_le_bytes());
        code.extend_from_slice(&[0x48, 0x89, 0x87]); // mov [rdi+disp32], rax
        code.extend_from_slice(&copied.to_le_bytes());
        copied += 8;
    }
    let remaining = value_layout.size - copied;
    if remaining >= 4 {
        code.extend_from_slice(&[0x8B, 0x86]); // mov eax, [rsi+disp32]
        code.extend_from_slice(&copied.to_le_bytes());
        code.extend_from_slice(&[0x89, 0x87]); // mov [rdi+disp32], eax
        code.extend_from_slice(&copied.to_le_bytes());
        copied += 4;
    }
    if value_layout.size - copied >= 2 {
        code.extend_from_slice(&[0x0F, 0xB7, 0x86]); // movzx eax, word [rsi+disp32]
        code.extend_from_slice(&copied.to_le_bytes());
        code.extend_from_slice(&[0x66, 0x89, 0x87]); // mov [rdi+disp32], ax
        code.extend_from_slice(&copied.to_le_bytes());
        copied += 2;
    }
    if value_layout.size - copied >= 1 {
        code.extend_from_slice(&[0x0F, 0xB6, 0x86]); // movzx eax, byte [rsi+disp32]
        code.extend_from_slice(&copied.to_le_bytes());
        code.extend_from_slice(&[0x88, 0x87]); // mov [rdi+disp32], al
        code.extend_from_slice(&copied.to_le_bytes());
    }
    Ok(())
}

fn emit_native_step_binary_numeric_inline(
    code: &mut Vec<u8>,
    layout: NativeStackLayout,
    op: NativeBinaryNumericOp,
) -> VmResult<()> {
    let stack_len_offset = checked_add_i32(
        layout.vm_stack_offset,
        layout.stack_vec.len_offset,
        "stack len offset overflow",
    )?;
    emit_stack_binary_setup(code, layout, 2)?;

    emit_load_tag_eax_from_rdi(code, layout.value)?;
    code.extend_from_slice(&[0x3D]); // cmp eax, int_tag
    code.extend_from_slice(&layout.value.int_tag.to_le_bytes());
    let lhs_not_int = emit_jcc_rel32(code, [0x0F, 0x85]); // jne
    emit_load_tag_edx_from_rsi(code, layout.value)?;
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, int_tag
    code.extend_from_slice(&layout.value.int_tag.to_le_bytes());
    let rhs_not_int = emit_jcc_rel32(code, [0x0F, 0x85]); // jne

    match op {
        NativeBinaryNumericOp::Add => {
            code.extend_from_slice(&[0x48, 0x8B, 0x86]); // mov rax, [rsi+disp32]
            code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
            code.extend_from_slice(&[0x48, 0x01, 0x87]); // add [rdi+disp32], rax
            code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
            emit_store_tag_rdi(code, layout.value, layout.value.int_tag)?;
            emit_adjust_stack_len_minus_one(code, stack_len_offset);
            emit_status_continue(code);
        }
        NativeBinaryNumericOp::Sub => {
            code.extend_from_slice(&[0x48, 0x8B, 0x87]); // mov rax, [rdi+disp32]
            code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
            code.extend_from_slice(&[0x48, 0x2B, 0x86]); // sub rax, [rsi+disp32]
            code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
            code.extend_from_slice(&[0x48, 0x89, 0x87]); // mov [rdi+disp32], rax
            code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
            emit_store_tag_rdi(code, layout.value, layout.value.int_tag)?;
            emit_adjust_stack_len_minus_one(code, stack_len_offset);
            emit_status_continue(code);
        }
        NativeBinaryNumericOp::Mul => {
            code.extend_from_slice(&[0x48, 0x8B, 0x87]); // mov rax, [rdi+disp32]
            code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
            code.extend_from_slice(&[0x48, 0x0F, 0xAF, 0x86]); // imul rax, [rsi+disp32]
            code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
            code.extend_from_slice(&[0x48, 0x89, 0x87]); // mov [rdi+disp32], rax
            code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
            emit_store_tag_rdi(code, layout.value, layout.value.int_tag)?;
            emit_adjust_stack_len_minus_one(code, stack_len_offset);
            emit_status_continue(code);
        }
        NativeBinaryNumericOp::Div => {
            code.extend_from_slice(&[0x48, 0x83, 0xBE]); // cmp qword [rsi+disp32], imm8
            code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
            code.push(0x00);
            let int_div_zero = emit_jcc_rel32(code, [0x0F, 0x84]); // je
            code.extend_from_slice(&[0x48, 0x8B, 0x87]); // mov rax, [rdi+disp32]
            code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
            code.extend_from_slice(&[0x48, 0xB9]); // mov rcx, imm64
            code.extend_from_slice(&(i64::MIN as u64).to_le_bytes());
            code.extend_from_slice(&[0x48, 0x39, 0xC8]); // cmp rax, rcx
            let lhs_not_min = emit_jcc_rel32(code, [0x0F, 0x85]); // jne
            code.extend_from_slice(&[0x48, 0x83, 0xBE]); // cmp qword [rsi+disp32], imm8
            code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
            code.push(0xFF);
            let rhs_not_minus_one = emit_jcc_rel32(code, [0x0F, 0x85]); // jne
            code.extend_from_slice(&[0x48, 0x89, 0x87]); // mov [rdi+disp32], rax
            code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
            emit_store_tag_rdi(code, layout.value, layout.value.int_tag)?;
            emit_adjust_stack_len_minus_one(code, stack_len_offset);
            emit_status_continue(code);
            let int_overflow_ok = emit_jmp_rel32(code);
            let int_div = code.len();
            code.extend_from_slice(&[0x48, 0x99]); // cqo
            code.extend_from_slice(&[0x48, 0xF7, 0xBE]); // idiv qword [rsi+disp32]
            code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
            code.extend_from_slice(&[0x48, 0x89, 0x87]); // mov [rdi+disp32], rax
            code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
            emit_store_tag_rdi(code, layout.value, layout.value.int_tag)?;
            emit_adjust_stack_len_minus_one(code, stack_len_offset);
            emit_status_continue(code);
            let int_ok = emit_jmp_rel32(code);
            let int_div_zero_label = code.len();
            emit_status_error(code);
            let int_div_zero_done = emit_jmp_rel32(code);
            let int_div_end = code.len();
            patch_rel32(code, int_div_zero, int_div_zero_label)?;
            patch_rel32(code, lhs_not_min, int_div)?;
            patch_rel32(code, rhs_not_minus_one, int_div)?;
            patch_rel32(code, int_overflow_ok, int_div_end)?;
            patch_rel32(code, int_ok, int_div_end)?;
            patch_rel32(code, int_div_zero_done, int_div_end)?;
        }
        NativeBinaryNumericOp::Clt | NativeBinaryNumericOp::Cgt => {
            code.extend_from_slice(&[0x48, 0x8B, 0x87]); // mov rax, [rdi+disp32]
            code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
            code.extend_from_slice(&[0x48, 0x3B, 0x86]); // cmp rax, [rsi+disp32]
            code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
            if matches!(op, NativeBinaryNumericOp::Clt) {
                code.extend_from_slice(&[0x0F, 0x9C, 0xC0]); // setl al
            } else {
                code.extend_from_slice(&[0x0F, 0x9F, 0xC0]); // setg al
            }
            emit_store_bool_from_al(code, layout.value)?;
            emit_adjust_stack_len_minus_one(code, stack_len_offset);
            emit_status_continue(code);
        }
    }
    let int_done = emit_jmp_rel32(code);

    let float_dispatch = code.len();
    patch_rel32(code, lhs_not_int, float_dispatch)?;
    patch_rel32(code, rhs_not_int, float_dispatch)?;

    emit_load_tag_eax_from_rdi(code, layout.value)?;
    code.extend_from_slice(&[0x3D]); // cmp eax, int_tag
    code.extend_from_slice(&layout.value.int_tag.to_le_bytes());
    let lhs_is_int = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x3D]); // cmp eax, float_tag
    code.extend_from_slice(&layout.value.float_tag.to_le_bytes());
    let lhs_not_float = emit_jcc_rel32(code, [0x0F, 0x85]); // jne

    let lhs_float = code.len();
    code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x87]); // movsd xmm0, [rdi+disp32]
    code.extend_from_slice(&layout.value.float_payload_offset.to_le_bytes());
    let lhs_float_ready = emit_jmp_rel32(code);

    let lhs_int = code.len();
    code.extend_from_slice(&[0x48, 0x8B, 0x87]); // mov rax, [rdi+disp32]
    code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
    code.extend_from_slice(&[0xF2, 0x48, 0x0F, 0x2A, 0xC0]); // cvtsi2sd xmm0, rax

    let rhs_dispatch = code.len();
    patch_rel32(code, lhs_is_int, lhs_int)?;
    patch_rel32(code, lhs_not_float, rhs_dispatch)?;
    patch_rel32(code, lhs_float_ready, rhs_dispatch)?;
    let _ = lhs_float;

    emit_load_tag_edx_from_rsi(code, layout.value)?;
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, int_tag
    code.extend_from_slice(&layout.value.int_tag.to_le_bytes());
    let rhs_is_int = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, float_tag
    code.extend_from_slice(&layout.value.float_tag.to_le_bytes());
    let rhs_not_float = emit_jcc_rel32(code, [0x0F, 0x85]); // jne
    code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x8E]); // movsd xmm1, [rsi+disp32]
    code.extend_from_slice(&layout.value.float_payload_offset.to_le_bytes());
    let rhs_ready = emit_jmp_rel32(code);

    let rhs_int = code.len();
    code.extend_from_slice(&[0x48, 0x8B, 0x86]); // mov rax, [rsi+disp32]
    code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
    code.extend_from_slice(&[0xF2, 0x48, 0x0F, 0x2A, 0xC8]); // cvtsi2sd xmm1, rax
    let rhs_int_ready = emit_jmp_rel32(code);

    let float_body = code.len();
    patch_rel32(code, rhs_is_int, rhs_int)?;
    patch_rel32(code, rhs_not_float, float_body)?;
    patch_rel32(code, rhs_ready, float_body)?;
    patch_rel32(code, rhs_int_ready, float_body)?;

    match op {
        NativeBinaryNumericOp::Add => {
            code.extend_from_slice(&[0xF2, 0x0F, 0x58, 0xC1]); // addsd xmm0, xmm1
            emit_store_tag_rdi(code, layout.value, layout.value.float_tag)?;
            code.extend_from_slice(&[0xF2, 0x0F, 0x11, 0x87]); // movsd [rdi+disp32], xmm0
            code.extend_from_slice(&layout.value.float_payload_offset.to_le_bytes());
            emit_adjust_stack_len_minus_one(code, stack_len_offset);
            emit_status_continue(code);
        }
        NativeBinaryNumericOp::Sub => {
            code.extend_from_slice(&[0xF2, 0x0F, 0x5C, 0xC1]); // subsd xmm0, xmm1
            emit_store_tag_rdi(code, layout.value, layout.value.float_tag)?;
            code.extend_from_slice(&[0xF2, 0x0F, 0x11, 0x87]); // movsd [rdi+disp32], xmm0
            code.extend_from_slice(&layout.value.float_payload_offset.to_le_bytes());
            emit_adjust_stack_len_minus_one(code, stack_len_offset);
            emit_status_continue(code);
        }
        NativeBinaryNumericOp::Mul => {
            code.extend_from_slice(&[0xF2, 0x0F, 0x59, 0xC1]); // mulsd xmm0, xmm1
            emit_store_tag_rdi(code, layout.value, layout.value.float_tag)?;
            code.extend_from_slice(&[0xF2, 0x0F, 0x11, 0x87]); // movsd [rdi+disp32], xmm0
            code.extend_from_slice(&layout.value.float_payload_offset.to_le_bytes());
            emit_adjust_stack_len_minus_one(code, stack_len_offset);
            emit_status_continue(code);
        }
        NativeBinaryNumericOp::Div => {
            code.extend_from_slice(&[0x66, 0x0F, 0x57, 0xD2]); // xorpd xmm2, xmm2
            code.extend_from_slice(&[0x66, 0x0F, 0x2E, 0xCA]); // ucomisd xmm1, xmm2
            let float_div_zero = emit_jcc_rel32(code, [0x0F, 0x84]); // je
            code.extend_from_slice(&[0xF2, 0x0F, 0x5E, 0xC1]); // divsd xmm0, xmm1
            emit_store_tag_rdi(code, layout.value, layout.value.float_tag)?;
            code.extend_from_slice(&[0xF2, 0x0F, 0x11, 0x87]); // movsd [rdi+disp32], xmm0
            code.extend_from_slice(&layout.value.float_payload_offset.to_le_bytes());
            emit_adjust_stack_len_minus_one(code, stack_len_offset);
            emit_status_continue(code);
            let div_ok = emit_jmp_rel32(code);
            let div_zero_label = code.len();
            emit_status_error(code);
            let div_zero_done = emit_jmp_rel32(code);
            let div_end = code.len();
            patch_rel32(code, float_div_zero, div_zero_label)?;
            patch_rel32(code, div_ok, div_end)?;
            patch_rel32(code, div_zero_done, div_end)?;
        }
        NativeBinaryNumericOp::Clt | NativeBinaryNumericOp::Cgt => {
            code.extend_from_slice(&[0x66, 0x0F, 0x2E, 0xC1]); // ucomisd xmm0, xmm1
            code.extend_from_slice(&[0x0F, 0x9A, 0xC2]); // setp dl
            if matches!(op, NativeBinaryNumericOp::Clt) {
                code.extend_from_slice(&[0x0F, 0x92, 0xC0]); // setb al
            } else {
                code.extend_from_slice(&[0x0F, 0x97, 0xC0]); // seta al
            }
            code.extend_from_slice(&[0xF6, 0xD2]); // not dl
            code.extend_from_slice(&[0x20, 0xD0]); // and al, dl
            emit_store_bool_from_al(code, layout.value)?;
            emit_adjust_stack_len_minus_one(code, stack_len_offset);
            emit_status_continue(code);
        }
    }
    let float_done = emit_jmp_rel32(code);

    let error_label = code.len();
    emit_status_error(code);
    let done_label = code.len();
    patch_rel32(code, lhs_not_float, error_label)?;
    patch_rel32(code, rhs_not_float, error_label)?;
    patch_rel32(code, int_done, done_label)?;
    patch_rel32(code, float_done, done_label)?;
    Ok(())
}

fn emit_native_step_neg_inline(code: &mut Vec<u8>, layout: NativeStackLayout) -> VmResult<()> {
    emit_stack_top_setup(code, layout, 1)?;
    emit_load_tag_eax_from_rdi(code, layout.value)?;
    code.extend_from_slice(&[0x3D]); // cmp eax, int_tag
    code.extend_from_slice(&layout.value.int_tag.to_le_bytes());
    let not_int = emit_jcc_rel32(code, [0x0F, 0x85]); // jne

    code.extend_from_slice(&[0x48, 0xF7, 0x9F]); // neg qword [rdi+disp32]
    code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
    emit_store_tag_rdi(code, layout.value, layout.value.int_tag)?;
    emit_status_continue(code);
    let int_done = emit_jmp_rel32(code);

    let float_check = code.len();
    patch_rel32(code, not_int, float_check)?;
    emit_load_tag_eax_from_rdi(code, layout.value)?;
    code.extend_from_slice(&[0x3D]); // cmp eax, float_tag
    code.extend_from_slice(&layout.value.float_tag.to_le_bytes());
    let not_float = emit_jcc_rel32(code, [0x0F, 0x85]); // jne

    code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x87]); // movsd xmm0, [rdi+disp32]
    code.extend_from_slice(&layout.value.float_payload_offset.to_le_bytes());
    code.extend_from_slice(&[0x66, 0x0F, 0x57, 0xC9]); // xorpd xmm1, xmm1
    code.extend_from_slice(&[0xF2, 0x0F, 0x5C, 0xC8]); // subsd xmm1, xmm0
    emit_store_tag_rdi(code, layout.value, layout.value.float_tag)?;
    code.extend_from_slice(&[0xF2, 0x0F, 0x11, 0x8F]); // movsd [rdi+disp32], xmm1
    code.extend_from_slice(&layout.value.float_payload_offset.to_le_bytes());
    emit_status_continue(code);
    let float_done = emit_jmp_rel32(code);

    let error_label = code.len();
    emit_status_error(code);
    let done_label = code.len();
    patch_rel32(code, not_float, error_label)?;
    patch_rel32(code, int_done, done_label)?;
    patch_rel32(code, float_done, done_label)?;
    Ok(())
}

fn emit_native_step_shift_inline(
    code: &mut Vec<u8>,
    layout: NativeStackLayout,
    is_shl: bool,
) -> VmResult<()> {
    let stack_len_offset = checked_add_i32(
        layout.vm_stack_offset,
        layout.stack_vec.len_offset,
        "stack len offset overflow",
    )?;
    emit_stack_binary_setup(code, layout, 2)?;
    emit_load_tag_eax_from_rdi(code, layout.value)?;
    code.extend_from_slice(&[0x3D]); // cmp eax, int_tag
    code.extend_from_slice(&layout.value.int_tag.to_le_bytes());
    let lhs_not_int = emit_jcc_rel32(code, [0x0F, 0x85]); // jne
    emit_load_tag_edx_from_rsi(code, layout.value)?;
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, int_tag
    code.extend_from_slice(&layout.value.int_tag.to_le_bytes());
    let rhs_not_int = emit_jcc_rel32(code, [0x0F, 0x85]); // jne

    code.extend_from_slice(&[0x4C, 0x8B, 0x86]); // mov r8, [rsi+disp32]
    code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
    code.extend_from_slice(&[0x4D, 0x85, 0xC0]); // test r8, r8
    let neg_shift = emit_jcc_rel32(code, [0x0F, 0x88]); // js
    code.extend_from_slice(&[0x49, 0x83, 0xF8, 0x3F]); // cmp r8, 63
    let big_shift = emit_jcc_rel32(code, [0x0F, 0x87]); // ja

    code.extend_from_slice(&[0x48, 0x8B, 0x87]); // mov rax, [rdi+disp32]
    code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
    code.extend_from_slice(&[0x44, 0x89, 0xC1]); // mov ecx, r8d
    if is_shl {
        code.extend_from_slice(&[0x48, 0xD3, 0xE0]); // shl rax, cl
    } else {
        code.extend_from_slice(&[0x48, 0xD3, 0xF8]); // sar rax, cl
    }
    code.extend_from_slice(&[0x48, 0x89, 0x87]); // mov [rdi+disp32], rax
    code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
    emit_store_tag_rdi(code, layout.value, layout.value.int_tag)?;
    emit_adjust_stack_len_minus_one(code, stack_len_offset);
    emit_status_continue(code);
    let ok_done = emit_jmp_rel32(code);

    let error_label = code.len();
    emit_status_error(code);
    let done_label = code.len();
    patch_rel32(code, lhs_not_int, error_label)?;
    patch_rel32(code, rhs_not_int, error_label)?;
    patch_rel32(code, neg_shift, error_label)?;
    patch_rel32(code, big_shift, error_label)?;
    patch_rel32(code, ok_done, done_label)?;
    Ok(())
}

fn emit_native_step_pop_inline(code: &mut Vec<u8>, layout: NativeStackLayout) -> VmResult<()> {
    let stack_len_offset = vec_len_disp(layout.vm_stack_offset, layout.stack_vec)?;
    emit_stack_top_setup(code, layout, 1)?;
    emit_load_tag_eax_from_rdi(code, layout.value)?;
    code.extend_from_slice(&[0x3D]); // cmp eax, int_tag
    code.extend_from_slice(&layout.value.int_tag.to_le_bytes());
    let primitive = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x3D]); // cmp eax, float_tag
    code.extend_from_slice(&layout.value.float_tag.to_le_bytes());
    let primitive_float = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x3D]); // cmp eax, bool_tag
    code.extend_from_slice(&layout.value.bool_tag.to_le_bytes());
    let primitive_bool = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    let fallback = emit_jmp_rel32(code);

    let primitive_label = code.len();
    code.extend_from_slice(&[0x48, 0xFF, 0xC9]); // dec rcx
    code.extend_from_slice(&[0x48, 0x89, 0x8B]); // mov [rbx+disp32], rcx
    code.extend_from_slice(&stack_len_offset.to_le_bytes());
    emit_status_continue(code);
    let done = emit_jmp_rel32(code);

    let fallback_label = code.len();
    let helper_addr = helper_ptr_to_u64(jit_native_pop_bridge as *const (), "pop helper")?;
    emit_vm_helper_call0(code, helper_addr);
    let done_label = code.len();

    patch_rel32(code, primitive, primitive_label)?;
    patch_rel32(code, primitive_float, primitive_label)?;
    patch_rel32(code, primitive_bool, primitive_label)?;
    patch_rel32(code, fallback, fallback_label)?;
    patch_rel32(code, done, done_label)?;
    Ok(())
}

fn emit_native_step_dup_inline(code: &mut Vec<u8>, layout: NativeStackLayout) -> VmResult<()> {
    let stack_len_offset = vec_len_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_ptr_offset = vec_ptr_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_cap_offset = vec_cap_disp(layout.vm_stack_offset, layout.stack_vec)?;

    code.extend_from_slice(&[0x48, 0x8B, 0x8B]); // mov rcx, [rbx+disp32]
    code.extend_from_slice(&stack_len_offset.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x83, 0xF9, 0x01]); // cmp rcx, 1
    let underflow = emit_jcc_rel32(code, [0x0F, 0x82]); // jb
    code.extend_from_slice(&[0x4C, 0x8B, 0x83]); // mov r8, [rbx+disp32]
    code.extend_from_slice(&stack_cap_offset.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x39, 0xC1]); // cmp rcx, r8
    let no_cap = emit_jcc_rel32(code, [0x0F, 0x83]); // jae
    code.extend_from_slice(&[0x48, 0x8B, 0x93]); // mov rdx, [rbx+disp32]
    code.extend_from_slice(&stack_ptr_offset.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8D, 0x41, 0xFF]); // lea rax, [rcx-1]
    code.extend_from_slice(&[0x48, 0x69, 0xC0]); // imul rax, rax, imm32
    code.extend_from_slice(&layout.value.size.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8D, 0x34, 0x02]); // lea rsi, [rdx+rax]
    emit_load_tag_edx_from_rsi(code, layout.value)?;
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, int_tag
    code.extend_from_slice(&layout.value.int_tag.to_le_bytes());
    let primitive = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, float_tag
    code.extend_from_slice(&layout.value.float_tag.to_le_bytes());
    let primitive_float = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, bool_tag
    code.extend_from_slice(&layout.value.bool_tag.to_le_bytes());
    let primitive_bool = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    let fallback = emit_jmp_rel32(code);

    let primitive_label = code.len();
    code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
    code.extend_from_slice(&[0x48, 0x69, 0xC0]); // imul rax, rax, imm32
    code.extend_from_slice(&layout.value.size.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8D, 0x3C, 0x02]); // lea rdi, [rdx+rax]
    emit_copy_value_rsi_to_rdi(code, layout.value)?;
    code.extend_from_slice(&[0x48, 0xFF, 0xC1]); // inc rcx
    code.extend_from_slice(&[0x48, 0x89, 0x8B]); // mov [rbx+disp32], rcx
    code.extend_from_slice(&stack_len_offset.to_le_bytes());
    emit_status_continue(code);
    let done = emit_jmp_rel32(code);

    let fallback_label = code.len();
    let helper_addr = helper_ptr_to_u64(jit_native_dup_bridge as *const (), "dup helper")?;
    emit_vm_helper_call0(code, helper_addr);
    let done_label = code.len();

    patch_rel32(code, underflow, fallback_label)?;
    patch_rel32(code, no_cap, fallback_label)?;
    patch_rel32(code, primitive, primitive_label)?;
    patch_rel32(code, primitive_float, primitive_label)?;
    patch_rel32(code, primitive_bool, primitive_label)?;
    patch_rel32(code, fallback, fallback_label)?;
    patch_rel32(code, done, done_label)?;
    Ok(())
}

fn emit_native_step_ldc_inline(
    code: &mut Vec<u8>,
    layout: NativeStackLayout,
    const_index: u32,
) -> VmResult<()> {
    let stack_len_offset = vec_len_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_ptr_offset = vec_ptr_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_cap_offset = vec_cap_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let constants_base = checked_add_i32(
        layout.vm_program_offset,
        layout.program_constants_offset,
        "vm constants base overflow",
    )?;
    let constants_len_offset = vec_len_disp(constants_base, layout.stack_vec)?;
    let constants_ptr_offset = vec_ptr_disp(constants_base, layout.stack_vec)?;

    code.extend_from_slice(&[0x4C, 0x8B, 0x83]); // mov r8, [rbx+disp32] ; constants len
    code.extend_from_slice(&constants_len_offset.to_le_bytes());
    code.push(0xB8); // mov eax, imm32
    code.extend_from_slice(&const_index.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x39, 0xC0]); // cmp rax, r8
    let bad_index = emit_jcc_rel32(code, [0x0F, 0x83]); // jae

    code.extend_from_slice(&[0x48, 0x8B, 0x8B]); // mov rcx, [rbx+disp32] ; stack len
    code.extend_from_slice(&stack_len_offset.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x8B, 0x83]); // mov r8, [rbx+disp32] ; stack cap
    code.extend_from_slice(&stack_cap_offset.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x39, 0xC1]); // cmp rcx, r8
    let no_cap = emit_jcc_rel32(code, [0x0F, 0x83]); // jae

    code.extend_from_slice(&[0x4C, 0x8B, 0x8B]); // mov r9, [rbx+disp32] ; stack ptr
    code.extend_from_slice(&stack_ptr_offset.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x8B, 0x93]); // mov r10, [rbx+disp32] ; constants ptr
    code.extend_from_slice(&constants_ptr_offset.to_le_bytes());
    code.push(0xB8); // mov eax, imm32
    code.extend_from_slice(&const_index.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x69, 0xC0]); // imul rax, rax, imm32
    code.extend_from_slice(&layout.value.size.to_le_bytes());
    code.extend_from_slice(&[0x49, 0x8D, 0x34, 0x02]); // lea rsi, [r10+rax]
    emit_load_tag_edx_from_rsi(code, layout.value)?;
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, int_tag
    code.extend_from_slice(&layout.value.int_tag.to_le_bytes());
    let primitive = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, float_tag
    code.extend_from_slice(&layout.value.float_tag.to_le_bytes());
    let primitive_float = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, bool_tag
    code.extend_from_slice(&layout.value.bool_tag.to_le_bytes());
    let primitive_bool = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    let fallback = emit_jmp_rel32(code);

    let primitive_label = code.len();
    code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
    code.extend_from_slice(&[0x48, 0x69, 0xC0]); // imul rax, rax, imm32
    code.extend_from_slice(&layout.value.size.to_le_bytes());
    code.extend_from_slice(&[0x49, 0x8D, 0x3C, 0x01]); // lea rdi, [r9+rax]
    emit_copy_value_rsi_to_rdi(code, layout.value)?;
    code.extend_from_slice(&[0x48, 0xFF, 0xC1]); // inc rcx
    code.extend_from_slice(&[0x48, 0x89, 0x8B]); // mov [rbx+disp32], rcx
    code.extend_from_slice(&stack_len_offset.to_le_bytes());
    emit_status_continue(code);
    let done = emit_jmp_rel32(code);

    let fallback_label = code.len();
    let helper_addr = helper_ptr_to_u64(jit_native_ldc_bridge as *const (), "ldc helper")?;
    emit_vm_helper_call1_u32(code, helper_addr, const_index);
    let done_label = code.len();

    patch_rel32(code, bad_index, fallback_label)?;
    patch_rel32(code, no_cap, fallback_label)?;
    patch_rel32(code, primitive, primitive_label)?;
    patch_rel32(code, primitive_float, primitive_label)?;
    patch_rel32(code, primitive_bool, primitive_label)?;
    patch_rel32(code, fallback, fallback_label)?;
    patch_rel32(code, done, done_label)?;
    Ok(())
}

fn emit_native_step_ldloc_inline(
    code: &mut Vec<u8>,
    layout: NativeStackLayout,
    local_index: u8,
) -> VmResult<()> {
    let stack_len_offset = vec_len_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_ptr_offset = vec_ptr_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_cap_offset = vec_cap_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let locals_len_offset = vec_len_disp(layout.vm_locals_offset, layout.stack_vec)?;
    let locals_ptr_offset = vec_ptr_disp(layout.vm_locals_offset, layout.stack_vec)?;

    code.extend_from_slice(&[0x4C, 0x8B, 0x83]); // mov r8, [rbx+disp32] ; locals len
    code.extend_from_slice(&locals_len_offset.to_le_bytes());
    code.push(0xB8); // mov eax, imm32
    code.extend_from_slice(&(local_index as u32).to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x39, 0xC0]); // cmp rax, r8
    let bad_index = emit_jcc_rel32(code, [0x0F, 0x83]); // jae

    code.extend_from_slice(&[0x48, 0x8B, 0x8B]); // mov rcx, [rbx+disp32] ; stack len
    code.extend_from_slice(&stack_len_offset.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x8B, 0x83]); // mov r8, [rbx+disp32] ; stack cap
    code.extend_from_slice(&stack_cap_offset.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x39, 0xC1]); // cmp rcx, r8
    let no_cap = emit_jcc_rel32(code, [0x0F, 0x83]); // jae

    code.extend_from_slice(&[0x4C, 0x8B, 0x8B]); // mov r9, [rbx+disp32] ; stack ptr
    code.extend_from_slice(&stack_ptr_offset.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x8B, 0x93]); // mov r10, [rbx+disp32] ; locals ptr
    code.extend_from_slice(&locals_ptr_offset.to_le_bytes());
    code.push(0xB8); // mov eax, imm32
    code.extend_from_slice(&(local_index as u32).to_le_bytes());
    code.extend_from_slice(&[0x48, 0x69, 0xC0]); // imul rax, rax, imm32
    code.extend_from_slice(&layout.value.size.to_le_bytes());
    code.extend_from_slice(&[0x49, 0x8D, 0x34, 0x02]); // lea rsi, [r10+rax]
    emit_load_tag_edx_from_rsi(code, layout.value)?;
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, int_tag
    code.extend_from_slice(&layout.value.int_tag.to_le_bytes());
    let primitive = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, float_tag
    code.extend_from_slice(&layout.value.float_tag.to_le_bytes());
    let primitive_float = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, bool_tag
    code.extend_from_slice(&layout.value.bool_tag.to_le_bytes());
    let primitive_bool = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    let fallback = emit_jmp_rel32(code);

    let primitive_label = code.len();
    code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
    code.extend_from_slice(&[0x48, 0x69, 0xC0]); // imul rax, rax, imm32
    code.extend_from_slice(&layout.value.size.to_le_bytes());
    code.extend_from_slice(&[0x49, 0x8D, 0x3C, 0x01]); // lea rdi, [r9+rax]
    emit_copy_value_rsi_to_rdi(code, layout.value)?;
    code.extend_from_slice(&[0x48, 0xFF, 0xC1]); // inc rcx
    code.extend_from_slice(&[0x48, 0x89, 0x8B]); // mov [rbx+disp32], rcx
    code.extend_from_slice(&stack_len_offset.to_le_bytes());
    emit_status_continue(code);
    let done = emit_jmp_rel32(code);

    let fallback_label = code.len();
    let helper_addr = helper_ptr_to_u64(jit_native_ldloc_bridge as *const (), "ldloc helper")?;
    emit_vm_helper_call1_u32(code, helper_addr, local_index as u32);
    let done_label = code.len();

    patch_rel32(code, bad_index, fallback_label)?;
    patch_rel32(code, no_cap, fallback_label)?;
    patch_rel32(code, primitive, primitive_label)?;
    patch_rel32(code, primitive_float, primitive_label)?;
    patch_rel32(code, primitive_bool, primitive_label)?;
    patch_rel32(code, fallback, fallback_label)?;
    patch_rel32(code, done, done_label)?;
    Ok(())
}

fn emit_native_step_stloc_inline(
    code: &mut Vec<u8>,
    layout: NativeStackLayout,
    local_index: u8,
) -> VmResult<()> {
    let stack_len_offset = vec_len_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_ptr_offset = vec_ptr_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let locals_len_offset = vec_len_disp(layout.vm_locals_offset, layout.stack_vec)?;
    let locals_ptr_offset = vec_ptr_disp(layout.vm_locals_offset, layout.stack_vec)?;

    code.extend_from_slice(&[0x48, 0x8B, 0x8B]); // mov rcx, [rbx+disp32] ; stack len
    code.extend_from_slice(&stack_len_offset.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x83, 0xF9, 0x01]); // cmp rcx, 1
    let underflow = emit_jcc_rel32(code, [0x0F, 0x82]); // jb

    code.extend_from_slice(&[0x4C, 0x8B, 0x83]); // mov r8, [rbx+disp32] ; locals len
    code.extend_from_slice(&locals_len_offset.to_le_bytes());
    code.push(0xB8); // mov eax, imm32
    code.extend_from_slice(&(local_index as u32).to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x39, 0xC0]); // cmp rax, r8
    let bad_index = emit_jcc_rel32(code, [0x0F, 0x83]); // jae

    code.extend_from_slice(&[0x48, 0x8B, 0x93]); // mov rdx, [rbx+disp32] ; stack ptr
    code.extend_from_slice(&stack_ptr_offset.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8D, 0x41, 0xFF]); // lea rax, [rcx-1]
    code.extend_from_slice(&[0x48, 0x69, 0xC0]); // imul rax, rax, imm32
    code.extend_from_slice(&layout.value.size.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8D, 0x34, 0x02]); // lea rsi, [rdx+rax] ; src(top)

    code.extend_from_slice(&[0x4C, 0x8B, 0x8B]); // mov r9, [rbx+disp32] ; locals ptr
    code.extend_from_slice(&locals_ptr_offset.to_le_bytes());
    code.push(0xB8); // mov eax, imm32
    code.extend_from_slice(&(local_index as u32).to_le_bytes());
    code.extend_from_slice(&[0x48, 0x69, 0xC0]); // imul rax, rax, imm32
    code.extend_from_slice(&layout.value.size.to_le_bytes());
    code.extend_from_slice(&[0x49, 0x8D, 0x3C, 0x01]); // lea rdi, [r9+rax] ; dst(local)

    emit_load_tag_edx_from_rsi(code, layout.value)?;
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, int_tag
    code.extend_from_slice(&layout.value.int_tag.to_le_bytes());
    let src_primitive = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, float_tag
    code.extend_from_slice(&layout.value.float_tag.to_le_bytes());
    let src_primitive_float = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, bool_tag
    code.extend_from_slice(&layout.value.bool_tag.to_le_bytes());
    let src_primitive_bool = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    let fallback = emit_jmp_rel32(code);

    let src_primitive_label = code.len();
    emit_load_tag_eax_from_rdi(code, layout.value)?;
    code.extend_from_slice(&[0x3D]); // cmp eax, int_tag
    code.extend_from_slice(&layout.value.int_tag.to_le_bytes());
    let dst_primitive = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x3D]); // cmp eax, float_tag
    code.extend_from_slice(&layout.value.float_tag.to_le_bytes());
    let dst_primitive_float = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x3D]); // cmp eax, bool_tag
    code.extend_from_slice(&layout.value.bool_tag.to_le_bytes());
    let dst_primitive_bool = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    let fallback_from_dst = emit_jmp_rel32(code);

    let dst_primitive_label = code.len();
    emit_copy_value_rsi_to_rdi(code, layout.value)?;
    code.extend_from_slice(&[0x48, 0xFF, 0xC9]); // dec rcx
    code.extend_from_slice(&[0x48, 0x89, 0x8B]); // mov [rbx+disp32], rcx
    code.extend_from_slice(&stack_len_offset.to_le_bytes());
    emit_status_continue(code);
    let done = emit_jmp_rel32(code);

    let fallback_label = code.len();
    let helper_addr = helper_ptr_to_u64(jit_native_stloc_bridge as *const (), "stloc helper")?;
    emit_vm_helper_call1_u32(code, helper_addr, local_index as u32);
    let done_label = code.len();

    patch_rel32(code, underflow, fallback_label)?;
    patch_rel32(code, bad_index, fallback_label)?;
    patch_rel32(code, src_primitive, src_primitive_label)?;
    patch_rel32(code, src_primitive_float, src_primitive_label)?;
    patch_rel32(code, src_primitive_bool, src_primitive_label)?;
    patch_rel32(code, fallback, fallback_label)?;
    patch_rel32(code, dst_primitive, dst_primitive_label)?;
    patch_rel32(code, dst_primitive_float, dst_primitive_label)?;
    patch_rel32(code, dst_primitive_bool, dst_primitive_label)?;
    patch_rel32(code, fallback_from_dst, fallback_label)?;
    patch_rel32(code, done, done_label)?;
    Ok(())
}

fn emit_native_step_ceq_inline(code: &mut Vec<u8>, layout: NativeStackLayout) -> VmResult<()> {
    let stack_len_offset = vec_len_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_ptr_offset = vec_ptr_disp(layout.vm_stack_offset, layout.stack_vec)?;

    code.extend_from_slice(&[0x48, 0x8B, 0x8B]); // mov rcx, [rbx+disp32]
    code.extend_from_slice(&stack_len_offset.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x83, 0xF9, 0x02]); // cmp rcx, 2
    let underflow = emit_jcc_rel32(code, [0x0F, 0x82]); // jb
    code.extend_from_slice(&[0x48, 0x8B, 0x93]); // mov rdx, [rbx+disp32]
    code.extend_from_slice(&stack_ptr_offset.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8D, 0x41, 0xFF]); // lea rax, [rcx-1]
    code.extend_from_slice(&[0x48, 0x69, 0xC0]); // imul rax, rax, imm32
    code.extend_from_slice(&layout.value.size.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8D, 0x34, 0x02]); // lea rsi, [rdx+rax] rhs
    code.extend_from_slice(&[0x48, 0x2D]); // sub rax, imm32
    code.extend_from_slice(&layout.value.size.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8D, 0x3C, 0x02]); // lea rdi, [rdx+rax] lhs

    emit_load_tag_eax_from_rdi(code, layout.value)?;
    emit_load_tag_edx_from_rsi(code, layout.value)?;
    code.extend_from_slice(&[0x3D]); // cmp eax, string_tag
    code.extend_from_slice(&layout.value.string_tag.to_le_bytes());
    let lhs_string = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, string_tag
    code.extend_from_slice(&layout.value.string_tag.to_le_bytes());
    let rhs_string = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x39, 0xD0]); // cmp eax, edx
    let tags_not_equal = emit_jcc_rel32(code, [0x0F, 0x85]); // jne

    code.extend_from_slice(&[0x3D]); // cmp eax, int_tag
    code.extend_from_slice(&layout.value.int_tag.to_le_bytes());
    let cmp_int = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x3D]); // cmp eax, float_tag
    code.extend_from_slice(&layout.value.float_tag.to_le_bytes());
    let cmp_float = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x3D]); // cmp eax, bool_tag
    code.extend_from_slice(&layout.value.bool_tag.to_le_bytes());
    let cmp_bool = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    let unknown_tag = emit_jmp_rel32(code);

    let cmp_int_label = code.len();
    code.extend_from_slice(&[0x4C, 0x8B, 0x87]); // mov r8, [rdi+disp32]
    code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x3B, 0x86]); // cmp r8, [rsi+disp32]
    code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x94, 0xC0]); // sete al
    let result_ready = emit_jmp_rel32(code);

    let cmp_float_label = code.len();
    code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x87]); // movsd xmm0, [rdi+disp32]
    code.extend_from_slice(&layout.value.float_payload_offset.to_le_bytes());
    code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x8E]); // movsd xmm1, [rsi+disp32]
    code.extend_from_slice(&layout.value.float_payload_offset.to_le_bytes());
    code.extend_from_slice(&[0x66, 0x0F, 0x2E, 0xC1]); // ucomisd xmm0, xmm1
    code.extend_from_slice(&[0x0F, 0x94, 0xC0]); // sete al
    code.extend_from_slice(&[0x0F, 0x9B, 0xC2]); // setnp dl
    code.extend_from_slice(&[0x20, 0xD0]); // and al, dl
    let float_ready = emit_jmp_rel32(code);

    let cmp_bool_label = code.len();
    code.extend_from_slice(&[0x0F, 0xB6, 0x87]); // movzx eax, byte [rdi+disp32]
    code.extend_from_slice(&layout.value.bool_payload_offset.to_le_bytes());
    code.extend_from_slice(&[0x3A, 0x86]); // cmp al, [rsi+disp32]
    code.extend_from_slice(&layout.value.bool_payload_offset.to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x94, 0xC0]); // sete al
    let bool_ready = emit_jmp_rel32(code);

    let not_equal_label = code.len();
    code.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax
    let ne_ready = emit_jmp_rel32(code);

    let result_label = code.len();
    emit_store_bool_from_al(code, layout.value)?;
    code.extend_from_slice(&[0x48, 0xFF, 0xC9]); // dec rcx
    code.extend_from_slice(&[0x48, 0x89, 0x8B]); // mov [rbx+disp32], rcx
    code.extend_from_slice(&stack_len_offset.to_le_bytes());
    emit_status_continue(code);
    let ok_done = emit_jmp_rel32(code);

    let error_label = code.len();
    emit_status_error(code);
    let done_label = code.len();

    patch_rel32(code, cmp_int, cmp_int_label)?;
    patch_rel32(code, cmp_float, cmp_float_label)?;
    patch_rel32(code, cmp_bool, cmp_bool_label)?;
    patch_rel32(code, tags_not_equal, not_equal_label)?;
    patch_rel32(code, result_ready, result_label)?;
    patch_rel32(code, float_ready, result_label)?;
    patch_rel32(code, bool_ready, result_label)?;
    patch_rel32(code, ne_ready, result_label)?;
    patch_rel32(code, underflow, error_label)?;
    patch_rel32(code, lhs_string, error_label)?;
    patch_rel32(code, rhs_string, error_label)?;
    patch_rel32(code, unknown_tag, error_label)?;
    patch_rel32(code, ok_done, done_label)?;
    Ok(())
}

fn emit_native_step_guard_false_inline(
    code: &mut Vec<u8>,
    layout: NativeStackLayout,
    exit_ip: u32,
) -> VmResult<()> {
    let stack_len_offset = vec_len_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_ptr_offset = vec_ptr_disp(layout.vm_stack_offset, layout.stack_vec)?;

    code.extend_from_slice(&[0x48, 0x8B, 0x8B]); // mov rcx, [rbx+disp32]
    code.extend_from_slice(&stack_len_offset.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x83, 0xF9, 0x01]); // cmp rcx, 1
    let underflow = emit_jcc_rel32(code, [0x0F, 0x82]); // jb
    code.extend_from_slice(&[0x48, 0xFF, 0xC9]); // dec rcx
    code.extend_from_slice(&[0x48, 0x8B, 0x93]); // mov rdx, [rbx+disp32]
    code.extend_from_slice(&stack_ptr_offset.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
    code.extend_from_slice(&[0x48, 0x69, 0xC0]); // imul rax, rax, imm32
    code.extend_from_slice(&layout.value.size.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8D, 0x3C, 0x02]); // lea rdi, [rdx+rax]
    emit_load_tag_eax_from_rdi(code, layout.value)?;
    code.extend_from_slice(&[0x3D]); // cmp eax, bool_tag
    code.extend_from_slice(&layout.value.bool_tag.to_le_bytes());
    let bad_type = emit_jcc_rel32(code, [0x0F, 0x85]); // jne
    code.extend_from_slice(&[0x0F, 0xB6, 0x87]); // movzx eax, byte [rdi+disp32]
    code.extend_from_slice(&layout.value.bool_payload_offset.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x89, 0x8B]); // mov [rbx+disp32], rcx
    code.extend_from_slice(&stack_len_offset.to_le_bytes());
    code.extend_from_slice(&[0x84, 0xC0]); // test al, al
    let condition_true = emit_jcc_rel32(code, [0x0F, 0x85]); // jne
    code.extend_from_slice(&[0x48, 0xB8]); // mov rax, imm64
    code.extend_from_slice(&(exit_ip as u64).to_le_bytes());
    code.extend_from_slice(&[0x48, 0x89, 0x83]); // mov [rbx+disp32], rax
    code.extend_from_slice(&layout.vm_ip_offset.to_le_bytes());
    code.push(0xB8); // mov eax, imm32
    code.extend_from_slice(&STATUS_TRACE_EXIT.to_le_bytes());
    let false_done = emit_jmp_rel32(code);

    let true_label = code.len();
    emit_status_continue(code);
    let ok_done = emit_jmp_rel32(code);

    let error_label = code.len();
    emit_status_error(code);
    let done_label = code.len();
    patch_rel32(code, underflow, error_label)?;
    patch_rel32(code, bad_type, error_label)?;
    patch_rel32(code, condition_true, true_label)?;
    patch_rel32(code, false_done, done_label)?;
    patch_rel32(code, ok_done, done_label)?;
    Ok(())
}

fn emit_native_step_jump_to_ip_inline(
    code: &mut Vec<u8>,
    layout: NativeStackLayout,
    target_ip: u32,
) -> VmResult<()> {
    code.extend_from_slice(&[0x48, 0xB8]); // mov rax, imm64
    code.extend_from_slice(&(target_ip as u64).to_le_bytes());
    code.extend_from_slice(&[0x48, 0x89, 0x83]); // mov [rbx+disp32], rax
    code.extend_from_slice(&layout.vm_ip_offset.to_le_bytes());
    code.push(0xB8); // mov eax, imm32
    code.extend_from_slice(&STATUS_TRACE_EXIT.to_le_bytes());
    Ok(())
}

fn emit_native_step_ret_inline(code: &mut Vec<u8>) {
    code.push(0xB8); // mov eax, imm32
    code.extend_from_slice(&STATUS_HALTED.to_le_bytes());
}

fn helper_ptr_to_u64(ptr: *const (), name: &str) -> VmResult<u64> {
    let addr = ptr as usize;
    u64::try_from(addr)
        .map_err(|_| VmError::JitNative(format!("native {name} pointer exceeds 64-bit range")))
}

fn emit_vm_helper_call0(code: &mut Vec<u8>, helper_addr: u64) {
    #[cfg(target_os = "windows")]
    {
        code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
    }
    #[cfg(not(target_os = "windows"))]
    {
        code.extend_from_slice(&[0x48, 0x89, 0xDF]); // mov rdi, rbx
    }
    code.extend_from_slice(&[0x48, 0xB8]); // mov rax, imm64
    code.extend_from_slice(&helper_addr.to_le_bytes());
    code.extend_from_slice(&[0xFF, 0xD0]); // call rax
}

fn emit_vm_helper_call1_u32(code: &mut Vec<u8>, helper_addr: u64, arg: u32) {
    #[cfg(target_os = "windows")]
    {
        code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
        code.push(0xBA); // mov edx, imm32
        code.extend_from_slice(&arg.to_le_bytes());
    }
    #[cfg(not(target_os = "windows"))]
    {
        code.extend_from_slice(&[0x48, 0x89, 0xDF]); // mov rdi, rbx
        code.push(0xBE); // mov esi, imm32
        code.extend_from_slice(&arg.to_le_bytes());
    }
    code.extend_from_slice(&[0x48, 0xB8]); // mov rax, imm64
    code.extend_from_slice(&helper_addr.to_le_bytes());
    code.extend_from_slice(&[0xFF, 0xD0]); // call rax
}

fn emit_native_step_call_inline(
    code: &mut Vec<u8>,
    layout: NativeStackLayout,
    index: u16,
    argc: u8,
    call_ip: usize,
) -> VmResult<()> {
    if let Some(builtin) = BuiltinFunction::from_call_index(index)
        && argc == builtin.arity()
    {
        match builtin {
            BuiltinFunction::Len => {
                return emit_native_builtin_len_fastpath_inline(code, layout);
            }
            BuiltinFunction::Get => {
                return emit_native_builtin_get_fastpath_inline(code, layout);
            }
            BuiltinFunction::Set => {
                return emit_native_builtin_set_fastpath_inline(code, layout);
            }
            _ => {
                let helper_addr = match builtin {
                    BuiltinFunction::Slice => helper_ptr_to_u64(
                        jit_native_builtin_slice_bridge as *const (),
                        "builtin slice helper",
                    )?,
                    BuiltinFunction::Concat => helper_ptr_to_u64(
                        jit_native_builtin_concat_bridge as *const (),
                        "builtin concat helper",
                    )?,
                    BuiltinFunction::Set => helper_ptr_to_u64(
                        jit_native_builtin_set_bridge as *const (),
                        "builtin set helper",
                    )?,
                    BuiltinFunction::ArrayNew => helper_ptr_to_u64(
                        jit_native_builtin_array_new_bridge as *const (),
                        "builtin array_new helper",
                    )?,
                    BuiltinFunction::ArrayPush => helper_ptr_to_u64(
                        jit_native_builtin_array_push_bridge as *const (),
                        "builtin array_push helper",
                    )?,
                    BuiltinFunction::MapNew => helper_ptr_to_u64(
                        jit_native_builtin_map_new_bridge as *const (),
                        "builtin map_new helper",
                    )?,
                    BuiltinFunction::Assert => helper_ptr_to_u64(
                        jit_native_builtin_assert_bridge as *const (),
                        "builtin assert helper",
                    )?,
                    _ => 0,
                };
                if helper_addr != 0 {
                    emit_vm_helper_call0(code, helper_addr);
                    return Ok(());
                }
            }
        }
    }

    let call_ip = u64::try_from(call_ip)
        .map_err(|_| VmError::JitNative("trace call_ip exceeds 64-bit range".to_string()))?;
    let helper_addr = helper_ptr_to_u64(jit_native_call_bridge as *const (), "call helper")?;

    #[cfg(target_os = "windows")]
    {
        code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
        code.push(0xBA); // mov edx, imm32
        code.extend_from_slice(&(index as u32).to_le_bytes());
        code.extend_from_slice(&[0x41, 0xB8]); // mov r8d, imm32
        code.extend_from_slice(&(argc as u32).to_le_bytes());
        code.extend_from_slice(&[0x49, 0xB9]); // mov r9, imm64
        code.extend_from_slice(&call_ip.to_le_bytes());
    }
    #[cfg(not(target_os = "windows"))]
    {
        code.extend_from_slice(&[0x48, 0x89, 0xDF]); // mov rdi, rbx
        code.push(0xBE); // mov esi, imm32
        code.extend_from_slice(&(index as u32).to_le_bytes());
        code.push(0xBA); // mov edx, imm32
        code.extend_from_slice(&(argc as u32).to_le_bytes());
        code.extend_from_slice(&[0x48, 0xB9]); // mov rcx, imm64
        code.extend_from_slice(&call_ip.to_le_bytes());
    }
    code.extend_from_slice(&[0x48, 0xB8]); // mov rax, imm64
    code.extend_from_slice(&helper_addr.to_le_bytes());
    code.extend_from_slice(&[0xFF, 0xD0]); // call rax
    Ok(())
}

fn emit_native_builtin_len_fastpath_inline(
    code: &mut Vec<u8>,
    layout: NativeStackLayout,
) -> VmResult<()> {
    emit_stack_top_setup(code, layout, 1)?;
    emit_load_tag_eax_from_rdi(code, layout.value)?;
    code.extend_from_slice(&[0x3D]); // cmp eax, array_tag
    code.extend_from_slice(&layout.value.array_tag.to_le_bytes());
    let fast = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    let fallback = emit_jmp_rel32(code);

    let fast_label = code.len();
    code.extend_from_slice(&[0x48, 0x8B, 0x87]); // mov rax, [rdi+disp32]
    code.extend_from_slice(&layout.value.array_len_offset.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x89, 0x87]); // mov [rdi+disp32], rax
    code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
    emit_store_tag_rdi(code, layout.value, layout.value.int_tag)?;
    emit_status_continue(code);
    let done = emit_jmp_rel32(code);

    let fallback_label = code.len();
    let helper_addr = helper_ptr_to_u64(
        jit_native_builtin_len_bridge as *const (),
        "builtin len helper",
    )?;
    emit_vm_helper_call0(code, helper_addr);
    let done_label = code.len();

    patch_rel32(code, fast, fast_label)?;
    patch_rel32(code, fallback, fallback_label)?;
    patch_rel32(code, done, done_label)?;
    Ok(())
}

fn emit_native_builtin_get_fastpath_inline(
    code: &mut Vec<u8>,
    layout: NativeStackLayout,
) -> VmResult<()> {
    let stack_len_offset = vec_len_disp(layout.vm_stack_offset, layout.stack_vec)?;
    emit_stack_binary_setup(code, layout, 2)?;

    emit_load_tag_edx_from_rsi(code, layout.value)?; // key tag
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, int_tag
    code.extend_from_slice(&layout.value.int_tag.to_le_bytes());
    let key_is_int = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    let fallback = emit_jmp_rel32(code);

    let key_ok_label = code.len();
    emit_load_tag_eax_from_rdi(code, layout.value)?; // container tag
    code.extend_from_slice(&[0x3D]); // cmp eax, array_tag
    code.extend_from_slice(&layout.value.array_tag.to_le_bytes());
    let is_array = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    let fallback_from_container = emit_jmp_rel32(code);

    let array_ok_label = code.len();
    code.extend_from_slice(&[0x4C, 0x8B, 0x86]); // mov r8, [rsi+disp32] ; index
    code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
    code.extend_from_slice(&[0x4D, 0x85, 0xC0]); // test r8, r8
    let bad_index = emit_jcc_rel32(code, [0x0F, 0x88]); // js
    code.extend_from_slice(&[0x4C, 0x8B, 0x8F]); // mov r9, [rdi+disp32] ; array len
    code.extend_from_slice(&layout.value.array_len_offset.to_le_bytes());
    code.extend_from_slice(&[0x4D, 0x39, 0xC8]); // cmp r8, r9
    let oob = emit_jcc_rel32(code, [0x0F, 0x83]); // jae
    code.extend_from_slice(&[0x4C, 0x8B, 0x97]); // mov r10, [rdi+disp32] ; array ptr
    code.extend_from_slice(&layout.value.array_ptr_offset.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x89, 0xC0]); // mov rax, r8
    code.extend_from_slice(&[0x48, 0x69, 0xC0]); // imul rax, rax, imm32
    code.extend_from_slice(&layout.value.size.to_le_bytes());
    code.extend_from_slice(&[0x49, 0x8D, 0x34, 0x02]); // lea rsi, [r10+rax] ; elem ptr

    emit_load_tag_edx_from_rsi(code, layout.value)?;
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, int_tag
    code.extend_from_slice(&layout.value.int_tag.to_le_bytes());
    let elem_primitive = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, float_tag
    code.extend_from_slice(&layout.value.float_tag.to_le_bytes());
    let elem_primitive_float = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, bool_tag
    code.extend_from_slice(&layout.value.bool_tag.to_le_bytes());
    let elem_primitive_bool = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    let fallback_from_elem = emit_jmp_rel32(code);

    let elem_ok_label = code.len();
    emit_copy_value_rsi_to_rdi(code, layout.value)?; // overwrite container slot with result
    code.extend_from_slice(&[0x48, 0xFF, 0xC9]); // dec rcx (pop key)
    code.extend_from_slice(&[0x48, 0x89, 0x8B]); // mov [rbx+disp32], rcx
    code.extend_from_slice(&stack_len_offset.to_le_bytes());
    emit_status_continue(code);
    let done = emit_jmp_rel32(code);

    let fallback_label = code.len();
    let helper_addr = helper_ptr_to_u64(
        jit_native_builtin_get_bridge as *const (),
        "builtin get helper",
    )?;
    emit_vm_helper_call0(code, helper_addr);
    let done_label = code.len();

    patch_rel32(code, key_is_int, key_ok_label)?;
    patch_rel32(code, is_array, array_ok_label)?;
    patch_rel32(code, elem_primitive, elem_ok_label)?;
    patch_rel32(code, elem_primitive_float, elem_ok_label)?;
    patch_rel32(code, elem_primitive_bool, elem_ok_label)?;
    patch_rel32(code, fallback, fallback_label)?;
    patch_rel32(code, fallback_from_container, fallback_label)?;
    patch_rel32(code, bad_index, fallback_label)?;
    patch_rel32(code, oob, fallback_label)?;
    patch_rel32(code, fallback_from_elem, fallback_label)?;
    patch_rel32(code, done, done_label)?;
    Ok(())
}

fn emit_native_builtin_set_fastpath_inline(
    code: &mut Vec<u8>,
    layout: NativeStackLayout,
) -> VmResult<()> {
    let stack_len_offset = vec_len_disp(layout.vm_stack_offset, layout.stack_vec)?;
    emit_stack_binary_setup(code, layout, 3)?;

    // Setup from emit_stack_binary_setup(min_len=3):
    //   rsi = value slot (top)
    //   rdi = key slot
    //   rcx = stack len
    //   rdx = stack ptr
    // We only fast-path primitive value assignment into existing primitive array slots.

    // Guard: value is primitive (int/float/bool).
    emit_load_tag_edx_from_rsi(code, layout.value)?;
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, int_tag
    code.extend_from_slice(&layout.value.int_tag.to_le_bytes());
    let value_is_int = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, float_tag
    code.extend_from_slice(&layout.value.float_tag.to_le_bytes());
    let value_is_float = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x81, 0xFA]); // cmp edx, bool_tag
    code.extend_from_slice(&layout.value.bool_tag.to_le_bytes());
    let value_is_bool = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    let fallback = emit_jmp_rel32(code);

    let value_ok_label = code.len();

    // Guard: key is int, and load index into r8.
    emit_load_tag_eax_from_rdi(code, layout.value)?;
    code.extend_from_slice(&[0x3D]); // cmp eax, int_tag
    code.extend_from_slice(&layout.value.int_tag.to_le_bytes());
    let key_is_int = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    let fallback_from_key = emit_jmp_rel32(code);

    let key_ok_label = code.len();
    code.extend_from_slice(&[0x4C, 0x8B, 0x87]); // mov r8, [rdi+disp32] ; key index
    code.extend_from_slice(&layout.value.int_payload_offset.to_le_bytes());
    code.extend_from_slice(&[0x4D, 0x85, 0xC0]); // test r8, r8
    let bad_index = emit_jcc_rel32(code, [0x0F, 0x88]); // js

    // Compute container slot pointer in r10 from key slot.
    code.extend_from_slice(&[0x4C, 0x8D, 0x97]); // lea r10, [rdi+disp32]
    code.extend_from_slice(&(-layout.value.size).to_le_bytes());

    // Guard: container is array.
    code.extend_from_slice(&[0x4C, 0x89, 0xD7]); // mov rdi, r10
    emit_load_tag_eax_from_rdi(code, layout.value)?;
    code.extend_from_slice(&[0x3D]); // cmp eax, array_tag
    code.extend_from_slice(&layout.value.array_tag.to_le_bytes());
    let is_array = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    let fallback_from_container = emit_jmp_rel32(code);

    let array_ok_label = code.len();
    code.extend_from_slice(&[0x4D, 0x8B, 0x8A]); // mov r9, [r10+disp32] ; array len
    code.extend_from_slice(&layout.value.array_len_offset.to_le_bytes());
    code.extend_from_slice(&[0x4D, 0x39, 0xC8]); // cmp r8, r9
    let index_in_bounds = emit_jcc_rel32(code, [0x0F, 0x82]); // jb
    // Keep append/overflow behavior in helper path to avoid capacity/drop hazards.
    let fallback_from_oob_or_append = emit_jmp_rel32(code);

    let replace_label = code.len();
    code.extend_from_slice(&[0x4D, 0x8B, 0x9A]); // mov r11, [r10+disp32] ; array ptr
    code.extend_from_slice(&layout.value.array_ptr_offset.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x89, 0xC0]); // mov rax, r8
    code.extend_from_slice(&[0x48, 0x69, 0xC0]); // imul rax, rax, imm32
    code.extend_from_slice(&layout.value.size.to_le_bytes());
    code.extend_from_slice(&[0x49, 0x8D, 0x3C, 0x03]); // lea rdi, [r11+rax] ; dst elem ptr

    // Guard: existing element is primitive so overwrite is drop-safe.
    emit_load_tag_eax_from_rdi(code, layout.value)?;
    code.extend_from_slice(&[0x3D]); // cmp eax, int_tag
    code.extend_from_slice(&layout.value.int_tag.to_le_bytes());
    let dst_is_int = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x3D]); // cmp eax, float_tag
    code.extend_from_slice(&layout.value.float_tag.to_le_bytes());
    let dst_is_float = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    code.extend_from_slice(&[0x3D]); // cmp eax, bool_tag
    code.extend_from_slice(&layout.value.bool_tag.to_le_bytes());
    let dst_is_bool = emit_jcc_rel32(code, [0x0F, 0x84]); // je
    let fallback_from_dst = emit_jmp_rel32(code);

    let replace_ok_label = code.len();
    emit_copy_value_rsi_to_rdi(code, layout.value)?;
    code.extend_from_slice(&[0x48, 0x83, 0xE9, 0x02]); // sub rcx, 2
    code.extend_from_slice(&[0x48, 0x89, 0x8B]); // mov [rbx+disp32], rcx
    code.extend_from_slice(&stack_len_offset.to_le_bytes());
    emit_status_continue(code);
    let done = emit_jmp_rel32(code);

    let fallback_label = code.len();
    let helper_addr = helper_ptr_to_u64(
        jit_native_builtin_set_bridge as *const (),
        "builtin set helper",
    )?;
    emit_vm_helper_call0(code, helper_addr);
    let done_label = code.len();

    patch_rel32(code, value_is_int, value_ok_label)?;
    patch_rel32(code, value_is_float, value_ok_label)?;
    patch_rel32(code, value_is_bool, value_ok_label)?;
    patch_rel32(code, key_is_int, key_ok_label)?;
    patch_rel32(code, is_array, array_ok_label)?;
    patch_rel32(code, index_in_bounds, replace_label)?;
    patch_rel32(code, dst_is_int, replace_ok_label)?;
    patch_rel32(code, dst_is_float, replace_ok_label)?;
    patch_rel32(code, dst_is_bool, replace_ok_label)?;
    patch_rel32(code, fallback, fallback_label)?;
    patch_rel32(code, fallback_from_key, fallback_label)?;
    patch_rel32(code, bad_index, fallback_label)?;
    patch_rel32(code, fallback_from_container, fallback_label)?;
    patch_rel32(code, fallback_from_oob_or_append, fallback_label)?;
    patch_rel32(code, fallback_from_dst, fallback_label)?;
    patch_rel32(code, done, done_label)?;
    Ok(())
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
    let mut array_vec_a = Vec::with_capacity(9);
    array_vec_a.push(Value::Int(11));
    array_vec_a.push(Value::Int(22));
    let array_ptr_a = array_vec_a.as_ptr() as usize;
    let array_len_a = array_vec_a.len();
    let mut array_vec_b = Vec::with_capacity(13);
    array_vec_b.push(Value::Int(1));
    array_vec_b.push(Value::Int(2));
    array_vec_b.push(Value::Int(3));
    let array_ptr_b = array_vec_b.as_ptr() as usize;
    let array_len_b = array_vec_b.len();
    let int_a_bytes = encode_value_bytes(Value::Int(int_a));
    let int_b_bytes = encode_value_bytes(Value::Int(int_b));
    let float_a_bytes = encode_value_bytes(Value::Float(float_a));
    let float_b_bytes = encode_value_bytes(Value::Float(float_b));
    let bool_false_bytes = encode_value_bytes(Value::Bool(false));
    let bool_true_bytes = encode_value_bytes(Value::Bool(true));
    let string_a_bytes = encode_value_bytes(Value::String("a".to_string()));
    let string_b_bytes = encode_value_bytes(Value::String("b".to_string()));
    let array_a_bytes = encode_value_bytes(Value::Array(array_vec_a));
    let array_b_bytes = encode_value_bytes(Value::Array(array_vec_b));
    let stable_tag_pairs = [
        (&int_a_bytes[..], &int_b_bytes[..]),
        (&float_a_bytes[..], &float_b_bytes[..]),
        (&bool_false_bytes[..], &bool_true_bytes[..]),
        (&string_a_bytes[..], &string_b_bytes[..]),
        (&array_a_bytes[..], &array_b_bytes[..]),
    ];
    let (tag_offset, tag_size) = detect_tag_layout(&stable_tag_pairs)?;
    let int_tag = decode_tag(&int_a_bytes, tag_offset, tag_size);
    let float_tag = decode_tag(&float_a_bytes, tag_offset, tag_size);
    let bool_tag = decode_tag(&bool_false_bytes, tag_offset, tag_size);
    let string_tag = decode_tag(&string_a_bytes, tag_offset, tag_size);
    let array_tag = decode_tag(&array_a_bytes, tag_offset, tag_size);

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

    let float_match_a = float_a.to_le_bytes();
    let float_match_b = float_b.to_le_bytes();
    let mut float_payload_offset = None;
    for offset in 0..=value_size.saturating_sub(8) {
        if float_a_bytes[offset..offset + 8] == float_match_a
            && float_b_bytes[offset..offset + 8] == float_match_b
        {
            if float_payload_offset.is_some() {
                return Err(VmError::JitNative(
                    "ambiguous Value::Float payload offset for native emission".to_string(),
                ));
            }
            float_payload_offset = Some(offset);
        }
    }
    let float_payload_offset = float_payload_offset.ok_or_else(|| {
        VmError::JitNative(
            "unable to find Value::Float payload offset for native emission".to_string(),
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

    let array_ptr_offset = detect_usize_field_offset(
        &array_a_bytes,
        &array_b_bytes,
        array_ptr_a,
        array_ptr_b,
        "Value::Array ptr offset",
    )?;
    let array_len_offset = detect_usize_field_offset(
        &array_a_bytes,
        &array_b_bytes,
        array_len_a,
        array_len_b,
        "Value::Array len offset",
    )?;

    Ok(ValueLayout {
        size: usize_to_i32(value_size, "Value size")?,
        tag_offset: usize_to_i32(tag_offset, "Value tag offset")?,
        tag_size: tag_size as u8,
        int_tag,
        float_tag,
        bool_tag,
        string_tag,
        array_tag,
        int_payload_offset: usize_to_i32(int_payload_offset, "Value::Int payload offset")?,
        float_payload_offset: usize_to_i32(float_payload_offset, "Value::Float payload offset")?,
        bool_payload_offset: usize_to_i32(bool_payload_offset, "Value::Bool payload offset")?,
        array_ptr_offset: usize_to_i32(array_ptr_offset, "Value::Array ptr offset")?,
        array_len_offset: usize_to_i32(array_len_offset, "Value::Array len offset")?,
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

fn detect_usize_field_offset(
    value_a: &[u8],
    value_b: &[u8],
    needle_a: usize,
    needle_b: usize,
    label: &str,
) -> VmResult<usize> {
    let width = std::mem::size_of::<usize>();
    if value_a.len() != value_b.len() || value_a.len() < width {
        return Err(VmError::JitNative(format!(
            "invalid probe sizes while detecting {label}"
        )));
    }

    let mut found = None;
    for offset in 0..=value_a.len() - width {
        let a = decode_usize(value_a, offset);
        let b = decode_usize(value_b, offset);
        if a == needle_a && b == needle_b {
            if found.is_some() {
                return Err(VmError::JitNative(format!(
                    "ambiguous {label} while probing native layout"
                )));
            }
            found = Some(offset);
        }
    }
    found.ok_or_else(|| VmError::JitNative(format!("failed to detect {label}")))
}

fn decode_usize(bytes: &[u8], offset: usize) -> usize {
    if std::mem::size_of::<usize>() == 8 {
        let mut data = [0u8; 8];
        data.copy_from_slice(&bytes[offset..offset + 8]);
        u64::from_le_bytes(data) as usize
    } else {
        let mut data = [0u8; 4];
        data.copy_from_slice(&bytes[offset..offset + 4]);
        u32::from_le_bytes(data) as usize
    }
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

fn emit_native_status_check(code: &mut Vec<u8>, patches: &mut Vec<usize>) {
    code.extend_from_slice(&[0x85, 0xC0]); // test eax, eax
    code.extend_from_slice(&[0x0F, 0x85]); // jne rel32
    patches.push(code.len());
    code.extend_from_slice(&[0, 0, 0, 0]);
}

thread_local! {
    static JIT_BRIDGE_ERROR: std::cell::RefCell<Option<VmError>> = const { std::cell::RefCell::new(None) };
}

fn clear_bridge_error() {
    JIT_BRIDGE_ERROR.with(|slot| {
        *slot.borrow_mut() = None;
    });
}

fn take_bridge_error() -> Option<VmError> {
    JIT_BRIDGE_ERROR.with(|slot| slot.borrow_mut().take())
}

fn set_bridge_error(error: VmError) {
    JIT_BRIDGE_ERROR.with(|slot| {
        *slot.borrow_mut() = Some(error);
    });
}

extern "C" fn jit_native_pop_bridge(vm_ptr: *mut Vm) -> i32 {
    if vm_ptr.is_null() {
        set_bridge_error(VmError::JitNative(
            "native trace pop helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let vm = unsafe { &mut *vm_ptr };
    match vm.pop_value() {
        Ok(_) => STATUS_CONTINUE,
        Err(err) => {
            set_bridge_error(err);
            STATUS_ERROR
        }
    }
}

extern "C" fn jit_native_dup_bridge(vm_ptr: *mut Vm) -> i32 {
    if vm_ptr.is_null() {
        set_bridge_error(VmError::JitNative(
            "native trace dup helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let vm = unsafe { &mut *vm_ptr };
    let value = match vm.peek_value() {
        Ok(value) => value.clone(),
        Err(err) => {
            set_bridge_error(err);
            return STATUS_ERROR;
        }
    };
    vm.stack.push(value);
    STATUS_CONTINUE
}

extern "C" fn jit_native_ldc_bridge(vm_ptr: *mut Vm, const_index: u32) -> i32 {
    if vm_ptr.is_null() {
        set_bridge_error(VmError::JitNative(
            "native trace ldc helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let vm = unsafe { &mut *vm_ptr };
    let value = match vm.program.constants.get(const_index as usize) {
        Some(value) => value.clone(),
        None => {
            set_bridge_error(VmError::InvalidConstant(const_index));
            return STATUS_ERROR;
        }
    };
    vm.stack.push(value);
    STATUS_CONTINUE
}

extern "C" fn jit_native_ldloc_bridge(vm_ptr: *mut Vm, local_index: u32) -> i32 {
    if vm_ptr.is_null() {
        set_bridge_error(VmError::JitNative(
            "native trace ldloc helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let index = match u8::try_from(local_index) {
        Ok(index) => index,
        Err(_) => {
            set_bridge_error(VmError::JitNative(
                "native trace ldloc helper received out-of-range local index".to_string(),
            ));
            return STATUS_ERROR;
        }
    };

    let vm = unsafe { &mut *vm_ptr };
    let value = match vm.locals.get(index as usize) {
        Some(value) => value.clone(),
        None => {
            set_bridge_error(VmError::InvalidLocal(index));
            return STATUS_ERROR;
        }
    };
    vm.stack.push(value);
    STATUS_CONTINUE
}

extern "C" fn jit_native_stloc_bridge(vm_ptr: *mut Vm, local_index: u32) -> i32 {
    if vm_ptr.is_null() {
        set_bridge_error(VmError::JitNative(
            "native trace stloc helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let index = match u8::try_from(local_index) {
        Ok(index) => index,
        Err(_) => {
            set_bridge_error(VmError::JitNative(
                "native trace stloc helper received out-of-range local index".to_string(),
            ));
            return STATUS_ERROR;
        }
    };

    let vm = unsafe { &mut *vm_ptr };
    let value = match vm.pop_value() {
        Ok(value) => value,
        Err(err) => {
            set_bridge_error(err);
            return STATUS_ERROR;
        }
    };
    let Some(slot) = vm.locals.get_mut(index as usize) else {
        set_bridge_error(VmError::InvalidLocal(index));
        return STATUS_ERROR;
    };
    *slot = value;
    STATUS_CONTINUE
}

extern "C" fn jit_native_builtin_len_bridge(vm_ptr: *mut Vm) -> i32 {
    if vm_ptr.is_null() {
        set_bridge_error(VmError::JitNative(
            "native trace builtin len helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let vm = unsafe { &mut *vm_ptr };
    let value = match vm.pop_value() {
        Ok(value) => value,
        Err(err) => {
            set_bridge_error(err);
            return STATUS_ERROR;
        }
    };
    let len = match value {
        Value::String(text) => text.chars().count() as i64,
        Value::Array(values) => values.len() as i64,
        Value::Map(entries) => entries.len() as i64,
        _ => {
            set_bridge_error(VmError::TypeMismatch("string/array/map"));
            return STATUS_ERROR;
        }
    };
    vm.stack.push(Value::Int(len));
    STATUS_CONTINUE
}

extern "C" fn jit_native_builtin_slice_bridge(vm_ptr: *mut Vm) -> i32 {
    if vm_ptr.is_null() {
        set_bridge_error(VmError::JitNative(
            "native trace builtin slice helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let vm = unsafe { &mut *vm_ptr };
    let len = match vm.pop_value() {
        Ok(value) => match value.as_int() {
            Ok(value) => value,
            Err(err) => {
                set_bridge_error(err);
                return STATUS_ERROR;
            }
        },
        Err(err) => {
            set_bridge_error(err);
            return STATUS_ERROR;
        }
    };
    let start = match vm.pop_value() {
        Ok(value) => match value.as_int() {
            Ok(value) => value,
            Err(err) => {
                set_bridge_error(err);
                return STATUS_ERROR;
            }
        },
        Err(err) => {
            set_bridge_error(err);
            return STATUS_ERROR;
        }
    };
    let source = match vm.pop_value() {
        Ok(value) => value,
        Err(err) => {
            set_bridge_error(err);
            return STATUS_ERROR;
        }
    };

    if start < 0 || len <= 0 {
        match source {
            Value::String(_) => vm.stack.push(Value::String(String::new())),
            Value::Array(_) => vm.stack.push(Value::Array(Vec::new())),
            _ => {
                set_bridge_error(VmError::TypeMismatch("string/array"));
                return STATUS_ERROR;
            }
        }
        return STATUS_CONTINUE;
    }

    let start = match usize::try_from(start) {
        Ok(value) => value,
        Err(_) => {
            set_bridge_error(VmError::HostError(
                "slice start overflow while converting to usize".to_string(),
            ));
            return STATUS_ERROR;
        }
    };
    let len = match usize::try_from(len) {
        Ok(value) => value,
        Err(_) => {
            set_bridge_error(VmError::HostError(
                "slice length overflow while converting to usize".to_string(),
            ));
            return STATUS_ERROR;
        }
    };

    match source {
        Value::String(text) => {
            vm.stack
                .push(Value::String(text.chars().skip(start).take(len).collect()));
        }
        Value::Array(values) => {
            vm.stack.push(Value::Array(
                values.into_iter().skip(start).take(len).collect(),
            ));
        }
        _ => {
            set_bridge_error(VmError::TypeMismatch("string/array"));
            return STATUS_ERROR;
        }
    }
    STATUS_CONTINUE
}

extern "C" fn jit_native_builtin_concat_bridge(vm_ptr: *mut Vm) -> i32 {
    if vm_ptr.is_null() {
        set_bridge_error(VmError::JitNative(
            "native trace builtin concat helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let vm = unsafe { &mut *vm_ptr };
    let rhs = match vm.pop_value() {
        Ok(value) => value,
        Err(err) => {
            set_bridge_error(err);
            return STATUS_ERROR;
        }
    };
    let lhs = match vm.pop_value() {
        Ok(value) => value,
        Err(err) => {
            set_bridge_error(err);
            return STATUS_ERROR;
        }
    };

    match (lhs, rhs) {
        (Value::String(lhs), Value::String(rhs)) => {
            let mut out = String::with_capacity(lhs.len() + rhs.len());
            out.push_str(&lhs);
            out.push_str(&rhs);
            vm.stack.push(Value::String(out));
        }
        (Value::Array(mut lhs), Value::Array(rhs)) => {
            lhs.extend(rhs);
            vm.stack.push(Value::Array(lhs));
        }
        _ => {
            set_bridge_error(VmError::TypeMismatch("string/string or array/array"));
            return STATUS_ERROR;
        }
    }
    STATUS_CONTINUE
}

extern "C" fn jit_native_builtin_get_bridge(vm_ptr: *mut Vm) -> i32 {
    if vm_ptr.is_null() {
        set_bridge_error(VmError::JitNative(
            "native trace builtin get helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let vm = unsafe { &mut *vm_ptr };
    let key = match vm.pop_value() {
        Ok(value) => value,
        Err(err) => {
            set_bridge_error(err);
            return STATUS_ERROR;
        }
    };
    let container = match vm.pop_value() {
        Ok(value) => value,
        Err(err) => {
            set_bridge_error(err);
            return STATUS_ERROR;
        }
    };

    match container {
        Value::Array(values) => {
            let index = match key.as_int() {
                Ok(value) => value,
                Err(err) => {
                    set_bridge_error(err);
                    return STATUS_ERROR;
                }
            };
            if index < 0 {
                set_bridge_error(VmError::HostError(
                    "array index must be non-negative".to_string(),
                ));
                return STATUS_ERROR;
            }
            let index = match usize::try_from(index) {
                Ok(value) => value,
                Err(_) => {
                    set_bridge_error(VmError::HostError("array index overflow".to_string()));
                    return STATUS_ERROR;
                }
            };
            let Some(value) = values.into_iter().nth(index) else {
                set_bridge_error(VmError::HostError(format!(
                    "array index {index} out of bounds"
                )));
                return STATUS_ERROR;
            };
            vm.stack.push(value);
        }
        Value::Map(entries) => {
            for (existing_key, value) in entries {
                if existing_key == key {
                    vm.stack.push(value);
                    return STATUS_CONTINUE;
                }
            }
            set_bridge_error(VmError::HostError("map key not found".to_string()));
            return STATUS_ERROR;
        }
        Value::String(text) => {
            let index = match key.as_int() {
                Ok(value) => value,
                Err(err) => {
                    set_bridge_error(err);
                    return STATUS_ERROR;
                }
            };
            if index < 0 {
                set_bridge_error(VmError::HostError(
                    "string index must be non-negative".to_string(),
                ));
                return STATUS_ERROR;
            }
            let index = match usize::try_from(index) {
                Ok(value) => value,
                Err(_) => {
                    set_bridge_error(VmError::HostError("string index overflow".to_string()));
                    return STATUS_ERROR;
                }
            };
            let Some(value) = text
                .chars()
                .nth(index)
                .map(|ch| Value::String(ch.to_string()))
            else {
                set_bridge_error(VmError::HostError(format!(
                    "string index {index} out of bounds"
                )));
                return STATUS_ERROR;
            };
            vm.stack.push(value);
        }
        _ => {
            set_bridge_error(VmError::TypeMismatch("array/map/string"));
            return STATUS_ERROR;
        }
    }

    STATUS_CONTINUE
}

extern "C" fn jit_native_builtin_set_bridge(vm_ptr: *mut Vm) -> i32 {
    if vm_ptr.is_null() {
        set_bridge_error(VmError::JitNative(
            "native trace builtin set helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let vm = unsafe { &mut *vm_ptr };
    let value = match vm.pop_value() {
        Ok(value) => value,
        Err(err) => {
            set_bridge_error(err);
            return STATUS_ERROR;
        }
    };
    let key = match vm.pop_value() {
        Ok(value) => value,
        Err(err) => {
            set_bridge_error(err);
            return STATUS_ERROR;
        }
    };
    let container = match vm.pop_value() {
        Ok(value) => value,
        Err(err) => {
            set_bridge_error(err);
            return STATUS_ERROR;
        }
    };

    match container {
        Value::Array(values) => {
            let index = match key.as_int() {
                Ok(value) => value,
                Err(err) => {
                    set_bridge_error(err);
                    return STATUS_ERROR;
                }
            };
            if index < 0 {
                set_bridge_error(VmError::HostError(
                    "array index must be non-negative".to_string(),
                ));
                return STATUS_ERROR;
            }
            let index = match usize::try_from(index) {
                Ok(value) => value,
                Err(_) => {
                    set_bridge_error(VmError::HostError("array index overflow".to_string()));
                    return STATUS_ERROR;
                }
            };
            let mut out = values;
            if index < out.len() {
                out[index] = value;
            } else if index == out.len() {
                out.push(value);
            } else {
                set_bridge_error(VmError::HostError(format!(
                    "array set index {index} out of bounds"
                )));
                return STATUS_ERROR;
            }
            vm.stack.push(Value::Array(out));
        }
        Value::Map(mut entries) => {
            if let Some((_, existing_value)) = entries
                .iter_mut()
                .find(|(existing_key, _)| *existing_key == key)
            {
                *existing_value = value;
            } else {
                entries.push((key, value));
            }
            vm.stack.push(Value::Map(entries));
        }
        _ => {
            set_bridge_error(VmError::TypeMismatch("array/map"));
            return STATUS_ERROR;
        }
    }
    STATUS_CONTINUE
}

extern "C" fn jit_native_builtin_array_new_bridge(vm_ptr: *mut Vm) -> i32 {
    if vm_ptr.is_null() {
        set_bridge_error(VmError::JitNative(
            "native trace builtin array_new helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let vm = unsafe { &mut *vm_ptr };
    vm.stack.push(Value::Array(Vec::new()));
    STATUS_CONTINUE
}

extern "C" fn jit_native_builtin_array_push_bridge(vm_ptr: *mut Vm) -> i32 {
    if vm_ptr.is_null() {
        set_bridge_error(VmError::JitNative(
            "native trace builtin array_push helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let vm = unsafe { &mut *vm_ptr };
    let value = match vm.pop_value() {
        Ok(value) => value,
        Err(err) => {
            set_bridge_error(err);
            return STATUS_ERROR;
        }
    };
    let array = match vm.pop_value() {
        Ok(value) => value,
        Err(err) => {
            set_bridge_error(err);
            return STATUS_ERROR;
        }
    };
    let Value::Array(mut values) = array else {
        set_bridge_error(VmError::TypeMismatch("array"));
        return STATUS_ERROR;
    };
    values.push(value);
    vm.stack.push(Value::Array(values));
    STATUS_CONTINUE
}

extern "C" fn jit_native_builtin_map_new_bridge(vm_ptr: *mut Vm) -> i32 {
    if vm_ptr.is_null() {
        set_bridge_error(VmError::JitNative(
            "native trace builtin map_new helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let vm = unsafe { &mut *vm_ptr };
    vm.stack.push(Value::Map(Vec::new()));
    STATUS_CONTINUE
}

extern "C" fn jit_native_builtin_assert_bridge(vm_ptr: *mut Vm) -> i32 {
    if vm_ptr.is_null() {
        set_bridge_error(VmError::JitNative(
            "native trace builtin assert helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let vm = unsafe { &mut *vm_ptr };
    let condition = match vm.pop_value() {
        Ok(value) => match value.as_bool() {
            Ok(value) => value,
            Err(err) => {
                set_bridge_error(err);
                return STATUS_ERROR;
            }
        },
        Err(err) => {
            set_bridge_error(err);
            return STATUS_ERROR;
        }
    };
    if !condition {
        set_bridge_error(VmError::HostError("assertion failed".to_string()));
        return STATUS_ERROR;
    }
    STATUS_CONTINUE
}

extern "C" fn jit_native_call_bridge(vm_ptr: *mut Vm, index: u16, argc: u8, call_ip: u64) -> i32 {
    if vm_ptr.is_null() {
        set_bridge_error(VmError::JitNative(
            "native trace call helper received null vm pointer".to_string(),
        ));
        return STATUS_ERROR;
    }

    let call_ip = match usize::try_from(call_ip) {
        Ok(value) => value,
        Err(_) => {
            set_bridge_error(VmError::JitNative(
                "native trace call helper received out-of-range call ip".to_string(),
            ));
            return STATUS_ERROR;
        }
    };

    let vm = unsafe { &mut *vm_ptr };
    match vm.execute_host_call(index, argc, call_ip) {
        Ok(false) => STATUS_CONTINUE,
        Ok(true) => STATUS_YIELDED,
        Err(err) => {
            set_bridge_error(err);
            STATUS_ERROR
        }
    }
}

fn write_machine_code(ptr: *mut u8, code: &[u8]) -> VmResult<()> {
    #[cfg(target_os = "macos")]
    unsafe {
        let use_write_protect = pthread_jit_write_protect_supported_np() != 0;
        if use_write_protect {
            pthread_jit_write_protect_np(0);
        }
        std::ptr::copy_nonoverlapping(code.as_ptr(), ptr, code.len());
        if use_write_protect {
            pthread_jit_write_protect_np(1);
        }
    }

    #[cfg(not(target_os = "macos"))]
    unsafe {
        std::ptr::copy_nonoverlapping(code.as_ptr(), ptr, code.len());
    }

    Ok(())
}

#[cfg(target_os = "windows")]
fn finalize_executable_region(ptr: *mut u8, len: usize) -> VmResult<()> {
    use windows_sys::Win32::{
        Foundation::HANDLE,
        System::{
            Diagnostics::Debug::FlushInstructionCache,
            Memory::{PAGE_EXECUTE_READ, VirtualProtect},
            Threading::GetCurrentProcess,
        },
    };

    if ptr.is_null() {
        return Ok(());
    }

    let mut old_protect = 0u32;
    let ok = unsafe { VirtualProtect(ptr as *mut _, len, PAGE_EXECUTE_READ, &mut old_protect) };
    if ok == 0 {
        return Err(VmError::JitNative(format!(
            "VirtualProtect(PAGE_EXECUTE_READ) failed: {}",
            std::io::Error::last_os_error()
        )));
    }

    let process: HANDLE = unsafe { GetCurrentProcess() };
    let ok = unsafe { FlushInstructionCache(process, ptr as *const _, len) };
    if ok == 0 {
        return Err(VmError::JitNative(format!(
            "FlushInstructionCache failed: {}",
            std::io::Error::last_os_error()
        )));
    }

    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn finalize_executable_region(ptr: *mut u8, len: usize) -> VmResult<()> {
    let rc = unsafe { libc::mprotect(ptr as *mut _, len, libc::PROT_READ | libc::PROT_EXEC) };
    if rc != 0 {
        return Err(VmError::JitNative(format!(
            "mprotect(PROT_READ|PROT_EXEC) failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn finalize_executable_region(_ptr: *mut u8, _len: usize) -> VmResult<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn pthread_jit_write_protect_supported_np() -> libc::c_int;
    fn pthread_jit_write_protect_np(enabled: libc::c_int);
}

#[cfg(target_os = "windows")]
fn alloc_executable_region(len: usize) -> VmResult<*mut u8> {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
    };

    let ptr = unsafe {
        VirtualAlloc(
            std::ptr::null_mut(),
            len,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        ) as *mut u8
    };
    if ptr.is_null() {
        return Err(VmError::JitNative(format!(
            "VirtualAlloc failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(ptr)
}

#[cfg(target_os = "windows")]
fn free_executable_region(ptr: *mut u8, _len: usize) -> VmResult<()> {
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};

    if ptr.is_null() {
        return Ok(());
    }
    let ok = unsafe { VirtualFree(ptr as *mut _, 0, MEM_RELEASE) };
    if ok == 0 {
        return Err(VmError::JitNative(format!(
            "VirtualFree failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn alloc_executable_region(len: usize) -> VmResult<*mut u8> {
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(VmError::JitNative(format!(
            "mmap failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(ptr as *mut u8)
}

#[cfg(target_os = "macos")]
fn alloc_executable_region(len: usize) -> VmResult<*mut u8> {
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_JIT,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(VmError::JitNative(format!(
            "mmap(MAP_JIT) failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(ptr as *mut u8)
}

#[cfg(unix)]
fn free_executable_region(ptr: *mut u8, len: usize) -> VmResult<()> {
    if ptr.is_null() {
        return Ok(());
    }
    let rc = unsafe { libc::munmap(ptr as *mut _, len) };
    if rc != 0 {
        return Err(VmError::JitNative(format!(
            "munmap failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

#[cfg(not(any(unix, target_os = "windows")))]
fn alloc_executable_region(_len: usize) -> VmResult<*mut u8> {
    Err(VmError::JitNative(
        "executable memory allocation not implemented for this platform".to_string(),
    ))
}

#[cfg(not(any(unix, target_os = "windows")))]
fn free_executable_region(_ptr: *mut u8, _len: usize) -> VmResult<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{STATUS_CONTINUE, STATUS_ERROR, STATUS_YIELDED};
    use super::*;
    use crate::builtins::BuiltinFunction;
    use crate::jit::{JitTrace, JitTraceTerminal, TraceStep};
    use crate::vm::Program;
    use crate::vm::{CallOutcome, HostFunction};

    type NativeEntry = unsafe extern "C" fn(*mut Vm) -> i32;

    fn build_single_step_trace(step: TraceStep) -> JitTrace {
        JitTrace {
            id: 0,
            root_ip: 0,
            start_line: None,
            has_call: false,
            has_yielding_call: false,
            steps: vec![step],
            terminal: JitTraceTerminal::LoopBack,
            executions: 0,
        }
    }

    fn execute_single_step(vm: &mut Vm, step: TraceStep) -> VmResult<i32> {
        let trace = build_single_step_trace(step);
        let code = emit_native_trace_bytes(&trace)?;
        let memory = BackendExecutableMemory::from_code(&code)?;
        let entry = unsafe { std::mem::transmute::<*mut u8, NativeEntry>(memory.ptr) };
        clear_bridge_error();
        let status = unsafe { entry(vm as *mut Vm) };
        drop(memory);
        Ok(status)
    }

    struct AddOneHost;

    impl HostFunction for AddOneHost {
        fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
            let value = match args.first() {
                Some(Value::Int(value)) => *value,
                _ => return Err(VmError::TypeMismatch("int")),
            };
            Ok(CallOutcome::Return(vec![Value::Int(value + 1)]))
        }
    }

    struct YieldOnceHost {
        yielded: bool,
    }

    impl HostFunction for YieldOnceHost {
        fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            if !self.yielded {
                self.yielded = true;
                Ok(CallOutcome::Yield)
            } else {
                Ok(CallOutcome::Return(Vec::new()))
            }
        }
    }

    #[test]
    fn add_step_emits_no_helper_calls() {
        let trace = build_single_step_trace(TraceStep::Add);
        let code = emit_native_trace_bytes(&trace).expect("native add trace should compile");
        let call_count = code
            .windows(2)
            .filter(|window| *window == [0xFF, 0xD0])
            .count();
        assert_eq!(
            call_count, 0,
            "add should emit no call-outs, code bytes: {:02X?}",
            code
        );
    }

    #[test]
    fn arithmetic_steps_emit_without_helper_calls() {
        let steps = [
            TraceStep::Add,
            TraceStep::Sub,
            TraceStep::Mul,
            TraceStep::Div,
            TraceStep::Shl,
            TraceStep::Shr,
            TraceStep::Neg,
            TraceStep::Clt,
            TraceStep::Cgt,
        ];
        for step in steps {
            let trace = build_single_step_trace(step.clone());
            let code = emit_native_trace_bytes(&trace).expect("native trace should compile");
            let call_count = code
                .windows(2)
                .filter(|window| *window == [0xFF, 0xD0])
                .count();
            assert_eq!(
                call_count, 0,
                "step {:?} should emit no helper calls, code bytes: {:02X?}",
                step, code
            );
        }
    }

    #[test]
    fn non_arithmetic_steps_emit_without_helper_calls() {
        let steps = [
            TraceStep::Ceq,
            TraceStep::GuardFalse { exit_ip: 0 },
            TraceStep::JumpToIp { target_ip: 0 },
            TraceStep::JumpToRoot,
            TraceStep::Ret,
        ];
        for step in steps {
            let trace = build_single_step_trace(step.clone());
            let code = emit_native_trace_bytes(&trace).expect("native trace should compile");
            let call_count = code
                .windows(2)
                .filter(|window| *window == [0xFF, 0xD0])
                .count();
            assert_eq!(
                call_count, 0,
                "step {:?} should emit no helper calls, code bytes: {:02X?}",
                step, code
            );
        }
    }

    #[test]
    fn clone_drop_steps_emit_helper_calls() {
        let steps = [
            TraceStep::Ldc(0),
            TraceStep::Pop,
            TraceStep::Dup,
            TraceStep::Ldloc(0),
            TraceStep::Stloc(0),
        ];
        for step in steps {
            let trace = build_single_step_trace(step.clone());
            let code = emit_native_trace_bytes(&trace).expect("native trace should compile");
            let call_count = code
                .windows(2)
                .filter(|window| *window == [0xFF, 0xD0])
                .count();
            assert!(
                call_count >= 1,
                "step {:?} should emit at least one helper call, code bytes: {:02X?}",
                step,
                code
            );
        }
    }

    #[test]
    fn call_step_emits_helper_call() {
        let trace = build_single_step_trace(TraceStep::Call {
            index: 0,
            argc: 1,
            call_ip: 0,
        });
        let code = emit_native_trace_bytes(&trace).expect("native call trace should compile");
        let call_count = code
            .windows(2)
            .filter(|window| *window == [0xFF, 0xD0])
            .count();
        assert!(
            call_count >= 1,
            "call step should emit at least one helper call, code bytes: {:02X?}",
            code
        );
    }

    #[test]
    fn call_step_bridge_executes_host_function_and_continues() {
        let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        vm.register_function(Box::new(AddOneHost));
        vm.stack.push(Value::Int(41));

        let status = execute_single_step(
            &mut vm,
            TraceStep::Call {
                index: 0,
                argc: 1,
                call_ip: 0,
            },
        )
        .expect("native call should run");
        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(vm.stack(), &[Value::Int(42)]);
        assert!(
            take_bridge_error().is_none(),
            "successful call bridge should not set bridge error"
        );
    }

    #[test]
    fn call_step_bridge_executes_builtin_len() {
        let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        vm.stack.push(Value::Array(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
        ]));

        let status = execute_single_step(
            &mut vm,
            TraceStep::Call {
                index: BuiltinFunction::Len.call_index(),
                argc: 1,
                call_ip: 0,
            },
        )
        .expect("native len call should run");
        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(vm.stack(), &[Value::Int(3)]);
        assert!(take_bridge_error().is_none());
    }

    #[test]
    fn call_step_bridge_executes_builtin_concat() {
        let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        vm.stack.push(Value::String("ab".to_string()));
        vm.stack.push(Value::String("cd".to_string()));

        let status = execute_single_step(
            &mut vm,
            TraceStep::Call {
                index: BuiltinFunction::Concat.call_index(),
                argc: 2,
                call_ip: 0,
            },
        )
        .expect("native concat call should run");
        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(vm.stack(), &[Value::String("abcd".to_string())]);
        assert!(take_bridge_error().is_none());
    }

    #[test]
    fn call_step_bridge_executes_builtin_slice() {
        let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        vm.stack.push(Value::String("abcdef".to_string()));
        vm.stack.push(Value::Int(2));
        vm.stack.push(Value::Int(3));

        let status = execute_single_step(
            &mut vm,
            TraceStep::Call {
                index: BuiltinFunction::Slice.call_index(),
                argc: 3,
                call_ip: 0,
            },
        )
        .expect("native slice call should run");
        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(vm.stack(), &[Value::String("cde".to_string())]);
        assert!(take_bridge_error().is_none());
    }

    #[test]
    fn call_step_bridge_executes_builtin_array_new_and_push() {
        let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));

        let new_status = execute_single_step(
            &mut vm,
            TraceStep::Call {
                index: BuiltinFunction::ArrayNew.call_index(),
                argc: 0,
                call_ip: 0,
            },
        )
        .expect("native array_new call should run");
        assert_eq!(new_status, STATUS_CONTINUE);
        assert_eq!(vm.stack(), &[Value::Array(Vec::new())]);
        assert!(take_bridge_error().is_none());

        vm.stack.push(Value::Int(7));
        let push_status = execute_single_step(
            &mut vm,
            TraceStep::Call {
                index: BuiltinFunction::ArrayPush.call_index(),
                argc: 2,
                call_ip: 0,
            },
        )
        .expect("native array_push call should run");
        assert_eq!(push_status, STATUS_CONTINUE);
        assert_eq!(vm.stack(), &[Value::Array(vec![Value::Int(7)])]);
        assert!(take_bridge_error().is_none());
    }

    #[test]
    fn call_step_bridge_executes_builtin_set_replace() {
        let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        vm.stack.push(Value::Array(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
        ]));
        vm.stack.push(Value::Int(1));
        vm.stack.push(Value::Int(9));

        let status = execute_single_step(
            &mut vm,
            TraceStep::Call {
                index: BuiltinFunction::Set.call_index(),
                argc: 3,
                call_ip: 0,
            },
        )
        .expect("native set call should run");
        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(
            vm.stack(),
            &[Value::Array(vec![
                Value::Int(1),
                Value::Int(9),
                Value::Int(3)
            ])]
        );
        assert!(take_bridge_error().is_none());
    }

    #[test]
    fn call_step_bridge_executes_builtin_set_append() {
        let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        vm.stack
            .push(Value::Array(vec![Value::Int(1), Value::Int(2)]));
        vm.stack.push(Value::Int(2));
        vm.stack.push(Value::Int(7));

        let status = execute_single_step(
            &mut vm,
            TraceStep::Call {
                index: BuiltinFunction::Set.call_index(),
                argc: 3,
                call_ip: 0,
            },
        )
        .expect("native set call should run");
        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(
            vm.stack(),
            &[Value::Array(vec![
                Value::Int(1),
                Value::Int(2),
                Value::Int(7)
            ])]
        );
        assert!(take_bridge_error().is_none());
    }

    #[test]
    fn call_step_bridge_propagates_yield_status() {
        let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        vm.register_function(Box::new(YieldOnceHost { yielded: false }));
        vm.stack.push(Value::Int(9));

        let status = execute_single_step(
            &mut vm,
            TraceStep::Call {
                index: 0,
                argc: 1,
                call_ip: 0,
            },
        )
        .expect("native call should run");
        assert_eq!(status, STATUS_YIELDED);
        assert_eq!(vm.stack(), &[Value::Int(9)]);
        assert_eq!(vm.ip(), 0);
        assert!(
            take_bridge_error().is_none(),
            "yielding call bridge should not set bridge error"
        );
    }

    #[test]
    fn dup_step_bridge_clones_owned_values() {
        let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        vm.stack.push(Value::String("hello".to_string()));

        let status = execute_single_step(&mut vm, TraceStep::Dup).expect("native dup should run");
        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(
            vm.stack(),
            &[
                Value::String("hello".to_string()),
                Value::String("hello".to_string())
            ]
        );
        assert!(
            take_bridge_error().is_none(),
            "successful dup bridge should not set bridge error"
        );
    }

    #[test]
    fn stloc_step_bridge_moves_owned_values_safely() {
        let mut vm = Vm::with_locals(Program::new(Vec::new(), Vec::new()), 1);
        vm.locals[0] = Value::String("old".to_string());
        vm.stack.push(Value::String("new".to_string()));

        let status =
            execute_single_step(&mut vm, TraceStep::Stloc(0)).expect("native stloc should run");
        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(vm.locals(), &[Value::String("new".to_string())]);
        assert!(vm.stack().is_empty());
        assert!(
            take_bridge_error().is_none(),
            "successful stloc bridge should not set bridge error"
        );
    }

    #[test]
    fn add_step_inline_success_updates_stack() {
        let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        vm.stack.push(Value::Int(2));
        vm.stack.push(Value::Int(3));

        let status = execute_single_step(&mut vm, TraceStep::Add).expect("native add should run");
        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(vm.stack, vec![Value::Int(5)]);
        assert!(
            take_bridge_error().is_none(),
            "success path should not set bridge error"
        );
    }

    #[test]
    fn add_step_inline_supports_float_and_mixed_numeric() {
        let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        vm.stack.push(Value::Float(1.5));
        vm.stack.push(Value::Int(2));

        let status = execute_single_step(&mut vm, TraceStep::Add).expect("native add should run");
        assert_eq!(status, STATUS_CONTINUE);
        match vm.stack.last() {
            Some(Value::Float(value)) => assert!((*value - 3.5).abs() < f64::EPSILON),
            other => panic!("expected float result, got {other:?}"),
        }
    }

    #[test]
    fn clt_step_inline_supports_float() {
        let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        vm.stack.push(Value::Float(1.5));
        vm.stack.push(Value::Float(2.0));

        let status = execute_single_step(&mut vm, TraceStep::Clt).expect("native clt should run");
        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(vm.stack, vec![Value::Bool(true)]);
    }

    #[test]
    fn div_step_inline_wraps_min_over_neg_one() {
        let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        vm.stack.push(Value::Int(i64::MIN));
        vm.stack.push(Value::Int(-1));

        let status = execute_single_step(&mut vm, TraceStep::Div).expect("native div should run");
        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(vm.stack, vec![Value::Int(i64::MIN)]);
        assert!(
            take_bridge_error().is_none(),
            "wrapped division should not set bridge error"
        );
    }

    #[test]
    fn div_step_inline_rejects_zero_divisor() {
        let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        vm.stack.push(Value::Int(1));
        vm.stack.push(Value::Int(0));

        let status = execute_single_step(&mut vm, TraceStep::Div).expect("native div should run");
        assert_eq!(status, STATUS_ERROR);
    }
}
