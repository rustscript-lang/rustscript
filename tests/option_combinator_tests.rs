#![cfg(feature = "runtime")]

use vm::{Value, Vm, VmStatus, compile_source};

fn eval(source: &str) -> Value {
    let compiled = compile_source(source).expect("Option combinator source should compile");
    let mut machine = Vm::new(compiled.program.with_local_count(compiled.locals));
    machine.set_fuel(100_000);
    assert_eq!(
        machine.run().expect("Option combinator program should run"),
        VmStatus::Halted
    );
    machine
        .stack()
        .last()
        .cloned()
        .expect("Option combinator program should leave a value")
}

fn reject(source: &str) -> String {
    match compile_source(source) {
        Ok(_) => panic!("Option combinator source should be rejected: {source}"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn map_wraps_exactly_the_callback_value_and_skips_none() {
    let source = r#"
        let present: Option<int> = Option::Some(2);
        let absent: Option<int> = Option::None;
        let mapped_present = present.map(|value| value + 1);
        let mapped_absent = absent.map(|value| value + 1);
        let some_value = match mapped_present { Option::Some(v) => v, Option::None => -1 };
        let none_value = match mapped_absent { Option::Some(v) => 100, Option::None => 3 };
        some_value + none_value;
    "#;
    // Some(2) -> Some(3); None stays None.
    assert_eq!(eval(source), Value::Int(6));
}

#[test]
fn map_does_not_flatten_nested_option_or_result_payloads() {
    let source = r#"
        let nested: Option<Option<int>> = Option::Some(Option::Some(7));
        let mapped = nested.map(|inner| inner);
        let observed = match mapped {
            Option::Some(inner) => match inner { Option::Some(v) => v, Option::None => -1 },
            Option::None => -2,
        };
        let result: Result<int, string> = Result::Ok(9);
        let wrapped = Option::Some(result).map(|value| match value {
            Result::Ok(v) => v + 1,
            Result::Err(e) => -3,
        });
        let second = match wrapped { Option::Some(v) => v, Option::None => -4 };
        observed + second;
    "#;
    // map(|inner| inner) keeps Some(Some(7)); map over Result yields Some(10).
    assert_eq!(eval(source), Value::Int(17));
}

#[test]
fn map_calls_the_selected_callback_body_once_and_never_the_unselected_body() {
    let source = r#"
        let mut some_calls = [0];
        let mut none_calls = [0];
        let present: Option<int> = Option::Some(5);
        let absent: Option<int> = Option::None;
        let mapped_present = present.map(|value| if true => {
            some_calls[0] = some_calls[0].copy() + 1;
            value + 1
        } else => { 0 });
        let mapped_absent = absent.map(|value| if true => {
            none_calls[0] = none_calls[0].copy() + 1;
            value + 1
        } else => { 0 });
        let observed = match mapped_present { Option::Some(v) => v, Option::None => 0 };
        let absent_observed = match mapped_absent { Option::Some(v) => 1, Option::None => 0 };
        observed + some_calls[0].copy() + none_calls[0].copy() + absent_observed;
    "#;
    // Some path body runs once (6), None path body never runs (0 calls).
    assert_eq!(eval(source), Value::Int(7));
}

#[test]
fn and_then_flattens_only_the_explicitly_returned_option() {
    let source = r#"
        fn keep(values: [Option<int>], index: int) -> Option<int> { values[index] }
        let present: Option<int> = Option::Some(4);
        let absent: Option<int> = Option::None;
        let nested: Option<Option<int>> = Option::Some(Option::Some(6));
        let kept = present.and_then(|value| keep([Option::Some(value + 1), Option::None], 0));
        let skipped = absent.and_then(|value| Option::Some(value + 1));
        // and_then flattens exactly one returned layer even for an Option payload.
        let double = nested.and_then(|inner| inner);
        let a = match kept { Option::Some(v) => v, Option::None => -1 };
        let b = match skipped { Option::Some(v) => 100, Option::None => 2 };
        let c = match double { Option::Some(v) => v, Option::None => 3 };
        a + b + c;
    "#;
    // kept = Some(5); skipped = None; double = Some(Some(6)) flattened once to Some(6).
    assert_eq!(eval(source), Value::Int(13));
}

#[test]
fn and_then_rejects_a_callback_that_returns_a_bare_payload() {
    let diagnostic =
        reject("let value: Option<int> = Option::Some(1); value.and_then(|item| item + 1);");
    assert!(
        diagnostic.contains("and_then") && diagnostic.contains("Option"),
        "and_then must require an Option-returning callback: {diagnostic}"
    );
}

#[test]
fn or_else_uses_a_zero_argument_lazy_fallback_and_keeps_the_payload() {
    let source = r#"
        let mut fallback_calls = [0];
        let present: Option<int> = Option::Some(8);
        let absent: Option<int> = Option::None;
        let present_value = present.or_else(|| if true => {
            fallback_calls[0] = fallback_calls[0].copy() + 1;
            Option::Some(99)
        } else => { Option::None });
        let absent_value = absent.or_else(|| if true => {
            fallback_calls[0] = fallback_calls[0].copy() + 1;
            Option::Some(9)
        } else => { Option::None });
        let a = match present_value { Option::Some(v) => v, Option::None => -1 };
        let b = match absent_value { Option::Some(v) => v, Option::None => -2 };
        a + b + fallback_calls[0].copy();
    "#;
    // Some keeps 8 without invoking the fallback; None invokes it once.
    assert_eq!(eval(source), Value::Int(18));
}

#[test]
fn or_else_requires_a_zero_argument_callback() {
    let diagnostic = reject(
        "let value: Option<int> = Option::None; value.or_else(|error| Option::Some(error));",
    );
    assert!(
        diagnostic.contains("or_else") && diagnostic.contains("zero"),
        "or_else must reject a Result-shaped callback: {diagnostic}"
    );
}

#[test]
fn unwrap_or_is_eager_and_returns_the_exact_payload_type() {
    let source = r#"
        let mut fallback_calls = [0];
        let bump = || if true => {
            fallback_calls[0] = fallback_calls[0].copy() + 1;
            11
        } else => { 0 };
        let present: Option<int> = Option::Some(5);
        let absent: Option<int> = Option::None;
        let a = present.unwrap_or(bump());
        let b = absent.unwrap_or(bump());
        let nested: Option<Option<int>> = Option::Some(Option::Some(3));
        let c = nested.unwrap_or(Option::Some(0));
        let c_value = match c { Option::Some(v) => v, Option::None => -1 };
        a + b + c_value + fallback_calls[0].copy();
    "#;
    // The eager fallback runs once even when Some is selected, and once for None.
    assert_eq!(eval(source), Value::Int(21));
}

#[test]
fn unwrap_or_else_runs_the_body_only_for_none() {
    let source = r#"
        let mut fallback_calls = [0];
        let present: Option<int> = Option::Some(6);
        let absent: Option<int> = Option::None;
        let a = present.unwrap_or_else(|| if true => {
            fallback_calls[0] = fallback_calls[0].copy() + 1;
            100
        } else => { 0 });
        let b = absent.unwrap_or_else(|| if true => {
            fallback_calls[0] = fallback_calls[0].copy() + 1;
            7
        } else => { 0 });
        a + b + fallback_calls[0].copy();
    "#;
    // Lazy body runs only for None: 6 + 7 + 1.
    assert_eq!(eval(source), Value::Int(14));
}

#[test]
fn ok_or_is_eager_and_converts_some_none_to_ok_err() {
    let source = r#"
        let mut error_calls = [0];
        let bump = || if true => {
            error_calls[0] = error_calls[0].copy() + 1;
            "missing"
        } else => { "unused" };
        let present: Option<int> = Option::Some(12);
        let absent: Option<int> = Option::None;
        let ok = present.ok_or(bump());
        let err = absent.ok_or(bump());
        let a = match ok { Result::Ok(v) => v, Result::Err(e) => -1 };
        let b = match err { Result::Ok(v) => -2, Result::Err(e) => 10 };
        a + b + error_calls[0].copy();
    "#;
    // Eager error argument is created on both selections.
    assert_eq!(eval(source), Value::Int(24));
}

#[test]
fn ok_or_else_runs_the_error_body_only_for_none() {
    let source = r#"
        let mut error_calls = [0];
        let present: Option<int> = Option::Some(3);
        let absent: Option<int> = Option::None;
        let ok = present.ok_or_else(|| if true => {
            error_calls[0] = error_calls[0].copy() + 1;
            "no present"
        } else => { "unused" });
        let err = absent.ok_or_else(|| if true => {
            error_calls[0] = error_calls[0].copy() + 1;
            "no absent"
        } else => { "unused" });
        let a = match ok { Result::Ok(v) => v, Result::Err(e) => -1 };
        let b = match err { Result::Ok(v) => -2, Result::Err(e) => 8 };
        a + b + error_calls[0].copy();
    "#;
    // Only the selected None path runs its callback.
    assert_eq!(eval(source), Value::Int(12));
}

#[test]
fn result_ok_and_err_select_and_discard_the_matching_payload() {
    let source = r#"
        let ok_value: Result<int, string> = Result::Ok(21);
        let err_value: Result<int, string> = Result::Err("bad");
        let ok_option = ok_value.ok();
        let err_option = err_value.ok();
        let ok_err_option = ok_value.err();
        let err_err_option = err_value.err();
        let a = match ok_option { Option::Some(v) => v, Option::None => -1 };
        let b = match err_option { Option::Some(v) => 100, Option::None => 1 };
        let c = match ok_err_option { Option::Some(e) => 100, Option::None => 2 };
        let d = match err_err_option { Option::Some(e) => 3, Option::None => -2 };
        a + b + c + d;
    "#;
    // Ok(21) -> Some(21)/None; Err("bad") -> None/Some("bad").
    assert_eq!(eval(source), Value::Int(27));
}

#[test]
fn result_ok_and_err_require_a_result_receiver() {
    let diagnostic = reject("let value: Option<int> = Option::Some(1); value.ok();");
    assert!(
        diagnostic.contains("Result") && diagnostic.contains("ok"),
        "ok on an Option receiver must require Result: {diagnostic}"
    );
    let diagnostic = reject("let value: Option<int> = Option::Some(1); value.err();");
    assert!(
        diagnostic.contains("Result") && diagnostic.contains("err"),
        "err on an Option receiver must require Result: {diagnostic}"
    );
}

#[test]
fn option_methods_reject_wrong_callback_shapes_and_return_types() {
    let wrong_map_return = reject(
        "let value: Option<int> = Option::Some(1); let out: Option<string> = value.map(|item| item + 1);",
    );
    assert!(
        wrong_map_return.contains("map") && wrong_map_return.contains("expects"),
        "map output type mismatch must be focused: {wrong_map_return}"
    );
    let wrong_unwrap =
        reject("let value: Option<int> = Option::Some(1); let out: string = value.unwrap_or(0);");
    assert!(
        wrong_unwrap.contains("unwrap_or") && wrong_unwrap.contains("string"),
        "unwrap_or output type mismatch must be focused: {wrong_unwrap}"
    );
    let wrong_ok_or = reject(
        "let value: Option<int> = Option::Some(1); let out: Result<int, int> = value.ok_or(\"bad\");",
    );
    assert!(
        wrong_ok_or.contains("ok_or") && wrong_ok_or.contains("int"),
        "ok_or error type mismatch must be focused: {wrong_ok_or}"
    );
    let map_err_on_option =
        reject("let value: Option<int> = Option::Some(1); value.map_err(|error| error);");
    assert!(
        map_err_on_option.contains("map_err") && map_err_on_option.contains("Result"),
        "map_err must remain Result-only: {map_err_on_option}"
    );
}

#[test]
fn methods_on_a_non_option_receiver_report_the_option_requirement() {
    let diagnostic = reject("let value: int = 1; value.map(|item| item + 1);");
    assert!(
        diagnostic.contains("Option") || diagnostic.contains("Result"),
        "non-container receiver must name the required owner: {diagnostic}"
    );
}
