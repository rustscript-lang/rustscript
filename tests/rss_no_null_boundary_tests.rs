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
    let compiled = compile_source(source).unwrap_or_else(|error| {
        panic!("RSS JSON source should compile: {source}\n{error}")
    });
    let mut vm = vm::Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_fuel(100_000);
    assert_eq!(vm.run().expect("JSON program should run"), vm::VmStatus::Halted);
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
    let compiled = compile_source(source).unwrap_or_else(|error| {
        panic!("logical print should compile: {source}\n{error}")
    });
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
    assert!(
        message.contains("dynamic or legacy-null"),
        "{message}"
    );
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
    let compiled = compile_source(source).unwrap_or_else(|error| {
        panic!("format template should compile: {source}\n{error}")
    });
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
