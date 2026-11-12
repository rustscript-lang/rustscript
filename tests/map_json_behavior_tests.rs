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
fn option_map_callback_encodes_nested_payload_as_natural_json() {
    let source = r#"
        use json;
        let src: Option<Option<int>> = Option::Some(Option::Some(1));
        let encoded = src.map(|item| json::encode(item));
        match encoded {
            Option::Some(text) => text == "1",
            Option::None => false,
        };
    "#;
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn result_map_callback_encodes_nested_payload_as_natural_json() {
    let source = r#"
        use json;
        let src: Result<Option<int>, string> = Result::Ok(Option::Some(1));
        let encoded = src.map(|item| json::encode(item));
        match encoded {
            Result::Ok(text) => text == "1",
            Result::Err(error) => error == "",
        };
    "#;
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn option_and_then_callback_encodes_nested_payload_as_natural_json() {
    let source = r#"
        use json;
        let src: Option<Option<int>> = Option::Some(Option::Some(1));
        let encoded = src.and_then(|item| Option::Some(json::encode(item)));
        match encoded {
            Option::Some(text) => text == "1",
            Option::None => false,
        };
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
    assert_eq!(
        vm.run().expect("reloaded program should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack().last().cloned(), Some(Value::Bool(true)));
}

#[test]
fn closure_contextual_unit_map_store_keeps_entry() {
    let source = r#"
        let set: fn(map<()>) -> bool = |m| if true => { m["x"] = (); m.has("x") } else => { false };
        set({});
    "#;
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn pattern_moved_unit_map_store_keeps_entry() {
    let source = r#"let maybe: Option<map<()>> = Option::Some({}); let ok = match maybe { Option::Some(m) => if true => { let mut values = m; values["x"] = (); values.has("x") } else => { false }, Option::None => false }; ok;"#;
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn option_carrier_index_store_is_rejected() {
    let source = r#"let mut o: Option<int> = Option::Some(1); o["x"] = ();"#;
    let message = match compile_source(source) {
        Err(error) => error.to_string(),
        Ok(_) => panic!("Option index store must be rejected"),
    };
    assert!(
        message.contains("cannot store into an Option or Result"),
        "expected carrier refusal, got {message}"
    );
}

#[test]
fn result_carrier_index_store_is_rejected() {
    let source = r#"let mut r: Result<int, string> = Result::Ok(1); r["x"] = 2;"#;
    let message = match compile_source(source) {
        Err(error) => error.to_string(),
        Ok(_) => panic!("Result index store must be rejected"),
    };
    assert!(
        message.contains("cannot store into an Option or Result"),
        "expected carrier refusal, got {message}"
    );
}

#[test]
fn option_carrier_stays_unmodified_without_store() {
    let source = r#"let o: Option<int> = Option::Some(1); o.has("x");"#;
    assert_eq!(eval(source), Value::Bool(false));
}

#[test]
fn result_pattern_moved_unit_map_store_keeps_entry() {
    let source = r#"let maybe: Result<map<()>, string> = Result::Ok({}); let ok = match maybe { Result::Ok(m) => if true => { let mut values = m; values["x"] = (); values.has("x") } else => { false }, Result::Err(_) => false }; ok;"#;
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn loop_unit_store_keeps_entry_after_iterations() {
    let source = r#"let mut values: map<()> = {}; let mut n = 0; while n < 2 { values["x"] = (); n = n + 1; } values.has("x");"#;
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn zero_iteration_loop_does_not_invent_unit_entry() {
    let source = r#"let mut values: map<()> = {}; let mut n = 0; while n < 0 { values["x"] = (); n = n + 1; } values.has("x");"#;
    assert_eq!(eval(source), Value::Bool(false));
}

#[test]
fn typed_map_remove_drops_key() {
    let source = r#"let mut values: map<int> = {}; values["x"] = 1; let updated = values.remove("x"); updated.has("x");"#;
    assert_eq!(eval(source), Value::Bool(false));
}

fn reject(source: &str, needle: &str) {
    let message = match compile_source(source) {
        Err(error) => error.to_string(),
        Ok(_) => panic!("source must be rejected: {source}"),
    };
    assert!(
        message.contains(needle),
        "expected {needle} in diagnostic, got {message}"
    );
}

fn call_indexes(program: &vm::Program, builtin: vm::BuiltinFunction) -> usize {
    let index = builtin.call_index().to_le_bytes();
    let mut count = 0;
    let code = &program.code;
    let mut offset = 0;
    while offset + 2 < code.len() {
        if code[offset] == vm::OpCode::Call as u8 && code[offset + 1..offset + 3] == index {
            count += 1;
        }
        offset += 1;
    }
    count
}

#[test]
fn factory_returned_map_keeps_unit_entry() {
    let source = r#"
        fn make() {
            let mut values = {};
            values
        }
        let mut values = make();
        values["present"] = ();
        values.has("present");
    "#;
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn object_field_update_keeps_unit_entry() {
    let source = r#"
        let mut row = {};
        row.present = ();
        row.has("present");
    "#;
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn named_field_update_keeps_unit_entry() {
    let source = r#"
        struct Entry { value: () }
        let mut row: Entry = {value: ()};
        row.value = ();
        row.has("value");
    "#;
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn closure_capture_unit_store_keeps_entry() {
    let source = r#"
        let mut values = {};
        let write = |flag| if flag => { values["captured"] = (); values.has("captured") } else => { false };
        write(true);
    "#;
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn source_order_reassignment_keeps_latest_value() {
    let source = r#"
        let mut values: map<int> = {};
        values["k"] = 1;
        values["k"] = 2;
        values["k"];
    "#;
    assert_eq!(eval(source), Value::Int(2));
}

#[test]
fn branch_join_keeps_only_taken_unit_entry() {
    let source = r#"
        let mut values = {};
        let flag = true;
        if flag {
            values["taken"] = ();
        } else {
            values["other"] = ();
        }
        values.has("taken") && values.has("other") == false;
    "#;
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn inferred_loop_unit_store_selects_checked_insert() {
    let source = r#"
        let mut values = {};
        let mut n = 0;
        while n < 2 {
            values["x"] = ();
            n = n + 1;
        }
        values.has("x");
    "#;
    let compiled = compile_source(source).expect("inferred loop should compile");
    assert!(
        call_indexes(&compiled.program, vm::BuiltinFunction::MapEdit) >= 1,
        "unannotated unit loop must be rewritten by the checked collector"
    );
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_fuel(100_000);
    assert_eq!(
        vm.run().expect("inferred loop should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack().last().cloned(), Some(Value::Bool(true)));
}

#[test]
fn inferred_zero_iteration_loop_does_not_invent_unit_entry() {
    let source = r#"
        let mut values = {};
        let mut n = 0;
        while n < 0 {
            values["x"] = ();
            n = n + 1;
        }
        values.has("x");
    "#;
    assert_eq!(eval(source), Value::Bool(false));
}

#[test]
fn annotated_int_loop_store_keeps_set_fast_path() {
    let source = r#"
        let mut values: map<int> = {};
        let mut n = 0;
        while n < 2 {
            values["k"] = n;
            n = n + 1;
        }
        values["k"];
    "#;
    let compiled = compile_source(source).expect("int map loop should compile");
    assert_eq!(
        call_indexes(&compiled.program, vm::BuiltinFunction::MapEdit),
        0,
        "proven int store must stay Set, not parser MapEdit"
    );
    assert!(
        call_indexes(&compiled.program, vm::BuiltinFunction::Set) >= 1,
        "proven int store must emit Set"
    );
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_fuel(100_000);
    assert_eq!(vm.run().expect("int map loop should run"), VmStatus::Halted);
    assert_eq!(vm.stack().last().cloned(), Some(Value::Int(1)));
}

#[test]
fn keys_length_and_readback_follow_unit_insert() {
    let source = r#"
        let mut values = {};
        values["a"] = ();
        values["b"] = ();
        let count = values.length;
        let keys = values.keys;
        let key_count = keys.length;
        count == 2 && key_count == 2 && values.has("a") && values.has("b");
    "#;
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn missing_remove_is_noop_and_alias_stays_independent() {
    let source = r#"
        let mut values: map<int> = {keep: 1, drop: 2};
        let alias = values.copy();
        let updated = values.remove("missing");
        let removed = updated.remove("drop");
        removed.has("drop") == false && removed.has("keep") && alias.has("drop") && alias.has("keep");
    "#;
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn discarded_remove_leaves_original_alias() {
    let source = r#"
        let mut values: map<int> = {drop: 1};
        let alias = values.copy();
        values.remove("drop");
        alias.has("drop");
    "#;
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn factory_map_remove_returns_updated_map() {
    let source = r#"
        fn make() -> map<int> {
            let mut values: map<int> = {k: 1};
            values
        }
        let updated = make().remove("k");
        updated.has("k");
    "#;
    assert_eq!(eval(source), Value::Bool(false));
}

#[test]
fn object_literal_remove_is_rejected() {
    reject(
        r#"let row = {value: 1}; row.remove("value");"#,
        "E_MAP_REMOVE_RECORD",
    );
}

#[test]
fn option_and_result_remove_are_rejected() {
    reject(
        r#"let carrier: Option<int> = Option::Some(1); carrier.remove("x");"#,
        "E_MAP_REMOVE_RECORD",
    );
    reject(
        r#"let carrier: Result<int, string> = Result::Ok(1); carrier.remove("x");"#,
        "E_MAP_REMOVE_RECORD",
    );
}

#[test]
fn option_non_unit_and_result_unit_index_stores_are_rejected() {
    reject(
        r#"let mut carrier: Option<int> = Option::Some(1); carrier["x"] = 2;"#,
        "cannot store into an Option or Result",
    );
    reject(
        r#"let mut carrier: Result<int, string> = Result::Ok(1); carrier["x"] = ();"#,
        "cannot store into an Option or Result",
    );
}

#[test]
fn option_and_result_dot_stores_are_rejected() {
    reject(
        r#"let mut carrier: Option<int> = Option::Some(1); carrier.x = ();"#,
        "cannot store into an Option or Result",
    );
    reject(
        r#"let mut carrier: Result<int, string> = Result::Ok(1); carrier.x = 2;"#,
        "cannot store into an Option or Result",
    );
}

#[test]
fn option_and_result_match_leave_payload_unmodified() {
    let source = r#"
        let option_value: Option<int> = Option::Some(4);
        let result_value: Result<int, string> = Result::Ok(5);
        let option_ok = match option_value {
            Option::Some(value) => value,
            Option::None => 0
        };
        let result_ok = match result_value {
            Result::Ok(value) => value,
            Result::Err(_) => 0
        };
        option_ok == 4 && result_ok == 5;
    "#;
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn option_map_combinator_keeps_payload() {
    let source = r#"
        let mapped = Option::Some(3).map(|value| value + 1);
        let shown = match mapped {
            Option::Some(value) => value,
            Option::None => 0
        };
        shown;
    "#;
    assert_eq!(eval(source), Value::Int(4));
}

#[test]
fn array_and_int_controls_are_not_map_unit_stores() {
    let source = r#"
        let mut items = [1, 2];
        items[0] = 9;
        let mut n = 1;
        n = n + 1;
        items[0] == 9 && items.length == 2 && n == 2;
    "#;
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn index_key_and_value_evaluate_once() {
    let source = r#"
        let mut values: map<int> = {};
        let mut hits = 0;
        fn key() -> string { hits = hits + 1; "k" }
        fn payload() -> int { hits = hits + 1; 7 }
        values[key()] = payload();
        hits == 2 && values["k"] == 7;
    "#;
    assert_eq!(eval(source), Value::Bool(true));
}

fn fail(source: &str) -> String {
    let compiled = compile_source(source).expect("source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_fuel(2_000_000);
    vm.run().expect_err("program should fail").to_string()
}

fn reload_eval(source: &str) -> Value {
    let compiled = compile_source(source).expect("source should compile");
    let wire = vm::vmbc::encode_program(&compiled.program).expect("program should encode");
    let reloaded = vm::vmbc::decode_program(&wire).expect("program should reload");
    let mut vm = Vm::new(reloaded.with_local_count(compiled.locals));
    vm.set_fuel(100_000);
    assert_eq!(
        vm.run().expect("reloaded program should run"),
        VmStatus::Halted
    );
    vm.stack().last().cloned().expect("reloaded value")
}

#[test]
fn unit_scalar_array_and_map_round_trip() {
    let source = r#"
        use json;
        let scalar: () = ();
        let items: [()] = [()];
        let mut values: map<()> = {};
        values["k"] = ();
        json::encode(scalar) == "null"
            && json::encode(items) == "[null]"
            && json::encode(values) == "{\"k\":null}"
            && json::decode::<()>("null") == ()
            && json::decode::<[()]>("[null]").length == 1
            && json::decode::<map<()>>("{\"k\":null}").has("k");
    "#;
    assert_eq!(eval(source), Value::Bool(true));
    assert_eq!(reload_eval(source), Value::Bool(true));
}

#[test]
fn option_unit_field_none_omits_and_some_encodes_null() {
    let source = r#"
        use json;
        struct Payload { value: Option<()> }
        let absent: Payload = {value: Option::None};
        let present: Payload = {value: Option::Some(())};
        let absent_text = json::encode(absent);
        let present_text = json::encode(present);
        let absent_back: Payload = json::decode::<Payload>(absent_text);
        let present_back: Payload = json::decode::<Payload>(present_text);
        let absent_ok = match absent_back.value { Option::None => absent_text == "{}", Option::Some(_) => false };
        let present_ok = match present_back.value { Option::Some(_) => present_text == "{\"value\":null}", Option::None => false };
        absent_ok && present_ok;
    "#;
    assert_eq!(eval(source), Value::Bool(true));
    assert_eq!(reload_eval(source), Value::Bool(true));
}

#[test]
fn option_int_field_distinguishes_none_zero_false_and_empty() {
    let source = r#"
        use json;
        struct Row { n: Option<int>, flag: Option<bool>, text: Option<string> }
        let missing: Row = json::decode::<Row>("{}");
        let explicit: Row = json::decode::<Row>("{\"n\":0,\"flag\":false,\"text\":\"\"}");
        let encoded_missing = json::encode(json::decode::<Row>("{}"));
        let encoded_explicit = json::encode(json::decode::<Row>("{\"n\":0,\"flag\":false,\"text\":\"\"}"));
        let missing_ok = match missing.n {
            Option::None => match missing.flag {
                Option::None => match missing.text { Option::None => true, Option::Some(_) => false },
                Option::Some(_) => false
            },
            Option::Some(_) => false
        };
        let explicit_ok = match explicit.n {
            Option::Some(number) => number == 0 && match explicit.flag {
                Option::Some(flag) => flag == false && match explicit.text { Option::Some(text) => text == "", _ => false },
                _ => false
            },
            _ => false
        };
        missing_ok && explicit_ok && encoded_missing == "{}" && encoded_explicit == "{\"flag\":false,\"n\":0,\"text\":\"\"}";
    "#;
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn extra_named_key_drops_and_typed_map_keeps_keys() {
    let source = r#"
        use json;
        struct Row { id: int }
        let encoded = json::encode(json::decode::<Row>("{\"id\":1,\"extra\":9}"));
        let row: Row = json::decode::<Row>("{\"id\":1,\"extra\":9}");
        let values: map<int> = json::decode::<map<int>>("{\"keep\":1,\"also\":2}");
        row.id == 1 && encoded == "{\"id\":1}" && values.has("keep") && values.has("also") && values["keep"] == 1;
    "#;
    assert_eq!(eval(source), Value::Bool(true));
}

#[test]
fn mutual_recursion_and_generic_substitution_round_trip() {
    let source = r#"
        use json;
        struct Left { n: int, right: Option<Right> }
        struct Right { n: int, left: Option<Left> }
        struct Box<T> { value: T }
        let decoded: Left = json::decode::<Left>("{\"n\":1,\"right\":{\"n\":2,\"left\":null}}");
        let encoded = json::encode(decoded);
        let again: Left = json::decode::<Left>(encoded);
        let left_ok = match again.right {
            Option::Some(right) => right.n == 2 && match right.left { Option::None => true, Option::Some(_) => false },
            Option::None => false
        };
        let boxed: Box<int> = json::decode::<Box<int>>("{\"value\":4}");
        left_ok && again.n == 1 && boxed.value == 4 && encoded == "{\"n\":1,\"right\":{\"n\":2}}";
    "#;
    assert_eq!(eval(source), Value::Bool(true));
    assert_eq!(reload_eval(source), Value::Bool(true));
}

#[test]
fn required_nested_field_fails_when_root_fields_are_present() {
    let source = r#"
        use json;
        struct Inner { id: int }
        struct Outer { ok: int, inner: Inner }
        let decoded: Outer = json::decode::<Outer>("{\"ok\":1,\"inner\":{}}");
        decoded.ok;
    "#;
    let message = fail(source);
    assert!(
        message.contains("missing a required field"),
        "expected nested required-field rejection, got {message}"
    );
}

#[test]
fn value_position_option_unit_and_nested_option_reject_before_payload() {
    let unit =
        fail(r#"use json; let decoded: Option<()> = json::decode::<Option<()>>("null"); decoded;"#);
    let nested = fail(
        r#"use json; let decoded: Option<Option<int>> = json::decode::<Option<Option<int>>>("null"); decoded;"#,
    );
    let empty_map = fail(
        r#"use json; let decoded: map<Option<()>> = json::decode::<map<Option<()>>>("{}"); decoded;"#,
    );
    for message in [unit, nested, empty_map] {
        assert!(
            message.contains("value-position Option") || message.contains("null-ambiguous"),
            "expected payload-independent refusal, got {message}"
        );
    }
}

#[test]
fn duplicate_malformed_and_nonfinite_json_are_rejected() {
    let duplicate = fail(
        r#"use json; let row: map<int> = json::decode::<map<int>>("{\"k\":1,\"k\":2}"); row;"#,
    );
    let malformed = fail(
        r#"use json; let value: Option<int> = json::decode::<Option<int>>("{\"value\":1}"); value;"#,
    );
    let nonfinite = fail(r#"use json; let value: float = 1.0 / 0.0; json::encode(value);"#);
    assert!(duplicate.contains("duplicate"), "got {duplicate}");
    assert!(
        malformed.contains("json")
            || malformed.contains("TypeMismatch")
            || malformed.contains("bool")
            || malformed.contains("int"),
        "malformed carrier must not become Some(0), got {malformed}"
    );
    assert!(
        nonfinite.contains("NaN") || nonfinite.contains("infinity") || nonfinite.contains("number"),
        "got {nonfinite}"
    );
}

#[test]
fn json_depth_and_node_budgets_fail() {
    let mut nested = "null".to_string();
    for _ in 0..70 {
        nested = format!("{{\\\"next\\\":{nested}}}");
    }
    let deep = format!(
        "use json; struct Node {{ next: Option<Node> }} let decoded: Node = json::decode::<Node>(\"{nested}\"); decoded;"
    );
    let deep_message = fail(&deep);
    assert!(
        deep_message.contains("nesting budget"),
        "got {deep_message}"
    );

    let wide = format!("[{}]", vec!["0"; 20_000].join(","));
    let wide_source =
        format!("use json; let values: [int] = json::decode::<[int]>(\"{wide}\"); values.length;");
    let wide_message = fail(&wide_source);
    assert!(wide_message.contains("node budget"), "got {wide_message}");
}
