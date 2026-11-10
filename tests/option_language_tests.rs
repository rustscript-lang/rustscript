#![cfg(feature = "runtime")]

use vm::{Vm, VmStatus, compile_source};

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
fn nested_option_states_are_constructed_matched_and_distinct() {
    let source = r#"
        let none: Option<Option<int>> = Option::None;
        let some_none: Option<Option<int>> = Option::Some(Option::None);
        let some_some: Option<Option<int>> = Option::Some(Option::Some(0));
        let output = match none {
            Option::Some(inner) => match inner { Option::Some(v) => 10, Option::None => 11 },
            Option::None => 0,
        };
        let output2 = match some_none {
            Option::Some(inner) => match inner { Option::Some(v) => 20 + v, Option::None => 21 },
            Option::None => 30,
        };
        let output3 = match some_some {
            Option::Some(inner) => match inner { Option::Some(v) => 40 + v, Option::None => 41 },
            Option::None => 50,
        };
        output + output2 + output3;
    "#;
    assert_eq!(eval(source), vm::Value::Int(61));
}

#[test]
fn unit_none_some_unit_false_zero_and_empty_string_are_distinct() {
    let source = r#"
        let unit_none: Option<()> = Option::None;
        let unit_some: Option<()> = Option::Some(());
        let none_bool: Option<bool> = Option::None;
        let some_false: Option<bool> = Option::Some(false);
        let none_int: Option<int> = Option::None;
        let some_zero: Option<int> = Option::Some(0);
        let none_string: Option<string> = Option::None;
        let some_empty: Option<string> = Option::Some("");
        let a = match unit_none { Option::Some(u) => 1, Option::None => 2 };
        let b = match unit_some { Option::Some(u) => 4, Option::None => 8 };
        let c = match none_bool { Option::Some(v) => 16, Option::None => 32 };
        let d = match some_false { Option::Some(v) => 64, Option::None => 128 };
        let e = match none_int { Option::Some(v) => 1, Option::None => 2 };
        let f = match some_zero { Option::Some(v) => 4, Option::None => 8 };
        let g = match none_string { Option::Some(v) => 16, Option::None => 32 };
        let h = match some_empty { Option::Some(v) => 64, Option::None => 128 };
        a + b + c + d + e + f + g + h;
    "#;
    assert_eq!(eval(source), vm::Value::Int(204));
}

#[test]
fn nested_result_option_and_option_result_compose() {
    let source = r#"
        let option_result: Option<Result<int, string>> = Option::Some(Result::Ok(3));
        let absent: Option<Result<int, string>> = Option::None;
        let result_option: Result<Option<int>, string> = Result::Ok(Option::Some(4));
        let result_absent: Result<Option<int>, string> = Result::Ok(Option::None);
        let first = match option_result {
            Option::Some(result) => match result { Result::Ok(value) => value, Result::Err(error) => -1 },
            Option::None => -2,
        };
        let second = match absent {
            Option::Some(result) => 100,
            Option::None => 10,
        };
        let third = match result_option {
            Result::Ok(option) => match option { Option::Some(value) => value, Option::None => -4 },
            Result::Err(error) => -8,
        };
        let fourth = match result_absent {
            Result::Ok(option) => match option { Option::Some(value) => 100, Option::None => 20 },
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
        let named: Option<Point> = Option::Some({ x: 5 });
        let array: Option<[Option<int>, string]> = Option::Some([Option::Some(1), "tag"]);
        let callable: Option<fn(int) -> int> = Option::Some(add1);
        let x = match named { Option::Some(value) => value.x, Option::None => -1 };
        let inner = match array {
            Option::Some(values) => match values[0] { Option::Some(v) => v, Option::None => -2 },
            Option::None => -4,
        };
        let applied = match callable { Option::Some(f) => f(6), Option::None => -8 };
        x + inner + applied;
    "#;
    assert_eq!(eval(source), vm::Value::Int(13));
}

#[test]
fn some_payload_is_evaluated_exactly_once() {
    let source = r#"
        let mut calls = [0];
        let bump: fn() -> int = || if true => { calls[0] = calls[0].copy() + 1; calls[0].copy() } else => { 0 };
        let some: Option<int> = Option::Some(bump());
        let observed = match some {
            Option::Some(value) => value,
            Option::None => 0,
        };
        observed + calls[0].copy();
    "#;
    assert_eq!(eval(source), vm::Value::Int(2));
}

#[test]
fn option_constructors_and_matches_require_explicit_context() {
    let missing_none = reject("Option::None;");
    assert!(
        missing_none.contains("Option<T> annotation"),
        "missing None context error was {missing_none}"
    );
    let missing_inner = reject("let value = Option::Some(Option::None);");
    assert!(
        missing_inner.contains("Option<T> annotation") || missing_inner.contains("Option::None"),
        "missing inner context error was {missing_inner}"
    );
    let wrong_context = reject("let value: int = Option::Some(1);");
    assert!(
        wrong_context.contains("Option") && wrong_context.contains("int"),
        "wrong context error was {wrong_context}"
    );
    let wrong_payload = reject("let value: Option<int> = Option::Some(\"text\");");
    assert!(
        wrong_payload.contains("schema") || wrong_payload.contains("assigned"),
        "wrong payload error was {wrong_payload}"
    );
    compile_source("let value: Option<int> = Option::Some(1); value;")
        .expect("Some payload inference should compile");
}

#[test]
fn option_match_must_be_exhaustive_and_unambiguous() {
    let missing_none =
        reject("let value: Option<int> = Option::Some(1); match value { Option::Some(v) => v };");
    assert!(
        missing_none.contains("missing Option::None"),
        "missing None arm error was {missing_none}"
    );
    let missing_some =
        reject("let value: Option<int> = Option::Some(1); match value { Option::None => 0 };");
    assert!(
        missing_some.contains("missing Option::Some"),
        "missing Some arm error was {missing_some}"
    );
    let duplicate = reject(
        "let value: Option<int> = Option::Some(1); match value { Option::Some(v) => v, Option::Some(v) => v, Option::None => 0 };",
    );
    assert!(
        duplicate.contains("duplicate Option::Some"),
        "duplicate arm error was {duplicate}"
    );
    let wildcard = reject(
        "let value: Option<int> = Option::Some(1); match value { Option::Some(v) => v, Option::None => 0, _ => 1 };",
    );
    assert!(
        wildcard.contains("wildcard") || wildcard.contains("both"),
        "wildcard error was {wildcard}"
    );
    let mixed = reject(
        "let value: Option<Result<int, string>> = Option::Some(Result::Ok(1)); match value { Option::Some(v) => 1, Option::None => 2, Result::Ok(v) => 3, Result::Err(e) => 4 };",
    );
    assert!(
        mixed.contains("mix") || mixed.contains("Result"),
        "mixed arm error was {mixed}"
    );
    let wildcard_none =
        "let value: Option<int> = Option::Some(1); match value { Option::Some(v) => v, _ => 0 };";
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
        let some: Option<()> = Option::Some(from_closure);
        let matched = match some { Option::Some(unit) => 7, Option::None => -1 };
        matched + 1;
    "#;
    assert_eq!(eval(source), vm::Value::Int(8));
}

#[test]
fn unsupported_logical_option_operations_are_rejected_not_nullable_lowered() {
    let unwrap = reject("let value: Option<int> = Option::Some(1); value.unwrap_or(0);");
    assert!(
        unwrap.contains("Option") && unwrap.contains("unwrap_or"),
        "Option unwrap_or error was {unwrap}"
    );
    let map = reject("let value: Option<int> = Option::Some(1); value.map(|v| v + 1);");
    assert!(
        map.contains("Option") && map.contains("map"),
        "Option map error was {map}"
    );
    let try_query = reject(
        "fn relay() -> Option<int> { let value: Option<int> = Option::Some(1); value? } relay();",
    );
    assert!(
        try_query.contains("Option") && (try_query.contains("Result") || try_query.contains("?")),
        "Option ? error was {try_query}"
    );
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
    compile_source("let value: int? = null; value;")
        .expect("legacy nullable values remain their own contract");
}

#[test]
fn option_types_are_reserved_and_not_struct_declarations() {
    let reserved = reject("struct Option { value: int }");
    assert!(
        reserved.contains("built-in") || reserved.contains("Option"),
        "reserved Option struct error was {reserved}"
    );
    let arity = reject("let value: Option = Option::None;");
    assert!(
        arity.contains("Option<T>") || arity.contains("type argument"),
        "Option arity error was {arity}"
    );
}

#[test]
fn option_generic_instantiation_keeps_distinct_payloads() {
    let source = r#"
        struct Box<T> { value: T }
        let inner: Option<Box<int>> = Option::Some({ value: 9 });
        let outer: Option<Option<Box<int>>> = Option::Some(Option::Some({ value: 4 }));
        let a = match inner { Option::Some(b) => b.value, Option::None => -1 };
        let b = match outer {
            Option::Some(option) => match option { Option::Some(boxed) => boxed.value, Option::None => -2 },
            Option::None => -4,
        };
        a + b;
    "#;
    assert_eq!(eval(source), vm::Value::Int(13));
}

#[test]
fn conditional_option_values_keep_both_branches_typed() {
    let source = r#"
        fn choose(flag: bool) -> Option<int> {
            if flag => { Option::Some(7) } else => { Option::None }
        }
        let present = match choose(true) { Option::Some(v) => v, Option::None => 0 };
        let absent = match choose(false) { Option::Some(v) => 10, Option::None => 1 };
        present + absent;
    "#;
    assert_eq!(eval(source), vm::Value::Int(8));
}

#[test]
fn option_payload_keeps_typed_value_in_a_match_arm() {
    let source = r#"
        fn identity(value: Option<int>) -> Option<int> { value }
        let typed: Option<int> = identity(Option::Some(11));
        match typed { Option::Some(value) => value, Option::None => 0 };
    "#;
    assert_eq!(eval(source), vm::Value::Int(11));
}
