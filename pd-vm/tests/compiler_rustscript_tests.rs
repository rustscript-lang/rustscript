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
    let mut vm = Vm::new(compiled.program);
    vm.bind_function("add_one", Box::new(AddOne));

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn rustscript_vm_http_subnamespace_host_calls_are_supported() {
    let source = r#"
        use vm;
        vm::http::request::get_header("x-client-id");
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.bind_function("http::request::get_header", Box::new(EchoString));

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::String("x-client-id".to_string())]);
}

#[test]
fn rustscript_host_namespace_import_without_vm_prefix_is_supported() {
    let source = r#"
        use runtime;
        runtime::sleep(41);
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.bind_function("runtime::sleep", Box::new(AddOne));

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn rustscript_host_namespace_alias_import_is_supported() {
    struct AlwaysAllow;

    impl HostFunction for AlwaysAllow {
        fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
            Ok(CallOutcome::Return(vec![Value::Bool(true)]))
        }
    }

    let source = r#"
        use rate_limit as rl;
        rl::allow("client-1", 3, 60);
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.bind_function("rate_limit::allow", Box::new(AlwaysAllow));

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(true)]);
}

#[test]
fn rustscript_vm_named_host_imports_are_supported() {
    let source = r#"
        use vm::{add_one as inc};
        inc(41);
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.bind_function("add_one", Box::new(AddOne));

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
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
fn rustscript_builtin_namespace_calls_require_use_import() {
    let source = r#"
        json::encode("ok");
    "#;
    let err = match compile_source(source) {
        Ok(_) => panic!("builtin namespace calls should require explicit use import"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse) => {
            assert!(
                parse.message.contains("import builtin namespaces first"),
                "{}",
                parse.message
            );
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn rustscript_re_namespace_supports_optional_inline_flags_across_functions() {
    let source = r#"
        use re;
        let a = re::match("^foo$", "FoO", "i");
        let b = re::find("^foo", "FoO bar", "i");
        let c = re::replace("foo", "FoO bar", "x", "i");
        let d = re::split("x", "aXb", "i");
        let e = re::captures("^(foo)-([0-9]+)$", "FoO-42", "i");

        let score = 0;
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
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(5)]);
}

#[test]
fn rustscript_json_encode_decode_builtins_are_supported() {
    let source = r#"
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
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(44)]);
}

#[test]
fn rustscript_jit_namespace_builtins_can_configure_and_read_jit() {
    let source = r#"
        use jit;
        let _set = jit::set_hot_loop_threshold(3);
        let after = jit::get_hot_loop_threshold();
        let cfg = jit::get_config();
        if after == 3 && cfg.hot_loop_threshold == 3 {
            1;
        } else {
            0;
        }
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(1)]);
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
fn rustscript_float_literal_binding_is_supported() {
    let source = r#"
        let a=1.1;
        a;
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Float(1.1)]);
}

#[test]
fn rustscript_char_and_hex_escape_literals_are_supported() {
    let source = r#"
        let c = '\x41';
        let s = "\x42";
        c;
        s;
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[
            Value::String("A".to_string()),
            Value::String("B".to_string())
        ]
    );
}

#[test]
fn rustscript_array_primitives_are_supported_without_namespace() {
    let source = r#"
        let values = [];
        values[values.length] = 7;
        values[0] + values.length;
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);

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
        a.length + b.length + c.length + d.length + e.length + f.length + g.length + h.length + i.length;
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);

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
    let mut vm = Vm::new(compiled.program);

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
    let mut vm = Vm::new(compiled.program);

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
    let mut vm = Vm::new(compiled.program);

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
fn closure_values_are_first_class_and_can_be_passed_to_functions() {
    let source = r#"
        fn apply_twice(func, value) {
            let once = func(value);
            func(once);
        }

        let inc = |x| x + 1;
        apply_twice(inc, 40);
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn named_functions_are_first_class_and_can_be_passed_to_functions() {
    let source = r#"
        fn add_one(value) {
            value + 1;
        }
        fn apply_twice(func, value) {
            let once = func(value);
            func(once);
        }

        apply_twice(add_one, 40);
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn closure_value_from_partial_control_flow_is_rejected_on_call() {
    let source = r#"
        if true {
            let inc = |x| x + 1;
        }
        inc(1);
    "#;

    let err = match compile_source(source) {
        Ok(_) => panic!("branch-local closure call should fail"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse) => {
            assert!(
                parse.message.contains("inc") && parse.message.contains("unavailable"),
                "{}",
                parse.message
            );
        }
        other => panic!("expected parse error, got {other:?}"),
    }
}

#[test]
fn function_value_from_partial_control_flow_is_rejected_on_call() {
    let source = r#"
        fn add_one(value) {
            value + 1;
        }
        if true {
            let f = add_one;
        }
        f(1);
    "#;

    let err = match compile_source(source) {
        Ok(_) => panic!("branch-local function value call should fail"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse) => {
            assert!(
                parse.message.contains("f") && parse.message.contains("unavailable"),
                "{}",
                parse.message
            );
        }
        other => panic!("expected parse error, got {other:?}"),
    }
}

#[test]
fn liveness_clears_local_after_closure_value_last_use() {
    let source = r#"
        fn apply_once(func, value) {
            func(value);
        }

        let closure = "stale";
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

        let func = "stale";
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
fn rustscript_callable_values_cannot_be_stored_in_arrays() {
    let source = r#"
        fn add_one(value) {
            value + 1;
        }
        let func = add_one;
        let values = [func];
        values.length;
    "#;

    let err = match compile_source(source) {
        Ok(_) => panic!("storing callable in array should fail in current subset"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Compile(vm::CompileError::CallableUsedAsValue) => {}
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn rustscript_callable_values_cannot_be_returned_from_functions() {
    let source = r#"
        fn add_one(value) {
            value + 1;
        }
        fn get_adder() {
            add_one;
        }

        let func = get_adder();
        func(41);
    "#;

    let err = match compile_source(source) {
        Ok(_) => panic!("returning callable should fail in current subset"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Compile(vm::CompileError::CallableUsedAsValue) => {}
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn rustscript_if_expression_assignment_syntax_is_supported() {
    let source = r#"
        let x = if 2 > 1 => { 42 } else => { 0 };
        x;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);

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
    let mut vm = Vm::new(compiled.program);

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
    let mut vm = Vm::new(compiled.program);

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
    let mut vm = Vm::new(compiled.program);

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
    let mut vm = Vm::new(compiled.program);

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
    let mut vm = Vm::new(compiled.program);

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
                parse
                    .message
                    .contains("int/string/null literals, type patterns"),
                "unexpected parse error: {parse:?}"
            );
        }
        other => panic!("expected parse error, got {other:?}"),
    }
}

#[test]
fn rustscript_match_expression_supports_type_patterns() {
    let source = r#"
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
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(6)]);
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
fn rustscript_modulo_and_logical_operators_work() {
    let source = r#"
        let a = 17 % 5;
        let b = true && false;
        let c = true || false;
        let d = (10 > 5) && (3 < 7);
        let e = (10 < 5) || (3 > 7);
        let f = 100 % 7;
        a + f;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4)]);
}

#[test]
fn rustscript_null_literal_is_supported() {
    let source = r#"
        let v = null;
        type(v) == "null";
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(true)]);
}

#[test]
fn rustscript_option_namespace_style_is_supported() {
    let source = r#"
        let some = Option::Some(1 + 1);
        let none = Option::None;
        let some2 = Option::Some(40);

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

        let t = type(Option::None);
        if t == "null" {
            (a + b + c) + some2;
        } else {
            0;
        }
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(43)]);
}

#[test]
fn rustscript_match_type_pattern_is_not_shadowed_by_local_name() {
    let source = r#"
        let String = 3;
        let b = match "" {
            Some(String) => 2,
            _ => 3,
        };
        b;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(2)]);
}

#[test]
fn rustscript_legacy_option_aliases_are_rejected() {
    let source = r#"
        let some = Some(1);
        some;
    "#;

    let err = match compile_source(source) {
        Ok(_) => panic!("bare Some(...) should be rejected"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse) => {
            assert!(
                parse.message.contains("unknown function 'Some'"),
                "unexpected parse error: {parse:?}"
            );
        }
        other => panic!("expected parse error, got {other:?}"),
    }
}

#[test]
fn rustscript_type_name_of_val_alias_is_rejected() {
    let source = r#"
        type_name_of_val(null);
    "#;

    let err = match compile_source(source) {
        Ok(_) => panic!("type_name_of_val alias should be rejected"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse) => {
            assert!(
                parse
                    .message
                    .contains("unknown function 'type_name_of_val'"),
                "unexpected parse error: {parse:?}"
            );
        }
        other => panic!("expected parse error, got {other:?}"),
    }
}

#[test]
fn rustscript_null_literal_can_be_used_as_map_key() {
    let source = r#"
        let m = { null: 42 };
        m[null];
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}
