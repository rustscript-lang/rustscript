#[path = "../common/mod.rs"]
mod common;
use common::*;

struct BoundRuntimeCase<'a> {
    case: RuntimeCase<'a>,
    bindings: Vec<HostBindingCase<'a>>,
}

fn run_bound_runtime_cases(cases: Vec<BoundRuntimeCase<'_>>) {
    for case in &cases {
        run_runtime_case_with_bindings(&case.case, &case.bindings);
    }
}

fn local_visible_at_current_line(vm: &Vm, name: &str) -> bool {
    let info = vm
        .debug_info()
        .expect("compiled program should include debug info");
    let local = info
        .locals
        .iter()
        .find(|local| local.name == name)
        .unwrap_or_else(|| panic!("expected local '{name}' in debug info"));
    let Some(line) = info.line_for_offset(vm.ip()) else {
        return true;
    };
    if let Some(declared_line) = local.declared_line
        && line < declared_line
    {
        return false;
    }
    if let Some(last_line) = local.last_line
        && line > last_line
    {
        return false;
    }
    true
}

fn assert_builtin_namespace_stays_builtin(
    temp_name: &str,
    source: &str,
    import_prefix: &str,
    expected_stack: &[Value],
) {
    let unique = format!(
        "{temp_name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp module root should be created");

    let main_path = root.join("main.rss");
    std::fs::write(&main_path, source).expect("main source should write");

    let compiled = compile_source_file(main_path.as_path()).expect("compile should succeed");
    assert!(
        compiled
            .program
            .imports
            .iter()
            .all(|import| !import.name.starts_with(import_prefix)),
        "{import_prefix} namespace calls should lower as builtins, not host imports"
    );

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), expected_stack);

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn rustscript_host_import_runtime_cases_work() {
    let cases = vec![
        BoundRuntimeCase {
            case: rustscript_runtime_case(
                "runtime namespace host calls are supported",
                r#"
                    use runtime;
                    runtime::sleep(41);
                "#,
                vec![Value::Int(42)],
            ),
            bindings: vec![HostBindingCase {
                name: "runtime::sleep",
                factory: make_add_one,
            }],
        },
        BoundRuntimeCase {
            case: rustscript_runtime_case(
                "http subnamespace host calls are supported",
                r#"
                    use http;
                    http::request::get_header("x-client-id");
                "#,
                vec![Value::string("x-client-id")],
            ),
            bindings: vec![HostBindingCase {
                name: "http::request::get_header",
                factory: make_echo_string,
            }],
        },
        BoundRuntimeCase {
            case: rustscript_runtime_case(
                "host namespace import is supported",
                r#"
                    use runtime;
                    runtime::sleep(41);
                "#,
                vec![Value::Int(42)],
            ),
            bindings: vec![HostBindingCase {
                name: "runtime::sleep",
                factory: make_add_one,
            }],
        },
        BoundRuntimeCase {
            case: rustscript_runtime_case(
                "host namespace alias import is supported",
                r#"
                    use rate_limit as rl;
                    rl::allow("client-1", 3, 60);
                "#,
                vec![Value::Bool(true)],
            ),
            bindings: vec![HostBindingCase {
                name: "rate_limit::allow",
                factory: make_always_allow,
            }],
        },
        BoundRuntimeCase {
            case: rustscript_runtime_case(
                "named host imports are supported",
                r#"
                    use runtime as rt;
                    rt::sleep(41);
                "#,
                vec![Value::Int(42)],
            ),
            bindings: vec![HostBindingCase {
                name: "runtime::sleep",
                factory: make_add_one,
            }],
        },
    ];

    run_bound_runtime_cases(cases);
}

#[test]
fn rustscript_io_namespace_builtin_calls_are_supported() {
    let source = r#"
        use io;
        io::exists(".");
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let mut registry = HostFunctionRegistry::empty();
    vm::register_io_builtin_module(&mut registry).expect("standard IO registration should succeed");
    registry
        .bind_vm_cached(&mut vm)
        .expect("standard exact host imports should bind");
    #[cfg(feature = "async")]
    super::async_test_bridge::install(&mut vm);

    loop {
        let status = vm.run().expect("vm should run");
        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => continue,
            VmStatus::Waiting(_op_id) => vm
                .wait_for_host_op_blocking()
                .expect("vm should complete builtin async op"),
        }
    }
    assert_eq!(vm.stack(), &[Value::Bool(true)]);
}

#[test]
fn rustscript_builtin_and_namespace_runtime_cases_work() {
    let cases = vec![
        RuntimeCase {
            name: "re namespace supports inline regex flags across functions",
            source: r#"
                use re;
                let a = re::match("(?i)^foo$", "FoO");
                let b = re::find("(?i)^foo", "FoO bar");
                let c = re::replace("(?i)foo", "FoO bar", "x");
                let d = re::split("(?i)x", "aXb");
                let e = re::captures("(?i)^(foo)-([0-9]+)$", "FoO-42");

                let mut score = 0;
                if a {
                    score = score + 1;
                }
                if b.unwrap_or("") == "FoO" {
                    score = score + 1;
                }
                if c == "x bar" {
                    score = score + 1;
                }
                if d.length == 2 && d[0] == "a" && d[1] == "b" {
                    score = score + 1;
                }
                if e.length == 3 && e[1] == "FoO" && e[2] == "42" {
                    score = score + 1;
                }
                score;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(5)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "literal string split preserves unicode and defines empty delimiter behavior",
            source: r#"
                let csv = string_split_literal("alpha,beta,,gamma", ",");
                let unicode = string_split_literal("甲｜乙｜丙", "｜");
                let unsplit = string_split_literal("value", "");
                if csv.length == 4 && csv[2] == "" && unicode.length == 3 && unicode[1] == "乙" && unsplit.length == 1 && unsplit[0] == "value" {
                    1;
                } else {
                    0;
                }
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(1)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "json encode decode builtins are supported",
            source: r#"
                use json;
                struct Inner { name: string }
                struct Payload {
                    answer: int,
                    ok: bool,
                    arr: [int],
                    inner: Inner,
                }
                let payload = {
                    answer: 42,
                    ok: true,
                    arr: [1, 2],
                    inner: { name: "pd" },
                };
                let text = json::encode(payload);
                let decoded = json::decode::<Payload>(text);
                decoded.answer + decoded.arr[1];
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(44)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "jit namespace builtins can configure and read jit",
            source: r#"
                use jit;
                let _set = jit::set_hot_loop_threshold(3);
                let after = jit::get_hot_loop_threshold();
                let cfg = jit::get_config();
                if after == 3 && cfg.hot_loop_threshold == 3 {
                    1;
                } else {
                    0;
                }
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(1)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "plus equal is supported for numeric locals",
            source: r#"
                let mut total = 1;
                total += 2;
                total += 3;
                total;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(6)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "math namespace builtins provide numeric helpers",
            source: r#"
                use math;
                let root = math::sqrt(81);
                let angle = math::to_radians(180);
                let diff = math::abs(angle - math::pi());
                let growth = math::powi(2, 5);
                if math::is_finite(root) && diff < 0.0000001 && growth == 32.0 {
                    1;
                } else {
                    0;
                }
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(1)],
            expected_locals: None,
        },
    ];
    run_runtime_cases(&cases);
}

#[test]
fn rustscript_plus_equal_rejects_non_numeric_values() {
    let case = SourceErrorCase {
        name: "plus equal rejects strings",
        source: r#"
            let mut value = "a";
            value += "b";
        "#,
        flavor: SourceFlavor::RustScript,
        expected_kind: SourceErrorKind::Compile(CompileErrorKind::BinaryOperandTypeMismatch),
        expected_contains_all: &["'+=' assignment requires a numeric local"],
    };
    expect_source_error_case(&case);
}

#[test]
fn rustscript_builtin_and_namespace_parse_rejection_cases_work() {
    let cases = vec![rustscript_parse_error_case(
        "builtin namespace calls require use import",
        r#"
            json::encode("ok");
        "#,
        &["import builtin namespaces first"],
    )];
    for case in &cases {
        expect_parse_error_case(case);
    }
}

#[test]
fn compile_source_file_preserves_jit_builtin_namespace_use_directive() {
    assert_builtin_namespace_stays_builtin(
        "vm_rustscript_jit_builtin_namespace_test",
        r#"
        use jit;
        let _set = jit::set_hot_loop_threshold(2);
        let out = jit::get_hot_loop_threshold();
        out;
    "#,
        "jit::",
        &[Value::Int(2)],
    );
}

#[test]
fn compile_source_file_preserves_math_builtin_namespace_use_directive() {
    assert_builtin_namespace_stays_builtin(
        "vm_rustscript_math_builtin_namespace_test",
        r#"
        use math;
        let out = math::sqrt(81);
        out;
    "#,
        "math::",
        &[Value::Float(9.0)],
    );
}

#[test]
fn rustscript_literal_and_slice_runtime_cases_work() {
    let cases = vec![
        RuntimeCase {
            name: "float literal binding is supported",
            source: r#"
                let a=1.1;
                a;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Float(1.1)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "char and hex escape literals are supported",
            source: r#"
                let c = '\x41';
                let s = "\x42";
                c;
                s;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::string("A"), Value::string("B")],
            expected_locals: None,
        },
        RuntimeCase {
            name: "array primitives are supported without namespace",
            source: r#"
                let mut values = [];
                values[values.length] = 7;
                values[0] + values.length;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(8)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "bracket slice syntax is supported",
            source: r#"
                let text = "abcdef";
                let end = -2;
                let a = text.copy()[1:4];
                let b = text.copy()[:3];
                let c = text.copy()[2:];
                let d = text.copy()[:-1];
                let e = text.copy()[1:end];

                let arr = [1, 2, 3, 4, 5];
                let f = arr.copy()[1:4];
                let g = arr.copy()[:2];
                let h = arr.copy()[3:];
                let i = arr.copy()[:-2];
                a.length + b.length + c.length + d.length + e.length + f.length + g.length + h.length + i.length;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(28)],
            expected_locals: None,
        },
    ];
    run_runtime_cases(&cases);
}

#[test]
fn rustscript_print_runtime_cases_work() {
    let cases = vec![
        BoundRuntimeCase {
            case: rustscript_runtime_case(
                "print builtin works without decl",
                r#"
                    print(40 + 2);
                "#,
                vec![Value::Int(42)],
            ),
            bindings: vec![HostBindingCase {
                name: "print",
                factory: make_print_builtin,
            }],
        },
        BoundRuntimeCase {
            case: rustscript_runtime_case(
                "println function adds newline",
                r#"
                    println(40 + 2);
                "#,
                vec![Value::string("42\n")],
            ),
            bindings: vec![HostBindingCase {
                name: "print",
                factory: make_print_builtin,
            }],
        },
        BoundRuntimeCase {
            case: rustscript_runtime_case(
                "println function supports basic rust style formatting",
                r#"
                    let foo = "hello";
                    let bar = 42;
                    println("{} {}!", foo, bar);
                "#,
                vec![Value::string("hello 42!\n")],
            ),
            bindings: vec![HostBindingCase {
                name: "print",
                factory: make_print_builtin,
            }],
        },
        BoundRuntimeCase {
            case: rustscript_runtime_case(
                "print function supports rust style formatting",
                r#"
                    print("hex={:#x} bin={:08b} sci={:.1e}", 42, 5, 1234.0);
                "#,
                vec![Value::string("hex=0x2a bin=00000101 sci=1.2e3")],
            ),
            bindings: vec![HostBindingCase {
                name: "print",
                factory: make_print_builtin,
            }],
        },
        BoundRuntimeCase {
            case: rustscript_runtime_case(
                "println function supports rust style formatting",
                r#"
                    println("{1} {0}", "left", "right");
                "#,
                vec![Value::string("right left\n")],
            ),
            bindings: vec![HostBindingCase {
                name: "print",
                factory: make_print_builtin,
            }],
        },
        BoundRuntimeCase {
            case: rustscript_runtime_case(
                "print alias handles mixed call arities",
                r#"
                    print(1);
                    print("{}", 2);
                "#,
                vec![Value::Int(1), Value::string("2")],
            ),
            bindings: vec![HostBindingCase {
                name: "print",
                factory: make_print_builtin,
            }],
        },
    ];

    run_bound_runtime_cases(cases);
}

#[test]
fn rustscript_print_rejects_non_literal_format_string() {
    let case = rustscript_parse_error_case(
        "print rejects non literal format string",
        r#"
            let fmt = "{}";
            print(fmt, 1);
        "#,
        &["print formatting requires a string literal as the first argument"],
    );
    expect_parse_error_case(&case);
}

#[test]
fn compile_source_file_with_rustscript_complex_fixture() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/example_complex.rss");
    let compiled = compile_source_file(path.as_path()).expect("compile should succeed");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    #[cfg(feature = "async")]
    super::async_test_bridge::install(&mut vm);

    let mut registry = HostFunctionRegistry::empty();
    vm::register_io_builtin_module(&mut registry).expect("standard IO registration should succeed");
    for func in &compiled.functions {
        match func.name.as_str() {
            "print" => registry.register("print", func.arity, || Box::new(PrintBuiltin)),
            "add_one" => registry.register("add_one", func.arity, || Box::new(AddOne)),
            "runtime::sleep" => {
                registry.register("runtime::sleep", func.arity, || Box::new(RuntimeSleep));
            }
            "io::exists" => {}
            _ => panic!("unexpected function {}", func.name),
        };
    }
    registry
        .bind_vm_cached(&mut vm)
        .expect("standard exact host imports should bind");

    loop {
        match vm.run().expect("vm should run") {
            VmStatus::Halted => break,
            VmStatus::Yielded => continue,
            VmStatus::Waiting(_op_id) => vm
                .wait_for_host_op_blocking()
                .expect("vm should complete host operation"),
        }
    }
    assert_eq!(vm.stack(), &[Value::Int(12)]);
}

#[test]
fn rustscript_named_function_capture_runtime_cases_work() {
    let cases = [
        RuntimeCase {
            name: "named functions can compose other named functions",
            source: r#"
                fn inc(x) { x + 1 }
                fn twice(x) { x * 2 }
                fn combine(a, b) {
                    let left = inc(a);
                    let right = twice(b);
                    left + right;
                }
                combine(3, 4);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(12)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "named function calls can nest without recursion",
            source: r#"
                fn inc(x) { x + 1 }
                fn double_inc(x) { inc(inc(x)) }
                fn score(x) {
                    let a = double_inc(x);
                    let b = inc(x);
                    a + b;
                }
                score(5);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(13)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "nested named function implicitly captures outer local",
            source: r#"
                fn outer(base) {
                    fn add(v) { v + base }
                    add(2);
                }
                outer(5);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(7)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "top-level named function capture snapshots at declaration time",
            source: r#"
                let mut base = 5;
                fn add(v) { v + base }
                base = 100;
                add(1);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(6)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "top-level named function keeps transitive captures alive across inline calls",
            source: r#"
                let lut = {"a": 1};
                fn parse_key(text) { lut[text[0:1]] }
                fn wrapper(text) { parse_key(text) }
                wrapper("ab");
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(1)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "top-level named function capture survives repeated calls",
            source: r#"
                let lut = {"a": 1};
                fn parse_key(text) { lut[text[0:1]] }
                parse_key("ab");
                parse_key("ab");
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(1), Value::Int(1)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "named function capture via copy keeps source reusable",
            source: r#"
                let a = "x";
                fn add(v) { v + a.copy() }
                let d = a;
                add(d);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::string("xx")],
            expected_locals: None,
        },
        RuntimeCase {
            name: "nested named functions can capture outer locals and call siblings",
            source: r#"
                fn outer(base) {
                    fn inc(v) { v + 1 }
                    fn add_base(v) { inc(v) + base }
                    add_base(2);
                }
                outer(3);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(6)],
            expected_locals: None,
        },
    ];
    run_runtime_cases(&cases);
}

#[test]
fn rustscript_named_function_block_body_allows_trailing_expression_without_semicolon() {
    let case = RuntimeCase {
        name: "named function block body allows trailing expression without semicolon",
        source: r#"
            fn addme(x) {
                let doubled = x + x;
                doubled + 1
            }

            addme(20);
        "#,
        flavor: SourceFlavor::RustScript,
        expected_stack: vec![Value::Int(41)],
        expected_locals: None,
    };
    run_runtime_case(&case);
}

#[test]
fn rustscript_named_function_capture_error_cases_work() {
    let cases = [
        SourceErrorCase {
            name: "named function default capture moves movable local",
            source: r#"
                let a = "";
                fn add(v: string) -> string { v + a }
                let d = a;
                d;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Parse,
            expected_contains_all: &[],
        },
        SourceErrorCase {
            name: "named function repeated default move capture is rejected",
            source: r#"
                let lut = {"a": 1};
                fn parse_a(text: string) -> int { lut[text[0:1]] }
                fn parse_b(text: string) -> int { lut[text[0:1]] }
                parse_a("ab");
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Parse,
            expected_contains_all: &["lut", "moved"],
        },
    ];

    for case in &cases {
        expect_source_error_case(case);
    }
}

#[test]
fn closure_captures_outer_value_at_definition_time() {
    let case = RuntimeCase {
        name: "closure captures outer value at definition time",
        source: r#"
            let mut base = 7;
            let add = |value| value + base;
            base = 8;
            print(add(5));
        "#,
        flavor: SourceFlavor::RustScript,
        expected_stack: vec![Value::Int(12)],
        expected_locals: None,
    };
    let bindings = [HostBindingCase {
        name: "print",
        factory: make_print_builtin,
    }];
    run_runtime_case_with_bindings(&case, &bindings);
}

#[test]
fn named_function_recursion_uses_runtime_frames_and_hits_depth_limit() {
    let compiled = vm::compile_source_for_repl(
        r#"
            fn recurse(x: int) -> int { recurse(x) }
            recurse(1);
        "#,
    )
    .expect("recursive function should compile");
    assert!(
        compiled
            .program
            .code
            .contains(&(vm::OpCode::CallScript as u8)),
        "non-capturing direct recursion lowers through CallScript"
    );
    assert_eq!(compiled.program.script_functions.len(), 1);

    let mut runtime = vm::Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    assert!(matches!(
        runtime.run(),
        Err(vm::VmError::CallStackOverflow { limit: 1024 })
    ));
}

#[test]
fn repeated_named_calls_share_one_emitted_body() {
    let compiled = vm::compile_source_for_repl(
        r#"
            fn add_one(value: int) -> int { value + 1 }
            add_one(1);
            add_one(2);
            add_one(3);
        "#,
    )
    .expect("repeated calls should compile");

    assert_eq!(compiled.program.script_functions.len(), 1);
    assert_eq!(
        compiled
            .program
            .function_regions
            .iter()
            .filter(|region| region.prototype_id.is_some())
            .count(),
        1
    );
    let mut ip = 0usize;
    let mut callscript_count = 0usize;
    while ip < compiled.program.code.len() {
        let opcode = vm::OpCode::try_from(compiled.program.code[ip])
            .expect("compiler should emit valid opcodes");
        if opcode == vm::OpCode::CallScript {
            callscript_count += 1;
        }
        ip += 1 + opcode.operand_len();
    }
    assert_eq!(callscript_count, 3);

    let mut runtime = vm::Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    assert_eq!(
        runtime.run().expect("runtime should halt"),
        VmStatus::Halted
    );
    assert_eq!(
        runtime.stack(),
        &[Value::Int(2), Value::Int(3), Value::Int(4)]
    );
}

#[test]
fn mutual_recursion_resolves_forward_function_declarations() {
    let case = rustscript_runtime_case(
        "mutual recursion",
        r#"
            fn a(x) { if x == 0 => { 0 } else => { b(x - 1); x } }
            fn b(x) { if x == 0 => { 0 } else => { a(x - 1); x } }
            a(4);
        "#,
        vec![Value::Int(4)],
    );
    run_runtime_case(&case);
}

#[test]
fn finite_recursive_closure_unwinds_and_returns() {
    let case = rustscript_runtime_case(
        "finite recursive closure",
        r#"
            let recurse = |x| if x == 0 => { 0 } else => { recurse(x - 1); x };
            recurse(5);
        "#,
        vec![Value::Int(5)],
    );
    run_runtime_case(&case);
}

#[test]
fn recursive_closure_uses_self_binding_and_hits_depth_limit() {
    let compiled = vm::compile_source_for_repl(
        r#"
            let recurse = |x| recurse(x);
            recurse(1);
        "#,
    )
    .expect("recursive closure should compile");
    let mut runtime = vm::Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    assert!(matches!(
        runtime.run(),
        Err(vm::VmError::CallStackOverflow { .. })
    ));
}

#[test]
fn rustscript_closure_value_runtime_cases_work() {
    let cases = vec![
        RuntimeCase {
            name: "closure values are first class and can be passed to functions",
            source: r#"
                fn apply_twice(func, value) {
                    let once = func(value);
                    func(once);
                }

                let inc = |x| x + 1;
                apply_twice(inc, 40);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(42)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "empty parameter closure literal captures and runs",
            source: r#"
                let x = 41;
                let f = || x + 1;
                f();
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(42)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "named functions are first class and can be passed to functions",
            source: r#"
                fn add_one(value) {
                    value + 1;
                }
                fn apply_twice(func, value) {
                    let once = func(value);
                    func(once);
                }

                apply_twice(add_one, 40);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(42)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "closure capture can feed named function call",
            source: r#"
                fn add(value, delta) {
                    value + delta;
                }
                let delta = 1;
                let apply = |value| add(value, delta);
                apply(41);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(42)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "closure capture numeric field is copy by default",
            source: r#"
                let mut p = { a: 1 };
                let f = |_| p.a + p.a;
                f(0);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(2)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "closure capture string concat auto copies field reads",
            source: r#"
                let p = { a: "x" };
                let f = |_| p.a + p.a;
                f(0);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::string("xx")],
            expected_locals: None,
        },
        RuntimeCase {
            name: "closure capture string concat with borrowed rhs is allowed",
            source: r#"
                let p = { a: "x" };
                let f = |_| p.a + &p.a;
                f(0);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::string("xx")],
            expected_locals: None,
        },
        RuntimeCase {
            name: "closure capture string concat with mut borrowed rhs is allowed",
            source: r#"
                let mut p = { a: "x" };
                let f = |_| p.a + &mut p.a;
                f(0);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::string("xx")],
            expected_locals: None,
        },
        RuntimeCase {
            name: "closure capture via copy keeps source reusable",
            source: r#"
                let a = "x";
                let f = |d| d + a.copy();
                let d = a;
                f(d);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::string("xx")],
            expected_locals: None,
        },
        RuntimeCase {
            name: "closure capture via borrow keeps source reusable",
            source: r#"
                let a = "x";
                let f = |d| d + &a;
                let d = a;
                f(d);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::string("xx")],
            expected_locals: None,
        },
        RuntimeCase {
            name: "closure capture via mut borrow keeps source reusable",
            source: r#"
                let mut a = "x";
                let f = |d| d + &mut a;
                let d = a;
                f(d);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::string("xx")],
            expected_locals: None,
        },
        RuntimeCase {
            name: "moved closure capture value stays alive until closure use",
            source: r#"
                fn apply_once(func, value) {
                    func(value);
                }
                let seed = "!";
                let closure = |x| x + seed;
                apply_once(closure, "a");
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::string("a!")],
            expected_locals: None,
        },
    ];
    run_runtime_cases(&cases);
}

#[test]
fn rustscript_closure_value_parse_rejection_cases_work() {
    let cases = vec![
        ParseErrorCase {
            name: "closure capture respects non numeric field move checks",
            source: r#"
                let p = { a: "x" };
                let _moved = p.a;
                let f = |_| p.a;
                f(0);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["moved"],
        },
        ParseErrorCase {
            name: "closure mut borrow still respects non numeric field move checks",
            source: r#"
                let mut p = { a: "x" };
                let _moved = p.a;
                let f = |_| &mut p.a;
                f(0);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["moved"],
        },
        ParseErrorCase {
            name: "closure value from partial control flow is rejected on call",
            source: r#"
                if true {
                    let inc = |x| x + 1;
                }
                inc(1);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["inc", "unavailable"],
        },
        ParseErrorCase {
            name: "closure default capture moves movable local",
            source: r#"
                let a = "";
                let x = |d| { d + a };
                let d = a;
                d;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["local 'a'", "moved"],
        },
        ParseErrorCase {
            name: "function value from partial control flow is rejected on call",
            source: r#"
                fn add_one(value: int) -> int {
                    value + 1;
                }
                if true {
                    let f = add_one;
                }
                f(1);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["f", "unavailable"],
        },
    ];
    for case in &cases {
        expect_parse_error_case(case);
    }
}

#[test]
fn closure_mut_capture_updates_outer_local() {
    let case = rustscript_runtime_case(
        "closure mutation capture updates outer local",
        r#"
            let mut state: string = "";
            let sink = |delta| if true => {
                state = state + delta;
                { action: "continue" }
            } else => {
                { action: "skip" }
            };
            let _ = sink("a");
            state;
        "#,
        vec![Value::string("a")],
    );
    run_runtime_case(&case);
}

#[test]
fn closure_mut_capture_survives_multiple_calls() {
    let case = rustscript_runtime_case(
        "closure mutation capture survives multiple calls",
        r#"
            let mut state: string = "";
            let sink = |delta| if true => {
                state = state + delta;
                { action: "continue" }
            } else => {
                { action: "skip" }
            };
            let _ = sink("a");
            let _ = sink("b");
            let _ = sink("c");
            state;
        "#,
        vec![Value::string("abc")],
    );
    run_runtime_case(&case);
}

#[test]
fn closure_mut_capture_is_visible_after_callback_returns() {
    let case = rustscript_runtime_case(
        "closure mutation capture visible after callback returns",
        r#"
            fn invoke(cb, x) {
                let _ = cb(x);
                null
            }
            let mut state: string = "";
            let sink = |delta| if true => {
                state = state + delta;
                { action: "continue" }
            } else => {
                { action: "skip" }
            };
            invoke(sink, "a");
            invoke(sink, "b");
            state;
        "#,
        vec![Value::Null, Value::Null, Value::string("ab")],
    );
    run_runtime_case(&case);
}

#[test]
fn closure_mut_capture_two_closures_share_one_cell() {
    let case = rustscript_runtime_case(
        "two closures mutating one captured local observe one value",
        r#"
            let mut state: string = "";
            let first = |delta| if true => {
                state = state + delta;
                { action: "continue" }
            } else => {
                { action: "skip" }
            };
            let second = |delta| if true => {
                state = state + delta;
                { action: "continue" }
            } else => {
                { action: "skip" }
            };
            let _ = first("x");
            let _ = second("y");
            state;
        "#,
        vec![Value::string("xy")],
    );
    run_runtime_case(&case);
}

#[test]
fn closure_copy_capture_keeps_source_reusable() {
    let case = rustscript_runtime_case(
        "closure copy capture keeps source reusable",
        r#"
            let a = "x";
            let f = |d| d + a.copy();
            let d = a;
            f(d);
        "#,
        vec![Value::string("xx")],
    );
    run_runtime_case(&case);
}

#[test]
fn closure_by_value_move_still_rejects_later_outer_use() {
    let case = ParseErrorCase {
        name: "closure by-value capture of movable local rejects later outer use",
        source: r#"
            let a = "";
            let f = |d| d + a;
            let _ = f("x");
            a;
        "#,
        flavor: SourceFlavor::RustScript,
        expected_contains_all: &["local 'a'", "moved"],
    };
    expect_parse_error_case(&case);
}

#[test]
fn closure_mut_capture_from_immutable_source_is_rejected() {
    let case = ParseErrorCase {
        name: "closure mutation capture from immutable source is rejected",
        source: r#"
            let state: string = "";
            let sink = |delta| if true => {
                state = state + delta;
                { action: "continue" }
            } else => {
                { action: "skip" }
            };
            let _ = sink("a");
            state;
        "#,
        flavor: SourceFlavor::RustScript,
        expected_contains_all: &["immutable local 'state'"],
    };
    expect_parse_error_case(&case);
}

#[test]
fn closure_mut_capture_compound_add_assign_updates_outer_local() {
    let case = rustscript_runtime_case(
        "closure `+=` on captured local updates outer local",
        r#"
            let mut state: int = 0;
            let bump = |delta| if true => {
                state += delta;
                { action: "continue" }
            } else => {
                { action: "skip" }
            };
            let _ = bump(1);
            let _ = bump(2);
            state;
        "#,
        vec![Value::Int(3)],
    );
    run_runtime_case(&case);
}

#[test]
fn closure_write_only_capture_assignment_overwrites_outer_local() {
    let case = rustscript_runtime_case(
        "closure write-only capture assignment (RHS does not read the slot) overwrites outer local",
        r#"
            let mut state: string = "initial";
            let reset = |value| if true => {
                state = value;
                { action: "continue" }
            } else => {
                { action: "skip" }
            };
            let _ = reset("after");
            state;
        "#,
        vec![Value::string("after")],
    );
    run_runtime_case(&case);
}

#[test]
fn closure_mut_capture_compound_and_write_only_modes_stay_shared() {
    for (name, source) in [
        (
            "compound `+=` capture is shared-mutable, not a move",
            r#"
                let mut state: int = 0;
                let bump = |delta| if true => {
                    state += delta;
                    null
                } else => {
                    null
                };
                let _ = bump(1);
                state;
            "#,
        ),
        (
            "write-only `=` capture is shared-mutable, not a move",
            r#"
                let mut state: string = "initial";
                let reset = |value| if true => {
                    state = value;
                    null
                } else => {
                    null
                };
                let _ = reset("after");
                state;
            "#,
        ),
    ] {
        let compiled = vm::compile_source_with_flavor(source, SourceFlavor::RustScript)
            .unwrap_or_else(|err| panic!("{name} should compile: {err}"));
        let prototype = compiled
            .program
            .callable_prototypes
            .iter()
            .find(|prototype| {
                prototype.kind == vm::CallableKind::Closure
                    && prototype
                        .capture_modes
                        .contains(&vm::CaptureBindingMode::BorrowMut)
            })
            .unwrap_or_else(|| panic!("{name} should carry a BorrowMut capture"));
        assert!(
            prototype
                .capture_modes
                .iter()
                .all(|mode| *mode != vm::CaptureBindingMode::Move),
            "{name} must not be classified as a move"
        );
    }
}

#[test]
fn closure_mut_capture_cell_is_fresh_after_vm_reset() {
    // A re-run of the same program on the same VM starts from a fresh
    // capture cell: the second run never reads the previous run's cell
    // value.
    let compiled = vm::compile_source_with_flavor(
        r#"
            let mut state: string = "";
            let sink = |delta| if true => {
                state = state + delta;
                { action: "continue" }
            } else => {
                { action: "skip" }
            };
            let _ = sink("a");
            let _ = sink("b");
            state;
        "#,
        SourceFlavor::RustScript,
    )
    .expect("mutable capture source should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("first run should halt"), VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::string("ab")],
        "first run should accumulate both deltas in the shared cell"
    );

    // Reset must close the run-scoped capture state: the operand stack
    // empties and the cell-backed local slot returns to Null.
    vm.reset_for_reuse();
    assert!(
        vm.stack().is_empty(),
        "reset should clear the operand stack"
    );
    assert!(
        vm.locals().iter().all(|local| *local == Value::Null),
        "reset should clear every local slot, including the cell-backed one"
    );

    // The second run starts from a fresh cell: accumulating the same two
    // deltas yields exactly "ab", not a value derived from the first run's
    // cell contents.
    assert_eq!(vm.run().expect("second run should halt"), VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::string("ab")],
        "a re-run must not read the previous run's capture cell value"
    );
}

#[test]
fn closure_explicit_move_then_use_inside_body_is_rejected() {
    let case = ParseErrorCase {
        name: "closure explicit move inside body rejects later use of captured local",
        source: r#"
            let a = "";
            let f = |d| if true => {
                let y = a;
                let z = a;
                y + z
            } else => {
                ""
            };
            let _ = f("x");
        "#,
        flavor: SourceFlavor::RustScript,
        // The captured slot is an unnamed hidden local (`#N`), so the moved
        // local is reported by its generated name.
        expected_contains_all: &["local '#", "moved"],
    };
    expect_parse_error_case(&case);
}

#[test]
fn rustscript_closure_captured_callable_invocation_works() {
    let cases = vec![
        RuntimeCase {
            name: "captured function-valued local can be invoked from closure body",
            source: r#"
                fn add_one(value) {
                    value + 1;
                }
                let func = add_one;
                let apply = |value| func(value);
                apply(41);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(42)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "captured closure-valued local can be invoked from closure body",
            source: r#"
                let inc = |x| x + 1;
                let apply = |value| inc(value);
                apply(41);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(42)],
            expected_locals: None,
        },
    ];
    for case in &cases {
        run_runtime_case(case);
    }
}

#[test]
fn rustscript_move_and_alias_runtime_cases_work() {
    let cases = vec![
        RuntimeCase {
            name: "numeric field access is copy by default",
            source: r#"
                let p = { a: 1, b: 1.5 };
                let first = p.a;
                let second = p.a;
                let sum = p.b + p.b;
                first + second;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(2)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "numeric field access is copy by default when initialized from numeric local",
            source: r#"
                let n = 1;
                let p = { a: n };
                let first = p.a;
                let second = p.a;
                first + second;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(2)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "copyable field access remains copyable through local roundtrip",
            source: r#"
                let p = { a: 1 };
                let x = p.a;
                let q = { b: x };
                let first = q.b;
                let second = q.b;
                first + second;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(2)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "bool fields are copy by default and null object entries are omitted",
            source: r#"
                let p = { b: true, n: null };
                let b1 = p.b;
                let b2 = p.b;
                if b1 && b2 && !p.has("n") {
                    1;
                } else {
                    0;
                }
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(1)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "non numeric field access can be copied with copy",
            source: r#"
                let p = { a: "x" };
                let first = p.a.copy();
                let second = p.a;
                first + second;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::string("xx")],
            expected_locals: None,
        },
        RuntimeCase {
            name: "non copy local can be explicitly copied before local move",
            source: r#"
                let a = "2";
                let b = a.copy();
                a + b;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::string("22")],
            expected_locals: None,
        },
        RuntimeCase {
            name: "array sibling index remains accessible after partial move",
            source: r#"
                let arr = [1, 2, 3, 4];
                let first = arr[0];
                let second = arr[1];
                first + second;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(3)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "map sibling field remains accessible after partial move",
            source: r#"
                let m = { a: 1, b: 2 };
                let first = m.a;
                let second = m.b;
                first + second;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(3)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "non numeric field access can be borrowed with ampersand",
            source: r#"
                let p = { a: "x" };
                let first = &p.a;
                let second = p.a;
                first + second;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::string("xx")],
            expected_locals: None,
        },
        RuntimeCase {
            name: "non numeric field access can be borrowed with rust style mut ampersand",
            source: r#"
                let mut p = { a: "x" };
                let first = &mut p.a;
                let second = p.a;
                first + second;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::string("xx")],
            expected_locals: None,
        },
        RuntimeCase {
            name: "non numeric field access can be borrowed with spaced mut ampersand",
            source: r#"
                let mut p = { a: "x" };
                let first = & mut p.a;
                let second = p.a;
                first + second;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::string("xx")],
            expected_locals: None,
        },
        RuntimeCase {
            name: "non numeric field access can be mut borrowed with parenthesized field",
            source: r#"
                let mut p = { a: "x" };
                let first = &mut (p.a);
                let second = p.a;
                first + second;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::string("xx")],
            expected_locals: None,
        },
        RuntimeCase {
            name: "non numeric field mut borrow can be passed through function call",
            source: r#"
                fn id(x) {
                    x;
                }
                let mut p = { a: "x" };
                let first = id(&mut p.a);
                let second = p.a;
                first + second;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::string("xx")],
            expected_locals: None,
        },
        RuntimeCase {
            name: "moved field can be reinitialized with indexed assignment",
            source: r#"
                let mut p = { a: "222", b: "666" };
                let _moved = p.a;
                p.a = "444";
                let y = p.a;
                y + p.b;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::string("444666")],
            expected_locals: None,
        },
        RuntimeCase {
            name: "moved field reinitialized inside loop remains usable",
            source: r#"
                let mut p = { a: "start" };
                let mut i = 0;
                while i < 2 {
                    let _moved = p.a;
                    p.a = "new";
                    i = i + 1;
                }
                p.a;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::string("new")],
            expected_locals: None,
        },
        RuntimeCase {
            name: "mutating after copy detach is allowed",
            source: r#"
                let mut p = { a: 1 };
                let mut q = p;
                q = q.copy();
                p.a = 2;
                p.a + q.a;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(3)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "reassigning collection breaks old alias for target local",
            source: r#"
                let mut a = [1];
                let b = a;
                a = [2];
                a[0] = 3;
                a[0] + b[0];
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(4)],
            expected_locals: None,
        },
    ];
    run_runtime_cases(&cases);
}

#[test]
fn rustscript_local_move_consumes_source_slot_at_runtime() {
    let source = r#"
        let a = "2";
        let b = a;
        b;
    "#;
    let compiled = vm::compile_source_for_repl(source).expect("compile should succeed");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::string("2")]);
    assert!(
        !local_visible_at_current_line(&vm, "a"),
        "moved source local should not remain visible at the final line"
    );
    assert!(
        local_visible_at_current_line(&vm, "b"),
        "move target should remain visible at the final line"
    );
}

#[test]
fn rustscript_interprocedural_consumed_param_moves_caller_local_at_runtime() {
    let source = r#"
        fn consume_once(value) {
            let taken = value;
            taken;
        }

        let a = "2";
        let b = consume_once(a);
        b;
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("debug info should exist");
    let a_index = debug.local_index("a").expect("a binding should exist");

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::string("2")]);
    assert_eq!(vm.locals()[a_index as usize], Value::Null);
}

#[test]
fn rustscript_field_move_updates_runtime_container_state() {
    let source = r#"
        let mut p = { a: "x", b: "y" };
        let moved = p.a;
        let rest = p.b;
        moved + rest;
    "#;
    let compiled = vm::compile_source_for_repl(source).expect("compile should succeed");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::string("xy")]);
}

#[test]
fn rustscript_field_move_expr_statement_updates_runtime_container_state() {
    let source = r#"
        let mut p = { a: "x", b: "y" };
        p.a;
    "#;
    let compiled = vm::compile_source_for_repl(source).expect("compile should succeed");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::string("x")]);

    let Some(Value::Map(entries)) = vm.locals().first() else {
        panic!("expected first local to be moved map container");
    };
    let mut saw_a_present = false;
    let mut saw_b_y = false;
    for (key, value) in entries.iter() {
        match (key, value) {
            (Value::String(name), _) if name.as_str() == "a" => saw_a_present = true,
            (Value::String(name), Value::String(text))
                if name.as_str() == "b" && text.as_str() == "y" =>
            {
                saw_b_y = true
            }
            _ => {}
        }
    }
    assert!(
        !saw_a_present,
        "expected moved field 'a' to be removed from local container"
    );
    assert!(
        saw_b_y,
        "expected untouched field 'b' to remain present in local container"
    );
}

#[test]
fn rustscript_index_move_updates_runtime_container_state() {
    let source = r#"
        let mut arr = ["x", "y"];
        let moved = arr[0];
        let rest = arr[1];
        moved + rest;
    "#;
    let compiled = vm::compile_source_for_repl(source).expect("compile should succeed");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::string("xy")]);
}

#[test]
fn rustscript_move_and_alias_parse_rejection_cases_work() {
    let branch_case = SourceErrorCase {
        name: "local move in one branch is rejected after merge",
        source: r#"
            let value = "x";
            if true {
                let _moved = value;
            } else {
                0;
            }
            value;
        "#,
        flavor: SourceFlavor::RustScript,
        expected_kind: SourceErrorKind::Parse,
        expected_contains_all: &["moved earlier", "value.copy()"],
    };
    expect_source_error_case(&branch_case);

    let loop_branch_case = SourceErrorCase {
        name: "move in one loop branch is visible after loop",
        source: r#"
            let p = { a: "x" };
            let mut i = 0;
            while i < 2 {
                if i == 0 {
                    let _moved = p.a;
                }
                i = i + 1;
            }
            p.a;
        "#,
        flavor: SourceFlavor::RustScript,
        expected_kind: SourceErrorKind::Parse,
        expected_contains_all: &["moved earlier", "p.a.copy()"],
    };
    expect_source_error_case(&loop_branch_case);

    let cases = vec![
        ParseErrorCase {
            name: "non numeric field access is moved by default",
            source: r#"
                let p = { a: "x" };
                let first = p.a;
                let second = p.a;
                second;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["p.a", "moved"],
        },
        ParseErrorCase {
            name: "whole local use after field move is rejected",
            source: r#"
                let p = { a: "x", b: "y" };
                let _moved = p.a;
                p;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["p", "partially moved"],
        },
        ParseErrorCase {
            name: "whole local use after index move is rejected",
            source: r#"
                let arr = [1, 2, 3];
                let _moved = arr[0];
                arr.length;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["arr", "partially moved"],
        },
        ParseErrorCase {
            name: "whole local use after slice move is rejected",
            source: r#"
                let arr = [1, 2, 3, 4];
                let _moved = arr[1:3];
                arr.length;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["arr", "partially moved"],
        },
        ParseErrorCase {
            name: "non copy local assignment moves source by default",
            source: r#"
                let a = "2";
                let b = a;
                a + b;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["local 'a'", "moved"],
        },
        ParseErrorCase {
            name: "callee consumed parameter moves caller local",
            source: r#"
                fn consume_once(value) {
                    let taken = value;
                    taken;
                }

                let a = "x";
                consume_once(a);
                a;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["local 'a'", "moved"],
        },
        ParseErrorCase {
            name: "transitive consumed parameter moves caller local",
            source: r#"
                fn consume_once(value) {
                    let taken = value;
                    taken;
                }

                fn forward(input) {
                    consume_once(input);
                    0;
                }

                let a = "x";
                forward(a);
                a;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["local 'a'", "moved"],
        },
        ParseErrorCase {
            name: "local move in loop body is rejected on next iteration",
            source: r#"
                let value = "x";
                let mut i = 0;
                while i < 2 {
                    let _moved = value;
                    i = i + 1;
                }
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["local 'value'", "moved"],
        },
        ParseErrorCase {
            name: "borrowed then moved then second move still fails",
            source: r#"
                let p = { a: "x" };
                let _loan = &p.a;
                let _move = p.a;
                let again = p.a;
                again;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["moved"],
        },
        ParseErrorCase {
            name: "borrowed field still respects prior move errors",
            source: r#"
                let p = { a: "x" };
                let _moved = p.a;
                let again = &p.a;
                again;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["moved"],
        },
        ParseErrorCase {
            name: "mut borrowed field still respects prior move errors",
            source: r#"
                let mut p = { a: "x" };
                let _moved = p.a;
                let again = &mut p.a;
                again;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["moved"],
        },
        ParseErrorCase {
            name: "mut borrow rejects temporary expression target",
            source: r#"
                let value = &mut (1 + 2);
                value;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["mutable borrow target"],
        },
        ParseErrorCase {
            name: "mut borrow rejects readonly borrow expression target",
            source: r#"
                let p = { a: "x" };
                let value = &mut &p.a;
                value;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["mutable borrow target"],
        },
        ParseErrorCase {
            name: "mut borrowed function argument still respects prior move errors",
            source: r#"
                fn id(x) {
                    x;
                }
                let mut p = { a: "x" };
                let _moved = p.a;
                id(&mut p.a);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["moved"],
        },
        ParseErrorCase {
            name: "while loop repeated field move is rejected",
            source: r#"
                let p = { a: "x" };
                let mut i = 0;
                while i < 2 {
                    let _moved = p.a;
                    i = i + 1;
                }
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["p.a", "moved"],
        },
        ParseErrorCase {
            name: "for loop repeated field move is rejected",
            source: r#"
                let p = { a: "x" };
                for i in 0..2 {
                    let _moved = p.a;
                }
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["p.a", "moved"],
        },
        ParseErrorCase {
            name: "break path preserves moved field state after loop",
            source: r#"
                let p = { a: "x" };
                while true {
                    let _moved = p.a;
                    break;
                }
                let again = p.a;
                again;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["p.a", "moved"],
        },
        ParseErrorCase {
            name: "continue path rechecks moved field on next iteration",
            source: r#"
                let p = { a: "x" };
                let mut i = 0;
                while i < 2 {
                    let _moved = p.a;
                    i = i + 1;
                    continue;
                }
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["p.a", "moved"],
        },
        ParseErrorCase {
            name: "match arm move marks field as possibly moved",
            source: r#"
                let p = { a: "x" };
                let key = 1;
                let _v = match key {
                    1 => p.a,
                    _ => "fallback",
                };
                p.a;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["p.a", "moved"],
        },
        ParseErrorCase {
            name: "map mutation is rejected while collection alias exists",
            source: r#"
                let mut p = { a: 1 };
                let q = p;
                p.a = 2;
                p.a + q.a;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["aliased", "p"],
        },
        ParseErrorCase {
            name: "array mutation is rejected while collection alias exists",
            source: r#"
                let mut a = [1];
                let b = a;
                a[0] = 2;
                a[0] + b[0];
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["aliased", "a"],
        },
        ParseErrorCase {
            name: "array append is rejected while collection alias exists",
            source: r#"
                let mut a = [1];
                let b = a;
                a[a.length] = 2;
                b[0];
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["aliased", "a"],
        },
        ParseErrorCase {
            name: "array mutation is rejected while mut borrow alias exists",
            source: r#"
                let mut a = [1];
                let b = &mut a;
                a[0] = 2;
                b[0];
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["aliased", "a"],
        },
        ParseErrorCase {
            name: "collection alias from passthrough function is tracked",
            source: r#"
                fn id(x) {
                    x;
                }
                let mut a = [1];
                let b = id(a);
                a[0] = 2;
                a[0] + b[0];
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["aliased", "a"],
        },
        ParseErrorCase {
            name: "collection alias from passthrough mut borrow function is tracked",
            source: r#"
                fn id(x) {
                    x;
                }
                let mut a = [1];
                let b = id(&mut a);
                a[0] = 2;
                a[0] + b[0];
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["aliased", "a"],
        },
    ];
    for case in &cases {
        expect_parse_error_case(case);
    }
}

#[test]
fn rustscript_mutability_runtime_cases_work() {
    let cases = vec![
        RuntimeCase {
            name: "mutable local assignment requires and supports let mut",
            source: r#"
                let mut value = 1;
                value = value + 1;
                value;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(2)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "mutable member assignment supports let mut binding",
            source: r#"
                let mut profile = { score: 1 };
                profile.score = 2;
                profile.score;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(2)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "mutable index assignment supports let mut binding",
            source: r#"
                let mut arr = [1];
                arr[0] = 2;
                arr[0];
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(2)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "mutable borrow requires mutable root binding and succeeds with let mut",
            source: r#"
                let mut p = { a: "x" };
                let first = &mut p.a;
                let second = p.a;
                first + second;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::string("xx")],
            expected_locals: None,
        },
        RuntimeCase {
            name: "rust style exclusive range for loop works",
            source: r#"
                let mut total = 0;
                for i in 0..3 {
                    total = total + i;
                }
                total;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(3)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "rust style inclusive range for loop works",
            source: r#"
                let mut total = 0;
                for i in 1..=3 {
                    total = total + i;
                }
                total;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(6)],
            expected_locals: None,
        },
    ];
    run_runtime_cases(&cases);
}

#[test]
fn c_style_for_loop_parentheses_are_rejected() {
    let err = match compile_source(
        r#"
        let mut total = 0;
        for (let mut i = 0; i < 3; i = i + 1) {
            total = total + i;
        }
        total;
    "#,
    ) {
        Ok(_) => panic!("parenthesized for loop should be rejected"),
        Err(err) => err,
    };

    let text = err.to_string();
    assert!(
        text.contains("expected Rust-style for-in loop") && text.contains("for i in 0..n"),
        "unexpected error: {text}"
    );
}

#[test]
fn rustscript_mutability_parse_rejection_cases_work() {
    let cases = vec![
        ParseErrorCase {
            name: "assignment to immutable local is rejected",
            source: r#"
                let value = 1;
                value = 2;
                value;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["immutable local", "let mut value"],
        },
        ParseErrorCase {
            name: "member assignment through immutable local is rejected",
            source: r#"
                let profile = { score: 1 };
                profile.score = 2;
                profile.score;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["immutable local", "let mut profile"],
        },
        ParseErrorCase {
            name: "index assignment through immutable local is rejected",
            source: r#"
                let arr = [1];
                arr[0] = 2;
                arr[0];
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["immutable local", "let mut arr"],
        },
        ParseErrorCase {
            name: "mutable borrow of immutable local is rejected",
            source: r#"
                let profile = { score: 1 };
                let b = &mut profile.score;
                b;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["immutable local", "let mut profile"],
        },
        ParseErrorCase {
            name: "mutable borrow of immutable collection local is rejected",
            source: r#"
                let arr = [1];
                let b = &mut arr;
                b[0];
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["immutable local", "let mut arr"],
        },
    ];
    for case in &cases {
        expect_parse_error_case(case);
    }
}

#[test]
fn liveness_clears_local_after_closure_value_last_use() {
    let source = r#"
        fn apply_once(func, value) {
            func(value);
        }

        let mut closure = "stale";
        let base = 1;
        closure = |x| x + base;
        let out = apply_once(closure, 41);
        out;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("debug info should exist");
    let closure_index = debug
        .local_index("closure")
        .expect("closure binding should exist");

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack().len(),
        1,
        "apply_once result should be the only remaining stack value"
    );
    assert_eq!(vm.stack()[0], Value::Int(42));
    assert!(
        !vm.stack()
            .iter()
            .any(|value| matches!(value, Value::String(text) if text.as_str() == "stale")),
        "stack should not retain pre-call placeholder values"
    );
    assert_eq!(vm.locals()[closure_index as usize], Value::Null);
}

#[test]
fn liveness_clears_local_after_function_value_last_use() {
    let source = r#"
        fn add_one(value) {
            value + 1;
        }
        fn apply_once(func, value) {
            func(value);
        }

        let mut func = "stale";
        func = add_one;
        let out = apply_once(func, 41);
        out;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("debug info should exist");
    let func_index = debug
        .local_index("func")
        .expect("func binding should exist");

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack().len(),
        1,
        "apply_once result should be the only remaining stack value"
    );
    assert_eq!(vm.stack()[0], Value::Int(42));
    assert!(
        !vm.stack()
            .iter()
            .any(|value| matches!(value, Value::String(text) if text.as_str() == "stale")),
        "stack should not retain pre-call placeholder values"
    );
    assert_eq!(vm.locals()[func_index as usize], Value::Null);
}

#[test]
fn script_function_frame_values_are_released_after_return() {
    let source = r#"
        fn make_pair() {
            let left = "L";
            let right = "R";
            left + right;
        }

        make_pair();
        0;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack().last(), Some(&Value::Int(0)));
    assert!(!vm.locals().iter().any(
        |value| matches!(value, Value::String(text) if matches!(text.as_str(), "L" | "R" | "LR"))
    ));
}

#[test]
fn interprocedural_closure_capture_slots_are_cleared_after_last_use() {
    let source = r#"
        fn apply_once(func, value) {
            func(value);
        }

        let seed = "!";
        let closure = |x| x + seed;
        apply_once(closure, "a");
        0;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack().last(), Some(&Value::Int(0)));
    assert!(
        !vm.locals().iter().any(
            |value| matches!(value, Value::String(text) if matches!(text.as_str(), "!" | "a!"))
        )
    );
}

#[test]
fn rustscript_callable_values_can_be_stored_in_arrays() {
    let case = RuntimeCase {
        name: "callable values can be stored in arrays",
        source: r#"
            fn add_one(value: int) -> int {
                value + 1;
            }
            let func = add_one;
            let values = [func];
            let stored = values[0];
            stored(41);
        "#,
        flavor: SourceFlavor::RustScript,
        expected_stack: vec![Value::Int(42)],
        expected_locals: None,
    };
    run_runtime_case(&case);
}

#[test]
fn rustscript_callable_values_can_be_stored_in_maps() {
    let case = RuntimeCase {
        name: "callable values can be stored in maps",
        source: r#"
            fn add_one(value: int) -> int {
                value + 1;
            }
            let values = { f: add_one };
            let stored = values.f;
            stored(41);
        "#,
        flavor: SourceFlavor::RustScript,
        expected_stack: vec![Value::Int(42)],
        expected_locals: None,
    };
    run_runtime_case(&case);
}

#[test]
fn builtin_host_functions_can_be_values() {
    let source = r#"
        let f = len;
        f("abc");
    "#;
    let path = std::env::temp_dir().join(format!(
        "rustscript_callable_builtin_{}_{}.rss",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    ));
    std::fs::write(&path, source).expect("source should write");
    let compiled =
        compile_source_file(path.as_path()).expect("builtin function value should compile");
    assert!(
        compiled
            .program
            .code
            .contains(&(vm::OpCode::CallValue as u8))
    );
    let mut runtime = Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    assert_eq!(
        runtime.run().expect("runtime should halt"),
        VmStatus::Halted
    );
    assert_eq!(runtime.stack(), &[Value::Int(3)]);
    let _ = std::fs::remove_file(path);
}

#[test]
fn compatible_callable_signatures_merge_across_branches() {
    let case = rustscript_runtime_case(
        "compatible callable branch merge",
        r#"
            fn add_one(value: int) -> int { value + 1 }
            fn add_two(value: int) -> int { value + 2 }
            let selected = if true => { add_one } else => { add_two };
            selected(40);
        "#,
        vec![Value::Int(41)],
    );

    run_runtime_case(&case);
}

#[test]
fn incompatible_callable_signatures_are_rejected_across_control_flow() {
    let cases = [
        SourceErrorCase {
            name: "if expression rejects incompatible callable signatures",
            source: r#"
                fn map_int(value: int) -> int { value + 1 }
                fn map_string(value: string) -> string { value + "!" }
                let choose_int = true;
                let selected = if choose_int => { map_int } else => { map_string };
                selected(41);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::IfElseBranchTypeMismatch),
            expected_contains_all: &["callable"],
        },
        SourceErrorCase {
            name: "match expression rejects incompatible callable signatures",
            source: r#"
                fn map_int(value: int) -> int { value + 1 }
                fn map_string(value: string) -> string { value + "!" }
                let selected = match 0 {
                    0 => map_int,
                    _ => map_string,
                };
                selected(41);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::IfElseBranchTypeMismatch),
            expected_contains_all: &["callable"],
        },
        SourceErrorCase {
            name: "loop merge rejects incompatible callable signatures",
            source: r#"
                fn map_int(value: int) -> int { value + 1 }
                fn map_string(value: string) -> string { value + "!" }
                let mut selected = map_int;
                let keep_running = false;
                while keep_running {
                    selected = map_string;
                }
                selected(41);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::IfElseBranchTypeMismatch),
            expected_contains_all: &["callable"],
        },
    ];

    for case in &cases {
        expect_source_error_case(case);
    }
}

#[test]
fn bare_generic_function_values_resolve_from_callable_context() {
    let cases = [
        rustscript_runtime_case(
            "bare generic function value resolves from local annotation",
            r#"
                fn identity<T>(value: T) -> T { value }
                let int_identity: fn(int) -> int = identity;
                int_identity(42);
            "#,
            vec![Value::Int(42)],
        ),
        rustscript_runtime_case(
            "bare generic function value resolves from higher order parameter",
            r#"
                fn identity<T>(value: T) -> T { value }
                fn apply<T>(mapper: fn(T) -> T, value: T) -> T { mapper(value) }
                apply::<int>(identity, 42);
            "#,
            vec![Value::Int(42)],
        ),
    ];

    run_runtime_cases(&cases);
}

#[test]
fn explicit_generic_function_values_use_substituted_callable_schemas() {
    let case = rustscript_runtime_case(
        "explicit generic function values",
        r#"
            fn identity<T>(value: T) -> T { value }
            let int_identity = identity::<int>;
            int_identity(42);
        "#,
        vec![Value::Int(42)],
    );

    run_runtime_case(&case);
}

#[test]
fn rustscript_callable_values_can_be_returned_from_functions() {
    let case = RuntimeCase {
        name: "callable values can be returned from functions",
        source: r#"
            fn add_one(value: int) -> int {
                value + 1;
            }
            fn get_adder() {
                add_one;
            }
            let func = get_adder();
            func(41);
        "#,

        flavor: SourceFlavor::RustScript,
        expected_stack: vec![Value::Int(42)],
        expected_locals: None,
    };
    run_runtime_case(&case);
}

#[test]
fn escaping_closure_retains_its_environment() {
    let case = RuntimeCase {
        name: "escaping closure retains captures after its defining frame returns",
        source: r#"
            fn make_adder(delta: int) {
                |value| value + delta;
            }
            let add_one = make_adder(1);
            add_one(41);
        "#,
        flavor: SourceFlavor::RustScript,
        expected_stack: vec![Value::Int(42)],
        expected_locals: None,
    };
    run_runtime_case(&case);
}

#[test]
fn closure_aliases_share_state_and_factory_evaluations_are_independent() {
    let case = rustscript_runtime_case(
        "closure aliases share one environment while factory calls allocate distinct environments",
        r#"
            fn make_counter() {
                let mut count = 0;
                fn next() {
                    count = count + 1;
                    count
                }
                next
            }
            let first = make_counter();
            let alias = first;
            let second = make_counter();
            first();
            alias();
            second();
        "#,
        vec![Value::Int(1), Value::Int(2), Value::Int(1)],
    );

    run_runtime_case(&case);
}

#[test]
fn borrowed_capture_shares_outer_mutation_cell() {
    let case = rustscript_runtime_case(
        "borrowed capture observes outer writes",
        r#"
            let mut base = 1;
            let read = || &base;
            base = 2;
            read();
            base;
        "#,
        vec![Value::Int(2), Value::Int(2)],
    );

    run_runtime_case(&case);
}

#[test]
fn recursive_mutable_capture_updates_one_shared_cell() {
    let case = rustscript_runtime_case(
        "recursive mutable capture does not restore stale snapshots",
        r#"
            let mut count = 0;
            let recurse = |depth| if depth == 0 => {
                count = count + 1;
                count
            } else => {
                count = count + 1;
                recurse(depth - 1);
                count
            };
            recurse(2);
            count;
        "#,
        vec![Value::Int(3), Value::Int(3)],
    );

    run_runtime_case(&case);
}

#[test]
fn capturing_named_functions_use_closure_runtime_kind() {
    let compiled = vm::compile_source_for_repl(
        r#"
            let captured = 42;
            fn read_captured() { captured }
            read_captured;
        "#,
    )
    .expect("capturing named function should compile");
    let prototype = compiled
        .program
        .callable_prototypes
        .iter()
        .find(|prototype| !prototype.capture_slots.is_empty())
        .expect("capturing named function should have an environment layout");

    assert_eq!(prototype.kind, vm::CallableKind::Closure);
}

#[test]
fn callable_equality_distinguishes_items_aliases_and_closure_instances() {
    let case = RuntimeCase {
        name: "callable equality follows item and environment identity",
        source: r#"
            fn add_one(value: int) -> int { value + 1 }
            let item = add_one;
            let first = |value| value + 1;
            let second = |value| value + 1;
            item == add_one;
            first == first;
            first == second;
        "#,
        flavor: SourceFlavor::RustScript,
        expected_stack: vec![Value::Bool(true), Value::Bool(true), Value::Bool(false)],
        expected_locals: None,
    };
    run_runtime_case(&case);
}

#[test]
fn rustscript_if_and_match_runtime_cases_work() {
    let cases = vec![
        RuntimeCase {
            name: "if expression assignment syntax is supported",
            source: r#"
                let x = if 2 > 1 => { 42 } else => { 0 };
                x;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(42)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "if expression branch blocks support multiline statements",
            source: r#"
                let base = 40;
                let out = if true => {
                    let bump = base + 2;
                    bump
                } else => {
                    let fallback = base - 1;
                    fallback
                };
                out;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(42)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "if expression assignment executes else branch",
            source: r#"
                let mut marker = 0;
                let out = if false => {
                    marker = 1;
                    10
                } else => {
                    marker = 2;
                    20
                };
                marker + out;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(22)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "if expression supports else if chains",
            source: r#"
                let key = 2;
                let out = if key == 1 => { 10 } else if key == 2 => { 20 } else => { 0 };
                out;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(20)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "match expression supports int and wildcard patterns",
            source: r#"
                let value = 2;
                let out = match value {
                    1 => 10,
                    2 => 20,
                    _ => 0,
                };
                out;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(20)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "match expression supports string patterns",
            source: r#"
                let key = "beta";
                let out = match key {
                    "alpha" => 1,
                    "beta" => 2,
                    _ => 0,
                };
                out;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(2)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "match expression supports type patterns",
            source: r#"
                let a = match "value" {
                    Some(String) => 1,
                    _ => 0,
                };
                let b = match 7 {
                    Some(Number) => 2,
                    _ => 0,
                };
                let c = match true {
                    Some(Number) => 100,
                    _ => 3,
                };
                a + b + c;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(6)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "match expression supports None and Some binding for optional values",
            source: r#"
                struct Data { values: [int] }
                let data: Data = { values: [41] };

                let present = match data?.values?.[0] {
                    None => 0,
                    Some(value) => value + 1,
                    _ => 0,
                };
                let missing = match data?.values?.[1] {
                    None => 1,
                    Some(value) => value,
                    _ => 0,
                };

                present + missing;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(43)],
            expected_locals: None,
        },
    ];
    run_runtime_cases(&cases);
}

#[test]
fn rustscript_if_and_match_parse_rejection_cases_work() {
    let cases = vec![
        ParseErrorCase {
            name: "if expression requires else branch",
            source: r#"
                let x = if true => { 1 };
                x;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["requires an else branch"],
        },
        ParseErrorCase {
            name: "match expression rejects unsupported patterns",
            source: r#"
                let value = 1;
                match value {
                    true => 10,
                    _ => 0,
                };
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["int/string/null literals, None, Some(name), type patterns"],
        },
    ];
    for case in &cases {
        expect_parse_error_case(case);
    }
}

#[test]
fn compile_source_file_rustscript_imports_merge_with_scoped_locals() {
    let unique = format!(
        "vm_rss_import_scope_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp module root should be created");

    let module_path = root.join("module.rss");
    let main_path = root.join("main.rss");
    std::fs::write(
        &module_path,
        r#"
        pub fn add_one(x) -> int;
        let shared = 40;
    "#,
    )
    .expect("module source should write");
    std::fs::write(
        &main_path,
        r#"
        use module;
        let shared = add_one(1);
        shared;
    "#,
    )
    .expect("main source should write");

    let compiled = compile_source_file(main_path.as_path()).expect("compile should succeed");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("compiled program should include debug info");
    assert!(
        debug
            .locals
            .iter()
            .any(|local| local.name.ends_with("::shared") && local.name != "shared"),
        "module-scoped local should remain visible in debug metadata with a deterministic module-identity scope (milestone 4), not a bare file stem"
    );
    assert!(
        debug.locals.iter().any(|local| local.name == "shared"),
        "root-scoped local should remain visible in debug metadata"
    );
    assert_eq!(
        compiled
            .functions
            .iter()
            .filter(|func| func.name == "add_one")
            .count(),
        1,
        "imported function should only be declared once",
    );

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    vm.bind_function("add_one", Box::new(AddOne));
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(2)]);

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_file(module_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_source_file_rustscript_imported_function_capture_binds_once() {
    let unique = format!(
        "vm_rss_import_capture_bind_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp module root should be created");

    let module_path = root.join("module.rss");
    let main_path = root.join("main.rss");
    std::fs::write(
        &module_path,
        r#"
        let t = [7];
        fn inner_arg(x) { x[0] }
        pub fn run() { inner_arg(t) }
    "#,
    )
    .expect("module source should write");
    std::fs::write(
        &main_path,
        r#"
        use self::module as m;
        m::run();
    "#,
    )
    .expect("main source should write");

    let compiled = compile_source_file(main_path.as_path()).expect("compile should succeed");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7)]);

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_file(module_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_source_file_imported_capture_survives_later_root_slot_compaction() {
    let unique = format!(
        "vm_rss_import_capture_compaction_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp module root should be created");

    let module_path = root.join("module.rss");
    let main_path = root.join("main.rss");
    std::fs::write(
        &module_path,
        r#"
        let lookup = [7];
        pub fn read() -> int { (&lookup)[0] }
    "#,
    )
    .expect("module source should write");
    std::fs::write(
        &main_path,
        r#"
        use self::module as m;
        let overwrite0 = "a";
        let overwrite1 = "b";
        let overwrite2 = "c";
        let overwrite3 = "d";
        let overwrite4 = "e";
        let overwrite5 = "f";
        let overwrite6 = "g";
        let overwrite7 = "h";
        let overwrite8 = "i";
        m::read();
    "#,
    )
    .expect("main source should write");

    let compiled = compile_source_file(main_path.as_path()).expect("compile should succeed");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7)]);

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_file(module_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_source_file_rustscript_imported_borrow_capture_survives_nested_calls() {
    let unique = format!(
        "vm_rss_import_direct_capture_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp module root should be created");

    let module_path = root.join("module.rss");
    let main_path = root.join("main.rss");
    std::fs::write(
        &module_path,
        r#"
        let lut = {"a": 1};
        let digits = [0, 2];
        fn parse_key(text: string) -> int { (&lut)[text[0:1]] }
        fn read_digit() -> int { (&digits)[1] }
        pub fn run() -> int {
            let a = parse_key("ab");
            let b = read_digit();
            let c = parse_key("ab");
            a + b + c;
        }
    "#,
    )
    .expect("module source should write");
    std::fs::write(
        &main_path,
        r#"
        use self::module as m;
        m::run();
    "#,
    )
    .expect("main source should write");

    let compiled = compile_source_file(main_path.as_path()).expect("compile should succeed");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4)]);

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_file(module_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_source_file_rustscript_imported_direct_capture_multiple_move_is_rejected() {
    let unique = format!(
        "vm_rss_import_direct_capture_error_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp module root should be created");

    let module_path = root.join("module.rss");
    let main_path = root.join("main.rss");
    std::fs::write(
        &module_path,
        r#"
        let lut = {"a": 1};
        fn parse_key(text: string) -> int { lut[text[0:1]] }
        fn parse_again(text: string) -> int { lut[text[0:1]] }
        pub fn run() {
            parse_key("ab");
        }
    "#,
    )
    .expect("module source should write");
    std::fs::write(
        &main_path,
        r#"
        use self::module as m;
        m::run();
    "#,
    )
    .expect("main source should write");

    let err = match compile_source_file(main_path.as_path()) {
        Ok(_) => panic!("compile should fail"),
        Err(err) => err,
    };
    match err {
        vm::SourcePathError::SourceWithMap {
            error: vm::SourceError::Parse(parse),
            ..
        } => {
            assert!(
                parse.message.contains("lut") && parse.message.contains("moved"),
                "unexpected parse error: {parse:?}"
            );
        }
        other => panic!("expected parse error, got {other:?}"),
    }

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_file(module_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_source_file_rustscript_rejects_import_keyword() {
    let unique = format!(
        "vm_rss_use_keyword_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp module root should be created");
    let main_path = root.join("main.rss");
    std::fs::write(&main_path, "import \"./module.rss\";\n1;\n").expect("source should write");

    let err = match compile_source_file(main_path.as_path()) {
        Ok(_) => panic!("legacy import syntax should be rejected for RustScript"),
        Err(err) => err,
    };
    assert!(
        matches!(
            &err,
            vm::SourcePathError::SourceWithMap {
                error: vm::SourceError::Parse(_),
                ..
            }
        ),
        "expected parser-level import diagnostic, got {err:?}"
    );
    assert!(err.to_string().contains("expected ';' after expression"));
    assert_eq!(
        err.sources().unwrap().file(0).unwrap().name,
        main_path.to_string_lossy()
    );

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_source_file_rustscript_supports_namespace_and_named_imports() {
    let unique = format!(
        "vm_rustscript_namespace_import_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp module root should be created");

    let module_path = root.join("strings.rss");
    std::fs::write(
        &module_path,
        r#"
        fn eq(lhs, rhs) {
            lhs == rhs;
        }
        pub fn is_empty(value) {
            eq(value, "");
        }
        pub fn non_empty(value) {
            eq(is_empty(value), false);
        }
    "#,
    )
    .expect("module source should write");

    let main_path = root.join("main.rss");
    std::fs::write(
        &main_path,
        r#"
        use strings as string;
        use strings::{is_empty as is_empty};

        string::non_empty("rss");
        is_empty("");
    "#,
    )
    .expect("main source should write");

    let compiled = compile_source_file(main_path.as_path()).expect("compile should succeed");
    assert!(
        compiled.functions.is_empty(),
        "module functions should be fully inlined for RustScript root"
    );

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(true), Value::Bool(true)]);

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_file(module_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_source_file_rustscript_all_public_import_supports_namespace_calls() {
    let unique = format!(
        "vm_rustscript_all_public_namespace_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp module root should be created");

    let module_path = root.join("runtime.rss");
    std::fs::write(
        &module_path,
        r#"
        pub fn sleep(ms) {
            ms;
        }
    "#,
    )
    .expect("module source should write");

    let main_path = root.join("main.rss");
    std::fs::write(
        &main_path,
        r#"
        use runtime;
        runtime::sleep(3);
    "#,
    )
    .expect("main source should write");

    let compiled = compile_source_file(main_path.as_path()).expect("compile should succeed");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(3)]);

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_file(module_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_source_file_rustscript_missing_runtime_module_falls_back_to_host_namespace() {
    let unique = format!(
        "vm_rustscript_runtime_host_fallback_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp module root should be created");

    let main_path = root.join("main.rss");
    std::fs::write(
        &main_path,
        r#"
        use runtime;
        runtime::sleep(3);
    "#,
    )
    .expect("main source should write");

    let compiled = compile_source_file(main_path.as_path()).expect("compile should succeed");
    assert!(
        compiled
            .program
            .imports
            .iter()
            .any(|import| import.name == "runtime::sleep"),
        "missing runtime.rss should fall back to runtime host namespace import"
    );

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_source_file_rustscript_host_namespace_alias_maps_to_host_import() {
    let unique = format!(
        "rustscript_runtime_alias_host_fallback_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp module root should be created");

    let main_path = root.join("main.rss");
    std::fs::write(
        &main_path,
        r#"
        use rate_limit as rl;
        rl::allow("client-a", 2, 30);
    "#,
    )
    .expect("main source should write");

    let compiled = compile_source_file(main_path.as_path()).expect("compile should succeed");
    assert!(
        compiled
            .program
            .imports
            .iter()
            .any(|import| import.name == "rate_limit::allow"),
        "namespace alias should map to rate_limit host import"
    );

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn rustscript_language_runtime_cases_work() {
    let cases = vec![
        RuntimeCase {
            name: "modulo and logical operators work",
            source: r#"
                let a = 17 % 5;
                let b = true && false;
                let c = true || false;
                let d = (10 > 5) && (3 < 7);
                let e = (10 < 5) || (3 > 7);
                let f = 100 % 7;
                a + f;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(4)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "null literal is supported",
            source: r#"
                let v = null;
                type(v) == "null";
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Bool(true)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "null values and type patterns are supported",
            source: r#"
                let some = 1 + 1;
                let none = null;
                let some2 = 40;

                let a = match none {
                    null => 1,
                    _ => 0,
                };
                let b = match some {
                    null => 0,
                    _ => 1,
                };
                let c = match some2 {
                    Some(Number) => 1,
                    _ => 0,
                };

                let t = type(null);
                if t == "null" {
                    (a + b + c) + some2;
                } else {
                    0;
                }
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(43)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "match type pattern is not shadowed by local name",
            source: r#"
                let String = 3;
                let b = match "" {
                    Some(String) => 2,
                    _ => 3,
                };
                b;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(2)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "null literal can be used as map key",
            source: r#"
                let m = { null: 42 };
                m[null];
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(42)],
            expected_locals: None,
        },
    ];
    run_runtime_cases(&cases);
}

#[test]
fn rustscript_language_parse_rejection_cases_work() {
    let cases = vec![
        ParseErrorCase {
            name: "legacy option aliases are rejected",
            source: r#"
                let some = Some(1);
                some;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["unknown function 'Some'"],
        },
        ParseErrorCase {
            name: "type name of val alias is rejected",
            source: r#"
                type_name_of_val(null);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_contains_all: &["unknown function 'type_name_of_val'"],
        },
    ];
    for case in &cases {
        expect_parse_error_case(case);
    }
}

#[test]
fn rustscript_non_strict_comparisons_and_integer_edge_literals_work() {
    let cases = vec![RuntimeCase {
        name: "non strict comparisons and integer edge literals",
        source: r#"
            let le = 1 <= 1;
            let ge = 2 >= 1;
            let hex = 0x2a;
            let min_dec = -9223372036854775808;
            let min_hex = -0x8000000000000000;
            if le && ge && hex == 42 && min_dec == min_hex {
                min_dec;
            } else {
                0;
            }
        "#,
        flavor: SourceFlavor::RustScript,
        expected_stack: vec![Value::Int(i64::MIN)],
        expected_locals: None,
    }];
    run_runtime_cases(&cases);
}

#[test]
fn rustscript_referenced_struct_schema_runtime_cases_work() {
    let cases = vec![
        rustscript_runtime_case(
            "referenced struct schema propagates through optional chain subobject",
            r#"
                struct Age { first: int }
                struct User { age: Age }
                let user: User = { age: { first: 41 } };
                let age = user?.age;
                age?.first;
            "#,
            vec![Value::Int(41)],
        ),
        rustscript_runtime_case(
            "referenced struct schema propagates through functions and closures",
            r#"
                struct Age { first: int }
                struct User { age: Age }

                fn first_age(user) {
                    user.age.first
                }

                let user_for_fn: User = { age: { first: 41 } };
                let user_for_capture: User = { age: { first: 41 } };
                let user_for_closure: User = { age: { first: 41 } };
                let read = || user_for_capture.age.first;
                let pick = |entry| entry.age.first;
                let via_fn = first_age(user_for_fn) + 1;
                let via_capture = read() + 2;
                let via_closure_param = pick(user_for_closure) + 3;
                via_fn;
                via_capture;
                via_closure_param;
            "#,
            vec![Value::Int(42), Value::Int(43), Value::Int(44)],
        ),
    ];

    run_runtime_cases(&cases);
}

#[test]
fn rustscript_referenced_struct_schema_compile_rejections_work() {
    let cases = vec![
        SourceErrorCase {
            name: "referenced struct schema rejects missing field access",
            source: r#"
                struct User { name: string }
                let user: User = { name: "Ada" };
                user.age;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::InvalidFieldAccess),
            expected_contains_all: &["field 'age' is not declared"],
        },
        SourceErrorCase {
            name: "referenced struct schema rejects wrong array index type",
            source: r#"
                struct User { colors: [int] }
                let user: User = { colors: [1, 2] };
                user.colors["first"];
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::InvalidFieldAccess),
            expected_contains_all: &["array access requires an int index"],
        },
        SourceErrorCase {
            name: "referenced struct schema rejects optional access to missing field",
            source: r#"
                struct User { name: string }
                let user: User = { name: "Ada" };
                user?.age;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::InvalidFieldAccess),
            expected_contains_all: &["field 'age' is not declared"],
        },
        SourceErrorCase {
            name: "referenced struct schema rejects invalid function param field",
            source: r#"
                struct Age { first: int }
                struct User { age: Age }

                fn first_age(user) {
                    user.age.agxe
                }

                let user: User = { age: { first: 41 } };
                first_age(user);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::InvalidFieldAccess),
            expected_contains_all: &["field 'agxe' is not declared"],
        },
        SourceErrorCase {
            name: "referenced struct schema rejects invalid closure capture field",
            source: r#"
                struct Age { first: int }
                struct User { age: Age }
                let user: User = { age: { first: 41 } };
                let read = || user.age.agxe;
                read();
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::InvalidFieldAccess),
            expected_contains_all: &["field 'agxe' is not declared"],
        },
        SourceErrorCase {
            name: "referenced struct schema rejects invalid closure param field",
            source: r#"
                struct Age { first: int }
                struct User { age: Age }
                let pick = |entry| entry.age.agxe;
                let user: User = { age: { first: 41 } };
                pick(user);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::InvalidFieldAccess),
            expected_contains_all: &["field 'agxe' is not declared"],
        },
    ];

    run_source_error_cases(&cases);
}

#[test]
fn rustscript_recursive_struct_schema_runtime_cases_work() {
    let cases = vec![rustscript_runtime_case(
        "self recursive struct schema propagates after optional chain assignment",
        r#"
            struct Node {
                value: int,
                next: Node,
            }

            let node: Node = { value: 1, next: { value: 41, next: {} }};
            let next = node?.next;
            next?.value;
        "#,
        vec![Value::Int(41)],
    )];

    run_runtime_cases(&cases);
}

#[test]
fn rustscript_recursive_struct_schema_compile_rejections_work() {
    let cases = vec![
        SourceErrorCase {
            name: "self recursive struct schema rejects invalid direct field after optional chain assignment",
            source: r#"
                struct Node {
                    value: int,
                    next: Node,
                }

                let node: Node = { value: 1, next: { value: 41, next: {} }};
                node.next.agxe;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::InvalidFieldAccess),
            expected_contains_all: &["field 'agxe' is not declared"],
        },
        SourceErrorCase {
            name: "self recursive struct schema rejects invalid optional field after optional chain assignment",
            source: r#"
                struct Node {
                    value: int,
                    next: Node,
                }

                let node: Node = { value: 1, next: { value: 41, next: {} }};
                let next = node?.next;
                next?.agxe;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::InvalidFieldAccess),
            expected_contains_all: &["field 'agxe' is not declared"],
        },
    ];

    run_source_error_cases(&cases);
}

#[test]
fn rustscript_optional_chain_requires_declared_schema_and_explicit_handling() {
    let runtime_cases = vec![rustscript_runtime_case(
        "declared schema optional chain unwrap_or keeps concrete type",
        r#"
            struct Stats { score: int }
            struct Profile { stats: Stats }

            let present: Profile = { stats: { score: 41 } };
            let missing: Profile? = null;

            let present_score = present?.stats?.score.unwrap_or(0);
            let missing_score = missing?.stats?.score.unwrap_or(0);
            present_score + missing_score + 1;
        "#,
        vec![Value::Int(42)],
    )];
    run_runtime_cases(&runtime_cases);

    let error_cases = vec![
        SourceErrorCase {
            name: "optional chain rejects undeclared rustscript schema",
            source: r#"
                let profile = { stats: { score: 41 } };
                profile?.stats?.score.unwrap_or(0);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::InvalidFieldAccess),
            expected_contains_all: &[
                "optional access requires a user-declared schema in RustScript",
            ],
        },
        SourceErrorCase {
            name: "optional chain result must be unwrapped before arithmetic",
            source: r#"
                struct Stats { score: int }
                struct Profile { stats: Stats }

                let profile: Profile = { stats: { score: 41 } };
                let score = profile?.stats?.score;
                score + 1;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::InvalidFieldAccess),
            expected_contains_all: &["optional value must be unwrapped before binary operation"],
        },
        SourceErrorCase {
            name: "optional chain capture in fn must be handled before arithmetic",
            source: r#"
                struct Stats { score: int }
                struct Profile { stats: Stats }

                let profile: Profile = { stats: { score: 41 } };
                let score = profile?.stats?.score;

                fn add() {
                    score + 1
                }

                add();
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::InvalidFieldAccess),
            expected_contains_all: &["optional value must be unwrapped before binary operation"],
        },
        SourceErrorCase {
            name: "Some and None match patterns require an optional value",
            source: r#"
                let value = 41;
                match value {
                    None => 0,
                    Some(found) => found + 1,
                    _ => 0,
                };
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::InvalidFieldAccess),
            expected_contains_all: &["Some(...) and None match patterns require an optional value"],
        },
    ];
    run_source_error_cases(&error_cases);
}

#[test]
fn rustscript_optional_chain_rejects_statistically_mismatched_declared_schemas() {
    let error_cases = vec![
        SourceErrorCase {
            name: "optional chain rejects wrong nested field type under declared schema",
            source: r#"
                struct Stats { score: int }
                struct Profile { stats: Stats }

                let profile: Profile = { stats: { score: "oops" } };
                profile?.stats?.score.unwrap_or(0);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::InvalidFieldAccess),
            expected_contains_all: &[
                "field 'stats.score' is declared as schema type 'int' but was assigned string",
            ],
        },
        SourceErrorCase {
            name: "optional chain rejects missing required nested field under declared schema",
            source: r#"
                struct Stats { score: int }
                struct Profile { stats: Stats }

                let profile: Profile = {};
                profile?.stats?.score.unwrap_or(0);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::InvalidFieldAccess),
            expected_contains_all: &[
                "field 'stats' is required by the declared schema but is missing",
            ],
        },
        SourceErrorCase {
            name: "optional chain rejects reassignment that violates declared schema",
            source: r#"
                struct Stats { score: int }
                struct Profile { stats: Stats }

                let mut profile: Profile = { stats: { score: 41 } };
                profile = { stats: { score: "oops" } };
                profile?.stats?.score.unwrap_or(0);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::InvalidFieldAccess),
            expected_contains_all: &[
                "field 'stats.score' is declared as schema type 'int' but was assigned string",
            ],
        },
        SourceErrorCase {
            name: "optional match binding still rejects wrong nested field type under declared schema",
            source: r#"
                struct Stats { score: int }
                struct Profile { stats: Stats }

                let profile: Profile = { stats: { score: "oops" } };
                match profile?.stats?.score {
                    None => 0,
                    Some(score) => score + 1,
                    _ => 0,
                };
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::InvalidFieldAccess),
            expected_contains_all: &[
                "field 'stats.score' is declared as schema type 'int' but was assigned string",
            ],
        },
    ];

    run_source_error_cases(&error_cases);
}

#[test]
fn rustscript_explicit_optional_type_annotations_work() {
    let runtime_cases = vec![
        rustscript_runtime_case(
            "optional local and return annotations preserve concrete inner typing",
            r#"
                fn maybe_score(text: string) -> int? {
                    let mut out: int? = null;
                    if text == "ok" {
                        out = 41;
                    }
                    out
                }

                maybe_score("ok").unwrap_or(0) + maybe_score("bad").unwrap_or(1);
            "#,
            vec![Value::Int(42)],
        ),
        rustscript_runtime_case(
            "typed callable locals enforce closure body schemas",
            r#"
                let mapper: fn(int) -> int = |value| value + 1;
                mapper(41);
            "#,
            vec![Value::Int(42)],
        ),
    ];
    run_runtime_cases(&runtime_cases);

    let error_cases = vec![
        SourceErrorCase {
            name: "non optional local rejects null assignment",
            source: r#"
                let value: int = null;
                value;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::InvalidFieldAccess),
            expected_contains_all: &[
                "local is declared as schema type 'int' but was assigned null",
            ],
        },
        SourceErrorCase {
            name: "non optional return rejects null result",
            source: r#"
                fn bad() -> int {
                    null
                }

                bad();
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::StrictTypingRequired),
            expected_contains_all: &[
                "function 'bad' is declared to return 'int' but produced null",
            ],
        },
        SourceErrorCase {
            name: "typed callable locals reject mismatched closure results",
            source: r#"
                let mapper: fn(int) -> int = |value| value == 41;
                mapper(41);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::CallableArgumentTypeMismatch),
            expected_contains_all: &["callable body result expects 'int'", "got bool"],
        },
        SourceErrorCase {
            name: "typed host callable parameters reject wrong closure arity",
            source: r#"
                fn stream(handler: fn(map) -> map) -> map;
                stream(|value, extra| value);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::CallableArgumentTypeMismatch),
            expected_contains_all: &[
                "argument 'handler'",
                "fn(map<unknown>) -> map<unknown>",
                "takes 2 parameters",
            ],
        },
        SourceErrorCase {
            name: "typed host callable parameters reject wrong closure parameter type",
            source: r#"
                fn stream(handler: fn(map) -> map) -> map;
                fn handle(value: int) -> map { { action: "continue" } }
                stream(handle);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::CallableArgumentTypeMismatch),
            expected_contains_all: &[
                "argument 'handler' type mismatch",
                "arg[0]",
                "map<unknown>",
                "int",
            ],
        },
        SourceErrorCase {
            name: "typed host callable parameters reject wrong closure return type",
            source: r#"
                fn stream(handler: fn(map) -> map) -> map;
                stream(|value| 1);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::CallableArgumentTypeMismatch),
            expected_contains_all: &["callable body result type mismatch", "map<unknown>", "int"],
        },
        SourceErrorCase {
            name: "json encode rejects bytes under strict rustscript typing",
            source: r#"
                use json;
                json::encode(b"abc");
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::CallableArgumentTypeMismatch),
            expected_contains_all: &["json::encode", "bytes"],
        },
    ];

    run_source_error_cases(&error_cases);
}

#[test]
fn rustscript_positive_min_magnitude_literal_is_rejected_with_hint() {
    expect_parse_error_contains_any_case(
        "positive min magnitude literal is rejected with hint",
        "0x8000000000000000;",
        SourceFlavor::RustScript,
        &["out of range", "i64::MIN"],
    );
}

#[test]
fn rustscript_generic_runtime_cases_work() {
    let cases = vec![
        rustscript_runtime_case(
            "generic passthrough functions support multiple concrete instantiations",
            r#"
                fn myfn<T>(v: T) {
                    let b = v;
                    b
                }

                let text = myfn::<string>("hello");
                let number = myfn::<int>(41);
                text;
                number;
            "#,
            vec![Value::string("hello"), Value::Int(41)],
        ),
        rustscript_runtime_case(
            "generic function returns preserve instantiated struct schemas",
            r#"
                struct Box<T> { value: T }

                fn myfn<T>(v: T) {
                    let b = v;
                    b
                }

                let input: Box<string> = { value: "hello" };
                let boxed = myfn::<Box<string>>(input);
                boxed.value;
            "#,
            vec![Value::string("hello")],
        ),
        rustscript_runtime_case(
            "host generic return schemas enable typed field access",
            r#"
                use json;
                struct Stats { score: int }
                struct Profile { stats: Stats }

                let profile = json::decode::<Profile>("{\"stats\":{\"score\":41}}");
                profile.stats.score + 1;
            "#,
            vec![Value::Int(42)],
        ),
    ];

    run_runtime_cases(&cases);
}

#[test]
fn rustscript_generic_parse_errors_are_reported() {
    let cases = vec![
        rustscript_parse_error_case(
            "generic functions currently require explicit type arguments",
            r#"
                fn myfn<T>(v: T) {
                    v
                }

                myfn("hello");
            "#,
            &["function 'myfn' expects 1 type arguments, got 0"],
        ),
        rustscript_parse_error_case(
            "non generic functions reject explicit type arguments",
            r#"
                fn plain(v) {
                    v
                }

                plain::<string>("hello");
            "#,
            &["function 'plain' does not accept explicit type arguments"],
        ),
        rustscript_parse_error_case(
            "host generic calls validate type argument arity",
            r#"
                use json;
                json::decode::<int, string>("{}");
            "#,
            &["function 'json::decode' expects 1 type arguments, got 2"],
        ),
        rustscript_parse_error_case(
            "generic struct schemas validate type argument arity",
            r#"
                struct Box<T> { value: T }
                let value: Box<string, int> = { value: "hello" };
                value;
            "#,
            &["struct schema 'Box' expects 1 type arguments, got 2"],
        ),
        rustscript_parse_error_case(
            "duplicate function type parameters are rejected",
            r#"
                fn bad<T, T>(value: T) {
                    value
                }
            "#,
            &["duplicate type parameter 'T' in function 'bad'"],
        ),
        rustscript_parse_error_case(
            "duplicate struct type parameters are rejected",
            r#"
                struct Bad<T, T> { value: T }
            "#,
            &["duplicate type parameter 'T' in struct 'Bad'"],
        ),
    ];

    for case in &cases {
        expect_parse_error_case(case);
    }
}

#[test]
fn rustscript_generic_schema_errors_are_reported() {
    let cases = vec![
        SourceErrorCase {
            name: "opaque generic parameters do not allow field access",
            source: r#"
                fn bad<T>(value: T) {
                    value.score
                }

                bad::<int>(1);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::InvalidFieldAccess),
            expected_contains_all: &["cannot access fields on unresolved generic parameter 'T'"],
        },
        SourceErrorCase {
            name: "generic struct instantiations enforce substituted field schemas",
            source: r#"
                struct Box<T> { value: T }

                let boxed: Box<int> = { value: "oops" };
                boxed.value;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::InvalidFieldAccess),
            expected_contains_all: &[
                "field 'value' is declared as schema type 'int' but was assigned string",
            ],
        },
    ];

    run_source_error_cases(&cases);
}

#[test]
fn rustscript_strict_stream_emit_accepts_any_payload() {
    // In strict RustScript, `stream::emit` is the one host function whose
    // `any` payload is accepted at compile time; the per-item event bound is
    // validated at runtime by the invocation stream. The exemption is tied to
    // the authoritative runtime builtin identity (see the compiler unit test
    // `stream_emit_any_payload_exemption_requires_authoritative_builtin_identity`),
    // so a same-name function registered through another catalog cannot
    // inherit it.
    compile_source(
        r#"
        use stream;
        pub fn run() -> int {
            stream::emit({"a": 1, "b": 2});
            stream::emit("text");
            42;
        }
        "#,
    )
    .expect("strict stream::emit with any payloads must compile");
}

#[test]
fn tail_expression_if_collects_annotated_literal_local() {
    // Port of the `letif_a.rss` provider repro: an annotated `let` declared
    // inside a block used by a tail-position expression-if must be collected
    // with the branch's refined state so strict slot validation sees a
    // concrete compile-time type.
    run_runtime_cases(&[rustscript_runtime_case(
        "annotated literal local in tail expression-if else branch",
        r#"
            fn pick(model: string) -> string {
                if model == "" => {
                    "empty"
                } else => {
                    let body_text: string = "literal";
                    body_text
                }
            }

            pick("x");
        "#,
        vec![Value::string("literal")],
    )]);
}

#[test]
fn tail_expression_if_collects_json_encode_local() {
    run_runtime_cases(&[rustscript_runtime_case(
        "annotated json::encode local in tail expression-if branch",
        r#"
            use json;

            fn pick(model: string) -> string {
                if model == "" => {
                    ""
                } else => {
                    let encoded: string = json::encode({ text: "literal" });
                    encoded
                }
            }

            pick("x");
        "#,
        vec![Value::string("{\"text\":\"literal\"}")],
    )]);
}

#[test]
fn tail_expression_if_collects_module_call_local() {
    // Port of the `tailif_root.rss` / `tailif_m2.rss` repro: a local bound to
    // a module call inside a tail expression-if branch must resolve to the
    // module function's declared return schema. The temp root is canonicalized
    // and panic-safe: it is removed on drop even when a later assertion
    // panics, so no cleanup call is needed on any path.
    let root = TempModuleRoot::new("a3_b2_tailif_module");

    let main_path = root.path().join("main.rss");
    std::fs::write(
        &main_path,
        r#"
            use self::m2 as adapter;
            adapter::call("other");
        "#,
    )
    .expect("main source should write");

    let options = CompileSourceFileOptions::new().with_module_override_source(
        "m2.rss",
        r#"
            pub fn call(request: string) -> string {
                if request == "hello" => {
                    "matched"
                } else => {
                    let transformed: string = inner(request);
                    transformed
                }
            }

            fn inner(value: string) -> string {
                value + "!"
            }
        "#,
    );

    let compiled = compile_source_file_with_options(&main_path, options)
        .expect("tail expression-if module-call local should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::string("other!")]);
}

#[test]
fn tail_expression_if_branch_local_does_not_leak_to_sibling_branch() {
    // A local declared inside one tail expression-if branch must not be
    // visible in the sibling branch: the then branch sees `body_text` as
    // unknown and the parser rejects the reference.
    let case = SourceErrorCase {
        name: "tail if branch local does not leak into sibling branch",
        source: r#"
            fn pick(model: string) -> string {
                if model == "" => {
                    body_text
                } else => {
                    let body_text: string = "literal";
                    body_text
                }
            }

            pick("x");
        "#,
        flavor: SourceFlavor::RustScript,
        expected_kind: SourceErrorKind::Parse,
        expected_contains_all: &["unknown local 'body_text'"],
    };
    expect_source_error_case(&case);
}

#[test]
fn tail_expression_if_branch_local_is_unavailable_after_branch() {
    // A local declared inside an expression-if branch stays branch-scoped:
    // using it after the branch is rejected as possibly-unavailable on the
    // other control-flow path.
    let case = SourceErrorCase {
        name: "tail if branch local is unavailable after the branch",
        source: r#"
            fn pick(model: string) -> string {
                if model == "" => {
                    "empty"
                } else => {
                    let body_text: string = "literal";
                    body_text
                };
                body_text
            }

            pick("x");
        "#,
        flavor: SourceFlavor::RustScript,
        expected_kind: SourceErrorKind::Parse,
        expected_contains_all: &["local 'body_text'", "may be unavailable"],
    };
    expect_source_error_case(&case);
}

#[test]
fn tail_expression_if_rejects_incompatible_branch_results() {
    // Incompatible tail branch results must still be rejected even though
    // branch collection is now state-refined.
    let case = SourceErrorCase {
        name: "tail if rejects incompatible branch result types",
        source: r#"
            fn pick(model: string) -> string {
                if model == "" => {
                    1
                } else => {
                    "literal"
                }
            }

            pick("x");
        "#,
        flavor: SourceFlavor::RustScript,
        expected_kind: SourceErrorKind::Compile(CompileErrorKind::IfElseBranchTypeMismatch),
        expected_contains_all: &["incompatible expression result", "int vs string"],
    };
    expect_source_error_case(&case);
}

#[test]
fn tail_expression_if_unknown_annotation_keeps_strict_diagnostic() {
    // A genuinely unknown declaration inside a tail expression-if branch must
    // keep the strict typing diagnostic: the branch-state refinement must not
    // turn `unknown` annotations into concrete types.
    let case = SourceErrorCase {
        name: "tail if unknown annotation keeps strict typing diagnostic",
        source: r#"
            fn pick(model: string) -> string {
                if model == "" => {
                    "empty"
                } else => {
                    let body_text: unknown = "literal";
                    body_text
                }
            }

            pick("x");
        "#,
        flavor: SourceFlavor::RustScript,
        expected_kind: SourceErrorKind::Parse,
        expected_contains_all: &[
            "concrete compile-time types",
            "'unknown' annotations are not allowed",
        ],
    };
    expect_source_error_case(&case);
}

#[test]
fn non_tail_expression_if_annotated_local_control() {
    // Control: an annotated local inside a non-tail expression-if branch.
    run_runtime_cases(&[rustscript_runtime_case(
        "annotated literal local in non-tail expression-if branch",
        r#"
            fn pick(model: string) -> string {
                let label: string = if model == "" => {
                    "empty"
                } else => {
                    let body_text: string = "literal";
                    body_text
                };
                label + "!"
            }

            pick("x");
        "#,
        vec![Value::string("literal!")],
    )]);
}

#[test]
fn tail_expression_if_unannotated_local_control() {
    // Control: an unannotated local in a tail expression-if branch executes
    // to the same value as the annotated form.
    run_runtime_cases(&[rustscript_runtime_case(
        "unannotated literal local in tail expression-if else branch",
        r#"
            fn pick(model: string) -> string {
                if model == "" => {
                    "empty"
                } else => {
                    let body_text = "literal";
                    body_text
                }
            }

            pick("x");
        "#,
        vec![Value::string("literal")],
    )]);
}

#[test]
fn tail_match_with_annotated_let_in_arm_branch() {
    // Match arm bodies parse as expression syntax (`{ ... }` in an arm is an
    // array literal, not a statement block), so the closest supported form of
    // "tail match with an annotated let" is an if-expression arm whose branch
    // declares the local. It must resolve and execute through the refined
    // branch states.
    run_runtime_cases(&[rustscript_runtime_case(
        "annotated literal local in tail match arm if-branch",
        r#"
            fn pick(model: string) -> string {
                match model {
                    "" => if model == "x" => { "a" } else => { let body_text: string = "literal"; body_text },
                    _ => "other"
                }
            }

            pick("");
        "#,
        vec![Value::string("literal")],
    )]);
}

#[test]
fn json_encode_accepts_string_key_runtime_map() {
    // A runtime map annotated as `map` has schema `map<unknown>`: key
    // legality cannot be proven statically, so the compile-time validator
    // must admit it and the runtime encoder's string-key check decides.
    let compiled = compile_source(
        r#"
        use json;
        let request: map = {
            "model": "test-model",
            "stream": false,
        };
        json::encode(request);
        "#,
    )
    .expect("string-key runtime maps must compile for json::encode");

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("json::encode should run");
    assert_eq!(status, VmStatus::Halted);
    let [Value::String(text)] = vm.stack() else {
        panic!("expected encoded json string, got {:?}", vm.stack());
    };
    let parsed: serde_json::Value =
        serde_json::from_str(text).expect("encoded text must be valid json");
    assert_eq!(
        parsed,
        serde_json::json!({ "model": "test-model", "stream": false })
    );
}

#[test]
fn json_encode_accepts_nested_runtime_maps_and_arrays() {
    // Provider-shaped payload: nested maps and arrays inside a runtime map.
    // The generated object key order is unspecified, so the assertion parses
    // the text and compares semantic JSON.
    let compiled = compile_source(
        r#"
        use json;
        let request: map = {
            "model": "test-model",
            "stream": false,
            "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "hello" }] }
            ],
            "tools": [
                { "type": "function", "function": { "name": "read_file", "parameters": { "type": "object" } } }
            ]
        };
        json::encode(request);
        "#,
    )
    .expect("nested runtime maps must compile for json::encode");

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("json::encode should run");
    assert_eq!(status, VmStatus::Halted);
    let [Value::String(text)] = vm.stack() else {
        panic!("expected encoded json string, got {:?}", vm.stack());
    };
    let parsed: serde_json::Value =
        serde_json::from_str(text).expect("encoded text must be valid json");
    assert_eq!(
        parsed,
        serde_json::json!({
            "model": "test-model",
            "stream": false,
            "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "hello" }] }
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "parameters": { "type": "object" }
                    }
                }
            ]
        })
    );
}

#[test]
fn json_encode_preserves_struct_support() {
    // Control: struct/object-shaped encoding must remain green while runtime
    // maps are admitted.
    let compiled = compile_source(
        r#"
        use json;
        struct Inner { name: string }
        struct Payload {
            answer: int,
            ok: bool,
            arr: [int],
            inner: Inner,
        }
        let payload = {
            answer: 42,
            ok: true,
            arr: [1, 2],
            inner: { name: "pd" },
        };
        json::encode(payload);
        "#,
    )
    .expect("struct-shaped values must keep compiling for json::encode");

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("json::encode should run");
    assert_eq!(status, VmStatus::Halted);
    let [Value::String(text)] = vm.stack() else {
        panic!("expected encoded json string, got {:?}", vm.stack());
    };
    let parsed: serde_json::Value =
        serde_json::from_str(text).expect("encoded text must be valid json");
    assert_eq!(
        parsed,
        serde_json::json!({
            "answer": 42,
            "ok": true,
            "arr": [1, 2],
            "inner": { "name": "pd" },
        })
    );
}

#[test]
fn json_encode_runtime_map_rejects_non_string_key() {
    // Non-string keys are not representable in `TypeSchema::Map`, so the
    // rejection must come from the runtime encoder, not the compiler.
    let compiled = compile_source(
        r#"
        use json;
        let payload = { 1: "one" };
        json::encode(payload);
        "#,
    )
    .expect("non-string-key maps must compile; runtime must reject them");

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let err = vm
        .run()
        .expect_err("json::encode must reject non-string map keys");
    match err {
        vm::VmError::HostError(message) => {
            assert!(
                message.contains("json_encode map keys must be strings"),
                "{message}"
            );
        }
        other => panic!("unexpected vm error: {other}"),
    }
}

#[test]
fn json_encode_runtime_map_rejects_nested_bytes() {
    let compiled = compile_source(
        r#"
        use json;
        let payload: map = { "data": b"abc" };
        json::encode(payload);
        "#,
    )
    .expect("runtime maps with bytes values must compile; runtime must reject them");

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let err = vm
        .run()
        .expect_err("json::encode must reject bytes values inside maps");
    match err {
        vm::VmError::HostError(message) => {
            assert!(
                message.contains("json_encode does not support bytes values"),
                "{message}"
            );
        }
        other => panic!("unexpected vm error: {other}"),
    }
}

#[test]
fn json_encode_runtime_map_rejects_nested_callable() {
    let compiled = compile_source(
        r#"
        use json;
        fn handler(value: int) -> int { value + 1 }
        let payload: map = { "handler": handler };
        json::encode(payload);
        "#,
    )
    .expect("runtime maps with callable values must compile; runtime must reject them");

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let err = vm
        .run()
        .expect_err("json::encode must reject callable values inside maps");
    match err {
        vm::VmError::HostError(message) => {
            assert!(
                message.contains("json_encode does not support callable values"),
                "{message}"
            );
        }
        other => panic!("unexpected vm error: {other}"),
    }
}

#[test]
fn json_encode_rejects_concrete_inner_map_of_bytes_at_compile_time() {
    // A map with a concrete `bytes` inner schema is provably non-encodable,
    // so the compile-time validator must recurse through the `map<bytes>`
    // arm and reject the program without ever running it. This is the
    // compile-time counterpart to `json_encode_runtime_map_rejects_nested_bytes`,
    // which uses an `Unknown` inner schema and defers to the runtime.
    match compile_source(
        r#"
        use json;
        let payload: map<bytes> = { "data": b"abc" };
        json::encode(payload);
        "#,
    ) {
        Err(err) => match err {
            vm::SourceError::Compile(vm::CompileError::CallableArgumentTypeMismatch {
                detail,
                ..
            }) => {
                assert!(
                    detail.contains("builtin 'json::encode' cannot encode this value"),
                    "{detail}"
                );
                // The recursion must reach the bytes check through the map arm
                // and report the original value path.
                assert!(detail.contains("value uses bytes"), "{detail}");
            }
            other => panic!("unexpected compiler error: {other}"),
        },
        Ok(_) => panic!("map<bytes> must be rejected at compile time"),
    }
}

#[test]
fn json_encode_rejects_nested_concrete_inner_maps_at_compile_time() {
    // Recursive validation must apply at every map nesting level: the outer
    // `map<map<bytes>>` arm recurses into the inner `map<bytes>` arm, which
    // recurses into the bytes check.
    match compile_source(
        r#"
        use json;
        let payload: map<map<bytes>> = { "outer": { "data": b"abc" } };
        json::encode(payload);
        "#,
    ) {
        Err(err) => match err {
            vm::SourceError::Compile(vm::CompileError::CallableArgumentTypeMismatch {
                detail,
                ..
            }) => {
                assert!(
                    detail.contains("builtin 'json::encode' cannot encode this value"),
                    "{detail}"
                );
                assert!(detail.contains("value uses bytes"), "{detail}");
            }
            other => panic!("unexpected compiler error: {other}"),
        },
        Ok(_) => panic!("nested concrete map inners must be rejected at compile time"),
    }
}

#[test]
fn json_encode_accepts_concrete_inner_map_of_encodable_values() {
    // Control: a map with a concrete encodable inner schema (`map<int>`)
    // must pass the recursive compile-time validation and encode at runtime,
    // proving the map arm does not blanket-reject concrete inners.
    let compiled = compile_source(
        r#"
        use json;
        let payload: map<int> = { "one": 1, "two": 2 };
        json::encode(payload);
        "#,
    )
    .expect("map<int> must compile for json::encode");

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("json::encode should run");
    assert_eq!(status, VmStatus::Halted);
    let [Value::String(text)] = vm.stack() else {
        panic!("expected encoded json string, got {:?}", vm.stack());
    };
    let parsed: serde_json::Value =
        serde_json::from_str(text).expect("encoded text must be valid json");
    assert_eq!(parsed, serde_json::json!({ "one": 1, "two": 2 }));
}

// ---------------------------------------------------------------------------
// Recursive schema guards for json::encode
// ---------------------------------------------------------------------------
//
// `validate_json_schema` walks the resolved schema of the encoded value. For
// a self- or mutually-recursive struct placed inside a concrete `map<A>`
// inner, the resolver terminates its own expansion by leaving a `Named`
// marker for the schema already being expanded, but the validator used to
// re-resolve at every level with a fresh seen set, so the marker
// re-expanded one level deeper on every descent and the compile-time
// validation recursed without bound: the compiler ground for minutes
// without terminating instead of overflowing quickly.
//
// The regression probes below therefore run the compile inside a
// subprocess. The child is spawned with a constrained stack (so a
// regression aborts in well under a second instead of grinding for
// minutes) and a hard deadline (so a non-terminating compiler can never
// hang the harness); either way the failure surfaces as a normal
// assertion failure in the parent instead of killing the whole test
// process.
//
// The positive probe is the deterministic regression: its literal is
// well-formed only because the declared-schema check admits partial
// objects at recursive re-entries (the innermost `{}` is an `A` whose
// required `b` is filled by nothing at runtime - structs are
// compile-time-typed maps, so the encoded value is exactly the literal
// map as written). The negative control keeps a `bytes` field in the
// cycle; `TypeSchema::Object` is a `HashMap`, so field order is
// randomized per process and the rejection path may be `value.tag` or
// `value.b.a.tag` - the assertion accepts either as long as the `tag`
// field is named.

/// Runs `child` inline when `probe_env` is set (subprocess mode). The parent
/// path returns immediately and `spawn_json_probe` drives the subprocess.
/// The child prints `sentinel` at probe entry *before* running the closure,
/// so the parent can distinguish "the right test ran but hung" (sentinel
/// present, deadline hit) from "a different test ran" (sentinel absent).
fn run_json_probe_child(probe_env: &str, sentinel: &str, child: impl FnOnce() -> bool) {
    if std::env::var_os(probe_env).is_some() {
        println!("{sentinel}");
        std::process::exit(if child() { 0 } else { 1 });
    }
}

/// Spawns this test binary with `--exact <test_name>` and `probe_env` set,
/// under a 512 KiB stack limit and a 60 s deadline. The child re-enters the
/// same test, prints `sentinel` via `run_json_probe_child`, runs its probe
/// closure, and exits 0/1. A stack overflow aborts the child with a
/// non-success status, and a non-terminating compiler is killed at the
/// deadline.
///
/// The parent does not trust the exit status alone: a mistyped filter makes
/// libtest exit 0 while running zero tests, which would silently void the
/// probe. The child's output is therefore captured and must show that
/// exactly one test was selected (`running 1 test`) *and* that the
/// test-specific `sentinel` was printed. The sentinel is unique per test,
/// so a filter that accidentally matches a *different* probe test still
/// fails: that test prints its own sentinel, not the demanded one. The
/// probe closure then exits the child with 0/1, so a successful status
/// plus the matched filter plus the sentinel is the reliable success
/// signal. On any failure the returned error includes the child's output
/// so the regression is diagnosable.
fn spawn_json_probe(probe_env: &str, test_name: &str, sentinel: &str) -> Result<(), String> {
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg("ulimit -s 512; exec \"$0\" --exact \"$1\" --nocapture")
        .arg(std::env::current_exe().expect("test binary path"))
        .arg(test_name)
        .env(probe_env, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("json probe subprocess should start");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let outcome = loop {
        match child
            .try_wait()
            .expect("json probe subprocess should be waitable")
        {
            Some(status) => break Ok(status),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break Err("child did not finish within 60 s (compiler did not terminate)");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(25)),
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    use std::io::Read;
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    let child_output = format!("--- child stdout ---\n{stdout}--- child stderr ---\n{stderr}");
    match outcome {
        Err(reason) => return Err(format!("{reason}\n{child_output}")),
        Ok(status) if !status.success() => {
            return Err(format!("child exited with {status}\n{child_output}"));
        }
        _ => {}
    }
    if !stdout.contains("running 1 test") {
        return Err(format!(
            "child did not select exactly one test (filter matched nothing?)\n{child_output}"
        ));
    }
    if !stdout.contains(sentinel) {
        return Err(format!(
            "child did not print the test-specific probe sentinel '{sentinel}' (a different test ran?)\n{child_output}"
        ));
    }
    Ok(())
}

#[test]
fn json_encode_accepts_mutually_recursive_structs_inside_concrete_map() {
    // A `map<A>` whose inner schema is a mutually recursive struct pair
    // (A -> B -> A) must compile and encode: the recursion is structural,
    // every runtime value is finite, and the cycle edge itself is
    // encodable. The innermost partial literal `{}` (an `A` missing its
    // required `b`) is admitted only because the declared-schema check
    // allows partial objects at recursive re-entries, so the runtime
    // value is exactly the map literal as written and the expected
    // encoding is `{"x":{"b":{"a":{"b":{"a":{}}}}}}`.
    const PROBE: &str = "RUSTSCRIPT_JSON_PROBE_ACCEPT_RECURSIVE_MAP";
    const SENTINEL: &str = "json-probe-entered:RUSTSCRIPT_JSON_PROBE_ACCEPT_RECURSIVE_MAP";
    run_json_probe_child(PROBE, SENTINEL, || {
        let compiled = match compile_source(
            r#"
            use json;
            struct A { b: B }
            struct B { a: A }
            let payload: map<A> = { "x": { b: { a: { b: { a: {} } } } } };
            json::encode(payload);
            "#,
        ) {
            Ok(compiled) => compiled,
            Err(err) => {
                eprintln!("mutually recursive map<A> must compile, got: {err}");
                return false;
            }
        };
        let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
        let status = match vm.run() {
            Ok(status) => status,
            Err(err) => {
                eprintln!("mutually recursive map<A> must run, got: {err}");
                return false;
            }
        };
        if status != VmStatus::Halted {
            eprintln!("mutually recursive map<A> must halt, got: {status:?}");
            return false;
        }
        let [Value::String(text)] = vm.stack() else {
            eprintln!("expected encoded json string, got {:?}", vm.stack());
            return false;
        };
        let parsed = match serde_json::from_str::<serde_json::Value>(text) {
            Ok(parsed) => parsed,
            Err(err) => {
                eprintln!("encoded text must be valid json: {err}");
                return false;
            }
        };
        let expected = serde_json::json!({ "x": { "b": { "a": { "b": { "a": {} } } } } });
        if parsed != expected {
            eprintln!("unexpected encoding: {parsed}");
            return false;
        }
        true
    });
    if let Err(reason) = spawn_json_probe(
        PROBE,
        "compiler_rustscript_tests::json_encode_accepts_mutually_recursive_structs_inside_concrete_map",
        SENTINEL,
    ) {
        panic!(
            "mutually recursive structs in map<A> must compile and encode (probe failed): {reason}"
        );
    }
}

#[test]
fn json_encode_rejects_unsupported_field_in_recursive_struct_inside_concrete_map() {
    // Negative control for the cycle contract: the cycle edge itself is
    // encodable and must not be rejected, but unsupported field types that
    // are reachable from the cycle (here `tag: bytes` as a sibling of the
    // recursive field `b`) must still fail at compile time. The literal
    // supplies `tag` at every level the declared-schema check inspects
    // strictly (the top-level `A` and the first `A` re-entry at `x.b.a`);
    // only the innermost `{}` at the cycle marker stays partial, exactly
    // like the positive probe. The `json::encode` rejection then always
    // names the top-level `tag` field (`value.tag`) - the cycle guard only
    // short-circuits the marker edge, never sibling fields.
    const PROBE: &str = "RUSTSCRIPT_JSON_PROBE_REJECT_RECURSIVE_MAP_BYTES";
    const SENTINEL: &str = "json-probe-entered:RUSTSCRIPT_JSON_PROBE_REJECT_RECURSIVE_MAP_BYTES";
    run_json_probe_child(PROBE, SENTINEL, || {
        match compile_source(
            r#"
            use json;
            struct A { b: B, tag: bytes }
            struct B { a: A }
            let payload: map<A> = { "x": { b: { a: { b: { a: {} }, tag: b"t" } }, tag: b"t" } };
            json::encode(payload);
            "#,
        ) {
            Err(vm::SourceError::Compile(vm::CompileError::CallableArgumentTypeMismatch {
                detail,
                ..
            })) => {
                if detail.contains("uses bytes") && detail.contains("tag") {
                    true
                } else {
                    eprintln!("unexpected rejection detail: {detail}");
                    false
                }
            }
            Err(err) => {
                eprintln!("recursive struct with bytes field must be rejected, got: {err}");
                false
            }
            Ok(_) => {
                eprintln!("recursive struct with bytes field must be rejected at compile time");
                false
            }
        }
    });
    if let Err(reason) = spawn_json_probe(
        PROBE,
        "compiler_rustscript_tests::json_encode_rejects_unsupported_field_in_recursive_struct_inside_concrete_map",
        SENTINEL,
    ) {
        panic!(
            "bytes reachable from a recursive map<A> must be rejected at compile time (probe failed): {reason}"
        );
    }
}

#[test]
fn json_probe_harness_rejects_child_that_runs_no_tests() {
    // The probe harness must not treat a child that matched no test as
    // success: a mistyped filter makes libtest exit 0 with "running 0
    // tests", which would silently void every probe's regression value.
    let ran = spawn_json_probe(
        "RUSTSCRIPT_JSON_PROBE_NO_MATCH",
        "compiler_rustscript_tests::json_probe_no_such_test_exists",
        "json-probe-entered:never-printed",
    );
    assert!(
        ran.is_err(),
        "probe with a non-matching test name must not be reported as success"
    );
}

#[test]
fn json_encode_rejects_unsupported_field_in_nested_generic_instantiation_map() {
    // `Node<Node<T>>` re-enters the recursion wrapped in a *different*
    // instantiation at every level. A cycle key built from the raw render
    // of the node grows one nesting per re-entry (`Node<int>`,
    // `Node<Node<int>>`, `Node<Node<Node<int>>>`, ...) and never repeats,
    // so the walk neither terminates nor reaches the unsupported `tag`
    // field. The key must collapse the wrapped re-entries to the one cycle
    // class and still reject the bytes reachable from the body.
    const PROBE: &str = "RUSTSCRIPT_JSON_PROBE_REJECT_NESTED_INSTANTIATION";
    const SENTINEL: &str = "json-probe-entered:RUSTSCRIPT_JSON_PROBE_REJECT_NESTED_INSTANTIATION";
    run_json_probe_child(PROBE, SENTINEL, || {
        match compile_source(
            r#"
            use json;
            struct Node<T> { child: Node<Node<T>>, tag: bytes }
            fn enc(m: map<Node<int>>) {
                json::encode(m);
            }
            "#,
        ) {
            Err(vm::SourceError::Compile(vm::CompileError::CallableArgumentTypeMismatch {
                detail,
                ..
            })) => {
                if detail.contains("uses bytes") && detail.contains("tag") {
                    true
                } else {
                    eprintln!("unexpected rejection detail: {detail}");
                    false
                }
            }
            Err(err) => {
                eprintln!("nested generic instantiation with bytes must be rejected, got: {err}");
                false
            }
            Ok(_) => {
                eprintln!(
                    "nested generic instantiation with bytes must be rejected at compile time"
                );
                false
            }
        }
    });
    if let Err(reason) = spawn_json_probe(
        PROBE,
        "compiler_rustscript_tests::json_encode_rejects_unsupported_field_in_nested_generic_instantiation_map",
        SENTINEL,
    ) {
        panic!(
            "bytes reachable through a nested generic instantiation must be rejected (probe failed): {reason}"
        );
    }
}

#[test]
fn json_encode_rejects_shadowed_generic_param_with_unsupported_field() {
    // The struct named `T` occupies the same name space as a generic
    // parameter named `T`: the raw render of `Node<T>` is ambiguous
    // between the struct instantiation `Node<struct T>` and a generic
    // instantiation `Node<param T>`. The resolved-identity cycle key must
    // keep the two readings distinct and still terminate the
    // named-wrapped recursion (`Node<Node<T>>` re-enters the same
    // collapsed identity), while the bytes reachable through `tagged: T`
    // (the struct - the parameter here is named `X`) are rejected at
    // compile time. Note that when a same-named parameter is actually in
    // scope, the parameter wins, so a struct name colliding with a live
    // parameter is unreachable inside that generic's body; this test
    // keeps the parameter under a different name to exercise the
    // name-space collision itself. This is a regression guard for the
    // current behavior, not a claim that the case previously failed.
    const PROBE: &str = "RUSTSCRIPT_JSON_PROBE_REJECT_SHADOWED_PARAM";
    const SENTINEL: &str = "json-probe-entered:RUSTSCRIPT_JSON_PROBE_REJECT_SHADOWED_PARAM";
    run_json_probe_child(PROBE, SENTINEL, || {
        match compile_source(
            r#"
            use json;
            struct T { tag: bytes }
            struct Node<X> { child: Node<Node<T>>, tagged: T }
            fn enc<U>(m: map<Node<U>>) {
                json::encode(m);
            }
            "#,
        ) {
            Err(vm::SourceError::Compile(vm::CompileError::CallableArgumentTypeMismatch {
                detail,
                ..
            })) => {
                if detail.contains("uses bytes") && detail.contains("tag") {
                    true
                } else {
                    eprintln!("unexpected rejection detail: {detail}");
                    false
                }
            }
            Err(err) => {
                eprintln!("shadowed generic param with bytes must be rejected, got: {err}");
                false
            }
            Ok(_) => {
                eprintln!("shadowed generic param with bytes must be rejected at compile time");
                false
            }
        }
    });
    if let Err(reason) = spawn_json_probe(
        PROBE,
        "compiler_rustscript_tests::json_encode_rejects_shadowed_generic_param_with_unsupported_field",
        SENTINEL,
    ) {
        panic!(
            "bytes reachable through a shadowed generic parameter must be rejected (probe failed): {reason}"
        );
    }
}

#[test]
fn json_probe_harness_rejects_child_without_matching_sentinel() {
    // The harness must demand the test-specific sentinel in addition to
    // `running 1 test`: any single test satisfies the latter, so a filter
    // typo that matches a *different* probe test would otherwise report
    // success while running the wrong closure. Spawn the acceptance probe
    // but demand a sentinel it never prints; the child still compiles and
    // runs fine, so the failure must come from the sentinel check.
    let ran = spawn_json_probe(
        "RUSTSCRIPT_JSON_PROBE_ACCEPT_RECURSIVE_MAP",
        "compiler_rustscript_tests::json_encode_accepts_mutually_recursive_structs_inside_concrete_map",
        "json-probe-entered:this-sentinel-is-never-printed",
    );
    assert!(
        ran.is_err(),
        "probe without its test-specific sentinel must not be reported as success"
    );
}

#[test]
fn json_encode_reports_unsupported_fields_in_deterministic_sorted_order() {
    // `TypeSchema::Object` is a HashMap, so raw iteration order is
    // per-process random. The compile-time `json::encode` walk must visit
    // object fields in sorted name order: with two unsupported fields the
    // rejection path must always name `a` first, never `z`, so error text
    // (and therefore probe assertions) are stable across processes and
    // runs instead of depending on the process hash seed.
    match compile_source(
        r#"
        use json;
        struct S { z: bytes, a: bytes }
        fn enc(m: map<S>) {
            json::encode(m);
        }
        "#,
    ) {
        Err(vm::SourceError::Compile(vm::CompileError::CallableArgumentTypeMismatch {
            detail,
            ..
        })) => {
            assert!(detail.contains("value.a uses bytes"), "{detail}");
            assert!(!detail.contains("value.z uses bytes"), "{detail}");
        }
        Err(err) => panic!("unexpected compile error: {err}"),
        Ok(_) => panic!("map<S> with two bytes fields must be rejected at compile time"),
    }
}

#[test]
fn json_encode_accepts_wrapped_recursion_in_concrete_map() {
    // Positive control for container-wrapped recursion. `Node<T>` wraps
    // the recursion in an array at every re-entry (`Node<int>`,
    // `Node<[int]>`, `Node<[[int]]>`, ...), so the resolved type
    // arguments grow one wrapping per level and no cycle key ever
    // repeats; only an explicit depth budget keeps the walk terminating.
    // The type has no unsupported fields, so it must compile and the
    // finite runtime value must encode. The optional base case
    // (`child: Node<[T]>?` with `child: null`) is what makes a finite
    // literal constructible: the declared-schema check only admits
    // partial objects at re-entries whose identity repeats, and wrapped
    // re-entries never repeat. An empty map exercises the non-optional
    // wrap without needing any value.
    const PROBE: &str = "RUSTSCRIPT_JSON_PROBE_ACCEPT_WRAPPED_RECURSION";
    const SENTINEL: &str = "json-probe-entered:RUSTSCRIPT_JSON_PROBE_ACCEPT_WRAPPED_RECURSION";
    run_json_probe_child(PROBE, SENTINEL, || {
        // Array wrap with an optional base case: a real value must encode.
        // Null map entries are dropped when the literal is built, so the
        // optional `child: null` disappears from the encoding and only
        // `data` survives - the point is that the wrapped recursion
        // compiles (the walk terminates on the budget) and the finite
        // value encodes.
        let compiled = match compile_source(
            r#"
            use json;
            struct Node<T> { child: Node<[T]>?, data: int }
            let payload: map<Node<int>> = { "x": { child: null, data: 1 } };
            json::encode(payload);
            "#,
        ) {
            Ok(compiled) => compiled,
            Err(err) => {
                eprintln!("wrapped array recursion must compile, got: {err}");
                return false;
            }
        };
        let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
        let status = match vm.run() {
            Ok(status) => status,
            Err(err) => {
                eprintln!("wrapped array recursion must run, got: {err}");
                return false;
            }
        };
        if status != VmStatus::Halted {
            eprintln!("wrapped array recursion must halt, got: {status:?}");
            return false;
        }
        let [Value::String(text)] = vm.stack() else {
            eprintln!("expected encoded json string, got {:?}", vm.stack());
            return false;
        };
        let parsed = match serde_json::from_str::<serde_json::Value>(text) {
            Ok(parsed) => parsed,
            Err(err) => {
                eprintln!("encoded text must be valid json: {err}");
                return false;
            }
        };
        let expected = serde_json::json!({ "x": { "data": 1 } });
        if parsed != expected {
            eprintln!("unexpected encoding: {parsed}");
            return false;
        }

        // Same wrap with no optional base and an empty map value: the
        // schema walk still has to terminate on the budget.
        let compiled = match compile_source(
            r#"
            use json;
            struct Node<T> { child: Node<[T]> }
            let payload: map<Node<int>> = {};
            json::encode(payload);
            "#,
        ) {
            Ok(compiled) => compiled,
            Err(err) => {
                eprintln!("non-optional wrapped recursion must compile, got: {err}");
                return false;
            }
        };
        let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
        let status = match vm.run() {
            Ok(status) => status,
            Err(err) => {
                eprintln!("non-optional wrapped recursion must run, got: {err}");
                return false;
            }
        };
        if status != VmStatus::Halted {
            eprintln!("non-optional wrapped recursion must halt, got: {status:?}");
            return false;
        }
        let [Value::String(text)] = vm.stack() else {
            eprintln!("expected encoded json string, got {:?}", vm.stack());
            return false;
        };
        let parsed = match serde_json::from_str::<serde_json::Value>(text) {
            Ok(parsed) => parsed,
            Err(err) => {
                eprintln!("encoded text must be valid json: {err}");
                return false;
            }
        };
        if parsed != serde_json::json!({}) {
            eprintln!("unexpected encoding: {parsed}");
            return false;
        }
        true
    });
    if let Err(reason) = spawn_json_probe(
        PROBE,
        "compiler_rustscript_tests::json_encode_accepts_wrapped_recursion_in_concrete_map",
        SENTINEL,
    ) {
        panic!(
            "container-wrapped recursion without unsupported fields must compile and encode (probe failed): {reason}"
        );
    }
}

#[test]
fn json_encode_rejects_wrapped_array_recursion_with_unsupported_sibling() {
    // Negative control for container-wrapped recursion: the cycle edge is
    // encodable and the depth budget must accept it, but the `tag: bytes`
    // sibling is reachable at the very first level and must still be
    // rejected at compile time. The budget must never mask the current
    // layer's explicitly unsupported fields.
    const PROBE: &str = "RUSTSCRIPT_JSON_PROBE_REJECT_WRAPPED_ARRAY_RECURSION";
    const SENTINEL: &str =
        "json-probe-entered:RUSTSCRIPT_JSON_PROBE_REJECT_WRAPPED_ARRAY_RECURSION";
    run_json_probe_child(PROBE, SENTINEL, || {
        match compile_source(
            r#"
            use json;
            struct Node<T> { child: Node<[T]>, tag: bytes }
            fn enc(m: map<Node<int>>) {
                json::encode(m);
            }
            "#,
        ) {
            Err(vm::SourceError::Compile(vm::CompileError::CallableArgumentTypeMismatch {
                detail,
                ..
            })) => {
                if detail.contains("uses bytes") && detail.contains("tag") {
                    true
                } else {
                    eprintln!("unexpected rejection detail: {detail}");
                    false
                }
            }
            Err(err) => {
                eprintln!("wrapped array recursion with bytes must be rejected, got: {err}");
                false
            }
            Ok(_) => {
                eprintln!("wrapped array recursion with bytes must be rejected at compile time");
                false
            }
        }
    });
    if let Err(reason) = spawn_json_probe(
        PROBE,
        "compiler_rustscript_tests::json_encode_rejects_wrapped_array_recursion_with_unsupported_sibling",
        SENTINEL,
    ) {
        panic!(
            "bytes reachable through array-wrapped recursion must be rejected at compile time (probe failed): {reason}"
        );
    }
}

#[test]
fn json_encode_rejects_wrapped_map_recursion_with_unsupported_sibling() {
    // Map-container variant of the wrapped-recursion negative control:
    // `Node<map<T>>` wraps the recursion in a map at every re-entry, so
    // the argument grows (`map<int>`, `map<map<int>>`, ...) and no cycle
    // key repeats. The walk must terminate on the depth budget and still
    // reject the `tag: bytes` sibling at the first level.
    const PROBE: &str = "RUSTSCRIPT_JSON_PROBE_REJECT_WRAPPED_MAP_RECURSION";
    const SENTINEL: &str = "json-probe-entered:RUSTSCRIPT_JSON_PROBE_REJECT_WRAPPED_MAP_RECURSION";
    run_json_probe_child(PROBE, SENTINEL, || {
        match compile_source(
            r#"
            use json;
            struct Node<T> { child: Node<map<T>>, tag: bytes }
            fn enc(m: map<Node<int>>) {
                json::encode(m);
            }
            "#,
        ) {
            Err(vm::SourceError::Compile(vm::CompileError::CallableArgumentTypeMismatch {
                detail,
                ..
            })) => {
                if detail.contains("uses bytes") && detail.contains("tag") {
                    true
                } else {
                    eprintln!("unexpected rejection detail: {detail}");
                    false
                }
            }
            Err(err) => {
                eprintln!("wrapped map recursion with bytes must be rejected, got: {err}");
                false
            }
            Ok(_) => {
                eprintln!("wrapped map recursion with bytes must be rejected at compile time");
                false
            }
        }
    });
    if let Err(reason) = spawn_json_probe(
        PROBE,
        "compiler_rustscript_tests::json_encode_rejects_wrapped_map_recursion_with_unsupported_sibling",
        SENTINEL,
    ) {
        panic!(
            "bytes reachable through map-wrapped recursion must be rejected at compile time (probe failed): {reason}"
        );
    }
}

/// Builds a pathological but non-recursive chain of `depth` distinct
/// struct declarations (`L0 { f: L1 }` -> `L1 { f: L2 }` -> ... ->
/// `L{depth-1}`), with `deepest` as the type of the single field of the
/// deepest struct. Every declaration name is unique, so the walk must
/// expand the chain in full: the resolution/validation budget only bounds
/// repeated re-entries of the *same* declaration identity (recursive
/// families like `Node<[T]>`), and distinct names must not consume any
/// shared budget.
fn distinct_named_chain_source(depth: usize, deepest: &str) -> String {
    let mut source = String::from("use json;\n");
    for index in 0..depth - 1 {
        source.push_str(&format!("struct L{index} {{ f: L{} }}\n", index + 1));
    }
    source.push_str(&format!("struct L{} {{ f: {deepest} }}\n", depth - 1));
    source.push_str("fn enc(m: map<L0>) {\n    json::encode(m);\n}\n");
    source
}

/// The exact `json::encode` rejection path for the deepest field of a
/// `depth`-layer distinct chain: `value` plus one `f` segment per layer.
fn deep_chain_path(depth: usize) -> String {
    format!("value.{}", "f.".repeat(depth - 1) + "f")
}

#[test]
fn json_encode_rejects_deep_distinct_named_chain_with_deepest_bytes() {
    // 40 distinct struct declarations chained by a single `f` field, with
    // `bytes` reachable only at the deepest level. The chain must expand
    // in full and the deepest `bytes` must be rejected at compile time
    // with the precise 40-segment path.
    let source = distinct_named_chain_source(40, "bytes");
    match compile_source(&source) {
        Err(vm::SourceError::Compile(vm::CompileError::CallableArgumentTypeMismatch {
            detail,
            ..
        })) => {
            let path = deep_chain_path(40);
            assert!(detail.contains(&format!("{path} uses bytes")), "{detail}");
        }
        Err(err) => {
            panic!("deep distinct chain with deepest bytes must be rejected, got: {err}");
        }
        Ok(_) => {
            panic!("deep distinct chain with deepest bytes must be rejected at compile time");
        }
    }
}

#[test]
fn json_encode_rejects_deep_distinct_named_chain_with_deepest_callable() {
    // Callable counterpart of the deep distinct-chain regression: the
    // deepest field is a `fn(int) -> int` callable, which `json::encode`
    // must reject at compile time with the precise 40-segment path.
    let source = distinct_named_chain_source(40, "fn(int) -> int");
    match compile_source(&source) {
        Err(vm::SourceError::Compile(vm::CompileError::CallableArgumentTypeMismatch {
            detail,
            ..
        })) => {
            let path = deep_chain_path(40);
            assert!(detail.contains(&format!("{path} is callable")), "{detail}");
        }
        Err(err) => {
            panic!("deep distinct chain with deepest callable must be rejected, got: {err}");
        }
        Ok(_) => {
            panic!("deep distinct chain with deepest callable must be rejected at compile time");
        }
    }
}

#[test]
fn json_encode_accepts_deep_distinct_named_chain_of_encodable_fields() {
    // Positive control for the same 40-declaration chain: with an
    // encodable leaf (`int`) the walk must expand the chain in full and
    // accept it, proving the re-entry budget never trips on distinct
    // declaration names.
    let source = distinct_named_chain_source(40, "int");
    compile_source(&source).expect("deep distinct chain of encodable fields must compile");
}

#[test]
fn json_encode_rejects_very_deep_distinct_named_chain_with_deepest_bytes() {
    // The regression this fix targets: a chain of 1100 distinct struct
    // declarations, well past the old global named-depth budget. The
    // global budget stopped the walk at 32 nested expansions and accepted
    // the chain as a structural recursion edge, so `json::encode`
    // compiled even though the deepest field is `bytes` - the compile-time
    // diagnostic was masked. The budget must only bound repeated
    // re-entries of the *same* declaration, so this chain expands in full
    // and the deepest `bytes` is rejected with the precise 1100-segment
    // path.
    let source = distinct_named_chain_source(1100, "bytes");
    match compile_source(&source) {
        Err(vm::SourceError::Compile(vm::CompileError::CallableArgumentTypeMismatch {
            detail,
            ..
        })) => {
            let path = deep_chain_path(1100);
            assert!(detail.contains(&format!("{path} uses bytes")), "{detail}");
        }
        Err(err) => {
            panic!("very deep distinct chain with deepest bytes must be rejected, got: {err}");
        }
        Ok(_) => {
            panic!("very deep distinct chain with deepest bytes must be rejected at compile time");
        }
    }
}

#[test]
fn json_encode_rejects_very_deep_distinct_named_chain_with_deepest_callable() {
    // Callable counterpart at the same depth: the deepest field is a
    // `fn(int) -> int` callable, which the global named-depth budget used
    // to mask exactly like the bytes variant.
    let source = distinct_named_chain_source(1100, "fn(int) -> int");
    match compile_source(&source) {
        Err(vm::SourceError::Compile(vm::CompileError::CallableArgumentTypeMismatch {
            detail,
            ..
        })) => {
            let path = deep_chain_path(1100);
            assert!(detail.contains(&format!("{path} is callable")), "{detail}");
        }
        Err(err) => {
            panic!("very deep distinct chain with deepest callable must be rejected, got: {err}");
        }
        Ok(_) => {
            panic!(
                "very deep distinct chain with deepest callable must be rejected at compile time"
            );
        }
    }
}
