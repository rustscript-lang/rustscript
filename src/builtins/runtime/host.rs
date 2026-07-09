use std::time::Duration;

use super::AnyValue;
use super::print::format_value;
use crate::vm::{CallOutcome, Value, Vm, VmError, VmResult};
use pd_host_function::pd_host_function;

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const PRINT_NAME: &str = "print";
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const PRINTLN_NAME: &str = "println";
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const RUNTIME_SLEEP_NAME: &str = "runtime::sleep";
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const RUNTIME_EXIT_NAME: &str = "runtime::exit";

fn render_print_value(value: &Value, newline: bool) -> String {
    let mut rendered = format_value(value);
    if newline {
        rendered.push('\n');
    }
    rendered
}

/// Writes a value to the runtime print sink.
#[pd_host_function(name = "print")]
fn runtime_print_impl(vm: &mut Vm, value: &AnyValue) -> VmResult<AnyValue> {
    vm.write_runtime_print(render_print_value(value, false))?;
    Ok(value.clone())
}

/// Writes a value to the runtime print sink and appends a newline.
#[pd_host_function(name = "println")]
fn runtime_println_impl(vm: &mut Vm, value: &AnyValue) -> VmResult<AnyValue> {
    vm.write_runtime_print(render_print_value(value, true))?;
    Ok(value.clone())
}

fn sleep_duration(millis: i64) -> VmResult<Duration> {
    if millis < 0 {
        return Err(VmError::HostError(format!(
            "runtime::sleep expects non-negative milliseconds, got {millis}",
        )));
    }
    Ok(Duration::from_millis(millis as u64))
}

/// Sleeps for the requested milliseconds.
#[pd_host_function(name = "runtime::sleep")]
fn runtime_sleep_impl(_vm: &mut Vm, ms: i64) -> VmResult<bool> {
    let duration = sleep_duration(ms)?;
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::sleep(duration);
    #[cfg(target_arch = "wasm32")]
    let _ = duration;
    Ok(true)
}

/// Halts the current VM invocation immediately.
#[pd_host_function(name = "runtime::exit")]
fn runtime_exit_impl(_vm: &mut Vm) -> VmResult<CallOutcome> {
    Ok(CallOutcome::Halt)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::assembler::BytecodeBuilder;
    use crate::bytecode::{HostImport, Program};
    use crate::vm::{CallOutcome, HostFunctionRegistry, Value, Vm, VmStatus};

    use super::{
        PRINT_NAME, PRINTLN_NAME, RUNTIME_EXIT_NAME, RUNTIME_SLEEP_NAME, runtime_exit_impl,
        runtime_sleep_impl,
    };

    fn host_call_program(name: &str) -> Program {
        let mut bc = BytecodeBuilder::new();
        bc.ldc(0);
        bc.call(0, 1);
        bc.ret();
        Program::with_imports_and_debug(
            vec![Value::string("line")],
            bc.finish(),
            vec![HostImport {
                name: name.to_string(),
                arity: 1,
                return_type: crate::bytecode::ValueType::Bool,
            }],
            None,
        )
    }

    #[test]
    fn runtime_sleep_rejects_negative_milliseconds() {
        let mut vm = Vm::new(Program::new(
            Vec::new(),
            vec![crate::bytecode::OpCode::Ret as u8],
        ));
        let err = runtime_sleep_impl(&mut vm, -1).expect_err("negative sleep should fail");
        assert!(
            err.to_string()
                .contains("runtime::sleep expects non-negative milliseconds"),
            "{err}"
        );
    }

    #[test]
    fn runtime_sleep_name_is_stable() {
        assert_eq!(RUNTIME_SLEEP_NAME, "runtime::sleep");
    }

    #[test]
    fn runtime_exit_name_is_stable() {
        assert_eq!(RUNTIME_EXIT_NAME, "runtime::exit");
    }

    #[test]
    fn runtime_exit_returns_halt_outcome() {
        let mut vm = Vm::new(Program::new(
            Vec::new(),
            vec![crate::bytecode::OpCode::Ret as u8],
        ));
        assert_eq!(
            runtime_exit_impl(&mut vm).expect("runtime::exit should halt"),
            CallOutcome::Halt
        );
    }

    #[test]
    fn default_print_binding_uses_vm_runtime_sink() {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink_lines = Arc::clone(&lines);
        let mut vm = Vm::new(host_call_program(PRINT_NAME));
        vm.set_runtime_print_sink(move |rendered| {
            sink_lines
                .lock()
                .expect("sink should be lockable")
                .push(rendered);
        });

        let status = vm.run().expect("vm should run");
        assert_eq!(status, VmStatus::Halted);
        assert_eq!(
            lines.lock().expect("sink should be lockable").as_slice(),
            ["line"]
        );
    }

    #[test]
    fn host_function_registry_includes_default_print_binding() {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink_lines = Arc::clone(&lines);
        let mut vm = Vm::new(host_call_program(PRINT_NAME));
        vm.set_runtime_print_sink(move |rendered| {
            sink_lines
                .lock()
                .expect("sink should be lockable")
                .push(rendered);
        });
        let registry = HostFunctionRegistry::new();
        registry
            .bind_vm_cached(&mut vm)
            .expect("registry should bind print");

        let status = vm.run().expect("vm should run");
        assert_eq!(status, VmStatus::Halted);
        assert_eq!(
            lines.lock().expect("sink should be lockable").as_slice(),
            ["line"]
        );
    }

    #[test]
    fn default_println_binding_appends_newline_before_sink() {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink_lines = Arc::clone(&lines);
        let mut vm = Vm::new(host_call_program(PRINTLN_NAME));
        vm.set_runtime_print_sink(move |rendered| {
            sink_lines
                .lock()
                .expect("sink should be lockable")
                .push(rendered);
        });

        let status = vm.run().expect("vm should run");
        assert_eq!(status, VmStatus::Halted);
        assert_eq!(
            lines.lock().expect("sink should be lockable").as_slice(),
            ["line\n"]
        );
    }
}
