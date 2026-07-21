use super::compile::{CompiledProgram, compile_program};
use crate::vm::host::VmHostFunction;
use crate::vm::native::{
    STATUS_CONTINUE, STATUS_ERROR, STATUS_HALTED, STATUS_LINKED_CONTINUE, STATUS_OUT_OF_FUEL,
    STATUS_TRACE_EXIT, STATUS_WAITING, STATUS_YIELDED, clear_bridge_error,
    selected_codegen_backend, take_bridge_error,
};
use crate::vm::{ExecOutcome, Vm, VmError, VmResult};

impl Vm {
    pub fn compile_aot(&mut self) -> VmResult<()> {
        self.ensure_call_bindings()?;
        let non_yielding_host_imports = self
            .resolved_calls
            .iter()
            .map(|&slot| {
                matches!(
                    self.host_functions.get(usize::from(slot)),
                    Some(VmHostFunction::ArgsStaticNonYielding(_))
                )
            })
            .collect::<Vec<_>>();
        self.aot_program = Some(compile_program(
            self.program(),
            non_yielding_host_imports.as_slice(),
        )?);
        self.aot_exec_count = 0;
        Ok(())
    }

    pub fn clear_aot(&mut self) {
        self.aot_program = None;
        self.aot_exec_count = 0;
    }

    pub fn has_aot_program(&self) -> bool {
        self.aot_program.is_some()
    }

    pub fn aot_exec_count(&self) -> u64 {
        self.aot_exec_count
    }

    pub fn aot_resume_ips(&self) -> Option<&[usize]> {
        self.aot_program
            .as_ref()
            .map(|program| program.resume_ips.as_ref())
    }

    pub fn dump_aot_info(&self) -> String {
        let Some(program) = self.aot_program.as_ref() else {
            return "whole-program aot: disabled\n".to_string();
        };

        let mut out = String::new();
        out.push_str("whole-program aot: enabled\n");
        out.push_str(&format!(
            "  native codegen backend: {}\n",
            selected_codegen_backend()
        ));
        out.push_str(&format!("  aot executions: {}\n", self.aot_exec_count));
        out.push_str(&format!("  code_bytes={}\n", program.code.len()));
        out.push_str("  lowering=ssa\n");
        out.push_str(&format!("  resume ips: {}\n", format_resume_ips(program)));
        out
    }

    pub(crate) fn execute_aot_entry(&mut self) -> VmResult<ExecOutcome> {
        let Some(entry) = self.aot_program.as_ref().map(|program| program.entry) else {
            return Ok(ExecOutcome::Continue);
        };

        clear_bridge_error();
        unsafe { crate::vm::native::prepare_for_execution() };
        let status = unsafe { entry(self as *mut Vm) };
        self.aot_exec_count = self.aot_exec_count.saturating_add(1);

        match status {
            STATUS_CONTINUE | STATUS_LINKED_CONTINUE => Ok(ExecOutcome::Continue),
            STATUS_HALTED => Ok(ExecOutcome::Halted),
            STATUS_YIELDED => {
                self.last_yield_reason = Some(super::super::VmYieldReason::Host);
                Ok(ExecOutcome::Yielded)
            }
            STATUS_WAITING => {
                let op_id = self.waiting_host_op.map(|op| op.op_id).ok_or_else(|| {
                    VmError::JitNative(
                        "aot call bridge reported waiting without a pending op".to_string(),
                    )
                })?;
                Ok(ExecOutcome::Waiting(op_id))
            }
            STATUS_OUT_OF_FUEL => match self.interrupt_mode {
                super::super::InterruptMode::Fuel => Err(VmError::OutOfFuel {
                    needed: 1,
                    remaining: self.fuel_remaining,
                }),
                super::super::InterruptMode::Epoch => Err(VmError::EpochDeadlineReached {
                    current: self.current_epoch(),
                    deadline: self.epoch_deadline,
                }),
                super::super::InterruptMode::None => Err(VmError::JitNative(
                    "aot interruption checkpoint fired while interruption was disabled".to_string(),
                )),
            },
            STATUS_ERROR => {
                if let Some(err) = take_bridge_error() {
                    return Err(err);
                }
                if self.ip == self.program.code.len() {
                    return Err(VmError::BytecodeBounds);
                }
                Err(VmError::JitNative(format!(
                    "aot entry reported failure without VmError (ip={} stack_len={} aot={})",
                    self.ip,
                    self.stack.len(),
                    self.has_aot_program()
                )))
            }
            STATUS_TRACE_EXIT => {
                self.aot_interpreter_boundary_hit = true;
                Ok(ExecOutcome::Continue)
            }
            other => Err(VmError::JitNative(format!(
                "unexpected aot return status {other}"
            ))),
        }
    }
}

fn format_resume_ips(program: &CompiledProgram) -> String {
    program
        .resume_ips
        .iter()
        .map(|ip| ip.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}
