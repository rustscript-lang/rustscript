#![cfg(feature = "runtime")]

use pd_host_function::pd_host_function;
use std::sync::Arc;
use vm::{GuestResult, Value, Vm, VmError, VmResult, VmStatus};

#[derive(Clone, Debug, PartialEq)]
struct SdkFailure<T> {
    code: T,
}
impl vm::HostNamedStruct for SdkFailure<i64> {
    const NAME: &'static str = "SdkFailureInt";
    fn host_struct_fields() -> Vec<vm::HostStructField> {
        vec![vm::HostStructField::new("code", vm::HostTypeSchema::Int)]
    }
}
impl<T: vm::IntoVmValue> vm::IntoVmValue for SdkFailure<T> {
    fn into_vm_value(self) -> Value {
        Value::map(vec![(Value::string("code"), self.code.into_vm_value())])
    }
}

mod generated {
    pub use vm::borrow_arg;
    pub mod functions {
        use super::super::*;
        include!("fixtures/result_sdk_signature.rs");

        /// Returns a guest success, guest failure, or fatal transport error.
        #[pd_host_function(name = "result_sdk::tri")]
        fn tri(vm: &mut Vm, mode: i64) -> VmResult<GuestResult<i64, String>> {
            let calls = vm
                .host_context()
                .module_state::<u64>()
                .copied()
                .unwrap_or(0);
            vm.host_context().set_module_state(calls + 1);
            match mode {
                0 => Ok(GuestResult::Ok(7)),
                1 => Ok(GuestResult::Err("guest failure".into())),
                _ => Err(VmError::HostError("transport failure".into())),
            }
        }
    }
}

include!(concat!(env!("OUT_DIR"), "/result_sdk_build_schemas.rs"));

#[test]
fn actual_build_fixture_and_proc_macro_have_identical_typed_descriptors() {
    for (macro_descriptor, (schema, effects, binding)) in [
        (
            generated::functions::fixture_descriptor(),
            result_sdk_build_fixture(),
        ),
        (
            generated::functions::named_descriptor(),
            result_sdk_build_named(),
        ),
        (
            generated::functions::inspect_descriptor(),
            result_sdk_build_inspect(),
        ),
    ] {
        assert_eq!(schema, macro_descriptor.schema);
        assert_eq!(effects, macro_descriptor.effects);
        assert_eq!(binding, macro_descriptor.binding.kind);
    }
    assert!(matches!(
        generated::functions::fixture_descriptor().schema.params[0].ty,
        vm::HostTypeSchema::Result { .. }
    ));
}

#[test]
fn generated_named_generic_result_catalog_declares_payload() {
    let descriptors = [generated::functions::named_descriptor()];
    let catalog = vm::HostFunctionDescriptor::collect_catalog(&descriptors).unwrap();
    assert!(catalog.struct_named("SdkFailureInt").is_some());
    for (branch, expected) in [("true", 7), ("false", 29)] {
        let source = format!(
            "use result_sdk; match result_sdk::named({branch}) {{ Result::Ok(v) => v, Result::Err(e) => e.code, }};"
        );
        let compiled = vm::compile_source_with_flavor_and_options(
            &source,
            vm::SourceFlavor::RustScript,
            vm::CompileSourceFileOptions::default()
                .with_host_api_catalog(Arc::new(catalog.clone())),
        )
        .unwrap();
        let mut registry = vm::HostFunctionRegistry::empty();
        registry.set_capability_profile(vm::CapabilityProfile::deny_all());
        vm::HostModuleDescriptor::install_descriptors(&mut registry, &descriptors).unwrap();
        registry.authorize_registered_builtin_import("result_sdk::named");
        let mut machine = Vm::new(compiled.program.with_local_count(compiled.locals));
        registry.bind_vm_cached(&mut machine).unwrap();
        machine.set_fuel(100_000);
        assert_eq!(machine.run().unwrap(), VmStatus::Halted);
        assert_eq!(machine.stack().last(), Some(&Value::Int(expected)));
    }
}

#[test]
fn typed_array_optional_generated_roundtrip() {
    for (value, expected) in [
        ("Result::Ok(null)", -1),
        ("Result::Ok([3, 4])", 7),
        ("Result::Err(\"typed error\")", -2),
    ] {
        let descriptors = vec![
            generated::functions::fixture_descriptor(),
            generated::functions::inspect_descriptor(),
        ];
        let catalog = Arc::new(vm::HostFunctionDescriptor::collect_catalog(&descriptors).unwrap());
        let source = format!(
            "use result_sdk; let input: Result<int[]?, string> = {value}; let value: Result<int[]?, string> = result_sdk::fixture(input); result_sdk::inspect(value);"
        );
        let compiled = vm::compile_source_with_flavor_and_options(
            &source,
            vm::SourceFlavor::RustScript,
            vm::CompileSourceFileOptions::default().with_host_api_catalog(catalog),
        )
        .unwrap();
        let mut registry = vm::HostFunctionRegistry::empty();
        registry.set_capability_profile(vm::CapabilityProfile::deny_all());
        vm::HostModuleDescriptor::install_descriptors(&mut registry, &descriptors).unwrap();
        for descriptor in &descriptors {
            registry.authorize_registered_builtin_import(&descriptor.schema.name);
        }
        let mut machine = Vm::new(compiled.program.with_local_count(compiled.locals));
        registry.bind_vm_cached(&mut machine).unwrap();
        machine.set_fuel(100_000);
        assert_eq!(machine.run().unwrap(), VmStatus::Halted);
        assert_eq!(machine.stack().last(), Some(&Value::Int(expected)));
    }
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn native_guest_err_and_outer_vm_error_remain_separate() {
    for (mode, guest_value) in [(0, Some(7)), (1, Some(-9)), (2, None)] {
        let mut machine = machine(&format!(
            "use result_sdk; fn run(mode: int) -> int {{ match result_sdk::tri(mode) {{ Result::Ok(v) => v, Result::Err(e) => -9, }} }} let f = run; f({mode});"
        ));
        machine.compile_aot().unwrap();
        let dump = machine.dump_aot_info();
        assert!(dump.contains("lowering=ssa"), "{dump}");
        assert!(!dump.contains("interpreter-boundary"), "{dump}");
        eprintln!("{dump}");
        match guest_value {
            Some(value) => {
                assert_eq!(machine.run().unwrap(), VmStatus::Halted);
                assert_eq!(machine.stack().last(), Some(&Value::Int(value)));
            }
            None => assert!(
                machine
                    .run()
                    .unwrap_err()
                    .to_string()
                    .contains("transport failure")
            ),
        }
        let count = machine.aot_exec_count();
        assert!(count > 0, "{dump}");
        assert_eq!(machine.host_context().module_state::<u64>(), Some(&1));
        eprintln!(
            "typed Result native mode={mode} payload={guest_value:?} lowering=ssa executions={count} host_calls=1"
        );
    }
}

#[test]
fn generated_catalog_runtime_rejects_malformed_carriers_before_handler() {
    let descriptor = generated::functions::inspect_descriptor();
    let descriptors = std::slice::from_ref(&descriptor);
    let catalog = vm::HostFunctionDescriptor::collect_catalog(descriptors).unwrap();
    for value in [
        Value::Null,
        Value::map(vec![
            (Value::string("tag"), Value::Int(1)),
            (Value::string("payload"), Value::array(vec![])),
        ]),
        Value::map(vec![(Value::string("payload"), Value::array(vec![]))]),
        vm::result::carrier(false, Value::Null),
        vm::result::carrier(true, Value::array(vec![Value::Bool(false)])),
        Value::map(vec![
            (Value::string("tag"), Value::Bool(true)),
            (Value::string("extra"), Value::Int(1)),
        ]),
    ] {
        let import = vm::catalog_import_schemas(&catalog, "result_sdk::inspect").remove(0);
        let mut code = vm::BytecodeBuilder::new();
        code.ldc(0);
        code.call(0, 1);
        code.ret();
        let program = vm::Program::with_imports_and_debug(
            vec![value],
            code.finish(),
            vec![vm::HostImport {
                name: "result_sdk::inspect".into(),
                arity: 1,
                return_type: vm::ValueType::Int,
            }],
            None,
        )
        .with_host_import_schemas(vec![import])
        .unwrap();
        let mut machine = Vm::new(program);
        let mut registry = vm::HostFunctionRegistry::empty();
        registry.set_capability_profile(vm::CapabilityProfile::deny_all());
        vm::HostModuleDescriptor::install_descriptors(&mut registry, descriptors).unwrap();
        registry.authorize_registered_builtin_import("result_sdk::inspect");
        registry.bind_vm_cached(&mut machine).unwrap();
        machine.set_fuel(100_000);
        assert!(machine.run().is_err());
        assert_eq!(machine.host_context().module_state::<usize>(), None);
        assert_eq!(machine.host_context().resource_count(), 0);
    }
}

#[test]
fn generated_catalog_without_adapter_binding_is_rejected() {
    let descriptors = [generated::functions::tri_descriptor()];
    let catalog = Arc::new(vm::HostFunctionDescriptor::collect_catalog(&descriptors).unwrap());
    let compiled = vm::compile_source_with_flavor_and_options(
        "use result_sdk; result_sdk::tri(0);",
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    )
    .unwrap();
    let mut machine = Vm::new(compiled.program.with_local_count(compiled.locals));
    let mut registry = vm::HostFunctionRegistry::empty();
    registry.set_capability_profile(vm::CapabilityProfile::deny_all());
    assert!(registry.bind_vm_cached(&mut machine).is_err());
    assert_eq!(machine.host_context().module_state::<u64>(), None);
}

fn machine(source: &str) -> Vm {
    let descriptors = vec![generated::functions::tri_descriptor()];
    let catalog = Arc::new(vm::HostFunctionDescriptor::collect_catalog(&descriptors).unwrap());
    let compiled = vm::compile_source_with_flavor_and_options(
        source,
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    )
    .unwrap();
    let mut registry = vm::HostFunctionRegistry::empty();
    registry.set_capability_profile(vm::CapabilityProfile::deny_all());
    vm::HostModuleDescriptor::install_descriptors(&mut registry, &descriptors).unwrap();
    for descriptor in &descriptors {
        registry.authorize_registered_builtin_import(&descriptor.schema.name);
    }
    let mut machine = Vm::new(compiled.program.with_local_count(compiled.locals));
    registry.bind_vm_cached(&mut machine).unwrap();
    machine.set_fuel(100_000);
    machine
}

#[test]
fn outer_ok_guest_ok_runs_through_generated_restricted_catalog() {
    let mut vm = machine(
        "use result_sdk; match result_sdk::tri(0) { Result::Ok(v) => v, Result::Err(e) => -1, };",
    );
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack().last(), Some(&Value::Int(7)));
}

#[test]
fn outer_ok_guest_err_is_guest_data() {
    let mut vm = machine(
        "use result_sdk; match result_sdk::tri(1) { Result::Ok(v) => \"wrong\", Result::Err(e) => e, };",
    );
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack().last(), Some(&Value::string("guest failure")));
}

#[test]
fn outer_vm_error_remains_fatal() {
    let mut vm = machine("use result_sdk; result_sdk::tri(2);");
    assert!(
        vm.run()
            .unwrap_err()
            .to_string()
            .contains("transport failure")
    );
}

#[test]
fn typed_conversions_preserve_branch_null_false_zero_and_reject_noncanonical() {
    use vm::{FromVmValue, IntoVmValue, TakeVmValue};
    for value in [
        GuestResult::<Option<bool>, Option<bool>>::Ok(None),
        GuestResult::Err(None),
        GuestResult::Ok(Some(false)),
        GuestResult::Err(Some(false)),
    ] {
        let encoded = value.into_vm_value();
        assert_eq!(
            GuestResult::<Option<bool>, Option<bool>>::from_vm_value(&encoded, "result").unwrap(),
            value
        );
    }
    let bytes = GuestResult::<Vec<u8>, ()>::Ok(vec![0, 255]);
    let mut encoded = bytes.clone().into_vm_value();
    assert_eq!(
        GuestResult::<Vec<u8>, ()>::from_vm_value(&encoded, "bytes").unwrap(),
        bytes
    );
    assert_eq!(
        GuestResult::<Vec<u8>, ()>::take_vm_value(&mut encoded, "bytes").unwrap(),
        bytes
    );
    let array = GuestResult::<Vec<i64>, String>::Ok(vec![0, 7]);
    let encoded = array.clone().into_vm_value();
    assert_eq!(
        GuestResult::<Vec<i64>, String>::from_vm_value(&encoded, "array").unwrap(),
        array
    );
    let mut value = GuestResult::<i64, String>::Ok(0).into_vm_value();
    assert_eq!(
        GuestResult::<i64, String>::take_vm_value(&mut value, "result").unwrap(),
        GuestResult::Ok(0)
    );
    assert_eq!(value, Value::Null);
    for value in [
        Value::map(vec![
            (Value::string("tag"), Value::Int(1)),
            (Value::string("payload"), Value::Int(0)),
        ]),
        Value::map(vec![(Value::string("tag"), Value::Bool(true))]),
        Value::map(vec![
            (Value::string("tag"), Value::Bool(true)),
            (Value::string("payload"), Value::Int(0)),
            (Value::string("extra"), Value::Int(1)),
        ]),
    ] {
        assert!(GuestResult::<i64, String>::from_vm_value(&value, "result").is_err());
    }
}

#[test]
fn callback_guest_result_both_variants_and_store_identity() {
    let compiled =
        vm::compile_source("pub fn echo(r: Result<int, string>) -> Result<int, string> { r }")
            .unwrap();
    let mut store = vm::Store::new(
        Vm::new(compiled.program.with_local_count(compiled.locals)),
        (),
    );
    store.vm_mut().set_fuel(100_000);
    assert_eq!(store.run().unwrap(), VmStatus::Halted);
    let callback = store
        .script_callback_by_name::<(GuestResult<i64, String>,), GuestResult<i64, String>>("echo")
        .unwrap();
    for value in [
        GuestResult::Ok(0),
        GuestResult::Err("typed error".to_string()),
    ] {
        assert_eq!(callback.call(&mut store, (value.clone(),)).unwrap(), value);
    }
    let mut other = vm::Store::new(
        Vm::new(vm::Program::new(Vec::new(), vec![vm::OpCode::Ret as u8])),
        (),
    );
    assert!(callback.call(&mut other, (GuestResult::Ok(1),)).is_err());
    store.reset_for_reuse().unwrap();
    assert!(callback.call(&mut store, (GuestResult::Ok(1),)).is_err());
}
