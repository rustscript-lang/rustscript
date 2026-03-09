use super::super::{Value, Vm, VmError, VmResult};
use super::BuiltinCallOutcome;

fn config_as_value(vm: &Vm) -> Value {
    let config = vm.jit_config();
    let max_trace_len = i64::try_from(config.max_trace_len).unwrap_or(i64::MAX);
    Value::map(vec![
        (Value::string("enabled"), Value::Bool(config.enabled)),
        (
            Value::string("hot_loop_threshold"),
            Value::Int(i64::from(config.hot_loop_threshold)),
        ),
        (Value::string("max_trace_len"), Value::Int(max_trace_len)),
    ])
}

fn arg_bool(args: &[Value], index: usize, label: &str) -> VmResult<bool> {
    match args.get(index) {
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(VmError::TypeMismatch("bool")),
        None => Err(VmError::HostError(format!("missing argument: {label}"))),
    }
}

fn arg_non_negative_u32(args: &[Value], index: usize, label: &str) -> VmResult<u32> {
    let raw = match args.get(index) {
        Some(Value::Int(value)) => *value,
        Some(_) => return Err(VmError::TypeMismatch("int")),
        None => return Err(VmError::HostError(format!("missing argument: {label}"))),
    };
    if raw < 0 {
        return Err(VmError::HostError(format!("{label} must be non-negative",)));
    }
    u32::try_from(raw).map_err(|_| VmError::HostError(format!("{label} overflow")))
}

fn arg_non_negative_usize(args: &[Value], index: usize, label: &str) -> VmResult<usize> {
    let raw = match args.get(index) {
        Some(Value::Int(value)) => *value,
        Some(_) => return Err(VmError::TypeMismatch("int")),
        None => return Err(VmError::HostError(format!("missing argument: {label}"))),
    };
    if raw < 0 {
        return Err(VmError::HostError(format!("{label} must be non-negative",)));
    }
    usize::try_from(raw).map_err(|_| VmError::HostError(format!("{label} overflow")))
}

pub(super) fn builtin_jit_set_config(
    vm: &mut Vm,
    args: Vec<Value>,
) -> VmResult<BuiltinCallOutcome> {
    let enabled = arg_bool(&args, 0, "jit.set_config enabled")?;
    let hot_loop_threshold = arg_non_negative_u32(&args, 1, "jit.set_config hot_loop_threshold")?;
    let max_trace_len = arg_non_negative_usize(&args, 2, "jit.set_config max_trace_len")?;

    let mut config = vm.jit_config().clone();
    config.enabled = enabled;
    config.hot_loop_threshold = hot_loop_threshold;
    config.max_trace_len = max_trace_len;
    vm.set_jit_config(config);
    Ok(BuiltinCallOutcome::Return(vec![config_as_value(vm)]))
}

pub(super) fn builtin_jit_get_config(vm: &mut Vm) -> VmResult<BuiltinCallOutcome> {
    Ok(BuiltinCallOutcome::Return(vec![config_as_value(vm)]))
}

pub(super) fn builtin_jit_set_enabled(
    vm: &mut Vm,
    args: Vec<Value>,
) -> VmResult<BuiltinCallOutcome> {
    let enabled = arg_bool(&args, 0, "jit.set_enabled value")?;
    let mut config = vm.jit_config().clone();
    config.enabled = enabled;
    vm.set_jit_config(config);
    Ok(BuiltinCallOutcome::Return(vec![Value::Bool(enabled)]))
}

pub(super) fn builtin_jit_get_enabled(vm: &mut Vm) -> VmResult<BuiltinCallOutcome> {
    Ok(BuiltinCallOutcome::Return(vec![Value::Bool(
        vm.jit_config().enabled,
    )]))
}

pub(super) fn builtin_jit_set_hot_loop_threshold(
    vm: &mut Vm,
    args: Vec<Value>,
) -> VmResult<BuiltinCallOutcome> {
    let hot_loop_threshold = arg_non_negative_u32(&args, 0, "jit.set_hot_loop_threshold value")?;
    let mut config = vm.jit_config().clone();
    config.hot_loop_threshold = hot_loop_threshold;
    vm.set_jit_config(config);
    Ok(BuiltinCallOutcome::Return(vec![Value::Int(i64::from(
        hot_loop_threshold,
    ))]))
}

pub(super) fn builtin_jit_get_hot_loop_threshold(vm: &mut Vm) -> VmResult<BuiltinCallOutcome> {
    Ok(BuiltinCallOutcome::Return(vec![Value::Int(i64::from(
        vm.jit_config().hot_loop_threshold,
    ))]))
}

pub(super) fn builtin_jit_set_max_trace_len(
    vm: &mut Vm,
    args: Vec<Value>,
) -> VmResult<BuiltinCallOutcome> {
    let max_trace_len = arg_non_negative_usize(&args, 0, "jit.set_max_trace_len value")?;
    let mut config = vm.jit_config().clone();
    config.max_trace_len = max_trace_len;
    vm.set_jit_config(config);
    Ok(BuiltinCallOutcome::Return(vec![Value::Int(
        i64::try_from(max_trace_len).unwrap_or(i64::MAX),
    )]))
}

pub(super) fn builtin_jit_get_max_trace_len(vm: &mut Vm) -> VmResult<BuiltinCallOutcome> {
    Ok(BuiltinCallOutcome::Return(vec![Value::Int(
        i64::try_from(vm.jit_config().max_trace_len).unwrap_or(i64::MAX),
    )]))
}
