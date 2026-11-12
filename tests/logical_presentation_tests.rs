#![cfg(feature = "runtime")]

use std::sync::{Arc, Mutex};
use vm::{TypeSchema, Value, Vm, VmStatus, compile_source, compile_source_for_repl_with_state};

fn run_prints(source: &str) -> (Vec<String>, Vec<Value>) {
    let compiled = compile_source(source).unwrap_or_else(|error| panic!("{source}\n{error}"));
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_fuel(100_000);
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&lines);
    vm.set_runtime_print_sink(move |rendered| {
        sink.lock().expect("print sink").push(rendered);
    });
    assert_eq!(vm.run().expect(source), VmStatus::Halted);
    (lines.lock().expect("printed").clone(), vm.stack().to_vec())
}

#[test]
fn direct_option_print_uses_the_expression_schema() {
    let (printed, stack) = run_prints(
        r#"
        fn none() -> Option<int> { Option::None; }
        print(none());
        "#,
    );
    assert_eq!(printed, ["None".to_string()]);
    assert!(!stack.is_empty(), "print keeps the original value");
}

#[test]
fn reassigned_option_prints_the_later_value() {
    let (printed, _) = run_prints(
        r#"
        let mut value: Option<int> = Option::None;
        value = Option::Some(7);
        print(value);
        "#,
    );
    assert_eq!(printed, ["Some(7)".to_string()]);
}

#[test]
fn function_frame_prints_the_parameter_schema() {
    let (printed, _) = run_prints(
        r#"
        fn show(value: Option<int>) { print(value); }
        show(Option::Some(3));
        "#,
    );
    assert_eq!(printed, ["Some(3)".to_string()]);
}

#[test]
fn captured_option_prints_the_captured_schema() {
    let (printed, _) = run_prints(
        r#"
        let value: Option<int> = Option::Some(4);
        fn show() { print(value); }
        show();
        "#,
    );
    assert_eq!(printed, ["Some(4)".to_string()]);
}

#[test]
fn print_evaluates_a_logical_input_once() {
    let (printed, stack) = run_prints(
        r#"
        let mut n = 0;
        fn bump() -> int { n = n + 1; n; }
        let printed: Option<int> = print(Option::Some(bump()));
        match printed { Option::Some(value) => value, Option::None => 0 };
        "#,
    );
    assert_eq!(printed, ["Some(1)".to_string()]);
    assert_eq!(stack.last(), Some(&Value::Int(1)));
}

#[test]
fn type_of_option_and_result_uses_the_logical_label() {
    let (_, option_stack) = run_prints("let missing: Option<int> = Option::None; type(missing);");
    assert_eq!(option_stack.last(), Some(&Value::string("Option")));
    let (_, result_stack) =
        run_prints("let value: Result<int, string> = Result::Ok(1); type(value);");
    assert_eq!(result_stack.last(), Some(&Value::string("Result")));
}

#[test]
fn format_print_renders_option_payload() {
    let (printed, _) =
        run_prints("let missing: Option<int> = Option::None; print(\"{}\", missing);");
    assert_eq!(printed, ["None".to_string()]);
}

#[test]
fn user_print_keeps_its_own_dispatch() {
    let (printed, stack) = run_prints(
        r#"
        fn print(value: int) -> int { value + 1; }
        print(1);
        "#,
    );
    assert!(printed.is_empty(), "{printed:?}");
    assert_eq!(stack.last(), Some(&Value::Int(2)));
}

#[test]
fn repl_result_distinguishes_absence_unit_and_option() {
    let absent = compile_source_for_repl_with_state("let x = 1;", &[]).expect("let");
    assert!(absent.result_schema.is_none());

    let unit = compile_source_for_repl_with_state("();", &[]).expect("unit");
    assert!(matches!(unit.result_schema, Some(TypeSchema::Unit)));

    let option = compile_source_for_repl_with_state(
        "let missing: Option<int> = Option::None; missing;",
        &[],
    )
    .expect("option");
    assert!(matches!(option.result_schema, Some(TypeSchema::Option(_))));

    let trailing = compile_source_for_repl_with_state(
        "let missing: Option<int> = Option::None; missing; let x = 1;",
        &[],
    )
    .expect("trailing let");
    assert!(
        matches!(trailing.result_schema, Some(TypeSchema::Option(_))),
        "trailing let must not clear the previous expression schema: {:?}",
        trailing.result_schema
    );
}

#[test]
fn print_operand_keeps_checked_source_position_schema() {
    let source = "\
let value: Option<int> = Option::Some(1);
print(value);
let plain = { tag: false };
print(plain);
fn show(item: Option<int>) { print(item); }
show(value);
";
    let model = vm::analyze_source_from_string_with_options(
        "main.rss",
        source,
        vm::CompileSourceFileOptions::default(),
    )
    .unwrap_or_else(|error| panic!("presentation source should analyze: {error}"));
    assert!(
        model.diagnostics().is_empty(),
        "checked presentation source has diagnostics: {:?}",
        model.diagnostics()
    );
    let parsed = model
        .ir()
        .parsed_semantic_index
        .as_ref()
        .expect("parser semantic index");
    for name in ["value", "plain", "item"] {
        let site = parsed
            .local_refs
            .iter()
            .find(|site| site.name == name)
            .unwrap_or_else(|| panic!("missing source ref for {name}"));
        assert_eq!(
            &source[site.ident_span.lo..site.ident_span.hi],
            name,
            "{name} source span must be the identifier"
        );
        let schema = model
            .inferred_schema_at(vm::SourcePosition::new(0, site.ident_span.lo))
            .unwrap_or_else(|| panic!("missing checked schema at {name}"));
        match name {
            "plain" => assert!(
                matches!(schema, TypeSchema::Object(_)),
                "ordinary object must stay an object at its source position, got {schema:?}"
            ),
            _ => assert!(
                matches!(schema, TypeSchema::Option(_)),
                "{name} must stay Option at its print source position, got {schema:?}"
            ),
        }
    }
}
