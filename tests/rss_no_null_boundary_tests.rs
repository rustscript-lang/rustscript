#![cfg(feature = "runtime")]

use vm::compile_source;

fn rejection(source: &str) -> String {
    match compile_source(source) {
        Ok(_) => panic!("RSS should reject legacy null syntax: {source}"),
        Err(error) => error.to_string(),
    }
}

fn span_text<'a>(source: &'a str, error: &str) -> &'a str {
    let open = error
        .rfind('[')
        .unwrap_or_else(|| panic!("missing span in {error}"));
    let dots = error[open..]
        .find("..")
        .unwrap_or_else(|| panic!("missing span range in {error}"));
    let close = error[open + dots..]
        .find(']')
        .unwrap_or_else(|| panic!("missing span end in {error}"));
    let lo: usize = error[open + 1..open + dots]
        .parse()
        .unwrap_or_else(|_| panic!("bad span lo in {error}"));
    let hi: usize = error[open + dots + 2..open + dots + close]
        .parse()
        .unwrap_or_else(|_| panic!("bad span hi in {error}"));
    &source[lo..hi]
}

#[test]
fn rss_rejects_null_literal_with_migration_and_span() {
    let source = "null;";
    let error = rejection(source);
    assert!(
        error.contains("Option::None") && error.contains("()"),
        "{error}"
    );
    assert_eq!(span_text(source, &error), "null");
}

#[test]
fn rss_rejects_null_pattern_with_migration_and_span() {
    let source = "let value: Option<int> = Option::None; match value { null => 1, _ => 2 };";
    let error = rejection(source);
    assert!(
        error.contains("Option::None") && error.contains("()"),
        "{error}"
    );
    assert_eq!(span_text(source, &error), "null");
}

#[test]
fn rss_rejects_null_type_with_option_migration_and_span() {
    let source = "fn take(value: null) { 1; }";
    let error = rejection(source);
    assert!(error.contains("Option<T>"), "{error}");
    assert_eq!(span_text(source, &error), "null");
}

#[test]
fn rss_rejects_nullable_suffix_with_option_migration_and_span() {
    let source = "fn take(value: int?) { 1; }";
    let error = rejection(source);
    assert!(error.contains("Option<T>"), "{error}");
    assert_eq!(span_text(source, &error), "?");
}

#[test]
fn rss_rejects_null_map_key_with_span() {
    let source = "{ null: 1 };";
    let error = rejection(source);
    assert!(
        error.contains("Option::None") && error.contains("()"),
        "{error}"
    );
    assert_eq!(span_text(source, &error), "null");
}

fn eval(source: &str) -> vm::Value {
    let compiled = compile_source(source)
        .unwrap_or_else(|error| panic!("RSS JSON source should compile: {source}\n{error}"));
    let mut vm = vm::Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_fuel(100_000);
    assert_eq!(
        vm.run().expect("JSON program should run"),
        vm::VmStatus::Halted
    );
    vm.stack()
        .last()
        .cloned()
        .expect("JSON program should leave a value")
}

#[test]
fn json_option_null_decodes_as_none() {
    let value = eval(
        r#"
        use json;
        let decoded: Option<int> = json::decode::<Option<int>>("null");
        match decoded { Option::None => 0, Option::Some(value) => 1 + value };
        "#,
    );
    assert_eq!(value, vm::Value::Int(0));
}

#[test]
fn json_option_none_encodes_as_null() {
    let value = eval(
        r#"
        use json;
        let missing: Option<int> = Option::None;
        let encoded: string = json::encode::<Option<int>>(missing);
        encoded;
        "#,
    );
    assert_eq!(value, vm::Value::string("null"));
}

#[test]
fn println_option_returns_original_and_renders_none() {
    let source = r#"
        use json;
        let decoded: Option<int> = json::decode::<Option<int>>("null");
        let printed = println(decoded);
        match printed { Option::None => 0, Option::Some(value) => 1 + value };
    "#;
    let compiled = compile_source(source)
        .unwrap_or_else(|error| panic!("logical print should compile: {source}\n{error}"));
    let mut vm = vm::Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_fuel(100_000);
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink_lines = std::sync::Arc::clone(&lines);
    vm.set_runtime_print_sink(move |rendered| {
        sink_lines
            .lock()
            .expect("sink should be lockable")
            .push(rendered);
    });
    assert_eq!(
        vm.run().expect("logical print should run"),
        vm::VmStatus::Halted
    );
    let value = vm.stack().last().cloned().expect("print result");
    assert_eq!(value, vm::Value::Int(0));
    assert_eq!(
        lines.lock().expect("printed lines").as_slice(),
        ["None\n".to_string()]
    );
}

#[test]
fn rss_rejects_custom_host_unknown_return() {
    use std::sync::Arc;
    let catalog = Arc::new({
        let mut builder = vm::HostApiBuilder::new();
        builder.function(vm::HostFunctionSchema::with_return(
            "demo::unknown",
            Vec::new(),
            vm::HostTypeSchema::Unknown,
        ));
        builder.function(vm::HostFunctionSchema::with_return(
            "demo::flag",
            Vec::new(),
            vm::HostTypeSchema::Bool,
        ));
        builder.build().expect("catalog")
    });
    let rejected = vm::compile_source_with_flavor_and_options(
        "use demo; demo::unknown();",
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
    );
    let message = match rejected {
        Ok(_) => panic!("unknown host return must be rejected"),
        Err(error) => error.to_string(),
    };
    assert!(message.contains("dynamic or legacy-null"), "{message}");
    vm::compile_source_with_flavor_and_options(
        "use demo; demo::flag();",
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    )
    .expect("concrete host return");
}

#[test]
fn format_template_renders_option_from_checked_schema() {
    let source = r#"
        let value: Option<int> = Option::None;
        println("{}", value);
        1;
    "#;
    let compiled = compile_source(source)
        .unwrap_or_else(|error| panic!("format template should compile: {source}\n{error}"));
    let mut vm = vm::Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_fuel(100_000);
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink_lines = std::sync::Arc::clone(&lines);
    vm.set_runtime_print_sink(move |rendered| {
        sink_lines
            .lock()
            .expect("sink should be lockable")
            .push(rendered);
    });
    assert_eq!(
        vm.run().expect("format template should run"),
        vm::VmStatus::Halted
    );
    assert_eq!(
        lines.lock().expect("printed lines").as_slice(),
        ["None\n".to_string()]
    );
}

#[test]
fn type_of_option_is_shallow_option_label() {
    let source = r#"
        use json;
        let decoded: Option<int> = json::decode::<Option<int>>("null");
        let label: string = type(decoded);
        label;
    "#;
    let value = eval(source);
    assert_eq!(value, vm::Value::string("Option"));
}

#[test]
fn type_of_unit_is_unit_label() {
    let source = r#"
        let value: () = ();
        let label: string = type(value);
        label;
    "#;
    let value = eval(source);
    assert_eq!(value, vm::Value::string("()"));
}

#[test]
fn unannotated_decode_keeps_explicit_type_argument() {
    let source = r#"
        use json;
        let decoded = json::decode::<Option<int>>("null");
        let typed: Option<int> = decoded;
        typed;
    "#;
    let value = eval(source);
    assert!(vm::option::payload(&value).expect("option").0 == false);
}

#[test]
fn encode_infers_option_schema_without_type_arguments() {
    let source = r#"
        use json;
        let value: Option<int> = Option::None;
        let encoded = json::encode(value);
        encoded;
    "#;
    let value = eval(source);
    assert_eq!(value, vm::Value::string("null"));
}

#[test]
fn repl_result_schema_comes_from_the_expression_not_an_equal_local() {
    let compiled = vm::compile_source_for_repl_with_locals(
        "let other: Option<string> = Option::None; let result: Option<int> = Option::None; result;",
        &[],
    )
    .expect("repl compile");
    assert_eq!(
        compiled.result_schema,
        Some(vm::TypeSchema::Option(Box::new(vm::TypeSchema::Int)))
    );
}

#[test]
fn unit_map_insert_keeps_the_key() {
    let value = eval(
        r#"
        let mut values: map<()> = {};
        values["present"] = ();
        values.has("present");
        "#,
    );
    assert_eq!(value, vm::Value::Bool(true));
}

#[test]
fn typed_map_remove_returns_map_without_the_key() {
    let value = eval(
        r#"
        let mut values: map<int> = {keep: 1, drop: 2};
        let updated = values.remove("drop");
        updated.has("drop");
        "#,
    );
    assert_eq!(value, vm::Value::Bool(false));
}

fn print_catalog(
    params: Vec<vm::HostParamSchema>,
    returns: vm::HostTypeSchema,
) -> std::sync::Arc<vm::HostApiCatalog> {
    let mut builder = vm::HostApiBuilder::new();
    builder.function(vm::HostFunctionSchema::with_return(
        "print", params, returns,
    ));
    std::sync::Arc::new(builder.build().expect("print catalog"))
}

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

fn assert_matching_print_rejected(returns: vm::HostTypeSchema) {
    let catalog = print_catalog(
        vec![vm::HostParamSchema::value("value", vm::HostTypeSchema::Int)],
        returns,
    );
    let message = match compile_catalog("print(1);", &catalog) {
        Ok(_) => panic!("matching catalog print must not become the language intrinsic"),
        Err(error) => error.to_string(),
    };
    assert!(
        message.contains("dynamic or legacy-null"),
        "expected dynamic output rejection, got {message}"
    );
    assert!(
        !message.contains("print formatting requires"),
        "catalog identity must be resolved before intrinsic formatting sugar: {message}"
    );
}

#[test]
fn matching_catalog_print_unknown_is_rejected() {
    assert_matching_print_rejected(vm::HostTypeSchema::Unknown);
}

#[test]
fn matching_catalog_print_null_is_rejected() {
    assert_matching_print_rejected(vm::HostTypeSchema::Null);
}

#[test]
fn matching_catalog_print_optional_is_rejected() {
    assert_matching_print_rejected(vm::HostTypeSchema::Optional(Box::new(
        vm::HostTypeSchema::Int,
    )));
}

static MATCHING_BOOL_PRINT_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn matching_bool_print(_: &mut vm::Vm, args: &[vm::Value]) -> vm::VmResult<vm::CallOutcome> {
    MATCHING_BOOL_PRINT_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(args, &[vm::Value::Int(1)]);
    Ok(vm::CallOutcome::Return(vm::return_one(vm::Value::Bool(
        true,
    ))))
}

static TWO_INT_PRINT_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn two_int_print(_: &mut vm::Vm, args: &[vm::Value]) -> vm::VmResult<vm::CallOutcome> {
    TWO_INT_PRINT_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(args, &[vm::Value::Int(1), vm::Value::Int(2)]);
    Ok(vm::CallOutcome::Return(vm::return_one(vm::Value::Bool(
        false,
    ))))
}

fn run_bound_print(
    source: &str,
    catalog: &std::sync::Arc<vm::HostApiCatalog>,
    handler: vm::StaticHostFunction,
    counter: &std::sync::atomic::AtomicUsize,
) -> vm::Value {
    counter.store(0, std::sync::atomic::Ordering::SeqCst);
    let compiled = compile_catalog(source, catalog).unwrap_or_else(|error| {
        panic!("matching catalog print should keep its declared return: {error}")
    });
    let mut registry = vm::HostFunctionRegistry::new();
    let schema = vm::catalog_import_schemas(catalog, "print")
        .into_iter()
        .next()
        .expect("catalog print schema");
    registry
        .register_catalog_static(schema, handler)
        .expect("register matching print handler");
    let mut machine = vm::Vm::new(compiled.program.with_local_count(compiled.locals));
    machine.set_fuel(100_000);
    let sink_lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let recorded = std::sync::Arc::clone(&sink_lines);
    machine.set_runtime_print_sink(move |rendered| {
        recorded.lock().expect("sink").push(rendered);
    });
    registry
        .bind_vm_cached(&mut machine)
        .expect("bind matching print handler");
    assert_eq!(
        machine.run().expect("matching print should run"),
        vm::VmStatus::Halted
    );
    assert!(
        sink_lines.lock().expect("sink").is_empty(),
        "custom print must not fall through to the language print sink"
    );
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "custom print handler must actually run"
    );
    machine
        .stack()
        .last()
        .cloned()
        .expect("matching print should leave its return value")
}

#[test]
fn matching_catalog_print_bool_keeps_handler_and_return() {
    let catalog = print_catalog(
        vec![vm::HostParamSchema::value("value", vm::HostTypeSchema::Int)],
        vm::HostTypeSchema::Bool,
    );
    let value = run_bound_print(
        "let output: bool = print(1); output;",
        &catalog,
        matching_bool_print,
        &MATCHING_BOOL_PRINT_CALLS,
    );
    assert_eq!(value, vm::Value::Bool(true));
}

static EXPLICIT_BOOL_PRINT_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn explicit_bool_print(_: &mut vm::Vm, args: &[vm::Value]) -> vm::VmResult<vm::CallOutcome> {
    EXPLICIT_BOOL_PRINT_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(args, &[vm::Value::Int(1)]);
    Ok(vm::CallOutcome::Return(vm::return_one(vm::Value::Bool(
        true,
    ))))
}

#[test]
fn explicit_print_declaration_keeps_catalog_bool_identity() {
    let catalog = print_catalog(
        vec![vm::HostParamSchema::value("value", vm::HostTypeSchema::Int)],
        vm::HostTypeSchema::Bool,
    );
    let value = run_bound_print(
        "fn print(value: int) -> bool; let output: bool = print(1); output;",
        &catalog,
        explicit_bool_print,
        &EXPLICIT_BOOL_PRINT_CALLS,
    );
    assert_eq!(value, vm::Value::Bool(true));
}

#[test]
fn matching_two_int_catalog_print_is_not_format_sugar() {
    let catalog = print_catalog(
        vec![
            vm::HostParamSchema::value("left", vm::HostTypeSchema::Int),
            vm::HostParamSchema::value("right", vm::HostTypeSchema::Int),
        ],
        vm::HostTypeSchema::Bool,
    );
    let value = run_bound_print(
        "let output: bool = print(1, 2); output;",
        &catalog,
        two_int_print,
        &TWO_INT_PRINT_CALLS,
    );
    assert_eq!(value, vm::Value::Bool(false));
}

#[test]
fn nonmatching_catalog_print_keeps_language_print() {
    let catalog = print_catalog(
        vec![
            vm::HostParamSchema::value("left", vm::HostTypeSchema::Int),
            vm::HostParamSchema::value("right", vm::HostTypeSchema::Int),
        ],
        vm::HostTypeSchema::Bool,
    );
    let compiled = compile_catalog("print(1);", &catalog).unwrap_or_else(|error| {
        panic!("a nonmatching catalog overload must not ban language print: {error}")
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
