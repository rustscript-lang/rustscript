#![cfg(feature = "runtime")]
mod common;
use common::*;

#[test]
fn javascript_runtime_namespace_custom_host_calls_are_supported() {
    let unique = format!(
        "runtime_js_host_namespace_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp module root should be created");
    let path = root.join("main.js");
    std::fs::write(
        &path,
        r#"
        import * as runtime from "runtime";
        runtime.add_one(41);
    "#,
    )
    .expect("js source should write");

    let compiled = compile_source_file(&path).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.bind_function("runtime::add_one", Box::new(AddOne));

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn javascript_http_subnamespace_host_calls_are_supported() {
    let case = RuntimeCase {
        name: "http subnamespace host calls are supported",
        source: r#"
            import * as http from "http";
            http.request.get_header("x-client-id");
        "#,
        flavor: SourceFlavor::JavaScript,
        expected_stack: vec![Value::string("x-client-id")],
        expected_locals: None,
    };
    let bindings = [HostBindingCase {
        name: "http::request::get_header",
        factory: make_echo_string,
    }];
    run_runtime_case_with_bindings(&case, &bindings);
}

#[test]
fn javascript_runtime_namespace_host_calls_are_supported() {
    let case = RuntimeCase {
        name: "runtime namespace host calls are supported",
        source: r#"
            import * as runtime from "runtime";
            runtime.sleep(1);
        "#,
        flavor: SourceFlavor::JavaScript,
        expected_stack: vec![Value::Bool(true)],
        expected_locals: None,
    };
    let bindings = [HostBindingCase {
        name: "runtime::sleep",
        factory: make_runtime_sleep,
    }];
    run_runtime_case_with_bindings(&case, &bindings);
}

#[test]
fn javascript_runtime_cases_work() {
    let cases = vec![
        RuntimeCase {
            name: "builtin namespace calls work with import",
            source: r#"
                import * as json from "json";
                let text = json.encode("ok");
                text;
            "#,
            flavor: SourceFlavor::JavaScript,
            expected_stack: vec![Value::string("\"ok\"")],
            expected_locals: None,
        },
        RuntimeCase {
            name: "jit namespace builtins work with import",
            source: r#"
                import * as jit from "jit";
                const _set = jit.set_hot_loop_threshold(4);
                const cfg = jit.get_config();
                if (cfg.hot_loop_threshold == 4) {
                    1;
                } else {
                    0;
                }
            "#,
            flavor: SourceFlavor::JavaScript,
            expected_stack: vec![Value::Int(1)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "assignment updates existing local without new slot",
            source: r#"
                let a = 1;
                a = 2;
                a;
            "#,
            flavor: SourceFlavor::JavaScript,
            expected_stack: vec![Value::Int(2)],
            expected_locals: Some(1),
        },
        RuntimeCase {
            name: "float literal binding is supported",
            source: r#"
                let a=1.1;
                a;
            "#,
            flavor: SourceFlavor::JavaScript,
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
            flavor: SourceFlavor::JavaScript,
            expected_stack: vec![Value::string("A"), Value::string("B")],
            expected_locals: None,
        },
        RuntimeCase {
            name: "empty param arrow closure is supported",
            source: r#"
                let make = () => 42;
                make();
            "#,
            flavor: SourceFlavor::JavaScript,
            expected_stack: vec![Value::Int(42)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "empty param arrow closure captures outer value",
            source: r#"
                let x = 41;
                let make = () => x + 1;
                make();
            "#,
            flavor: SourceFlavor::JavaScript,
            expected_stack: vec![Value::Int(42)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "function return statement is lowered",
            source: r#"
                function inc(v) { return v + 1; }
                inc(41);
            "#,
            flavor: SourceFlavor::JavaScript,
            expected_stack: vec![Value::Int(42)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "object property named return is not rewritten",
            source: r#"
                const obj = { return: 42 };
                obj.return;
            "#,
            flavor: SourceFlavor::JavaScript,
            expected_stack: vec![Value::Int(42)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "allows omitted semicolons at line end",
            source: r#"
                let out = 40
                out = out + 1
                if (out < 50) {
                    out = out + 1
                }
                out
            "#,
            flavor: SourceFlavor::JavaScript,
            expected_stack: vec![Value::Int(42)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "modulo and logical operators work",
            source: r#"
                const a = 17 % 5;
                const b = true && false;
                const c = true || false;
                const d = (10 > 5) && (3 < 7);
                const e = (10 < 5) || (3 > 7);
                const f = 100 % 7;
                a + f;
            "#,
            flavor: SourceFlavor::JavaScript,
            expected_stack: vec![Value::Int(4)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "typeof operator is supported",
            source: r#"
                const value = null;
                typeof value == "null";
            "#,
            flavor: SourceFlavor::JavaScript,
            expected_stack: vec![Value::Bool(true)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "typeof property name is not rewritten as operator",
            source: r#"
                const obj = { typeof: 42 };
                obj.typeof;
            "#,
            flavor: SourceFlavor::JavaScript,
            expected_stack: vec![Value::Int(42)],
            expected_locals: None,
        },
    ];
    run_runtime_cases(&cases);
}

#[test]
fn javascript_parse_rejection_cases_work() {
    let cases = vec![
        ParseErrorCase {
            name: "builtin namespace calls require import",
            source: r#"
                json.encode("ok");
            "#,
            flavor: SourceFlavor::JavaScript,
            expected_contains_all: &["unknown local 'json'"],
        },
        ParseErrorCase {
            name: "builtin namespace calls reject path separator",
            source: r#"
                import * as json from "json";
                json::encode("ok");
            "#,
            flavor: SourceFlavor::JavaScript,
            expected_contains_all: &["namespace calls use '.' in this language"],
        },
        ParseErrorCase {
            name: "block body arrow closure is rejected",
            source: r#"
                let inc = (value) => { value + 1; };
                inc(41);
            "#,
            flavor: SourceFlavor::JavaScript,
            expected_contains_all: &["block bodies are not supported"],
        },
        ParseErrorCase {
            name: "undeclared host call is rejected",
            source: r#"
                add_one(41);
            "#,
            flavor: SourceFlavor::JavaScript,
            expected_contains_all: &["unknown function 'add_one'"],
        },
        ParseErrorCase {
            name: "direct builtin len call is rejected",
            source: r#"
                let value = "hello";
                len(value);
            "#,
            flavor: SourceFlavor::JavaScript,
            expected_contains_all: &["unknown function 'len'"],
        },
    ];
    for case in &cases {
        expect_parse_error_case(case);
    }
}

#[test]
fn compile_source_with_javascript_flavor() {
    let case = RuntimeCase {
        name: "compile source with javascript flavor",
        source: include_str!("../examples/example.js"),
        flavor: SourceFlavor::JavaScript,
        expected_stack: vec![Value::Int(6)],
        expected_locals: None,
    };
    let bindings = [
        HostBindingCase {
            name: "add_one",
            factory: make_add_one,
        },
        HostBindingCase {
            name: "print",
            factory: make_print_builtin,
        },
    ];
    run_runtime_case_with_bindings(&case, &bindings);
}

#[test]
fn javascript_allows_omitted_semicolons_with_multiline_calls() {
    let case = RuntimeCase {
        name: "allows omitted semicolons with multiline calls",
        source: r#"
            import * as runtime from "runtime"
            let value = runtime.add_one(
                41
            )
            value
        "#,
        flavor: SourceFlavor::JavaScript,
        expected_stack: vec![Value::Int(42)],
        expected_locals: None,
    };
    let bindings = [HostBindingCase {
        name: "runtime::add_one",
        factory: make_add_one,
    }];
    run_runtime_case_with_bindings(&case, &bindings);
}

#[test]
fn javascript_console_log_works_without_decl() {
    let case = RuntimeCase {
        name: "console log works without decl",
        source: r#"
            console.log(40 + 2);
        "#,
        flavor: SourceFlavor::JavaScript,
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
fn javascript_print_supports_multiple_arguments_without_decl() {
    let case = RuntimeCase {
        name: "print supports multiple arguments without decl",
        source: r#"
            print(40, 2);
        "#,
        flavor: SourceFlavor::JavaScript,
        expected_stack: vec![Value::string("40 2")],
        expected_locals: None,
    };
    let bindings = [HostBindingCase {
        name: "print",
        factory: make_print_builtin,
    }];
    run_runtime_case_with_bindings(&case, &bindings);
}

#[test]
fn javascript_print_alias_handles_mixed_call_arities() {
    let case = RuntimeCase {
        name: "print alias handles mixed call arities",
        source: r#"
            print(1);
            print(2, 3);
        "#,
        flavor: SourceFlavor::JavaScript,
        expected_stack: vec![Value::Int(1), Value::string("2 3")],
        expected_locals: None,
    };
    let bindings = [HostBindingCase {
        name: "print",
        factory: make_print_builtin,
    }];
    run_runtime_case_with_bindings(&case, &bindings);
}

#[test]
fn compile_source_file_with_javascript_complex_fixture() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/example_complex.js");
    let compiled = compile_source_file(&path).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);

    for func in &compiled.functions {
        match func.name.as_str() {
            "add_one" => {
                vm.register_function(Box::new(AddOne));
            }
            "print" => {
                vm.register_function(Box::new(PrintBuiltin));
            }
            "runtime::sleep" => {
                vm.register_function(Box::new(RuntimeSleep));
            }
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
fn compile_source_file_js_replay_break_line_uses_original_source_lines() {
    let unique = format!(
        "vm_js_replay_line_map_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp module root should be created");

    let module_path = root.join("module.rss");
    std::fs::write(&module_path, "pub fn add_one(x);\n").expect("module source should write");

    let main_path = root.join("main.js");
    let main_source = r#"import { add_one } from "./module.rss";
let value = add_one(41);
console.log(value);
"#;
    std::fs::write(&main_path, main_source).expect("js source should write");

    let compiled = compile_source_file(&main_path).expect("compile should succeed");
    let recording_program = compiled.program.clone();
    let mut vm = Vm::new(compiled.program);
    for func in &compiled.functions {
        match func.name.as_str() {
            "add_one" => {
                vm.register_function(Box::new(AddOne));
            }
            "print" => {
                vm.register_function(Box::new(PrintBuiltin));
            }
            "runtime::sleep" => {
                vm.register_function(Box::new(RuntimeSleep));
            }
            _ => panic!("unexpected function {}", func.name),
        };
    }

    let mut debugger = vm::Debugger::with_recording(recording_program);
    loop {
        match vm
            .run_with_debugger(&mut debugger)
            .expect("vm should run under debugger recording")
        {
            VmStatus::Halted => break,
            VmStatus::Yielded => continue,
            VmStatus::Waiting(_op_id) => vm
                .wait_for_host_op_blocking()
                .expect("vm should complete host operation"),
        }
    }

    let recording = debugger
        .take_recording()
        .expect("recording should be available");
    let mut replay = vm::VmRecordingReplayState::default();
    let _ = vm::run_recording_replay_command(&recording, &mut replay, "break line 2");
    let continue_response = vm::run_recording_replay_command(&recording, &mut replay, "continue");
    assert_eq!(
        continue_response.current_line,
        Some(2),
        "expected replay to stop at source line 2, got output: {}",
        continue_response.output
    );
    let where_response = vm::run_recording_replay_command(&recording, &mut replay, "where");
    assert!(
        where_response.output.contains("line 2"),
        "where output should report line 2, got: {}",
        where_response.output
    );

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_file(module_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_source_file_js_complex_replay_break_line_resolves_non_executable_lines() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/example_complex.js");
    let compiled = compile_source_file(&path).expect("compile should succeed");
    let recording_program = compiled.program.clone();
    let mut vm = Vm::new(compiled.program);
    for func in &compiled.functions {
        match func.name.as_str() {
            "print" => {
                vm.register_function(Box::new(PrintBuiltin));
            }
            "runtime::sleep" => {
                vm.register_function(Box::new(RuntimeSleep));
            }
            _ => panic!("unexpected function {}", func.name),
        };
    }

    let mut debugger = vm::Debugger::with_recording(recording_program);
    loop {
        match vm
            .run_with_debugger(&mut debugger)
            .expect("vm should run under debugger recording")
        {
            VmStatus::Halted => break,
            VmStatus::Yielded => continue,
            VmStatus::Waiting(_op_id) => vm
                .wait_for_host_op_blocking()
                .expect("vm should complete host operation"),
        }
    }

    let recording = debugger
        .take_recording()
        .expect("recording should be available");
    let mut replay = vm::VmRecordingReplayState::default();

    let set_response = vm::run_recording_replay_command(&recording, &mut replay, "break line 12");
    assert!(
        set_response.output.contains("line 13 (requested line 12)"),
        "non-executable line should resolve to next executable source line, got: {}",
        set_response.output
    );

    let continue_response = vm::run_recording_replay_command(&recording, &mut replay, "continue");
    assert_eq!(
        continue_response.current_line,
        Some(13),
        "expected replay to stop at resolved source line 13, got output: {}",
        continue_response.output
    );

    let where_response = vm::run_recording_replay_command(&recording, &mut replay, "where");
    assert!(
        where_response.output.contains("line 13"),
        "where output should report line 13, got: {}",
        where_response.output
    );
}

#[test]
fn javascript_module_declarations_are_ignored() {
    let case = RuntimeCase {
        name: "module declarations are ignored",
        source: r#"
            import {
                add_one
            } from "runtime";
            const { ignored } = require("runtime");
            console.log(add_one(41));
        "#,
        flavor: SourceFlavor::JavaScript,
        expected_stack: vec![Value::Int(42)],
        expected_locals: None,
    };
    let bindings = [
        HostBindingCase {
            name: "runtime::add_one",
            factory: make_add_one,
        },
        HostBindingCase {
            name: "print",
            factory: make_print_builtin,
        },
    ];
    run_runtime_case_with_bindings(&case, &bindings);
}

#[test]
fn compile_source_file_js_supports_namespace_and_named_alias_imports() {
    let unique = format!(
        "vm_js_namespace_import_test_{}_{}",
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

    let main_path = root.join("main.js");
    std::fs::write(
        &main_path,
        r#"
        import * as string from "./strings.rss";
        import { is_empty as is_empty } from "./strings.rss";

        console.log(string.non_empty("rss"));
        console.log(is_empty(""));
    "#,
    )
    .expect("js source should write");

    let compiled = compile_source_file(&main_path).expect("compile should succeed");
    assert_eq!(compiled.functions.len(), 1);
    assert_eq!(compiled.functions[0].name, "print");

    let mut vm = Vm::new(compiled.program);
    vm.bind_function("print", Box::new(PrintBuiltin));
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(true), Value::Bool(true)]);

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_file(module_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn javascript_non_strict_comparisons_and_integer_edge_literals_work() {
    let case = RuntimeCase {
        name: "non strict comparisons and integer edge literals",
        source: r#"
            const le = 1 <= 1;
            const ge = 2 >= 1;
            const hex = 0x2a;
            const minDec = -9223372036854775808;
            const minHex = -0x8000000000000000;
            if (le && ge && hex == 42 && minDec == minHex) {
                minDec;
            } else {
                0;
            }
        "#,
        flavor: SourceFlavor::JavaScript,
        expected_stack: vec![Value::Int(i64::MIN)],
        expected_locals: None,
    };
    run_runtime_case(&case);
}
