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

fn namespaced_print_catalog(
    name: &str,
    params: Vec<vm::HostParamSchema>,
    returns: vm::HostTypeSchema,
) -> std::sync::Arc<vm::HostApiCatalog> {
    let mut builder = vm::HostApiBuilder::new();
    builder.function(vm::HostFunctionSchema::with_return(name, params, returns));
    std::sync::Arc::new(builder.build().expect("namespaced print catalog"))
}

fn run_bound_catalog_function(
    source: &str,
    catalog: &std::sync::Arc<vm::HostApiCatalog>,
    catalog_name: &str,
    handler: vm::StaticHostFunction,
    counter: &std::sync::atomic::AtomicUsize,
) -> vm::Value {
    counter.store(0, std::sync::atomic::Ordering::SeqCst);
    let compiled = compile_catalog(source, catalog).unwrap_or_else(|error| {
        panic!("imported catalog call should keep its declared target: {error}")
    });
    let mut registry = vm::HostFunctionRegistry::new();
    let schema = vm::catalog_import_schemas(catalog, catalog_name)
        .into_iter()
        .next()
        .expect("catalog schema");
    registry
        .register_catalog_static(schema, handler)
        .expect("register catalog handler");
    let mut machine = vm::Vm::new(compiled.program.with_local_count(compiled.locals));
    machine.set_fuel(100_000);
    let sink_lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let recorded = std::sync::Arc::clone(&sink_lines);
    machine.set_runtime_print_sink(move |rendered| {
        recorded.lock().expect("sink").push(rendered);
    });
    registry
        .bind_vm_cached(&mut machine)
        .expect("bind catalog handler");
    assert_eq!(
        machine.run().expect("catalog call should run"),
        vm::VmStatus::Halted
    );
    assert!(
        sink_lines.lock().expect("sink").is_empty(),
        "the selected catalog call must not fall through to language print"
    );
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "selected catalog handler must actually run"
    );
    machine
        .stack()
        .last()
        .cloned()
        .expect("catalog call should leave its return value")
}

fn bool_print_catalog_for(catalog_name: &str) -> std::sync::Arc<vm::HostApiCatalog> {
    namespaced_print_catalog(
        catalog_name,
        vec![vm::HostParamSchema::value("value", vm::HostTypeSchema::Int)],
        vm::HostTypeSchema::Bool,
    )
}

static EXPLICIT_IMPORTED_PRINT_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn explicit_imported_bool_print(
    _: &mut vm::Vm,
    args: &[vm::Value],
) -> vm::VmResult<vm::CallOutcome> {
    EXPLICIT_IMPORTED_PRINT_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(args, &[vm::Value::Int(1)]);
    Ok(vm::CallOutcome::Return(vm::return_one(vm::Value::Bool(
        true,
    ))))
}

#[test]
fn explicit_imported_catalog_print_keeps_handler_identity() {
    let catalog = bool_print_catalog_for("demo::print");
    let value = run_bound_catalog_function(
        "use demo::{print};\nlet output: bool = print(1);\noutput;",
        &catalog,
        "demo::print",
        explicit_imported_bool_print,
        &EXPLICIT_IMPORTED_PRINT_CALLS,
    );
    assert_eq!(value, vm::Value::Bool(true));
}

static WILDCARD_IMPORTED_PRINT_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn wildcard_imported_bool_print(
    _: &mut vm::Vm,
    args: &[vm::Value],
) -> vm::VmResult<vm::CallOutcome> {
    WILDCARD_IMPORTED_PRINT_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(args, &[vm::Value::Int(1)]);
    Ok(vm::CallOutcome::Return(vm::return_one(vm::Value::Bool(
        true,
    ))))
}

#[test]
fn wildcard_imported_catalog_print_keeps_handler_identity() {
    let catalog = bool_print_catalog_for("demo::print");
    let value = run_bound_catalog_function(
        "use demo::*;\nlet output: bool = print(1);\noutput;",
        &catalog,
        "demo::print",
        wildcard_imported_bool_print,
        &WILDCARD_IMPORTED_PRINT_CALLS,
    );
    assert_eq!(value, vm::Value::Bool(true));
}

static NAMESPACED_PRINTLN_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn namespaced_bool_println(_: &mut vm::Vm, args: &[vm::Value]) -> vm::VmResult<vm::CallOutcome> {
    NAMESPACED_PRINTLN_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(args, &[vm::Value::Int(1)]);
    Ok(vm::CallOutcome::Return(vm::return_one(vm::Value::Bool(
        true,
    ))))
}

#[test]
fn imported_catalog_println_keeps_handler_identity() {
    let catalog = bool_print_catalog_for("demo::println");
    let value = run_bound_catalog_function(
        "use demo::{println};\nlet output: bool = println(1);\noutput;",
        &catalog,
        "demo::println",
        namespaced_bool_println,
        &NAMESPACED_PRINTLN_CALLS,
    );
    assert_eq!(value, vm::Value::Bool(true));
}

#[test]
fn explicit_imported_catalog_print_arity_mismatch_is_a_host_error() {
    let catalog = namespaced_print_catalog(
        "demo::print",
        vec![
            vm::HostParamSchema::value("left", vm::HostTypeSchema::Int),
            vm::HostParamSchema::value("right", vm::HostTypeSchema::Int),
        ],
        vm::HostTypeSchema::Bool,
    );
    let error = match compile_catalog("use demo::{print};\nprint(1);", &catalog) {
        Ok(_) => panic!("an explicit imported print target must keep its declared arity"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("no overload with 1 argument"),
        "explicit imported target must report its own arity contract: {error}"
    );
}

#[test]
fn wildcard_namespace_without_matching_print_keeps_language_print() {
    let catalog = namespaced_print_catalog(
        "demo::other",
        vec![vm::HostParamSchema::value("value", vm::HostTypeSchema::Int)],
        vm::HostTypeSchema::Bool,
    );
    let compiled = compile_catalog("use demo::*;\nprint(1);", &catalog).unwrap_or_else(|error| {
        panic!("wildcard import without print must keep language print: {error}")
    });
    let mut machine = vm::Vm::new(compiled.program.with_local_count(compiled.locals));
    machine.set_fuel(100_000);
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let recorded = std::sync::Arc::clone(&lines);
    machine.set_runtime_print_sink(move |rendered| {
        recorded.lock().expect("sink").push(rendered);
    });
    assert_eq!(machine.run().expect("language print"), vm::VmStatus::Halted);
    assert_eq!(lines.lock().expect("sink").as_slice(), ["1".to_string()]);
}
