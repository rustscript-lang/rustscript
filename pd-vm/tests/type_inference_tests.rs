#![cfg(feature = "runtime")]

use vm::{CompiledProgram, OpCode, TypeMap, Value, ValueType, Vm, VmStatus, compile_source};

fn opcode_offsets(code: &[u8], opcode: OpCode) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut ip = 0usize;
    while let Some(&raw) = code.get(ip) {
        let start = ip;
        ip += 1;
        let Ok(decoded) = OpCode::try_from(raw) else {
            break;
        };
        if decoded == opcode {
            offsets.push(start);
        }
        ip = ip.saturating_add(decoded.operand_len());
    }
    offsets
}

fn compiled_type_map(compiled: &CompiledProgram) -> &TypeMap {
    compiled
        .program
        .type_map
        .as_ref()
        .expect("compiled program should include type metadata")
}

fn assert_opcode_operand_types(
    compiled: &CompiledProgram,
    opcode: OpCode,
    expected: &[(ValueType, ValueType)],
) {
    let type_map = compiled_type_map(compiled);
    let offsets = opcode_offsets(&compiled.program.code, opcode);

    assert_eq!(
        offsets.len(),
        expected.len(),
        "unexpected {opcode:?} count in bytecode"
    );
    for (offset, expected_types) in offsets.into_iter().zip(expected.iter().copied()) {
        assert_eq!(
            type_map.operand_types.get(&offset),
            Some(&expected_types),
            "unexpected operand type metadata at bytecode offset {offset}"
        );
    }
}

fn assert_last_opcode_operand_types(
    compiled: &CompiledProgram,
    opcode: OpCode,
    expected: (ValueType, ValueType),
) {
    let type_map = compiled_type_map(compiled);
    let offset = *opcode_offsets(&compiled.program.code, opcode)
        .last()
        .expect("expected opcode in bytecode");
    assert_eq!(
        type_map.operand_types.get(&offset),
        Some(&expected),
        "unexpected operand type metadata at bytecode offset {offset}"
    );
}

fn assert_last_opcode_has_no_operand_types(compiled: &CompiledProgram, opcode: OpCode) {
    let type_map = compiled_type_map(compiled);
    let offset = *opcode_offsets(&compiled.program.code, opcode)
        .last()
        .expect("expected opcode in bytecode");
    assert!(
        !type_map.operand_types.contains_key(&offset),
        "expected no operand type metadata at bytecode offset {offset}"
    );
}

fn assert_opcode_operand_types_present(
    compiled: &CompiledProgram,
    opcode: OpCode,
    expected: &[(ValueType, ValueType)],
) {
    let type_map = compiled_type_map(compiled);
    let mut actual = opcode_offsets(&compiled.program.code, opcode)
        .into_iter()
        .filter_map(|offset| type_map.operand_types.get(&offset).copied())
        .collect::<Vec<_>>();

    for expected_types in expected {
        let index = actual
            .iter()
            .position(|actual_types| actual_types == expected_types)
            .expect("expected operand type metadata was not emitted");
        actual.swap_remove(index);
    }
}

#[test]
fn compiler_attaches_known_operand_types_to_programs() {
    let source = r#"
        let x = 2 + 3;
        let y = 1.5 + 2.5;
        let z = "a" + "b";
        x;
        y;
        z;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    assert_opcode_operand_types(
        &compiled,
        OpCode::Add,
        &[
            (ValueType::Int, ValueType::Int),
            (ValueType::Float, ValueType::Float),
            (ValueType::String, ValueType::String),
        ],
    );
    assert!(
        !compiled_type_map(&compiled).local_types.is_empty(),
        "type metadata should include local slot entries"
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::Int(5), Value::Float(4.0), Value::string("ab")]
    );
}

#[test]
fn compiler_rejects_mixed_if_else_branch_types() {
    let source = r#"
        let mut value = 1;
        if true {
            value = 2;
        } else {
            value = "x";
        }
        value + 1;
    "#;

    let err = match compile_source(source) {
        Ok(_) => panic!("compile should reject mixed branch types"),
        Err(err) => err,
    };
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("IfElseBranchTypeMismatch"),
        "unexpected error: {rendered}"
    );
    assert!(
        rendered.contains("int") && rendered.contains("string"),
        "expected concrete type names in error: {rendered}"
    );
}

#[test]
fn compiler_rejects_shadowed_if_else_branch_mismatch() {
    let source = r#"
        let total = 1;
        let total = if true => {
            "222"
        } else => {
            let bumped = total + 1;
            bumped
        };
        total;
    "#;

    let err = match compile_source(source) {
        Ok(_) => panic!("compile should reject shadowed mixed branch types"),
        Err(err) => err,
    };
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("IfElseBranchTypeMismatch"),
        "unexpected error: {rendered}"
    );
    assert!(
        rendered.contains("string") && rendered.contains("int"),
        "expected concrete type names in error: {rendered}"
    );
}

#[test]
fn compiler_rejects_shadowed_if_else_branch_mismatch_after_loop() {
    let source = r#"
        let mut total = 0;
        for (let mut i = 0; i < 4; i = i + 1) {
            total = total + i;
        }

        let total = if true => {
            "222"
        } else => {
            let bumped = total + 1;
            bumped
        };
        total;
    "#;

    let err = match compile_source(source) {
        Ok(_) => panic!("compile should reject loop-carried shadowed mixed branch types"),
        Err(err) => err,
    };
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("IfElseBranchTypeMismatch"),
        "unexpected error: {rendered}"
    );
    assert!(
        rendered.contains("string") && rendered.contains("int"),
        "expected concrete type names in error: {rendered}"
    );
}

#[test]
fn compiler_propagates_callable_return_types_through_functions_and_closures() {
    let source = r#"
        fn add_one(value) {
            value + 1;
        }

        fn apply_twice(func, value) {
            let once = func(value);
            func(once);
        }

        let named = add_one;
        let inc = |x| x + 1;
        let direct = add_one(40) + 1;
        let via_named_local = named(40) + 1;
        let via_closure_local = inc(40) + 1;
        let via_named_param = apply_twice(named, 40) + 1;
        let via_closure_param = apply_twice(inc, 40) + 1;
        direct;
        via_named_local;
        via_closure_local;
        via_named_param;
        via_closure_param;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    assert_opcode_operand_types(
        &compiled,
        OpCode::Add,
        &[
            (ValueType::Int, ValueType::Int),
            (ValueType::Int, ValueType::Int),
            (ValueType::Int, ValueType::Int),
            (ValueType::Int, ValueType::Int),
            (ValueType::Int, ValueType::Int),
            (ValueType::Int, ValueType::Int),
            (ValueType::Int, ValueType::Int),
            (ValueType::Int, ValueType::Int),
            (ValueType::Int, ValueType::Int),
            (ValueType::Int, ValueType::Int),
            (ValueType::Int, ValueType::Int),
            (ValueType::Int, ValueType::Int),
        ],
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[
            Value::Int(42),
            Value::Int(42),
            Value::Int(42),
            Value::Int(43),
            Value::Int(43),
        ]
    );
}

#[test]
fn compiler_marks_string_plus_number_paths_as_string_concat() {
    let source = r#"
        fn label(value) {
            "v=" + value;
        }

        let number = 123;
        let formatter = |value| value + "!";
        let a = "text" + 123;
        let b = "text" + number;
        let c = label(number);
        let d = formatter(number);
        let joined = c + d;
        a;
        b;
        joined;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    assert_opcode_operand_types(
        &compiled,
        OpCode::Add,
        &[
            (ValueType::String, ValueType::String),
            (ValueType::String, ValueType::String),
            (ValueType::String, ValueType::String),
            (ValueType::String, ValueType::String),
            (ValueType::String, ValueType::String),
        ],
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[
            Value::string("text123"),
            Value::string("text123"),
            Value::string("v=123123!"),
        ]
    );
}

#[test]
fn compiler_infers_named_function_plus_operands_from_consistent_calls() {
    let source = r#"
        fn addme(x) {
            x + x
        }

        addme(21);
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    assert_opcode_operand_types(&compiled, OpCode::Add, &[(ValueType::Int, ValueType::Int)]);

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn compiler_rejects_conflicting_named_function_plus_operand_flows() {
    let source = r#"
        fn addme(x) {
            x + x
        }

        addme(1);
        addme("as");
    "#;

    let err = match compile_source(source) {
        Ok(_) => panic!("compile should reject conflicting function operand types"),
        Err(err) => err,
    };
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("FunctionParameterTypeConflict"),
        "unexpected error: {rendered}"
    );
    assert!(
        rendered.contains("addme") && rendered.contains("int") && rendered.contains("string"),
        "expected function/type details in error: {rendered}"
    );
}

#[test]
fn compiler_allows_unused_named_function_with_unobserved_plus_operands() {
    let source = r#"
        fn addme(x) {
            x + x
        }

        42;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn compiler_preserves_stable_for_loop_counter_types_after_loop() {
    let source = r#"
        let mut total = 0;
        for (let mut i = 0; i < 4; i = i + 1) {
            total = total + i;
        }
        let after = total + 1;
        after;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    assert_last_opcode_operand_types(&compiled, OpCode::Add, (ValueType::Int, ValueType::Int));

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7)]);
}

#[test]
fn compiler_preserves_stable_while_float_types_after_loop() {
    let source = r#"
        let mut total = 1.5;
        let mut remaining = 2;
        while remaining > 0 {
            total = total + 0.5;
            remaining = remaining - 1;
        }
        let after = total + 1.0;
        after;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    assert_last_opcode_operand_types(&compiled, OpCode::Add, (ValueType::Float, ValueType::Float));

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Float(3.5)]);
}

#[test]
fn compiler_drops_unstable_loop_types_after_conflicting_assignments() {
    let source = r#"
        let mut value = 0;
        let values = ["x", 1];
        for (let mut i = 0; i < 2; i = i + 1) {
            value = values[i].copy();
        }
        value + 1;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    assert_last_opcode_has_no_operand_types(&compiled, OpCode::Add);
}

#[test]
fn compiler_preserves_outer_loop_types_across_nested_loops() {
    let source = r#"
        let mut total = 0;
        for (let mut i = 0; i < 2; i = i + 1) {
            for (let mut j = 0; j < 2; j = j + 1) {
                total = total + i + j;
            }
        }
        total + 1;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    assert_last_opcode_operand_types(&compiled, OpCode::Add, (ValueType::Int, ValueType::Int));
}

#[test]
fn compiler_infers_homogeneous_container_get_results() {
    let source = r#"
        let array = [1, 2, 3];
        let map = {"a": 4, "b": 5};
        let keys = [10, 20].keys;
        let array_value = array[0] + 3;
        let map_value = map["a"] + 2;
        let key_value = keys[0] + 1;
        array_value;
        map_value;
        key_value;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    assert_opcode_operand_types(
        &compiled,
        OpCode::Add,
        &[
            (ValueType::Int, ValueType::Int),
            (ValueType::Int, ValueType::Int),
            (ValueType::Int, ValueType::Int),
        ],
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4), Value::Int(6), Value::Int(1)]);
}

#[test]
fn compiler_propagates_explicit_host_return_type_signatures() {
    let source = r#"
        fn add_one(x) -> int;
        let value = add_one(41);
        value + 1;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    assert_last_opcode_operand_types(&compiled, OpCode::Add, (ValueType::Int, ValueType::Int));

    let mut vm = Vm::new(compiled.program);
    vm.bind_static_function("add_one", |_vm, args| {
        let value = match args.first() {
            Some(Value::Int(value)) => *value,
            other => panic!("unexpected args: {other:?}"),
        };
        Ok(vm::CallOutcome::Return(vec![Value::Int(value + 1)]))
    });
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(43)]);
}

#[test]
fn compiler_uses_edge_host_namespace_return_signatures() {
    let source = r#"
        use runtime;
        let slept = runtime::sleep(1);
        slept == true;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    assert_last_opcode_operand_types(&compiled, OpCode::Ceq, (ValueType::Bool, ValueType::Bool));

    let mut vm = Vm::new(compiled.program);
    vm.bind_static_function("runtime::sleep", |_vm, _args| {
        Ok(vm::CallOutcome::Return(vec![Value::Bool(true)]))
    });
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(true)]);
}

#[test]
fn compiler_uses_generated_builtin_namespace_return_signatures() {
    let source = r#"
        use json;
        use re;
        use jit;
        use math;
        let encoded = json::encode({"a": 1});
        let matched = re::match("a", "a");
        let enabled = jit::set_enabled(false);
        let pi = math::pi();
        encoded + "!";
        matched == true;
        enabled == false;
        pi + 1.0;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    assert_opcode_operand_types(
        &compiled,
        OpCode::Add,
        &[
            (ValueType::String, ValueType::String),
            (ValueType::Float, ValueType::Float),
        ],
    );
    assert_opcode_operand_types_present(
        &compiled,
        OpCode::Ceq,
        &[
            (ValueType::Bool, ValueType::Bool),
            (ValueType::Bool, ValueType::Bool),
        ],
    );
}
