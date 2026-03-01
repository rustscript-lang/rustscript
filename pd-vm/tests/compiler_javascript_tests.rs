#![cfg(feature = "runtime")]
mod common;
use common::*;

#[test]
fn javascript_vm_namespace_host_calls_are_supported() {
    let unique = format!(
        "vm_js_host_namespace_test_{}_{}",
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
        import * as vm from "vm";
        vm.add_one(41);
    "#,
    )
    .expect("js source should write");

    let compiled = compile_source_file(&path).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    vm.bind_function("add_one", Box::new(AddOne));

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn javascript_vm_http_subnamespace_host_calls_are_supported() {
    let source = r#"
        import * as vm from "vm";
        vm.http.request.get_header("x-client-id");
    "#;
    let compiled = compile_source_with_flavor(source, SourceFlavor::JavaScript)
        .expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    vm.bind_function("http::request::get_header", Box::new(EchoString));

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::String("x-client-id".to_string())]);
}

#[test]
fn compile_source_with_javascript_flavor() {
    let source = include_str!("../examples/example.js");

    let compiled = compile_source_with_flavor(source, SourceFlavor::JavaScript)
        .expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    for func in &compiled.functions {
        match func.name.as_str() {
            "add_one" => vm.register_function(Box::new(AddOne)),
            "print" => vm.register_function(Box::new(PrintBuiltin)),
            _ => panic!("unexpected function {}", func.name),
        };
    }

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(6)]);
}

#[test]
fn javascript_assignment_updates_existing_local_without_new_slot() {
    let source = r#"
        let a = 1;
        a = 2;
        a;
    "#;
    let compiled = compile_source_with_flavor(source, SourceFlavor::JavaScript)
        .expect("compile should succeed");
    assert_eq!(compiled.locals, 1);

    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(2)]);
}

#[test]
fn javascript_float_literal_binding_is_supported() {
    let source = r#"
        let a=1.1;
        a;
    "#;
    let compiled = compile_source_with_flavor(source, SourceFlavor::JavaScript)
        .expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Float(1.1)]);
}

#[test]
fn javascript_empty_param_arrow_closure_is_supported() {
    let source = r#"
        let make = () => 42;
        make();
    "#;
    let compiled = compile_source_with_flavor(source, SourceFlavor::JavaScript)
        .expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn javascript_function_return_statement_is_lowered() {
    let source = r#"
        function inc(v) { return v + 1; }
        inc(41);
    "#;
    let compiled = compile_source_with_flavor(source, SourceFlavor::JavaScript)
        .expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn javascript_object_property_named_return_is_not_rewritten() {
    let source = r#"
        const obj = { return: 42 };
        obj.return;
    "#;
    let compiled = compile_source_with_flavor(source, SourceFlavor::JavaScript)
        .expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn javascript_block_body_arrow_closure_is_rejected() {
    let source = r#"
        let inc = (value) => { value + 1; };
        inc(41);
    "#;
    let err = match compile_source_with_flavor(source, SourceFlavor::JavaScript) {
        Ok(_) => panic!("block-body arrow closure should fail in this subset"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse) => {
            assert!(parse.message.contains("block bodies are not supported"));
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn javascript_allows_omitted_semicolons_at_line_end() {
    let source = r#"
        let out = 40
        out = out + 1
        if (out < 50) {
            out = out + 1
        }
        out
    "#;
    let compiled = compile_source_with_flavor(source, SourceFlavor::JavaScript)
        .expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn javascript_allows_omitted_semicolons_with_multiline_calls() {
    let source = r#"
        import * as vm from "vm"
        let value = vm.add_one(
            41
        )
        value
    "#;

    let compiled = compile_source_with_flavor(source, SourceFlavor::JavaScript)
        .expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    vm.bind_function("add_one", Box::new(AddOne));

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn javascript_console_log_works_without_decl() {
    let source = r#"
        console.log(40 + 2);
    "#;
    let compiled = compile_source_with_flavor(source, SourceFlavor::JavaScript)
        .expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    for func in &compiled.functions {
        match func.name.as_str() {
            "print" => vm.register_function(Box::new(PrintBuiltin)),
            _ => panic!("unexpected function {}", func.name),
        };
    }

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn compile_source_file_with_javascript_complex_fixture() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/example_complex.js");
    let compiled = compile_source_file(&path).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    for func in &compiled.functions {
        match func.name.as_str() {
            "print" => vm.register_function(Box::new(PrintBuiltin)),
            "add_one" => vm.register_function(Box::new(AddOne)),
            _ => panic!("unexpected function {}", func.name),
        };
    }

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(12)]);
}

#[test]
fn javascript_module_declarations_are_ignored() {
    let source = r#"
        import {
            add_one
        } from "vm";
        const { ignored } = require("vm");
        console.log(add_one(41));
    "#;
    let compiled = compile_source_with_flavor(source, SourceFlavor::JavaScript)
        .expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    for func in &compiled.functions {
        match func.name.as_str() {
            "add_one" => vm.register_function(Box::new(AddOne)),
            "print" => vm.register_function(Box::new(PrintBuiltin)),
            _ => panic!("unexpected function {}", func.name),
        };
    }

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
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

    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    vm.bind_function("print", Box::new(PrintBuiltin));
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(true), Value::Bool(true)]);

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_file(module_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn javascript_undeclared_host_call_is_rejected() {
    let source = r#"
        add_one(41);
    "#;
    let err = match compile_source_with_flavor(source, SourceFlavor::JavaScript) {
        Ok(_) => panic!("undeclared host call should fail"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse) => {
            assert!(parse.message.contains("unknown function 'add_one'"));
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn javascript_modulo_and_logical_operators_work() {
    let source = r#"
        const a = 17 % 5;
        const b = true && false;
        const c = true || false;
        const d = (10 > 5) && (3 < 7);
        const e = (10 < 5) || (3 > 7);
        const f = 100 % 7;
        a + f;
    "#;

    let compiled = compile_source_with_flavor(source, SourceFlavor::JavaScript)
        .expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4)]);
}

#[test]
fn javascript_typeof_operator_is_supported() {
    let source = r#"
        const value = null;
        typeof value == "null";
    "#;

    let compiled = compile_source_with_flavor(source, SourceFlavor::JavaScript)
        .expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(true)]);
}

#[test]
fn javascript_typeof_property_name_is_not_rewritten_as_operator() {
    let source = r#"
        const obj = { typeof: 42 };
        obj.typeof;
    "#;

    let compiled = compile_source_with_flavor(source, SourceFlavor::JavaScript)
        .expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn javascript_direct_builtin_len_call_is_rejected() {
    let source = r#"
        let value = "hello";
        len(value);
    "#;

    let err = match compile_source_with_flavor(source, SourceFlavor::JavaScript) {
        Ok(_) => panic!("direct builtin len call should be rejected in JavaScript frontend"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse) => {
            assert!(parse.message.contains("not exposed in JavaScript frontend"));
        }
        other => panic!("unexpected error: {other}"),
    }
}
