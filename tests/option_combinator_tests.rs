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
fn map_preserves_known_named_return_schema_and_materializes_omitted_option_field() {
    let source = r#"
        struct Entry { name: string, note: Option<int> }
        let flag: Option<int> = Option::Some(1);
        let item: Option<Entry> = flag.map(|n| { name: "x" });
        let out = match item {
            Option::Some(entry) => match entry.note {
                Option::Some(value) => value,
                Option::None => 0,
            },
            Option::None => -1,
        };
        out;
    "#;
    assert_eq!(eval(source), Value::Int(0));
}

#[test]
fn unwrap_or_preserves_known_named_payload_schema_and_materializes_omitted_option_field() {
    let source = r#"
        struct Entry { name: string, note: Option<int> }
        let flag: Option<Entry> = Option::None;
        let item: Entry = flag.unwrap_or({ name: "x" });
        let projected = item?.note;
        let out = match projected {
            Option::Some(inner) => match inner {
                Option::Some(value) => value,
                Option::None => 1,
            },
            Option::None => 2,
        };
        out;
    "#;
    assert_eq!(eval(source), Value::Int(1));
}

#[test]
fn methods_on_a_non_option_receiver_report_the_option_requirement() {
    let diagnostic = reject("let value: int = 1; value.map(|item| item + 1);");
    assert!(
        diagnostic.contains("Option") || diagnostic.contains("Result"),
        "non-container receiver must name the required owner: {diagnostic}"
    );
}

#[test]
fn map_function_return_known_named_schema_materializes_omitted_option_field() {
    let source = r#"
        struct Entry { name: string, note: Option<int> }
        fn make(flag: Option<int>) -> Option<Entry> {
            flag.map(|n| { name: "x" })
        }
        let flag: Option<int> = Option::Some(1);
        let item = make(flag);
        let out = match item {
            Option::Some(entry) => match entry.note {
                Option::Some(value) => value,
                Option::None => 0,
            },
            Option::None => -1,
        };
        out;
    "#;
    assert_eq!(eval(source), Value::Int(0));
}

#[test]
fn map_generic_nested_named_payload_materializes_all_omitted_option_fields() {
    let source = r#"
        struct Entry { name: string, note: Option<int> }
        struct Box<T> { value: T, note: Option<int> }
        fn make(flag: Option<int>) -> Option<Box<Entry>> {
            flag.map(|n| { value: { name: "x" } })
        }
        fn observe_note(note: Option<Option<int>>) -> int {
            match note {
                Option::Some(inner_note) => match inner_note {
                    Option::Some(value) => value,
                    Option::None => 1,
                },
                Option::None => -2,
            }
        }
        fn observe_box(item: Option<Box<Entry>>) -> int {
            match item {
                Option::Some(inner) => observe_note(inner?.note),
                Option::None => -1,
            }
        }
        fn entry_note(value: Entry) -> Option<Option<int>> {
            value?.note
        }
        fn observe_entry(item: Option<Box<Entry>>) -> int {
            match item {
                Option::Some(inner) => observe_note(entry_note(inner.value)),
                Option::None => -1,
            }
        }
        observe_box(make(Option::Some(1))) + observe_entry(make(Option::Some(1)));
    "#;
    // Both the Box.note and nested Entry.note materialize as tagged None.
    assert_eq!(eval(source), Value::Int(2));
}

#[test]
fn map_conditional_and_block_callbacks_keep_the_named_return_context() {
    let source = r#"
        struct Entry { name: string, note: Option<int> }
        let flag: Option<int> = Option::Some(1);
        let item: Option<Entry> = flag.map(if true => {
            |n| { name: "chosen" }
        } else => {
            |n| { name: "other" }
        });
        let out = match item {
            Option::Some(entry) => match entry.note {
                Option::Some(value) => value,
                Option::None => 0,
            },
            Option::None => -1,
        };
        out;
    "#;
    assert_eq!(eval(source), Value::Int(0));
}

#[test]
fn map_named_function_callback_keeps_its_declared_return_context() {
    let source = r#"
        struct Entry { name: string, note: Option<int> }
        fn build(n: int) -> Entry {
            { name: "built" }
        }
        let flag: Option<int> = Option::Some(1);
        let item: Option<Entry> = flag.map(build);
        let out = match item {
            Option::Some(entry) => match entry.note {
                Option::Some(value) => value,
                Option::None => 0,
            },
            Option::None => -1,
        };
        out;
    "#;
    assert_eq!(eval(source), Value::Int(0));
}

#[test]
fn map_prebound_annotated_callback_keeps_its_declared_return_context() {
    let source = r#"
        struct Entry { name: string, note: Option<int> }
        let callback: fn(int) -> Entry = |n| { name: "bound" };
        let flag: Option<int> = Option::Some(1);
        let item: Option<Entry> = flag.map(callback);
        let out = match item {
            Option::Some(entry) => match entry.note {
                Option::Some(value) => value,
                Option::None => 0,
            },
            Option::None => -1,
        };
        out;
    "#;
    assert_eq!(eval(source), Value::Int(0));
}

#[test]
fn map_known_return_schema_preserves_callback_captures_and_single_body_run() {
    let source = r#"
        struct Entry { name: string, note: Option<int> }
        let mut calls = [0];
        let flag: Option<int> = Option::Some(1);
        let item: Option<Entry> = flag.map(|n| if true => {
            calls[0] = calls[0].copy() + 1;
            { name: "x" }
        } else => {
            { name: "y" }
        });
        let out = match item {
            Option::Some(entry) => match entry.note {
                Option::Some(value) => value,
                Option::None => 0,
            },
            Option::None => -1,
        };
        out + calls[0].copy();
    "#;
    assert_eq!(eval(source), Value::Int(1));
}

#[test]
fn unwrap_or_else_named_payload_keeps_the_body_lazy_and_materializes_none() {
    let source = r#"
        struct Entry { name: string, note: Option<int> }
        let mut calls = [0];
        let present: Option<Entry> = Option::Some({ name: "p", note: Option::Some(1) });
        let absent: Option<Entry> = Option::None;
        let a: Entry = present.unwrap_or_else(|| if true => {
            calls[0] = calls[0].copy() + 1;
            { name: "a" }
        } else => {
            { name: "unused" }
        });
        let b: Entry = absent.unwrap_or_else(|| if true => {
            calls[0] = calls[0].copy() + 1;
            { name: "b" }
        } else => {
            { name: "unused" }
        });
        let a_note = match a.note { Option::Some(value) => value, Option::None => -1 };
        let b_note = match b?.note {
            Option::Some(inner_note) => match inner_note {
                Option::Some(value) => value,
                Option::None => 1,
            },
            Option::None => -2,
        };
        a_note + b_note + calls[0].copy();
    "#;
    // The Some branch keeps its payload and skips the lazy body; the None
    // branch runs it once and materializes Entry.note as tagged None.
    assert_eq!(eval(source), Value::Int(3));
}

#[test]
fn and_then_and_or_else_named_option_payloads_materialize_omitted_option_fields() {
    let source = r#"
        struct Entry { name: string, note: Option<int> }
        fn observed(item: Option<Entry>) -> int {
            match item {
                Option::Some(entry) => match entry?.note {
                    Option::Some(inner_note) => match inner_note {
                        Option::Some(value) => value,
                        Option::None => 1,
                    },
                    Option::None => -2,
                },
                Option::None => -1,
            }
        }
        let flag: Option<int> = Option::Some(1);
        let chained: Option<Entry> = flag.and_then(|n| Option::Some({ name: "x" }));
        let absent: Option<Entry> = Option::None;
        let recovered: Option<Entry> = absent.or_else(|| Option::Some({ name: "y" }));
        observed(chained) + observed(recovered);
    "#;
    assert_eq!(eval(source), Value::Int(2));
}

#[test]
fn ok_or_and_ok_or_else_named_errors_materialize_omitted_option_fields() {
    let source = r#"
        struct Failure { code: int, detail: Option<int> }
        fn observed(result: Result<int, Failure>) -> int {
            match result {
                Result::Ok(value) => -1,
                Result::Err(error) => match error?.detail {
                    Option::Some(inner_detail) => match inner_detail {
                        Option::Some(value) => value,
                        Option::None => 1,
                    },
                    Option::None => -2,
                },
            }
        }
        let present: Option<int> = Option::Some(1);
        let absent: Option<int> = Option::None;
        let eager: Result<int, Failure> = absent.ok_or({ code: 7 });
        let lazy: Result<int, Failure> = absent.ok_or_else(|| { code: 8 });
        observed(eager) + observed(lazy) + match present.ok_or({ code: 9 }) {
            Result::Ok(value) => value,
            Result::Err(error) => -1,
        };
    "#;
    assert_eq!(eval(source), Value::Int(3));
}

#[test]
fn nonconstruction_sparse_object_cannot_be_annotated_as_named_record() {
    let diagnostic = reject(
        r#"
        struct Entry { name: string, note: Option<int> }
        let raw = { name: "x" };
        let item: Entry = raw;
        item.name;
        "#,
    );
    assert!(
        diagnostic.contains("field 'note'")
            && diagnostic.contains("non-construction")
            && diagnostic.contains("must be materialized"),
        "the missing-field obligation must be asserted at the compile boundary: {diagnostic}"
    );
}

#[test]
fn nonconstruction_sparse_object_cannot_be_an_ordinary_record_argument() {
    let diagnostic = reject(
        r#"
        struct Entry { name: string, note: Option<int> }
        fn read(entry: Entry) -> string { entry.name }
        let raw = { name: "x" };
        read(raw);
        "#,
    );
    assert!(
        diagnostic.contains("field 'note'") && diagnostic.contains("must be materialized"),
        "an ordinary record argument must carry the same missing-field obligation: {diagnostic}"
    );
}

#[test]
fn nonconstruction_sparse_object_cannot_be_an_eager_unwrap_or_fallback() {
    let diagnostic = reject(
        r#"
        struct Entry { name: string, note: Option<int> }
        let flag: Option<Entry> = Option::None;
        let raw = { name: "x" };
        let item: Entry = flag.unwrap_or(raw);
        item.name;
        "#,
    );
    assert!(
        diagnostic.contains("unwrap_or")
            && diagnostic.contains("field 'note'")
            && diagnostic.contains("must be materialized"),
        "the eager fallback must be rejected before the named label is installed: {diagnostic}"
    );
}

#[test]
fn nonconstruction_sparse_object_cannot_be_a_map_callback_payload() {
    let diagnostic = reject(
        r#"
        struct Entry { name: string, note: Option<int> }
        let flag: Option<int> = Option::Some(1);
        let raw = { name: "x" };
        let item: Option<Entry> = flag.map(|n| raw);
        item;
        "#,
    );
    assert!(
        diagnostic.contains("map")
            && diagnostic.contains("field 'note'")
            && diagnostic.contains("must be materialized"),
        "a callback payload that is a plain variable must not be relabelled: {diagnostic}"
    );
}

#[test]
fn nested_nonconstruction_sparse_object_is_not_excused_by_an_ancestor_literal() {
    let diagnostic = reject(
        r#"
        struct Inner { name: string, note: Option<int> }
        struct Outer { inner: Option<Inner> }
        let raw = { name: "x" };
        let outer: Outer = { inner: Option::Some(raw) };
        outer;
        "#,
    );
    assert!(
        diagnostic.contains("inner.note") && diagnostic.contains("must be materialized"),
        "the nested value position, not the ancestor literal, owns the obligation: {diagnostic}"
    );
}

#[test]
fn unwrap_or_keeps_a_previously_materialized_named_fallback() {
    let source = r#"
        struct Entry { name: string, note: Option<int> }
        let fallback: Entry = { name: "x" };
        let flag: Option<Entry> = Option::None;
        let item: Entry = flag.unwrap_or(fallback);
        match item.note { Option::Some(value) => value, Option::None => 0 };
    "#;
    assert_eq!(eval(source), Value::Int(0));
}

#[test]
fn conditional_callbacks_with_distinct_present_key_shapes_materialize_the_known_record() {
    let source = r#"
        struct Entry { name: string, note: Option<int> }
        fn make(flag: Option<int>, select: bool) -> Option<Entry> {
            flag.map(if select => {
                |n| { name: "x" }
            } else => {
                |n| { name: "y", note: Option::None }
            })
        }
        let item: Entry = make(Option::Some(1), true).unwrap_or({ name: "fallback" });
        match item.note { Option::Some(value) => value, Option::None => 0 };
    "#;
    assert_eq!(eval(source), Value::Int(0));
}

#[test]
fn match_callbacks_with_distinct_present_key_shapes_materialize_the_known_record() {
    let source = r#"
        struct Entry { name: string, note: Option<int> }
        fn make(flag: Option<int>, select: int) -> Option<Entry> {
            flag.map(match select {
                1 => |n| { name: "x" },
                _ => |n| { name: "y", note: Option::Some(1) },
            })
        }
        let item: Entry = make(Option::Some(1), 1).unwrap_or({ name: "fallback" });
        match item.note { Option::Some(value) => value, Option::None => 0 };
    "#;
    assert_eq!(eval(source), Value::Int(0));
}

#[test]
fn conditional_callback_missing_a_required_field_is_rejected() {
    let diagnostic = reject(
        r#"
        struct Entry { name: string, note: Option<int> }
        fn make(flag: Option<int>, select: bool) -> Option<Entry> {
            flag.map(if select => {
                |n| { name: "x" }
            } else => {
                |n| { note: Option::None }
            })
        }
        make(Option::Some(1), true);
        "#,
    );
    assert!(
        diagnostic.contains("field 'name'") && diagnostic.contains("required"),
        "a branch missing a required field must keep the focused missing-field rejection: {diagnostic}"
    );
}

#[test]
fn conditional_callback_with_a_wrong_field_type_is_rejected() {
    let diagnostic = reject(
        r#"
        struct Entry { name: string, note: Option<int> }
        fn make(flag: Option<int>, select: bool) -> Option<Entry> {
            flag.map(if select => {
                |n| { name: "x" }
            } else => {
                |n| { name: 2 }
            })
        }
        make(Option::Some(1), true);
        "#,
    );
    assert!(
        diagnostic.contains("field 'name'") && diagnostic.contains("string"),
        "a wrong branch field type must keep the focused schema rejection: {diagnostic}"
    );
}

#[test]
fn expression_block_local_sparse_object_cannot_be_a_declared_return() {
    let diagnostic = reject(
        r#"
        struct Entry { name: string, note: Option<int> }
        fn make(flag: bool) -> Entry {
            if flag => {
                let raw = { name: "x" };
                raw
            } else => {
                { name: "y" }
            }
        }
        let item: Entry = make(true);
        match item.note { Option::Some(value) => value, Option::None => 0 };
        "#,
    );
    assert!(
        diagnostic.contains("field 'note'") && diagnostic.contains("must be materialized"),
        "a sparse object bound inside the expression block and returned as a declared record must keep the missing-field obligation: {diagnostic}"
    );
}

#[test]
fn expression_block_local_sparse_alias_cannot_be_a_declared_return() {
    let diagnostic = reject(
        r#"
        struct Entry { name: string, note: Option<int> }
        fn make(flag: bool) -> Entry {
            if flag => {
                let raw = { name: "x" };
                let alias = raw;
                alias
            } else => {
                { name: "y" }
            }
        }
        let item: Entry = make(true);
        match item.note { Option::Some(value) => value, Option::None => 0 };
        "#,
    );
    assert!(
        diagnostic.contains("field 'note'") && diagnostic.contains("must be materialized"),
        "a block-local alias must not hide the missing-field obligation: {diagnostic}"
    );
}

#[test]
fn expression_block_literal_tail_materializes_the_omitted_option_field() {
    let source = r#"
        struct Entry { name: string, note: Option<int> }
        fn make(flag: bool) -> Entry {
            if flag => {
                let name = "x";
                { name: name }
            } else => {
                { name: "y" }
            }
        }
        let item: Entry = make(true);
        match item.note { Option::Some(value) => value, Option::None => 0 };
    "#;
    assert_eq!(eval(source), Value::Int(0));
}

#[test]
fn expression_block_materialized_variable_tail_keeps_the_named_record() {
    let source = r#"
        struct Entry { name: string, note: Option<int> }
        fn make(flag: bool) -> Entry {
            if flag => {
                let entry: Entry = { name: "x" };
                entry
            } else => {
                { name: "y" }
            }
        }
        let item: Entry = make(true);
        match item.note { Option::Some(value) => value, Option::None => 0 };
    "#;
    assert_eq!(eval(source), Value::Int(0));
}
