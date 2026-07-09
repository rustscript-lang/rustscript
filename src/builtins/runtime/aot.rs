use pd_host_function::pd_host_function;

use crate::vm::{Value, Vm, VmError, VmResult};

/// Compile and install a whole-program AOT artifact for the current VM program.
#[pd_host_function(name = "aot::compile")]
pub(super) fn builtin_aot_compile(vm: &mut Vm) -> VmResult<bool> {
    vm.compile_aot()?;
    Ok(true)
}

/// Clear the currently installed whole-program AOT artifact.
#[pd_host_function(name = "aot::clear")]
pub(super) fn builtin_aot_clear(vm: &mut Vm) -> VmResult<bool> {
    vm.clear_aot();
    Ok(false)
}

/// Return whether a whole-program AOT artifact is currently installed.
#[pd_host_function(name = "aot::is_compiled")]
pub(super) fn builtin_aot_is_compiled(vm: &mut Vm) -> VmResult<bool> {
    Ok(vm.has_aot_program())
}

/// Return the number of times the current whole-program AOT artifact has executed.
#[pd_host_function(name = "aot::exec_count")]
pub(super) fn builtin_aot_exec_count(vm: &mut Vm) -> VmResult<i64> {
    i64::try_from(vm.aot_exec_count())
        .map_err(|_| VmError::HostError("aot execution count overflow".to_string()))
}

/// Return the resumable bytecode instruction pointers for the current AOT artifact.
#[pd_host_function(name = "aot::resume_ips")]
pub(super) fn builtin_aot_resume_ips(vm: &mut Vm) -> VmResult<Vec<Value>> {
    Ok(vm
        .aot_resume_ips()
        .unwrap_or(&[])
        .iter()
        .map(|ip| Value::Int(i64::try_from(*ip).unwrap_or(i64::MAX)))
        .collect())
}

/// Return a human-readable summary of the current whole-program AOT state.
#[pd_host_function(name = "aot::dump")]
pub(super) fn builtin_aot_dump(vm: &mut Vm) -> VmResult<String> {
    Ok(vm.dump_aot_info())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BytecodeBuilder, Program, Value};

    fn native_aot_supported() -> bool {
        (cfg!(target_arch = "x86_64")
            && (cfg!(target_os = "windows") || (cfg!(unix) && !cfg!(target_os = "macos"))))
            || (cfg!(target_arch = "aarch64")
                && (cfg!(target_os = "linux") || cfg!(target_os = "macos")))
    }

    #[test]
    fn builtin_aot_compile_and_clear_manage_program_state() {
        if !native_aot_supported() {
            return;
        }

        let mut bc = BytecodeBuilder::new();
        bc.ldc(0);
        bc.ret();
        let mut vm = Vm::new(Program::new(vec![Value::Int(7)], bc.finish()));
        let args: &[Value] = &[];

        assert!(
            !builtin_aot_is_compiled(&mut vm, args).expect("query should succeed"),
            "fresh vm should not have aot installed"
        );

        assert!(
            builtin_aot_compile(&mut vm, args).expect("compile should succeed"),
            "compile should report success"
        );
        assert!(vm.has_aot_program(), "aot program should be installed");
        assert_eq!(
            builtin_aot_resume_ips(&mut vm, args).expect("resume query should succeed"),
            vec![Value::Int(0)]
        );

        assert!(
            !builtin_aot_clear(&mut vm, args).expect("clear should succeed"),
            "clear should report the disabled state"
        );
        assert!(!vm.has_aot_program(), "aot program should be cleared");
    }

    #[test]
    fn builtin_aot_dump_reports_disabled_and_exec_count_defaults() {
        let mut vm = Vm::new(Program::new(Vec::new(), vec![crate::OpCode::Ret as u8]));
        let args: &[Value] = &[];

        assert_eq!(
            builtin_aot_exec_count(&mut vm, args).expect("exec count should succeed"),
            0
        );
        assert_eq!(
            builtin_aot_resume_ips(&mut vm, args).expect("resume query should succeed"),
            Vec::<Value>::new()
        );
        assert!(
            builtin_aot_dump(&mut vm, args)
                .expect("dump should succeed")
                .contains("whole-program aot: disabled"),
            "dump should describe the disabled state"
        );
    }
}
