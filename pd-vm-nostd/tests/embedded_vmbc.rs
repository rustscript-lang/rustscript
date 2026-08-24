use std::collections::HashMap;

use pd_vm_nostd::{
    HostParamPassing as EmbeddedHostParamPassing, OpCode as EmbeddedOpCode,
    TypeSchema as EmbeddedTypeSchema, Value as EmbeddedValue, Vm as EmbeddedVm,
    VmStatus as EmbeddedVmStatus, WireError, decode_program,
};
use vm::compiler::TypeSchema;
use vm::{
    HostApiBuilder, HostFunctionSchema, HostImport, HostImportParam, HostImportSchema,
    HostParamPassing, NamedStructSchema, OpCode, Program, ReplLocalBinding, ResourceTypeKey, Value,
    ValueType, compile_source, compile_source_for_repl, compile_source_for_repl_with_locals,
    encode_program,
};

fn encoded_scalar_program() -> (Vec<u8>, u64) {
    let mut catalog = HostApiBuilder::new();
    catalog.function(HostFunctionSchema::new("serial::write", vec![]));
    let fingerprint = catalog.build().expect("test catalog").fingerprint();
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
        schema: Some(HostImportSchema {
            params: vec![HostImportParam {
                name: "file".to_string(),
                schema: TypeSchema::Resource(ResourceTypeKey::new("io.file").unwrap()),
                passing: HostParamPassing::Borrow,
            }],
            return_type: TypeSchema::Null,
            fingerprint,
        }),
    });
    (
        encode_program(&program).expect("std VMBC encoder should succeed"),
        fingerprint.as_u64(),
    )
}

fn scalar_import_field_offsets(bytes: &[u8]) -> (usize, usize) {
    let import_name = b"serial::write";
    let name_offset = bytes
        .windows(import_name.len())
        .position(|window| window == import_name)
        .expect("encoded fixture should contain the host import name");
    let name_length_offset = name_offset
        .checked_sub(4)
        .expect("host import name should have a length prefix");
    assert_eq!(
        u32::from_le_bytes(
            bytes[name_length_offset..name_offset]
                .try_into()
                .expect("host import name length should be four bytes"),
        ),
        import_name.len() as u32
    );

    let arity_offset = name_offset + import_name.len();
    let return_type_offset = arity_offset + 1;
    assert_eq!(bytes[arity_offset], 1);
    assert_eq!(bytes[return_type_offset], ValueType::Null as u8);
    assert_eq!(bytes[return_type_offset + 1], 1);
    (arity_offset, return_type_offset)
}

fn assert_invalid_host_import_schema(bytes: &[u8]) {
    let error = decode_program(bytes).expect_err("malformed host import schema must be rejected");
    assert!(
        matches!(error, WireError::InvalidHostImportSchema(_)),
        "expected typed InvalidHostImportSchema, got {error:?}"
    );
}

#[test]
fn embedded_decoder_reads_host_generated_v14() {
    let (bytes, fingerprint) = encoded_scalar_program();
    let program = decode_program(&bytes).expect("embedded decoder should accept VMBC v14");

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
    let schema = program.imports()[0]
        .schema
        .as_ref()
        .expect("embedded import should retain exact schema");
    assert_eq!(schema.fingerprint.as_u64(), fingerprint);
    assert_eq!(schema.params[0].passing, EmbeddedHostParamPassing::Borrow);
    assert!(matches!(
        &schema.params[0].schema,
        EmbeddedTypeSchema::Resource(key) if key.as_str() == "io.file"
    ));
}

#[test]
fn embedded_decoder_reads_host_generated_v13() {
    let mut bytes = encode_program(&Program::new(Vec::new(), Vec::new()))
        .expect("std VMBC encoder should succeed");
    assert_eq!(&bytes[19..23], &[0, 0, 0, 0]);
    bytes.drain(19..23);
    bytes[4..6].copy_from_slice(&13u16.to_le_bytes());

    let program = decode_program(&bytes).expect("embedded decoder should accept VMBC v13");
    assert!(program.constants().is_empty());
    assert!(program.code().is_empty());
    assert_eq!(program.local_count(), 0);
}

fn named_schema_bytes() -> Vec<u8> {
    let mut schemas = HashMap::new();
    schemas.insert(
        "AA".to_string(),
        NamedStructSchema {
            type_params: vec!["T0".to_string(), "T1".to_string()],
            body_schema: TypeSchema::GenericParam("T0".to_string()),
        },
    );
    schemas.insert(
        "BB".to_string(),
        NamedStructSchema {
            type_params: Vec::new(),
            body_schema: TypeSchema::Int,
        },
    );
    encode_program(&Program::new(Vec::new(), Vec::new()).with_named_struct_schemas(schemas))
        .expect("named schemas should encode")
}

fn replace_wire_string(bytes: &mut [u8], old: &[u8], new: &[u8]) {
    assert_eq!(old.len(), new.len());
    let mut marker = (old.len() as u32).to_le_bytes().to_vec();
    marker.extend_from_slice(old);
    let offset = bytes
        .windows(marker.len())
        .position(|window| window == marker)
        .expect("wire string should exist");
    bytes[offset + 4..offset + 4 + new.len()].copy_from_slice(new);
}

#[test]
fn embedded_decoder_rejects_duplicate_v14_named_struct_names() {
    let mut bytes = named_schema_bytes();
    replace_wire_string(&mut bytes, b"BB", b"AA");
    let error = decode_program(&bytes).expect_err("duplicate named struct names must be rejected");
    assert!(matches!(
        error,
        WireError::InvalidNamedStructSchema("duplicate struct name")
    ));
}

#[test]
fn embedded_decoder_rejects_duplicate_v14_type_parameters() {
    let mut bytes = named_schema_bytes();
    replace_wire_string(&mut bytes, b"T1", b"T0");
    let error = decode_program(&bytes).expect_err("duplicate type parameters must be rejected");
    assert!(matches!(
        error,
        WireError::InvalidNamedStructSchema("duplicate type parameter")
    ));
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
    let (mut bytes, _) = encoded_scalar_program();
    bytes.push(0xff);

    assert_eq!(decode_program(&bytes), Err(WireError::TrailingBytes));
}

#[test]
fn embedded_decoder_rejects_invalid_magic() {
    let (mut bytes, _) = encoded_scalar_program();
    bytes[0] = b'X';

    assert!(matches!(
        decode_program(&bytes),
        Err(WireError::InvalidMagic(_))
    ));
}

#[test]
fn embedded_decoder_rejects_malformed_host_import_arity() {
    let (mut bytes, _) = encoded_scalar_program();
    let (arity_offset, _) = scalar_import_field_offsets(&bytes);

    // The std encoder rejects this inconsistency before serialization. Mutating
    // a valid V13 payload exercises the no_std decoder's wire-level check.
    bytes[arity_offset] = 2;
    assert_invalid_host_import_schema(&bytes);
}

#[test]
fn embedded_decoder_rejects_malformed_host_import_coarse_return_type() {
    let (mut bytes, _) = encoded_scalar_program();
    let (_, return_type_offset) = scalar_import_field_offsets(&bytes);

    // Keep the exact schema's `null` return and mutate only the coarse wire
    // type, producing a malformed V13 import that the decoder must reject.
    bytes[return_type_offset] = ValueType::Int as u8;
    assert_invalid_host_import_schema(&bytes);
}

#[test]
fn embedded_decoder_rejects_duplicate_object_fields_in_host_import_schema() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"VMBC");
    bytes.extend_from_slice(&13u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.push(b'x');
    bytes.push(1);
    bytes.push(ValueType::Null as u8);
    bytes.push(1);
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.push(b'p');
    bytes.push(14);
    bytes.extend_from_slice(&2u32.to_le_bytes());
    for schema_tag in [2, 6] {
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.push(b'a');
        bytes.push(schema_tag);
    }
    bytes.push(0);
    bytes.push(1);
    bytes.push(0);
    bytes.push(0);
    for _ in 0..5 {
        bytes.extend_from_slice(&0u32.to_le_bytes());
    }

    assert!(matches!(
        decode_program(&bytes),
        Err(WireError::InvalidHostImportSchema(
            "duplicate object field name"
        ))
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
fn call_script_opcode_is_0x1a_in_both_crates() {
    // The historical callable-creation opcode slot (0x1A) is now the static
    // script-call opcode in both the std and embedded opcode tables.
    assert_eq!(OpCode::try_from(0x1a), Ok(OpCode::CallScript));
    assert_eq!(
        EmbeddedOpCode::try_from(0x1a),
        Ok(EmbeddedOpCode::CallScript)
    );
    assert!(EmbeddedOpCode::try_from(0x7f).is_err());
}
