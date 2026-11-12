#![cfg(feature = "runtime")]

use vm::{Value, Vm, VmStatus, compile_source};

fn eval(source: &str) -> Value {
    let compiled = compile_source(source).expect("source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_fuel(100_000);
    assert_eq!(
        vm.run().expect("program should run"),
        VmStatus::Halted,
        "program should halt"
    );
    vm.stack()
        .last()
        .cloned()
        .expect("program should leave a value")
}

#[test]
fn inferred_empty_map_keeps_unit_entry() {
    let source = r#"
        let mut values = {};
        values["present"] = ();
        values.has("present");
    "#;
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn record_factory_remove_is_rejected_at_admission() {
    let source = r#"
        struct Entry { value: int }
        fn make() -> Entry { {value: 7} }
        let broken = make().remove("value");
        broken.value;
    "#;
    let message = match compile_source(source) {
        Err(error) => error.to_string(),
        Ok(_) => panic!("record remove must be rejected"),
    };
    assert!(
        message.contains("E_MAP_REMOVE_RECORD"),
        "expected typed map removal guidance, got {message}"
    );
    assert!(
        message.contains("map<"),
        "expected map<T> guidance, got {message}"
    );
}

#[test]
fn recursive_named_decode_keeps_finite_option_chain() {
    let source = include_str!("fixtures/map_json/recursive_named_decode.rss");
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn missing_optional_named_field_decodes_as_none() {
    let source = include_str!("fixtures/map_json/optional_named_missing.rss");
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn missing_required_named_field_is_rejected() {
    let source = include_str!("fixtures/map_json/required_named_missing.rss");
    let compiled = compile_source(source).expect("required-field source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_fuel(100_000);
    let error = vm.run().expect_err("missing required id must fail");
    let message = error.to_string();
    assert!(
        message.contains("missing a required field"),
        "expected required-field rejection, got {message}"
    );
}

#[test]
fn unit_field_encodes_as_null() {
    let source = include_str!("fixtures/map_json/unit_field_encode.rss");
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn option_unit_field_some_encodes_present_null() {
    let source = include_str!("fixtures/map_json/option_unit_field_encode.rss");
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn generic_unit_insert_keeps_entry() {
    let source = include_str!("fixtures/map_json/generic_unit_insert.rss");
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn inferred_unit_insert_survives_vmbc_reload() {
    let source = "let mut values = {}; values[\"present\"] = (); values.has(\"present\");";
    let compiled = compile_source(source).expect("source should compile");
    let wire = vm::vmbc::encode_program(&compiled.program).expect("program should encode");
    let reloaded = vm::vmbc::decode_program(&wire).expect("program should reload");
    let mut vm = Vm::new(reloaded.with_local_count(compiled.locals));
    vm.set_fuel(100_000);
    assert_eq!(vm.run().expect("reloaded program should run"), VmStatus::Halted);
    assert_eq!(vm.stack().last().cloned(), Some(Value::Bool(true)));
}
