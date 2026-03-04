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
    let mut vm = Vm::new(compiled.program);
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
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");

    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(2)]);
}

#[test]
fn compiler_reuses_slots_when_declared_locals_exceed_bytecode_limit() {
    let mut source = String::from("let out = 0;\n");
    for idx in 0..600usize {
        source.push_str(&format!("let v{idx} = {idx};\n"));
        source.push_str(&format!("out = v{idx};\n"));
    }
    source.push_str("out;\n");

    let compiled = compile_source(&source).expect("compile should succeed");
    assert!(
        compiled.locals <= (u8::MAX as usize + 1),
        "slot allocator should remap to bytecode locals"
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(599)]);
}

#[test]
fn slot_reuse_preserves_distinct_debug_locals() {
    use std::collections::HashSet;

    let mut source = String::new();
    for idx in 0..300usize {
        source.push_str(&format!("let v{idx} = {idx};\n"));
    }
    source.push_str("0;\n");

    let compiled = compile_source(&source).expect("compile should succeed");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("compiled program should include debug info");

    let locals = debug
        .locals
        .iter()
        .filter(|local| local.name.starts_with('v'))
        .collect::<Vec<_>>();
    assert_eq!(locals.len(), 300);
    assert!(
        locals
            .iter()
            .all(|local| local.declared_line.is_some() && local.last_line.is_some()),
        "all named locals should include declaration and last-use lines"
    );

    let distinct_slots = locals
        .iter()
        .map(|local| local.index)
        .collect::<HashSet<_>>()
        .len();
    assert!(
        distinct_slots < locals.len(),
        "debug locals should remain distinct even when physical slots are reused"
    );
}

#[test]
fn compiler_rejects_programs_with_more_than_256_simultaneously_live_locals() {
    let live_count = (u8::MAX as usize) + 2;
    let mut source = String::new();
    for idx in 0..live_count {
        source.push_str(&format!("let v{idx} = {idx};\n"));
    }
    source.push_str("let out = ");
    for idx in 0..live_count {
        if idx > 0 {
            source.push_str(" + ");
        }
        source.push_str(&format!("v{idx}"));
    }
    source.push_str(";\nout;\n");

    let err = match compile_source(&source) {
        Ok(_) => panic!("compile should fail"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse_err) => {
            assert!(
                parse_err
                    .message
                    .contains("too many simultaneously live locals"),
                "unexpected parse error: {parse_err:?}"
            );
        }
        other => panic!("expected parse error, got {other:?}"),
    }
}

#[test]
fn compiler_reuses_slots_with_large_programs_that_inline_functions() {
    let mut source = String::from("fn id(x) { x; }\nlet out = 0;\n");
    for idx in 0..400usize {
        source.push_str(&format!("let v{idx} = {idx};\n"));
        source.push_str(&format!("out = id(v{idx});\n"));
    }
    source.push_str("out;\n");

    let compiled = compile_source(&source).expect("compile should succeed");
    assert!(
        compiled.locals <= (u8::MAX as usize + 1),
        "slot allocator should keep inline-call programs within bytecode local limits"
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(399)]);
}
#[test]
fn compile_source_with_functions() {
    let source = include_str!("../examples/example.rss");

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);

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
    let mut vm = Vm::new(compiled.program);

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
    let mut vm = Vm::new(compiled.program);
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

    let mut vm1 = Vm::new(compiled.program.clone());
    registry
        .bind_vm_cached(&mut vm1)
        .expect("cached host binding should succeed");
    let status1 = vm1.run().expect("vm should run");
    assert_eq!(status1, VmStatus::Halted);
    assert_eq!(vm1.stack(), &[Value::Int(6)]);

    let mut vm2 = Vm::new(compiled.program);
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
    let mut vm = Vm::new(compiled.program);
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

    let mut vm1 = Vm::new(compiled.program.clone());
    registry
        .bind_vm_with_plan(&mut vm1, &plan)
        .expect("cached static host binding should succeed");
    let status1 = vm1.run().expect("vm should run");
    assert_eq!(status1, VmStatus::Halted);
    assert_eq!(vm1.stack(), &[Value::Int(6)]);

    let mut vm2 = Vm::new(compiled.program);
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
        let mut vm = Vm::new(compiled.program);
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
        let mut vm = Vm::new(compiled.program);
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
        let mut vm = Vm::new(compiled.program);
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
        let mut vm = Vm::new(compiled.program);
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
        let mut vm = Vm::new(compiled.program);
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
        let mut vm = Vm::new(compiled.program);
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
        let mut vm = Vm::new(compiled.program);
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
fn local_declared_only_on_one_if_path_is_rejected_on_later_use() {
    let source = r#"
        if true {
            let branch_only = 7;
        }
        branch_only;
    "#;

    let err = match compile_source(source) {
        Ok(_) => panic!("using a path-dependent local should fail"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse) => {
            assert!(
                parse.message.contains("branch_only") && parse.message.contains("unavailable"),
                "{}",
                parse.message
            );
        }
        other => panic!("expected parse error, got {other:?}"),
    }
}

#[test]
fn local_declared_only_in_loop_body_is_rejected_after_loop() {
    let source = r#"
        while false {
            let loop_only = 1;
        }
        loop_only;
    "#;

    let err = match compile_source(source) {
        Ok(_) => panic!("using a loop-path local should fail"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse) => {
            assert!(
                parse.message.contains("loop_only") && parse.message.contains("unavailable"),
                "{}",
                parse.message
            );
        }
        other => panic!("expected parse error, got {other:?}"),
    }
}

#[test]
fn path_dependent_local_assignment_requires_redeclaration() {
    let source = r#"
        if true {
            let path_local = 1;
        }
        path_local = 9;
        path_local;
    "#;

    let err = match compile_source(source) {
        Ok(_) => panic!("path-dependent assignment should fail without redeclaration"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse) => {
            assert!(
                parse.message.contains("path_local") && parse.message.contains("before assignment"),
                "{}",
                parse.message
            );
        }
        other => panic!("expected parse error, got {other:?}"),
    }
}

#[test]
fn path_dependent_local_redeclaration_before_assignment_is_allowed() {
    let source = r#"
        if true {
            let path_local = 1;
        }
        let path_local = 9;
        path_local;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(9)]);
}

#[test]
fn compiler_clears_uncertain_locals_after_control_flow_join() {
    let source = r#"
        let gate = true;
        if gate {
            let ephemeral = "payload";
        }
        0;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("debug info should be present");
    let ephemeral_index = debug
        .local_index("ephemeral")
        .expect("ephemeral local should be emitted");

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(0)]);
    assert_eq!(vm.locals()[ephemeral_index as usize], Value::Null);
}

#[test]
fn liveness_pass_clears_dead_locals_after_last_use() {
    let source = r#"
        let d = "12321312";
        let e = "23232";
        e;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("debug info should be present");
    let d_index = debug.local_index("d").expect("d should exist");
    let e_index = debug.local_index("e").expect("e should exist");

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::String("23232".to_string())]);
    assert_eq!(vm.locals()[d_index as usize], Value::Null);
    assert_eq!(vm.locals()[e_index as usize], Value::Null);
}

#[derive(Clone, Copy, Debug)]
struct DecodedInstr {
    ip: usize,
    op: u8,
    width: usize,
    u32_operand: Option<u32>,
    u8_operand: Option<u8>,
}

fn decode_instructions(code: &[u8]) -> Vec<DecodedInstr> {
    let mut ip = 0usize;
    let mut instructions = Vec::new();
    while ip < code.len() {
        let op = code[ip];
        let (width, u32_operand, u8_operand) = if op == vm::OpCode::Ldc as u8
            || op == vm::OpCode::Br as u8
            || op == vm::OpCode::Brfalse as u8
        {
            assert!(
                ip + 5 <= code.len(),
                "truncated 4-byte operand at instruction {ip}"
            );
            let mut bytes = [0u8; 4];
            bytes.copy_from_slice(&code[ip + 1..ip + 5]);
            (5usize, Some(u32::from_le_bytes(bytes)), None)
        } else if op == vm::OpCode::Ldloc as u8 || op == vm::OpCode::Stloc as u8 {
            assert!(
                ip + 2 <= code.len(),
                "truncated 1-byte operand at instruction {ip}"
            );
            (2usize, None, Some(code[ip + 1]))
        } else if op == vm::OpCode::Call as u8 {
            assert!(
                ip + 4 <= code.len(),
                "truncated call operand at instruction {ip}"
            );
            (4usize, None, None)
        } else {
            (1usize, None, None)
        };
        instructions.push(DecodedInstr {
            ip,
            op,
            width,
            u32_operand,
            u8_operand,
        });
        ip += width;
    }
    instructions
}

fn find_first_while_loop_span(instructions: &[DecodedInstr]) -> (usize, usize, usize) {
    for instruction in instructions {
        if instruction.op != vm::OpCode::Brfalse as u8 {
            continue;
        }
        let Some(loop_end_u32) = instruction.u32_operand else {
            continue;
        };
        let loop_end = loop_end_u32 as usize;
        if loop_end <= instruction.ip + instruction.width {
            continue;
        }

        let loop_body_start = instruction.ip + instruction.width;
        let mut backedge_ip: Option<usize> = None;
        for candidate in instructions {
            if candidate.ip < loop_body_start || candidate.ip >= loop_end {
                continue;
            }
            if candidate.op != vm::OpCode::Br as u8 {
                continue;
            }
            let Some(target_u32) = candidate.u32_operand else {
                continue;
            };
            let target = target_u32 as usize;
            if target < instruction.ip {
                backedge_ip =
                    Some(backedge_ip.map_or(candidate.ip, |current| current.max(candidate.ip)));
            }
        }

        if let Some(loop_backedge_ip) = backedge_ip {
            return (loop_body_start, loop_backedge_ip, loop_end);
        }
    }
    panic!("expected to find at least one while-loop span in emitted bytecode");
}

fn collect_null_store_pairs(
    instructions: &[DecodedInstr],
    constants: &[Value],
) -> Vec<(usize, u8)> {
    let mut null_stores = Vec::new();
    for pair in instructions.windows(2) {
        let lhs = pair[0];
        let rhs = pair[1];
        if lhs.op != vm::OpCode::Ldc as u8 || rhs.op != vm::OpCode::Stloc as u8 {
            continue;
        }
        if lhs.ip + lhs.width != rhs.ip {
            continue;
        }

        let Some(const_index_u32) = lhs.u32_operand else {
            continue;
        };
        let const_index = const_index_u32 as usize;
        let is_null_const = matches!(constants.get(const_index), Some(Value::Null));
        if !is_null_const {
            continue;
        }
        let slot = rhs.u8_operand.expect("stloc should include local slot");
        null_stores.push((lhs.ip, slot));
    }
    null_stores
}

#[test]
fn liveness_avoids_in_loop_null_clears_but_clears_after_loop_exit() {
    let source = r#"
        let iter = 0;
        let carry = 0;
        while iter < 2 {
            let a = iter + 1;
            let b = a + carry;
            carry = b;
            iter = iter + 1;
        }
        carry;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("debug info should be present");
    let a_index = debug.local_index("a").expect("a should exist");
    let b_index = debug.local_index("b").expect("b should exist");

    let instructions = decode_instructions(&compiled.program.code);
    let (loop_body_start, loop_backedge_ip, loop_end) = find_first_while_loop_span(&instructions);
    let null_stores = collect_null_store_pairs(&instructions, &compiled.program.constants);

    let in_loop_null_stores = null_stores
        .iter()
        .copied()
        .filter(|(ip, _)| *ip >= loop_body_start && *ip < loop_backedge_ip)
        .collect::<Vec<_>>();
    assert!(
        in_loop_null_stores.is_empty(),
        "expected no `ldc null; stloc` in loop body [{}..{}), found {:?}",
        loop_body_start,
        loop_backedge_ip,
        in_loop_null_stores
    );

    for slot in [a_index, b_index] {
        assert!(
            null_stores
                .iter()
                .any(|(ip, cleared_slot)| *ip >= loop_end && *cleared_slot == slot),
            "expected post-loop null clear for local slot {slot}, clears={:?}, loop_end={loop_end}",
            null_stores
        );
    }

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(3)]);
    assert_eq!(vm.locals()[a_index as usize], Value::Null);
    assert_eq!(vm.locals()[b_index as usize], Value::Null);
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
    let mut vm = Vm::new(compiled.program);
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
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/example.lua");
    let compiled = compile_source_file(&path).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
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
    std::fs::write(
        &path,
        r#"
        (define i 0)
        (define total 0)
        (while (< i 3)
          (set! total (+ total 1))
          (set! i (+ i 1)))
        (+ total 3)
    "#,
    )
    .expect("temp source should write");

    let compiled = compile_source_file(&path).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(6)]);

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
    let mut js_vm = Vm::new(js_compiled.program);
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
    let lua_compiled = compile_source_file(&lua_path).expect("lua compile should succeed");
    let mut lua_vm = Vm::new(lua_compiled.program);
    for func in &lua_compiled.functions {
        match func.name.as_str() {
            "add_one" => lua_vm.register_function(Box::new(AddOne)),
            "print" => lua_vm.register_function(Box::new(PrintBuiltin)),
            _ => panic!("unexpected function {}", func.name),
        };
    }
    let lua_status = lua_vm.run().expect("lua vm should run");
    assert_eq!(lua_status, VmStatus::Halted);
    assert_eq!(lua_vm.stack(), &[Value::Int(42)]);

    let scm_path = root.join("main.scm");
    std::fs::write(
        &scm_path,
        r#"
        (import "./module.rss")
        (print (add_one 41))
    "#,
    )
    .expect("scheme source should write");
    let scm_compiled = compile_source_file(&scm_path).expect("scheme compile should succeed");
    let mut scm_vm = Vm::new(scm_compiled.program);
    for func in &scm_compiled.functions {
        match func.name.as_str() {
            "add_one" => scm_vm.register_function(Box::new(AddOne)),
            "print" => scm_vm.register_function(Box::new(PrintBuiltin)),
            _ => panic!("unexpected function {}", func.name),
        };
    }
    let scm_status = scm_vm.run().expect("scheme vm should run");
    assert_eq!(scm_status, VmStatus::Halted);
    assert_eq!(scm_vm.stack(), &[Value::Int(42)]);

    let _ = std::fs::remove_file(scm_path);
    let _ = std::fs::remove_file(lua_path);
    let _ = std::fs::remove_file(js_path);
    let _ = std::fs::remove_file(module_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_source_file_keeps_debug_lines_in_original_source_coordinates() {
    let unique = format!(
        "vm_debug_line_map_test_{}_{}",
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

    let js_source = r#"import { add_one } from "./module.rss";
let value = add_one(41);
console.log(value);
"#;
    let js_path = root.join("main.js");
    std::fs::write(&js_path, js_source).expect("js source should write");

    let compiled = compile_source_file(&js_path).expect("js compile should succeed");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("compiled program should have debug info");
    let source = debug
        .source
        .as_ref()
        .expect("debug source should be embedded");

    assert!(
        source.contains("import { add_one } from \"./module.rss\";"),
        "debug source should stay in original source coordinates"
    );
    let max_source_line = js_source.lines().count() as u32;
    assert!(
        debug.lines.iter().all(|line| line.line <= max_source_line),
        "all debug lines should fit within original source line count"
    );

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
    let mut vm = Vm::new(compiled.program);

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
