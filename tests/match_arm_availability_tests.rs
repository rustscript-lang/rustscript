#![cfg(feature = "runtime")]

use vm::{Value, Vm, VmStatus, compile_source};

fn run(source: &str) -> Vec<Value> {
    let compiled = compile_source(source)
        .unwrap_or_else(|error| panic!("source should compile, got {error}\n{source}"));
    let mut machine = Vm::new(compiled.program.with_local_count(compiled.locals));
    machine.set_fuel(100_000);
    let status = machine
        .run()
        .unwrap_or_else(|error| panic!("runtime error {error}\n{source}"));
    assert_eq!(status, VmStatus::Halted, "{source}");
    machine.stack().to_vec()
}

fn reject(source: &str) -> String {
    match compile_source(source) {
        Ok(_) => panic!("source should be rejected:\n{source}"),
        Err(error) => error.to_string(),
    }
}

/// Supplied function parameters are already initialized. A match arm must read
/// them on every applicable branch, for both Copy and non-copy payloads.
#[test]
fn match_arms_read_supplied_outer_parameters() {
    let source = r#"
        fn option_int(actual: Option<int>, expected: int) -> bool {
            match actual {
                Some(value) => value == expected,
                None => false
            }
        }
        fn option_string(actual: Option<string>, expected: string) -> bool {
            match actual {
                Some(value) => value == expected,
                None => false
            }
        }
        fn result_string(actual: Result<string, string>, expected: string) -> bool {
            match actual {
                Result::Ok(value) => value == expected,
                Result::Err(_) => false
            }
        }
        fn result_error(actual: Result<string, string>, expected: string) -> bool {
            match actual {
                Result::Ok(_) => false,
                Result::Err(value) => value == expected
            }
        }
        option_int(Some(7), 7);
        option_int(Some(7), 8);
        option_int(None, 7);
        option_string(Some("same"), "same");
        option_string(Some("same"), "other");
        option_string(None, "same");
        result_string(Result::Ok("same"), "same");
        result_string(Result::Ok("same"), "other");
        result_string(Result::Err("missing"), "same");
        result_error(Result::Err("same"), "same");
        result_error(Result::Err("same"), "other");
        result_error(Result::Ok("same"), "same");
    "#;
    assert_eq!(
        run(source),
        vec![
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(false),
        ]
    );
}

#[test]
fn match_arms_read_same_frame_lets_and_nested_matches() {
    let source = r#"
        fn same_frame(actual: Option<int>, expected: int) -> bool {
            let probe = expected;
            match actual {
                Some(value) => value == probe,
                None => false
            }
        }
        fn nested(actual: Option<Option<int>>, expected: int) -> bool {
            match actual {
                Some(inner) => match inner {
                    Some(value) => value == expected,
                    None => false
                },
                None => false
            }
        }
        fn both_arms(actual: Option<int>, expected: int) -> int {
            match actual {
                Some(value) => value + expected,
                None => expected
            }
        }
        same_frame(Some(7), 7);
        same_frame(Some(7), 8);
        same_frame(None, 7);
        nested(Some(Some(7)), 7);
        nested(Some(Some(7)), 8);
        nested(Some(None), 7);
        nested(None, 7);
        both_arms(Some(7), 1);
        both_arms(None, 4);
    "#;
    assert_eq!(
        run(source),
        vec![
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Int(8),
            Value::Int(4),
        ]
    );
}

#[test]
fn match_arm_keeps_real_captures_separate_from_same_frame_locals() {
    let source = r#"
        let outer = "same";
        fn captured_root(actual: Option<string>) -> bool {
            match actual {
                Some(value) => value == outer,
                None => false
            }
        }
        fn closure_over_parameter(actual: Option<string>, expected: string) -> bool {
            let check = |probe| probe == expected;
            match actual {
                Some(value) => check(value),
                None => false
            }
        }
        captured_root(Some("same"));
        captured_root(Some("other"));
        captured_root(None);
        closure_over_parameter(Some("same"), "same");
        closure_over_parameter(Some("same"), "other");
        closure_over_parameter(None, "same");
    "#;
    assert_eq!(
        run(source),
        vec![
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(false),
        ]
    );
}

/// Equality does not consume a string parameter. A match arm must follow that
/// contract rather than inventing a move.
#[test]
fn string_equality_reuse_matches_the_non_match_baseline() {
    let source = r#"
        fn plain(actual: string, expected: string) -> bool {
            let first = actual == expected;
            let second = actual == expected;
            first && second
        }
        fn matched(actual: Option<string>, expected: string) -> bool {
            let first = match actual {
                Some(value) => value == expected,
                None => false
            };
            let second = match actual {
                Some(value) => value == expected,
                None => false
            };
            first && second
        }
        plain("same", "same");
        plain("same", "other");
        matched(Some("same"), "same");
        matched(Some("same"), "other");
        matched(None, "same");
    "#;
    assert_eq!(
        run(source),
        vec![
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(false),
        ]
    );
}

#[test]
fn match_arm_availability_still_rejects_illegal_uses() {
    let escaped = reject(
        r#"
        fn compare(actual: Option<int>) -> int {
            match actual {
                Some(value) => value,
                None => 0
            };
            value
        }
        compare(Some(7));
        "#,
    );
    assert!(
        escaped.contains("unknown local 'value'"),
        "arm binding must not escape, got {escaped}"
    );

    let maybe = reject(
        r#"
        fn compare(actual: Option<int>, flag: bool) -> bool {
            if flag => {
                let expected = 7;
                expected
            } else => {
                0
            };
            match actual {
                Some(value) => value == expected,
                None => false
            }
        }
        compare(Some(7), true);
        "#,
    );
    assert!(
        maybe.contains("may be unavailable"),
        "conditionally initialized local must stay unavailable, got {maybe}"
    );

    let wrong_scrutinee = reject(
        r#"
        fn compare(actual: int, expected: int) -> bool {
            match actual {
                Some(value) => value == expected,
                None => false
            }
        }
        compare(7, 7);
        "#,
    );
    assert!(
        wrong_scrutinee.contains("expected Option<T>"),
        "wrong scrutinee must stay rejected, got {wrong_scrutinee}"
    );

    let wrong_payload = reject(
        r#"
        fn bad() -> Option<int> {
            Some("no")
        }
        bad();
        "#,
    );
    assert!(
        wrong_payload.contains("assigned string"),
        "wrong Option payload must stay rejected, got {wrong_payload}"
    );
}
