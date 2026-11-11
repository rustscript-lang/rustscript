#![cfg(feature = "runtime")]

use vm::{Value, Vm, VmStatus, compile_source};

fn eval(source: &str) -> Value {
    let compiled = compile_source(source).expect("Option try source should compile");
    let mut machine = Vm::new(compiled.program.with_local_count(compiled.locals));
    machine.set_fuel(100_000);
    assert_eq!(
        machine.run().expect("Option try program should run"),
        VmStatus::Halted
    );
    machine
        .stack()
        .last()
        .cloned()
        .expect("Option try program should leave a value")
}

fn reject(source: &str) -> String {
    match compile_source(source) {
        Ok(_) => panic!("Option try source should be rejected: {source}"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn question_mark_passes_some_payload_and_stops_after_none() {
    let source = r#"
        fn load(flag: bool) -> Option<int> {
            if flag => { Option::Some(21) } else => { Option::None }
        }
        fn relay(flag: bool) -> Option<int> {
            let value = load(flag)?;
            Option::Some(value + 1)
        }
        let present = match relay(true) { Option::Some(v) => v, Option::None => -1 };
        let absent = match relay(false) { Option::Some(v) => 100, Option::None => 2 };
        present + absent;
    "#;
    // Some(21) continues as 22; None exits only this Option-returning function.
    assert_eq!(eval(source), Value::Int(24));
}

#[test]
fn none_stops_later_arguments_and_side_effects() {
    let source = r#"
        fn relay(flag: bool) -> Option<int> {
            let mut step_calls = [0];
            let mut continuation = [0];
            let step: fn() -> Option<int> = || if flag => {
                step_calls[0] = step_calls[0].copy() + 1;
                Option::Some(1)
            } else => { Option::None };
            let first = step()?;
            continuation[0] = continuation[0].copy() + 10;
            let second = step()?;
            continuation[0] = continuation[0].copy() + 100;
            Option::Some(first + second + continuation[0].copy() + step_calls[0].copy())
        }
        let present = match relay(true) { Option::Some(v) => v, Option::None => -1 };
        let absent = match relay(false) { Option::Some(v) => 100, Option::None => 2 };
        present + absent;
    "#;
    // Present: 1 + 1 + 110 + 2 = 114; absent exits before any continuation.
    assert_eq!(eval(source), Value::Int(116));
}

#[test]
fn none_in_a_closure_returns_only_from_that_closure() {
    let source = r#"
        let mut calls = [0];
        let inner: fn() -> Option<int> = || if true => {
            let absent: Option<int> = Option::None;
            let value = absent?;
            calls[0] = calls[0].copy() + 100;
            Option::Some(value)
        } else => { Option::None };
        let outer: fn() -> Option<int> = || if true => {
            let inside = inner();
            calls[0] = calls[0].copy() + 1;
            inside
        } else => { Option::None };
        let observed = match outer() { Option::Some(v) => v, Option::None => -1 };
        observed + calls[0].copy();
    "#;
    // The caller continues after the inner closure's None: -1 + 1 == 0.
    assert_eq!(eval(source), Value::Int(0));
}

#[test]
fn question_mark_covers_named_functions_aliases_and_finite_recursion() {
    let source = r#"
        fn countdown(value: int) -> Option<int> {
            if value <= 0 => { Option::Some(0) }
            else => {
                let next = countdown(value - 1)?;
                Option::Some(next + 1)
            }
        }
        fn relay(value: int) -> Option<int> {
            let alias = countdown;
            let forwarded = alias(value)?;
            Option::Some(forwarded + 1)
        }
        match relay(4) { Option::Some(v) => v, Option::None => -1 };
    "#;
    // Four recursive steps plus the relay increment.
    assert_eq!(eval(source), Value::Int(5));
}

#[test]
fn option_try_accepts_a_wider_option_return_payload() {
    let source = r#"
        fn widen() -> Option<string> {
            let absent: Option<int> = Option::None;
            let value = absent?;
            Option::Some("done")
        }
        match widen() { Option::Some(v) => 4, Option::None => 7 };
    "#;
    // None carries no payload, so the enclosing Option<String> receives None.
    assert_eq!(eval(source), Value::Int(7));
}

#[test]
fn explicit_ok_or_and_ok_or_else_convert_before_result_propagation() {
    let source = r#"
        fn load(flag: bool) -> Option<int> {
            if flag => { Option::Some(4) } else => { Option::None }
        }
        fn runtime_error() -> string { "missing" }
        fn relay(flag: bool) -> Result<int, string> {
            let value = load(flag).ok_or_else(runtime_error)?;
            let fallback_checked = load(flag).ok_or("second missing")?;
            Result::Ok(value + fallback_checked)
        }
        let present = match relay(true) { Result::Ok(v) => v, Result::Err(e) => -1 };
        let absent = match relay(false) { Result::Ok(v) => 100, Result::Err(e) => 2 };
        present + absent;
    "#;
    // Some(4) runs both conversions and continues as 8; None stops at Err.
    assert_eq!(eval(source), Value::Int(10));
}

#[test]
fn explicit_result_ok_converts_before_option_propagation() {
    let source = r#"
        fn load(flag: bool) -> Result<int, string> {
            if flag => { Result::Ok(3) } else => { Result::Err("bad") }
        }
        fn relay(flag: bool) -> Option<int> {
            let value = load(flag).ok()?;
            Option::Some(value + 1)
        }
        let present = match relay(true) { Option::Some(v) => v, Option::None => -1 };
        let absent = match relay(false) { Option::Some(v) => 100, Option::None => 5 };
        present + absent;
    "#;
    // Ok(3) -> Some(4); Err -> None.
    assert_eq!(eval(source), Value::Int(9));
}

#[test]
fn map_err_before_result_propagation_remains_exact() {
    let source = r#"
        fn load() -> Result<int, string> { Result::Err("source") }
        fn convert(error: string) -> int { 42 }
        fn relay() -> Result<int, int> {
            let value = load().map_err(convert)?;
            Result::Ok(value)
        }
        match relay() { Result::Ok(v) => v, Result::Err(e) => e };
    "#;
    assert_eq!(eval(source), Value::Int(42));
}

#[test]
fn mixed_option_and_result_propagation_without_conversion_is_rejected() {
    let diagnostic = reject(
        r#"
        fn bad() -> Result<int, string> {
            let value: Option<int> = Option::None;
            let unwrapped = value?;
            Result::Ok(unwrapped)
        }
        bad();
    "#,
    );
    assert!(
        diagnostic.contains("Option") && diagnostic.contains("Result"),
        "mixed propagation must explain the explicit conversion: {diagnostic}"
    );
    let diagnostic = reject(
        r#"
        fn bad() -> Option<int> {
            let value: Result<int, string> = Result::Err("bad");
            let unwrapped = value?;
            Option::Some(unwrapped)
        }
        bad();
    "#,
    );
    assert!(
        diagnostic.contains("Option") && diagnostic.contains("Result"),
        "Result in an Option-returning callable must stay explicit: {diagnostic}"
    );
}

#[test]
fn question_mark_outside_option_or_result_context_reports_the_owner() {
    let diagnostic = reject(
        r#"
        fn plain() -> int {
            let value: Option<int> = Option::None;
            let unwrapped = value?;
            unwrapped
        }
        plain();
    "#,
    );
    assert!(
        diagnostic.contains("Option") && diagnostic.contains("enclosing"),
        "a plain function must reject Option try: {diagnostic}"
    );
}

#[test]
fn unwrap_returns_the_nested_payload_not_an_outer_option() {
    let source = r#"
        fn relay() -> Option<int> {
            let nested: Option<Option<int>> = Option::Some(Option::None);
            let inner = nested.unwrap_or(Option::None);
            let value = inner?;
            Option::Some(value + 1)
        }
        match relay() { Option::Some(v) => 8, Option::None => 9 };
    "#;
    assert_eq!(eval(source), Value::Int(9));
}

// ---- Fallible `?.` projection semantics ----

#[test]
fn option_methods_and_projection_execute_from_loaded_vmbc() {
    let source = r#"
        struct Entry { note: Option<int> }
        let entry: Entry = { note: Option::Some(4) };
        let mapped = entry?.note.unwrap_or(Option::None);
        let converted = mapped.ok_or("missing").map_err(|error| error.length);
        let recovered = match converted { Result::Ok(value) => value, Result::Err(length) => length };
        let absent: Option<int> = Option::None;
        let fallback = absent.or_else(|| Option::Some(3));
        let final_value = match fallback { Option::Some(value) => value, Option::None => -1 };
        recovered + final_value;
    "#;
    let compiled = compile_source(source).expect("loaded-VMBC Option source should compile");
    let bytes = vm::encode_program(&compiled.program.with_local_count(compiled.locals)).unwrap();
    let decoded = vm::decode_program(&bytes).unwrap();
    let mut loaded = Vm::new(decoded.with_local_count(compiled.locals));
    loaded.set_fuel(100_000);
    assert_eq!(loaded.run().unwrap(), VmStatus::Halted);
    assert_eq!(loaded.stack().last(), Some(&Value::Int(7)));
}

#[test]
fn optional_projection_preserves_none_some_none_and_some_some() {
    let source = r#"
        struct Entry { name: string, note: Option<int> }
        let present: Entry = { name: "first", note: Option::Some(0) };
        let explicit_none: Entry = { name: "second", note: Option::None };
        let omitted: Entry = { name: "third" };

        let projected_some = present?.note;
        let a = match projected_some {
            Option::Some(inner) => match inner {
                Option::Some(value) => value + 10,
                Option::None => 11,
            },
            Option::None => 12,
        };
        let projected_none = explicit_none?.note;
        let b = match projected_none {
            Option::Some(inner) => match inner {
                Option::Some(value) => 100,
                Option::None => 13,
            },
            Option::None => 14,
        };
        let projected_missing = omitted?.note;
        let c = match projected_missing {
            Option::Some(inner) => match inner {
                Option::Some(value) => 200,
                Option::None => 15,
            },
            Option::None => 16,
        };
        a + b + c;
    "#;
    // Some(Some(0)) keeps two layers, explicit None stays Some(None), and an
    // omitted permitted Option field normalizes to None.
    assert_eq!(eval(source), Value::Int(39));
}

#[test]
fn optional_projection_distinguishes_absent_dynamic_key_from_present_none() {
    let source = r#"
        let values: map<Option<int>> = { inner: Option::None, some: Option::Some(3) };
        let present_none = values?.["inner"];
        let present_some = values?.["some"];
        let absent = values?.["absent"];
        let a = match present_none {
            Option::Some(inner) => match inner { Option::Some(v) => 100, Option::None => 1 },
            Option::None => 1000,
        };
        let b = match present_some {
            Option::Some(inner) => match inner { Option::Some(v) => v, Option::None => 200 },
            Option::None => 300,
        };
        let c = match absent { Option::Some(inner) => 400, Option::None => 10 };
        a + b + c;
    "#;
    // present None = 1, present Some(3) = 3, absent = None -> 10.
    assert_eq!(eval(source), Value::Int(14));
}

#[test]
fn optional_projection_of_unit_and_out_of_range_indexes_stays_typed() {
    let source = r#"
        struct UnitEntry { value: () }
        let units: UnitEntry = { value: () };
        let projected_unit = units?.value;
        let unit_state = match projected_unit { Option::Some(value) => 1, Option::None => 2 };
        let text: string = "ab";
        let outside = text?.[10];
        let outside_state = match outside { Option::Some(value) => 100, Option::None => 3 };
        let inside = text?.[1];
        let inside_state = match inside { Option::Some(value) => if value == "b" => { 4 } else => { 5 }, Option::None => 6 };
        unit_state + outside_state + inside_state;
    "#;
    // Unit payload is Some(Unit), an out-of-range index is None, and a valid
    // index keeps the exact projected string.
    assert_eq!(eval(source), Value::Int(8));
}

#[test]
fn optional_receiver_and_then_projects_one_layer_without_flattening_payload_options() {
    let source = r#"
        struct Entry { note: Option<int> }
        let some_entry: Option<Entry> = Option::Some({ note: Option::Some(7) });
        let none_entry: Option<Entry> = Option::None;
        let from_some = some_entry?.note;
        let from_none = none_entry?.note;
        let a = match from_some {
            Option::Some(inner) => match inner { Option::Some(v) => v, Option::None => -1 },
            Option::None => -2,
        };
        let b = match from_none { Option::Some(inner) => 100, Option::None => 3 };
        a + b;
    "#;
    // Some(Entry) projects its child without an extra outer layer; None
    // receiver projects to None.
    assert_eq!(eval(source), Value::Int(10));
}

#[test]
fn optional_projection_keeps_struct_required_field_checks() {
    let diagnostic = match compile_source(
        r#"struct Entry { name: string, note: Option<int> }
let entry: Entry = { note: Option::None };
entry?.name;"#,
    ) {
        Ok(_) => panic!("missing required field must still reject"),
        Err(error) => error.to_string(),
    };
    assert!(
        diagnostic.contains("required") && diagnostic.contains("name"),
        "required-field check must remain: {diagnostic}"
    );
}

#[test]
fn optional_projection_does_not_implicitly_convert_option_and_result() {
    let diagnostic = match compile_source(
        r#"fn out() -> Option<int> {
let value: Result<int, string> = Result::Ok(1);
let projected = value?;
Option::Some(projected)
}
out();"#,
    ) {
        Ok(_) => panic!("mixed projection propagation must reject"),
        Err(error) => error.to_string(),
    };
    assert!(
        diagnostic.contains("Option") && diagnostic.contains("Result"),
        "explicit conversion requirement must stay visible: {diagnostic}"
    );
}
