#![cfg(feature = "runtime")]
mod common;
use common::*;

#[test]
fn rustscript_vm_namespace_host_calls_are_supported() {
    let source = r#"
        use vm;
        vm::add_one(41);
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    vm.bind_function("add_one", Box::new(AddOne));

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn rustscript_vm_named_host_imports_are_supported() {
    let source = r#"
        use vm::{add_one as inc};
        inc(41);
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    vm.bind_function("add_one", Box::new(AddOne));

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn rustscript_io_namespace_builtin_calls_are_supported() {
    let source = r#"
        io::exists(".");
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(true)]);
}

#[test]
fn rustscript_array_primitives_are_supported_without_namespace() {
    let source = r#"
        let values = [];
        values[len(values)] = 7;
        values[0] + len(values);
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(8)]);
}

#[test]
fn rustscript_bracket_slice_syntax_is_supported() {
    let source = r#"
        let text = "abcdef";
        let end = -2;
        let a = text[1:4];
        let b = text[:3];
        let c = text[2:];
        let d = text[:-1];
        let e = text[1:end];

        let arr = [1, 2, 3, 4, 5];
        let f = arr[1:4];
        let g = arr[:2];
        let h = arr[3:];
        let i = arr[:-2];
        len(a) + len(b) + len(c) + len(d) + len(e) + len(f) + len(g) + len(h) + len(i);
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(28)]);
}

#[test]
fn rss_print_macro_works_without_decl() {
    let source = r#"
        print!(40 + 2);
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
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
fn compile_source_file_with_rustscript_complex_fixture() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/example_complex.rss");
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
fn closure_captures_outer_value_at_definition_time() {
    let source = r#"
        let base = 7;
        let add = |value| value + base;
        base = 8;
        print!(add(5));
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    for func in &compiled.functions {
        match func.name.as_str() {
            "print" => vm.register_function(Box::new(PrintBuiltin)),
            _ => panic!("unexpected function {}", func.name),
        };
    }

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(12)]);
}

#[test]
fn rustscript_if_expression_assignment_syntax_is_supported() {
    let source = r#"
        let x = if 2 > 1 => { 42 } else => { 0 };
        x;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn rustscript_if_expression_branch_blocks_support_multiline_statements() {
    let source = r#"
        let base = 40;
        let out = if true => {
            let bump = base + 2;
            bump
        } else => {
            let fallback = base - 1;
            fallback
        };
        out;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn rustscript_if_expression_assignment_executes_else_branch() {
    let source = r#"
        let marker = 0;
        let out = if false => {
            marker = 1;
            10
        } else => {
            marker = 2;
            20
        };
        marker + out;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(22)]);
}

#[test]
fn rustscript_if_expression_supports_else_if_chains() {
    let source = r#"
        let key = 2;
        let out = if key == 1 => { 10 } else if key == 2 => { 20 } else => { 0 };
        out;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(20)]);
}

#[test]
fn rustscript_if_expression_requires_else_branch() {
    let source = r#"
        let x = if true => { 1 };
        x;
    "#;

    let err = match compile_source(source) {
        Ok(_) => panic!("if expression without else should be rejected"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse) => {
            assert!(
                parse.message.contains("requires an else branch"),
                "unexpected parse error: {parse:?}"
            );
        }
        other => panic!("expected parse error, got {other:?}"),
    }
}

#[test]
fn rustscript_match_expression_supports_int_and_wildcard_patterns() {
    let source = r#"
        let value = 2;
        let out = match value {
            1 => 10,
            2 => 20,
            _ => 0,
        };
        out;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(20)]);
}

#[test]
fn rustscript_match_expression_supports_string_patterns() {
    let source = r#"
        let key = "beta";
        let out = match key {
            "alpha" => 1,
            "beta" => 2,
            _ => 0,
        };
        out;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(2)]);
}

#[test]
fn rustscript_match_expression_rejects_unsupported_patterns() {
    let source = r#"
        let value = 1;
        match value {
            true => 10,
            _ => 0,
        };
    "#;

    let err = match compile_source(source) {
        Ok(_) => panic!("bool patterns should be rejected"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse) => {
            assert!(
                parse.message.contains("only int/string literals and '_'"),
                "unexpected parse error: {parse:?}"
            );
        }
        other => panic!("expected parse error, got {other:?}"),
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

    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
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

    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(true), Value::Bool(true)]);

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_file(module_path);
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
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
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

    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(true)]);
}
