use pd_vm_nostd::{Value as EmbeddedValue, WireError, decode_program};
use vm::{HostImport, OpCode, Program, Value, ValueType, compile_source, encode_program};

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
fn embedded_decoder_reads_host_generated_v8() {
    let bytes = encoded_scalar_program();
    let program = decode_program(&bytes).expect("embedded decoder should accept VMBC v8");

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
