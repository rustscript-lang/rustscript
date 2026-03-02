#![cfg(feature = "runtime")]
mod common;
use common::*;

#[test]
fn compiler_emits_expression() {
    let expr = Expr::Mul(
        Box::new(Expr::Add(Box::new(Expr::Int(2)), Box::new(Expr::Int(3)))),
        Box::new(Expr::Int(4)),
    );
    let program = Compiler::new()
        .compile_program(&[Stmt::Expr { expr, line: 1 }])
        .expect("compiler should emit program");

    let mut vm = Vm::new(program);
    let status = vm.run().expect("vm should run");

    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(20)]);
}

#[test]
fn compile_source_program() {
    let source = r#"
        let x = 2 + 3;
        let y = x * 4;
        if y > 10 {
            y;
        } else {
            0;
        }
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    let status = vm.run().expect("vm should run");

    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(20)]);
}

#[test]
fn assignment_updates_existing_local_without_new_slot() {
    let source = r#"
        let a = 1;
        a = 2;
        a;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    assert_eq!(compiled.locals, 1);
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    let status = vm.run().expect("vm should run");

    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(2)]);
}
#[test]
fn compile_source_with_functions() {
    let source = include_str!("../examples/example.rss");

    let compiled = compile_source(source).expect("compile should succeed");
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
fn compile_source_resolves_imports_by_name_not_registration_order() {
    let source = include_str!("../examples/example.rss");
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    vm.bind_function("print", Box::new(PrintBuiltin));
    vm.bind_function("add_one", Box::new(AddOne));

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(6)]);
}

#[test]
fn run_fails_when_import_is_unbound() {
    let source = r#"
        fn add_one(x);
        add_one(41);
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    vm.bind_function("print", Box::new(PrintBuiltin));

    let err = vm.run().expect_err("missing import should fail");
    assert!(matches!(err, vm::VmError::UnboundImport(name) if name == "add_one"));
}

#[test]
fn host_function_registry_caches_import_plan_across_vms() {
    let source = include_str!("../examples/example.rss");
    let compiled = compile_source(source).expect("compile should succeed");

    let mut registry = HostFunctionRegistry::new();
    registry.register("print", 1, || Box::new(PrintBuiltin));
    registry.register("add_one", 1, || Box::new(AddOne));

    let mut vm1 = Vm::with_locals(compiled.program.clone(), compiled.locals);
    registry
        .bind_vm_cached(&mut vm1)
        .expect("cached host binding should succeed");
    let status1 = vm1.run().expect("vm should run");
    assert_eq!(status1, VmStatus::Halted);
    assert_eq!(vm1.stack(), &[Value::Int(6)]);

    let mut vm2 = Vm::with_locals(compiled.program, compiled.locals);
    registry
        .bind_vm_cached(&mut vm2)
        .expect("cached host binding should succeed");
    let status2 = vm2.run().expect("vm should run");
    assert_eq!(status2, VmStatus::Halted);
    assert_eq!(vm2.stack(), &[Value::Int(6)]);
}

#[test]
fn compile_source_supports_static_function_pointer_binding() {
    let source = r#"
        fn add_one(x);
        add_one(41);
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    vm.bind_static_function("add_one", static_add_one);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn host_function_registry_caches_static_function_pointer_plan_across_vms() {
    let source = include_str!("../examples/example.rss");
    let compiled = compile_source(source).expect("compile should succeed");

    let mut registry = HostFunctionRegistry::new();
    registry.register_static("print", 1, |_vm, args| {
        Ok(CallOutcome::Return(args.to_vec()))
    });
    registry.register_static("add_one", 1, static_add_one);
    let plan = registry
        .prepare_plan(&compiled.program.imports)
        .expect("plan should build");

    let mut vm1 = Vm::with_locals(compiled.program.clone(), compiled.locals);
    registry
        .bind_vm_with_plan(&mut vm1, &plan)
        .expect("cached static host binding should succeed");
    let status1 = vm1.run().expect("vm should run");
    assert_eq!(status1, VmStatus::Halted);
    assert_eq!(vm1.stack(), &[Value::Int(6)]);

    let mut vm2 = Vm::with_locals(compiled.program, compiled.locals);
    registry
        .bind_vm_with_plan(&mut vm2, &plan)
        .expect("cached static host binding should succeed");
    let status2 = vm2.run().expect("vm should run");
    assert_eq!(status2, VmStatus::Halted);
    assert_eq!(vm2.stack(), &[Value::Int(6)]);
}

#[test]
fn not_not_equal_and_else_if_are_supported_across_frontends() {
    let rustscript = r#"
        let a = 2;
        let out = 0;
        if !(a != 2) {
            out = 10;
        } else if a == 3 {
            out = 20;
        } else {
            out = 30;
        }
        out;
    "#;
    let javascript = r#"
        let a = 2;
        let out = 0;
        if (!(a != 2)) {
            out = 10;
        } else if (a == 3) {
            out = 20;
        } else {
            out = 30;
        }
        out;
    "#;
    let cases = [
        (SourceFlavor::RustScript, rustscript),
        (SourceFlavor::JavaScript, javascript),
    ];

    for (flavor, source) in cases {
        let compiled = compile_source_with_flavor(source, flavor).expect("compile should succeed");
        let mut vm = Vm::with_locals(compiled.program, compiled.locals);
        let status = vm.run().expect("vm should run");
        assert_eq!(status, VmStatus::Halted);
        assert_eq!(vm.stack(), &[Value::Int(10)]);
    }
}

#[test]
fn collections_are_created_and_accessed_in_all_frontends() {
    let rustscript = r#"
        let arr = [1, 2, 3];
        let second = arr[1];
        arr[1] = 9;
        let m = {"x": 1, "y": 2};
        m.z = 7;
        m["x"] = 4;
        let v1 = m.x;
        let v2 = m["z"];
        second + arr[1] + v1 + v2;
    "#;
    let javascript = r#"
        let arr = [1, 2, 3];
        let second = arr[1];
        arr[1] = 9;
        let m = { x: 1, y: 2 };
        m.z = 7;
        m["x"] = 4;
        let v1 = m.x;
        let v2 = m["z"];
        second + arr[1] + v1 + v2;
    "#;
    let cases = [
        (SourceFlavor::RustScript, rustscript),
        (SourceFlavor::JavaScript, javascript),
    ];

    for (flavor, source) in cases {
        let compiled = compile_source_with_flavor(source, flavor).expect("compile should succeed");
        assert!(
            compiled.functions.is_empty(),
            "collection intrinsics should be compiler-managed, not host imports"
        );
        let mut vm = Vm::with_locals(compiled.program, compiled.locals);
        let status = vm.run().expect("vm should run");
        assert_eq!(status, VmStatus::Halted);
        assert_eq!(vm.stack(), &[Value::Int(22)]);
    }
}

#[test]
fn collection_cardinality_uses_language_syntax_in_all_frontends() {
    let rustscript = r#"
        let arr = [1, 2, 3];
        let m = {"x": 1, "y": 2};
        arr.length + m.length;
    "#;
    let javascript = r#"
        let arr = [1, 2, 3];
        let m = { x: 1, y: 2 };
        arr.length + m.length;
    "#;
    let cases = [
        (SourceFlavor::RustScript, rustscript),
        (SourceFlavor::JavaScript, javascript),
    ];

    for (flavor, source) in cases {
        let compiled = compile_source_with_flavor(source, flavor).expect("compile should succeed");
        let mut vm = Vm::with_locals(compiled.program, compiled.locals);
        let status = vm.run().expect("vm should run");
        assert_eq!(status, VmStatus::Halted);
        assert_eq!(vm.stack(), &[Value::Int(5)]);
    }
}

#[test]
fn count_builtin_is_not_exposed_to_frontends() {
    let rustscript = r#"
        let arr = [1, 2, 3];
        count(arr);
    "#;
    let javascript = r#"
        let arr = [1, 2, 3];
        count(arr);
    "#;
    let lua = r#"
        local arr = {1, 2, 3}
        count(arr)
    "#;
    let scheme = r#"
        (define arr (vector 1 2 3))
        (count arr)
    "#;

    let cases = [
        (SourceFlavor::RustScript, rustscript),
        (SourceFlavor::JavaScript, javascript),
        (SourceFlavor::Lua, lua),
        (SourceFlavor::Scheme, scheme),
    ];

    for (flavor, source) in cases {
        let err = match compile_source_with_flavor(source, flavor) {
            Ok(_) => panic!("count should not be frontend-visible for {flavor:?}"),
            Err(err) => err,
        };
        match err {
            vm::SourceError::Parse(parse) => {
                assert!(
                    parse.message.contains("unknown function 'count'")
                        || parse.message.contains("not exposed in")
                        || parse.message.contains("unsupported"),
                    "unexpected parse error for {flavor:?}: {parse:?}"
                );
            }
            other => panic!("expected parse error for {flavor:?}, got {other:?}"),
        }
    }
}

#[test]
fn string_and_array_concat_work_via_plus_in_all_frontends() {
    let rustscript = r#"
        let joined = "he" + "llo";
        let arr = [1] + [2];
        joined.length + arr[0] + arr[1];
    "#;
    let javascript = r#"
        let joined = "he" + "llo";
        let arr = [1] + [2];
        joined.length + arr[0] + arr[1];
    "#;
    let cases = [
        (SourceFlavor::RustScript, rustscript),
        (SourceFlavor::JavaScript, javascript),
    ];

    for (flavor, source) in cases {
        let compiled = compile_source_with_flavor(source, flavor).expect("compile should succeed");
        let mut vm = Vm::with_locals(compiled.program, compiled.locals);
        let status = vm.run().expect("vm should run");
        assert_eq!(status, VmStatus::Halted);
        assert_eq!(vm.stack(), &[Value::Int(8)]);
    }
}

#[test]
fn string_and_int_concat_work_via_plus_in_all_frontends() {
    let rustscript = r#"
        let a = "x" + 1;
        let b = 2 + "y";
        let c = "x" + 1 + 2;
        let d = 3 + "y" + 4;
        a.length + b.length + c.length + d.length;
    "#;
    let javascript = r#"
        let a = "x" + 1;
        let b = 2 + "y";
        let c = "x" + 1 + 2;
        let d = 3 + "y" + 4;
        a.length + b.length + c.length + d.length;
    "#;
    let cases = [
        (SourceFlavor::RustScript, rustscript),
        (SourceFlavor::JavaScript, javascript),
    ];

    for (flavor, source) in cases {
        let compiled = compile_source_with_flavor(source, flavor).expect("compile should succeed");
        let mut vm = Vm::with_locals(compiled.program, compiled.locals);
        let status = vm.run().expect("vm should run");
        assert_eq!(status, VmStatus::Halted);
        assert_eq!(vm.stack(), &[Value::Int(10)]);
    }
}

#[test]
fn string_and_nonconstant_int_concat_autoconverts_in_all_frontends() {
    let rustscript = r#"
        let n = 41;
        let a = "v=" + n;
        let b = n + "!";
        a.length + b.length;
    "#;
    let javascript = r#"
        let n = 41;
        let a = "v=" + n;
        let b = n + "!";
        a.length + b.length;
    "#;
    let cases = [
        (SourceFlavor::RustScript, rustscript),
        (SourceFlavor::JavaScript, javascript),
    ];

    for (flavor, source) in cases {
        let compiled = compile_source_with_flavor(source, flavor).expect("compile should succeed");
        let mut vm = Vm::with_locals(compiled.program, compiled.locals);
        let status = vm.run().expect("vm should run");
        assert_eq!(status, VmStatus::Halted);
        assert_eq!(vm.stack(), &[Value::Int(7)]);
    }
}

#[test]
fn slice_ranges_work_in_all_frontends() {
    let rustscript = r#"
        let text = "abcdef";
        let end_pos = -2;
        let a = text[1:4];
        let b = text[:3];
        let c = text[2:];
        let d = text[:-1];
        let e = text[1:end_pos];
        let arr = [1, 2, 3, 4, 5];
        let f = arr[1:4];
        let g = arr[:2];
        let h = arr[3:];
        let i = arr[:-2];
        a.length + b.length + c.length + d.length + e.length + f.length + g.length + h.length + i.length;
    "#;
    let javascript = r#"
        let text = "abcdef";
        let end_pos = -2;
        let a = text[1:4];
        let b = text[:3];
        let c = text[2:];
        let d = text[:-1];
        let e = text[1:end_pos];
        let arr = [1, 2, 3, 4, 5];
        let f = arr[1:4];
        let g = arr[:2];
        let h = arr[3:];
        let i = arr[:-2];
        a.length + b.length + c.length + d.length + e.length + f.length + g.length + h.length + i.length;
    "#;
    let cases = [
        (SourceFlavor::RustScript, rustscript),
        (SourceFlavor::JavaScript, javascript),
    ];

    for (flavor, source) in cases {
        let compiled = compile_source_with_flavor(source, flavor).expect("compile should succeed");
        let mut vm = Vm::with_locals(compiled.program, compiled.locals);
        let status = vm.run().expect("vm should run");
        assert_eq!(status, VmStatus::Halted);
        assert_eq!(vm.stack(), &[Value::Int(28)]);
    }
}

#[test]
fn break_and_continue_outside_loop_are_rejected() {
    let break_err = match compile_source("break;") {
        Ok(_) => panic!("break outside loop should fail"),
        Err(err) => err,
    };
    let continue_err = match compile_source("continue;") {
        Ok(_) => panic!("continue outside loop should fail"),
        Err(err) => err,
    };

    match break_err {
        vm::SourceError::Parse(err) => assert!(err.message.contains("inside loops")),
        other => panic!("unexpected error: {other}"),
    }
    match continue_err {
        vm::SourceError::Parse(err) => assert!(err.message.contains("inside loops")),
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn compile_source_file_detects_extension() {
    let unique = format!(
        "vm_extension_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let base = std::env::temp_dir().join(unique);
    let path = base.with_extension("js");
    std::fs::write(&path, include_str!("../examples/example.js"))
        .expect("temp source should write");

    let compiled = compile_source_file(&path).expect("compile should succeed");
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

    let _ = std::fs::remove_file(path);
}

#[test]
fn compile_source_file_detects_lua_extension() {
    let unique = format!(
        "vm_extension_test_lua_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let base = std::env::temp_dir().join(unique);
    let path = base.with_extension("lua");
    std::fs::write(&path, include_str!("../examples/example.lua"))
        .expect("temp source should write");

    let err = match compile_source_file(&path) {
        Ok(_) => panic!("Lua example fixture should be rejected in direct subset"),
        Err(err) => err,
    };
    match err {
        vm::SourcePathError::Source(vm::SourceError::Parse(parse)) => {
            assert!(
                parse.message.contains("unsupported Lua syntax"),
                "{}",
                parse.message
            );
        }
        other => panic!("unexpected error: {other:?}"),
    }

    let _ = std::fs::remove_file(path);
}

#[test]
fn compile_source_file_detects_scheme_extension() {
    let unique = format!(
        "vm_extension_test_scheme_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let base = std::env::temp_dir().join(unique);
    let path = base.with_extension("scm");
    std::fs::write(&path, include_str!("../examples/example.scm"))
        .expect("temp source should write");

    let err = match compile_source_file(&path) {
        Ok(_) => panic!("Scheme example fixture should be rejected in direct subset"),
        Err(err) => err,
    };
    match err {
        vm::SourcePathError::Source(vm::SourceError::Parse(parse)) => {
            assert!(
                parse.message.contains("unsupported Scheme syntax"),
                "{}",
                parse.message
            );
        }
        other => panic!("unexpected error: {other:?}"),
    }

    let _ = std::fs::remove_file(path);
}

#[test]
fn compile_source_file_supports_rss_modules_from_js_lua_and_scheme() {
    let unique = format!(
        "vm_cross_flavor_import_test_{}_{}",
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

    let js_path = root.join("main.js");
    std::fs::write(
        &js_path,
        r#"
        import { add_one } from "./module.rss";
        console.log(add_one(41));
    "#,
    )
    .expect("js source should write");
    let js_compiled = compile_source_file(&js_path).expect("js compile should succeed");
    let mut js_vm = Vm::with_locals(js_compiled.program, js_compiled.locals);
    for func in &js_compiled.functions {
        match func.name.as_str() {
            "add_one" => js_vm.register_function(Box::new(AddOne)),
            "print" => js_vm.register_function(Box::new(PrintBuiltin)),
            _ => panic!("unexpected function {}", func.name),
        };
    }
    let js_status = js_vm.run().expect("js vm should run");
    assert_eq!(js_status, VmStatus::Halted);
    assert_eq!(js_vm.stack(), &[Value::Int(42)]);

    let lua_path = root.join("main.lua");
    std::fs::write(
        &lua_path,
        r#"
        local _m = require("./module.rss")
        print(add_one(41))
    "#,
    )
    .expect("lua source should write");
    let lua_err = match compile_source_file(&lua_path) {
        Ok(_) => panic!("lua compile should be rejected in direct subset"),
        Err(err) => err,
    };
    match lua_err {
        vm::SourcePathError::Source(vm::SourceError::Parse(parse)) => {
            assert!(
                parse.message.contains("unsupported Lua syntax"),
                "{}",
                parse.message
            );
        }
        other => panic!("unexpected lua error: {other:?}"),
    }

    let scm_path = root.join("main.scm");
    std::fs::write(
        &scm_path,
        r#"
        (import "./module.rss")
        (print (add_one 41))
    "#,
    )
    .expect("scheme source should write");
    let scm_err = match compile_source_file(&scm_path) {
        Ok(_) => panic!("scheme compile should be rejected in direct subset"),
        Err(err) => err,
    };
    match scm_err {
        vm::SourcePathError::Source(vm::SourceError::Parse(parse)) => {
            assert!(
                parse.message.contains("unsupported Scheme syntax"),
                "{}",
                parse.message
            );
        }
        other => panic!("unexpected scheme error: {other:?}"),
    }

    let _ = std::fs::remove_file(scm_path);
    let _ = std::fs::remove_file(lua_path);
    let _ = std::fs::remove_file(js_path);
    let _ = std::fs::remove_file(module_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_source_file_rejects_import_cycles() {
    let unique = format!(
        "vm_import_cycle_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp module root should be created");

    let main_path = root.join("main.rss");
    let a_path = root.join("a.rss");
    let b_path = root.join("b.rss");
    std::fs::write(&main_path, "use a;\n1;\n").expect("main source should write");
    std::fs::write(&a_path, "use b;\n").expect("module a source should write");
    std::fs::write(&b_path, "use a;\n").expect("module b source should write");

    let err = match compile_source_file(&main_path) {
        Ok(_) => panic!("cycle should fail"),
        Err(err) => err,
    };
    assert!(matches!(err, vm::SourcePathError::ImportCycle(_)));

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_file(a_path);
    let _ = std::fs::remove_file(b_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_source_with_string_literals() {
    let source = r#"
        fn echo(x);
        echo("hello");
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);

    for func in &compiled.functions {
        match func.name.as_str() {
            "echo" => vm.register_function(Box::new(EchoString)),
            _ => panic!("unexpected function {}", func.name),
        };
    }

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::String("hello".to_string())]);
}

#[test]
fn compile_source_emits_named_locals_in_debug_info() {
    let source = r#"
        let alpha = 1;
        let beta = alpha + 2;
        beta;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("compiled program should have debug info");

    assert_eq!(debug.local_index("alpha"), Some(0));
    assert_eq!(debug.local_index("beta"), Some(1));
}
