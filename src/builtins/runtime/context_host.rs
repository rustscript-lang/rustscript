use pd_host_function::pd_host_function;

use super::AnyValue;
use crate::vm::{Value, Vm, VmResult};

/// Returns the embedding-provided input for the current run.
#[pd_host_function(name = "runtime::input")]
fn runtime_input_impl(vm: &mut Vm) -> VmResult<AnyValue> {
    vm.runtime_input_value()
}

/// Returns the run-scoped input encoded with the runtime's strict JSON contract.
#[pd_host_function(name = "runtime::input_json")]
fn runtime_input_json_impl(vm: &mut Vm) -> VmResult<String> {
    let value = vm.runtime_input_value()?;
    super::json::encode_value_to_string(&value)
}

/// Emits one bounded event without changing the script return value.
#[pd_host_function(name = "runtime::emit")]
fn runtime_emit_impl(vm: &mut Vm, value: AnyValue) -> VmResult<()> {
    vm.emit_runtime_event(value)
}

/// Emits one JSON text event for strict RSS boundary adapters.
#[pd_host_function(name = "runtime::emit_json")]
fn runtime_emit_json_impl(vm: &mut Vm, value: &str) -> VmResult<()> {
    vm.emit_runtime_event(Value::string(value))
}
