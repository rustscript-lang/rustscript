#![cfg(feature = "runtime")]
use std::collections::HashMap;

use vm::{
    ArgInfo, Assembler, BytecodeBuilder, DebugFunction, DebugInfo, DisassembleOptions, HostImport,
    LineInfo, LocalInfo, Program, TypeMap, ValidationError, Value, ValueType, WireError,
    decode_program, disassemble_vmbc, disassemble_vmbc_with_options, encode_program,
    infer_local_count, validate_program,
};

#[test]
fn wire_roundtrip_preserves_constants_and_code() {
    let mut operand_types = HashMap::new();
    operand_types.insert(0usize, (ValueType::Int, ValueType::Int));
    let program = Program::with_imports_and_debug(
        vec![
            Value::Int(42),
            Value::Float(3.5),
            Value::Bool(true),
            Value::string("hello"),
        ],
        vec![0x00, 0x01, 0x02],
        vec![HostImport {
            name: "print".to_string(),
            arity: 1,
        }],
        Some(DebugInfo {
            source: Some("fn a(x);\na(1);".to_string()),
            lines: vec![
                LineInfo { offset: 0, line: 1 },
                LineInfo { offset: 1, line: 2 },
            ],
            functions: vec![DebugFunction {
                name: "a".to_string(),
                args: vec![ArgInfo {
                    name: "x".to_string(),
                    position: 0,
                }],
            }],
            locals: vec![LocalInfo {
                name: "v".to_string(),
                index: 0,
                declared_line: None,
                last_line: None,
            }],
        }),
    )
    .with_type_map(TypeMap {
        local_types: vec![ValueType::Int, ValueType::Unknown],
        operand_types,
    });

    let encoded = encode_program(&program).expect("encode should succeed");
    let decoded = decode_program(&encoded).expect("decode should succeed");

    assert_eq!(decoded.constants, program.constants);
    assert_eq!(decoded.code, program.code);
    assert_eq!(decoded.imports, program.imports);
    assert_eq!(decoded.debug, program.debug);
    assert_eq!(decoded.type_map, program.type_map);
}

#[test]
fn decode_rejects_invalid_magic_version_and_truncation() {
    let program = Program::new(vec![Value::Int(7)], vec![0x01]);
    let encoded = encode_program(&program).expect("encode should succeed");

    let mut bad_magic = encoded.clone();
    bad_magic[0..4].copy_from_slice(b"NOPE");
    assert!(matches!(
        decode_program(&bad_magic),
        Err(WireError::InvalidMagic(_))
    ));

    let mut bad_version = encoded.clone();
    bad_version[4..6].copy_from_slice(&99u16.to_le_bytes());
    assert!(matches!(
        decode_program(&bad_version),
        Err(WireError::UnsupportedVersion(99))
    ));

    let truncated = &encoded[..encoded.len() - 1];
    assert!(matches!(
        decode_program(truncated),
        Err(WireError::UnexpectedEof)
    ));
}

#[test]
fn validate_rejects_invalid_const_call_jump_and_opcode() {
    let bad_const = Program::new(vec![Value::Int(1)], vec![0x02, 0x01, 0x00, 0x00, 0x00]);
    assert!(matches!(
        validate_program(&bad_const, 4),
        Err(ValidationError::InvalidConstant { .. })
    ));

    let bad_call = Program::new(vec![], vec![0x11, 0x05, 0x00, 0x00]);
    assert!(matches!(
        validate_program(&bad_call, 4),
        Err(ValidationError::InvalidCall { index: 5, .. })
    ));

    let bad_jump = Program::new(vec![], vec![0x0B, 0xFF, 0x00, 0x00, 0x00]);
    assert!(matches!(
        validate_program(&bad_jump, 4),
        Err(ValidationError::InvalidJumpTarget { .. })
    ));

    let bad_opcode = Program::new(vec![], vec![0xFF]);
    assert!(matches!(
        validate_program(&bad_opcode, 4),
        Err(ValidationError::InvalidOpcode { opcode: 0xFF, .. })
    ));
}

#[test]
fn validate_accepts_known_good_program() {
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.call(0, 1);
    bc.ret();

    let program = Program::with_imports_and_debug(
        vec![Value::string("x")],
        bc.finish(),
        vec![HostImport {
            name: "print".to_string(),
            arity: 1,
        }],
        None,
    );
    validate_program(&program, 4).expect("program should validate");
}

#[test]
fn validate_rejects_invalid_call_arity_for_import() {
    let mut bc = BytecodeBuilder::new();
    bc.call(0, 2);
    bc.ret();

    let program = Program::with_imports_and_debug(
        vec![],
        bc.finish(),
        vec![HostImport {
            name: "print".to_string(),
            arity: 1,
        }],
        None,
    );
    assert!(matches!(
        validate_program(&program, 4),
        Err(ValidationError::InvalidCallArity {
            index: 0,
            expected: 1,
            got: 2,
            ..
        })
    ));
}

#[test]
fn infer_local_count_finds_highest_local_index() {
    let mut bc = BytecodeBuilder::new();
    bc.ldloc(3);
    bc.stloc(7);
    bc.ret();

    let program = Program::new(vec![], bc.finish());
    let locals = infer_local_count(&program).expect("infer should succeed");
    assert_eq!(locals, 8);
}

#[test]
fn disassemble_vmbc_outputs_readable_listing() {
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.call(0, 1);
    bc.ret();
    let program = Program::with_imports_and_debug(
        vec![Value::string("x")],
        bc.finish(),
        vec![HostImport {
            name: "print".to_string(),
            arity: 1,
        }],
        None,
    );
    let bytes = encode_program(&program).expect("encode should succeed");

    let listing = disassemble_vmbc(&bytes).expect("disassembly should succeed");

    assert!(listing.contains("constants (1):"));
    assert!(listing.contains("[0000] String(\"x\")"));
    assert!(listing.contains("imports (1):"));
    assert!(listing.contains("[0000] print/1"));
    assert!(listing.contains("ldc 0 ; const[0]=String(\"x\")"));
    assert!(listing.contains("call 0 1 ; import print/1"));
    assert!(listing.contains("ret"));
}

#[test]
fn disassemble_vmbc_can_include_embedded_source() {
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.stloc(0);
    bc.ldloc(0);
    bc.ret();
    let program = Program::with_imports_and_debug(
        vec![Value::Int(1)],
        bc.finish(),
        vec![],
        Some(DebugInfo {
            source: Some("let x = 1;\nx;".to_string()),
            lines: vec![
                LineInfo { offset: 0, line: 1 },
                LineInfo { offset: 5, line: 1 },
                LineInfo { offset: 7, line: 2 },
            ],
            functions: vec![],
            locals: vec![],
        }),
    );
    let bytes = encode_program(&program).expect("encode should succeed");

    let listing = disassemble_vmbc_with_options(&bytes, DisassembleOptions { show_source: true })
        .expect("disassembly should succeed");

    let src1 = listing
        .find("; src 0001  let x = 1;")
        .expect("line 1 source marker");
    let op1 = listing.find("0000\t02 00 00 00 00").expect("line 1 opcode");
    let src2 = listing
        .find("; src 0002  x;")
        .expect("line 2 source marker");
    let op2 = listing.find("0007\t0F 00").expect("line 2 opcode");
    assert!(src1 < op1);
    assert!(src2 < op2);
}

#[test]
fn disassemble_vmbc_hides_source_without_flag() {
    let mut bc = BytecodeBuilder::new();
    bc.ret();
    let program = Program::with_imports_and_debug(
        vec![],
        bc.finish(),
        vec![],
        Some(DebugInfo {
            source: Some("let x = 1;\nx;".to_string()),
            lines: vec![],
            functions: vec![],
            locals: vec![],
        }),
    );
    let bytes = encode_program(&program).expect("encode should succeed");

    let listing = disassemble_vmbc(&bytes).expect("disassembly should succeed");

    assert!(!listing.contains("source:"));
    assert!(!listing.contains("let x = 1;"));
}

#[test]
fn assembler_deduplicates_equal_string_constants() {
    let mut asm = Assembler::new();
    let idx0 = asm.add_constant(Value::string("same"));
    let idx1 = asm.add_constant(Value::string("same"));
    assert_eq!(idx0, idx1);
    asm.ldc(idx0);
    asm.ldc(idx1);
    asm.ret();

    let program = asm.finish_program().expect("assembler should finish");
    assert_eq!(program.constants, vec![Value::string("same")]);
}

#[test]
fn assembler_deduplicates_equal_scalar_constants() {
    let mut asm = Assembler::new();
    let int0 = asm.add_constant(Value::Int(7));
    let int1 = asm.add_constant(Value::Int(7));
    let bool0 = asm.add_constant(Value::Bool(true));
    let bool1 = asm.add_constant(Value::Bool(true));
    let float0 = asm.add_constant(Value::Float(3.5));
    let float1 = asm.add_constant(Value::Float(3.5));

    assert_eq!(int0, int1);
    assert_eq!(bool0, bool1);
    assert_eq!(float0, float1);

    asm.ldc(int0);
    asm.ldc(bool0);
    asm.ldc(float0);
    asm.ret();

    let program = asm.finish_program().expect("assembler should finish");
    assert_eq!(
        program.constants,
        vec![Value::Int(7), Value::Bool(true), Value::Float(3.5)]
    );
}
