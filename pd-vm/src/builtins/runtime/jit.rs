use pd_host_function::pd_host_function;

use super::VmMap;
use crate::vm::{Value, Vm, VmResult};

fn config_as_map(vm: &Vm) -> VmMap {
    let config = vm.jit_config();
    let max_trace_len = i64::try_from(config.max_trace_len).unwrap_or(i64::MAX);
    VmMap::from_entries(vec![
        (Value::string("enabled"), Value::Bool(config.enabled)),
        (
            Value::string("hot_loop_threshold"),
            Value::Int(i64::from(config.hot_loop_threshold)),
        ),
        (Value::string("max_trace_len"), Value::Int(max_trace_len)),
    ])
}

#[pd_host_function(name = "jit::set_config")]
pub(super) fn builtin_jit_set_config(
    vm: &mut Vm,
    enabled: bool,
    hot_loop_threshold: u32,
    max_trace_len: usize,
) -> VmResult<VmMap> {
    let mut config = *vm.jit_config();
    config.enabled = enabled;
    config.hot_loop_threshold = hot_loop_threshold;
    config.max_trace_len = max_trace_len;
    vm.set_jit_config(config);
    Ok(config_as_map(vm))
}

#[pd_host_function(name = "jit::get_config")]
pub(super) fn builtin_jit_get_config(vm: &mut Vm) -> VmResult<VmMap> {
    Ok(config_as_map(vm))
}

#[pd_host_function(name = "jit::set_enabled")]
pub(super) fn builtin_jit_set_enabled(vm: &mut Vm, enabled: bool) -> VmResult<bool> {
    let mut config = *vm.jit_config();
    config.enabled = enabled;
    vm.set_jit_config(config);
    Ok(enabled)
}

#[pd_host_function(name = "jit::get_enabled")]
pub(super) fn builtin_jit_get_enabled(vm: &mut Vm) -> VmResult<bool> {
    Ok(vm.jit_config().enabled)
}

#[pd_host_function(name = "jit::set_hot_loop_threshold")]
pub(super) fn builtin_jit_set_hot_loop_threshold(
    vm: &mut Vm,
    hot_loop_threshold: u32,
) -> VmResult<u32> {
    let mut config = *vm.jit_config();
    config.hot_loop_threshold = hot_loop_threshold;
    vm.set_jit_config(config);
    Ok(hot_loop_threshold)
}

#[pd_host_function(name = "jit::get_hot_loop_threshold")]
pub(super) fn builtin_jit_get_hot_loop_threshold(vm: &mut Vm) -> VmResult<u32> {
    Ok(vm.jit_config().hot_loop_threshold)
}

#[pd_host_function(name = "jit::set_max_trace_len")]
pub(super) fn builtin_jit_set_max_trace_len(vm: &mut Vm, max_trace_len: usize) -> VmResult<usize> {
    let mut config = *vm.jit_config();
    config.max_trace_len = max_trace_len;
    vm.set_jit_config(config);
    Ok(max_trace_len)
}

#[pd_host_function(name = "jit::get_max_trace_len")]
pub(super) fn builtin_jit_get_max_trace_len(vm: &mut Vm) -> VmResult<usize> {
    Ok(vm.jit_config().max_trace_len)
}
