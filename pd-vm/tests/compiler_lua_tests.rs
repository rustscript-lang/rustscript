#![cfg(feature = "runtime")]
mod common;
use common::*;

#[test]
fn lua_vm_require_namespace_host_calls_are_supported() {
    let source = r#"
        local vm = require("vm")
        vm.add_one(41)
    "#;
    let compiled =
        compile_source_with_flavor(source, SourceFlavor::Lua).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    vm.bind_function("add_one", Box::new(AddOne));

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn lua_vm_require_member_alias_host_calls_are_supported() {
    let source = r#"
        local inc = require("vm").add_one
        inc(41)
    "#;
    let compiled =
        compile_source_with_flavor(source, SourceFlavor::Lua).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    vm.bind_function("add_one", Box::new(AddOne));

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn compile_source_with_lua_flavor() {
    let source = include_str!("../examples/example.lua");

    let compiled =
        compile_source_with_flavor(source, SourceFlavor::Lua).expect("compile should succeed");
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
fn lua_assignment_updates_existing_local_without_new_slot() {
    let source = r#"
        local a = 1
        a = 2
        a
    "#;
    let compiled =
        compile_source_with_flavor(source, SourceFlavor::Lua).expect("compile should succeed");
    assert_eq!(compiled.locals, 1);

    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(2)]);
}

#[test]
fn lua_print_works_without_decl() {
    let source = r#"
        print(40 + 2)
    "#;
    let compiled =
        compile_source_with_flavor(source, SourceFlavor::Lua).expect("compile should succeed");
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
fn compile_source_file_with_lua_complex_fixture() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/example_complex.lua");
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
fn lua_function_literal_without_return_is_rejected() {
    let source = r#"
        local base = 7
        local add = function(value) value + base end
        print(add(5))
    "#;
    let err = match compile_source_with_flavor(source, SourceFlavor::Lua) {
        Ok(_) => panic!("lua closure without return should fail"),
        Err(err) => err,
    };

    match err {
        vm::SourceError::Parse(err) => assert!(err.message.contains("return <expr>")),
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn lua_require_namespace_enables_direct_host_calls() {
    let source = r#"
        local vm = require("vm")
        print(add_one(41))
    "#;
    let compiled =
        compile_source_with_flavor(source, SourceFlavor::Lua).expect("compile should succeed");
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
fn compile_source_file_lua_supports_namespace_and_named_require_imports() {
    let unique = format!(
        "vm_lua_namespace_import_test_{}_{}",
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

    let main_path = root.join("main.lua");
    std::fs::write(
        &main_path,
        r#"
        local string = require("./strings.rss")
        local is_empty = require("./strings.rss").is_empty
        print(string.non_empty("rss"))
        print(is_empty(""))
    "#,
    )
    .expect("lua source should write");

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
fn lua_undeclared_host_call_is_rejected() {
    let source = r#"
        add_one(41)
    "#;
    let err = match compile_source_with_flavor(source, SourceFlavor::Lua) {
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
