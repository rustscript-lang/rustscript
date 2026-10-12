use vm::compiler::TypeSchema;
use vm::{ReplLocalBinding, ReplLocalState, compile_source_for_repl_with_state};

const NINE_MODULE_LOCALS: &str = "let a = 101; let b = 102; let c = 103; let d = 104; let e = 105; let f = 106; let g = 107; let h = 108; let i = 109; pub fn value() -> int { 1 }";

#[cfg(feature = "runtime")]
fn execute_persistent_snippets(
    snippets: &[&str],
    options: vm::CompileSourceFileOptions,
) -> Vec<vm::Value> {
    let mut states = Vec::<ReplLocalState>::new();
    let mut values = std::collections::BTreeMap::<String, vm::Value>::new();
    let mut results = Vec::new();
    for source in snippets {
        let compiled =
            vm::compile_source_for_repl_with_state_and_options(source, &states, options.clone())
                .unwrap();
        let mut machine = vm::Vm::new(compiled.compiled.program);
        for (name, value) in &values {
            let slot = machine.debug_info().unwrap().local_index(name).unwrap();
            machine.set_local(slot, value.clone()).unwrap();
        }
        assert_eq!(machine.run().unwrap(), vm::VmStatus::Halted);
        results.extend_from_slice(machine.stack());
        states.clear();
        values.clear();
        for mut binding in compiled.bindings {
            let slot = machine
                .debug_info()
                .unwrap()
                .local_index(&binding.name)
                .unwrap() as usize;
            let map = machine.program().type_map.as_ref().unwrap();
            binding.schema = map.local_schemas[slot].clone();
            binding.optional = map.optional_slots[slot];
            values.insert(binding.name.clone(), machine.locals()[slot].clone());
            states.push(ReplLocalState {
                binding,
                moved: false,
            });
        }
    }
    results
}

#[cfg(feature = "runtime")]
#[test]
fn final_layout_preserves_unused_entry_across_nine_local_module() {
    let options = vm::CompileSourceFileOptions::default()
        .with_module_override_source("calibration.rss", NINE_MODULE_LOCALS);
    let results = execute_persistent_snippets(
        &[
            "let x = 41;",
            "use calibration; calibration::value();",
            "x;",
        ],
        options,
    );
    assert_eq!(results.last(), Some(&vm::Value::Int(41)));
}

#[cfg(feature = "runtime")]
#[test]
fn final_layout_preserves_all_nine_first_snippet_locals() {
    let results = execute_persistent_snippets(
        &[
            "let a = 11; let b = 12; let c = 13; let d = 14; let e = 15; let f = 16; let g = 17; let h = 18; let i = 19;",
            "a;",
            "b;",
            "c;",
            "d;",
            "e;",
            "f;",
            "g;",
            "h;",
            "i;",
        ],
        vm::CompileSourceFileOptions::default(),
    );
    assert_eq!(results, (11..=19).map(vm::Value::Int).collect::<Vec<_>>());
}

#[test]
fn final_layout_ordinary_source_and_file_keep_compaction() {
    let source = "let a = 11; let b = 12; let c = 13; let d = 14; let e = 15; let f = 16; let g = 17; let h = 18; let i = 19; i;";
    let ordinary = vm::compile_source_with_flavor(source, vm::SourceFlavor::RustScript).unwrap();
    let file = vm::compile_source_at_path_with_flavor_and_options(
        "layout.rss",
        source,
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default(),
    )
    .unwrap();
    assert!(ordinary.locals < 9, "{}", ordinary.locals);
    assert!(file.locals < 9, "{}", file.locals);
    let repl = vm::compile_source_for_repl_with_state(source, &[]).unwrap();
    assert!(repl.compiled.locals >= 9, "{}", repl.compiled.locals);
    #[cfg(feature = "runtime")]
    for program in [ordinary.program, file.program] {
        let mut machine = vm::Vm::new(program);
        machine.run().unwrap();
        assert_eq!(machine.stack().last(), Some(&vm::Value::Int(19)));
    }
}

#[test]
fn final_layout_named_optional_entry_seeds_follow_final_debug_slots() {
    let named = vm::HostStructSchema::new(
        "RootNamedOptional",
        vec![vm::HostStructField::new("value", vm::HostTypeSchema::Int)],
    );
    let mut builder = vm::HostApiBuilder::new();
    builder.named_struct(named.clone());
    builder.function(vm::HostFunctionSchema::with_return(
        "layout::root",
        vec![],
        named.as_type(),
    ));
    let options = vm::CompileSourceFileOptions::default()
        .with_host_api_catalog(std::sync::Arc::new(builder.build().unwrap()))
        .with_module_override_source("calibration.rss", NINE_MODULE_LOCALS);
    let first = vm::compile_source_for_repl_with_state_and_options(
        "let root = layout::root();",
        &[],
        options.clone(),
    )
    .unwrap();
    let slot = first
        .compiled
        .program
        .debug
        .as_ref()
        .unwrap()
        .local_index("root")
        .unwrap() as usize;
    let schema = first
        .compiled
        .program
        .type_map
        .as_ref()
        .unwrap()
        .local_schemas[slot]
        .clone()
        .unwrap();
    assert_eq!(
        schema,
        TypeSchema::Named("RootNamedOptional".into(), vec![])
    );
    let states = [false, true].map(|optional| ReplLocalState {
        binding: ReplLocalBinding {
            name: if optional { "maybe" } else { "root" }.into(),
            mutable: false,
            schema: Some(schema.clone()),
            optional,
        },
        moved: false,
    });
    for source in [
        "use calibration; root.value; maybe == null; calibration::value();",
        "use calibration; calibration::value();",
    ] {
        let compiled =
            vm::compile_source_for_repl_with_state_and_options(source, &states, options.clone())
                .unwrap();
        let program = compiled.compiled.program;
        let map = program.type_map.as_ref().unwrap();
        for state in &states {
            let slot = program
                .debug
                .as_ref()
                .unwrap()
                .local_index(&state.binding.name)
                .unwrap() as usize;
            assert_eq!(
                map.local_schemas[slot],
                Some(schema.clone()),
                "{} in {source}",
                state.binding.name
            );
            assert_eq!(
                map.optional_slots[slot], state.binding.optional,
                "{} in {source}",
                state.binding.name
            );
        }
    }
}

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

#[test]
fn native_repl_module_admission_rejects_wrong_types_resource_copy_and_moved_entry() {
    let options = vm::CompileSourceFileOptions::default()
        .with_module_override_source("calibration.rss", NINE_MODULE_LOCALS);
    let state = |name: &str, schema, moved| ReplLocalState {
        binding: ReplLocalBinding {
            name: name.into(),
            mutable: false,
            schema: Some(schema),
            optional: false,
        },
        moved,
    };
    let wrong = vm::compile_source_for_repl_with_state_and_options(
        "use calibration; let result: bool = x + calibration::value();",
        &[state("x", TypeSchema::Int, false)],
        options.clone(),
    )
    .err()
    .expect("admission must reject before producing a Program");
    assert!(matches!(wrong, vm::SourceError::Compile(_)), "{wrong:?}");
    let resource = vm::ResourceTypeKey::new("repl.admission-resource").unwrap();
    let copy = vm::compile_source_for_repl_with_state_and_options(
        "use calibration; let duplicate = owned.copy(); calibration::value();",
        &[state("owned", TypeSchema::Resource(resource), false)],
        options.clone(),
    )
    .err()
    .expect("admission must reject before producing a Program");
    assert!(
        matches!(&copy, vm::SourceError::Parse(error) if error.code.as_deref() == Some("E_OWNERSHIP_COPY_RESOURCE")),
        "{copy:?}"
    );
    let moved = vm::compile_source_for_repl_with_state_and_options(
        "use calibration; message; calibration::value();",
        &[state("message", TypeSchema::String, true)],
        options,
    )
    .err()
    .expect("admission must reject before producing a Program");
    assert!(
        matches!(&moved, vm::SourceError::Parse(error) if error.code.as_deref() == Some("E_LOCAL_MOVED")),
        "{moved:?}"
    );
    // Compilation has no VM/initializer authority. No failed admission result
    // exposes a Program that an embedding can initialize or execute.
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
