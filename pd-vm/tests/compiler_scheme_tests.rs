#![cfg(feature = "runtime")]
mod common;
use common::*;

#[test]
fn scheme_vm_prefixed_namespace_host_calls_are_supported() {
    let unique = format!(
        "vm_scheme_host_namespace_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp module root should be created");
    let path = root.join("main.scm");
    std::fs::write(
        &path,
        r#"
        (require (prefix-in vm. "vm"))
        (vm.add_one 41)
    "#,
    )
    .expect("scheme source should write");

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
fn compile_source_with_scheme_flavor() {
    let source = include_str!("../examples/example.scm");

    let compiled =
        compile_source_with_flavor(source, SourceFlavor::Scheme).expect("compile should succeed");
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
fn scheme_assignment_updates_existing_local_without_new_slot() {
    let source = r#"
        (define a 1)
        (set! a 2)
        a
    "#;
    let compiled =
        compile_source_with_flavor(source, SourceFlavor::Scheme).expect("compile should succeed");
    assert_eq!(compiled.locals, 1);

    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(2)]);
}

#[test]
fn scheme_do_loop_syntax_is_supported() {
    let source = r#"
        (do ((i 1 (+ i 1))
             (p 3 (* 3 p)))
            ((> i 4) p))
    "#;
    let compiled =
        compile_source_with_flavor(source, SourceFlavor::Scheme).expect("compile should succeed");

    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(243)]);
}

#[test]
fn compile_source_file_with_scheme_complex_fixture() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/example_complex.scm");
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
fn compile_source_file_scheme_supports_library_import_sets() {
    let unique = format!(
        "vm_scheme_library_import_test_{}_{}",
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

    let main_path = root.join("main.scm");
    std::fs::write(
        &main_path,
        r#"
        (import (prefix "./strings.rss" string:))
        (import (only "./strings.rss" is_empty))
        (print (string:non_empty "rss"))
        (print (is_empty ""))
    "#,
    )
    .expect("scheme source should write");

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
fn compile_source_file_scheme_supports_module_language_require_sets() {
    let unique = format!(
        "vm_scheme_require_import_test_{}_{}",
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

    let main_path = root.join("main.scm");
    std::fs::write(
        &main_path,
        r#"
        (require (prefix-in string: "./strings.rss"))
        (require (only-in "./strings.rss" is_empty))
        (print (string:non_empty "rss"))
        (print (is_empty ""))
    "#,
    )
    .expect("scheme source should write");

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
fn scheme_undeclared_host_call_is_rejected() {
    let source = r#"
        (add_one 41)
    "#;
    let err = match compile_source_with_flavor(source, SourceFlavor::Scheme) {
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
