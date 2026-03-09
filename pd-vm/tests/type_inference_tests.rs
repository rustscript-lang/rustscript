#![cfg(feature = "runtime")]

use vm::{OpCode, Value, ValueType, Vm, VmStatus, compile_source};

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
    let type_map = compiled
        .program
        .type_map
        .as_ref()
        .expect("compiled program should include type metadata");
    let add_offsets = opcode_offsets(&compiled.program.code, OpCode::Add);

    assert_eq!(
        add_offsets.len(),
        3,
        "expected exactly three add instructions"
    );
    assert_eq!(
        type_map.operand_types.get(&add_offsets[0]),
        Some(&(ValueType::Int, ValueType::Int))
    );
    assert_eq!(
        type_map.operand_types.get(&add_offsets[1]),
        Some(&(ValueType::Float, ValueType::Float))
    );
    assert_eq!(
        type_map.operand_types.get(&add_offsets[2]),
        Some(&(ValueType::String, ValueType::String))
    );
    assert!(
        !type_map.local_types.is_empty(),
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
