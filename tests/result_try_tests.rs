#![cfg(feature = "runtime")]

use vm::{Vm, VmStatus, compile_source};

fn eval(source: &str) -> vm::Value {
    let compiled = compile_source(source).expect("Result try source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_fuel(100_000);
    assert_eq!(
        vm.run().expect("Result try program should run"),
        VmStatus::Halted
    );
    vm.stack()
        .last()
        .cloned()
        .expect("Result try program should leave a value")
}

#[test]
fn err_propagation_returns_original_error_and_skips_later_effect() {
    let source = r#"
        let mut calls = [0];
        fn fail() -> Result<int, string> {
            Result::Err("bad")
        }
        fn relay() -> Result<string, string> {
            let x: int = fail()?;
            calls[0] = calls[0].copy() + 1;
            Result::Ok("unreachable")
        }
        let text = match relay() {
            Result::Ok(v) => v,
            Result::Err(e) => e,
        };
        let flag = if text == "bad" && calls[0] == 0 => { 1 } else => { 0 };
        flag;
    "#;
    assert_eq!(eval(source), vm::Value::Int(1));
}

fn reject(source: &str) -> vm::SourceError {
    match compile_source(source) {
        Err(error) => error,
        Ok(_) => panic!("Result try source should be rejected: {source}"),
    }
}

fn detail(error: &vm::SourceError) -> String {
    error.to_string()
}

fn assert_question_span(source: &str, error: &vm::SourceError) {
    let vm::SourceError::Compile(vm::CompileError::StrictTypingRequired { span, .. }) = error
    else {
        panic!("expected a spanned try diagnostic, got {error:?}");
    };
    let span = span.expect("try diagnostic should keep the ? span");
    let lo = span.lo as usize;
    let hi = span.hi as usize;
    assert_eq!(
        hi,
        lo + 1,
        "try diagnostic should cover only '?', got {span:?}"
    );
    assert_eq!(&source[lo..hi], "?", "diagnostic span should point at '?'");
}

#[test]
fn ok_propagation_changes_success_type_and_keeps_later_effect() {
    let source = r#"
        let mut calls = [0];
        fn pass() -> Result<int, string> {
            Result::Ok(7)
        }
        fn relay() -> Result<string, string> {
            let x: int = pass()?;
            calls[0] = calls[0].copy() + x;
            Result::Ok("kept")
        }
        let text = match relay() {
            Result::Ok(v) => v,
            Result::Err(e) => e,
        };
        let flag = if text == "kept" && calls[0] == 7 => { 1 } else => { 0 };
        flag;
    "#;
    assert_eq!(eval(source), vm::Value::Int(1));
}

#[test]
fn try_diagnostics_reject_root_non_result_wrong_error_and_keep_question_span() {
    let root = "let value: Result<int, string> = Result::Err(\"bad\");\nlet x: int = value?;\nx;";
    let root_error = reject(root);
    assert!(
        detail(&root_error).contains("enclosing function"),
        "{root_error:?}"
    );
    assert_question_span(root, &root_error);

    let plain = "fn plain() -> int {\n    let value: Result<int, string> = Result::Err(\"bad\");\n    let x: int = value?;\n    x\n}\nplain;";
    let plain_error = reject(plain);
    assert!(
        detail(&plain_error).contains("enclosing function"),
        "{plain_error:?}"
    );
    assert_question_span(plain, &plain_error);

    let operand = "fn relay() -> Result<string, string> {\n    let x: int = 1?;\n    Result::Ok(\"no\")\n}\nrelay;";
    let operand_error = reject(operand);
    assert!(
        detail(&operand_error).contains("operand must be Result"),
        "{operand_error:?}"
    );
    assert_question_span(operand, &operand_error);

    let wrong = "fn fail() -> Result<int, int> { Result::Err(1) }\nfn relay() -> Result<string, string> {\n    let x: int = fail()?;\n    Result::Ok(\"no\")\n}\nrelay;";
    let wrong_error = reject(wrong);
    assert!(
        detail(&wrong_error).contains("error type"),
        "{wrong_error:?}"
    );
    assert_question_span(wrong, &wrong_error);
}

#[test]
fn optional_access_and_optional_type_remain_distinct_from_try() {
    let source = r#"
        struct Age { first: int }
        struct User { age: Age }
        let user: User = { age: { first: 41 } };
        let age = user?.age;
        let first = age?.first;
        first;
    "#;
    assert_eq!(eval(source), vm::Value::Int(41));
    let optional_type = r#"
        let maybe: int? = 1;
        maybe.unwrap_or(0);
    "#;
    assert_eq!(eval(optional_type), vm::Value::Int(1));
}

#[test]
fn default_dialect_rejects_plain_try_and_keeps_optional_access() {
    struct DefaultDialect;
    impl vm::ParserDialect for DefaultDialect {}
    static DIALECT: DefaultDialect = DefaultDialect;
    let options = vm::SharedParserOptions {
        source_id: 0,
        allow_implicit_externs: false,
        allow_implicit_semicolons: false,
        enforce_mutable_bindings: true,
        import_scan_mode: false,
    };
    let rejected = vm::parse_source_with_dialect("1?", &DIALECT, options)
        .expect_err("default dialect must reject plain ?");
    assert!(
        rejected.message.contains("expected '.' after '?'"),
        "{rejected:?}"
    );
}

#[test]
fn closure_try_exits_only_the_closure() {
    let source = r#"
        let mut outer = [0];
        fn fail() -> Result<int, string> {
            Result::Err("bad")
        }
        fn relay() -> Result<string, string> {
            let inner: fn() -> Result<int, string> = || Result::Ok(fail()?);
            let text = match inner() {
                Result::Ok(v) => "ok",
                Result::Err(e) => e,
            };
            outer[0] = outer[0].copy() + 1;
            Result::Ok(text)
        }
        let text = match relay() {
            Result::Ok(v) => v,
            Result::Err(e) => e,
        };
        let flag = if text == "bad" && outer[0] == 1 => { 1 } else => { 0 };
        flag;
    "#;
    assert_eq!(eval(source), vm::Value::Int(1));
}

#[test]
fn call_arguments_stop_after_err_and_run_in_order_on_ok() {
    let err_source = r#"
        let mut log = [0];
        fn tick(n: int) -> int {
            log[0] = log[0].copy() * 10 + n;
            n
        }
        fn fail() -> Result<int, string> {
            Result::Err("bad")
        }
        fn f(a: int, mid: int, b: int) -> int {
            log[0] = log[0].copy() * 10 + 4;
            a + mid + b
        }
        fn relay() -> Result<int, string> {
            let value: int = f(tick(1), fail()?, tick(3));
            Result::Ok(value)
        }
        let text = match relay() {
            Result::Ok(v) => "ok",
            Result::Err(e) => e,
        };
        let flag = if text == "bad" && log[0] == 1 => { 1 } else => { 0 };
        flag;
    "#;
    assert_eq!(eval(err_source), vm::Value::Int(1));

    let ok_source = r#"
        let mut log = [0];
        fn tick(n: int) -> int {
            log[0] = log[0].copy() * 10 + n;
            n
        }
        fn pass() -> Result<int, string> {
            Result::Ok(2)
        }
        fn f(a: int, mid: int, b: int) -> int {
            log[0] = log[0].copy() * 10 + 4;
            a + mid + b
        }
        fn relay() -> Result<int, string> {
            let value: int = f(tick(1), pass()?, tick(3));
            Result::Ok(value)
        }
        let value = match relay() {
            Result::Ok(v) => v,
            Result::Err(e) => -1,
        };
        let flag = if value == 6 && log[0] == 134 => { 1 } else => { 0 };
        flag;
    "#;
    assert_eq!(eval(ok_source), vm::Value::Int(1));
}

#[test]
fn err_stops_unexecuted_siblings_in_arithmetic_if_and_loop() {
    let source = r#"
        let mut log = [0];
        fn tick(n: int) -> int {
            log[0] = log[0].copy() * 10 + n;
            n
        }
        fn fail() -> Result<int, string> {
            Result::Err("bad")
        }
        fn relay() -> Result<int, string> {
            let sum: int = tick(1) + fail()? + tick(2);
            let picked = if false => { tick(3) } else => { sum };
            while false {
                tick(4);
            }
            Result::Ok(picked)
        }
        let text = match relay() {
            Result::Ok(v) => "ok",
            Result::Err(e) => e,
        };
        let flag = if text == "bad" && log[0] == 1 => { 1 } else => { 0 };
        flag;
    "#;
    assert_eq!(eval(source), vm::Value::Int(1));
}

#[test]
fn generic_try_substitutes_error_type() {
    let source = r#"
        fn fail<E>(err: E) -> Result<int, E> {
            Result::Err(err)
        }
        fn relay() -> Result<string, string> {
            let x: int = fail::<string>("bad")?;
            Result::Ok("no")
        }
        let text = match relay() {
            Result::Ok(v) => v,
            Result::Err(e) => e,
        };
        let flag = if text == "bad" => { 1 } else => { 0 };
        flag;
    "#;
    assert_eq!(eval(source), vm::Value::Int(1));
}

#[test]
fn typed_frontend_ir_records_result_try_schemas() {
    let source = r#"
        fn fail() -> Result<int, string> {
            Result::Err("bad")
        }
        fn relay() -> Result<string, string> {
            let x: int = fail()?;
            Result::Ok("no")
        }
        relay;
    "#;
    let model = vm::analyze_source(source).expect("typed analysis should accept try");
    let mut found = false;
    for function in model.ir().function_impls.values() {
        for stmt in &function.body_stmts {
            if let vm::Stmt::Let {
                expr:
                    vm::Expr::ResultTry {
                        operand_schema,
                        return_schema,
                        ..
                    },
                ..
            } = stmt
            {
                assert!(matches!(
                    operand_schema,
                    vm::TypeSchema::Result { ok, err }
                        if matches!(ok.as_ref(), vm::TypeSchema::Int)
                            && matches!(err.as_ref(), vm::TypeSchema::String)
                ));
                assert!(matches!(
                    return_schema,
                    vm::TypeSchema::Result { ok, err }
                        if matches!(ok.as_ref(), vm::TypeSchema::String)
                            && matches!(err.as_ref(), vm::TypeSchema::String)
                ));
                found = true;
            }
        }
    }
    assert!(found, "typed FrontendIr should contain ResultTry");
}

fn eval_aot(source: &str) -> vm::Value {
    let compiled = compile_source(source).expect("AOT Result try source should compile");
    let mut machine = vm::Vm::new_with_jit_config(
        compiled.program.with_local_count(compiled.locals),
        vm::JitConfig {
            enabled: false,
            hot_loop_threshold: 1,
            max_trace_len: 512,
        },
    );
    machine.set_fuel(100_000);
    machine
        .compile_aot()
        .expect("Result try should compile to AOT");
    let dump = machine.dump_aot_info();
    assert!(dump.contains("lowering=ssa"), "{dump}");
    assert_eq!(
        machine.run().expect("AOT Result try should run"),
        VmStatus::Halted
    );
    assert!(machine.aot_exec_count() > 0, "{dump}");
    machine
        .stack()
        .last()
        .cloned()
        .expect("AOT Result try should leave a value")
}

#[test]
fn aot_executes_ok_and_err_try_payloads() {
    let err = r#"
        fn fail() -> Result<int, string> { Result::Err("bad") }
        fn relay() -> Result<string, string> {
            let x: int = fail()?;
            Result::Ok("no")
        }
        let text = match relay() {
            Result::Ok(v) => v,
            Result::Err(e) => e,
        };
        let flag = if text == "bad" => { 1 } else => { 0 };
        flag;
    "#;
    assert_eq!(eval_aot(err), vm::Value::Int(1));
    let ok = r#"
        fn pass() -> Result<int, string> { Result::Ok(4) }
        fn relay() -> Result<string, string> {
            let x: int = pass()?;
            Result::Ok("kept")
        }
        let text = match relay() {
            Result::Ok(v) => v,
            Result::Err(e) => e,
        };
        let flag = if text == "kept" => { 1 } else => { 0 };
        flag;
    "#;
    assert_eq!(eval_aot(ok), vm::Value::Int(1));
}

#[test]
fn nested_recursive_and_aliased_calls_exit_only_the_lexical_callee() {
    let nested = r#"
        fn outer() -> Result<string, string> {
            fn inner() -> Result<int, string> {
                Result::Err("bad")
            }
            fn relay() -> Result<string, string> {
                let x: int = inner()?;
                Result::Ok("no")
            }
            let text = match relay() {
                Result::Ok(v) => v,
                Result::Err(e) => e,
            };
            Result::Ok(text)
        }
        let text = match outer() {
            Result::Ok(v) => v,
            Result::Err(e) => e,
        };
        let flag = if text == "bad" => { 1 } else => { 0 };
        flag;
    "#;
    assert_eq!(eval(nested), vm::Value::Int(1));

    let recursive = r#"
        fn walk(n: int) -> Result<int, string> {
            if n == 0 => { Result::Err("bad") } else => { Result::Ok(walk(n - 1)?) }
        }
        fn relay() -> Result<string, string> {
            let x: int = walk(2)?;
            Result::Ok("no")
        }
        let mut after = [0];
        let text = match relay() {
            Result::Ok(v) => v,
            Result::Err(e) => e,
        };
        after[0] = 1;
        let flag = if text == "bad" && after[0] == 1 => { 1 } else => { 0 };
        flag;
    "#;
    assert_eq!(eval(recursive), vm::Value::Int(1));

    let aliased = r#"
        fn fail() -> Result<int, string> { Result::Err("bad") }
        fn relay() -> Result<string, string> {
            let alias: fn() -> Result<int, string> = fail;
            let x: int = alias()?;
            Result::Ok("no")
        }
        let text = match relay() {
            Result::Ok(v) => v,
            Result::Err(e) => e,
        };
        let flag = if text == "bad" => { 1 } else => { 0 };
        flag;
    "#;
    assert_eq!(eval(aliased), vm::Value::Int(1));
}

#[test]
fn err_stops_match_array_and_short_circuit_siblings() {
    let match_source = r#"
        let mut log = [0];
        fn tick(n: int) -> int {
            log[0] = log[0].copy() * 10 + n;
            n
        }
        fn fail() -> Result<int, string> { Result::Err("bad") }
        fn relay() -> Result<int, string> {
            let picked = match fail()? {
                0 => tick(2),
                _ => tick(3),
            };
            Result::Ok(picked)
        }
        let text = match relay() {
            Result::Ok(v) => "ok",
            Result::Err(e) => e,
        };
        let flag = if text == "bad" && log[0] == 0 => { 1 } else => { 0 };
        flag;
    "#;
    assert_eq!(eval(match_source), vm::Value::Int(1));
    let array_source = r#"
        let mut log = [0];
        fn tick(n: int) -> int {
            log[0] = log[0].copy() * 10 + n;
            n
        }
        fn fail() -> Result<int, string> { Result::Err("bad") }
        fn relay() -> Result<int, string> {
            let values = [tick(1), fail()?, tick(2)];
            Result::Ok(values[0])
        }
        let text = match relay() {
            Result::Ok(v) => "ok",
            Result::Err(e) => e,
        };
        let flag = if text == "bad" && log[0] == 1 => { 1 } else => { 0 };
        flag;
    "#;
    assert_eq!(eval(array_source), vm::Value::Int(1));
    let and_source = r#"
        let mut log = [0];
        fn tick(n: int) -> int {
            log[0] = log[0].copy() * 10 + n;
            n
        }
        fn fail() -> Result<int, string> { Result::Err("bad") }
        fn relay() -> Result<int, string> {
            let flag = if tick(1) == 1 && fail()? == 1 && tick(2) == 2 => { 1 } else => { 0 };
            Result::Ok(flag)
        }
        let text = match relay() {
            Result::Ok(v) => "ok",
            Result::Err(e) => e,
        };
        let flag = if text == "bad" && log[0] == 1 => { 1 } else => { 0 };
        flag;
    "#;
    assert_eq!(eval(and_source), vm::Value::Int(1));
}

#[test]
fn try_inside_executed_if_arm_and_loop_exits_the_function() {
    let if_source = r#"
        let mut log = [0];
        fn tick(n: int) -> int {
            log[0] = log[0].copy() * 10 + n;
            n
        }
        fn fail() -> Result<int, string> { Result::Err("bad") }
        fn relay() -> Result<string, string> {
            if true {
                let x: int = fail()?;
                tick(x);
            } else {
                tick(9);
            }
            tick(8);
            Result::Ok("no")
        }
        let text = match relay() {
            Result::Ok(v) => v,
            Result::Err(e) => e,
        };
        let flag = if text == "bad" && log[0] == 0 => { 1 } else => { 0 };
        flag;
    "#;
    assert_eq!(eval(if_source), vm::Value::Int(1));
    let loop_source = r#"
        let mut log = [0];
        fn tick(n: int) -> int {
            log[0] = log[0].copy() * 10 + n;
            n
        }
        fn fail() -> Result<int, string> { Result::Err("bad") }
        fn relay() -> Result<string, string> {
            let mut i = 0;
            while i < 3 {
                i = i + 1;
                let x: int = fail()?;
                tick(x);
            }
            tick(8);
            Result::Ok("no")
        }
        let text = match relay() {
            Result::Ok(v) => v,
            Result::Err(e) => e,
        };
        let flag = if text == "bad" && log[0] == 0 => { 1 } else => { 0 };
        flag;
    "#;
    assert_eq!(eval(loop_source), vm::Value::Int(1));
}

#[test]
fn optional_success_payload_is_not_a_definite_scalar() {
    let arithmetic = r#"
        fn maybe() -> Result<int?, string> { Result::Ok(null) }
        fn relay() -> Result<int, string> {
            Result::Ok(maybe()? + 1)
        }
        relay();
    "#;
    let error = reject(arithmetic);
    let text = detail(&error);
    assert!(
        text.contains("optional") || text.contains("null"),
        "selected optional payload must not be used as a definite int, got {error:?}"
    );
}

#[test]
fn optional_success_payload_unwrap_keeps_null_and_nonnull() {
    let nonnull = r#"
        fn maybe() -> Result<int?, string> { Result::Ok(4) }
        fn relay() -> Result<int, string> {
            let inner: int? = maybe()?;
            Result::Ok(inner.unwrap_or(0) + 1)
        }
        let value = match relay() {
            Result::Ok(v) => v,
            Result::Err(_) => 0,
        };
        value;
    "#;
    assert_eq!(eval(nonnull), vm::Value::Int(5));
    let null_ok = r#"
        fn maybe() -> Result<int?, string> { Result::Ok(null) }
        fn relay() -> Result<int, string> {
            let inner: int? = maybe()?;
            Result::Ok(inner.unwrap_or(9))
        }
        let value = match relay() {
            Result::Ok(v) => v,
            Result::Err(_) => 0,
        };
        value;
    "#;
    assert_eq!(eval(null_ok), vm::Value::Int(9));
}

#[test]
fn optional_result_operand_is_rejected_at_try_boundary() {
    let source = r#"
        fn relay(value: Result<int, string>?) -> Result<int, string> {
            Result::Ok(value?)
        }
        relay;
    "#;
    let error = reject(source);
    assert!(
        detail(&error).contains("operand must be Result"),
        "{error:?}"
    );
    assert_question_span(source, &error);
}

#[test]
fn optional_try_payload_cannot_flow_to_required_let_or_argument() {
    let head =
        "fn maybe() -> Result<int?, string> { Result::Ok(null) }\nfn take(n: int) -> int { n }\n";
    for body in [
        "let required: int = maybe()?; Result::Ok(required)",
        "Result::Ok(take(maybe()?))",
        "let selected = maybe()?; let required: int = selected; Result::Ok(required)",
        "let selected = maybe()?; Result::Ok(take(selected))",
        "let selected = maybe()?; let callback: fn() -> int = || selected; Result::Ok(callback())",
        "let selected = maybe()?; fn captured() -> int { take(selected) } Result::Ok(captured())",
    ] {
        let source = format!("{head} fn relay() -> Result<int, string> {{ {body} }} relay();");
        let error = reject(&source);
        let text = detail(&error);
        assert!(
            text.contains("optional")
                || text.contains("null")
                || text.contains("annotation")
                || text.contains("schema"),
            "{body}: {error:?}"
        );
    }
}

#[test]
fn optional_try_refinement_keeps_both_payload_states() {
    for (payload, expected) in [("null", 9), ("4", 5)] {
        let source = format!(
            r#"
            fn maybe() -> Result<int?, string> {{ Result::Ok({payload}) }}
            fn relay() -> Result<int, string> {{
                let selected = maybe()?;
                let n = if selected != null => {{ selected + 1 }} else => {{ 9 }};
                Result::Ok(n)
            }}
            let n = match relay() {{ Result::Ok(v) => v, Result::Err(_) => 0, }};
            n;
        "#
        );
        assert_eq!(eval(&source), vm::Value::Int(expected));
    }
}

#[test]
fn trace_jit_try_in_hot_region_preserves_payload_and_order() {
    let mut region_failures = Vec::new();
    for fail_at in [1000, 80] {
        let source = format!(
            r#"
            fn relay() -> Result<int, int> {{
                let mut i = 0;
                let mut total = 0;
                let mut carrier: Result<int, int> = Result::Ok(1);
                while i < 100 {{
                    let value = carrier?;
                    total = total + value;
                    i = i + 1;
                    if i == {fail_at} {{ carrier = Result::Err(total); }}
                }}
                Result::Ok(total)
            }}
            relay();
        "#
        );
        let compiled = compile_source(&source).expect("Trace Try source admission");
        let mut machine = Vm::new_with_jit_config(
            compiled.program.with_local_count(compiled.locals),
            vm::JitConfig {
                enabled: true,
                hot_loop_threshold: 1,
                max_trace_len: 512,
            },
        );
        machine.set_fuel(1_000_000);
        assert_eq!(
            machine.run().expect("Trace Try execution"),
            VmStatus::Halted
        );
        let expected = if fail_at == 80 { 80 } else { 100 };
        let vm::Value::Map(result) = machine.stack().last().expect("Result carrier") else {
            panic!("missing carrier")
        };
        assert_eq!(
            result.get(&vm::Value::string("tag")),
            Some(&vm::Value::Bool(fail_at != 80))
        );
        assert_eq!(
            result.get(&vm::Value::string("payload")),
            Some(&vm::Value::Int(expected))
        );
        let snapshot = machine.jit_snapshot();
        let dump = machine.dump_jit_info();
        println!(
            "fail_at={fail_at} native={}\n{dump}",
            machine.jit_native_exec_count()
        );
        assert!(
            machine.jit_native_exec_count() > 0,
            "Try loop did not execute natively: {dump}"
        );
        // A BranchExit region is sufficient: the Try tag/payload reads and
        // guard execute natively; destructive payload extraction and Err Ret
        // can resume in the interpreter. Do not claim a direct native Ret.
        if !snapshot.traces.iter().any(|trace| {
            trace.executions > 0
                && trace.op_names().iter().any(|op| op == "map_get")
                && trace.ssa_text().contains("String(\"tag\")")
                && trace.ssa_text().contains("String(\"payload\")")
                && trace.ssa_text().contains("branch_bool")
        }) {
            region_failures.push(format!(
                "fail_at={fail_at}: executing Try tag/payload/guard region missing; attempts={:?}",
                snapshot.attempts
            ));
        }
    }
    assert!(region_failures.is_empty(), "{}", region_failures.join("\n"));
}

#[test]
fn optional_entry_parameter_stays_optional_across_try_selection() {
    let negative = r#"
        fn relay(value: int?) -> Result<int, string> {
            let carrier: Result<int?, string> = Result::Ok(value);
            let required: int = carrier?;
            Result::Ok(required)
        }
        relay(null);
    "#;
    let error = reject(negative);
    assert!(
        detail(&error).contains("optional") || detail(&error).contains("annotation"),
        "{error:?}"
    );
    for (payload, expected) in [("null", 9), ("4", 5)] {
        let source = format!(
            r#"
            fn relay(value: int?) -> Result<int, string> {{
                let carrier: Result<int?, string> = Result::Ok(value);
                let selected = carrier?;
                let n = if selected != null => {{ selected + 1 }} else => {{ 9 }};
                Result::Ok(n)
            }}
            let n = match relay({payload}) {{ Result::Ok(v) => v, Result::Err(_) => 0, }};
            n;
        "#
        );
        assert_eq!(eval(&source), vm::Value::Int(expected));
    }
}

struct TryModuleFixture(std::path::PathBuf);
impl TryModuleFixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("r2-sol-import-{}-{id}", std::process::id()));
        std::fs::create_dir(&root).expect("portable module fixture");
        Self(root)
    }
    fn write(&self, name: &str, source: &str) -> std::path::PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, source).expect("module source");
        path
    }
}
impl Drop for TryModuleFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn actual_import_try_remaps_generic_named_error_and_rejects_distinct_identity() {
    let fixture = TryModuleFixture::new();
    let module = r#"
        struct Failure<T> { value: T }
        struct Other<T> { value: T }
        fn fail<E>(error: E) -> Result<int, E> { Result::Err(error) }
        pub fn relay() -> Result<int, Failure<string>> {
            let padding = 31;
            let error: Failure<string> = { value: "import-error" };
            let n = fail::<Failure<string>>(error)?;
            Result::Ok(n + padding)
        }
    "#;
    fixture.write("relay.rss", module);
    let entry = fixture.write(
        "main.rss",
        r#"
        use self::relay as imported;
        let padding = [4, 5, 6];
        fn before(n: int) -> int { n + 1 }
        let keep = before(padding[0]);
        let text = match imported::relay() { Result::Ok(_) => "wrong", Result::Err(e) => e.value, };
        let correct = if text == "import-error" && keep == 5 => { 1 } else => { 0 };
        correct;
    "#,
    );
    let compiled = vm::compile_source_file(&entry).expect("entry imports function containing Try");
    assert!(compiled.locals > 0);
    assert!(
        compiled
            .program
            .named_struct_decls()
            .keys()
            .any(|name| name.starts_with("__guest_type_"))
    );
    let mut machine = Vm::new(compiled.program.with_local_count(compiled.locals));
    machine.set_fuel(100_000);
    assert_eq!(
        machine.run().expect("imported Try runtime"),
        VmStatus::Halted
    );
    assert_eq!(machine.stack().last(), Some(&vm::Value::Int(1)));
    fixture.write(
        "relay.rss",
        &module.replace(
            "pub fn relay() -> Result<int, Failure<string>>",
            "pub fn relay() -> Result<int, Other<string>>",
        ),
    );
    let error = vm::compile_source_file(&entry)
        .err()
        .expect("same-shape distinct Named E must reject");
    assert!(error.to_string().contains("error type"), "{error:?}");
}

#[test]
fn actual_import_try_selects_int_and_string_host_signatures_and_payload_order() {
    use std::sync::{Arc, Mutex};
    struct Pick(Arc<Mutex<Vec<vm::Value>>>);
    impl vm::HostFunction for Pick {
        fn call(&mut self, _: &mut Vm, args: &[vm::Value]) -> vm::VmResult<vm::CallOutcome> {
            self.0.lock().unwrap().push(args[0].clone());
            Ok(vm::CallOutcome::Return(vm::CallReturn::one(
                vm::Value::Int(1),
            )))
        }
    }
    let mut builder = vm::HostApiBuilder::new();
    for schema in [vm::HostTypeSchema::Int, vm::HostTypeSchema::String] {
        builder.function(vm::HostFunctionSchema::with_return(
            "test::pick",
            vec![vm::HostParamSchema::value("v", schema)],
            vm::HostTypeSchema::Int,
        ));
    }
    let catalog = Arc::new(builder.build().unwrap());
    let options = vm::CompileSourceFileOptions::new().with_host_api_catalog(catalog);
    let fixture = TryModuleFixture::new();
    for (ty, first_payload, second_payload, expected, host_ty) in [
        (
            "int",
            "7",
            "8",
            vec![vm::Value::Int(7), vm::Value::Int(8)],
            vm::HostTypeSchema::Int,
        ),
        (
            "string",
            "\"seven\"",
            "\"eight\"",
            vec![vm::Value::string("seven"), vm::Value::string("eight")],
            vm::HostTypeSchema::String,
        ),
    ] {
        fixture.write(
            "relay.rss",
            &format!(
                r#"
            fn pass(value: {ty}) -> Result<{ty}, string> {{ Result::Ok(value) }}
            pub fn relay() -> Result<int, string> {{
                let first = test::pick(pass({first_payload})?);
                let second = test::pick(pass({second_payload})?);
                Result::Ok(first + second)
            }}
        "#
            ),
        );
        let entry = fixture.write("main.rss", "use self::relay as imported; let pad = [1, 2]; let carried = imported::relay(); carried;");
        let compiled = vm::compile_source_file_with_options(&entry, options.clone())
            .expect("imported overloaded Try admission");
        let imports: Vec<_> = compiled
            .program
            .host_import_schemas()
            .iter()
            .flatten()
            .filter(|s| s.name == "test::pick")
            .collect();
        assert!(!imports.is_empty());
        assert!(
            imports.iter().all(|s| s.params[0].schema == host_ty),
            "{imports:?}"
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut machine = Vm::new(compiled.program.with_local_count(compiled.locals));
        machine.set_fuel(100_000);
        machine.bind_function("test::pick", Box::new(Pick(calls.clone())));
        assert_eq!(
            machine.run().expect("imported selected overload runtime"),
            VmStatus::Halted
        );
        assert_eq!(*calls.lock().unwrap(), expected, "host payload order: {ty}");
        let vm::Value::Map(result) = machine.stack().last().unwrap() else {
            panic!("missing Result")
        };
        assert_eq!(
            result.get(&vm::Value::string("payload")),
            Some(&vm::Value::Int(2))
        );
    }
    fixture.write("relay.rss", "fn pass() -> Result<bool, string> { Result::Ok(true) } pub fn relay() -> Result<int, string> { Result::Ok(test::pick(pass()?)) }");
    let error = vm::compile_source_file_with_options(fixture.0.join("main.rss"), options)
        .err()
        .expect("bool has no overload");
    assert!(error.to_string().contains("pick"), "{error:?}");
}

#[test]
fn fatal_host_operand_vmerror_does_not_become_guest_err() {
    struct Fatal;
    impl vm::HostFunction for Fatal {
        fn call(&mut self, _: &mut Vm, _: &[vm::Value]) -> vm::VmResult<vm::CallOutcome> {
            Err(vm::VmError::HostError("sol-fatal-operand".into()))
        }
    }
    let mut builder = vm::HostApiBuilder::new();
    builder.function(vm::HostFunctionSchema::with_return(
        "test::fatal",
        Vec::new(),
        vm::HostTypeSchema::Result {
            ok: Box::new(vm::HostTypeSchema::Int),
            err: Box::new(vm::HostTypeSchema::String),
        },
    ));
    let compiled = vm::compile_source_with_flavor_and_options(
        r#"
        fn relay() -> Result<int, string> { let n = test::fatal()?; Result::Ok(n) }
        let caught = match relay() { Result::Ok(_) => 1, Result::Err(_) => 2, }; caught;
    "#,
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::new()
            .with_host_api_catalog(std::sync::Arc::new(builder.build().unwrap())),
    )
    .expect("fatal operand source admission");
    let mut machine = Vm::new(compiled.program.with_local_count(compiled.locals));
    machine.set_fuel(100_000);
    machine.bind_function("test::fatal", Box::new(Fatal));
    let error = machine
        .run()
        .expect_err("host VmError must escape the guest Result protocol");
    assert!(
        matches!(error, vm::VmError::HostError(ref s) if s == "sol-fatal-operand"),
        "{error:?}"
    );
    assert!(
        !machine.stack().contains(&vm::Value::Int(2)),
        "guest Err arm must not execute"
    );
}

#[test]
fn fuel_yields_inside_try_operand_then_resumes_to_real_guest_err() {
    let compiled = compile_source(
        r#"
        fn work() -> Result<int, string> {
            let mut i = 0; while i < 100 { i = i + 1; }
            Result::Err("finished")
        }
        fn relay() -> Result<int, string> { let n = work()?; Result::Ok(n) }
        let caught = match relay() { Result::Ok(_) => "wrong", Result::Err(e) => e, }; caught;
    "#,
    )
    .expect("fuel source admission");
    let mut machine = Vm::new_with_jit_config(
        compiled.program.with_local_count(compiled.locals),
        vm::JitConfig {
            enabled: false,
            hot_loop_threshold: 1,
            max_trace_len: 512,
        },
    );
    machine.set_fuel(1);
    assert_eq!(
        machine.run().expect("fuel suspension is not guest Err"),
        VmStatus::Yielded
    );
    assert_eq!(machine.last_yield_reason(), Some(vm::VmYieldReason::Fuel));
    let mut halted = false;
    let mut yielded_inside_operand = false;
    for _ in 0..1024 {
        machine.recharge_fuel(32).unwrap();
        match machine.run().expect("resumed operand") {
            VmStatus::Halted => {
                halted = true;
                break;
            }
            VmStatus::Yielded => {
                assert_eq!(machine.last_yield_reason(), Some(vm::VmYieldReason::Fuel));
                yielded_inside_operand |= machine.call_depth() >= 2;
            }
            status => panic!("unexpected status {status:?}"),
        }
    }
    assert!(halted, "bounded fuel driver must finish");
    assert!(
        yielded_inside_operand,
        "fuel must yield inside work() called by relay's Try operand"
    );
    assert_eq!(machine.stack().last(), Some(&vm::Value::string("finished")));
}

#[test]
fn call_depth_failure_inside_try_operand_is_not_guest_err() {
    let compiled = compile_source(
        r#"
        fn recurse(n: int) -> Result<int, string> { Result::Ok(recurse(n + 1)? + 1) }
        fn relay() -> Result<int, string> { Result::Ok(recurse(0)?) }
        let caught = match relay() { Result::Ok(_) => 1, Result::Err(_) => 2, }; caught;
    "#,
    )
    .expect("call depth source admission");
    let mut machine = Vm::new_with_jit_config(
        compiled.program.with_local_count(compiled.locals),
        vm::JitConfig {
            enabled: false,
            hot_loop_threshold: 1,
            max_trace_len: 512,
        },
    );
    machine.set_fuel(100_000);
    machine.set_max_script_call_depth(4).unwrap();
    let error = machine.run().expect_err("call depth must be fatal");
    assert!(
        matches!(error, vm::VmError::CallStackOverflow { limit: 4 }),
        "{error:?}"
    );
    assert!(!machine.stack().contains(&vm::Value::Int(2)));
}

#[test]
fn host_overload_sees_selected_try_success_type() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static PICK_INT: AtomicUsize = AtomicUsize::new(0);
    static PICK_STRING: AtomicUsize = AtomicUsize::new(0);
    struct Pick;
    impl vm::HostFunction for Pick {
        fn call(&mut self, _vm: &mut vm::Vm, args: &[vm::Value]) -> vm::VmResult<vm::CallOutcome> {
            match &args[0] {
                vm::Value::Int(value) => {
                    PICK_INT.fetch_add(1, Ordering::SeqCst);
                    Ok(vm::CallOutcome::Return(vm::CallReturn::one(
                        vm::Value::Int(value + 1),
                    )))
                }
                vm::Value::String(_) => {
                    PICK_STRING.fetch_add(1, Ordering::SeqCst);
                    Ok(vm::CallOutcome::Return(vm::CallReturn::one(
                        vm::Value::Int(2),
                    )))
                }
                other => panic!("pick saw {other:?}"),
            }
        }
    }
    let mut builder = vm::HostApiBuilder::new();
    builder.function(vm::HostFunctionSchema::with_return(
        "test::pick",
        vec![vm::HostParamSchema::value("value", vm::HostTypeSchema::Int)],
        vm::HostTypeSchema::Int,
    ));
    builder.function(vm::HostFunctionSchema::with_return(
        "test::pick",
        vec![vm::HostParamSchema::value(
            "value",
            vm::HostTypeSchema::String,
        )],
        vm::HostTypeSchema::Int,
    ));
    let catalog = Arc::new(builder.build().expect("pick catalog"));
    let options = vm::CompileSourceFileOptions::new().with_host_api_catalog(Arc::clone(&catalog));
    let int_source = r#"
        fn pass() -> Result<int, string> { Result::Ok(7) }
        fn relay() -> Result<int, string> {
            Result::Ok(test::pick(pass()?))
        }
        let value = match relay() {
            Result::Ok(v) => v,
            Result::Err(_) => 0,
        };
        value;
    "#;
    PICK_INT.store(0, Ordering::SeqCst);
    let compiled = vm::compile_source_with_flavor_and_options(
        int_source,
        vm::SourceFlavor::RustScript,
        options.clone(),
    )
    .unwrap_or_else(|error| panic!("int overload through ? should resolve, got {error:?}"));
    let selected = compiled
        .program
        .host_import_schemas()
        .iter()
        .flatten()
        .find(|schema| schema.name == "test::pick")
        .expect("selected pick import");
    assert!(
        matches!(
            selected.params.first().map(|param| &param.schema),
            Some(vm::HostTypeSchema::Int)
        ),
        "int success type must select the int overload, got {selected:?}"
    );
    let mut machine = vm::Vm::new(compiled.program.with_local_count(compiled.locals));
    machine.set_fuel(100_000);
    machine.bind_function("test::pick", Box::new(Pick));
    assert_eq!(
        machine.run().expect("int pick should run"),
        vm::VmStatus::Halted
    );
    assert_eq!(machine.stack().last().cloned(), Some(vm::Value::Int(8)));
    assert_eq!(PICK_INT.load(Ordering::SeqCst), 1);
    assert_eq!(PICK_STRING.load(Ordering::SeqCst), 0);

    let mismatch = r#"
        fn pass() -> Result<bool, string> { Result::Ok(true) }
        fn relay() -> Result<int, string> {
            Result::Ok(test::pick(pass()?))
        }
        relay();
    "#;
    let rejected = match vm::compile_source_with_flavor_and_options(
        mismatch,
        vm::SourceFlavor::RustScript,
        options,
    ) {
        Err(error) => error,
        Ok(_) => panic!("bool is not a pick overload"),
    };
    let text = rejected.to_string();
    assert!(
        text.contains("pick") || text.contains("overload") || text.contains("argument"),
        "{rejected:?}"
    );
}
