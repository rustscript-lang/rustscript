use std::time::Duration;

use super::super::{CallOutcome, HostFunctionRegistry, Value, Vm, VmError, VmResult};

pub(crate) const RUNTIME_SLEEP_NAME: &str = "runtime::sleep";

pub(crate) fn register_default_host_functions(registry: &mut HostFunctionRegistry) {
    registry.register_static(RUNTIME_SLEEP_NAME, 1, runtime_sleep);
}

pub(crate) fn bind_default_host_function(vm: &mut Vm, name: &str) -> bool {
    match name {
        RUNTIME_SLEEP_NAME => {
            vm.bind_static_function(RUNTIME_SLEEP_NAME, runtime_sleep);
            true
        }
        _ => false,
    }
}

fn sleep_duration(args: &[Value]) -> VmResult<Duration> {
    let millis = match args.first() {
        Some(Value::Int(value)) => *value,
        Some(_) => return Err(VmError::TypeMismatch("int")),
        None => {
            return Err(VmError::HostError(
                "missing argument: runtime::sleep milliseconds".to_string(),
            ));
        }
    };
    if millis < 0 {
        return Err(VmError::HostError(format!(
            "runtime::sleep expects non-negative milliseconds, got {millis}",
        )));
    }
    Ok(Duration::from_millis(millis as u64))
}

fn runtime_sleep(_vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    let duration = sleep_duration(args)?;
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::sleep(duration);
    #[cfg(target_arch = "wasm32")]
    let _ = duration;
    Ok(CallOutcome::Return(vec![Value::Bool(true)]))
}

#[cfg(test)]
mod tests {
    use crate::bytecode::Program;
    use crate::vm::{Value, Vm};

    use super::{RUNTIME_SLEEP_NAME, runtime_sleep};

    #[test]
    fn runtime_sleep_rejects_negative_milliseconds() {
        let mut vm = Vm::new(Program::new(
            Vec::new(),
            vec![crate::bytecode::OpCode::Ret as u8],
        ));
        let err =
            runtime_sleep(&mut vm, &[Value::Int(-1)]).expect_err("negative sleep should fail");
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
}
