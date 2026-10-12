use vm::compiler::TypeSchema;
use vm::{ReplLocalBinding, ReplLocalState, compile_source_for_repl_with_state};

#[test]
fn public_repl_state_api_preserves_moved_local_semantics() {
    let binding = ReplLocalBinding {
        name: "message".to_string(),
        mutable: false,
        schema: Some(TypeSchema::String),
        optional: false,
    };

    let available = ReplLocalState {
        binding: binding.clone(),
        moved: false,
    };
    compile_source_for_repl_with_state("message;", &[available])
        .expect("available local should compile");

    let moved = ReplLocalState {
        binding,
        moved: true,
    };
    assert!(
        compile_source_for_repl_with_state("message;", &[moved]).is_err(),
        "moved local must be rejected"
    );
}

#[cfg(feature = "runtime")]
#[test]
fn repl_options_preserve_named_host_identity_and_runtime_values_between_snippets() {
    use std::sync::Arc;
    use vm::{
        CallOutcome, CompileSourceFileOptions, HostApiBuilder, HostFunctionRegistry,
        HostFunctionSchema, HostParamSchema, HostStructField, HostStructSchema, HostTypeSchema,
        Value, Vm, VmStatus,
    };
    let point = HostStructSchema::new(
        "Calibration",
        vec![HostStructField::new("value", HostTypeSchema::Int)],
    );
    let mut builder = HostApiBuilder::new();
    builder.named_struct(point.clone());
    builder.function(HostFunctionSchema::with_return(
        "calibration::origin",
        vec![],
        point.as_type(),
    ));
    builder.function(HostFunctionSchema::with_return(
        "calibration::read",
        vec![HostParamSchema::value("point", point.as_type())],
        HostTypeSchema::Int,
    ));
    let catalog = Arc::new(builder.build().unwrap());
    let options = CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog));
    fn origin(_: &mut Vm, _: &[Value]) -> vm::VmResult<CallOutcome> {
        Ok(CallOutcome::Return(vm::return_one(Value::map(vec![(
            Value::string("value"),
            Value::Int(42),
        )]))))
    }
    fn read(_: &mut Vm, args: &[Value]) -> vm::VmResult<CallOutcome> {
        let Value::Map(fields) = &args[0] else {
            return Err(vm::VmError::TypeMismatch("Calibration"));
        };
        Ok(CallOutcome::Return(vm::return_one(
            fields.get(&Value::string("value")).unwrap().clone(),
        )))
    }
    let mut registry = HostFunctionRegistry::new();
    for (name, handler) in [
        ("calibration::origin", origin as vm::StaticHostFunction),
        ("calibration::read", read),
    ] {
        registry
            .register_catalog_static(
                vm::catalog_import_schemas(&catalog, name).remove(0),
                handler,
            )
            .unwrap();
    }
    let first = vm::compile_source_for_repl_with_state_and_options(
        "let point = calibration::origin();",
        &[],
        options.clone(),
    )
    .unwrap();
    let mut first_vm = Vm::new(first.compiled.program);
    registry.bind_vm_cached(&mut first_vm).unwrap();
    assert_eq!(first_vm.run().unwrap(), VmStatus::Halted);
    let slot = first_vm.debug_info().unwrap().local_index("point").unwrap();
    let schema = first_vm.program().type_map.as_ref().unwrap().local_schemas[slot as usize]
        .clone()
        .unwrap();
    assert!(matches!(&schema, TypeSchema::Named(name, _) if name == "Calibration"));
    let state = ReplLocalState {
        binding: ReplLocalBinding {
            name: "point".into(),
            mutable: false,
            schema: Some(schema),
            optional: false,
        },
        moved: false,
    };
    let second = vm::compile_source_for_repl_with_state_and_options(
        "calibration::read(point);",
        &[state],
        options.clone(),
    )
    .unwrap();
    let exact = second
        .compiled
        .program
        .host_import_schemas()
        .iter()
        .flatten()
        .find(|s| s.name == "calibration::read")
        .unwrap();
    assert_eq!(
        *exact,
        vm::catalog_import_schemas(&catalog, "calibration::read").remove(0)
    );
    let mut second_vm = Vm::new(second.compiled.program);
    registry.bind_vm_cached(&mut second_vm).unwrap();
    let next_slot = second_vm
        .debug_info()
        .unwrap()
        .local_index("point")
        .unwrap();
    second_vm
        .set_local(next_slot, first_vm.locals()[slot as usize].clone())
        .unwrap();
    assert_eq!(second_vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(second_vm.stack().last(), Some(&Value::Int(42)));
    let model = vm::analyze_source_from_string_with_options(
        "calibration.rss",
        "let point = calibration::origin(); point.value;",
        options.clone(),
    )
    .unwrap();
    assert_eq!(model.catalog().as_ref(), catalog.as_ref());
    let hints = vm::collect_inferred_local_type_hints_with_options(
        "let point = calibration::origin(); point.value;",
        vm::SourceFlavor::RustScript,
        options,
    )
    .unwrap();
    assert!(
        hints.iter().any(
            |h| matches!(&h.schema, Some(TypeSchema::Named(name, _)) if name == "Calibration")
        )
    );
}
