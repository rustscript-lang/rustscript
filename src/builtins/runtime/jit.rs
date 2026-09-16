use std::sync::{Arc, OnceLock};

use pd_host_function::pd_host_function;

use super::typed::VmMapHandle;
use super::{CallOutcome, FromVmValue, VmMap, return_one};
use crate::host_api::{
    HostApiCatalog, HostFunctionSchema, HostParamSchema, HostStructField, HostStructSchema,
    HostTypeSchema,
};
use crate::vm::{HostFunctionRegistry, Value, Vm, VmError, VmResult};

const GET_CONFIG: &str = "jit::get_config";
const SET_CONFIG: &str = "jit::set_config";

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

fn map_field<'a, T: FromVmValue<'a>>(map: &'a VmMap, key: &str) -> VmResult<T> {
    let value = map
        .get(&Value::string(key))
        .ok_or_else(|| VmError::HostError(format!("missing JIT config field '{key}'")))?;
    T::from_vm_value(value, key)
}

fn apply_jit_config(
    vm: &mut Vm,
    enabled: bool,
    hot_loop_threshold: u32,
    max_trace_len: usize,
) -> VmMap {
    let mut config = *vm.jit_config();
    config.enabled = enabled;
    config.hot_loop_threshold = hot_loop_threshold;
    config.max_trace_len = max_trace_len;
    vm.set_jit_config(config);
    config_as_map(vm)
}

/// Sets the JIT runtime configuration from positional `enabled`,
/// `hot_loop_threshold`, and `max_trace_len` arguments.
#[pd_host_function(name = "jit::set_config", contract = jit_set_config_positional_contract)]
pub(super) fn builtin_jit_set_config(
    vm: &mut Vm,
    enabled: bool,
    hot_loop_threshold: u32,
    max_trace_len: usize,
) -> VmResult<VmMap> {
    Ok(apply_jit_config(
        vm,
        enabled,
        hot_loop_threshold,
        max_trace_len,
    ))
}

fn set_config_from_map(vm: &mut Vm, config: &VmMap) -> VmResult<VmMap> {
    let enabled = map_field(config, "enabled")?;
    let hot_loop_threshold = map_field(config, "hot_loop_threshold")?;
    let max_trace_len = map_field(config, "max_trace_len")?;
    Ok(apply_jit_config(
        vm,
        enabled,
        hot_loop_threshold,
        max_trace_len,
    ))
}

/// Returns the current JIT runtime configuration as a typed `JitConfig`.
#[pd_host_function(name = "jit::get_config", contract = jit_get_config_contract)]
pub(super) fn builtin_jit_get_config(vm: &mut Vm) -> VmResult<VmMap> {
    Ok(config_as_map(vm))
}

/// Enables or disables the JIT at runtime.
#[pd_host_function(name = "jit::set_enabled")]
pub(super) fn builtin_jit_set_enabled(vm: &mut Vm, enabled: bool) -> VmResult<bool> {
    let mut config = *vm.jit_config();
    config.enabled = enabled;
    vm.set_jit_config(config);
    Ok(enabled)
}

/// Returns whether the JIT is enabled at runtime.
#[pd_host_function(name = "jit::get_enabled")]
pub(super) fn builtin_jit_get_enabled(vm: &mut Vm) -> VmResult<bool> {
    Ok(vm.jit_config().enabled)
}

/// Sets the loop-hotness threshold used by the JIT.
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

/// Returns the loop-hotness threshold used by the JIT.
#[pd_host_function(name = "jit::get_hot_loop_threshold")]
pub(super) fn builtin_jit_get_hot_loop_threshold(vm: &mut Vm) -> VmResult<u32> {
    Ok(vm.jit_config().hot_loop_threshold)
}

/// Sets the maximum trace length used by the JIT.
#[pd_host_function(name = "jit::set_max_trace_len")]
pub(super) fn builtin_jit_set_max_trace_len(vm: &mut Vm, max_trace_len: usize) -> VmResult<usize> {
    let mut config = *vm.jit_config();
    config.max_trace_len = max_trace_len;
    vm.set_jit_config(config);
    Ok(max_trace_len)
}

/// Returns the maximum trace length used by the JIT.
#[pd_host_function(name = "jit::get_max_trace_len")]
pub(super) fn builtin_jit_get_max_trace_len(vm: &mut Vm) -> VmResult<usize> {
    Ok(vm.jit_config().max_trace_len)
}

fn jit_config_struct() -> HostStructSchema {
    HostStructSchema::new(
        "JitConfig",
        vec![
            HostStructField::new("enabled", HostTypeSchema::Bool),
            HostStructField::new("hot_loop_threshold", HostTypeSchema::Int),
            HostStructField::new("max_trace_len", HostTypeSchema::Int),
        ],
    )
    .with_description("JIT runtime configuration.")
}

/// Guest contract for `jit::set_config` positional form.
fn jit_set_config_positional_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        SET_CONFIG,
        vec![
            HostParamSchema::value("enabled", HostTypeSchema::Bool),
            HostParamSchema::value("hot_loop_threshold", HostTypeSchema::Int),
            HostParamSchema::value("max_trace_len", HostTypeSchema::Int),
        ],
        jit_config_struct().as_type(),
    )
    .with_description(
        "Sets the JIT runtime configuration from enabled, hot_loop_threshold, and max_trace_len.",
    )
}

/// Guest contract for `jit::get_config`.
fn jit_get_config_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(GET_CONFIG, vec![], jit_config_struct().as_type())
        .with_description("Returns the current JIT runtime configuration.")
}

/// The `jit::set_config` overload that accepts a `JitConfig` value.
///
/// The positional overload is generated from
/// `#[pd_host_function(name = "jit::set_config", contract = ...)]`; this named
/// overload has no Rust mirror to infer from, so it is declared as an explicit
/// descriptor next to the module's other descriptors. It still carries its own
/// schema, adapter, and binding class, so there is no parallel catalog entry.
fn jit_set_config_named_descriptor() -> crate::host_extension::HostFunctionDescriptor {
    use crate::host_extension::{
        HostAdapterDescriptor, HostBindingDescriptor, HostBindingKind, HostFunctionDescriptor,
    };

    HostFunctionDescriptor {
        schema: HostFunctionSchema::with_return(
            SET_CONFIG,
            vec![HostParamSchema::value(
                "config",
                jit_config_struct().as_type(),
            )],
            jit_config_struct().as_type(),
        )
        .with_description("Sets the JIT runtime configuration from a JitConfig value."),
        binding: HostBindingDescriptor {
            kind: HostBindingKind::StaticStack,
        },
        effects: Vec::new(),
        adapter: HostAdapterDescriptor::StaticStack(set_config_named_adapter),
        resource_types: Vec::new(),
    }
}

/// Documentation for the named structs this module's catalog declares.
const JIT_NAMED_STRUCTS: &[(&str, &str)] = &[("JitConfig", "JIT runtime configuration.")];

/// The JIT host catalog surface: `jit::get_config` returns named `JitConfig`;
/// `jit::set_config` accepts that struct or the positional `(bool, int, int)`
/// overload.
///
/// Runtime values remain maps. Other `jit::*` members stay namespaced builtins
/// and are owned by this module without a catalog entry.
const JIT_CATALOG_FUNCTIONS: &[fn() -> crate::host_extension::HostFunctionDescriptor] = &[
    builtin_jit_get_config_descriptor,
    jit_set_config_named_descriptor,
    builtin_jit_set_config_descriptor,
];

static JIT_HOST_CATALOG: OnceLock<Arc<HostApiCatalog>> = OnceLock::new();

fn set_config_named_adapter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    let config = args
        .first()
        .ok_or_else(|| VmError::HostError("missing argument: config".to_string()))?;
    let handle = VmMapHandle::from_vm_value(config, "config")?;
    let map = set_config_from_map(vm, handle.as_ref())?;
    Ok(CallOutcome::Return(return_one(map)))
}

/// JIT host catalog derived from the module descriptors.
pub fn jit_host_catalog() -> Arc<HostApiCatalog> {
    Arc::clone(JIT_HOST_CATALOG.get_or_init(|| {
        super::host_modules::module_catalog("jit", JIT_CATALOG_FUNCTIONS, &[], JIT_NAMED_STRUCTS)
    }))
}

/// The standard `jit` host module: the catalog surface plus every owned
/// function.
pub(super) fn jit_host_module() -> super::host_modules::StandardHostModule {
    use super::host_modules::{StandardHostModule, catalog_module};

    const OWNED: &[fn() -> crate::host_extension::HostFunctionDescriptor] = &[
        builtin_jit_set_config_descriptor,
        builtin_jit_get_config_descriptor,
        builtin_jit_set_enabled_descriptor,
        builtin_jit_get_enabled_descriptor,
        builtin_jit_set_hot_loop_threshold_descriptor,
        builtin_jit_get_hot_loop_threshold_descriptor,
        builtin_jit_set_max_trace_len_descriptor,
        builtin_jit_get_max_trace_len_descriptor,
        jit_set_config_named_descriptor,
    ];

    StandardHostModule {
        name: "jit",
        catalog: || catalog_module("jit", JIT_CATALOG_FUNCTIONS, &[]),
        owned: OWNED,
        named_structs: JIT_NAMED_STRUCTS,
    }
}

/// Registers `jit::get_config` / `jit::set_config` from [`standard_host_catalog`].
pub fn register_jit_builtin_module(registry: &mut HostFunctionRegistry) -> VmResult<()> {
    let catalog = crate::builtins::runtime::standard_host_catalog();
    register_jit_builtin_module_from_catalog(registry, catalog.as_ref())
}

/// Registers JIT config functions using schemas from `catalog`.
///
/// `catalog` must declare the same `JitConfig` shape and `jit::get_config` /
/// `jit::set_config` overloads as [`jit_host_catalog`]; the registered adapters
/// are the module descriptors, so a compile/bind pair always agrees on
/// identity.
pub fn register_jit_builtin_module_from_catalog(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
) -> VmResult<()> {
    jit_host_module()
        .catalog_module()
        .expect("the JIT module publishes a catalog surface")
        .install_from_catalog(registry, catalog)
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OpCode, Program};

    #[test]
    fn named_set_config_is_atomic_on_missing_field() {
        let mut vm = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
            .expect("test VM construction must not fail");
        let original = *vm.jit_config();
        let map = VmMap::from_entries(vec![(Value::string("enabled"), Value::Bool(true))]);
        let err = set_config_from_map(&mut vm, &map).expect_err("missing fields must fail");
        assert!(
            err.to_string().contains("hot_loop_threshold")
                || err.to_string().contains("missing JIT config field"),
            "{err}"
        );
        assert_eq!(*vm.jit_config(), original);
    }
}
