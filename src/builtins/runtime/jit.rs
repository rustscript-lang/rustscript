use std::sync::{Arc, OnceLock};

use pd_host_function::pd_host_function;

use super::typed::VmMapHandle;
use super::{CallOutcome, FromVmValue, VmMap, return_one};
use crate::host_api::{
    HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamSchema, HostStructField,
    HostStructSchema, HostTypeSchema,
};
use crate::vm::{
    HostFunctionRegistry, Value, Vm, VmError, VmResult, catalog_named_struct_schemas,
    host_extension,
};

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

/// Sets the JIT runtime configuration from a `JitConfig` map.
#[pd_host_function(name = "jit::set_config")]
pub(super) fn builtin_jit_set_config(vm: &mut Vm, config: VmMapHandle) -> VmResult<VmMap> {
    let mut next = *vm.jit_config();
    next.enabled = map_field(config.as_ref(), "enabled")?;
    next.hot_loop_threshold = map_field(config.as_ref(), "hot_loop_threshold")?;
    next.max_trace_len = map_field(config.as_ref(), "max_trace_len")?;
    vm.set_jit_config(next);
    Ok(config_as_map(vm))
}

/// Returns the current JIT runtime configuration as a `JitConfig` map.
#[pd_host_function(name = "jit::get_config")]
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

fn build_jit_host_catalog() -> HostApiCatalog {
    let config = jit_config_struct();
    let config_ty = config.as_type();
    let mut builder = HostApiBuilder::new();
    builder.named_struct(config);
    builder.function(
        HostFunctionSchema::with_return(GET_CONFIG, vec![], config_ty.clone())
            .with_description("Returns the current JIT runtime configuration."),
    );
    builder.function(
        HostFunctionSchema::with_return(
            SET_CONFIG,
            vec![HostParamSchema::value("config", config_ty.clone())],
            config_ty,
        )
        .with_description("Sets the JIT runtime configuration."),
    );
    builder.build().expect("JIT host catalog must be valid")
}

static JIT_HOST_CATALOG: OnceLock<Arc<HostApiCatalog>> = OnceLock::new();

/// JIT-local host catalog: `jit::get_config` / `jit::set_config` use named `JitConfig`.
///
/// Runtime values remain maps. Other `jit::*` members stay namespaced builtins.
pub fn jit_host_catalog() -> Arc<HostApiCatalog> {
    Arc::clone(JIT_HOST_CATALOG.get_or_init(|| Arc::new(build_jit_host_catalog())))
}

struct JitAdapterContract {
    name: &'static str,
    arity: u8,
    adapter: fn(&mut Vm, &[Value]) -> VmResult<CallOutcome>,
}

const JIT_ADAPTER_CONTRACTS: &[JitAdapterContract] = &[
    JitAdapterContract {
        name: GET_CONFIG,
        arity: 0,
        adapter: get_config_adapter,
    },
    JitAdapterContract {
        name: SET_CONFIG,
        arity: 1,
        adapter: set_config_adapter,
    },
];

fn get_config_adapter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    let map = builtin_jit_get_config(vm, args)?;
    Ok(CallOutcome::Return(return_one(map)))
}

fn set_config_adapter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    let map = builtin_jit_set_config(vm, args)?;
    Ok(CallOutcome::Return(return_one(map)))
}

/// Registers `jit::get_config` / `jit::set_config` from [`jit_host_catalog`].
pub fn register_jit_builtin_module(registry: &mut HostFunctionRegistry) -> VmResult<()> {
    register_jit_builtin_module_from_catalog(registry, jit_host_catalog().as_ref())
}

/// Registers JIT config functions using schemas from `catalog`.
///
/// `catalog` must declare the same `JitConfig` shape as [`jit_host_catalog`];
/// registered fingerprints match the supplied catalog so exact compile/bind
/// pairs.
pub fn register_jit_builtin_module_from_catalog(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
) -> VmResult<()> {
    let contract = jit_host_catalog();
    let catalog_fingerprint = catalog.fingerprint();
    let contract_fingerprint = contract.fingerprint();
    let schemas = JIT_ADAPTER_CONTRACTS
        .iter()
        .map(|entry| {
            host_extension::validate_catalog_import_schemas_with_fingerprints(
                catalog,
                &contract,
                entry.name,
                catalog_fingerprint,
                contract_fingerprint,
            )
            .map(|schemas| (entry, schemas))
        })
        .collect::<VmResult<Vec<_>>>()?;

    registry.transactionally(|staged| {
        staged.install_named_struct_schemas(catalog_named_struct_schemas(catalog));
        for (entry, schemas) in &schemas {
            for schema in schemas.iter().cloned() {
                staged.register_exact_static(entry.name, entry.arity, schema, entry.adapter)?;
            }
            staged.authorize_registered_builtin_import(entry.name);
        }
        Ok(())
    })
}
