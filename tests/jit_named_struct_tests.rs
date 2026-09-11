//! JIT config host maps migrate to one named struct `JitConfig`.
//!
//! Runtime values remain maps. Production compile/bind uses the standard
//! catalog; `jit_host_catalog()` remains the JIT subcatalog.

#![cfg(feature = "runtime")]

use std::sync::Arc;

use vm::compiler::{
    CompileSourceFileOptions, SourceFlavor, compile_source_with_flavor_and_options,
};
use vm::host_api::{HostApiCatalog, HostTypeSchema};
use vm::{
    CompiledProgram, HostFunctionRegistry, SourcePathError, Value, Vm, VmStatus, compile_source,
    jit_host_catalog, register_jit_builtin_module, register_jit_builtin_module_from_catalog,
    standard_composition, standard_host_catalog,
};

const JIT_CONFIG: &str = "JitConfig";
const GET_CONFIG: &str = "jit::get_config";
const SET_CONFIG: &str = "jit::set_config";

fn compile(source: &str, catalog: Arc<HostApiCatalog>) -> Result<CompiledProgram, SourcePathError> {
    compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    )
}

fn compile_err(source: &str, catalog: Arc<HostApiCatalog>) -> String {
    match compile(source, catalog) {
        Ok(_) => panic!("expected compile error"),
        Err(err) => err.to_string(),
    }
}

fn jit_config_type(catalog: &HostApiCatalog) -> HostTypeSchema {
    catalog
        .struct_named(JIT_CONFIG)
        .expect("JitConfig must be declared")
        .as_type()
}

fn run_jit_host(source: &str) -> Vec<Value> {
    let catalog = jit_host_catalog();
    let compiled = compile(source, Arc::clone(&catalog)).expect("compile should succeed");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let mut registry = HostFunctionRegistry::empty();
    register_jit_builtin_module_from_catalog(&mut registry, catalog.as_ref())
        .expect("JIT exact registration should succeed");
    registry
        .bind_vm_cached(&mut vm)
        .expect("JIT exact host imports should bind");
    loop {
        match vm.run().expect("vm should run") {
            VmStatus::Halted => break,
            VmStatus::Yielded => continue,
            VmStatus::Waiting(_) => panic!("JIT config calls must not pending"),
        }
    }
    vm.stack().to_vec()
}

#[test]
fn jit_catalog_declares_named_config_struct() {
    let catalog = jit_host_catalog();
    let schema = catalog
        .struct_named(JIT_CONFIG)
        .expect("JitConfig must be present in the JIT catalog");
    let names: Vec<&str> = schema
        .fields
        .iter()
        .map(|field| field.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["enabled", "hot_loop_threshold", "max_trace_len"],
        "JitConfig fields must be exactly the implementation config keys"
    );
    assert_eq!(schema.fields[0].ty, HostTypeSchema::Bool);
    assert_eq!(schema.fields[1].ty, HostTypeSchema::Int);
    assert_eq!(schema.fields[2].ty, HostTypeSchema::Int);
}

#[test]
fn jit_get_config_returns_named_struct() {
    let catalog = jit_host_catalog();
    let functions = catalog.functions_named(GET_CONFIG);
    assert_eq!(functions.len(), 1);
    assert!(functions[0].params.is_empty());
    assert_eq!(functions[0].return_type, jit_config_type(&catalog));
}

#[test]
fn jit_set_config_takes_named_struct() {
    let catalog = jit_host_catalog();
    let named = catalog
        .functions_named(SET_CONFIG)
        .into_iter()
        .find(|function| function.params.len() == 1)
        .expect("jit::set_config(JitConfig) overload must exist");
    assert_eq!(named.params[0].name, "config");
    assert_eq!(named.params[0].ty, jit_config_type(&catalog));
    assert_eq!(named.return_type, jit_config_type(&catalog));
}

#[test]
fn jit_set_config_keeps_positional_overload() {
    let catalog = jit_host_catalog();
    let positional = catalog
        .functions_named(SET_CONFIG)
        .into_iter()
        .find(|function| function.params.len() == 3)
        .expect("jit::set_config(bool, int, int) overload must exist");
    assert_eq!(
        positional
            .params
            .iter()
            .map(|param| (param.name.as_str(), &param.ty))
            .collect::<Vec<_>>(),
        [
            ("enabled", &HostTypeSchema::Bool),
            ("hot_loop_threshold", &HostTypeSchema::Int),
            ("max_trace_len", &HostTypeSchema::Int),
        ]
    );
    assert_eq!(positional.return_type, jit_config_type(&catalog));
}

#[test]
fn compiler_emits_named_struct_import_schemas() {
    let catalog = jit_host_catalog();
    let compiled = compile(
        r#"
        use jit;
        let cfg = jit::get_config();
        cfg.enabled;
        "#,
        Arc::clone(&catalog),
    )
    .expect("field access on jit::get_config should compile");
    let schema = compiled
        .program
        .host_import_schemas()
        .iter()
        .flatten()
        .find(|schema| schema.name == GET_CONFIG)
        .expect("jit::get_config must have an import schema");
    assert_eq!(schema.return_type, jit_config_type(&catalog));
    assert_eq!(schema.fingerprint, catalog.fingerprint());
}

#[test]
fn field_access_on_get_config_compiles() {
    compile(
        r#"
        use jit;
        let cfg = jit::get_config();
        let _enabled = cfg.enabled;
        let _hot = cfg.hot_loop_threshold;
        let _max = cfg.max_trace_len;
        "#,
        jit_host_catalog(),
    )
    .expect(".field access on JitConfig should compile");
}

#[test]
fn object_literal_set_config_compiles() {
    compile(
        r#"
        use jit;
        jit::set_config({
            enabled: true,
            hot_loop_threshold: 3,
            max_trace_len: 64
        });
        "#,
        jit_host_catalog(),
    )
    .expect("object literal should match JitConfig");
}

#[test]
fn unknown_field_is_rejected() {
    let message = compile_err(
        r#"
        use jit;
        jit::set_config({
            enabled: true,
            hot_loop_threshold: 3,
            max_trace_len: 64,
            extra: 1
        });
        "#,
        jit_host_catalog(),
    );
    assert!(
        message.contains("JitConfig")
            || message.contains("field")
            || message.contains("extra")
            || message.contains("match"),
        "unknown field should be rejected, got {message}"
    );
}

#[test]
fn missing_field_is_rejected() {
    let message = compile_err(
        r#"
        use jit;
        jit::set_config({ enabled: true, hot_loop_threshold: 3 });
        "#,
        jit_host_catalog(),
    );
    assert!(
        message.contains("JitConfig")
            || message.contains("field")
            || message.contains("max_trace_len")
            || message.contains("match"),
        "missing field should be rejected, got {message}"
    );
}

#[test]
fn wrong_field_type_is_rejected() {
    let message = compile_err(
        r#"
        use jit;
        jit::set_config({
            enabled: 1,
            hot_loop_threshold: 3,
            max_trace_len: 64
        });
        "#,
        jit_host_catalog(),
    );
    assert!(
        message.contains("JitConfig")
            || message.contains("bool")
            || message.contains("enabled")
            || message.contains("match")
            || message.contains("type"),
        "wrong field type should be rejected, got {message}"
    );
}

#[test]
fn exact_registration_resolves_jit_config_imports() {
    let catalog = jit_host_catalog();
    let mut registry = HostFunctionRegistry::empty();
    register_jit_builtin_module(&mut registry).expect("register JIT");
    assert!(
        registry.named_struct_schemas().contains_key(JIT_CONFIG),
        "JIT exact registration must install JitConfig"
    );
    assert_eq!(
        vm::catalog_import_schemas(&catalog, GET_CONFIG)[0].return_type,
        jit_config_type(&catalog)
    );
}

#[test]
fn runtime_object_literal_and_field_access() {
    let stack = run_jit_host(
        r#"
        use jit;
        let _updated = jit::set_config({
            enabled: true,
            hot_loop_threshold: 3,
            max_trace_len: 64
        });
        let cfg = jit::get_config();
        cfg.hot_loop_threshold;
        "#,
    );
    assert_eq!(stack, vec![Value::Int(3)]);
}

#[test]
fn runtime_carrier_remains_map() {
    let catalog = jit_host_catalog();
    let compiled = compile("use jit; jit::get_config();", Arc::clone(&catalog))
        .expect("get_config should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let mut registry = HostFunctionRegistry::empty();
    register_jit_builtin_module_from_catalog(&mut registry, catalog.as_ref())
        .expect("register JIT");
    registry.bind_vm_cached(&mut vm).expect("bind JIT");
    match vm.run().expect("run") {
        VmStatus::Halted => {}
        other => panic!("expected halt, got {other:?}"),
    }
    match vm.stack() {
        [Value::Map(_)] => {}
        other => panic!("runtime carrier must remain a map, got {other:?}"),
    }
}

#[test]
fn unrelated_generated_host_schemas_are_unchanged() {
    let io = vm::io_host_catalog();
    assert!(
        io.struct_named(JIT_CONFIG).is_none(),
        "IO catalog must not gain JitConfig"
    );
    assert!(
        io.functions_named(GET_CONFIG).is_empty() && io.functions_named(SET_CONFIG).is_empty(),
        "IO catalog must not declare jit config functions"
    );
    #[cfg(feature = "http-client")]
    {
        let http = vm::http_host_catalog();
        assert!(http.struct_named(JIT_CONFIG).is_none());
        assert!(http.functions_named(GET_CONFIG).is_empty());
        assert!(http.functions_named(SET_CONFIG).is_empty());
        for function in http.functions() {
            assert!(
                !matches!(&function.return_type, HostTypeSchema::Named { name, .. } if name == JIT_CONFIG),
                "HTTP function {} must not return JitConfig",
                function.name
            );
        }
    }
    for callable in vm::default_host_callables() {
        assert!(
            !callable.name.contains("jit::"),
            "generated default host callables must not include {}",
            callable.name
        );
    }
}

#[test]
fn standard_catalog_includes_typed_jit_config() {
    let jit = jit_host_catalog();
    let standard = standard_host_catalog();
    assert_ne!(
        jit.fingerprint(),
        standard.fingerprint(),
        "JIT-local catalog remains a subcatalog of the combined snapshot"
    );
    let schema = standard
        .struct_named(JIT_CONFIG)
        .expect("standard catalog must declare JitConfig whenever JIT builtins are available");
    assert_eq!(
        schema
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>(),
        ["enabled", "hot_loop_threshold", "max_trace_len"]
    );
    let get = standard.functions_named(GET_CONFIG);
    assert_eq!(get.len(), 1);
    assert!(get[0].params.is_empty());
    assert_eq!(get[0].return_type, jit_config_type(&standard));
    let set = standard.functions_named(SET_CONFIG);
    assert_eq!(set.len(), 2, "named and positional set_config overloads");
    assert!(set.iter().any(|function| function.params.len() == 1));
    assert!(set.iter().any(|function| function.params.len() == 3));
}

fn compile_defaults(source: &str) -> CompiledProgram {
    compile_source(source).expect("default compiler should attach the standard catalog")
}

fn run_defaults(source: &str) -> Vec<Value> {
    let compiled = compile_defaults(source);
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    vm.set_standard_composition(standard_composition());
    loop {
        match vm.run().expect("vm should run") {
            VmStatus::Halted => break,
            VmStatus::Yielded => continue,
            VmStatus::Waiting(_) => panic!("JIT config calls must not pending"),
        }
    }
    vm.stack().to_vec()
}

#[test]
fn default_compiler_emits_standard_jit_config_imports() {
    let compiled = compile_defaults(
        r#"
        use jit;
        let cfg = jit::get_config();
        cfg.enabled;
        "#,
    );
    let schema = compiled
        .program
        .host_import_schemas()
        .iter()
        .flatten()
        .find(|schema| schema.name == GET_CONFIG)
        .expect("default compiler must not lower jit::get_config as a namespaced builtin");
    assert_eq!(
        schema.return_type,
        jit_config_type(&standard_host_catalog())
    );
    assert_eq!(schema.fingerprint, standard_host_catalog().fingerprint());
}

#[test]
fn default_compiler_accepts_positional_set_config() {
    let compiled = compile_defaults("use jit; jit::set_config(true, 3, 64);");
    let schema = compiled
        .program
        .host_import_schemas()
        .iter()
        .flatten()
        .find(|schema| schema.name == SET_CONFIG)
        .expect("positional jit::set_config must be a catalog host import");
    assert_eq!(schema.params.len(), 3);
    assert_eq!(schema.fingerprint, standard_host_catalog().fingerprint());
}

#[test]
fn standard_registry_resolves_default_jit_imports() {
    let compiled = compile_defaults(
        r#"
        use jit;
        jit::get_config();
        jit::set_config(true, 3, 64);
        jit::set_config({
            enabled: false,
            hot_loop_threshold: 1,
            max_trace_len: 8
        });
        "#,
    );
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let mut registry = HostFunctionRegistry::empty();
    register_jit_builtin_module(&mut registry).expect("production JIT registration");
    registry
        .bind_vm_cached(&mut vm)
        .expect("standard fingerprint registration must bind JIT imports");
}

#[test]
fn default_vm_installs_typed_jit_config() {
    let stack = run_defaults(
        r#"
        use jit;
        let _positional = jit::set_config(true, 3, 64);
        let named = jit::set_config({
            enabled: true,
            hot_loop_threshold: 5,
            max_trace_len: 32
        });
        named.hot_loop_threshold;
        "#,
    );
    assert_eq!(stack, vec![Value::Int(5)]);
}

#[test]
fn default_registry_bind_installs_typed_jit_config() {
    let compiled = compile_defaults(
        r#"
        use jit;
        let _updated = jit::set_config(false, 7, 16);
        let cfg = jit::get_config();
        cfg.max_trace_len;
        "#,
    );
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let registry = HostFunctionRegistry::new();
    registry
        .bind_vm_cached(&mut vm)
        .expect("default registry must stage JIT exact adapters");
    match vm.run().expect("run") {
        VmStatus::Halted => {}
        other => panic!("expected halt, got {other:?}"),
    }
    assert_eq!(vm.stack(), &[Value::Int(16)]);
}
