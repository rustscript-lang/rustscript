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
