#![cfg(feature = "runtime")]

use vm::{
    Assembler, AssemblerError, DebugInfo, DisassembleOptions, LineInfo, OpCode, Program,
    ValidationError, Value, WireError, assemble, decode_program, disassemble_program_with_options,
    encode_program, validate_program,
};

fn assert_asm_error_contains(source: &str, expected: &str) {
    let err = assemble(source).expect_err("assembly should fail");
    assert!(
        err.message.contains(expected),
        "expected assembler error containing '{expected}', got '{:?}'",
        err
    );
}

#[test]
fn assembler_api_reports_duplicate_and_unknown_labels() {
    let mut asm = Assembler::new();
    asm.label("entry").expect("first label should succeed");
    let duplicate = asm.label("entry").expect_err("duplicate label should fail");
    assert!(matches!(duplicate, AssemblerError::DuplicateLabel(_)));

    let mut asm = Assembler::new();
    asm.br_label("missing_target");
    asm.ret();
    let unknown = asm
        .finish_program()
        .expect_err("undefined label fixup should fail");
    assert!(matches!(unknown, AssemblerError::UnknownLabel(_)));
}

#[test]
fn assemble_rejects_invalid_label_forms_and_sections() {
    assert_asm_error_contains("start:\nret\n", "label definitions must use");
    assert_asm_error_contains(
        ".data\n.label start\n",
        "labels are only valid in code section",
    );
}

#[test]
fn assemble_rejects_numeric_jump_targets_and_unknown_locals() {
    assert_asm_error_contains("br 12\n", "numeric jump targets are not supported");
    assert_asm_error_contains("ldloc missing_local\n", "unknown local");
    assert_asm_error_contains("stloc missing_local\n", "unknown local");
}

#[test]
fn assemble_supports_string_escapes_and_case_insensitive_bools() {
    let program = assemble(
        r#"
        .data
        const yes TRUE
        const no fAlSe
        string msg "line\n\t\"quote\"\\slash"
        .code
        ldc yes
        ldc no
        ldc msg
        ret
    "#,
    )
    .expect("assembly should succeed");

    assert_eq!(program.constants.len(), 3);
    assert_eq!(program.constants[0], Value::Bool(true));
    assert_eq!(program.constants[1], Value::Bool(false));
    assert_eq!(
        program.constants[2],
        Value::String("line\n\t\"quote\"\\slash".to_string())
    );
}

#[test]
fn assemble_rejects_invalid_string_escapes_and_trailing_text() {
    assert_asm_error_contains(
        r#" .data
string bad "oops\q"
"#,
        "invalid escape",
    );
    assert_asm_error_contains(
        r#" .data
string bad "ok" trailing
"#,
        "unexpected trailing characters",
    );
}

#[test]
fn assemble_reports_local_index_overflow() {
    let mut source = String::new();
    for index in 0..=256 {
        source.push_str(&format!(".local l{index}\n"));
    }
    source.push_str("ret\n");
    assert_asm_error_contains(&source, "local index overflow");
}

#[test]
fn encode_rejects_array_and_map_constants() {
    let array_program = Program::new(vec![Value::Array(vec![])], vec![OpCode::Ret as u8]);
    let array_err = encode_program(&array_program).expect_err("array constants should be rejected");
    assert!(matches!(
        array_err,
        WireError::UnsupportedConstantType("array")
    ));

    let map_program = Program::new(vec![Value::Map(vec![])], vec![OpCode::Ret as u8]);
    let map_err = encode_program(&map_program).expect_err("map constants should be rejected");
    assert!(matches!(map_err, WireError::UnsupportedConstantType("map")));
}

#[test]
fn decode_rejects_invalid_flag_tag_bool_utf8_and_trailing_bytes() {
    let simple = Program::new(vec![], vec![OpCode::Ret as u8]);
    let encoded_simple = encode_program(&simple).expect("encode should succeed");

    let mut bad_flags = encoded_simple.clone();
    bad_flags[6..8].copy_from_slice(&1u16.to_le_bytes());
    assert!(matches!(
        decode_program(&bad_flags),
        Err(WireError::UnsupportedFlags(1))
    ));

    let mut bad_debug_flag = encoded_simple.clone();
    let last = bad_debug_flag.len() - 1;
    bad_debug_flag[last] = 9;
    assert!(matches!(
        decode_program(&bad_debug_flag),
        Err(WireError::InvalidDebugFlag(9))
    ));

    let bool_program = Program::new(vec![Value::Bool(true)], vec![OpCode::Ret as u8]);
    let mut bad_bool = encode_program(&bool_program).expect("encode should succeed");
    bad_bool[13] = 2;
    assert!(matches!(
        decode_program(&bad_bool),
        Err(WireError::InvalidBool(2))
    ));

    let int_program = Program::new(vec![Value::Int(7)], vec![OpCode::Ret as u8]);
    let mut bad_tag = encode_program(&int_program).expect("encode should succeed");
    bad_tag[12] = 255;
    assert!(matches!(
        decode_program(&bad_tag),
        Err(WireError::InvalidConstantTag(255))
    ));

    let string_program = Program::new(
        vec![Value::String("x".to_string())],
        vec![OpCode::Ret as u8],
    );
    let mut bad_utf8 = encode_program(&string_program).expect("encode should succeed");
    bad_utf8[17] = 0xFF;
    assert!(matches!(
        decode_program(&bad_utf8),
        Err(WireError::InvalidUtf8)
    ));

    let mut trailing = encoded_simple.clone();
    trailing.push(0xAA);
    assert!(matches!(
        decode_program(&trailing),
        Err(WireError::TrailingBytes)
    ));
}

#[test]
fn validate_rejects_truncated_operands_for_ldc_call_and_branches() {
    let bad_ldc = Program::new(vec![], vec![OpCode::Ldc as u8, 0x01]);
    assert!(matches!(
        validate_program(&bad_ldc, 0),
        Err(ValidationError::TruncatedOperand {
            opcode,
            expected_bytes: 4,
            ..
        }) if opcode == OpCode::Ldc as u8
    ));

    let bad_call = Program::new(vec![], vec![OpCode::Call as u8, 0x01, 0x00]);
    assert!(matches!(
        validate_program(&bad_call, 0),
        Err(ValidationError::TruncatedOperand {
            opcode,
            expected_bytes: 3,
            ..
        }) if opcode == OpCode::Call as u8
    ));

    let bad_br = Program::new(vec![], vec![OpCode::Br as u8, 0x01, 0x00]);
    assert!(matches!(
        validate_program(&bad_br, 0),
        Err(ValidationError::TruncatedOperand {
            opcode,
            expected_bytes: 4,
            ..
        }) if opcode == OpCode::Br as u8
    ));

    let bad_brfalse = Program::new(vec![], vec![OpCode::Brfalse as u8, 0x01, 0x00]);
    assert!(matches!(
        validate_program(&bad_brfalse, 0),
        Err(ValidationError::TruncatedOperand {
            opcode,
            expected_bytes: 4,
            ..
        }) if opcode == OpCode::Brfalse as u8
    ));
}

#[test]
fn validate_rejects_builtin_call_arity_mismatch() {
    // Builtin len() currently lives at 0xFFE0 and expects arity 1.
    let program = Program::new(vec![], vec![OpCode::Call as u8, 0xE0, 0xFF, 0x02]);
    assert!(matches!(
        validate_program(&program, 0),
        Err(ValidationError::InvalidCallArity {
            index: 0xFFE0,
            expected: 1,
            got: 2,
            ..
        })
    ));
}

#[test]
fn disassemble_outputs_invalid_opcode_and_missing_source_line_marker() {
    let invalid_opcode_program = Program::new(vec![], vec![0xFF]);
    let listing = disassemble_program_with_options(
        &invalid_opcode_program,
        DisassembleOptions { show_source: false },
    );
    assert!(listing.contains("invalid opcode"));

    let with_debug = Program::with_debug(
        vec![],
        vec![OpCode::Ret as u8],
        Some(DebugInfo {
            source: Some("only one line".to_string()),
            lines: vec![LineInfo { offset: 0, line: 9 }],
            functions: vec![],
            locals: vec![],
        }),
    );
    let listing =
        disassemble_program_with_options(&with_debug, DisassembleOptions { show_source: true });
    assert!(listing.contains("<missing source line>"));
}
