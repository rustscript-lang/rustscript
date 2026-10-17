#![cfg(feature = "runtime")]

use vm::{Vm, VmStatus, compile_source};

fn eval(source: &str) -> vm::Value {
    let compiled = compile_source(source).expect("Result source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_fuel(100_000);
    assert_eq!(
        vm.run().expect("Result program should run"),
        VmStatus::Halted
    );
    vm.stack()
        .last()
        .cloned()
        .expect("Result program should leave a value")
}

#[test]
fn ok_and_err_match_return_distinct_payloads() {
    let source = r#"
        let a: Result<int, string> = Result::Ok(7);
        let b: Result<int, string> = Result::Err("bad");
        let ok_flag = a.is_ok();
        let err_flag = b.is_err();
        let value = match a {
            Result::Ok(v) => v + 1,
            Result::Err(e) => -1,
        };
        let ignored = match b {
            Result::Ok(v) => v,
            Result::Err(e) => -1,
        };
        let result = if ok_flag && err_flag => { value } else => { ignored };
        result;
    "#;
    assert_eq!(eval(source), vm::Value::Int(8));
}

fn reject(source: &str) -> String {
    match compile_source(source) {
        Ok(_) => panic!("Result source should be rejected: {source}"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn same_payload_keeps_ok_and_err_distinct() {
    let source = r#"
        let ok_zero: Result<int, int> = Result::Ok(0);
        let err_zero: Result<int, int> = Result::Err(0);
        let value = match ok_zero {
            Result::Ok(v) => v + 3,
            Result::Err(e) => e - 3,
        };
        let other = match err_zero {
            Result::Ok(v) => v + 3,
            Result::Err(e) => e - 3,
        };
        value - other;
    "#;
    assert_eq!(eval(source), vm::Value::Int(6));
}

#[test]
fn false_and_null_payloads_follow_the_active_branch() {
    let source = r#"
        let ok_false: Result<bool, int> = Result::Ok(false);
        let err_false: Result<int, bool> = Result::Err(false);
        let ok_null: Result<int?, string> = Result::Ok(null);
        let flag = match ok_false {
            Result::Ok(v) => v,
            Result::Err(e) => true,
        };
        let err_flag = match err_false {
            Result::Ok(v) => true,
            Result::Err(e) => e,
        };
        let missing = match ok_null {
            Result::Ok(v) => v,
            Result::Err(e) => 9,
        };
        let err_null: Result<string, int?> = Result::Err(null);
        let result = if flag || err_flag => { -1 } else => { missing };
        let err_observed = match err_null {
            Result::Ok(v) => 2,
            Result::Err(e) => e,
        };
        let observed = if result == null => { err_observed } else => { result };
        observed;
    "#;
    assert_eq!(eval(source), vm::Value::Null);
}

#[test]
fn nested_array_and_named_payloads_match_the_active_branch() {
    let source = r#"
        struct Point { x: int }
        let nested: Result<Result<int, string>, bool> = Result::Ok(Result::Ok(4));
        let numbers: Result<array<int>, string> = Result::Ok([1, 2]);
        let point: Result<Point, string> = Result::Ok({ x: 5 });
        let inner = match nested {
            Result::Ok(v) => match v {
                Result::Ok(n) => n,
                Result::Err(e) => -1,
            },
            Result::Err(e) => -2,
        };
        let sum = match numbers {
            Result::Ok(v) => v[0] + v[1],
            Result::Err(e) => -1,
        };
        let x = match point {
            Result::Ok(v) => v.x,
            Result::Err(e) => -1,
        };
        inner + sum + x;
    "#;
    assert_eq!(eval(source), vm::Value::Int(12));
}

#[test]
fn result_struct_declaration_and_nonexhaustive_match_are_rejected() {
    let reserved = reject("struct Result { value: int }");
    assert!(
        reserved.contains("built-in generic type"),
        "reserved struct error was {reserved}"
    );
    let missing = reject(
        "let value: Result<int, string> = Result::Ok(1); match value { Result::Ok(v) => v };",
    );
    assert!(
        missing.contains("missing Result::Err"),
        "missing arm error was {missing}"
    );
    let duplicate = reject(
        "let value: Result<int, string> = Result::Ok(1); match value { Result::Ok(v) => v, Result::Ok(v) => v, Result::Err(e) => -1 };",
    );
    assert!(
        duplicate.contains("duplicate Result::Ok"),
        "duplicate arm error was {duplicate}"
    );
    let wildcard = reject(
        "let value: Result<int, string> = Result::Ok(1); match value { Result::Ok(v) => v, Result::Err(e) => -1, _ => 0 };",
    );
    assert!(
        wildcard.contains("wildcard"),
        "wildcard-after-both-arms error was {wildcard}"
    );
    let question = reject("fn fail() -> Result<int, string> { Result::Err(\"x\")? }");
    assert!(
        !question.is_empty(),
        "question-mark must stay outside R1, got {question}"
    );
    compile_source("fn ok_only() -> Result<int, string> { Result::Ok(1) }")
        .expect("Result function without ? should compile");
}

#[test]
fn generic_instantiations_keep_distinct_payloads() {
    let source = r#"
        fn wrap<T>(value: T) -> Result<T, int> {
            Result::Ok(value)
        }
        let text = wrap::<string>("hi");
        let number = wrap::<int>(9);
        let text_ok = match text {
            Result::Ok(v) => v == "hi",
            Result::Err(e) => false,
        };
        let number_value = match number {
            Result::Ok(v) => v,
            Result::Err(e) => -1,
        };
        let result = if text_ok => { number_value } else => { -1 };
        result;
    "#;
    assert_eq!(eval(source), vm::Value::Int(9));
}

#[test]
fn callable_payload_is_invoked_from_the_selected_arm() {
    let source = r#"
        fn add1(value: int) -> int { value + 1 }
        let boxed: Result<fn(int) -> int, string> = Result::Ok(add1);
        let applied = match boxed {
            Result::Ok(f) => f(6),
            Result::Err(e) => -1,
        };
        applied;
    "#;
    assert_eq!(eval(source), vm::Value::Int(7));
}

#[test]
fn result_constructor_requires_result_context_and_matching_payload() {
    let missing = reject("Result::Ok(1);");
    assert!(
        missing.contains("inactive branch"),
        "missing context error was {missing}"
    );
    let wrong_type = reject("let value: int = Result::Ok(1);");
    assert!(
        wrong_type.contains("is not a value of type"),
        "wrong context error was {wrong_type}"
    );
    let wrong_payload = reject("let value: Result<int, string> = Result::Ok(\"no\");");
    assert!(
        wrong_payload.contains("schema") || wrong_payload.contains("assigned"),
        "wrong payload error was {wrong_payload}"
    );
    compile_source("fn typed() -> Result<int, string> { Result::Ok(1) }")
        .expect("return context should supply the inactive branch");
}

#[test]
fn match_evaluates_scrutinee_once_and_only_the_selected_arm() {
    let source = r#"
        fn make(n: int) -> Result<int, int> {
            Result::Ok(n + 1)
        }
        let mut calls = [0];
        let value = match make(if true => { calls[0] = calls[0].copy() + 1; calls[0].copy() } else => { 0 }) {
            Result::Ok(v) => if true => { calls[0] = calls[0].copy() + 10; v } else => { 0 },
            Result::Err(e) => if true => { calls[0] = calls[0].copy() + 100; 0 } else => { 0 },
        };
        value * 1000 + calls[0].copy();
    "#;
    assert_eq!(eval(source), vm::Value::Int(2011));
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_ssa_match_returns_runtime_ok_and_err_payloads() {
    fn run_pick(flag: i64, ok_value: i64, err_value: i64) -> (vm::Value, u64) {
        let source = r#"
            pub fn pick(flag: int, ok_value: int, err_value: int) -> int {
                let value: Result<int, int> = if flag == 1 => {
                    Result::Ok(ok_value)
                } else => {
                    Result::Err(err_value)
                };
                match value {
                    Result::Ok(v) => v + 1,
                    Result::Err(e) => 0 - e,
                }
            }
            let callable = pick;
            callable(41, 42, 43);
        "#;
        let compiled = compile_source(source).expect("Result pick should compile");
        let mut program = compiled.program.with_local_count(compiled.locals);
        for constant in &mut program.constants {
            if let vm::Value::Int(value) = constant {
                *constant = match *value {
                    41 => vm::Value::Int(flag),
                    42 => vm::Value::Int(ok_value),
                    43 => vm::Value::Int(err_value),
                    other => vm::Value::Int(other),
                };
            }
        }
        let mut machine = vm::Vm::new_with_jit_config(
            program,
            vm::JitConfig {
                enabled: false,
                hot_loop_threshold: 1,
                max_trace_len: 512,
            },
        );
        machine.set_fuel(100_000);
        machine
            .compile_aot()
            .expect("Result match should compile to AOT");
        let dump = machine.dump_aot_info();
        assert!(dump.contains("lowering=ssa"), "{dump}");
        assert!(!dump.contains("interpreter-boundary"), "{dump}");
        assert_eq!(
            machine.run().expect("AOT Result match should run"),
            VmStatus::Halted
        );
        let count = machine.aot_exec_count();
        assert!(count > 0, "AOT execution count was {count}; dump {dump}");
        (
            machine
                .stack()
                .last()
                .cloned()
                .expect("AOT Result match should return a payload"),
            count,
        )
    }

    let (ok_payload, ok_count) = run_pick(1, 7, 9);
    assert_eq!(ok_payload, vm::Value::Int(8));
    assert!(ok_count > 0);
    let (err_payload, err_count) = run_pick(0, 7, 9);
    assert_eq!(err_payload, vm::Value::Int(-9));
    assert!(err_count > 0);

    let source = r#"
        pub fn pick(flag: int) -> bool {
            let value: Result<bool, int> = if flag == 1 => { Result::Ok(false) } else => { Result::Err(1) };
            match value {
                Result::Ok(v) => v,
                Result::Err(e) => true,
            }
        }
        let callable = pick;
        callable(41);
    "#;
    let compiled = compile_source(source).expect("bool Result should compile");
    let mut program = compiled.program.with_local_count(compiled.locals);
    for constant in &mut program.constants {
        if let vm::Value::Int(41) = constant {
            *constant = vm::Value::Int(1);
        }
    }
    let mut machine = vm::Vm::new_with_jit_config(
        program,
        vm::JitConfig {
            enabled: false,
            hot_loop_threshold: 1,
            max_trace_len: 512,
        },
    );
    machine.set_fuel(100_000);
    machine
        .compile_aot()
        .expect("bool Result should compile to AOT");
    assert_eq!(
        machine.run().expect("bool Result should run"),
        VmStatus::Halted
    );
    assert!(machine.aot_exec_count() > 0);
    assert_eq!(
        machine.stack().last().cloned(),
        Some(vm::Value::Bool(false))
    );
}

#[test]
fn direct_call_receiver_queries_both_outcomes_once() {
    let source = r#"
        fn choose(flag: bool) -> Result<int, string> {
            if flag => { Result::Ok(1) } else => { Result::Err("e") }
        }
        let mut calls = [0];
        let ok_is_ok = choose(true).is_ok();
        let ok_is_err = choose(true).is_err();
        let err_is_ok = choose(false).is_ok();
        let err_is_err = choose(false).is_err();
        let once = choose(if true => { calls[0] = calls[0].copy() + 1; true } else => { false }).is_ok();
        let local: Result<int, string> = Result::Ok(2);
        let local_ok = local.is_ok();
        let result = if ok_is_ok && err_is_err && once && local_ok => {
            if ok_is_err || err_is_ok => { -1 } else => { calls[0].copy() }
        } else => { -2 };
        result;
    "#;
    assert_eq!(eval(source), vm::Value::Int(1));
    let non_result = reject("fn plain(n: int) -> int { n } plain(1).is_ok();");
    assert!(
        non_result.contains("Result"),
        "non-Result query error was {non_result}"
    );
}

#[test]
fn nested_result_array_constructors_keep_inactive_branches() {
    let source = r#"
        let rows: Result<array<Result<int, string>>, string> = Result::Ok([Result::Ok(1), Result::Err("e")]);
        let outer: Result<array<Result<int, string>>, string> = Result::Err("outer");
        let deep: Result<array<Result<Result<int, bool>, string>>, string> = Result::Ok([Result::Ok(Result::Err(false))]);
        let nullable: Result<array<Result<int?, string>>, string> = Result::Ok([Result::Ok(null)]);
        let first = match rows {
            Result::Ok(items) => match items[0] {
                Result::Ok(n) => n,
                Result::Err(e) => -1,
            },
            Result::Err(e) => -1,
        };
        let second = match rows {
            Result::Ok(items) => match items[1] {
                Result::Ok(n) => n,
                Result::Err(e) => 2,
            },
            Result::Err(e) => -1,
        };
        let outer_flag = match outer {
            Result::Ok(items) => -1,
            Result::Err(e) => 4,
        };
        let nested_flag = match deep {
            Result::Ok(items) => match items[0] {
                Result::Ok(inner) => match inner {
                    Result::Ok(n) => n,
                    Result::Err(flag) => if flag => { -1 } else => { 8 },
                },
                Result::Err(e) => -1,
            },
            Result::Err(e) => -1,
        };
        let null_flag = match nullable {
            Result::Ok(items) => match items[0] {
                Result::Ok(n) => if n == null => { 16 } else => { -1 },
                Result::Err(e) => -1,
            },
            Result::Err(e) => -1,
        };
        first + second + outer_flag + nested_flag + null_flag;
    "#;
    assert_eq!(eval(source), vm::Value::Int(31));
    let wrong = reject(
        "let value: Result<array<Result<int, string>>, string> = Result::Ok([Result::Ok(\"no\")]);",
    );
    assert!(
        (wrong.contains("schema") || wrong.contains("assigned") || wrong.contains("string"))
            && !wrong.contains("inactive branch"),
        "wrong nested payload error was {wrong}"
    );
}

#[test]
fn named_and_generic_result_fields_use_declaration_context() {
    let source = r#"
        struct Box { value: Result<int, string> }
        struct Holder<T> { item: Result<T, string> }
        let boxed: Result<Box, string> = Result::Ok({ value: Result::Ok(5) });
        let err_boxed: Result<Box, string> = Result::Ok({ value: Result::Err("e") });
        let held: Result<Holder<int>, string> = Result::Ok({ item: Result::Err("e") });
        let ok_value = match boxed {
            Result::Ok(v) => match v.value {
                Result::Ok(n) => n,
                Result::Err(e) => -1,
            },
            Result::Err(e) => -1,
        };
        let err_flag = match err_boxed {
            Result::Ok(v) => match v.value {
                Result::Ok(n) => n,
                Result::Err(e) => 3,
            },
            Result::Err(e) => -1,
        };
        let generic_flag = match held {
            Result::Ok(v) => match v.item {
                Result::Ok(n) => n,
                Result::Err(e) => 7,
            },
            Result::Err(e) => -1,
        };
        ok_value + err_flag + generic_flag;
    "#;
    assert_eq!(eval(source), vm::Value::Int(15));
    let wrong = reject(
        "struct Box { value: Result<int, string> } let value: Result<Box, string> = Result::Ok({ value: Result::Ok(\"no\") });",
    );
    assert!(
        (wrong.contains("schema") || wrong.contains("assigned") || wrong.contains("string"))
            && !wrong.contains("inactive branch"),
        "invalid field payload error was {wrong}"
    );
    let missing = reject(
        "struct Box { value: Result<int, string> } let value: Result<Box, string> = Result::Ok({});",
    );
    assert!(
        missing.contains("field") && !missing.contains("inactive branch"),
        "missing field error was {missing}"
    );
}

#[test]
fn typed_closure_result_return_uses_callable_schema() {
    let source = r#"
        let empty: fn() -> Result<int, string> = || Result::Ok(1);
        let choose: fn(bool) -> Result<int, string> = |flag| if flag => { Result::Ok(8) } else => { Result::Err("e") };
        let empty_value = match empty() {
            Result::Ok(v) => v,
            Result::Err(e) => -1,
        };
        let ok_value = match choose(true) {
            Result::Ok(v) => v,
            Result::Err(e) => -1,
        };
        let err_flag = match choose(false) {
            Result::Ok(v) => v,
            Result::Err(e) => 2,
        };
        empty_value + ok_value + err_flag;
    "#;
    assert_eq!(eval(source), vm::Value::Int(11));
}

fn eval_file(path: &std::path::Path) -> vm::Value {
    let compiled = vm::compile_source_file(path).expect("module Result source should compile");
    let mut machine = vm::Vm::new(compiled.program.with_local_count(compiled.locals));
    machine.set_fuel(100_000);
    assert_eq!(
        machine.run().expect("module Result program should run"),
        VmStatus::Halted
    );
    machine
        .stack()
        .last()
        .cloned()
        .expect("module Result program should leave a value")
}

#[test]
fn imported_module_formal_supplies_result_constructor_context() {
    let root = std::env::temp_dir().join(format!(
        "r1_address01_module_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("module root");
    let module_path = root.join("module.rss");
    let main_path = root.join("main.rss");
    std::fs::write(
        &module_path,
        r#"
        pub fn take(value: Result<int, string>) -> int {
            let observed = match value {
                Result::Ok(v) => v + 1,
                Result::Err(e) => -1,
            };
            observed
        }
        "#,
    )
    .expect("module source");
    std::fs::write(
        &main_path,
        r#"
        use module::{take};
        let ok_value = take(Result::Ok(6));
        let err_value = take(Result::Err("e"));
        ok_value + err_value;
        "#,
    )
    .expect("main source");
    let observed = eval_file(&main_path);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(observed, vm::Value::Int(6));
    let wrong_root = std::env::temp_dir().join(format!(
        "r1_address01_module_wrong_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&wrong_root).expect("wrong module root");
    std::fs::write(
        wrong_root.join("module.rss"),
        r#"
        pub fn take(value: Result<int, string>) -> int {
            let observed = match value {
                Result::Ok(v) => v,
                Result::Err(e) => -1,
            };
            observed
        }
        "#,
    )
    .expect("wrong module source");
    std::fs::write(
        wrong_root.join("main.rss"),
        "use module::{take};\ntake(Result::Ok(\"no\"));\n",
    )
    .expect("wrong main source");
    let wrong = match vm::compile_source_file(wrong_root.join("main.rss")) {
        Ok(_) => {
            let _ = std::fs::remove_dir_all(&wrong_root);
            panic!("wrong module payload should be rejected");
        }
        Err(error) => error.to_string(),
    };
    let _ = std::fs::remove_dir_all(&wrong_root);
    assert!(
        (wrong.contains("schema") || wrong.contains("assigned") || wrong.contains("string"))
            && !wrong.contains("inactive branch"),
        "wrong module payload error was {wrong}"
    );
}

#[test]
fn contextual_captured_result_returns_captured_ok() {
    let value = eval(
        r#"
        let n: int = 7;
        let f: fn() -> Result<int, string> = || Result::Ok(n);
        match f() { Result::Ok(v) => v, Result::Err(e) => -1 };
        "#,
    );
    assert_eq!(value, vm::Value::Int(7));
}

#[test]
fn contextual_captured_result_selects_err_payload() {
    let value = eval(
        r#"
        let message: string = "bad";
        let f: fn() -> Result<int, string> = || Result::Err(message);
        match f() { Result::Ok(v) => v, Result::Err(e) => -1 };
        "#,
    );
    assert_eq!(value, vm::Value::Int(-1));
}

#[test]
fn contextual_captured_result_captured_callable() {
    let value = eval(
        r#"
        let add1: fn(int) -> int = |value| value + 1;
        let f: fn() -> Result<fn(int) -> int, string> = || Result::Ok(add1);
        let picked = match f() {
            Result::Ok(g) => g(6),
            Result::Err(e) => -1,
        };
        picked;
        "#,
    );
    assert_eq!(value, vm::Value::Int(7));
}

#[test]
fn contextual_captured_result_rejects_wrong_payload() {
    let wrong = match compile_source(
        r#"
        let message: string = "no";
        let f: fn() -> Result<int, string> = || Result::Ok(message);
        "#,
    ) {
        Ok(_) => panic!("wrong captured payload must be rejected"),
        Err(error) => error.to_string(),
    };
    assert!(
        (wrong.contains("schema") || wrong.contains("assigned") || wrong.contains("string"))
            && !wrong.contains("inactive branch"),
        "wrong captured payload error was {wrong}"
    );
}

#[test]
fn contextual_captured_result_rejects_moved_resource() {
    let key = vm::ResourceTypeKey::new("capture.resource").expect("resource key");
    let mut builder = vm::HostApiBuilder::new();
    builder.resource(vm::ResourceTypeSchema::new(key.clone(), "capture resource"));
    builder.function(vm::HostFunctionSchema::with_return(
        "capture::open",
        Vec::new(),
        vm::HostTypeSchema::Resource(key),
    ));
    let catalog = std::sync::Arc::new(builder.build().expect("catalog"));
    let wrong = match vm::compile_source_with_flavor_and_options(
        "let owned = capture::open();\nlet f = || owned.copy();\n",
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    ) {
        Ok(_) => panic!("moved/copied resource capture must be rejected"),
        Err(error) => format!("{error:?} {error}"),
    };
    assert!(
        wrong.contains("E_OWNERSHIP_COPY_RESOURCE") || wrong.contains("moved"),
        "moved resource diagnostic was {wrong}"
    );
}

fn observe_nested_closure(source: &str) -> (String, Result<vm::Value, String>) {
    let compiled = compile_source(source).expect("nested closure source should compile");
    let prototypes = format!("{:?}", compiled.program.callable_prototypes);
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_fuel(100_000);
    let runtime = match vm.run() {
        Ok(status) => {
            if status != VmStatus::Halted {
                Err(format!("status {status:?}"))
            } else {
                vm.stack()
                    .last()
                    .cloned()
                    .ok_or_else(|| "missing stack value".to_string())
            }
        }
        Err(error) => Err(format!("{error:?}")),
    };
    (prototypes, runtime)
}

fn assert_nonrecursive_nested_returns_three(source: &str, label: &str) {
    let (prototypes, runtime) = observe_nested_closure(source);
    let self_claimed = prototypes.contains("self_slot: Some");
    assert!(
        !self_claimed && matches!(runtime, Ok(vm::Value::Int(3))),
        "{label} nonrecursive prototypes must keep self_slot None and return 3; prototypes={prototypes} runtime={runtime:?}"
    );
    assert!(
        prototypes.contains("capture_slots: [") && prototypes.contains("Callable"),
        "{label} must keep an ordinary captured callable schema; prototypes={prototypes}"
    );
}

#[test]
fn contextual_captured_result_nested_call_returns_three() {
    assert_nonrecursive_nested_returns_three(
        r#"
        let n: int = 3;
        let f: fn() -> Result<int, string> = || Result::Ok(n);
        let g: fn() -> Result<int, string> = || f();
        match g() { Result::Ok(v) => v, Result::Err(e) => -1 };
        "#,
        "green-f1 nested call",
    );
}

#[test]
fn contextual_captured_result_nested_inner_call_returns_three() {
    assert_nonrecursive_nested_returns_three(
        r#"
        let n: int = 3;
        let inner: fn() -> int = || n;
        let outer: fn() -> Result<int, string> = || Result::Ok(inner());
        match outer() { Result::Ok(v) => v, Result::Err(e) => -1 };
        "#,
        "green-f1b nested inner call",
    );
}

#[test]
fn contextual_captured_result_nested_returned_callable_returns_three() {
    assert_nonrecursive_nested_returns_three(
        r#"
        let n: int = 3;
        let inner: fn() -> int = || n;
        let outer: fn() -> Result<fn() -> int, string> = || Result::Ok(inner);
        let picked = match outer() {
            Result::Ok(f) => f(),
            Result::Err(e) => -1,
        };
        picked;
        "#,
        "green-f1c returned callable",
    );
}

#[test]
fn no_result_nested_inner_outer_returns_three() {
    assert_nonrecursive_nested_returns_three(
        r#"
        let n: int = 3;
        let inner: fn() -> int = || n;
        let outer = || inner;
        let picked = outer();
        picked();
        "#,
        "no-Result nested inner/outer",
    );
}
