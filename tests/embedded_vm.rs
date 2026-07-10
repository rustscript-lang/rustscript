use vm::embedded::{Value as EmbeddedValue, Vm as EmbeddedVm, VmError, VmStatus, decode_program};
use vm::{OpCode, Program, Value, compile_source_for_repl, encode_program};

fn compile_embedded(source: &str) -> vm::embedded::Program {
    let compiled = compile_source_for_repl(source).expect("RustScript source should compile");
    let bytes = encode_program(&compiled.program.with_local_count(compiled.locals))
        .expect("compiled program should encode");
    decode_program(&bytes).expect("embedded runtime should decode compiler VMBC")
}

fn run_source(source: &str) -> Result<EmbeddedValue, VmError> {
    let mut vm = EmbeddedVm::new(compile_embedded(source));
    assert_eq!(vm.run()?, VmStatus::Halted);
    vm.stack().last().cloned().ok_or(VmError::StackUnderflow)
}

fn decode_direct(constants: Vec<Value>, code: Vec<u8>) -> vm::embedded::Program {
    let bytes = encode_program(&Program::new(constants, code)).expect("program should encode");
    decode_program(&bytes).expect("program should decode")
}

#[test]
fn arithmetic_and_boolean_opcodes_match_compiler_output() {
    assert_eq!(run_source("1 + 2 * 3;"), Ok(EmbeddedValue::Int(7)));
    assert_eq!(run_source("17 % 5;"), Ok(EmbeddedValue::Int(2)));
    assert_eq!(run_source("!(1 < 0);"), Ok(EmbeddedValue::Bool(true)));
}

#[test]
fn direct_stack_and_logical_shift_opcodes_execute() {
    let mut string_vm = EmbeddedVm::new(decode_direct(
        vec![Value::string("rust"), Value::string("script")],
        vec![
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Ldc as u8,
            1,
            0,
            0,
            0,
            OpCode::Add as u8,
            OpCode::Ret as u8,
        ],
    ));
    assert_eq!(string_vm.run(), Ok(VmStatus::Halted));
    assert_eq!(string_vm.stack(), &[EmbeddedValue::string("rustscript")]);

    let shift_code = vec![
        OpCode::Ldc as u8,
        0,
        0,
        0,
        0,
        OpCode::Ldc as u8,
        1,
        0,
        0,
        0,
        OpCode::Shl as u8,
        OpCode::Ldc as u8,
        2,
        0,
        0,
        0,
        OpCode::Ldc as u8,
        3,
        0,
        0,
        0,
        OpCode::Shr as u8,
        OpCode::Add as u8,
        OpCode::Ret as u8,
    ];
    let mut shift_vm = EmbeddedVm::new(decode_direct(
        vec![Value::Int(8), Value::Int(2), Value::Int(32), Value::Int(1)],
        shift_code,
    ));
    assert_eq!(shift_vm.run(), Ok(VmStatus::Halted));
    assert_eq!(shift_vm.stack(), &[EmbeddedValue::Int(48)]);

    let code = vec![
        OpCode::Ldc as u8,
        0,
        0,
        0,
        0,
        OpCode::Dup as u8,
        OpCode::Pop as u8,
        OpCode::Ldc as u8,
        1,
        0,
        0,
        0,
        OpCode::Lshr as u8,
        OpCode::Ret as u8,
    ];
    let mut vm = EmbeddedVm::new(decode_direct(vec![Value::Int(-1), Value::Int(1)], code));

    assert_eq!(vm.run(), Ok(VmStatus::Halted));
    assert_eq!(vm.stack(), &[EmbeddedValue::Int(i64::MAX)]);
}

#[test]
fn branches_and_locals_execute_compiler_loop() {
    let source = r#"
        let mut total = 0;
        let mut n = 1;
        while n < 5 {
            total = total + n;
            n = n + 1;
        }
        total;
    "#;

    assert_eq!(run_source(source), Ok(EmbeddedValue::Int(10)));
}

#[test]
fn division_by_zero_is_reported() {
    assert_eq!(run_source("1 / 0;"), Err(VmError::DivisionByZero));
}

#[test]
fn invalid_jump_and_local_are_reported() {
    let mut jump_vm = EmbeddedVm::new(decode_direct(
        vec![],
        vec![OpCode::Br as u8, 0xff, 0xff, 0xff, 0x7f],
    ));
    assert_eq!(jump_vm.run(), Err(VmError::InvalidJump(0x7fff_ffff)));

    let mut local_vm = EmbeddedVm::new(decode_direct(vec![], vec![OpCode::Ret as u8]));
    assert_eq!(
        local_vm.set_local(9, EmbeddedValue::Int(1)),
        Err(VmError::InvalidLocal(9))
    );
}
