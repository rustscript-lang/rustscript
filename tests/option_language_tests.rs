#![cfg(feature = "runtime")]

use vm::{Vm, VmStatus, compile_source};

#[test]
fn non_rss_dialect_retains_legacy_optional_match_patterns() {
    struct Legacy;
    impl vm::ParserDialect for Legacy {}
    vm::parse_source_with_dialect(
        "let value = 1; match value { Some(Number) => 1, None => 0, _ => -1 };",
        &Legacy,
        vm::SharedParserOptions::default(),
    )
    .expect("non-RSS type patterns remain parseable");
}

fn eval(source: &str) -> vm::Value {
    let compiled = compile_source(source).expect("Option source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_fuel(100_000);
    assert_eq!(
        vm.run().expect("Option program should run"),
        VmStatus::Halted
    );
    vm.stack()
        .last()
        .cloned()
        .expect("Option program should leave a value")
}

fn reject(source: &str) -> String {
    match compile_source(source) {
        Ok(_) => panic!("Option source should be rejected: {source}"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn bare_option_constructors_infer_and_match_logical_options() {
    let source = r#"
        let a: Option<int> = Some(1);
        let b: Option<int> = None;
        let c = Some("x");
        let x = match a { Some(value) => value, None => -1 };
        let y = match b { Some(value) => value, None => 2 };
        let z = match c { Some(TypeName) => TypeName, None => "missing" };
        x + y + z.length - 1;
    "#;
    assert_eq!(eval(source), vm::Value::Int(3));
}

#[test]
fn bare_option_names_are_reserved_but_members_and_keys_remain_available() {
    for source in ["let Some = 1;", "let None = 1;", "fn Some() { 1 }"] {
        assert!(!reject(source).is_empty(), "{source}");
    }
    compile_source("let obj = { None: 1 }; obj.None; let map = {\"None\": 1}; map;")
        .expect("member and quoted map key remain valid");
    compile_source("struct Item { None: int } let item: Item = { None: 1 }; item.None;")
        .expect("named field remains valid");
    compile_source(
        "struct Item { None: int } let mut item: Item = { None: 1 }; item.None = 2; item.None;",
    )
    .expect("member assignment remains valid");
}

#[test]
fn bare_option_constructor_patterns_reject_invalid_arity_and_missing_arms() {
    for pattern in ["Some()", "Some(a, b)", "None(x)"] {
        let source = format!(
            "let value: Option<int> = Some(1); match value {{ {pattern} => 0, None => 1 }};"
        );
        assert!(!reject(&source).is_empty(), "{pattern}");
    }
    let missing = reject("let value: Option<int> = Some(1); match value { Some(x) => x };");
    assert!(missing.contains("None"), "{missing}");
    let non_option = reject("let value = 1; match value { Some(x) => x, None => 0 };");
    assert!(non_option.contains("Option<T>"), "{non_option}");
    let duplicate = reject(
        "let value: Option<int> = Some(1); match value { Some(x) => x, Some(y) => y, None => 0 };",
    );
    assert!(duplicate.contains("duplicate"), "{duplicate}");
    assert!(!reject("let value: Option<int> = Option::Some(1);").is_empty());
    assert!(
        !reject(
            "let value: Option<int> = Some(1); match value { Option::Some(x) => x, None => 0 };"
        )
        .is_empty()
    );
}

#[test]
fn nested_option_states_are_constructed_matched_and_distinct() {
    let source = r#"
        let none: Option<Option<int>> = None;
        let some_none: Option<Option<int>> = Some(None);
        let some_some: Option<Option<int>> = Some(Some(0));
        let output = match none {
            Some(inner) => match inner { Some(v) => 10, None => 11 },
            None => 0,
        };
        let output2 = match some_none {
            Some(inner) => match inner { Some(v) => 20 + v, None => 21 },
            None => 30,
        };
        let output3 = match some_some {
            Some(inner) => match inner { Some(v) => 40 + v, None => 41 },
            None => 50,
        };
        output + output2 + output3;
    "#;
    assert_eq!(eval(source), vm::Value::Int(61));
}

#[test]
fn unit_none_some_unit_false_zero_and_empty_string_are_distinct() {
    let source = r#"
        let unit_none: Option<()> = None;
        let unit_some: Option<()> = Some(());
        let none_bool: Option<bool> = None;
        let some_false: Option<bool> = Some(false);
        let none_int: Option<int> = None;
        let some_zero: Option<int> = Some(0);
        let none_string: Option<string> = None;
        let some_empty: Option<string> = Some("");
        let a = match unit_none { Some(u) => 1, None => 2 };
        let b = match unit_some { Some(u) => 4, None => 8 };
        let c = match none_bool { Some(v) => 16, None => 32 };
        let d = match some_false { Some(v) => 64, None => 128 };
        let e = match none_int { Some(v) => 1, None => 2 };
        let f = match some_zero { Some(v) => 4, None => 8 };
        let g = match none_string { Some(v) => 16, None => 32 };
        let h = match some_empty { Some(v) => 64, None => 128 };
        a + b + c + d + e + f + g + h;
    "#;
    assert_eq!(eval(source), vm::Value::Int(204));
}

#[test]
fn nested_result_option_and_option_result_compose() {
    let source = r#"
        let option_result: Option<Result<int, string>> = Some(Result::Ok(3));
        let absent: Option<Result<int, string>> = None;
        let result_option: Result<Option<int>, string> = Result::Ok(Some(4));
        let result_absent: Result<Option<int>, string> = Result::Ok(None);
        let first = match option_result {
            Some(result) => match result { Result::Ok(value) => value, Result::Err(error) => -1 },
            None => -2,
        };
        let second = match absent {
            Some(result) => 100,
            None => 10,
        };
        let third = match result_option {
            Result::Ok(option) => match option { Some(value) => value, None => -4 },
            Result::Err(error) => -8,
        };
        let fourth = match result_absent {
            Result::Ok(option) => match option { Some(value) => 100, None => 20 },
            Result::Err(error) => -16,
        };
        first + second + third + fourth;
    "#;
    assert_eq!(eval(source), vm::Value::Int(37));
}

#[test]
fn named_array_tuple_and_callable_payloads_keep_their_schemas() {
    let source = r#"
        struct Point { x: int }
        fn add1(value: int) -> int { value + 1 }
        let named: Option<Point> = Some({ x: 5 });
        let array: Option<[Option<int>, string]> = Some([Some(1), "tag"]);
        let callable: Option<fn(int) -> int> = Some(add1);
        let x = match named { Some(value) => value.x, None => -1 };
        let inner = match array {
            Some(values) => match values[0] { Some(v) => v, None => -2 },
            None => -4,
        };
        let applied = match callable { Some(f) => f(6), None => -8 };
        x + inner + applied;
    "#;
    assert_eq!(eval(source), vm::Value::Int(13));
}

#[test]
fn some_payload_is_evaluated_exactly_once() {
    let source = r#"
        let mut calls = [0];
        let bump: fn() -> int = || if true => { calls[0] = calls[0].copy() + 1; calls[0].copy() } else => { 0 };
        let some: Option<int> = Some(bump());
        let observed = match some {
            Some(value) => value,
            None => 0,
        };
        observed + calls[0].copy();
    "#;
    assert_eq!(eval(source), vm::Value::Int(2));
}

#[test]
fn option_constructors_and_matches_require_explicit_context() {
    let missing_none = reject("None;");
    assert!(
        missing_none.contains("Option<T> annotation"),
        "missing None context error was {missing_none}"
    );
    let missing_inner = reject("let value = Some(None);");
    assert!(
        missing_inner.contains("Option<T> annotation") || missing_inner.contains("None"),
        "missing inner context error was {missing_inner}"
    );
    let wrong_context = reject("let value: int = Some(1);");
    assert!(
        wrong_context.contains("Some") && wrong_context.contains("int"),
        "wrong context error was {wrong_context}"
    );
    let wrong_payload = reject("let value: Option<int> = Some(\"text\");");
    assert!(
        wrong_payload.contains("schema") || wrong_payload.contains("assigned"),
        "wrong payload error was {wrong_payload}"
    );
    compile_source("let value: Option<int> = Some(1); value;")
        .expect("Some payload inference should compile");
}

#[test]
fn option_match_must_be_exhaustive_and_unambiguous() {
    let missing_none = reject("let value: Option<int> = Some(1); match value { Some(v) => v };");
    assert!(
        missing_none.contains("missing None"),
        "missing None arm error was {missing_none}"
    );
    let missing_some = reject("let value: Option<int> = Some(1); match value { None => 0 };");
    assert!(
        missing_some.contains("missing Some"),
        "missing Some arm error was {missing_some}"
    );
    let duplicate = reject(
        "let value: Option<int> = Some(1); match value { Some(v) => v, Some(v) => v, None => 0 };",
    );
    assert!(
        duplicate.contains("duplicate Some"),
        "duplicate arm error was {duplicate}"
    );
    let wildcard = reject(
        "let value: Option<int> = Some(1); match value { Some(v) => v, None => 0, _ => 1 };",
    );
    assert!(
        wildcard.contains("wildcard") || wildcard.contains("both"),
        "wildcard error was {wildcard}"
    );
    let mixed = reject(
        "let value: Option<Result<int, string>> = Some(Result::Ok(1)); match value { Some(v) => 1, None => 2, Result::Ok(v) => 3, Result::Err(e) => 4 };",
    );
    assert!(
        mixed.contains("mix") || mixed.contains("Result"),
        "mixed arm error was {mixed}"
    );
    let wildcard_none = "let value: Option<int> = Some(1); match value { Some(v) => v, _ => 0 };";
    assert_eq!(eval(wildcard_none), vm::Value::Int(1));
}

#[test]
fn unit_literal_blocks_and_callable_returns_carry_unit_facts() {
    let source = r#"
        fn empty() { }
        fn explicit() -> () { () }
        fn block() -> () { let local = 1; () }
        fn branch(flag: bool) -> () { if flag => { () } else => { () } }
        let a = empty();
        let b = explicit();
        let c = block();
        let d = branch(true);
        let unit: () = ();
        unit;
    "#;
    assert_eq!(eval(source), vm::Value::Null);
}

#[test]
fn empty_blocks_and_no_tail_closures_carry_unit_facts() {
    let source = r#"
        fn no_tail(value: int) { let local = value + 1; }
        let closure: fn() -> () = || if true => { } else => { };
        let from_function: () = no_tail(1);
        let from_closure: () = closure();
        let some: Option<()> = Some(from_closure);
        let matched = match some { Some(unit) => 7, None => -1 };
        matched + 1;
    "#;
    assert_eq!(eval(source), vm::Value::Int(8));
}

#[test]
fn option_operations_cover_supported_methods_and_remaining_negatives() {
    let source = r#"
        let value: Option<int> = Some(1);
        let mapped = value.map(|item| item + 1);
        let unwrapped = value.unwrap_or(0);
        let a = match mapped { Some(v) => v, None => -1 };
        a + unwrapped;
    "#;
    assert_eq!(eval(source), vm::Value::Int(3));

    let absent = r#"
        fn relay() -> Option<int> {
            let value: Option<int> = None;
            let unwrapped = value?;
            Some(unwrapped)
        }
        match relay() { Some(v) => v, None => 7 };
    "#;
    assert_eq!(eval(absent), vm::Value::Int(7));

    let map_err_on_option =
        reject("let value: Option<int> = Some(1); value.map_err(|error| error);");
    assert!(
        map_err_on_option.contains("map_err") && map_err_on_option.contains("Result"),
        "Option.map_err remains Result-only: {map_err_on_option}"
    );
    let unwrap = reject("let value: Option<int> = Some(1); value.unwrap();");
    assert!(
        unwrap.contains("unwrap") && unwrap.contains("not supported"),
        "bare unwrap remains rejected: {unwrap}"
    );
    let mixed = reject(
        "fn out() -> Result<int, string> { let value: Option<int> = None; let item = value?; Result::Ok(item) } out();",
    );
    assert!(
        mixed.contains("Option") && mixed.contains("Result"),
        "mixed propagation requires an explicit conversion: {mixed}"
    );
}

#[test]
fn option_methods_and_projection_format_twice_and_publish_hints() {
    let source = r#"
        struct Entry { note: Option<int> }
        fn relay(flag: bool) -> Option<int> {
            let value: Option<int> = if flag => { Some(2) } else => { None };
            let item = value?;
            Some(item + 1)
        }
        let entry: Entry = { note: Some(2) };
        let projected = entry?.note.unwrap_or(None);
        let mapped = projected.map(|value| value + 1);
        let converted = mapped.ok_or("bad");
        match converted { Result::Ok(value) => value, Result::Err(error) => -1 };
    "#;
    let once = vm::format_source(source).expect("Option formatting succeeds");
    let twice = vm::format_source(&once).expect("second Option formatting succeeds");
    assert_eq!(once, twice, "Option formatter must be idempotent");

    let options = vm::CompileSourceFileOptions::default();
    let model =
        vm::analyze_source_from_string_with_options("main.rss", source, options.clone()).unwrap();
    assert!(model.diagnostics().is_empty(), "{:?}", model.diagnostics());
    let hints = vm::collect_inferred_local_type_hints_with_options(
        source,
        vm::SourceFlavor::RustScript,
        options,
    )
    .unwrap();
    let projected = hints
        .iter()
        .find(|hint| hint.name == "projected")
        .expect("projected local hint");
    let schema = projected.schema.as_ref().expect("projected schema");
    assert!(
        format!("{schema:?}").contains("Option"),
        "logical projection hint must publish Option, found {schema:?}"
    );
    let span = projected.span.unwrap();
    assert_eq!(&source[span.lo..span.hi], "projected");
}

#[test]
fn explicit_null_does_not_become_logical_unit_or_option() {
    let null_return = reject("fn bad() -> () { null }");
    assert!(
        null_return.contains("null") || null_return.contains("Unit") || null_return.contains("()"),
        "explicit null return error was {null_return}"
    );
    let null_unit = reject("let value: () = null;");
    assert!(
        null_unit.contains("null") || null_unit.contains("()"),
        "explicit null unit error was {null_unit}"
    );
    let unit_int = reject("let value: int = ();");
    assert!(
        unit_int.contains("()") || unit_int.contains("int"),
        "unit as int error was {unit_int}"
    );
    let legacy = reject("let value: int? = null; value;");
    assert!(
        legacy.contains("RSS rejects T?") && legacy.contains("Option<T>"),
        "legacy nullable annotation must give an actionable migration diagnostic, got {legacy}"
    );
}

#[test]
fn option_types_are_reserved_and_not_struct_declarations() {
    let reserved = reject("struct Option { value: int }");
    assert!(
        reserved.contains("built-in") || reserved.contains("Option"),
        "reserved Option struct error was {reserved}"
    );
    let arity = reject("let value: Option = None;");
    assert!(
        arity.contains("Option<T>") || arity.contains("type argument"),
        "Option arity error was {arity}"
    );
}

#[test]
fn option_generic_instantiation_keeps_distinct_payloads() {
    let source = r#"
        struct Box<T> { value: T }
        let inner: Option<Box<int>> = Some({ value: 9 });
        let outer: Option<Option<Box<int>>> = Some(Some({ value: 4 }));
        let a = match inner { Some(b) => b.value, None => -1 };
        let b = match outer {
            Some(option) => match option { Some(boxed) => boxed.value, None => -2 },
            None => -4,
        };
        a + b;
    "#;
    assert_eq!(eval(source), vm::Value::Int(13));
}

#[test]
fn conditional_option_values_keep_both_branches_typed() {
    let source = r#"
        fn choose(flag: bool) -> Option<int> {
            if flag => { Some(7) } else => { None }
        }
        let present = match choose(true) { Some(v) => v, None => 0 };
        let absent = match choose(false) { Some(v) => 10, None => 1 };
        present + absent;
    "#;
    assert_eq!(eval(source), vm::Value::Int(8));
}

#[test]
fn option_payload_keeps_typed_value_in_a_match_arm() {
    let source = r#"
        fn identity(value: Option<int>) -> Option<int> { value }
        let typed: Option<int> = identity(Some(11));
        match typed { Some(value) => value, None => 0 };
    "#;
    assert_eq!(eval(source), vm::Value::Int(11));
}
