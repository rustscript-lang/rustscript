use super::super::{Program, Value, Vm, VmError, VmResult};
use super::{
    NativeBackend, STATUS_CONTINUE, STATUS_ERROR, STATUS_HALTED, STATUS_TRACE_EXIT, STATUS_YIELDED,
};
use std::sync::OnceLock;

pub(super) struct AArch64Backend;

impl NativeBackend for AArch64Backend {
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
    int_payload_offset: i32,
    float_payload_offset: i32,
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

static NATIVE_STACK_LAYOUT: OnceLock<Result<NativeStackLayout, String>> = OnceLock::new();

#[derive(Clone, Copy)]
enum Cond {
    Eq = 0,
    Ne = 1,
    Hs = 2,
    Lo = 3,
    Mi = 4,
    Hi = 8,
    Ge = 10,
    Lt = 11,
    Gt = 12,
    Le = 13,
}

const VM_REG: u8 = 19;

fn emit_native_trace_bytes(trace: &crate::jit::JitTrace) -> VmResult<Vec<u8>> {
    let layout = detect_native_stack_layout()?;
    let mut code = Vec::with_capacity(1024);
    let mut status_checks = Vec::new();

    emit_native_prologue(&mut code);
    emit_status_continue(&mut code);

    for step in &trace.steps {
        match step {
            crate::jit::TraceStep::Nop => {}
            crate::jit::TraceStep::Ldc(index) => {
                emit_native_step_ldc_inline(&mut code, layout, *index)?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::Add => {
                emit_native_step_binary_numeric_inline(
                    &mut code,
                    layout,
                    NativeBinaryNumericOp::Add,
                )?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::Sub => {
                emit_native_step_binary_numeric_inline(
                    &mut code,
                    layout,
                    NativeBinaryNumericOp::Sub,
                )?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::Mul => {
                emit_native_step_binary_numeric_inline(
                    &mut code,
                    layout,
                    NativeBinaryNumericOp::Mul,
                )?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::Div => {
                emit_native_step_binary_numeric_inline(
                    &mut code,
                    layout,
                    NativeBinaryNumericOp::Div,
                )?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::Mod => {
                emit_native_step_mod_inline(&mut code, layout)?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::Shl => {
                emit_native_step_shift_inline(&mut code, layout, true)?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::Shr => {
                emit_native_step_shift_inline(&mut code, layout, false)?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::And => {
                emit_native_step_and_inline(&mut code, layout)?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::Or => {
                emit_native_step_or_inline(&mut code, layout)?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::Neg => {
                emit_native_step_neg_inline(&mut code, layout)?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::Ceq => {
                emit_native_step_ceq_inline(&mut code, layout)?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::Clt => {
                emit_native_step_binary_numeric_inline(
                    &mut code,
                    layout,
                    NativeBinaryNumericOp::Clt,
                )?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::Cgt => {
                emit_native_step_binary_numeric_inline(
                    &mut code,
                    layout,
                    NativeBinaryNumericOp::Cgt,
                )?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::Pop => {
                emit_native_step_pop_inline(&mut code, layout)?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::Dup => {
                emit_native_step_dup_inline(&mut code, layout)?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::Ldloc(index) => {
                emit_native_step_ldloc_inline(&mut code, layout, *index)?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::Stloc(index) => {
                emit_native_step_stloc_inline(&mut code, layout, *index)?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::Call {
                index,
                argc,
                call_ip,
            } => {
                emit_native_step_call_inline(&mut code, *index, *argc, *call_ip)?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::GuardFalse { exit_ip } => {
                let exit_ip = u64::try_from(*exit_ip).map_err(|_| {
                    VmError::JitNative("guard exit ip exceeds 64-bit range".to_string())
                })?;
                emit_native_step_guard_false_inline(&mut code, layout, exit_ip)?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::JumpToIp { target_ip } => {
                let target_ip = u64::try_from(*target_ip).map_err(|_| {
                    VmError::JitNative("trace jump target ip exceeds 64-bit range".to_string())
                })?;
                emit_native_step_jump_to_ip_inline(&mut code, layout, target_ip)?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::JumpToRoot => {
                let root_ip = u64::try_from(trace.root_ip).map_err(|_| {
                    VmError::JitNative("trace root ip exceeds 64-bit range".to_string())
                })?;
                emit_native_step_jump_to_ip_inline(&mut code, layout, root_ip)?;
                emit_native_status_check(&mut code, &mut status_checks);
            }
            crate::jit::TraceStep::Ret => {
                emit_native_step_ret_inline(&mut code);
                emit_native_status_check(&mut code, &mut status_checks);
            }
        }
    }

    emit_status_continue(&mut code);

    let return_label = code.len();
    for patch in status_checks {
        patch_cbnz_w_rel19(&mut code, patch, return_label)?;
    }

    emit_native_epilogue(&mut code);
    Ok(code)
}

fn emit_native_prologue(code: &mut Vec<u8>) {
    emit_sub_imm(code, 31, 31, 32);
    emit_str_x_imm12(code, 29, 31, 16).expect("static stack frame offsets must fit");
    emit_str_x_imm12(code, 30, 31, 24).expect("static stack frame offsets must fit");
    emit_str_x_imm12(code, VM_REG, 31, 0).expect("static stack frame offsets must fit");
    emit_add_imm(code, 29, 31, 0);
    emit_mov_reg(code, VM_REG, 0);
}

fn emit_native_epilogue(code: &mut Vec<u8>) {
    emit_ldr_x_imm12(code, VM_REG, 31, 0).expect("static stack frame offsets must fit");
    emit_ldr_x_imm12(code, 29, 31, 16).expect("static stack frame offsets must fit");
    emit_ldr_x_imm12(code, 30, 31, 24).expect("static stack frame offsets must fit");
    emit_add_imm(code, 31, 31, 32);
    emit_u32(code, 0xD65F03C0); // ret
}

fn emit_native_status_check(code: &mut Vec<u8>, patches: &mut Vec<usize>) {
    let at = code.len();
    emit_u32(code, 0x35000000); // cbnz w0, <imm19>
    patches.push(at);
}

fn emit_status_continue(code: &mut Vec<u8>) {
    emit_mov_imm64(code, 0, STATUS_CONTINUE as i64 as u64);
}

fn emit_status_error(code: &mut Vec<u8>) {
    emit_mov_imm64(code, 0, STATUS_ERROR as i64 as u64);
}

fn emit_status_trace_exit(code: &mut Vec<u8>) {
    emit_mov_imm64(code, 0, STATUS_TRACE_EXIT as i64 as u64);
}

fn emit_status_halted(code: &mut Vec<u8>) {
    emit_mov_imm64(code, 0, STATUS_HALTED as i64 as u64);
}

#[derive(Clone, Copy)]
enum NativeBinaryNumericOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Clt,
    Cgt,
}

fn emit_native_step_binary_numeric_inline(
    code: &mut Vec<u8>,
    layout: NativeStackLayout,
    op: NativeBinaryNumericOp,
) -> VmResult<()> {
    let stack_len_offset = vec_len_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_ptr_offset = vec_ptr_disp(layout.vm_stack_offset, layout.stack_vec)?;

    emit_ldr_x_disp(code, 9, VM_REG, stack_len_offset)?;
    emit_cmp_imm(code, 9, 2)?;
    let underflow = emit_b_cond_placeholder(code, Cond::Lo);

    emit_ldr_x_disp(code, 10, VM_REG, stack_ptr_offset)?;
    emit_sub_imm(code, 11, 9, 1);
    emit_mov_imm64(code, 14, layout.value.size as u64);
    emit_mul_x(code, 11, 11, 14);
    emit_add_reg(code, 13, 10, 11); // rhs ptr
    emit_sub_reg(code, 11, 11, 14);
    emit_add_reg(code, 12, 10, 11); // lhs ptr

    emit_load_tag_w_from_ptr(code, 16, 12, layout.value)?;
    emit_cmp_imm(
        code,
        16,
        u16::try_from(layout.value.int_tag).unwrap_or(0xFFFF),
    )?;
    let lhs_not_int = emit_b_cond_placeholder(code, Cond::Ne);
    emit_load_tag_w_from_ptr(code, 17, 13, layout.value)?;
    emit_cmp_imm(
        code,
        17,
        u16::try_from(layout.value.int_tag).unwrap_or(0xFFFF),
    )?;
    let rhs_not_int = emit_b_cond_placeholder(code, Cond::Ne);

    emit_ldr_x_disp(code, 14, 12, layout.value.int_payload_offset)?;
    emit_ldr_x_disp(code, 15, 13, layout.value.int_payload_offset)?;
    let mut int_div_zero = None;
    let mut float_div_zero = None;

    match op {
        NativeBinaryNumericOp::Add => {
            emit_add_reg(code, 14, 14, 15);
            emit_str_x_disp(code, 14, 12, layout.value.int_payload_offset)?;
            emit_store_tag_ptr(code, 12, layout.value, layout.value.int_tag)?;
            emit_sub_imm(code, 9, 9, 1);
            emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
            emit_status_continue(code);
        }
        NativeBinaryNumericOp::Sub => {
            emit_sub_reg(code, 14, 14, 15);
            emit_str_x_disp(code, 14, 12, layout.value.int_payload_offset)?;
            emit_store_tag_ptr(code, 12, layout.value, layout.value.int_tag)?;
            emit_sub_imm(code, 9, 9, 1);
            emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
            emit_status_continue(code);
        }
        NativeBinaryNumericOp::Mul => {
            emit_mul_x(code, 14, 14, 15);
            emit_str_x_disp(code, 14, 12, layout.value.int_payload_offset)?;
            emit_store_tag_ptr(code, 12, layout.value, layout.value.int_tag)?;
            emit_sub_imm(code, 9, 9, 1);
            emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
            emit_status_continue(code);
        }
        NativeBinaryNumericOp::Div => {
            emit_cmp_imm(code, 15, 0)?;
            int_div_zero = Some(emit_b_cond_placeholder(code, Cond::Eq));
            emit_sdiv_x(code, 14, 14, 15);
            emit_str_x_disp(code, 14, 12, layout.value.int_payload_offset)?;
            emit_store_tag_ptr(code, 12, layout.value, layout.value.int_tag)?;
            emit_sub_imm(code, 9, 9, 1);
            emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
            emit_status_continue(code);
        }
        NativeBinaryNumericOp::Mod => {
            emit_cmp_imm(code, 15, 0)?;
            int_div_zero = Some(emit_b_cond_placeholder(code, Cond::Eq));
            emit_sdiv_x(code, 16, 14, 15); // q in x16
            emit_mul_x(code, 16, 16, 15); // q * rhs
            emit_sub_reg(code, 14, 14, 16); // lhs - q*rhs
            emit_str_x_disp(code, 14, 12, layout.value.int_payload_offset)?;
            emit_store_tag_ptr(code, 12, layout.value, layout.value.int_tag)?;
            emit_sub_imm(code, 9, 9, 1);
            emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
            emit_status_continue(code);
        }
        NativeBinaryNumericOp::Clt | NativeBinaryNumericOp::Cgt => {
            emit_cmp_reg(code, 14, 15);
            emit_mov_imm64(code, 14, 0);
            let false_branch = if matches!(op, NativeBinaryNumericOp::Clt) {
                emit_b_cond_placeholder(code, Cond::Ge)
            } else {
                emit_b_cond_placeholder(code, Cond::Le)
            };
            emit_mov_imm64(code, 14, 1);
            let result_label = code.len();
            patch_b_cond_rel19(code, false_branch, result_label)?;
            emit_store_bool_from_w(code, 14, 12, layout.value)?;
            emit_sub_imm(code, 9, 9, 1);
            emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
            emit_status_continue(code);
        }
    }
    let int_done = emit_b_placeholder(code);

    let float_dispatch = code.len();
    patch_b_cond_rel19(code, lhs_not_int, float_dispatch)?;
    patch_b_cond_rel19(code, rhs_not_int, float_dispatch)?;

    emit_load_tag_w_from_ptr(code, 16, 12, layout.value)?;
    emit_load_tag_w_from_ptr(code, 17, 13, layout.value)?;

    emit_cmp_imm(
        code,
        16,
        u16::try_from(layout.value.int_tag).unwrap_or(0xFFFF),
    )?;
    let lhs_is_int = emit_b_cond_placeholder(code, Cond::Eq);
    emit_cmp_imm(
        code,
        16,
        u16::try_from(layout.value.float_tag).unwrap_or(0xFFFF),
    )?;
    let lhs_not_float = emit_b_cond_placeholder(code, Cond::Ne);
    emit_ldr_d_disp(code, 0, 12, layout.value.float_payload_offset)?;
    let lhs_ready_branch = emit_b_placeholder(code);
    let lhs_int = code.len();
    emit_ldr_x_disp(code, 14, 12, layout.value.int_payload_offset)?;
    emit_scvtf_d_from_x(code, 0, 14);
    let lhs_ready = code.len();
    patch_b_cond_rel19(code, lhs_is_int, lhs_int)?;
    patch_b_rel26(code, lhs_ready_branch, lhs_ready)?;

    emit_cmp_imm(
        code,
        17,
        u16::try_from(layout.value.int_tag).unwrap_or(0xFFFF),
    )?;
    let rhs_is_int = emit_b_cond_placeholder(code, Cond::Eq);
    emit_cmp_imm(
        code,
        17,
        u16::try_from(layout.value.float_tag).unwrap_or(0xFFFF),
    )?;
    let rhs_not_float = emit_b_cond_placeholder(code, Cond::Ne);
    emit_ldr_d_disp(code, 1, 13, layout.value.float_payload_offset)?;
    let rhs_ready_branch = emit_b_placeholder(code);
    let rhs_int = code.len();
    emit_ldr_x_disp(code, 15, 13, layout.value.int_payload_offset)?;
    emit_scvtf_d_from_x(code, 1, 15);
    let rhs_ready = code.len();
    patch_b_cond_rel19(code, rhs_is_int, rhs_int)?;
    patch_b_rel26(code, rhs_ready_branch, rhs_ready)?;

    match op {
        NativeBinaryNumericOp::Add => {
            emit_fadd_d(code, 0, 0, 1);
            emit_store_tag_ptr(code, 12, layout.value, layout.value.float_tag)?;
            emit_str_d_disp(code, 0, 12, layout.value.float_payload_offset)?;
            emit_sub_imm(code, 9, 9, 1);
            emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
            emit_status_continue(code);
        }
        NativeBinaryNumericOp::Sub => {
            emit_fsub_d(code, 0, 0, 1);
            emit_store_tag_ptr(code, 12, layout.value, layout.value.float_tag)?;
            emit_str_d_disp(code, 0, 12, layout.value.float_payload_offset)?;
            emit_sub_imm(code, 9, 9, 1);
            emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
            emit_status_continue(code);
        }
        NativeBinaryNumericOp::Mul => {
            emit_fmul_d(code, 0, 0, 1);
            emit_store_tag_ptr(code, 12, layout.value, layout.value.float_tag)?;
            emit_str_d_disp(code, 0, 12, layout.value.float_payload_offset)?;
            emit_sub_imm(code, 9, 9, 1);
            emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
            emit_status_continue(code);
        }
        NativeBinaryNumericOp::Div => {
            emit_fcmp_d_zero(code, 1);
            float_div_zero = Some(emit_b_cond_placeholder(code, Cond::Eq));
            emit_fdiv_d(code, 0, 0, 1);
            emit_store_tag_ptr(code, 12, layout.value, layout.value.float_tag)?;
            emit_str_d_disp(code, 0, 12, layout.value.float_payload_offset)?;
            emit_sub_imm(code, 9, 9, 1);
            emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
            emit_status_continue(code);
        }
        NativeBinaryNumericOp::Mod => {
            emit_fcmp_d_zero(code, 1);
            float_div_zero = Some(emit_b_cond_placeholder(code, Cond::Eq));
            emit_fdiv_d(code, 2, 0, 1); // q = lhs / rhs
            emit_fcvtzs_x_from_d(code, 14, 2);
            emit_scvtf_d_from_x(code, 2, 14);
            emit_fmul_d(code, 2, 2, 1);
            emit_fsub_d(code, 0, 0, 2);
            emit_store_tag_ptr(code, 12, layout.value, layout.value.float_tag)?;
            emit_str_d_disp(code, 0, 12, layout.value.float_payload_offset)?;
            emit_sub_imm(code, 9, 9, 1);
            emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
            emit_status_continue(code);
        }
        NativeBinaryNumericOp::Clt | NativeBinaryNumericOp::Cgt => {
            emit_fcmp_d(code, 0, 1);
            emit_mov_imm64(code, 14, 0);
            let true_branch = if matches!(op, NativeBinaryNumericOp::Clt) {
                emit_b_cond_placeholder(code, Cond::Mi)
            } else {
                emit_b_cond_placeholder(code, Cond::Gt)
            };
            let false_done = emit_b_placeholder(code);
            let true_label = code.len();
            emit_mov_imm64(code, 14, 1);
            let result_label = code.len();
            patch_b_cond_rel19(code, true_branch, true_label)?;
            patch_b_rel26(code, false_done, result_label)?;
            emit_store_bool_from_w(code, 14, 12, layout.value)?;
            emit_sub_imm(code, 9, 9, 1);
            emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
            emit_status_continue(code);
        }
    }
    let float_done = emit_b_placeholder(code);

    let err_label = code.len();
    emit_status_error(code);
    let done_label = code.len();

    patch_b_cond_rel19(code, underflow, err_label)?;
    patch_b_cond_rel19(code, lhs_not_float, err_label)?;
    patch_b_cond_rel19(code, rhs_not_float, err_label)?;
    if let Some(patch) = int_div_zero {
        patch_b_cond_rel19(code, patch, err_label)?;
    }
    if let Some(patch) = float_div_zero {
        patch_b_cond_rel19(code, patch, err_label)?;
    }
    patch_b_rel26(code, int_done, done_label)?;
    patch_b_rel26(code, float_done, done_label)?;
    Ok(())
}

fn emit_native_step_neg_inline(code: &mut Vec<u8>, layout: NativeStackLayout) -> VmResult<()> {
    let stack_len_offset = vec_len_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_ptr_offset = vec_ptr_disp(layout.vm_stack_offset, layout.stack_vec)?;

    emit_ldr_x_disp(code, 9, VM_REG, stack_len_offset)?;
    emit_cmp_imm(code, 9, 1)?;
    let underflow = emit_b_cond_placeholder(code, Cond::Lo);

    emit_ldr_x_disp(code, 10, VM_REG, stack_ptr_offset)?;
    emit_sub_imm(code, 11, 9, 1);
    emit_mov_imm64(code, 14, layout.value.size as u64);
    emit_mul_x(code, 11, 11, 14);
    emit_add_reg(code, 12, 10, 11);

    emit_load_tag_w_from_ptr(code, 16, 12, layout.value)?;
    emit_cmp_imm(
        code,
        16,
        u16::try_from(layout.value.int_tag).unwrap_or(0xFFFF),
    )?;
    let not_int = emit_b_cond_placeholder(code, Cond::Ne);

    emit_ldr_x_disp(code, 14, 12, layout.value.int_payload_offset)?;
    emit_sub_reg(code, 14, 31, 14); // neg
    emit_str_x_disp(code, 14, 12, layout.value.int_payload_offset)?;
    emit_store_tag_ptr(code, 12, layout.value, layout.value.int_tag)?;
    emit_status_continue(code);
    let int_done = emit_b_placeholder(code);

    let float_check = code.len();
    patch_b_cond_rel19(code, not_int, float_check)?;

    emit_load_tag_w_from_ptr(code, 16, 12, layout.value)?;
    emit_cmp_imm(
        code,
        16,
        u16::try_from(layout.value.float_tag).unwrap_or(0xFFFF),
    )?;
    let not_float = emit_b_cond_placeholder(code, Cond::Ne);
    emit_ldr_d_disp(code, 0, 12, layout.value.float_payload_offset)?;
    emit_fneg_d(code, 0, 0);
    emit_store_tag_ptr(code, 12, layout.value, layout.value.float_tag)?;
    emit_str_d_disp(code, 0, 12, layout.value.float_payload_offset)?;
    emit_status_continue(code);
    let float_done = emit_b_placeholder(code);

    let err_label = code.len();
    emit_status_error(code);
    let done_label = code.len();

    patch_b_cond_rel19(code, underflow, err_label)?;
    patch_b_cond_rel19(code, not_float, err_label)?;
    patch_b_rel26(code, int_done, done_label)?;
    patch_b_rel26(code, float_done, done_label)?;
    Ok(())
}

fn emit_native_step_shift_inline(
    code: &mut Vec<u8>,
    layout: NativeStackLayout,
    is_shl: bool,
) -> VmResult<()> {
    let stack_len_offset = vec_len_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_ptr_offset = vec_ptr_disp(layout.vm_stack_offset, layout.stack_vec)?;

    emit_ldr_x_disp(code, 9, VM_REG, stack_len_offset)?;
    emit_cmp_imm(code, 9, 2)?;
    let underflow = emit_b_cond_placeholder(code, Cond::Lo);

    emit_ldr_x_disp(code, 10, VM_REG, stack_ptr_offset)?;
    emit_sub_imm(code, 11, 9, 1);
    emit_mov_imm64(code, 14, layout.value.size as u64);
    emit_mul_x(code, 11, 11, 14);
    emit_add_reg(code, 13, 10, 11); // rhs
    emit_sub_reg(code, 11, 11, 14);
    emit_add_reg(code, 12, 10, 11); // lhs

    emit_load_tag_w_from_ptr(code, 16, 12, layout.value)?;
    emit_cmp_imm(
        code,
        16,
        u16::try_from(layout.value.int_tag).unwrap_or(0xFFFF),
    )?;
    let lhs_not_int = emit_b_cond_placeholder(code, Cond::Ne);
    emit_load_tag_w_from_ptr(code, 17, 13, layout.value)?;
    emit_cmp_imm(
        code,
        17,
        u16::try_from(layout.value.int_tag).unwrap_or(0xFFFF),
    )?;
    let rhs_not_int = emit_b_cond_placeholder(code, Cond::Ne);

    emit_ldr_x_disp(code, 15, 13, layout.value.int_payload_offset)?; // rhs shift amount
    emit_cmp_imm(code, 15, 0)?;
    let neg_shift = emit_b_cond_placeholder(code, Cond::Lt);
    emit_cmp_imm(code, 15, 63)?;
    let big_shift = emit_b_cond_placeholder(code, Cond::Hi);

    emit_ldr_x_disp(code, 14, 12, layout.value.int_payload_offset)?;
    if is_shl {
        emit_lslv_x(code, 14, 14, 15);
    } else {
        emit_asrv_x(code, 14, 14, 15);
    }
    emit_str_x_disp(code, 14, 12, layout.value.int_payload_offset)?;
    emit_store_tag_ptr(code, 12, layout.value, layout.value.int_tag)?;
    emit_sub_imm(code, 9, 9, 1);
    emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
    emit_status_continue(code);

    let ok_done = emit_b_placeholder(code);
    let err_label = code.len();
    emit_status_error(code);
    let done_label = code.len();

    patch_b_cond_rel19(code, underflow, err_label)?;
    patch_b_cond_rel19(code, lhs_not_int, err_label)?;
    patch_b_cond_rel19(code, rhs_not_int, err_label)?;
    patch_b_cond_rel19(code, neg_shift, err_label)?;
    patch_b_cond_rel19(code, big_shift, err_label)?;
    patch_b_rel26(code, ok_done, done_label)?;
    Ok(())
}

fn emit_native_step_mod_inline(code: &mut Vec<u8>, layout: NativeStackLayout) -> VmResult<()> {
    emit_native_step_binary_numeric_inline(code, layout, NativeBinaryNumericOp::Mod)
}

fn emit_native_step_and_inline(code: &mut Vec<u8>, layout: NativeStackLayout) -> VmResult<()> {
    emit_native_step_bool_inline(code, layout, true)
}

fn emit_native_step_or_inline(code: &mut Vec<u8>, layout: NativeStackLayout) -> VmResult<()> {
    emit_native_step_bool_inline(code, layout, false)
}

fn emit_native_step_bool_inline(
    code: &mut Vec<u8>,
    layout: NativeStackLayout,
    is_and: bool,
) -> VmResult<()> {
    let stack_len_offset = vec_len_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_ptr_offset = vec_ptr_disp(layout.vm_stack_offset, layout.stack_vec)?;

    emit_ldr_x_disp(code, 9, VM_REG, stack_len_offset)?;
    emit_cmp_imm(code, 9, 2)?;
    let underflow = emit_b_cond_placeholder(code, Cond::Lo);

    emit_ldr_x_disp(code, 10, VM_REG, stack_ptr_offset)?;
    emit_sub_imm(code, 11, 9, 1);
    emit_mov_imm64(code, 14, layout.value.size as u64);
    emit_mul_x(code, 11, 11, 14);
    emit_add_reg(code, 13, 10, 11); // rhs
    emit_sub_reg(code, 11, 11, 14);
    emit_add_reg(code, 12, 10, 11); // lhs

    emit_load_tag_w_from_ptr(code, 16, 12, layout.value)?;
    emit_cmp_imm(
        code,
        16,
        u16::try_from(layout.value.bool_tag).unwrap_or(0xFFFF),
    )?;
    let lhs_not_bool = emit_b_cond_placeholder(code, Cond::Ne);
    emit_load_tag_w_from_ptr(code, 17, 13, layout.value)?;
    emit_cmp_imm(
        code,
        17,
        u16::try_from(layout.value.bool_tag).unwrap_or(0xFFFF),
    )?;
    let rhs_not_bool = emit_b_cond_placeholder(code, Cond::Ne);

    emit_ldr_b_disp(code, 14, 12, layout.value.bool_payload_offset)?;
    emit_ldr_b_disp(code, 15, 13, layout.value.bool_payload_offset)?;
    if is_and {
        emit_and_w(code, 14, 14, 15);
    } else {
        emit_orr_w(code, 14, 14, 15);
    }
    emit_store_bool_from_w(code, 14, 12, layout.value)?;
    emit_sub_imm(code, 9, 9, 1);
    emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
    emit_status_continue(code);

    let ok_done = emit_b_placeholder(code);
    let err_label = code.len();
    emit_status_error(code);
    let done_label = code.len();

    patch_b_cond_rel19(code, underflow, err_label)?;
    patch_b_cond_rel19(code, lhs_not_bool, err_label)?;
    patch_b_cond_rel19(code, rhs_not_bool, err_label)?;
    patch_b_rel26(code, ok_done, done_label)?;
    Ok(())
}

fn emit_native_step_pop_inline(code: &mut Vec<u8>, layout: NativeStackLayout) -> VmResult<()> {
    let stack_len_offset = vec_len_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_ptr_offset = vec_ptr_disp(layout.vm_stack_offset, layout.stack_vec)?;

    emit_ldr_x_disp(code, 9, VM_REG, stack_len_offset)?;
    emit_cmp_imm(code, 9, 1)?;
    let underflow = emit_b_cond_placeholder(code, Cond::Lo);

    emit_sub_imm(code, 11, 9, 1);
    emit_ldr_x_disp(code, 10, VM_REG, stack_ptr_offset)?;
    emit_mov_imm64(code, 14, layout.value.size as u64);
    emit_mul_x(code, 11, 11, 14);
    emit_add_reg(code, 12, 10, 11);

    emit_load_tag_w_from_ptr(code, 16, 12, layout.value)?;
    emit_cmp_imm(
        code,
        16,
        u16::try_from(layout.value.string_tag).unwrap_or(0xFFFF),
    )?;
    let is_string = emit_b_cond_placeholder(code, Cond::Eq);

    emit_sub_imm(code, 9, 9, 1);
    emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
    emit_status_continue(code);

    let ok_done = emit_b_placeholder(code);
    let err_label = code.len();
    emit_status_error(code);
    let done_label = code.len();

    patch_b_cond_rel19(code, underflow, err_label)?;
    patch_b_cond_rel19(code, is_string, err_label)?;
    patch_b_rel26(code, ok_done, done_label)?;
    Ok(())
}

fn emit_native_step_dup_inline(code: &mut Vec<u8>, layout: NativeStackLayout) -> VmResult<()> {
    let stack_len_offset = vec_len_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_ptr_offset = vec_ptr_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_cap_offset = vec_cap_disp(layout.vm_stack_offset, layout.stack_vec)?;

    emit_ldr_x_disp(code, 9, VM_REG, stack_len_offset)?;
    emit_ldr_x_disp(code, 15, VM_REG, stack_cap_offset)?;
    emit_cmp_imm(code, 9, 1)?;
    let underflow = emit_b_cond_placeholder(code, Cond::Lo);
    emit_cmp_reg(code, 9, 15);
    let no_cap = emit_b_cond_placeholder(code, Cond::Hs);

    emit_ldr_x_disp(code, 10, VM_REG, stack_ptr_offset)?;
    emit_sub_imm(code, 11, 9, 1);
    emit_mov_imm64(code, 14, layout.value.size as u64);
    emit_mul_x(code, 11, 11, 14);
    emit_add_reg(code, 13, 10, 11); // src top

    emit_load_tag_w_from_ptr(code, 16, 13, layout.value)?;
    emit_cmp_imm(
        code,
        16,
        u16::try_from(layout.value.string_tag).unwrap_or(0xFFFF),
    )?;
    let src_string = emit_b_cond_placeholder(code, Cond::Eq);

    emit_mov_imm64(code, 11, layout.value.size as u64);
    emit_mul_x(code, 11, 9, 11);
    emit_add_reg(code, 12, 10, 11); // dst at len
    emit_copy_value_ptr_to_ptr(code, layout.value, 13, 12)?;

    emit_add_imm(code, 9, 9, 1);
    emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
    emit_status_continue(code);

    let ok_done = emit_b_placeholder(code);
    let err_label = code.len();
    emit_status_error(code);
    let done_label = code.len();

    patch_b_cond_rel19(code, underflow, err_label)?;
    patch_b_cond_rel19(code, no_cap, err_label)?;
    patch_b_cond_rel19(code, src_string, err_label)?;
    patch_b_rel26(code, ok_done, done_label)?;
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

    emit_ldr_x_disp(code, 15, VM_REG, constants_len_offset)?;
    emit_mov_imm64(code, 14, u64::from(const_index));
    emit_cmp_reg(code, 14, 15);
    let bad_index = emit_b_cond_placeholder(code, Cond::Hs);

    emit_ldr_x_disp(code, 9, VM_REG, stack_len_offset)?;
    emit_ldr_x_disp(code, 15, VM_REG, stack_cap_offset)?;
    emit_cmp_reg(code, 9, 15);
    let no_cap = emit_b_cond_placeholder(code, Cond::Hs);

    emit_ldr_x_disp(code, 10, VM_REG, constants_ptr_offset)?;
    emit_mov_imm64(code, 11, u64::from(const_index));
    emit_mov_imm64(code, 14, layout.value.size as u64);
    emit_mul_x(code, 11, 11, 14);
    emit_add_reg(code, 13, 10, 11); // src const value

    emit_load_tag_w_from_ptr(code, 16, 13, layout.value)?;
    emit_cmp_imm(
        code,
        16,
        u16::try_from(layout.value.string_tag).unwrap_or(0xFFFF),
    )?;
    let src_string = emit_b_cond_placeholder(code, Cond::Eq);

    emit_ldr_x_disp(code, 10, VM_REG, stack_ptr_offset)?;
    emit_mov_imm64(code, 11, layout.value.size as u64);
    emit_mul_x(code, 11, 9, 11);
    emit_add_reg(code, 12, 10, 11); // dst stack slot
    emit_copy_value_ptr_to_ptr(code, layout.value, 13, 12)?;

    emit_add_imm(code, 9, 9, 1);
    emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
    emit_status_continue(code);

    let ok_done = emit_b_placeholder(code);
    let err_label = code.len();
    emit_status_error(code);
    let done_label = code.len();

    patch_b_cond_rel19(code, bad_index, err_label)?;
    patch_b_cond_rel19(code, no_cap, err_label)?;
    patch_b_cond_rel19(code, src_string, err_label)?;
    patch_b_rel26(code, ok_done, done_label)?;
    Ok(())
}

fn emit_native_step_ldloc_inline(
    code: &mut Vec<u8>,
    layout: NativeStackLayout,
    local_index: u8,
) -> VmResult<()> {
    let locals_len_offset = vec_len_disp(layout.vm_locals_offset, layout.stack_vec)?;
    let locals_ptr_offset = vec_ptr_disp(layout.vm_locals_offset, layout.stack_vec)?;
    let stack_len_offset = vec_len_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_ptr_offset = vec_ptr_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_cap_offset = vec_cap_disp(layout.vm_stack_offset, layout.stack_vec)?;

    emit_ldr_x_disp(code, 15, VM_REG, locals_len_offset)?;
    emit_mov_imm64(code, 14, u64::from(local_index));
    emit_cmp_reg(code, 14, 15);
    let bad_index = emit_b_cond_placeholder(code, Cond::Hs);

    emit_ldr_x_disp(code, 9, VM_REG, stack_len_offset)?;
    emit_ldr_x_disp(code, 15, VM_REG, stack_cap_offset)?;
    emit_cmp_reg(code, 9, 15);
    let no_cap = emit_b_cond_placeholder(code, Cond::Hs);

    emit_ldr_x_disp(code, 10, VM_REG, locals_ptr_offset)?;
    emit_mov_imm64(code, 11, u64::from(local_index));
    emit_mov_imm64(code, 14, layout.value.size as u64);
    emit_mul_x(code, 11, 11, 14);
    emit_add_reg(code, 13, 10, 11); // src local

    emit_load_tag_w_from_ptr(code, 16, 13, layout.value)?;
    emit_cmp_imm(
        code,
        16,
        u16::try_from(layout.value.string_tag).unwrap_or(0xFFFF),
    )?;
    let src_string = emit_b_cond_placeholder(code, Cond::Eq);

    emit_ldr_x_disp(code, 10, VM_REG, stack_ptr_offset)?;
    emit_mov_imm64(code, 11, layout.value.size as u64);
    emit_mul_x(code, 11, 9, 11);
    emit_add_reg(code, 12, 10, 11); // dst stack
    emit_copy_value_ptr_to_ptr(code, layout.value, 13, 12)?;

    emit_add_imm(code, 9, 9, 1);
    emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
    emit_status_continue(code);

    let ok_done = emit_b_placeholder(code);
    let err_label = code.len();
    emit_status_error(code);
    let done_label = code.len();

    patch_b_cond_rel19(code, bad_index, err_label)?;
    patch_b_cond_rel19(code, no_cap, err_label)?;
    patch_b_cond_rel19(code, src_string, err_label)?;
    patch_b_rel26(code, ok_done, done_label)?;
    Ok(())
}

fn emit_native_step_stloc_inline(
    code: &mut Vec<u8>,
    layout: NativeStackLayout,
    local_index: u8,
) -> VmResult<()> {
    let locals_len_offset = vec_len_disp(layout.vm_locals_offset, layout.stack_vec)?;
    let locals_ptr_offset = vec_ptr_disp(layout.vm_locals_offset, layout.stack_vec)?;
    let stack_len_offset = vec_len_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_ptr_offset = vec_ptr_disp(layout.vm_stack_offset, layout.stack_vec)?;

    emit_ldr_x_disp(code, 9, VM_REG, stack_len_offset)?;
    emit_cmp_imm(code, 9, 1)?;
    let underflow = emit_b_cond_placeholder(code, Cond::Lo);

    emit_ldr_x_disp(code, 15, VM_REG, locals_len_offset)?;
    emit_mov_imm64(code, 14, u64::from(local_index));
    emit_cmp_reg(code, 14, 15);
    let bad_index = emit_b_cond_placeholder(code, Cond::Hs);

    emit_ldr_x_disp(code, 10, VM_REG, stack_ptr_offset)?;
    emit_sub_imm(code, 11, 9, 1);
    emit_mov_imm64(code, 14, layout.value.size as u64);
    emit_mul_x(code, 11, 11, 14);
    emit_add_reg(code, 13, 10, 11); // src top

    emit_load_tag_w_from_ptr(code, 16, 13, layout.value)?;
    emit_cmp_imm(
        code,
        16,
        u16::try_from(layout.value.string_tag).unwrap_or(0xFFFF),
    )?;
    let src_string = emit_b_cond_placeholder(code, Cond::Eq);

    emit_ldr_x_disp(code, 10, VM_REG, locals_ptr_offset)?;
    emit_mov_imm64(code, 11, u64::from(local_index));
    emit_mov_imm64(code, 14, layout.value.size as u64);
    emit_mul_x(code, 11, 11, 14);
    emit_add_reg(code, 12, 10, 11); // dst local

    emit_load_tag_w_from_ptr(code, 16, 12, layout.value)?;
    emit_cmp_imm(
        code,
        16,
        u16::try_from(layout.value.string_tag).unwrap_or(0xFFFF),
    )?;
    let dst_string = emit_b_cond_placeholder(code, Cond::Eq);

    emit_copy_value_ptr_to_ptr(code, layout.value, 13, 12)?;
    emit_sub_imm(code, 9, 9, 1);
    emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
    emit_status_continue(code);

    let ok_done = emit_b_placeholder(code);
    let err_label = code.len();
    emit_status_error(code);
    let done_label = code.len();

    patch_b_cond_rel19(code, underflow, err_label)?;
    patch_b_cond_rel19(code, bad_index, err_label)?;
    patch_b_cond_rel19(code, src_string, err_label)?;
    patch_b_cond_rel19(code, dst_string, err_label)?;
    patch_b_rel26(code, ok_done, done_label)?;
    Ok(())
}

fn emit_native_step_ceq_inline(code: &mut Vec<u8>, layout: NativeStackLayout) -> VmResult<()> {
    let stack_len_offset = vec_len_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_ptr_offset = vec_ptr_disp(layout.vm_stack_offset, layout.stack_vec)?;

    emit_ldr_x_disp(code, 9, VM_REG, stack_len_offset)?;
    emit_cmp_imm(code, 9, 2)?;
    let underflow = emit_b_cond_placeholder(code, Cond::Lo);

    emit_ldr_x_disp(code, 10, VM_REG, stack_ptr_offset)?;
    emit_sub_imm(code, 11, 9, 1);
    emit_mov_imm64(code, 14, layout.value.size as u64);
    emit_mul_x(code, 11, 11, 14);
    emit_add_reg(code, 13, 10, 11); // rhs
    emit_sub_reg(code, 11, 11, 14);
    emit_add_reg(code, 12, 10, 11); // lhs

    emit_load_tag_w_from_ptr(code, 16, 12, layout.value)?;
    emit_load_tag_w_from_ptr(code, 17, 13, layout.value)?;

    emit_cmp_imm(
        code,
        16,
        u16::try_from(layout.value.string_tag).unwrap_or(0xFFFF),
    )?;
    let lhs_string = emit_b_cond_placeholder(code, Cond::Eq);
    emit_cmp_imm(
        code,
        17,
        u16::try_from(layout.value.string_tag).unwrap_or(0xFFFF),
    )?;
    let rhs_string = emit_b_cond_placeholder(code, Cond::Eq);

    emit_cmp_w_reg(code, 16, 17);
    let tags_not_equal = emit_b_cond_placeholder(code, Cond::Ne);

    emit_cmp_imm(
        code,
        16,
        u16::try_from(layout.value.int_tag).unwrap_or(0xFFFF),
    )?;
    let tag_is_int = emit_b_cond_placeholder(code, Cond::Eq);
    emit_cmp_imm(
        code,
        16,
        u16::try_from(layout.value.bool_tag).unwrap_or(0xFFFF),
    )?;
    let tag_is_bool = emit_b_cond_placeholder(code, Cond::Eq);
    let unknown_tag = emit_b_placeholder(code);

    let int_label = code.len();
    emit_ldr_x_disp(code, 14, 12, layout.value.int_payload_offset)?;
    emit_ldr_x_disp(code, 15, 13, layout.value.int_payload_offset)?;
    emit_cmp_reg(code, 14, 15);
    emit_mov_imm64(code, 14, 0);
    let int_ne = emit_b_cond_placeholder(code, Cond::Ne);
    emit_mov_imm64(code, 14, 1);
    let int_done = emit_b_placeholder(code);

    let bool_label = code.len();
    emit_ldr_b_disp(code, 14, 12, layout.value.bool_payload_offset)?;
    emit_ldr_b_disp(code, 15, 13, layout.value.bool_payload_offset)?;
    emit_cmp_reg(code, 14, 15);
    emit_mov_imm64(code, 14, 0);
    let bool_ne = emit_b_cond_placeholder(code, Cond::Ne);
    emit_mov_imm64(code, 14, 1);
    let bool_done = emit_b_placeholder(code);

    let not_equal_label = code.len();
    emit_mov_imm64(code, 14, 0);
    let ne_done = emit_b_placeholder(code);

    let result_label = code.len();
    emit_store_bool_from_w(code, 14, 12, layout.value)?;
    emit_sub_imm(code, 9, 9, 1);
    emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
    emit_status_continue(code);
    let ok_done = emit_b_placeholder(code);

    let err_label = code.len();
    emit_status_error(code);
    let done_label = code.len();

    patch_b_cond_rel19(code, tag_is_int, int_label)?;
    patch_b_cond_rel19(code, tag_is_bool, bool_label)?;
    patch_b_rel26(code, unknown_tag, err_label)?;
    patch_b_cond_rel19(code, int_ne, result_label)?;
    patch_b_rel26(code, int_done, result_label)?;
    patch_b_cond_rel19(code, bool_ne, result_label)?;
    patch_b_rel26(code, bool_done, result_label)?;
    patch_b_cond_rel19(code, tags_not_equal, not_equal_label)?;
    patch_b_rel26(code, ne_done, result_label)?;
    patch_b_cond_rel19(code, underflow, err_label)?;
    patch_b_cond_rel19(code, lhs_string, err_label)?;
    patch_b_cond_rel19(code, rhs_string, err_label)?;
    patch_b_rel26(code, ok_done, done_label)?;
    Ok(())
}

fn emit_native_step_guard_false_inline(
    code: &mut Vec<u8>,
    layout: NativeStackLayout,
    exit_ip: u64,
) -> VmResult<()> {
    let stack_len_offset = vec_len_disp(layout.vm_stack_offset, layout.stack_vec)?;
    let stack_ptr_offset = vec_ptr_disp(layout.vm_stack_offset, layout.stack_vec)?;

    emit_ldr_x_disp(code, 9, VM_REG, stack_len_offset)?;
    emit_cmp_imm(code, 9, 1)?;
    let underflow = emit_b_cond_placeholder(code, Cond::Lo);

    emit_sub_imm(code, 9, 9, 1);
    emit_ldr_x_disp(code, 10, VM_REG, stack_ptr_offset)?;
    emit_mov_imm64(code, 14, layout.value.size as u64);
    emit_mul_x(code, 11, 9, 14);
    emit_add_reg(code, 12, 10, 11);

    emit_load_tag_w_from_ptr(code, 16, 12, layout.value)?;
    emit_cmp_imm(
        code,
        16,
        u16::try_from(layout.value.bool_tag).unwrap_or(0xFFFF),
    )?;
    let bad_type = emit_b_cond_placeholder(code, Cond::Ne);

    emit_ldr_b_disp(code, 14, 12, layout.value.bool_payload_offset)?;
    emit_str_x_disp(code, 9, VM_REG, stack_len_offset)?;
    emit_cmp_imm(code, 14, 0)?;
    let cond_true = emit_b_cond_placeholder(code, Cond::Ne);

    emit_mov_imm64(code, 14, exit_ip);
    emit_str_x_disp(code, 14, VM_REG, layout.vm_ip_offset)?;
    emit_status_trace_exit(code);
    let false_done = emit_b_placeholder(code);

    let true_label = code.len();
    emit_status_continue(code);
    let ok_done = emit_b_placeholder(code);

    let err_label = code.len();
    emit_status_error(code);
    let done_label = code.len();

    patch_b_cond_rel19(code, underflow, err_label)?;
    patch_b_cond_rel19(code, bad_type, err_label)?;
    patch_b_cond_rel19(code, cond_true, true_label)?;
    patch_b_rel26(code, false_done, done_label)?;
    patch_b_rel26(code, ok_done, done_label)?;
    Ok(())
}

fn emit_native_step_jump_to_ip_inline(
    code: &mut Vec<u8>,
    layout: NativeStackLayout,
    target_ip: u64,
) -> VmResult<()> {
    emit_mov_imm64(code, 14, target_ip);
    emit_str_x_disp(code, 14, VM_REG, layout.vm_ip_offset)?;
    emit_status_trace_exit(code);
    Ok(())
}

fn emit_native_step_ret_inline(code: &mut Vec<u8>) {
    emit_status_halted(code);
}

fn helper_ptr_to_u64(ptr: *const (), name: &str) -> VmResult<u64> {
    let addr = ptr as usize;
    u64::try_from(addr)
        .map_err(|_| VmError::JitNative(format!("native {name} pointer exceeds 64-bit range")))
}

fn emit_vm_helper_call0(code: &mut Vec<u8>, helper_addr: u64) {
    emit_mov_reg(code, 0, VM_REG);
    emit_mov_imm64(code, 16, helper_addr);
    emit_u32(code, 0xD63F0200); // blr x16
}

fn emit_native_step_call_inline(
    code: &mut Vec<u8>,
    index: u16,
    argc: u8,
    call_ip: usize,
) -> VmResult<()> {
    let call_ip = u64::try_from(call_ip)
        .map_err(|_| VmError::JitNative("trace call_ip exceeds 64-bit range".to_string()))?;
    let helper_addr = helper_ptr_to_u64(jit_native_call_bridge as *const (), "call helper")?;

    emit_mov_reg(code, 0, VM_REG);
    emit_mov_imm64(code, 1, u64::from(index));
    emit_mov_imm64(code, 2, u64::from(argc));
    emit_mov_imm64(code, 3, call_ip);
    emit_mov_imm64(code, 16, helper_addr);
    emit_u32(code, 0xD63F0200); // blr x16
    Ok(())
}

fn emit_copy_value_ptr_to_ptr(
    code: &mut Vec<u8>,
    value_layout: ValueLayout,
    src_ptr_reg: u8,
    dst_ptr_reg: u8,
) -> VmResult<()> {
    let mut copied = 0i32;
    while copied + 8 <= value_layout.size {
        emit_ldr_x_disp(code, 14, src_ptr_reg, copied)?;
        emit_str_x_disp(code, 14, dst_ptr_reg, copied)?;
        copied += 8;
    }
    if value_layout.size - copied >= 4 {
        emit_ldr_w_disp(code, 14, src_ptr_reg, copied)?;
        emit_str_w_disp(code, 14, dst_ptr_reg, copied)?;
        copied += 4;
    }
    if value_layout.size - copied >= 2 {
        emit_ldr_h_disp(code, 14, src_ptr_reg, copied)?;
        emit_str_h_disp(code, 14, dst_ptr_reg, copied)?;
        copied += 2;
    }
    if value_layout.size - copied >= 1 {
        emit_ldr_b_disp(code, 14, src_ptr_reg, copied)?;
        emit_str_b_disp(code, 14, dst_ptr_reg, copied)?;
    }
    Ok(())
}

fn emit_load_tag_w_from_ptr(
    code: &mut Vec<u8>,
    dst_w: u8,
    ptr_x: u8,
    value_layout: ValueLayout,
) -> VmResult<()> {
    match value_layout.tag_size {
        1 => emit_ldr_b_disp(code, dst_w, ptr_x, value_layout.tag_offset),
        2 => emit_ldr_h_disp(code, dst_w, ptr_x, value_layout.tag_offset),
        4 => emit_ldr_w_disp(code, dst_w, ptr_x, value_layout.tag_offset),
        other => Err(VmError::JitNative(format!(
            "unsupported native tag width {}",
            other
        ))),
    }
}

fn emit_store_tag_ptr(
    code: &mut Vec<u8>,
    ptr_x: u8,
    value_layout: ValueLayout,
    tag: u32,
) -> VmResult<()> {
    match value_layout.tag_size {
        1 => {
            let v = u8::try_from(tag).map_err(|_| {
                VmError::JitNative("native value tag out of byte range".to_string())
            })?;
            emit_mov_imm64(code, 14, u64::from(v));
            emit_str_b_disp(code, 14, ptr_x, value_layout.tag_offset)
        }
        2 => {
            let v = u16::try_from(tag).map_err(|_| {
                VmError::JitNative("native value tag out of word range".to_string())
            })?;
            emit_mov_imm64(code, 14, u64::from(v));
            emit_str_h_disp(code, 14, ptr_x, value_layout.tag_offset)
        }
        4 => {
            emit_mov_imm64(code, 14, u64::from(tag));
            emit_str_w_disp(code, 14, ptr_x, value_layout.tag_offset)
        }
        other => Err(VmError::JitNative(format!(
            "unsupported native tag width {}",
            other
        ))),
    }
}

fn emit_store_bool_from_w(
    code: &mut Vec<u8>,
    src_w: u8,
    ptr_x: u8,
    value_layout: ValueLayout,
) -> VmResult<()> {
    emit_mov_reg(code, 15, src_w);
    emit_store_tag_ptr(code, ptr_x, value_layout, value_layout.bool_tag)?;
    emit_str_b_disp(code, 15, ptr_x, value_layout.bool_payload_offset)
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

fn emit_u32(code: &mut Vec<u8>, insn: u32) {
    code.extend_from_slice(&insn.to_le_bytes());
}

fn emit_mov_reg(code: &mut Vec<u8>, dst: u8, src: u8) {
    let insn = 0xAA0003E0_u32 | ((src as u32) << 16) | (dst as u32);
    emit_u32(code, insn);
}

fn emit_mov_imm64(code: &mut Vec<u8>, dst: u8, value: u64) {
    let parts = [
        (value & 0xFFFF) as u16,
        ((value >> 16) & 0xFFFF) as u16,
        ((value >> 32) & 0xFFFF) as u16,
        ((value >> 48) & 0xFFFF) as u16,
    ];

    let mut first = None;
    for (i, part) in parts.iter().enumerate() {
        if *part != 0 {
            first = Some(i);
            break;
        }
    }

    let Some(first_index) = first else {
        emit_u32(code, 0xD2800000_u32 | (dst as u32));
        return;
    };

    emit_u32(
        code,
        0xD2800000_u32
            | ((first_index as u32) << 21)
            | ((parts[first_index] as u32) << 5)
            | (dst as u32),
    );

    for (i, part) in parts.iter().enumerate() {
        if i == first_index || *part == 0 {
            continue;
        }
        emit_u32(
            code,
            0xF2800000_u32 | ((i as u32) << 21) | ((*part as u32) << 5) | (dst as u32),
        );
    }
}

fn emit_add_imm(code: &mut Vec<u8>, dst: u8, src: u8, imm12: u16) {
    let insn = 0x91000000_u32 | ((imm12 as u32) << 10) | ((src as u32) << 5) | (dst as u32);
    emit_u32(code, insn);
}

fn emit_sub_imm(code: &mut Vec<u8>, dst: u8, src: u8, imm12: u16) {
    let insn = 0xD1000000_u32 | ((imm12 as u32) << 10) | ((src as u32) << 5) | (dst as u32);
    emit_u32(code, insn);
}

fn emit_add_reg(code: &mut Vec<u8>, dst: u8, lhs: u8, rhs: u8) {
    let insn = 0x8B000000_u32 | ((rhs as u32) << 16) | ((lhs as u32) << 5) | (dst as u32);
    emit_u32(code, insn);
}

fn emit_sub_reg(code: &mut Vec<u8>, dst: u8, lhs: u8, rhs: u8) {
    let insn = 0xCB000000_u32 | ((rhs as u32) << 16) | ((lhs as u32) << 5) | (dst as u32);
    emit_u32(code, insn);
}

fn emit_mul_x(code: &mut Vec<u8>, dst: u8, lhs: u8, rhs: u8) {
    let insn = 0x9B007C00_u32 | ((rhs as u32) << 16) | ((lhs as u32) << 5) | (dst as u32);
    emit_u32(code, insn);
}

fn emit_sdiv_x(code: &mut Vec<u8>, dst: u8, lhs: u8, rhs: u8) {
    let insn = 0x9AC00C00_u32 | ((rhs as u32) << 16) | ((lhs as u32) << 5) | (dst as u32);
    emit_u32(code, insn);
}

fn emit_lslv_x(code: &mut Vec<u8>, dst: u8, lhs: u8, rhs: u8) {
    let insn = 0x9AC02000_u32 | ((rhs as u32) << 16) | ((lhs as u32) << 5) | (dst as u32);
    emit_u32(code, insn);
}

fn emit_asrv_x(code: &mut Vec<u8>, dst: u8, lhs: u8, rhs: u8) {
    let insn = 0x9AC02800_u32 | ((rhs as u32) << 16) | ((lhs as u32) << 5) | (dst as u32);
    emit_u32(code, insn);
}

fn emit_cmp_reg(code: &mut Vec<u8>, lhs: u8, rhs: u8) {
    let insn = 0xEB00001F_u32 | ((rhs as u32) << 16) | ((lhs as u32) << 5);
    emit_u32(code, insn);
}

fn emit_cmp_w_reg(code: &mut Vec<u8>, lhs: u8, rhs: u8) {
    let insn = 0x6B00001F_u32 | ((rhs as u32) << 16) | ((lhs as u32) << 5);
    emit_u32(code, insn);
}

fn emit_cmp_imm(code: &mut Vec<u8>, lhs: u8, imm: u16) -> VmResult<()> {
    if imm > 4095 {
        return Err(VmError::JitNative(format!(
            "cmp immediate {} exceeds encodable range",
            imm
        )));
    }
    let insn = 0xF100001F_u32 | ((imm as u32) << 10) | ((lhs as u32) << 5);
    emit_u32(code, insn);
    Ok(())
}

fn encode_imm12_scaled(offset: i32, scale: i32, label: &str) -> VmResult<u32> {
    if offset < 0 {
        return Err(VmError::JitNative(format!(
            "{} negative offset {} unsupported",
            label, offset
        )));
    }
    let unit = 1_i32 << scale;
    if offset % unit != 0 {
        return Err(VmError::JitNative(format!(
            "{} offset {} misaligned for scale {}",
            label, offset, unit
        )));
    }
    let imm = offset / unit;
    if imm > 4095 {
        return Err(VmError::JitNative(format!(
            "{} offset {} exceeds immediate range",
            label, offset
        )));
    }
    Ok((imm as u32) << 10)
}

fn emit_ldr_x_imm12(code: &mut Vec<u8>, rt: u8, rn: u8, offset: i32) -> VmResult<()> {
    let insn = 0xF9400000_u32
        | encode_imm12_scaled(offset, 3, "ldr x")?
        | ((rn as u32) << 5)
        | (rt as u32);
    emit_u32(code, insn);
    Ok(())
}

fn emit_str_x_imm12(code: &mut Vec<u8>, rt: u8, rn: u8, offset: i32) -> VmResult<()> {
    let insn = 0xF9000000_u32
        | encode_imm12_scaled(offset, 3, "str x")?
        | ((rn as u32) << 5)
        | (rt as u32);
    emit_u32(code, insn);
    Ok(())
}

fn emit_ldr_w_imm12(code: &mut Vec<u8>, rt: u8, rn: u8, offset: i32) -> VmResult<()> {
    let insn = 0xB9400000_u32
        | encode_imm12_scaled(offset, 2, "ldr w")?
        | ((rn as u32) << 5)
        | (rt as u32);
    emit_u32(code, insn);
    Ok(())
}

fn emit_str_w_imm12(code: &mut Vec<u8>, rt: u8, rn: u8, offset: i32) -> VmResult<()> {
    let insn = 0xB9000000_u32
        | encode_imm12_scaled(offset, 2, "str w")?
        | ((rn as u32) << 5)
        | (rt as u32);
    emit_u32(code, insn);
    Ok(())
}

fn emit_ldr_h_imm12(code: &mut Vec<u8>, rt: u8, rn: u8, offset: i32) -> VmResult<()> {
    let insn = 0x79400000_u32
        | encode_imm12_scaled(offset, 1, "ldr h")?
        | ((rn as u32) << 5)
        | (rt as u32);
    emit_u32(code, insn);
    Ok(())
}

fn emit_str_h_imm12(code: &mut Vec<u8>, rt: u8, rn: u8, offset: i32) -> VmResult<()> {
    let insn = 0x79000000_u32
        | encode_imm12_scaled(offset, 1, "str h")?
        | ((rn as u32) << 5)
        | (rt as u32);
    emit_u32(code, insn);
    Ok(())
}

fn emit_ldr_b_imm12(code: &mut Vec<u8>, rt: u8, rn: u8, offset: i32) -> VmResult<()> {
    let insn = 0x39400000_u32
        | encode_imm12_scaled(offset, 0, "ldr b")?
        | ((rn as u32) << 5)
        | (rt as u32);
    emit_u32(code, insn);
    Ok(())
}

fn emit_str_b_imm12(code: &mut Vec<u8>, rt: u8, rn: u8, offset: i32) -> VmResult<()> {
    let insn = 0x39000000_u32
        | encode_imm12_scaled(offset, 0, "str b")?
        | ((rn as u32) << 5)
        | (rt as u32);
    emit_u32(code, insn);
    Ok(())
}

fn emit_ldr_d_imm12(code: &mut Vec<u8>, vt: u8, rn: u8, offset: i32) -> VmResult<()> {
    let insn = 0xFD400000_u32
        | encode_imm12_scaled(offset, 3, "ldr d")?
        | ((rn as u32) << 5)
        | (vt as u32);
    emit_u32(code, insn);
    Ok(())
}

fn emit_str_d_imm12(code: &mut Vec<u8>, vt: u8, rn: u8, offset: i32) -> VmResult<()> {
    let insn = 0xFD000000_u32
        | encode_imm12_scaled(offset, 3, "str d")?
        | ((rn as u32) << 5)
        | (vt as u32);
    emit_u32(code, insn);
    Ok(())
}

fn emit_ldr_x_disp(code: &mut Vec<u8>, rt: u8, rn: u8, offset: i32) -> VmResult<()> {
    emit_ldr_x_imm12(code, rt, rn, offset)
}

fn emit_str_x_disp(code: &mut Vec<u8>, rt: u8, rn: u8, offset: i32) -> VmResult<()> {
    emit_str_x_imm12(code, rt, rn, offset)
}

fn emit_ldr_w_disp(code: &mut Vec<u8>, rt: u8, rn: u8, offset: i32) -> VmResult<()> {
    emit_ldr_w_imm12(code, rt, rn, offset)
}

fn emit_str_w_disp(code: &mut Vec<u8>, rt: u8, rn: u8, offset: i32) -> VmResult<()> {
    emit_str_w_imm12(code, rt, rn, offset)
}

fn emit_ldr_h_disp(code: &mut Vec<u8>, rt: u8, rn: u8, offset: i32) -> VmResult<()> {
    emit_ldr_h_imm12(code, rt, rn, offset)
}

fn emit_str_h_disp(code: &mut Vec<u8>, rt: u8, rn: u8, offset: i32) -> VmResult<()> {
    emit_str_h_imm12(code, rt, rn, offset)
}

fn emit_ldr_b_disp(code: &mut Vec<u8>, rt: u8, rn: u8, offset: i32) -> VmResult<()> {
    emit_ldr_b_imm12(code, rt, rn, offset)
}

fn emit_str_b_disp(code: &mut Vec<u8>, rt: u8, rn: u8, offset: i32) -> VmResult<()> {
    emit_str_b_imm12(code, rt, rn, offset)
}

fn emit_ldr_d_disp(code: &mut Vec<u8>, vt: u8, rn: u8, offset: i32) -> VmResult<()> {
    emit_ldr_d_imm12(code, vt, rn, offset)
}

fn emit_str_d_disp(code: &mut Vec<u8>, vt: u8, rn: u8, offset: i32) -> VmResult<()> {
    emit_str_d_imm12(code, vt, rn, offset)
}

fn emit_scvtf_d_from_x(code: &mut Vec<u8>, vd: u8, xn: u8) {
    let insn = 0x9E620000_u32 | ((xn as u32) << 5) | (vd as u32);
    emit_u32(code, insn);
}

fn emit_fcvtzs_x_from_d(code: &mut Vec<u8>, xd: u8, vn: u8) {
    let insn = 0x9E780000_u32 | ((vn as u32) << 5) | (xd as u32);
    emit_u32(code, insn);
}

fn emit_fadd_d(code: &mut Vec<u8>, vd: u8, vn: u8, vm: u8) {
    let insn = 0x1E602800_u32 | ((vm as u32) << 16) | ((vn as u32) << 5) | (vd as u32);
    emit_u32(code, insn);
}

fn emit_fsub_d(code: &mut Vec<u8>, vd: u8, vn: u8, vm: u8) {
    let insn = 0x1E603800_u32 | ((vm as u32) << 16) | ((vn as u32) << 5) | (vd as u32);
    emit_u32(code, insn);
}

fn emit_fmul_d(code: &mut Vec<u8>, vd: u8, vn: u8, vm: u8) {
    let insn = 0x1E600800_u32 | ((vm as u32) << 16) | ((vn as u32) << 5) | (vd as u32);
    emit_u32(code, insn);
}

fn emit_fdiv_d(code: &mut Vec<u8>, vd: u8, vn: u8, vm: u8) {
    let insn = 0x1E601800_u32 | ((vm as u32) << 16) | ((vn as u32) << 5) | (vd as u32);
    emit_u32(code, insn);
}

fn emit_fcmp_d(code: &mut Vec<u8>, vn: u8, vm: u8) {
    let insn = 0x1E602000_u32 | ((vm as u32) << 16) | ((vn as u32) << 5);
    emit_u32(code, insn);
}

fn emit_fcmp_d_zero(code: &mut Vec<u8>, vn: u8) {
    let insn = 0x1E602008_u32 | ((vn as u32) << 5);
    emit_u32(code, insn);
}

fn emit_fneg_d(code: &mut Vec<u8>, vd: u8, vn: u8) {
    let insn = 0x1E614000_u32 | ((vn as u32) << 5) | (vd as u32);
    emit_u32(code, insn);
}

fn emit_and_w(code: &mut Vec<u8>, wd: u8, wn: u8, wm: u8) {
    let insn = 0x0A000000_u32 | ((wm as u32) << 16) | ((wn as u32) << 5) | (wd as u32);
    emit_u32(code, insn);
}

fn emit_orr_w(code: &mut Vec<u8>, wd: u8, wn: u8, wm: u8) {
    let insn = 0x2A000000_u32 | ((wm as u32) << 16) | ((wn as u32) << 5) | (wd as u32);
    emit_u32(code, insn);
}

fn emit_b_placeholder(code: &mut Vec<u8>) -> usize {
    let at = code.len();
    emit_u32(code, 0x14000000);
    at
}

fn emit_b_cond_placeholder(code: &mut Vec<u8>, cond: Cond) -> usize {
    let at = code.len();
    emit_u32(code, 0x54000000 | (cond as u32));
    at
}

fn patch_b_rel26(code: &mut [u8], at: usize, target: usize) -> VmResult<()> {
    if at + 4 > code.len() {
        return Err(VmError::JitNative(
            "native b patch offset out of bounds".to_string(),
        ));
    }
    let diff = (target as isize) - (at as isize);
    if diff % 4 != 0 {
        return Err(VmError::JitNative(
            "native b patch has unaligned branch displacement".to_string(),
        ));
    }
    let imm26 = diff / 4;
    if !(-(1 << 25)..(1 << 25)).contains(&imm26) {
        return Err(VmError::JitNative(
            "native b patch displacement out of range".to_string(),
        ));
    }
    let imm26_u32 = (imm26 as i32 as u32) & 0x03FF_FFFF;
    let insn = 0x14000000_u32 | imm26_u32;
    code[at..at + 4].copy_from_slice(&insn.to_le_bytes());
    Ok(())
}

fn patch_b_cond_rel19(code: &mut [u8], at: usize, target: usize) -> VmResult<()> {
    if at + 4 > code.len() {
        return Err(VmError::JitNative(
            "native b.cond patch offset out of bounds".to_string(),
        ));
    }
    let original = u32::from_le_bytes([code[at], code[at + 1], code[at + 2], code[at + 3]]);
    let cond = original & 0xF;
    let diff = (target as isize) - (at as isize);
    if diff % 4 != 0 {
        return Err(VmError::JitNative(
            "native b.cond patch has unaligned branch displacement".to_string(),
        ));
    }
    let imm19 = diff / 4;
    if !(-(1 << 18)..(1 << 18)).contains(&imm19) {
        return Err(VmError::JitNative(
            "native b.cond patch displacement out of range".to_string(),
        ));
    }
    let imm19_u32 = (imm19 as i32 as u32) & 0x7FFFF;
    let insn = 0x54000000_u32 | (imm19_u32 << 5) | cond;
    code[at..at + 4].copy_from_slice(&insn.to_le_bytes());
    Ok(())
}

fn patch_cbnz_w_rel19(code: &mut [u8], at: usize, target: usize) -> VmResult<()> {
    if at + 4 > code.len() {
        return Err(VmError::JitNative(
            "native cbnz patch offset out of bounds".to_string(),
        ));
    }
    let diff = (target as isize) - (at as isize);
    if diff % 4 != 0 {
        return Err(VmError::JitNative(
            "native cbnz patch has unaligned branch displacement".to_string(),
        ));
    }
    let imm19 = diff / 4;
    if !(-(1 << 18)..(1 << 18)).contains(&imm19) {
        return Err(VmError::JitNative(
            "native cbnz patch displacement out of range".to_string(),
        ));
    }
    let imm19_u32 = (imm19 as i32 as u32) & 0x7FFFF;
    let insn = 0x35000000_u32 | (imm19_u32 << 5);
    code[at..at + 4].copy_from_slice(&insn.to_le_bytes());
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
    let int_a_bytes = encode_value_bytes(Value::Int(int_a));
    let int_b_bytes = encode_value_bytes(Value::Int(int_b));
    let float_a_bytes = encode_value_bytes(Value::Float(float_a));
    let float_b_bytes = encode_value_bytes(Value::Float(float_b));
    let bool_false_bytes = encode_value_bytes(Value::Bool(false));
    let bool_true_bytes = encode_value_bytes(Value::Bool(true));
    let string_a_bytes = encode_value_bytes(Value::String("a".to_string()));
    let string_b_bytes = encode_value_bytes(Value::String("b".to_string()));
    let stable_tag_pairs = [
        (&int_a_bytes[..], &int_b_bytes[..]),
        (&float_a_bytes[..], &float_b_bytes[..]),
        (&bool_false_bytes[..], &bool_true_bytes[..]),
        (&string_a_bytes[..], &string_b_bytes[..]),
    ];
    let (tag_offset, tag_size) = detect_tag_layout(&stable_tag_pairs)?;
    let int_tag = decode_tag(&int_a_bytes, tag_offset, tag_size);
    let float_tag = decode_tag(&float_a_bytes, tag_offset, tag_size);
    let bool_tag = decode_tag(&bool_false_bytes, tag_offset, tag_size);
    let string_tag = decode_tag(&string_a_bytes, tag_offset, tag_size);

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

    Ok(ValueLayout {
        size: usize_to_i32(value_size, "Value size")?,
        tag_offset: usize_to_i32(tag_offset, "Value tag offset")?,
        tag_size: tag_size as u8,
        int_tag,
        float_tag,
        bool_tag,
        string_tag,
        int_payload_offset: usize_to_i32(int_payload_offset, "Value::Int payload offset")?,
        float_payload_offset: usize_to_i32(float_payload_offset, "Value::Float payload offset")?,
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

#[cfg(target_os = "linux")]
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

fn write_machine_code(ptr: *mut u8, code: &[u8]) -> VmResult<()> {
    #[cfg(target_os = "macos")]
    unsafe {
        let use_write_protect = pthread_jit_write_protect_supported_np() != 0;
        if use_write_protect {
            pthread_jit_write_protect_np(0);
        }
        std::ptr::copy_nonoverlapping(code.as_ptr(), ptr, code.len());
        sys_icache_invalidate(ptr as *mut libc::c_void, code.len());
        if use_write_protect {
            pthread_jit_write_protect_np(1);
        }
    }

    #[cfg(target_os = "linux")]
    unsafe {
        std::ptr::copy_nonoverlapping(code.as_ptr(), ptr, code.len());
        __clear_cache(
            ptr as *mut libc::c_char,
            ptr.add(code.len()) as *mut libc::c_char,
        );
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (ptr, code);
        return Err(VmError::JitNative(
            "unsupported platform for aarch64 native code writes".to_string(),
        ));
    }

    Ok(())
}

#[cfg(target_os = "linux")]
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
    fn sys_icache_invalidate(start: *mut libc::c_void, len: libc::size_t);
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn __clear_cache(begin: *mut libc::c_char, end: *mut libc::c_char);
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn execute_trace(vm: &mut Vm, trace: JitTrace) -> VmResult<i32> {
        let code = emit_native_trace_bytes(&trace)?;
        let memory = BackendExecutableMemory::from_code(&code)?;
        let entry = unsafe { std::mem::transmute::<*mut u8, NativeEntry>(memory.ptr) };
        clear_bridge_error();
        let status = unsafe { entry(vm as *mut Vm) };
        drop(memory);
        Ok(status)
    }

    fn execute_trace_with_entry(
        vm: &mut Vm,
        trace: JitTrace,
    ) -> VmResult<(i32, BackendExecutableMemory)> {
        let code = emit_native_trace_bytes(&trace)?;
        let memory = BackendExecutableMemory::from_code(&code)?;
        let entry = unsafe { std::mem::transmute::<*mut u8, NativeEntry>(memory.ptr) };
        clear_bridge_error();
        let status = unsafe { entry(vm as *mut Vm) };
        Ok((status, memory))
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

    #[test]
    fn add_step_executes() {
        let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        vm.stack.push(Value::Int(2));
        vm.stack.push(Value::Int(3));

        let status = execute_single_step(&mut vm, TraceStep::Add).expect("native add should run");
        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(vm.stack, vec![Value::Int(5)]);
    }

    #[test]
    fn add_step_supports_float_and_mixed_numeric() {
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
    fn neg_step_supports_float() {
        let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        vm.stack.push(Value::Float(8.5));

        let status = execute_single_step(&mut vm, TraceStep::Neg).expect("native neg should run");
        assert_eq!(status, STATUS_CONTINUE);
        match vm.stack.last() {
            Some(Value::Float(value)) => assert!((*value + 8.5).abs() < f64::EPSILON),
            other => panic!("expected float result, got {other:?}"),
        }
    }

    #[test]
    fn div_step_supports_float_and_rejects_zero() {
        let mut ok_vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        ok_vm.stack.push(Value::Float(7.5));
        ok_vm.stack.push(Value::Float(2.5));
        let status =
            execute_single_step(&mut ok_vm, TraceStep::Div).expect("native float div should run");
        assert_eq!(status, STATUS_CONTINUE);
        match ok_vm.stack.last() {
            Some(Value::Float(value)) => assert!((*value - 3.0).abs() < f64::EPSILON),
            other => panic!("expected float result, got {other:?}"),
        }

        let mut err_vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        err_vm.stack.push(Value::Float(1.0));
        err_vm.stack.push(Value::Float(-0.0));
        let status =
            execute_single_step(&mut err_vm, TraceStep::Div).expect("native float div should run");
        assert_eq!(status, STATUS_ERROR);
    }

    #[test]
    fn mod_step_supports_int_and_float_and_rejects_zero() {
        let mut int_vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        int_vm.stack.push(Value::Int(17));
        int_vm.stack.push(Value::Int(5));
        let status =
            execute_single_step(&mut int_vm, TraceStep::Mod).expect("native mod should run");
        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(int_vm.stack, vec![Value::Int(2)]);

        let mut float_vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        float_vm.stack.push(Value::Float(7.5));
        float_vm.stack.push(Value::Float(2.0));
        let status =
            execute_single_step(&mut float_vm, TraceStep::Mod).expect("native mod should run");
        assert_eq!(status, STATUS_CONTINUE);
        match float_vm.stack.last() {
            Some(Value::Float(value)) => assert!((*value - 1.5).abs() < f64::EPSILON),
            other => panic!("expected float result, got {other:?}"),
        }

        let mut err_vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        err_vm.stack.push(Value::Int(1));
        err_vm.stack.push(Value::Int(0));
        let status =
            execute_single_step(&mut err_vm, TraceStep::Mod).expect("native mod should run");
        assert_eq!(status, STATUS_ERROR);
    }

    #[test]
    fn and_or_steps_execute_boolean_logic() {
        let mut and_vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        and_vm.stack.push(Value::Bool(true));
        and_vm.stack.push(Value::Bool(false));
        let status =
            execute_single_step(&mut and_vm, TraceStep::And).expect("native and should run");
        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(and_vm.stack, vec![Value::Bool(false)]);

        let mut or_vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        or_vm.stack.push(Value::Bool(true));
        or_vm.stack.push(Value::Bool(false));
        let status = execute_single_step(&mut or_vm, TraceStep::Or).expect("native or should run");
        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(or_vm.stack, vec![Value::Bool(true)]);
    }

    #[test]
    fn clt_supports_float_and_nan_is_false() {
        let mut clt_vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        clt_vm.stack.push(Value::Float(1.5));
        clt_vm.stack.push(Value::Float(2.0));
        let status =
            execute_single_step(&mut clt_vm, TraceStep::Clt).expect("native clt should run");
        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(clt_vm.stack, vec![Value::Bool(true)]);

        let mut nan_vm = Vm::new(Program::new(Vec::new(), Vec::new()));
        nan_vm.stack.push(Value::Float(f64::NAN));
        nan_vm.stack.push(Value::Float(1.0));
        let status =
            execute_single_step(&mut nan_vm, TraceStep::Clt).expect("native clt should run");
        assert_eq!(status, STATUS_CONTINUE);
        assert_eq!(nan_vm.stack, vec![Value::Bool(false)]);
    }

    #[test]
    fn add_step_emits_no_helper_call() {
        let trace = build_single_step_trace(TraceStep::Add);
        let code = emit_native_trace_bytes(&trace).expect("trace should compile");
        let call_count = code
            .chunks_exact(4)
            .filter(|chunk| {
                u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) == 0xD63F0200
            })
            .count();
        assert_eq!(call_count, 0, "add should not emit helper calls");
    }

    #[test]
    fn call_step_emits_helper_call() {
        let trace = build_single_step_trace(TraceStep::Call {
            index: 0,
            argc: 1,
            call_ip: 0,
        });
        let code = emit_native_trace_bytes(&trace).expect("trace should compile");
        let call_count = code
            .chunks_exact(4)
            .filter(|chunk| {
                u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) == 0xD63F0200
            })
            .count();
        assert!(call_count >= 1, "call step should emit helper call");
    }

    #[test]
    fn call_step_executes_host_function() {
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
    }

    #[test]
    fn jump_to_root_step_returns_trace_exit() {
        let trace = build_single_step_trace(TraceStep::JumpToRoot);
        let code = emit_native_trace_bytes(&trace).expect("jump trace should compile");
        let mut found_cbnz = false;
        for chunk in code.chunks_exact(4) {
            let insn = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            if (insn & 0x7F00_001F) == 0x3500_0000 {
                found_cbnz = true;
                let imm19 = ((insn >> 5) & 0x7FFFF) as i32;
                assert_ne!(
                    imm19, 0,
                    "status check branch must not be self-looping, code={:02X?}",
                    code
                );
            }
        }
        assert!(found_cbnz, "expected a status-check cbnz instruction");
    }

    #[test]
    fn emitter_supports_all_trace_steps() {
        let trace = JitTrace {
            id: 0,
            root_ip: 0,
            start_line: None,
            has_call: true,
            has_yielding_call: true,
            steps: vec![
                TraceStep::Nop,
                TraceStep::Ldc(0),
                TraceStep::Add,
                TraceStep::Sub,
                TraceStep::Mul,
                TraceStep::Div,
                TraceStep::Mod,
                TraceStep::Shl,
                TraceStep::Shr,
                TraceStep::And,
                TraceStep::Or,
                TraceStep::Neg,
                TraceStep::Ceq,
                TraceStep::Clt,
                TraceStep::Cgt,
                TraceStep::Pop,
                TraceStep::Dup,
                TraceStep::Ldloc(0),
                TraceStep::Stloc(0),
                TraceStep::Call {
                    index: 0,
                    argc: 0,
                    call_ip: 0,
                },
                TraceStep::GuardFalse { exit_ip: 0 },
                TraceStep::JumpToIp { target_ip: 0 },
                TraceStep::JumpToRoot,
                TraceStep::Ret,
            ],
            terminal: JitTraceTerminal::LoopBack,
            executions: 0,
        };

        let code = emit_native_trace_bytes(&trace).expect("all steps should emit");
        assert!(!code.is_empty());
    }

    #[test]
    fn guard_false_short_circuits_before_jump_to_root() {
        let trace = JitTrace {
            id: 0,
            root_ip: 0,
            start_line: None,
            has_call: false,
            has_yielding_call: false,
            steps: vec![
                TraceStep::Ldc(0),
                TraceStep::GuardFalse { exit_ip: 7 },
                TraceStep::JumpToRoot,
            ],
            terminal: JitTraceTerminal::LoopBack,
            executions: 0,
        };
        let mut vm = Vm::new(Program::new(vec![Value::Bool(false)], vec![0; 8]));
        vm.stack = Vec::with_capacity(4);
        let status = execute_trace(&mut vm, trace).expect("trace should execute");
        assert_eq!(status, STATUS_TRACE_EXIT);
        assert_eq!(vm.ip(), 7, "guard false must exit before jump-to-root");
    }

    #[test]
    fn loop_trace_updates_locals_and_exits() {
        let trace = JitTrace {
            id: 0,
            root_ip: 0,
            start_line: None,
            has_call: false,
            has_yielding_call: false,
            steps: vec![
                TraceStep::Ldloc(0),
                TraceStep::Ldc(0),
                TraceStep::Clt,
                TraceStep::GuardFalse { exit_ip: 99 },
                TraceStep::Ldloc(1),
                TraceStep::Ldloc(0),
                TraceStep::Add,
                TraceStep::Stloc(1),
                TraceStep::Ldloc(0),
                TraceStep::Ldc(1),
                TraceStep::Add,
                TraceStep::Stloc(0),
                TraceStep::JumpToRoot,
            ],
            terminal: JitTraceTerminal::LoopBack,
            executions: 0,
        };

        let mut vm = Vm::with_locals(
            Program::new(vec![Value::Int(200), Value::Int(1)], vec![0; 128]),
            2,
        );
        vm.stack = Vec::with_capacity(16);

        let (status, memory) = execute_trace_with_entry(&mut vm, trace.clone()).expect("compile");
        assert_eq!(status, STATUS_TRACE_EXIT);
        let entry = unsafe { std::mem::transmute::<*mut u8, NativeEntry>(memory.ptr) };

        let mut exited = false;
        for _ in 0..400 {
            clear_bridge_error();
            let status = unsafe { entry(&mut vm as *mut Vm) };
            if status != STATUS_TRACE_EXIT {
                panic!("expected trace-exit status, got {status}");
            }
            if vm.ip() == 99 {
                exited = true;
                break;
            }
        }

        assert!(
            exited,
            "trace did not reach guard exit ip (ip={}, locals={:?}, stack_len={})",
            vm.ip(),
            vm.locals,
            vm.stack.len()
        );
        assert_eq!(vm.locals[0], Value::Int(200));
        assert_eq!(vm.locals[1], Value::Int(19_900));
    }
}
