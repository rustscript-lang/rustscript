//! JIT config host maps migrate to one named struct `JitConfig`.
//!
//! Runtime values remain maps. Compiler/VM consume `jit_host_catalog()`.

#![cfg(feature = "runtime")]

use std::sync::Arc;

use vm::compiler::{
    CompileSourceFileOptions, SourceFlavor, TypeSchema, compile_source_with_flavor_and_options,
};
use vm::host_api::{HostApiCatalog, HostTypeSchema};
use vm::{
    CompiledProgram, HostFunctionRegistry, HostImport, SourcePathError, Value, ValueType, Vm,
    VmStatus, jit_host_catalog, register_jit_builtin_module,
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
    register_jit_builtin_module(&mut registry).expect("JIT exact registration should succeed");
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
    let functions = catalog.functions_named(SET_CONFIG);
    assert_eq!(functions.len(), 1);
    assert_eq!(functions[0].params.len(), 1);
    assert_eq!(functions[0].params[0].name, "config");
    assert_eq!(functions[0].params[0].ty, jit_config_type(&catalog));
    assert_eq!(functions[0].return_type, jit_config_type(&catalog));
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
    let import = compiled
        .program
        .imports
        .iter()
        .find(|import| import.name == GET_CONFIG)
        .expect("jit::get_config must be a host import");
    let schema = import.schema.as_ref().expect("exact schema");
    assert_eq!(
        schema.return_type,
        TypeSchema::Named(JIT_CONFIG.to_string(), vec![])
    );
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
    for name in [GET_CONFIG, SET_CONFIG] {
        for schema in vm::catalog_import_schemas(&catalog, name) {
            let import = HostImport {
                name: name.to_string(),
                arity: schema.params.len() as u8,
                return_type: ValueType::Map,
                schema: Some(schema),
            };
            assert!(
                registry.resolve_import(&import).is_ok(),
                "exact registration must resolve {name}"
            );
        }
    }
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
    register_jit_builtin_module(&mut registry).expect("register JIT");
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
fn jit_catalog_is_not_the_standard_snapshot() {
    let jit = jit_host_catalog();
    let standard = vm::standard_host_catalog();
    assert_ne!(
        jit.fingerprint(),
        standard.fingerprint(),
        "JIT-local catalog must not rewrite the standard combined snapshot"
    );
    assert!(
        standard.struct_named(JIT_CONFIG).is_none(),
        "standard catalog must not pick up JitConfig unless this surface is composed in"
    );
}
