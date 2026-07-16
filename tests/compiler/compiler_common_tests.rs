#[path = "../common/mod.rs"]
mod common;
use common::*;
use vm::OpCode;

const LOCAL_SLOT_COMPAT_THRESHOLD: usize = 8;

fn sequential_locals_source(local_count: usize) -> String {
    let mut source = String::new();
    for idx in 0..local_count {
        source.push_str(&format!("let v{idx} = {idx};\n"));
    }
    source.push_str(&format!("v{};\n", local_count - 1));
    source
}

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
        let mut a = 1;
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
    let mut source = String::from("let mut out = 0;\n");
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
fn compiler_preserves_source_slots_at_compat_threshold() {
    use std::collections::HashSet;

    let source = sequential_locals_source(LOCAL_SLOT_COMPAT_THRESHOLD);

    let compiled = compile_source(&source).expect("compile should succeed");
    assert_eq!(
        compiled.locals, LOCAL_SLOT_COMPAT_THRESHOLD,
        "locals at the compat threshold should keep source slot identities"
    );

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
    assert_eq!(locals.len(), LOCAL_SLOT_COMPAT_THRESHOLD);

    let distinct_slots = locals
        .iter()
        .map(|local| local.index)
        .collect::<HashSet<_>>()
        .len();
    assert_eq!(
        distinct_slots, LOCAL_SLOT_COMPAT_THRESHOLD,
        "debug locals at the threshold should retain distinct physical slots"
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::Int((LOCAL_SLOT_COMPAT_THRESHOLD - 1) as i64)]
    );
}

#[test]
fn compiler_reuses_slots_immediately_above_compat_threshold() {
    use std::collections::HashSet;

    let source = sequential_locals_source(LOCAL_SLOT_COMPAT_THRESHOLD + 1);

    let compiled = compile_source(&source).expect("compile should succeed");
    assert!(
        compiled.locals <= LOCAL_SLOT_COMPAT_THRESHOLD,
        "locals above the compat threshold should be remapped into a smaller physical slot set"
    );

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
    assert_eq!(locals.len(), LOCAL_SLOT_COMPAT_THRESHOLD + 1);

    let distinct_slots = locals
        .iter()
        .map(|local| local.index)
        .collect::<HashSet<_>>()
        .len();
    assert!(
        distinct_slots < locals.len(),
        "debug locals above the threshold should show physical slot reuse"
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::Int(LOCAL_SLOT_COMPAT_THRESHOLD as i64)]
    );
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
    let mut source = String::from("fn id(x) { x }\nlet mut out = 0;\n");
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
    let source = include_str!("../../examples/example.rss");

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
    let source = include_str!("../../examples/example.rss");
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);

    vm.bind_function("print", Box::new(PrintBuiltin));
    vm.bind_function("add_one", Box::new(AddOne));

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(6)]);
}

#[test]
fn compiler_rejects_if_else_type_mismatch_cases() {
    let cases = [
        SourceErrorCase {
            name: "if else expression branch type mismatch",
            source: r#"
                let value = if true => { 1 } else => { "x" };
                value;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::IfElseBranchTypeMismatch),
            expected_contains_all: &["expression result", "int", "string"],
        },
        SourceErrorCase {
            name: "if else local merge mismatch",
            source: r#"
                let mut value = 0;
                if true {
                    value = 1;
                } else {
                    value = "x";
                }
                value;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::IfElseBranchTypeMismatch),
            expected_contains_all: &["local slot", "int", "string"],
        },
        SourceErrorCase {
            name: "if else initializer self reference is unknown local",
            source: r#"
                let total = if true => {
                    "222"
                } else => {
                    let bumped = total + 1;
                    bumped
                };
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Parse,
            expected_contains_all: &["unknown local 'total'"],
        },
        SourceErrorCase {
            name: "if else branch type mismatch through shadowed outer local",
            source: r#"
                let total = 1;
                let total = if true => {
                    "222"
                } else => {
                    let bumped = total + 1;
                    bumped
                };
                total;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::IfElseBranchTypeMismatch),
            expected_contains_all: &["expression result", "string", "int"],
        },
        SourceErrorCase {
            name: "if else branch type mismatch through shadowed outer local after loop",
            source: r#"
                let mut total = 0;
                for i in 0..4 {
                    total = total + i;
                }

                let total = if true => {
                    "222"
                } else => {
                    let bumped = total + 1;
                    bumped
                };
                total;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::IfElseBranchTypeMismatch),
            expected_contains_all: &["expression result", "string", "int"],
        },
    ];
    run_source_error_cases(&cases);
}

#[test]
fn compiler_rejects_callable_argument_type_mismatches() {
    let cases = [
        SourceErrorCase {
            name: "language builtin rejects wrong argument type",
            source: r#"
                assert(1);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::CallableArgumentTypeMismatch),
            expected_contains_all: &["builtin 'assert'", "int", "condition: bool"],
        },
        SourceErrorCase {
            name: "builtin namespace member rejects wrong argument type",
            source: r#"
                use math;
                math::sqrt(true);
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::CallableArgumentTypeMismatch),
            expected_contains_all: &["builtin 'math::sqrt'", "bool", "value: number"],
        },
        SourceErrorCase {
            name: "host function rejects wrong argument type",
            source: r#"
                use runtime;
                runtime::sleep("later");
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::CallableArgumentTypeMismatch),
            expected_contains_all: &["host function 'runtime::sleep'", "string", "ms: int"],
        },
    ];
    run_source_error_cases(&cases);
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
    let source = include_str!("../../examples/example.rss");
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
fn host_function_registry_shared_plan_cache_survives_clone() {
    let source = include_str!("../../examples/example.rss");
    let compiled = compile_source(source).expect("compile should succeed");

    let mut registry = HostFunctionRegistry::new();
    registry.register("print", 1, || Box::new(PrintBuiltin));
    registry.register("add_one", 1, || Box::new(AddOne));

    let first_plan = registry
        .prepare_shared_plan(&compiled.program.imports)
        .expect("shared plan should build");
    let cloned = registry.clone();
    let second_plan = cloned
        .prepare_shared_plan(&compiled.program.imports)
        .expect("cloned registry should reuse shared plan");

    assert!(
        std::sync::Arc::ptr_eq(&first_plan, &second_plan),
        "cloned registries should share prepared host binding plans"
    );
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
fn compile_source_supports_static_args_function_pointer_binding() {
    let source = r#"
        fn add_one(x);
        add_one(41);
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.bind_static_args_function("add_one", static_add_one_args);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn host_function_registry_caches_static_function_pointer_plan_across_vms() {
    let source = include_str!("../../examples/example.rss");
    let compiled = compile_source(source).expect("compile should succeed");

    let mut registry = HostFunctionRegistry::new();
    registry.register_static("print", 1, |_vm, args| {
        Ok(CallOutcome::Return(args.to_vec().into()))
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
fn host_function_registry_caches_static_args_function_pointer_plan_across_vms() {
    let source = include_str!("../../examples/example.rss");
    let compiled = compile_source(source).expect("compile should succeed");

    let mut registry = HostFunctionRegistry::new();
    registry.register_static_args("print", 1, |args| {
        Ok(CallOutcome::Return(args.to_vec().into()))
    });
    registry.register_static_args("add_one", 1, static_add_one_args);
    let plan = registry
        .prepare_plan(&compiled.program.imports)
        .expect("plan should build");

    let mut vm1 = Vm::new(compiled.program.clone());
    registry
        .bind_vm_with_plan(&mut vm1, &plan)
        .expect("cached static args host binding should succeed");
    let status1 = vm1.run().expect("vm should run");
    assert_eq!(status1, VmStatus::Halted);
    assert_eq!(vm1.stack(), &[Value::Int(6)]);

    let mut vm2 = Vm::new(compiled.program);
    registry
        .bind_vm_with_plan(&mut vm2, &plan)
        .expect("cached static args host binding should succeed");
    let status2 = vm2.run().expect("vm should run");
    assert_eq!(status2, VmStatus::Halted);
    assert_eq!(vm2.stack(), &[Value::Int(6)]);
}

#[test]
fn host_function_registry_caches_static_non_yielding_args_function_pointer_plan_across_vms() {
    let source = include_str!("../../examples/example.rss");
    let compiled = compile_source(source).expect("compile should succeed");

    let mut registry = HostFunctionRegistry::new();
    registry.register_static_non_yielding_args("print", 1, |args| {
        Ok(CallOutcome::Return(args.to_vec().into()))
    });
    registry.register_static_non_yielding_args("add_one", 1, static_add_one_args);
    let plan = registry
        .prepare_plan(&compiled.program.imports)
        .expect("plan should build");

    let mut vm1 = Vm::new(compiled.program.clone());
    registry
        .bind_vm_with_plan(&mut vm1, &plan)
        .expect("cached static non-yielding args host binding should succeed");
    let status1 = vm1.run().expect("vm should run");
    assert_eq!(status1, VmStatus::Halted);
    assert_eq!(vm1.stack(), &[Value::Int(6)]);

    let mut vm2 = Vm::new(compiled.program);
    registry
        .bind_vm_with_plan(&mut vm2, &plan)
        .expect("cached static non-yielding args host binding should succeed");
    let status2 = vm2.run().expect("vm should run");
    assert_eq!(status2, VmStatus::Halted);
    assert_eq!(vm2.stack(), &[Value::Int(6)]);
}

#[test]
fn host_function_registry_preserves_static_non_yielding_args_contract_in_prepared_plan() {
    let source = r#"
        fn yield_now();
        yield_now();
    "#;
    let compiled = compile_source(source).expect("compile should succeed");

    let mut registry = HostFunctionRegistry::new();
    registry.register_static_non_yielding_args("yield_now", 0, |_args| Ok(CallOutcome::Yield));
    let plan = registry
        .prepare_plan(&compiled.program.imports)
        .expect("plan should build");

    let mut vm = Vm::new(compiled.program);
    registry
        .bind_vm_with_plan(&mut vm, &plan)
        .expect("cached static non-yielding args host binding should succeed");

    let err = vm.run().expect_err("yield should violate the contract");
    assert!(
        matches!(
            err,
            vm::VmError::HostError(ref detail)
                if detail.contains("non-yielding host function returned yield")
        ),
        "unexpected error: {err:?}"
    );
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
            let mut path_local = 1;
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
                parse.message.contains("path_local")
                    && (parse.message.contains("before assignment")
                        || parse.message.contains("unavailable")),
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
    assert_eq!(vm.stack(), &[Value::string("23232")]);
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
    call_index: Option<u16>,
    call_arity: Option<u8>,
}

fn decode_instructions(code: &[u8]) -> Vec<DecodedInstr> {
    let mut ip = 0usize;
    let mut instructions = Vec::new();
    while ip < code.len() {
        let op = code[ip];
        let (width, u32_operand, u8_operand, call_index, call_arity) = if op
            == vm::OpCode::Ldc as u8
            || op == vm::OpCode::Br as u8
            || op == vm::OpCode::Brfalse as u8
        {
            assert!(
                ip + 5 <= code.len(),
                "truncated 4-byte operand at instruction {ip}"
            );
            let mut bytes = [0u8; 4];
            bytes.copy_from_slice(&code[ip + 1..ip + 5]);
            (5usize, Some(u32::from_le_bytes(bytes)), None, None, None)
        } else if op == vm::OpCode::Ldloc as u8 || op == vm::OpCode::Stloc as u8 {
            assert!(
                ip + 2 <= code.len(),
                "truncated 1-byte operand at instruction {ip}"
            );
            (2usize, None, Some(code[ip + 1]), None, None)
        } else if op == vm::OpCode::Call as u8 {
            assert!(
                ip + 4 <= code.len(),
                "truncated call operand at instruction {ip}"
            );
            (
                4usize,
                None,
                None,
                Some(u16::from_le_bytes([code[ip + 1], code[ip + 2]])),
                Some(code[ip + 3]),
            )
        } else {
            (1usize, None, None, None, None)
        };
        instructions.push(DecodedInstr {
            ip,
            op,
            width,
            u32_operand,
            u8_operand,
            call_index,
            call_arity,
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
fn same_local_collection_set_clears_target_immediately_before_call() {
    let source = r#"
        let mut a = [10, 20];
        a[0] = a[1];
        a[0];
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let a_index = compiled
        .program
        .debug
        .as_ref()
        .and_then(|debug| debug.local_index("a"))
        .expect("a should exist in debug info");
    let instructions = decode_instructions(&compiled.program.code);
    let set_index = vm::BuiltinFunction::Set.call_index();
    let call_position = instructions
        .iter()
        .position(|instruction| {
            instruction.op == OpCode::Call as u8
                && instruction.call_index == Some(set_index)
                && instruction.call_arity == Some(3)
        })
        .expect("indexed assignment should emit set/3");
    assert!(
        call_position >= 2,
        "set call should have preceding instructions"
    );
    assert!(
        call_position + 1 < instructions.len(),
        "set call should have a following stloc"
    );

    let clear_const = instructions[call_position - 2];
    let clear_store = instructions[call_position - 1];
    let result_store = instructions[call_position + 1];
    assert_eq!(clear_const.op, OpCode::Ldc as u8);
    assert!(matches!(
        clear_const
            .u32_operand
            .and_then(|index| compiled.program.constants.get(index as usize)),
        Some(Value::Null)
    ));
    assert_eq!(clear_store.op, OpCode::Stloc as u8);
    assert_eq!(clear_store.u8_operand, Some(a_index));
    assert_eq!(result_store.op, OpCode::Stloc as u8);
    assert_eq!(result_store.u8_operand, Some(a_index));

    assert!(
        instructions[..call_position - 2].iter().any(|instruction| {
            instruction.op == OpCode::Ldloc as u8 && instruction.u8_operand == Some(a_index)
        }),
        "target should remain readable while key and rhs are evaluated"
    );

    let mut vm = Vm::new(compiled.program);
    assert_eq!(vm.run().expect("vm should run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(20)]);
}

#[test]
fn same_local_collection_set_preserves_key_then_rhs_evaluation_order() {
    let array_new = Expr::Call(
        vm::BuiltinFunction::ArrayNew.call_index(),
        Vec::new(),
        Vec::new(),
    );
    let array_with_first = Expr::Call(
        vm::BuiltinFunction::ArrayPush.call_index(),
        Vec::new(),
        vec![array_new, Expr::Int(10)],
    );
    let array = Expr::Call(
        vm::BuiltinFunction::ArrayPush.call_index(),
        Vec::new(),
        vec![array_with_first, Expr::Int(20)],
    );
    let append_order = |suffix: &str| Stmt::Assign {
        kind: vm::AssignmentKind::Set,
        index: 1,
        expr: Expr::Add(
            Box::new(Expr::Var(1)),
            Box::new(Expr::String(suffix.to_string())),
        ),
        line: 2,
    };
    let key = Expr::Block {
        stmts: vec![append_order("k")],
        expr: Box::new(Expr::Int(0)),
    };
    let rhs = Expr::Block {
        stmts: vec![append_order("v")],
        expr: Box::new(Expr::Call(
            vm::BuiltinFunction::Get.call_index(),
            Vec::new(),
            vec![Expr::Var(0), Expr::Int(1)],
        )),
    };

    let mut compiler = Compiler::new();
    compiler.set_enable_local_move_semantics(true);
    let program = compiler
        .compile_program(&[
            Stmt::Let {
                index: 0,
                declared_schema: None,
                expr: array,
                line: 1,
            },
            Stmt::Let {
                index: 1,
                declared_schema: None,
                expr: Expr::String(String::new()),
                line: 1,
            },
            Stmt::Assign {
                kind: vm::AssignmentKind::Set,
                index: 0,
                expr: Expr::Call(
                    vm::BuiltinFunction::Set.call_index(),
                    Vec::new(),
                    vec![Expr::Var(0), key, rhs],
                ),
                line: 2,
            },
            Stmt::Expr {
                expr: Expr::Var(1),
                line: 3,
            },
            Stmt::Expr {
                expr: Expr::Call(
                    vm::BuiltinFunction::Get.call_index(),
                    Vec::new(),
                    vec![Expr::Var(0), Expr::Int(0)],
                ),
                line: 3,
            },
        ])
        .expect("compiler should preserve collection rebind evaluation order");

    let mut vm = Vm::new(program.with_local_count(2));
    assert_eq!(vm.run().expect("vm should run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::string("kv"), Value::Int(20)]);
}

#[test]
fn same_local_array_push_clears_target_immediately_before_call() {
    let mut compiler = Compiler::new();
    compiler.set_enable_local_move_semantics(true);
    let program = compiler
        .compile_program(&[
            Stmt::Let {
                index: 0,
                declared_schema: None,
                expr: Expr::Call(
                    vm::BuiltinFunction::ArrayNew.call_index(),
                    Vec::new(),
                    Vec::new(),
                ),
                line: 1,
            },
            Stmt::Assign {
                kind: vm::AssignmentKind::Set,
                index: 0,
                expr: Expr::Call(
                    vm::BuiltinFunction::ArrayPush.call_index(),
                    Vec::new(),
                    vec![Expr::Var(0), Expr::Int(7)],
                ),
                line: 2,
            },
            Stmt::Expr {
                expr: Expr::Var(0),
                line: 3,
            },
        ])
        .expect("compiler should emit array push program");
    let instructions = decode_instructions(&program.code);
    let call_position = instructions
        .iter()
        .position(|instruction| {
            instruction.op == OpCode::Call as u8
                && instruction.call_index == Some(vm::BuiltinFunction::ArrayPush.call_index())
                && instruction.call_arity == Some(2)
        })
        .expect("array rebind should emit array_push/2");

    let clear_const = instructions[call_position - 2];
    let clear_store = instructions[call_position - 1];
    let result_store = instructions[call_position + 1];
    assert!(matches!(
        clear_const
            .u32_operand
            .and_then(|index| program.constants.get(index as usize)),
        Some(Value::Null)
    ));
    assert_eq!(clear_store.op, OpCode::Stloc as u8);
    assert_eq!(clear_store.u8_operand, Some(0));
    assert_eq!(result_store.op, OpCode::Stloc as u8);
    assert_eq!(result_store.u8_operand, Some(0));

    let mut vm = Vm::new(program.with_local_count(1));
    assert_eq!(vm.run().expect("vm should run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::array(vec![Value::Int(7)])]);
}

#[test]
fn liveness_avoids_in_loop_null_clears_but_clears_after_loop_exit() {
    let source = r#"
        let mut iter = 0;
        let mut carry = 0;
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
fn ordinary_local_reads_compile_to_bare_ldloc() {
    let source = r#"
        let x = 1;
        let y = x + 1;
        let z = x;
        z;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let instructions = decode_instructions(&compiled.program.code);
    let ldloc_ips = instructions
        .iter()
        .filter(|instr| instr.op == OpCode::Ldloc as u8)
        .map(|instr| instr.ip)
        .collect::<Vec<_>>();
    assert!(
        !ldloc_ips.is_empty(),
        "expected at least one ldloc in compiled bytecode"
    );

    for window in instructions.windows(3) {
        let [first, second, third] = window else {
            continue;
        };
        let is_copy_pattern = first.op == OpCode::Ldloc as u8
            && second.op == OpCode::Dup as u8
            && third.op == OpCode::Stloc as u8
            && first.ip + first.width == second.ip
            && second.ip + second.width == third.ip;
        assert!(
            !is_copy_pattern,
            "ordinary reads should not lower to ldloc/dup/stloc: {:?}",
            window
        );
    }
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

    let err = match compile_source_file(main_path.as_path()) {
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
    assert_eq!(vm.stack(), &[Value::string("hello")]);
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
// ---------------------------------------------------------------------------
// Liveness / Availability - Additional Edge Cases
// ---------------------------------------------------------------------------

#[test]
fn nested_if_in_while_local_availability() {
    // A local declared inside an if-branch inside a while-loop should be
    // unavailable after the if/loop exits.
    let source = r#"
        let mut i = 0;
        while i < 3 {
            if i == 1 {
                let deep = "nested";
            }
            i = i + 1;
        }
        deep;
    "#;
    let err = match compile_source(source) {
        Ok(_) => panic!("using deep outside its scope should fail"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse) => {
            assert!(
                parse.message.contains("deep") && parse.message.contains("unavailable"),
                "expected 'unavailable' error for 'deep', got: {}",
                parse.message
            );
        }
        other => panic!("expected parse error, got {other:?}"),
    }
}

#[test]
fn else_if_branch_local_is_unavailable_after_merge() {
    // Variable declared only in the `else` branch of an `if/else` chain
    // should be unavailable after the if/else merge.
    let source = r#"
        let cond = true;
        if cond {
            let x = 1;
        } else {
            let only_else = 99;
        }
        only_else;
    "#;
    let err = match compile_source(source) {
        Ok(_) => panic!("only_else should be unavailable"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse) => {
            assert!(
                parse.message.contains("only_else") && parse.message.contains("unavailable"),
                "expected 'unavailable' for 'only_else', got: {}",
                parse.message
            );
        }
        other => panic!("expected parse error, got {other:?}"),
    }
}

#[test]
fn local_declared_in_both_branches_is_available_after_merge() {
    // If a local is declared in BOTH branches with the same name, it shadows
    // the outer binding. Verify execution still works correctly.
    let source = r#"
        let cond = true;
        let mut val = 0;
        if cond {
            let inner = 10;
        } else {
            let inner = 20;
        }
        val;
    "#;
    // This should compile and run - val in the outer scope is still 0.
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(0)]);
}

#[test]
fn liveness_clears_dead_locals_in_nested_control_flow() {
    // Locals that die at different nesting depths should all be Null after halt.
    let source = r#"
        let outer = { tag: "outer" };
        let mut i = 0;
        while i < 2 {
            let inner = { tag: "inner" };
            if i == 0 {
                let deep = { tag: "deep" };
            }
            i = i + 1;
        }
        i;
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("debug info should exist");
    let outer_idx = debug.local_index("outer").expect("outer should exist");
    let inner_idx = debug.local_index("inner").expect("inner should exist");

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(2)]);
    assert_eq!(
        vm.locals()[outer_idx as usize],
        Value::Null,
        "'outer' should be Null - dead before result expression"
    );
    assert_eq!(
        vm.locals()[inner_idx as usize],
        Value::Null,
        "'inner' should be Null after loop exit"
    );
}

#[test]
fn for_loop_variable_is_null_after_last_use() {
    // The for-loop induction variable and a loop-body temporary should be
    // cleared after the loop when they are no longer live.
    let source = r#"
        let mut sum = 0;
        let mut i = 0;
        while i < 4 {
            let tmp = i * 2;
            sum = sum + tmp;
            i = i + 1;
        }
        sum;
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("debug info should exist");
    let tmp_idx = debug.local_index("tmp").expect("tmp should exist");

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    // sum = 0 + 2 + 4 + 6 = 12
    assert_eq!(vm.stack(), &[Value::Int(12)]);
    assert_eq!(
        vm.locals()[tmp_idx as usize],
        Value::Null,
        "'tmp' should be Null after loop exit"
    );
}

#[test]
fn stack_is_clean_after_halt_with_single_result() {
    // After a program halts, the stack should contain exactly the final
    // expression value and nothing else - no stale temporaries.
    let source = r#"
        let a = 1 + 2;
        let b = a * 3;
        let c = b - 1;
        c;
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack().len(),
        1,
        "stack should contain exactly one result value, got {:?}",
        vm.stack()
    );
    assert_eq!(vm.stack(), &[Value::Int(8)]);
}
// NOTE: function parameter slot cleanup is already covered by
// `inline_function_call_frame_slots_are_cleared_after_return` in
// compiler_rustscript_tests.rs (which asserts ALL locals are Null).
