use pd_vm_nostd::{
    OpCode as EmbeddedOpCode, Value as EmbeddedValue, Vm as EmbeddedVm,
    VmStatus as EmbeddedVmStatus, WireError, decode_program,
};
use vm::compiler::TypeSchema;
use vm::{
    HostImport, OpCode, Program, ReplLocalBinding, Value, ValueType, compile_source,
    compile_source_for_repl, compile_source_for_repl_with_locals, encode_program,
};

fn encoded_scalar_program() -> Vec<u8> {
    let mut program = Program::new(
        vec![
            Value::Null,
            Value::Int(40),
            Value::Float(2.5),
            Value::Bool(true),
            Value::string("pico"),
            Value::bytes([0x52, 0x53, 0x53]),
        ],
        vec![OpCode::Ldc as u8, 1, 0, 0, 0, OpCode::Ret as u8],
    );
    program.imports.push(HostImport {
        name: "serial::write".to_string(),
        arity: 1,
        return_type: ValueType::Null,
    });
    encode_program(&program).expect("std VMBC encoder should succeed")
}

#[test]
fn embedded_decoder_reads_host_generated_v10() {
    let bytes = encoded_scalar_program();
    let program = decode_program(&bytes).expect("embedded decoder should accept VMBC v10");

    assert_eq!(
        program.code(),
        &[OpCode::Ldc as u8, 1, 0, 0, 0, OpCode::Ret as u8]
    );
    assert_eq!(program.local_count(), 0);
    assert_eq!(program.constants()[0], EmbeddedValue::Null);
    assert_eq!(program.constants()[1], EmbeddedValue::Int(40));
    assert_eq!(program.constants()[2], EmbeddedValue::Float(2.5));
    assert_eq!(program.constants()[3], EmbeddedValue::Bool(true));
    assert_eq!(program.constants()[4], EmbeddedValue::string("pico"));
    assert_eq!(
        program.constants()[5],
        EmbeddedValue::bytes([0x52, 0x53, 0x53])
    );
    assert_eq!(program.imports().len(), 1);
    assert_eq!(program.imports()[0].name, "serial::write");
    assert_eq!(program.imports()[0].arity, 1);
}

#[test]
fn embedded_decoder_reads_nested_container_constants() {
    let source = Program::new(
        vec![Value::array(vec![
            Value::Int(1),
            Value::map(vec![(Value::string("key"), Value::Bool(true))]),
        ])],
        vec![OpCode::Ret as u8],
    );
    let bytes = encode_program(&source).expect("nested constants should encode");
    let program = decode_program(&bytes).expect("nested constants should decode");
    assert_eq!(
        program.constants()[0],
        EmbeddedValue::array(vec![
            EmbeddedValue::Int(1),
            EmbeddedValue::map(vec![(
                EmbeddedValue::string("key"),
                EmbeddedValue::Bool(true),
            )]),
        ])
    );
}

#[test]
fn embedded_decoder_accepts_compiler_type_and_debug_metadata() {
    let compiled = compile_source("let mut x = 40; x = x + 2; print(x);")
        .expect("RustScript source should compile");
    let bytes = encode_program(&compiled.program.with_local_count(compiled.locals))
        .expect("compiler output should encode");

    let program = decode_program(&bytes).expect("embedded decoder should skip std-only metadata");
    assert!(!program.code().is_empty());
    assert_eq!(program.local_count(), compiled.locals);
}

#[test]
fn embedded_decoder_preserves_exported_callable_names() {
    let compiled = compile_source_for_repl("pub fn answer() -> int { 42 }")
        .expect("exported function should compile");
    let bytes = encode_program(&compiled.program.with_local_count(compiled.locals))
        .expect("exported program should encode");
    let program = decode_program(&bytes).expect("embedded decoder should preserve exports");
    assert_eq!(program.exported_callables().len(), 1);
    assert_eq!(program.exported_callables()[0].name, "answer");
    assert!(
        program
            .root_callable_bindings()
            .iter()
            .any(|binding| binding.local_slot == program.exported_callables()[0].local_slot)
    );
    let mut vm = EmbeddedVm::new(program);
    assert!(matches!(
        vm.resolve_exported_callable("answer"),
        Some(EmbeddedValue::Callable(_))
    ));
    assert_eq!(
        vm.run().expect("embedded root should halt"),
        EmbeddedVmStatus::Halted
    );
    assert!(matches!(
        vm.resolve_exported_callable("answer"),
        Some(EmbeddedValue::Callable(_))
    ));
    assert_eq!(vm.resolve_exported_callable("missing"), None);
}

#[test]
fn embedded_decoder_preserves_metadata_only_repl_locals() {
    let compiled = compile_source_for_repl_with_locals(
        "print(42);",
        &[ReplLocalBinding {
            name: "saved".to_string(),
            mutable: false,
            schema: Some(TypeSchema::Int),
            optional: false,
        }],
    )
    .expect("REPL source should compile");
    let bytes = encode_program(
        &compiled
            .compiled
            .program
            .with_local_count(compiled.compiled.locals),
    )
    .expect("REPL output should encode");

    let program = decode_program(&bytes).expect("embedded decoder should accept REPL VMBC");
    assert_eq!(program.local_count(), compiled.compiled.locals);
    assert_eq!(program.local_count(), 1);
}

#[test]
fn embedded_decoder_rejects_trailing_bytes() {
    let mut bytes = encoded_scalar_program();
    bytes.push(0xff);

    assert_eq!(decode_program(&bytes), Err(WireError::TrailingBytes));
}

#[test]
fn embedded_decoder_rejects_invalid_magic() {
    let mut bytes = encoded_scalar_program();
    bytes[0] = b'X';

    assert!(matches!(
        decode_program(&bytes),
        Err(WireError::InvalidMagic(_))
    ));
}

#[test]
fn embedded_runtime_executes_compiler_generated_capturing_callable() {
    let compiled = compile_source_for_repl(
        r#"
            let base = 40;
            let add = |value| value + base;
            add(2);
        "#,
    )
    .expect("capturing callable source should compile");
    let bytes = encode_program(&compiled.program.with_local_count(compiled.locals))
        .expect("compiler output should encode");
    let program = decode_program(&bytes).expect("embedded decoder should accept callable VMBC");
    let mut runtime = EmbeddedVm::new(program);

    assert_eq!(runtime.run(), Ok(EmbeddedVmStatus::Halted));
    assert_eq!(runtime.stack(), &[EmbeddedValue::Int(42)]);
}

#[test]
fn removed_callable_creation_opcode_is_rejected() {
    assert!(OpCode::try_from(0x1a).is_err());
    assert!(EmbeddedOpCode::try_from(0x1a).is_err());
}
