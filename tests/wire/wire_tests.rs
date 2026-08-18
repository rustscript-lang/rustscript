use std::collections::HashMap;

use vm::compiler::TypeSchema;
use vm::{
    ArgInfo, Assembler, BuiltinFunction, BytecodeBuilder, CallableKind, CallablePrototype,
    CallableTarget, DebugFunction, DebugInfo, DisassembleOptions, HostImport, LineInfo, LocalInfo,
    Program, ScriptFunction, TypeMap, ValidationError, Value, ValueType, WireError,
    builtin_call_index, decode_program, disassemble_vmbc, disassemble_vmbc_with_options,
    encode_program, infer_local_count, validate_program, HostApiCatalog, ResourceTypeKey,
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
            return_type: ValueType::Unknown,
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
        strict_types: true,
        local_types: vec![ValueType::Int, ValueType::Unknown],
        local_schemas: vec![None, None],
        callable_slots: vec![false, false],
        optional_slots: vec![false, false],
        operand_types,
    });

    let encoded = encode_program(&program).expect("encode should succeed");
    assert_eq!(u16::from_le_bytes([encoded[4], encoded[5]]), 12);
    let decoded = decode_program(&encoded).expect("decode should succeed");

    assert_eq!(decoded.constants, program.constants);
    assert_eq!(decoded.code, program.code);
    assert_eq!(decoded.imports, program.imports);
    assert_eq!(decoded.debug, program.debug);
    assert_eq!(decoded.type_map, program.type_map);
}

#[test]
fn wire_roundtrip_recovers_locals_reserved_by_type_metadata() {
    let local_count = 8;
    let program = Program::new(Vec::new(), vec![vm::OpCode::Ret as u8])
        .with_local_count(local_count)
        .with_type_map(TypeMap {
            strict_types: true,
            local_types: vec![ValueType::Unknown; local_count],
            local_schemas: vec![None; local_count],
            callable_slots: vec![false; local_count],
            optional_slots: vec![false; local_count],
            operand_types: HashMap::new(),
        });

    let encoded = encode_program(&program).expect("encode reserved locals");
    let decoded = decode_program(&encoded).expect("decode reserved locals");
    assert_eq!(decoded.local_count, local_count);
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

    let mut old_version = encoded.clone();
    old_version[4..6].copy_from_slice(&9u16.to_le_bytes());
    assert!(matches!(
        decode_program(&old_version),
        Err(WireError::UnsupportedVersion(9))
    ));

    let mut previous_version = encoded.clone();
    previous_version[4..6].copy_from_slice(&10u16.to_le_bytes());
    assert!(matches!(
        decode_program(&previous_version),
        Err(WireError::UnsupportedVersion(10))
    ));

    let mut v11_version = encoded.clone();
    v11_version[4..6].copy_from_slice(&11u16.to_le_bytes());
    assert!(matches!(
        decode_program(&v11_version),
        Err(WireError::UnsupportedVersion(11))
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
            return_type: ValueType::Unknown,
        }],
        None,
    );
    validate_program(&program, 4).expect("program should validate");
}

#[test]
fn callable_metadata_roundtrips_vmbc_v12() {
    let compiled = vm::compile_source_for_repl(
        r#"
            fn add_one(value: int) -> int { value + 1 }
            let closure = |value| add_one(value);
            closure(41);
        "#,
    )
    .expect("callable source should compile");
    let encoded = encode_program(&compiled.program).expect("encode callable metadata");
    let decoded = decode_program(&encoded).expect("decode callable metadata");
    assert_eq!(decoded.script_functions, compiled.program.script_functions);
    assert_eq!(
        decoded.callable_prototypes,
        compiled.program.callable_prototypes
    );
    assert_eq!(decoded.function_regions, compiled.program.function_regions);
    assert_eq!(
        decoded.root_callable_bindings,
        compiled.program.root_callable_bindings
    );
    validate_program(&decoded, 0).expect("decoded program should validate");
}

#[test]
fn closure_shared_capture_vmbc_round_trip() {
    let compiled = vm::compile_source_with_flavor(
        r#"
            let mut state: string = "";
            let sink = |delta| if true => {
                state = state + delta;
                { action: "continue" }
            } else => {
                { action: "skip" }
            };
            let _ = sink("a");
            state;
        "#,
        vm::SourceFlavor::RustScript,
    )
    .expect("mutable capture source should compile");
    let sink_prototype = compiled
        .program
        .callable_prototypes
        .iter()
        .find(|prototype| {
            prototype.kind == vm::CallableKind::Closure
                && prototype
                    .capture_modes
                    .contains(&vm::CaptureBindingMode::BorrowMut)
        })
        .expect("closure prototype should carry a BorrowMut capture");
    assert!(
        sink_prototype
            .capture_modes
            .iter()
            .all(|mode| *mode != vm::CaptureBindingMode::Move),
        "mutation capture must not be classified as a move"
    );
    let encoded = encode_program(&compiled.program).expect("encode shared capture program");
    let decoded = decode_program(&encoded).expect("decode shared capture program");
    assert_eq!(
        decoded.callable_prototypes, compiled.program.callable_prototypes,
        "capture modes must survive the VMBC round trip"
    );
    validate_program(&decoded, 0).expect("decoded program should validate");
    let mut runtime = vm::Vm::new(decoded);
    assert_eq!(
        runtime.run().expect("decoded program should run"),
        vm::VmStatus::Halted
    );
    assert_eq!(runtime.stack(), &[Value::string("a")]);
}

#[test]
fn callvalue_roundtrips_validation_and_disassembly() {
    let mut bc = BytecodeBuilder::new();
    bc.call_value(2);
    bc.ret();
    let program = Program::new(vec![], bc.finish());

    validate_program(&program, 0).expect("callvalue should validate structurally");
    let bytes = encode_program(&program).expect("callvalue should encode");
    let decoded = decode_program(&bytes).expect("callvalue should decode");
    assert_eq!(decoded.code, program.code);
    assert!(disassemble_vmbc(&bytes).unwrap().contains("callvalue 2"));
}

#[test]
fn validate_rejects_truncated_callvalue() {
    let program = Program::new(vec![], vec![vm::OpCode::CallValue as u8]);
    assert!(matches!(
        validate_program(&program, 0),
        Err(ValidationError::TruncatedOperand {
            expected_bytes: 1,
            ..
        })
    ));
}

#[test]
fn validation_rejects_cross_region_branches() {
    let mut code = vec![vm::OpCode::Br as u8];
    code.extend_from_slice(&6u32.to_le_bytes());
    code.push(vm::OpCode::Ret as u8);
    code.push(vm::OpCode::Ret as u8);
    let program = Program::new(Vec::new(), code).with_callable_metadata(
        vec![vm::ScriptFunction {
            entry_ip: 6,
            end_ip: 7,
        }],
        vec![vm::CallablePrototype {
            kind: vm::CallableKind::FunctionItem,
            target: vm::CallableTarget::ScriptFunction(0),
            arity: 0,
            frame_local_count: 0,
            parameter_slots: Vec::new(),
            capture_source_slots: Vec::new(),
            capture_slots: Vec::new(),
            capture_modes: Vec::new(),
            self_slot: None,
            schema: None,
        }],
        vec![
            vm::FunctionRegion {
                start_ip: 0,
                end_ip: 6,
                prototype_id: None,
            },
            vm::FunctionRegion {
                start_ip: 6,
                end_ip: 7,
                prototype_id: Some(0),
            },
        ],
        Vec::new(),
    );
    assert!(matches!(
        validate_program(&program, 0),
        Err(ValidationError::InvalidJumpTarget {
            offset: 0,
            target: 6
        })
    ));
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
            return_type: ValueType::Unknown,
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
            return_type: ValueType::Unknown,
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
fn wire_roundtrip_preserves_host_import_return_types() {
    let program = Program::with_imports_and_debug(
        vec![],
        vec![0x01],
        vec![HostImport {
            name: "typed_host".to_string(),
            arity: 1,
            return_type: ValueType::Int,
        }],
        None,
    );

    let encoded = encode_program(&program).expect("encode should succeed");
    let decoded = decode_program(&encoded).expect("decode should succeed");

    assert_eq!(decoded.imports, program.imports);
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

#[test]
fn literal_string_builtin_indices_are_appended_and_publicly_resolved() {
    assert_eq!(BuiltinFunction::Count.call_index(), 65_531);
    assert_eq!(BuiltinFunction::FormatTemplate.call_index(), 65_438);
    assert_eq!(BuiltinFunction::ToString.call_index(), 65_439);
    assert_eq!(BuiltinFunction::TypeOf.call_index(), 65_440);
    assert_eq!(BuiltinFunction::Assert.call_index(), 65_441);

    let first = BuiltinFunction::FormatTemplate.call_index() - 3;
    assert_eq!(builtin_call_index("string_contains"), Some(first));
    assert_eq!(
        builtin_call_index("string_replace_literal"),
        Some(first + 1)
    );
    assert_eq!(builtin_call_index("string_lower_ascii"), Some(first + 2));
    assert_eq!(builtin_call_index("string_split_literal"), Some(first - 1));
    assert_eq!(BuiltinFunction::StringContains.call_index(), first);
    assert_eq!(
        BuiltinFunction::StringReplaceLiteral.call_index(),
        first + 1
    );
    assert_eq!(BuiltinFunction::StringLowerAscii.call_index(), first + 2);
    assert_eq!(BuiltinFunction::StringSplitLiteral.call_index(), first - 1);
}

// ---------------------------------------------------------------------------
// Milestone 6: CallScript wire support (VMBC V12)
// ---------------------------------------------------------------------------

#[test]
fn call_script_roundtrips_validation_and_disassembly() {
    let mut code = vec![0x1A];
    code.extend_from_slice(&7u32.to_le_bytes());
    code.push(2);
    code.push(vm::OpCode::Ret as u8);
    // The V12 validator resolves the prototype id against the callable
    // metadata, so the fixture carries a matching prototype (id 7, arity 2,
    // script-function target) plus one script function boundary.
    let program = Program::new(vec![], code).with_callable_metadata(
        vec![ScriptFunction {
            entry_ip: 6,
            end_ip: 7,
        }],
        (0..8)
            .map(|_| CallablePrototype {
                kind: CallableKind::FunctionItem,
                target: CallableTarget::ScriptFunction(0),
                arity: 2,
                frame_local_count: 2,
                parameter_slots: vec![0, 1],
                capture_source_slots: Vec::new(),
                capture_slots: Vec::new(),
                capture_modes: Vec::new(),
                self_slot: None,
                schema: None,
            })
            .collect(),
        Vec::new(),
        Vec::new(),
    );

    validate_program(&program, 0).expect("callscript should validate structurally");
    let bytes = encode_program(&program).expect("callscript should encode");
    let decoded = decode_program(&bytes).expect("callscript should decode");
    assert_eq!(decoded.code, program.code);
    validate_program(&decoded, 0).expect("decoded callscript should validate");
    assert!(disassemble_vmbc(&bytes).unwrap().contains("callscript 7 2"));
}

#[test]
fn call_script_text_assembler_parses_prototype_and_argc() {
    let program =
        vm::assemble("callscript 7 2\nret\n").expect("text assembler should parse callscript");
    let mut expected = vec![0x1A];
    expected.extend_from_slice(&7u32.to_le_bytes());
    expected.push(2);
    expected.push(vm::OpCode::Ret as u8);
    assert_eq!(program.code, expected);
}

#[test]
fn validate_rejects_truncated_call_script_operands() {
    // No operand bytes at all.
    let missing_all = Program::new(vec![], vec![0x1A]);
    assert!(matches!(
        validate_program(&missing_all, 0),
        Err(ValidationError::TruncatedOperand {
            expected_bytes: 5,
            ..
        })
    ));
    // Four of the five operand bytes present: the u32 prototype id without
    // the trailing argc byte.
    let mut missing_argc = vec![0x1A];
    missing_argc.extend_from_slice(&3u32.to_le_bytes());
    let missing_argc = Program::new(vec![], missing_argc);
    assert!(matches!(
        validate_program(&missing_argc, 0),
        Err(ValidationError::TruncatedOperand {
            expected_bytes: 5,
            ..
        })
    ));
}

#[test]
fn validate_rejects_out_of_range_call_script_prototype() {
    // CallScript(7, 2) with no callable prototypes at all: the target id is
    // out of range and must be rejected deterministically at validation
    // time instead of surfacing later as a runtime VM error.
    let mut code = vec![0x1A];
    code.extend_from_slice(&7u32.to_le_bytes());
    code.push(2);
    code.push(vm::OpCode::Ret as u8);
    let no_prototypes = Program::new(vec![], code);
    assert!(matches!(
        validate_program(&no_prototypes, 0),
        Err(ValidationError::InvalidCallScriptTarget {
            offset: 0,
            prototype_id: 7
        })
    ));

    // One prototype exists (id 0) but the call targets id 1.
    let mut code = vec![0x1A];
    code.extend_from_slice(&1u32.to_le_bytes());
    code.push(0);
    code.push(vm::OpCode::Ret as u8);
    let out_of_range = Program::new(vec![], code).with_callable_metadata(
        vec![ScriptFunction {
            entry_ip: 6,
            end_ip: 7,
        }],
        vec![CallablePrototype {
            kind: CallableKind::FunctionItem,
            target: CallableTarget::ScriptFunction(0),
            arity: 0,
            frame_local_count: 0,
            parameter_slots: Vec::new(),
            capture_source_slots: Vec::new(),
            capture_slots: Vec::new(),
            capture_modes: Vec::new(),
            self_slot: None,
            schema: None,
        }],
        Vec::new(),
        Vec::new(),
    );
    assert!(matches!(
        validate_program(&out_of_range, 0),
        Err(ValidationError::InvalidCallScriptTarget {
            offset: 0,
            prototype_id: 1
        })
    ));
}

#[test]
fn validate_rejects_call_script_arity_mismatch() {
    // Prototype 0 declares arity 1 but the call passes 2 operands.
    let mut code = vec![0x1A];
    code.extend_from_slice(&0u32.to_le_bytes());
    code.push(2);
    code.push(vm::OpCode::Ret as u8);
    let program = Program::new(vec![], code).with_callable_metadata(
        vec![ScriptFunction {
            entry_ip: 6,
            end_ip: 7,
        }],
        vec![CallablePrototype {
            kind: CallableKind::FunctionItem,
            target: CallableTarget::ScriptFunction(0),
            arity: 1,
            frame_local_count: 1,
            parameter_slots: vec![0],
            capture_source_slots: Vec::new(),
            capture_slots: Vec::new(),
            capture_modes: Vec::new(),
            self_slot: None,
            schema: None,
        }],
        Vec::new(),
        Vec::new(),
    );
    assert!(matches!(
        validate_program(&program, 0),
        Err(ValidationError::InvalidCallScriptArity {
            offset: 0,
            prototype_id: 0,
            expected: 1,
            got: 2
        })
    ));
}

#[test]
fn validate_rejects_call_script_targeting_host_import_prototype() {
    // `CallScript` is a static script-function call: a host-import
    // prototype is not a valid target. The VM rejects the same program
    // shape with the typed `InvalidCallablePrototype` runtime error, so
    // VMBC must reject it deterministically at validation time too.
    let mut code = vec![0x1A];
    code.extend_from_slice(&0u32.to_le_bytes());
    code.push(1);
    code.push(vm::OpCode::Ret as u8);
    let program = Program::with_imports_and_debug(
        Vec::new(),
        code,
        vec![HostImport {
            name: "host_fn".to_string(),
            arity: 1,
            return_type: ValueType::Unknown,
        }],
        None,
    )
    .with_callable_metadata(
        Vec::new(),
        vec![CallablePrototype {
            kind: CallableKind::HostFunction,
            target: CallableTarget::HostImport(0),
            arity: 1,
            frame_local_count: 1,
            parameter_slots: vec![0],
            capture_source_slots: Vec::new(),
            capture_slots: Vec::new(),
            capture_modes: Vec::new(),
            self_slot: None,
            schema: None,
        }],
        Vec::new(),
        Vec::new(),
    );
    assert!(matches!(
        validate_program(&program, 0),
        Err(ValidationError::InvalidCallScriptTarget {
            offset: 0,
            prototype_id: 0
        })
    ));
}

#[test]
fn call_script_wire_version_is_v12_and_rejects_v11() {
    let program = Program::new(vec![], vec![vm::OpCode::Ret as u8]);
    let encoded = encode_program(&program).expect("encode should succeed");
    assert_eq!(u16::from_le_bytes([encoded[4], encoded[5]]), 12);

    let mut old = encoded.clone();
    old[4..6].copy_from_slice(&11u16.to_le_bytes());
    assert!(matches!(
        decode_program(&old),
        Err(WireError::UnsupportedVersion(11))
    ));
}

#[test]
fn call_script_no_script_program_code_bytes_unchanged_by_version_bump() {
    // The V12 bump must not alter instruction bytes for programs without
    // script calls: encode a plain arithmetic program and verify the
    // embedded code section is exactly the assembler output.
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.ldc(1);
    bc.add();
    bc.ret();
    let program = Program::new(vec![Value::Int(1), Value::Int(2)], bc.finish());
    let encoded = encode_program(&program).expect("encode should succeed");
    assert_eq!(u16::from_le_bytes([encoded[4], encoded[5]]), 12);
    let decoded = decode_program(&encoded).expect("decode should succeed");
    assert_eq!(decoded.code, program.code);
    assert_eq!(decoded.constants, program.constants);
}

#[test]
fn schema_round_trip_resource_wire_nominally() {
    // A nominal host resource must round-trip through the shared wire
    // type-schema encoding (tag 17), preserving its identity as a resource
    // rather than being flattened into a structural type.
    let key = vm::ResourceTypeKey::new("io.file").expect("valid key");
    let schema = vm::compiler::TypeSchema::Resource(key.clone());
    let program = vm::Program::new(Vec::new(), Vec::new()).with_type_map(vm::TypeMap {
        strict_types: false,
        local_types: vec![vm::ValueType::Int],
        local_schemas: vec![Some(schema.clone())],
        callable_slots: vec![false],
                    optional_slots: vec![false],
        operand_types: HashMap::new(),
    });

    let encoded = vm::encode_program(&program).expect("encode should succeed");
    let decoded = vm::decode_program(&encoded).expect("decode should succeed");
    assert_eq!(decoded.type_map, program.type_map);
    assert_eq!(
        decoded.type_map.as_ref().and_then(|tm| tm.local_schemas[0].as_ref()),
        Some(&TypeSchema::Resource(key))
    );
}

#[test]
fn malformed_resource_key_is_rejected_on_read() {
    // A resource key that violates the resource-key grammar must be rejected by the
    // key's own Deserialize impl.
    assert!(ResourceTypeKey::new("has space").is_err());
    assert!(ResourceTypeKey::new("").is_err());
    assert!(ResourceTypeKey::new(".leading").is_err());

    // ... and a malformed key hiding inside a catalog's resources must also fail
    // catalog deserialization (serde runs the same validation).
    let mut v = serde_json::json!({
        "resources": [{ "key": "io.file", "description": "file" }],
        "functions": []
    });
    v["resources"][0]["key"] = serde_json::json!("bad key");
    assert!(serde_json::from_value::<HostApiCatalog>(v).is_err());
}
