#![cfg(feature = "runtime")]

fn compile_catalog(
    source: &str,
    catalog: &std::sync::Arc<vm::HostApiCatalog>,
) -> Result<vm::CompiledProgram, vm::SourcePathError> {
    vm::compile_source_with_flavor_and_options(
        source,
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default()
            .with_host_api_catalog(std::sync::Arc::clone(catalog)),
    )
}

fn tag_only_unit_catalog() -> std::sync::Arc<vm::HostApiCatalog> {
    let mut builder = vm::HostApiBuilder::new();
    builder.function(vm::HostFunctionSchema::with_return(
        "host::some_unit",
        Vec::new(),
        vm::HostTypeSchema::Option(Box::new(vm::HostTypeSchema::Unit)),
    ));
    builder.function(vm::HostFunctionSchema::with_return(
        "host::some_bad",
        Vec::new(),
        vm::HostTypeSchema::Option(Box::new(vm::HostTypeSchema::Int)),
    ));
    std::sync::Arc::new(builder.build().expect("tag-only unit catalog"))
}

fn tag_only_some_handler(_: &mut vm::Vm, _: &[vm::Value]) -> vm::VmResult<vm::CallOutcome> {
    Ok(vm::CallOutcome::Return(vm::return_one(vm::Value::map(
        vec![(vm::Value::string("tag"), vm::Value::Bool(true))],
    ))))
}

fn run_with_tag_only_unit(source: &str, host_name: &str) -> vm::VmResult<vm::Value> {
    let catalog = tag_only_unit_catalog();
    let compiled = compile_catalog(source, &catalog)
        .unwrap_or_else(|error| panic!("tag-only host source should compile: {source}\n{error}"));
    let mut registry = vm::HostFunctionRegistry::new();
    let schema = vm::catalog_import_schemas(&catalog, host_name)
        .into_iter()
        .next()
        .expect("tag-only host schema");
    registry
        .register_catalog_static(schema, tag_only_some_handler)
        .expect("register tag-only host handler");
    let mut machine = vm::Vm::new(compiled.program.with_local_count(compiled.locals));
    machine.set_fuel(100_000);
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let recorded = std::sync::Arc::clone(&lines);
    machine.set_runtime_print_sink(move |rendered| {
        recorded.lock().expect("sink").push(rendered);
    });
    registry
        .bind_vm_cached(&mut machine)
        .expect("bind tag-only host handler");
    let run = machine.run();
    if run.is_ok() {
        assert_eq!(
            lines.lock().expect("sink").as_slice(),
            ["Some(())".to_string()],
            "canonical tag-only Some(()) must render logically"
        );
    }
    run.map(|_| machine.stack().last().cloned().expect("run result"))
}

#[test]
fn canonical_tag_only_some_unit_prints_and_encodes_a_declared_field() {
    let source = r#"
        use host;
        use json;
        struct Row { value: Option<()> }
        let unit: Option<()> = host::some_unit();
        print(unit);
        let row: Row = {value: unit};
        let text = json::encode(row);
        text == "{\"value\":null}";
    "#;
    let value =
        run_with_tag_only_unit(source, "host::some_unit").expect("canonical Some(()) must run");
    assert_eq!(value, vm::Value::Bool(true));
}

#[test]
fn tag_only_some_with_non_unit_schema_still_fails() {
    let source = "use host;\nlet value: Option<int> = host::some_bad();\nvalue;\n";
    let error = run_with_tag_only_unit(source, "host::some_bad")
        .expect_err("tag-only Some must not satisfy a non-Unit payload schema")
        .to_string();
    assert!(
        error.contains("Option Some payload") || error.contains("json unit"),
        "unexpected non-Unit tag-only failure: {error}"
    );
}
