#![cfg(feature = "runtime")]
mod common;
use common::*;

#[test]
fn rustscript_vm_namespace_host_calls_are_supported() {
    let case = RuntimeCase {
        name: "vm namespace host calls are supported",
        source: r#"
            use vm;
            vm::add_one(41);
        "#,
        flavor: SourceFlavor::RustScript,
        expected_stack: vec![Value::Int(42)],
        expected_locals: None,
    };
    let bindings = [HostBindingCase {
        name: "add_one",
        factory: make_add_one,
    }];
    run_runtime_case_with_bindings(&case, &bindings);
}

#[test]
fn rustscript_vm_http_subnamespace_host_calls_are_supported() {
    let case = RuntimeCase {
        name: "vm http subnamespace host calls are supported",
        source: r#"
            use vm;
            vm::http::request::get_header("x-client-id");
        "#,
        flavor: SourceFlavor::RustScript,
        expected_stack: vec![Value::String("x-client-id".to_string())],
        expected_locals: None,
    };
    let bindings = [HostBindingCase {
        name: "http::request::get_header",
        factory: make_echo_string,
    }];
    run_runtime_case_with_bindings(&case, &bindings);
}

#[test]
fn rustscript_host_namespace_import_without_vm_prefix_is_supported() {
    let case = RuntimeCase {
        name: "host namespace import without vm prefix is supported",
        source: r#"
            use runtime;
            runtime::sleep(41);
        "#,
        flavor: SourceFlavor::RustScript,
        expected_stack: vec![Value::Int(42)],
        expected_locals: None,
    };
    let bindings = [HostBindingCase {
        name: "runtime::sleep",
        factory: make_add_one,
    }];
    run_runtime_case_with_bindings(&case, &bindings);
}

#[test]
fn rustscript_host_namespace_alias_import_is_supported() {
    let case = RuntimeCase {
        name: "host namespace alias import is supported",
        source: r#"
            use rate_limit as rl;
            rl::allow("client-1", 3, 60);
        "#,
        flavor: SourceFlavor::RustScript,
        expected_stack: vec![Value::Bool(true)],
        expected_locals: None,
    };
    let bindings = [HostBindingCase {
        name: "rate_limit::allow",
        factory: make_always_allow,
    }];
    run_runtime_case_with_bindings(&case, &bindings);
}

#[test]
fn rustscript_vm_named_host_imports_are_supported() {
    let case = RuntimeCase {
        name: "vm named host imports are supported",
        source: r#"
            use vm::{add_one as inc};
            inc(41);
        "#,
        flavor: SourceFlavor::RustScript,
        expected_stack: vec![Value::Int(42)],
        expected_locals: None,
    };
    let bindings = [HostBindingCase {
        name: "add_one",
        factory: make_add_one,
    }];
    run_runtime_case_with_bindings(&case, &bindings);
}

#[test]
fn rustscript_io_namespace_builtin_calls_are_supported() {
    let source = r#"
        use io;
        io::exists(".");
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);

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
            name: "re namespace supports optional inline flags across functions",
            source: r#"
                use re;
                let a = re::match("^foo$", "FoO", "i");
                let b = re::find("^foo", "FoO bar", "i");
                let c = re::replace("foo", "FoO bar", "x", "i");
                let d = re::split("x", "aXb", "i");
                let e = re::captures("^(foo)-([0-9]+)$", "FoO-42", "i");

                let mut score = 0;
                if a {
                    score = score + 1;
                }
                if b == "FoO" {
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
            name: "json encode decode builtins are supported",
            source: r#"
                use json;
                let payload = {
                    answer: 42,
                    ok: true,
                    arr: [1, 2],
                    inner: { name: "pd" },
                };
                let text = json::encode(payload);
                let decoded = json::decode(text);
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
    ];
    run_runtime_cases(&cases);
}

#[test]
fn rustscript_builtin_and_namespace_parse_rejection_cases_work() {
    let cases = vec![ParseErrorCase {
        name: "builtin namespace calls require use import",
        source: r#"
            json::encode("ok");
        "#,
        flavor: SourceFlavor::RustScript,
        expected_contains_all: &["import builtin namespaces first"],
    }];
    for case in &cases {
        expect_parse_error_case(case);
    }
}

#[test]
fn compile_source_file_preserves_jit_builtin_namespace_use_directive() {
    let unique = format!(
        "vm_rustscript_jit_builtin_namespace_test_{}_{}",
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
        use jit;
        let _set = jit::set_hot_loop_threshold(2);
        let out = jit::get_hot_loop_threshold();
        out;
    "#,
    )
    .expect("main source should write");

    let compiled = compile_source_file(&main_path).expect("compile should succeed");
    assert!(
        compiled
            .program
            .imports
            .iter()
            .all(|import| !import.name.starts_with("jit::")),
        "jit namespace calls should lower as builtins, not host imports"
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(2)]);

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_dir(root);
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
            expected_stack: vec![
                Value::String("A".to_string()),
                Value::String("B".to_string()),
            ],
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
fn rss_print_builtin_works_without_decl() {
    let case = RuntimeCase {
        name: "print builtin works without decl",
        source: r#"
            print(40 + 2);
        "#,
        flavor: SourceFlavor::RustScript,
        expected_stack: vec![Value::Int(42)],
        expected_locals: None,
    };
    let bindings = [HostBindingCase {
        name: "print",
        factory: make_print_builtin,
    }];
    run_runtime_case_with_bindings(&case, &bindings);
}

#[test]
fn rustscript_println_function_adds_newline() {
    let case = RuntimeCase {
        name: "println function adds newline",
        source: r#"
            println(40 + 2);
        "#,
        flavor: SourceFlavor::RustScript,
        expected_stack: vec![Value::String("42\n".to_string())],
        expected_locals: None,
    };
    let bindings = [HostBindingCase {
        name: "print",
        factory: make_print_builtin,
    }];
    run_runtime_case_with_bindings(&case, &bindings);
}

#[test]
fn rustscript_println_function_supports_basic_rust_style_formatting() {
    let case = RuntimeCase {
        name: "println function supports basic rust style formatting",
        source: r#"
            let foo = "hello";
            let bar = 42;
            println("{} {}!", foo, bar);
        "#,
        flavor: SourceFlavor::RustScript,
        expected_stack: vec![Value::String("hello 42!\n".to_string())],
        expected_locals: None,
    };
    let bindings = [HostBindingCase {
        name: "print",
        factory: make_print_builtin,
    }];
    run_runtime_case_with_bindings(&case, &bindings);
}

#[test]
fn rustscript_print_function_supports_rust_style_formatting() {
    let case = RuntimeCase {
        name: "print function supports rust style formatting",
        source: r#"
            print("hex={:#x} bin={:08b} sci={:.1e}", 42, 5, 1234.0);
        "#,
        flavor: SourceFlavor::RustScript,
        expected_stack: vec![Value::String("hex=0x2a bin=00000101 sci=1.2e3".to_string())],
        expected_locals: None,
    };
    let bindings = [HostBindingCase {
        name: "print",
        factory: make_print_builtin,
    }];
    run_runtime_case_with_bindings(&case, &bindings);
}

#[test]
fn rustscript_println_function_supports_rust_style_formatting() {
    let case = RuntimeCase {
        name: "println function supports rust style formatting",
        source: r#"
            println("{1} {0}", "left", "right");
        "#,
        flavor: SourceFlavor::RustScript,
        expected_stack: vec![Value::String("right left\n".to_string())],
        expected_locals: None,
    };
    let bindings = [HostBindingCase {
        name: "print",
        factory: make_print_builtin,
    }];
    run_runtime_case_with_bindings(&case, &bindings);
}

#[test]
fn rustscript_print_rejects_non_literal_format_string() {
    let case = ParseErrorCase {
        name: "print rejects non literal format string",
        source: r#"
            let fmt = "{}";
            print(fmt, 1);
        "#,
        flavor: SourceFlavor::RustScript,
        expected_contains_all: &[
            "print formatting requires a string literal as the first argument",
        ],
    };
    expect_parse_error_case(&case);
}

#[test]
fn compile_source_file_with_rustscript_complex_fixture() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/example_complex.rss");
    let compiled = compile_source_file(&path).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);

    for func in &compiled.functions {
        match func.name.as_str() {
            "print" => vm.register_function(Box::new(PrintBuiltin)),
            "add_one" => vm.register_function(Box::new(AddOne)),
            _ => panic!("unexpected function {}", func.name),
        };
    }

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
            expected_stack: vec![Value::String("xx".to_string())],
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
            expected_stack: vec![Value::String("xx".to_string())],
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
            expected_stack: vec![Value::String("xx".to_string())],
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
            expected_stack: vec![Value::String("xx".to_string())],
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
            expected_stack: vec![Value::String("xx".to_string())],
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
            expected_stack: vec![Value::String("xx".to_string())],
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
            expected_stack: vec![Value::String("a!".to_string())],
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
                fn add_one(value) {
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
fn rustscript_closure_captured_callable_invocation_is_rejected() {
    let cases = vec![
        SourceErrorCase {
            name: "captured function-valued local cannot be invoked from closure body",
            source: r#"
                fn add_one(value) {
                    value + 1;
                }
                let func = add_one;
                let apply = |value| func(value);
                apply(41);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::NonCallableLocal),
            expected_contains_all: &[],
        },
        SourceErrorCase {
            name: "captured closure-valued local cannot be invoked from closure body",
            source: r#"
                let inc = |x| x + 1;
                let apply = |value| inc(value);
                apply(41);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::NonCallableLocal),
            expected_contains_all: &[],
        },
    ];
    for case in &cases {
        expect_source_error_case(case);
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
            name: "bool and null fields are copy by default",
            source: r#"
                let p = { b: true, n: null };
                let b1 = p.b;
                let b2 = p.b;
                let n1 = p.n;
                let n2 = p.n;
                if b1 && b2 && n1 == null && n2 == null {
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
            expected_stack: vec![Value::String("xx".to_string())],
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
            expected_stack: vec![Value::String("22".to_string())],
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
            expected_stack: vec![Value::String("xx".to_string())],
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
            expected_stack: vec![Value::String("xx".to_string())],
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
            expected_stack: vec![Value::String("xx".to_string())],
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
            expected_stack: vec![Value::String("xx".to_string())],
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
            expected_stack: vec![Value::String("xx".to_string())],
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
            expected_stack: vec![Value::String("444666".to_string())],
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
            expected_stack: vec![Value::String("new".to_string())],
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
    assert!(
        compiled.locals >= 2,
        "expected at least two locals for source/binding pair"
    );
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::String("2".to_string())]);
    assert_eq!(vm.locals().first(), Some(&Value::Null));
    assert_eq!(vm.locals().get(1), Some(&Value::String("2".to_string())));
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

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::String("2".to_string())]);
    assert_eq!(vm.locals()[a_index as usize], Value::Null);
}

#[test]
fn rustscript_field_move_updates_runtime_container_state() {
    let source = r#"
        let mut p = { a: "x", b: "y" };
        let moved = p.a;
        moved;
    "#;
    let compiled = vm::compile_source_for_repl(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::String("x".to_string())]);

    let Some(Value::Map(entries)) = vm.locals().first() else {
        panic!("expected first local to be moved map container");
    };
    let mut saw_a_null = false;
    let mut saw_b_y = false;
    for (key, value) in entries {
        match (key, value) {
            (Value::String(name), Value::Null) if name == "a" => saw_a_null = true,
            (Value::String(name), Value::String(text)) if name == "b" && text == "y" => {
                saw_b_y = true
            }
            _ => {}
        }
    }
    assert!(
        saw_a_null,
        "expected moved field 'a' to be null in local container"
    );
    assert!(
        saw_b_y,
        "expected untouched field 'b' to remain present in local container"
    );
}

#[test]
fn rustscript_field_move_expr_statement_updates_runtime_container_state() {
    let source = r#"
        let mut p = { a: "x", b: "y" };
        p.a;
    "#;
    let compiled = vm::compile_source_for_repl(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::String("x".to_string())]);

    let Some(Value::Map(entries)) = vm.locals().first() else {
        panic!("expected first local to be moved map container");
    };
    let mut saw_a_null = false;
    let mut saw_b_y = false;
    for (key, value) in entries {
        match (key, value) {
            (Value::String(name), Value::Null) if name == "a" => saw_a_null = true,
            (Value::String(name), Value::String(text)) if name == "b" && text == "y" => {
                saw_b_y = true
            }
            _ => {}
        }
    }
    assert!(
        saw_a_null,
        "expected moved field 'a' to be null in local container"
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
        moved;
    "#;
    let compiled = vm::compile_source_for_repl(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::String("x".to_string())]);

    let Some(Value::Array(values)) = vm.locals().first() else {
        panic!("expected first local to be moved array container");
    };
    assert_eq!(values.len(), 2);
    assert_eq!(values[0], Value::Null);
    assert_eq!(values[1], Value::String("y".to_string()));
}

#[test]
fn rustscript_move_and_alias_parse_rejection_cases_work() {
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
            expected_contains_all: &["local 'value'", "moved"],
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
                for (let mut i = 0; i < 2; i = i + 1) {
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
            expected_stack: vec![Value::String("xx".to_string())],
            expected_locals: None,
        },
        RuntimeCase {
            name: "for loop mutating iterator local supports let mut initializer",
            source: r#"
                let mut total = 0;
                for (let mut i = 0; i < 3; i = i + 1) {
                    total = total + i;
                }
                total;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_stack: vec![Value::Int(3)],
            expected_locals: None,
        },
    ];
    run_runtime_cases(&cases);
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

    let mut vm = Vm::new(compiled.program);
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
            .any(|value| matches!(value, Value::String(text) if text == "stale")),
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

    let mut vm = Vm::new(compiled.program);
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
            .any(|value| matches!(value, Value::String(text) if text == "stale")),
        "stack should not retain pre-call placeholder values"
    );
    assert_eq!(vm.locals()[func_index as usize], Value::Null);
}

#[test]
fn inline_function_call_frame_slots_are_cleared_after_return() {
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
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack().last(), Some(&Value::Int(0)));
    assert!(
        vm.locals().iter().all(|value| matches!(value, Value::Null)),
        "expected all inline call-frame locals to be cleared after return, got {:?}",
        vm.locals()
    );
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
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack().last(), Some(&Value::Int(0)));
    assert!(
        vm.locals().iter().all(|value| matches!(value, Value::Null)),
        "expected closure capture and call-frame slots to clear after last use, got {:?}",
        vm.locals()
    );
}

#[test]
fn rustscript_callable_values_cannot_be_stored_in_arrays() {
    let case = SourceErrorCase {
        name: "callable values cannot be stored in arrays",
        source: r#"
            fn add_one(value) {
                value + 1;
            }
            let func = add_one;
            let values = [func];
            values.length;
        "#,
        flavor: SourceFlavor::RustScript,
        expected_kind: SourceErrorKind::Compile(CompileErrorKind::CallableUsedAsValue),
        expected_contains_all: &[],
    };
    expect_source_error_case(&case);
}

#[test]
fn rustscript_callable_values_cannot_be_returned_from_functions() {
    let case = SourceErrorCase {
        name: "callable values cannot be returned from functions",
        source: r#"
            fn add_one(value) {
                value + 1;
            }
            fn get_adder() {
                add_one;
            }

            let func = get_adder();
            func(41);
        "#,
        flavor: SourceFlavor::RustScript,
        expected_kind: SourceErrorKind::Compile(CompileErrorKind::CallableUsedAsValue),
        expected_contains_all: &[],
    };
    expect_source_error_case(&case);
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
            expected_contains_all: &["int/string/null literals, type patterns"],
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
        pub fn add_one(x);
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

    let compiled = compile_source_file(&main_path).expect("compile should succeed");
    assert_eq!(
        compiled.locals, 2,
        "module and root locals should be isolated"
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

    let mut vm = Vm::new(compiled.program);
    vm.bind_function("add_one", Box::new(AddOne));
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(2)]);

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

    let err = match compile_source_file(&main_path) {
        Ok(_) => panic!("legacy import syntax should be rejected for RustScript"),
        Err(err) => err,
    };
    assert!(
        matches!(
            err,
            vm::SourcePathError::InvalidImportSyntax { ref message, .. }
            if message.contains("uses 'use', not 'import'")
        ),
        "expected use-keyword guidance, got {err:?}"
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

    let compiled = compile_source_file(&main_path).expect("compile should succeed");
    assert!(
        compiled.functions.is_empty(),
        "module functions should be fully inlined for RustScript root"
    );

    let mut vm = Vm::new(compiled.program);
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

    let compiled = compile_source_file(&main_path).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
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

    let compiled = compile_source_file(&main_path).expect("compile should succeed");
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
fn compile_source_file_rustscript_host_namespace_alias_maps_to_vm_host_import() {
    let unique = format!(
        "vm_rustscript_runtime_alias_host_fallback_test_{}_{}",
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

    let compiled = compile_source_file(&main_path).expect("compile should succeed");
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
fn compile_source_file_module_override_path_redirects_import_spec() {
    let unique = format!(
        "vm_rustscript_module_override_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp module root should be created");

    let override_module_path = root.join("edge_io_async_override.rss");
    std::fs::write(
        &override_module_path,
        r#"
        pub fn request_body_read() {
            "override-body";
        }
    "#,
    )
    .expect("override module source should write");

    let main_path = root.join("main.rss");
    std::fs::write(
        &main_path,
        r#"
        use edge::io_async as edge_io;
        edge_io::request_body_read();
    "#,
    )
    .expect("main source should write");

    let options = CompileSourceFileOptions::new()
        .with_module_override_path("edge/io_async.rss", &override_module_path);
    let compiled =
        compile_source_file_with_options(&main_path, options).expect("compile should succeed");
    assert!(
        compiled.functions.is_empty(),
        "override module functions should be inlined into root program"
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::String("override-body".to_string())]);

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_file(override_module_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_source_file_rustscript_named_import_is_selective() {
    let unique = format!(
        "vm_rustscript_selective_import_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp module root should be created");

    let module_path = root.join("module.rss");
    std::fs::write(
        &module_path,
        r#"
        pub fn add_one(x) {
            x + 1;
        }
        pub fn add_two(x) {
            x + 2;
        }
    "#,
    )
    .expect("module source should write");

    let main_path = root.join("main.rss");
    std::fs::write(
        &main_path,
        r#"
        use module::{add_one};
        add_two(40);
    "#,
    )
    .expect("main source should write");

    let err = match compile_source_file(&main_path) {
        Ok(_) => panic!("selective import should not expose unlisted exports"),
        Err(err) => err,
    };
    assert!(
        matches!(
            err,
            vm::SourcePathError::Source(vm::SourceError::Parse(vm::ParseError { ref message, .. }))
            if message.contains("unknown function 'add_two'")
        ),
        "expected unknown function error, got {err:?}"
    );

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_file(module_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_source_file_rustscript_module_exports_only_pub_functions() {
    let unique = format!(
        "vm_rustscript_pub_export_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp module root should be created");

    let module_path = root.join("module.rss");
    std::fs::write(
        &module_path,
        r#"
        fn private_add(x) {
            x + 1;
        }
        pub fn public_add(x) {
            private_add(x);
        }
    "#,
    )
    .expect("module source should write");

    let ok_main_path = root.join("main_ok.rss");
    std::fs::write(
        &ok_main_path,
        r#"
        use module;
        public_add(41);
    "#,
    )
    .expect("ok main source should write");
    let compiled = compile_source_file(&ok_main_path).expect("compile should succeed");
    assert!(
        compiled.functions.is_empty(),
        "pure RustScript function module should not require host imports"
    );
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);

    let bad_main_path = root.join("main_bad.rss");
    std::fs::write(
        &bad_main_path,
        r#"
        use module;
        private_add(41);
    "#,
    )
    .expect("bad main source should write");
    let err = match compile_source_file(&bad_main_path) {
        Ok(_) => panic!("private import should fail"),
        Err(err) => err,
    };
    assert!(
        matches!(
            err,
            vm::SourcePathError::Source(vm::SourceError::Parse(vm::ParseError { ref message, .. }))
            if message.contains("unknown function 'private_add'")
        ),
        "expected unknown function error, got {err:?}"
    );

    let _ = std::fs::remove_file(bad_main_path);
    let _ = std::fs::remove_file(ok_main_path);
    let _ = std::fs::remove_file(module_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn rss_function_definition_is_inlined_without_host_imports() {
    let source = r#"
        fn eq(lhs, rhs) {
            lhs == rhs;
        }
        fn is_empty(value) {
            eq(value, "");
        }
        pub fn non_empty(value) {
            eq(is_empty(value), false);
        }
        non_empty("x");
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    assert!(
        compiled.functions.is_empty(),
        "rss-defined functions should not be emitted as host imports"
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(true)]);
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
