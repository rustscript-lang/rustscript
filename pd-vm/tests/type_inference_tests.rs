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
