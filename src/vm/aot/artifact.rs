use std::io;
use std::path::Path;

use crate::bytecode::{Program, TypeMap, Value, ValueType};
use crate::compiler::ir::TypeSchema;
use crate::vm::native::{
    helper_entry_offset, interrupt_helper_entry_offset, selected_codegen_backend,
};
use crate::vm::{Vm, VmError};

use super::super::jit::JitConfig;
use super::compile::CompiledProgram;

const MAGIC: [u8; 4] = *b"PAT\0";
const VERSION: u16 = 2;
const ABI_VERSION: u16 = 1;
const FLAGS: u16 = 0;

#[derive(Debug)]
pub enum AotArtifactError {
    Io(io::Error),
    Vm(VmError),
    Wire(crate::WireError),
    MissingAotProgram,
    UnexpectedEof,
    InvalidMagic([u8; 4]),
    UnsupportedVersion(u16),
    UnsupportedAbiVersion(u16),
    UnsupportedFlags(u16),
    InvalidUtf8,
    TrailingBytes,
    LengthTooLarge(&'static str, usize),
    IncompatibleProgramHash {
        expected: u64,
        found: u64,
    },
    EmbeddedProgramHashMismatch {
        stored: u64,
        computed: u64,
    },
    InvalidValueTag(u8),
    InvalidBool(u8),
    InvalidValueType(u8),
    IncompatibleRuntime {
        field: &'static str,
        expected: String,
        found: String,
    },
}

impl std::fmt::Display for AotArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Vm(err) => write!(f, "{err}"),
            Self::Wire(err) => write!(f, "{err}"),
            Self::MissingAotProgram => write!(f, "whole-program aot is not compiled"),
            Self::UnexpectedEof => write!(f, "unexpected end of aot artifact"),
            Self::InvalidMagic(found) => write!(f, "invalid aot artifact magic: {found:?}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported aot artifact version: {version}")
            }
            Self::UnsupportedAbiVersion(version) => {
                write!(f, "unsupported aot artifact abi version: {version}")
            }
            Self::UnsupportedFlags(flags) => write!(f, "unsupported aot artifact flags: {flags}"),
            Self::InvalidUtf8 => write!(f, "invalid utf-8 in aot artifact"),
            Self::TrailingBytes => write!(f, "trailing bytes after aot artifact payload"),
            Self::LengthTooLarge(field, len) => write!(f, "{field} length too large: {len}"),
            Self::IncompatibleProgramHash { expected, found } => write!(
                f,
                "aot artifact program hash mismatch: expected {expected}, found {found}"
            ),
            Self::EmbeddedProgramHashMismatch { stored, computed } => write!(
                f,
                "aot artifact embedded program hash mismatch: stored {stored}, computed {computed}"
            ),
            Self::InvalidValueTag(tag) => write!(f, "invalid aot artifact value tag: {tag}"),
            Self::InvalidBool(value) => write!(f, "invalid bool value in aot artifact: {value}"),
            Self::InvalidValueType(value) => {
                write!(f, "invalid value type in aot artifact: {value}")
            }
            Self::IncompatibleRuntime {
                field,
                expected,
                found,
            } => write!(
                f,
                "aot artifact runtime mismatch for {field}: expected {expected}, found {found}"
            ),
        }
    }
}

impl std::error::Error for AotArtifactError {}

impl From<VmError> for AotArtifactError {
    fn from(value: VmError) -> Self {
        Self::Vm(value)
    }
}

impl From<crate::WireError> for AotArtifactError {
    fn from(value: crate::WireError) -> Self {
        Self::Wire(value)
    }
}

impl Vm {
    pub fn encode_aot_artifact(&mut self) -> Result<Vec<u8>, AotArtifactError> {
        if self.aot_program.is_none() {
            self.compile_aot()?;
        }
        let program_hash = self.ensure_program_cache_key();
        let aot_program = self
            .aot_program
            .as_ref()
            .ok_or(AotArtifactError::MissingAotProgram)?;
        encode_artifact(self.program(), aot_program, program_hash)
    }

    pub fn save_aot_artifact_to_file<P: AsRef<Path>>(
        &mut self,
        path: P,
    ) -> Result<(), AotArtifactError> {
        let bytes = self.encode_aot_artifact()?;
        std::fs::write(path, bytes).map_err(AotArtifactError::Io)
    }

    pub fn load_aot_artifact(&mut self, bytes: &[u8]) -> Result<(), AotArtifactError> {
        let expected_program_hash = self.ensure_program_cache_key();
        let decoded = decode_artifact(bytes, Some(expected_program_hash))?;
        self.aot_program = Some(CompiledProgram::from_code(
            decoded.code,
            decoded.resume_ips,
        )?);
        self.aot_exec_count = 0;
        Ok(())
    }

    pub fn load_aot_artifact_from_file<P: AsRef<Path>>(
        &mut self,
        path: P,
    ) -> Result<(), AotArtifactError> {
        let bytes = std::fs::read(path).map_err(AotArtifactError::Io)?;
        self.load_aot_artifact(&bytes)
    }

    pub fn new_from_aot_artifact_with_jit_config(
        bytes: &[u8],
        jit_config: JitConfig,
    ) -> Result<Self, AotArtifactError> {
        let decoded = decode_artifact(bytes, None)?;
        let mut vm = Vm::new_with_jit_config(decoded.program, jit_config);
        vm.aot_program = Some(CompiledProgram::from_code(
            decoded.code,
            decoded.resume_ips,
        )?);
        vm.aot_exec_count = 0;
        Ok(vm)
    }

    pub fn new_from_aot_artifact_file_with_jit_config<P: AsRef<Path>>(
        path: P,
        jit_config: JitConfig,
    ) -> Result<Self, AotArtifactError> {
        let bytes = std::fs::read(path).map_err(AotArtifactError::Io)?;
        Self::new_from_aot_artifact_with_jit_config(&bytes, jit_config)
    }
}

struct DecodedArtifact {
    program: Program,
    code: Vec<u8>,
    resume_ips: Vec<usize>,
}

fn encode_artifact(
    source_program: &Program,
    aot_program: &CompiledProgram,
    program_hash: u64,
) -> Result<Vec<u8>, AotArtifactError> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&ABI_VERSION.to_le_bytes());
    out.extend_from_slice(&FLAGS.to_le_bytes());
    out.push(u8::try_from(std::mem::size_of::<usize>()).expect("pointer width fits u8"));

    write_string("arch", std::env::consts::ARCH, &mut out)?;
    write_string("os", std::env::consts::OS, &mut out)?;
    write_string("backend", selected_codegen_backend(), &mut out)?;

    write_u32("vm ip offset", std::mem::offset_of!(Vm, ip), &mut out)?;
    write_u32(
        "native helper offset",
        helper_entry_offset() as usize,
        &mut out,
    )?;
    write_u32(
        "interrupt helper offset",
        interrupt_helper_entry_offset() as usize,
        &mut out,
    )?;
    out.extend_from_slice(&program_hash.to_le_bytes());

    write_u32("program local count", source_program.local_count, &mut out)?;
    let encoded_program = encode_program_payload(source_program)?;
    write_u32("embedded program length", encoded_program.len(), &mut out)?;
    out.extend_from_slice(&encoded_program);

    write_u32("resume ip count", aot_program.resume_ips.len(), &mut out)?;
    for &ip in aot_program.resume_ips.iter() {
        write_u32("resume ip", ip, &mut out)?;
    }

    write_u32(
        "native code length",
        aot_program.code_bytes().len(),
        &mut out,
    )?;
    out.extend_from_slice(aot_program.code_bytes());
    Ok(out)
}

fn decode_artifact(
    bytes: &[u8],
    expected_program_hash: Option<u64>,
) -> Result<DecodedArtifact, AotArtifactError> {
    let mut cursor = Cursor::new(bytes);

    let magic = cursor.read_exact_array::<4>()?;
    if magic != MAGIC {
        return Err(AotArtifactError::InvalidMagic(magic));
    }

    let version = cursor.read_u16()?;
    if version != VERSION {
        return Err(AotArtifactError::UnsupportedVersion(version));
    }

    let abi_version = cursor.read_u16()?;
    if abi_version != ABI_VERSION {
        return Err(AotArtifactError::UnsupportedAbiVersion(abi_version));
    }

    let flags = cursor.read_u16()?;
    if flags != FLAGS {
        return Err(AotArtifactError::UnsupportedFlags(flags));
    }

    let pointer_width = cursor.read_u8()?;
    validate_runtime_field(
        "pointer width",
        std::mem::size_of::<usize>().to_string(),
        pointer_width.to_string(),
    )?;
    validate_runtime_field(
        "arch",
        std::env::consts::ARCH.to_string(),
        cursor.read_string()?,
    )?;
    validate_runtime_field(
        "os",
        std::env::consts::OS.to_string(),
        cursor.read_string()?,
    )?;
    validate_runtime_field(
        "backend",
        selected_codegen_backend().to_string(),
        cursor.read_string()?,
    )?;
    validate_runtime_field(
        "vm ip offset",
        std::mem::offset_of!(Vm, ip).to_string(),
        cursor.read_u32()?.to_string(),
    )?;
    validate_runtime_field(
        "native helper offset",
        helper_entry_offset().to_string(),
        cursor.read_u32()?.to_string(),
    )?;
    validate_runtime_field(
        "interrupt helper offset",
        interrupt_helper_entry_offset().to_string(),
        cursor.read_u32()?.to_string(),
    )?;

    let found_program_hash = cursor.read_u64()?;
    let local_count = cursor.read_u32()? as usize;
    let embedded_program_len = cursor.read_u32()? as usize;
    let mut program = decode_program_payload(cursor.read_exact(embedded_program_len)?)?;
    program.local_count = local_count;
    let computed_hash = super::super::compute_program_cache_key(&program);
    if computed_hash != found_program_hash {
        return Err(AotArtifactError::EmbeddedProgramHashMismatch {
            stored: found_program_hash,
            computed: computed_hash,
        });
    }

    if let Some(expected_program_hash) = expected_program_hash
        && found_program_hash != expected_program_hash
    {
        return Err(AotArtifactError::IncompatibleProgramHash {
            expected: expected_program_hash,
            found: found_program_hash,
        });
    }

    let resume_count = cursor.read_u32()? as usize;
    let mut resume_ips = Vec::with_capacity(resume_count);
    for _ in 0..resume_count {
        resume_ips.push(cursor.read_u32()? as usize);
    }

    let code_len = cursor.read_u32()? as usize;
    let code = cursor.read_exact(code_len)?.to_vec();

    if !cursor.is_at_end() {
        return Err(AotArtifactError::TrailingBytes);
    }

    Ok(DecodedArtifact {
        program,
        code,
        resume_ips,
    })
}

fn validate_runtime_field(
    field: &'static str,
    expected: String,
    found: String,
) -> Result<(), AotArtifactError> {
    if expected == found {
        return Ok(());
    }
    Err(AotArtifactError::IncompatibleRuntime {
        field,
        expected,
        found,
    })
}

fn write_u32(field: &'static str, value: usize, out: &mut Vec<u8>) -> Result<(), AotArtifactError> {
    let value = u32::try_from(value).map_err(|_| AotArtifactError::LengthTooLarge(field, value))?;
    out.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_string(
    field: &'static str,
    value: &str,
    out: &mut Vec<u8>,
) -> Result<(), AotArtifactError> {
    write_u32(field, value.len(), out)?;
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn encode_program_payload(program: &Program) -> Result<Vec<u8>, AotArtifactError> {
    let mut out = Vec::new();
    write_u32("program constants", program.constants.len(), &mut out)?;
    for constant in &program.constants {
        write_value(constant, &mut out)?;
    }
    write_u32("program code length", program.code.len(), &mut out)?;
    out.extend_from_slice(&program.code);
    write_u32("program imports", program.imports.len(), &mut out)?;
    for import in &program.imports {
        write_string("import name", &import.name, &mut out)?;
        out.push(import.arity);
        out.push(import.return_type as u8);
    }
    write_type_map(program.type_map.as_ref(), &mut out)?;
    Ok(out)
}

fn decode_program_payload(bytes: &[u8]) -> Result<Program, AotArtifactError> {
    let mut cursor = Cursor::new(bytes);

    let constant_count = cursor.read_u32()? as usize;
    let mut constants = Vec::with_capacity(constant_count);
    for _ in 0..constant_count {
        constants.push(cursor.read_value()?);
    }

    let code_len = cursor.read_u32()? as usize;
    let code = cursor.read_exact(code_len)?.to_vec();

    let import_count = cursor.read_u32()? as usize;
    let mut imports = Vec::with_capacity(import_count);
    for _ in 0..import_count {
        imports.push(crate::bytecode::HostImport {
            name: cursor.read_string()?,
            arity: cursor.read_u8()?,
            return_type: read_value_type(cursor.read_u8()?)?,
        });
    }

    let type_map = read_type_map(&mut cursor)?;
    if !cursor.is_at_end() {
        return Err(AotArtifactError::TrailingBytes);
    }

    let mut program = Program::with_imports_and_debug(constants, code, imports, None);
    program.type_map = type_map;
    Ok(program)
}

fn write_value(value: &Value, out: &mut Vec<u8>) -> Result<(), AotArtifactError> {
    match value {
        Value::Null => out.push(0),
        Value::Int(value) => {
            out.push(1);
            out.extend_from_slice(&value.to_le_bytes());
        }
        Value::Float(value) => {
            out.push(2);
            out.extend_from_slice(&value.to_le_bytes());
        }
        Value::Bool(value) => {
            out.push(3);
            out.push(u8::from(*value));
        }
        Value::String(value) => {
            out.push(4);
            write_u32("value string", value.len(), out)?;
            out.extend_from_slice(value.as_bytes());
        }
        Value::Bytes(value) => {
            out.push(5);
            write_u32("value bytes", value.len(), out)?;
            out.extend_from_slice(value.as_slice());
        }
        Value::Array(values) => {
            out.push(6);
            write_u32("value array", values.len(), out)?;
            for value in values.iter() {
                write_value(value, out)?;
            }
        }
        Value::Map(entries) => {
            out.push(7);
            let mut encoded_entries = entries
                .iter()
                .map(|(key, value)| {
                    let mut entry_bytes = Vec::new();
                    write_value(key, &mut entry_bytes)?;
                    write_value(value, &mut entry_bytes)?;
                    Ok(entry_bytes)
                })
                .collect::<Result<Vec<_>, AotArtifactError>>()?;
            encoded_entries.sort_unstable();
            write_u32("value map", encoded_entries.len(), out)?;
            for entry in encoded_entries {
                out.extend_from_slice(&entry);
            }
        }
    }
    Ok(())
}

fn write_type_map(type_map: Option<&TypeMap>, out: &mut Vec<u8>) -> Result<(), AotArtifactError> {
    let Some(type_map) = type_map else {
        out.push(0);
        return Ok(());
    };

    out.push(1);
    out.push(u8::from(type_map.strict_types));
    write_u32("type map locals", type_map.local_types.len(), out)?;
    for ty in &type_map.local_types {
        out.push(*ty as u8);
    }
    for schema in &type_map.local_schemas {
        write_optional_schema(schema.as_ref(), out)?;
    }
    write_bool_slice("type map callable slots", &type_map.callable_slots, out)?;
    write_bool_slice("type map optional slots", &type_map.optional_slots, out)?;

    let mut operand_entries = type_map
        .operand_types
        .iter()
        .map(|(offset, pair)| (*offset, *pair))
        .collect::<Vec<_>>();
    operand_entries.sort_unstable_by_key(|(offset, _)| *offset);
    write_u32("type map operands", operand_entries.len(), out)?;
    for (offset, (lhs, rhs)) in operand_entries {
        write_u32("type map operand offset", offset, out)?;
        out.push(lhs as u8);
        out.push(rhs as u8);
    }
    Ok(())
}

fn read_type_map(cursor: &mut Cursor<'_>) -> Result<Option<TypeMap>, AotArtifactError> {
    let flag = cursor.read_u8()?;
    if flag == 0 {
        return Ok(None);
    }
    if flag != 1 {
        return Err(AotArtifactError::InvalidValueType(flag));
    }

    let strict_types = match cursor.read_u8()? {
        0 => false,
        1 => true,
        other => return Err(AotArtifactError::InvalidValueType(other)),
    };
    let local_count = cursor.read_u32()? as usize;
    let mut local_types = Vec::with_capacity(local_count);
    for _ in 0..local_count {
        local_types.push(read_value_type(cursor.read_u8()?)?);
    }
    let mut local_schemas = Vec::with_capacity(local_count);
    for _ in 0..local_count {
        local_schemas.push(read_optional_schema(cursor)?);
    }
    let callable_slots = read_bool_vec(cursor, local_count)?;
    let optional_slots = read_bool_vec(cursor, local_count)?;

    let operand_count = cursor.read_u32()? as usize;
    let mut operand_types = std::collections::HashMap::with_capacity(operand_count);
    for _ in 0..operand_count {
        let offset = cursor.read_u32()? as usize;
        let lhs = read_value_type(cursor.read_u8()?)?;
        let rhs = read_value_type(cursor.read_u8()?)?;
        operand_types.insert(offset, (lhs, rhs));
    }

    Ok(Some(TypeMap {
        strict_types,
        local_types,
        local_schemas,
        callable_slots,
        optional_slots,
        operand_types,
    }))
}

fn read_value_type(raw: u8) -> Result<ValueType, AotArtifactError> {
    match raw {
        0 => Ok(ValueType::Unknown),
        1 => Ok(ValueType::Null),
        2 => Ok(ValueType::Int),
        3 => Ok(ValueType::Float),
        4 => Ok(ValueType::Bool),
        5 => Ok(ValueType::String),
        6 => Ok(ValueType::Bytes),
        7 => Ok(ValueType::Array),
        8 => Ok(ValueType::Map),
        other => Err(AotArtifactError::InvalidValueType(other)),
    }
}

fn write_bool_slice(
    field: &'static str,
    values: &[bool],
    out: &mut Vec<u8>,
) -> Result<(), AotArtifactError> {
    write_u32(field, values.len(), out)?;
    out.extend(values.iter().map(|value| u8::from(*value)));
    Ok(())
}

fn read_bool_vec(
    cursor: &mut Cursor<'_>,
    expected_len: usize,
) -> Result<Vec<bool>, AotArtifactError> {
    let count = cursor.read_u32()? as usize;
    if count != expected_len {
        return Err(AotArtifactError::UnexpectedEof);
    }
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(match cursor.read_u8()? {
            0 => false,
            1 => true,
            other => return Err(AotArtifactError::InvalidValueType(other)),
        });
    }
    Ok(values)
}

fn write_optional_schema(
    schema: Option<&TypeSchema>,
    out: &mut Vec<u8>,
) -> Result<(), AotArtifactError> {
    match schema {
        Some(schema) => {
            out.push(1);
            write_schema(schema, out)?;
        }
        None => out.push(0),
    }
    Ok(())
}

fn read_optional_schema(cursor: &mut Cursor<'_>) -> Result<Option<TypeSchema>, AotArtifactError> {
    match cursor.read_u8()? {
        0 => Ok(None),
        1 => Ok(Some(read_schema(cursor)?)),
        other => Err(AotArtifactError::InvalidValueType(other)),
    }
}

fn write_schema(schema: &TypeSchema, out: &mut Vec<u8>) -> Result<(), AotArtifactError> {
    match schema {
        TypeSchema::Unknown => out.push(0),
        TypeSchema::Null => out.push(1),
        TypeSchema::Int => out.push(2),
        TypeSchema::Float => out.push(3),
        TypeSchema::Number => out.push(4),
        TypeSchema::Bool => out.push(5),
        TypeSchema::String => out.push(6),
        TypeSchema::Bytes => out.push(7),
        TypeSchema::Optional(inner) => {
            out.push(16);
            write_schema(inner, out)?;
        }
        TypeSchema::GenericParam(name) => {
            out.push(8);
            write_string("schema generic", name, out)?;
        }
        TypeSchema::Named(name, type_args) => {
            out.push(9);
            write_string("schema name", name, out)?;
            write_u32("schema type args", type_args.len(), out)?;
            for type_arg in type_args {
                write_schema(type_arg, out)?;
            }
        }
        TypeSchema::Array(item) => {
            out.push(10);
            write_schema(item, out)?;
        }
        TypeSchema::ArrayTuple(items) => {
            out.push(11);
            write_u32("schema tuple items", items.len(), out)?;
            for item in items {
                write_schema(item, out)?;
            }
        }
        TypeSchema::ArrayTupleRest { prefix, rest } => {
            out.push(12);
            write_u32("schema tuple prefix", prefix.len(), out)?;
            for item in prefix {
                write_schema(item, out)?;
            }
            write_schema(rest, out)?;
        }
        TypeSchema::Map(item) => {
            out.push(13);
            write_schema(item, out)?;
        }
        TypeSchema::Object(fields) => {
            out.push(14);
            let mut entries = fields.iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|(lhs, _), (rhs, _)| lhs.cmp(rhs));
            write_u32("schema object fields", entries.len(), out)?;
            for (name, value) in entries {
                write_string("schema object field", name, out)?;
                write_schema(value, out)?;
            }
        }
        TypeSchema::Callable { params, result } => {
            out.push(15);
            write_u32("schema callable params", params.len(), out)?;
            for param in params {
                write_schema(param, out)?;
            }
            write_schema(result, out)?;
        }
    }
    Ok(())
}

fn read_schema(cursor: &mut Cursor<'_>) -> Result<TypeSchema, AotArtifactError> {
    match cursor.read_u8()? {
        0 => Ok(TypeSchema::Unknown),
        1 => Ok(TypeSchema::Null),
        2 => Ok(TypeSchema::Int),
        3 => Ok(TypeSchema::Float),
        4 => Ok(TypeSchema::Number),
        5 => Ok(TypeSchema::Bool),
        6 => Ok(TypeSchema::String),
        7 => Ok(TypeSchema::Bytes),
        16 => Ok(TypeSchema::Optional(Box::new(read_schema(cursor)?))),
        8 => Ok(TypeSchema::GenericParam(cursor.read_string()?)),
        9 => {
            let name = cursor.read_string()?;
            let count = cursor.read_u32()? as usize;
            let mut type_args = Vec::with_capacity(count);
            for _ in 0..count {
                type_args.push(read_schema(cursor)?);
            }
            Ok(TypeSchema::Named(name, type_args))
        }
        10 => Ok(TypeSchema::Array(Box::new(read_schema(cursor)?))),
        11 => {
            let count = cursor.read_u32()? as usize;
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                items.push(read_schema(cursor)?);
            }
            Ok(TypeSchema::ArrayTuple(items))
        }
        12 => {
            let count = cursor.read_u32()? as usize;
            let mut prefix = Vec::with_capacity(count);
            for _ in 0..count {
                prefix.push(read_schema(cursor)?);
            }
            let rest = Box::new(read_schema(cursor)?);
            Ok(TypeSchema::ArrayTupleRest { prefix, rest })
        }
        13 => Ok(TypeSchema::Map(Box::new(read_schema(cursor)?))),
        14 => {
            let count = cursor.read_u32()? as usize;
            let mut fields = std::collections::HashMap::with_capacity(count);
            for _ in 0..count {
                let name = cursor.read_string()?;
                let value = read_schema(cursor)?;
                fields.insert(name, value);
            }
            Ok(TypeSchema::Object(fields))
        }
        15 => {
            let count = cursor.read_u32()? as usize;
            let mut params = Vec::with_capacity(count);
            for _ in 0..count {
                params.push(read_schema(cursor)?);
            }
            let result = Box::new(read_schema(cursor)?);
            Ok(TypeSchema::Callable { params, result })
        }
        other => Err(AotArtifactError::InvalidValueType(other)),
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], AotArtifactError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(AotArtifactError::UnexpectedEof)?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or(AotArtifactError::UnexpectedEof)?;
        self.offset = end;
        Ok(slice)
    }

    fn read_exact_array<const N: usize>(&mut self) -> Result<[u8; N], AotArtifactError> {
        let bytes = self.read_exact(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(bytes);
        Ok(out)
    }

    fn read_u8(&mut self) -> Result<u8, AotArtifactError> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16, AotArtifactError> {
        Ok(u16::from_le_bytes(self.read_exact_array::<2>()?))
    }

    fn read_u32(&mut self) -> Result<u32, AotArtifactError> {
        Ok(u32::from_le_bytes(self.read_exact_array::<4>()?))
    }

    fn read_u64(&mut self) -> Result<u64, AotArtifactError> {
        Ok(u64::from_le_bytes(self.read_exact_array::<8>()?))
    }

    fn read_string(&mut self) -> Result<String, AotArtifactError> {
        let len = self.read_u32()? as usize;
        String::from_utf8(self.read_exact(len)?.to_vec()).map_err(|_| AotArtifactError::InvalidUtf8)
    }

    fn read_i64(&mut self) -> Result<i64, AotArtifactError> {
        Ok(i64::from_le_bytes(self.read_exact_array::<8>()?))
    }

    fn read_f64(&mut self) -> Result<f64, AotArtifactError> {
        Ok(f64::from_le_bytes(self.read_exact_array::<8>()?))
    }

    fn read_value(&mut self) -> Result<Value, AotArtifactError> {
        match self.read_u8()? {
            0 => Ok(Value::Null),
            1 => Ok(Value::Int(self.read_i64()?)),
            2 => Ok(Value::Float(self.read_f64()?)),
            3 => match self.read_u8()? {
                0 => Ok(Value::Bool(false)),
                1 => Ok(Value::Bool(true)),
                other => Err(AotArtifactError::InvalidBool(other)),
            },
            4 => Ok(Value::string(self.read_string()?)),
            5 => {
                let len = self.read_u32()? as usize;
                Ok(Value::bytes(self.read_exact(len)?.to_vec()))
            }
            6 => {
                let len = self.read_u32()? as usize;
                let mut values = Vec::with_capacity(len);
                for _ in 0..len {
                    values.push(self.read_value()?);
                }
                Ok(Value::array(values))
            }
            7 => {
                let len = self.read_u32()? as usize;
                let mut entries = Vec::with_capacity(len);
                for _ in 0..len {
                    let key = self.read_value()?;
                    let value = self.read_value()?;
                    entries.push((key, value));
                }
                Ok(Value::map(entries))
            }
            other => Err(AotArtifactError::InvalidValueTag(other)),
        }
    }

    fn is_at_end(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BytecodeBuilder, Program, Value, ValueType};

    #[test]
    fn aot_artifact_decode_rejects_invalid_magic_and_trailing_bytes() {
        let mut bc = BytecodeBuilder::new();
        bc.ret();
        let mut vm = Vm::new(Program::new(vec![Value::Int(1)], bc.finish()));
        vm.compile_aot().expect("aot compile should succeed");

        let encoded = vm
            .encode_aot_artifact()
            .expect("artifact encode should succeed");

        let mut bad_magic = encoded.clone();
        bad_magic[0..4].copy_from_slice(b"NOPE");
        assert!(matches!(
            vm.load_aot_artifact(&bad_magic),
            Err(AotArtifactError::InvalidMagic(_))
        ));

        let mut trailing = encoded;
        trailing.extend_from_slice(&[1, 2, 3]);
        assert!(matches!(
            vm.load_aot_artifact(&trailing),
            Err(AotArtifactError::TrailingBytes)
        ));
    }

    #[test]
    fn aot_artifact_decode_rejects_incompatible_program_hash() {
        let mut first_bc = BytecodeBuilder::new();
        first_bc.ldc(0);
        first_bc.ret();
        let mut first_vm = Vm::new(Program::new(vec![Value::Int(1)], first_bc.finish()));
        first_vm
            .compile_aot()
            .expect("first aot compile should succeed");
        let encoded = first_vm
            .encode_aot_artifact()
            .expect("artifact encode should succeed");

        let mut second_bc = BytecodeBuilder::new();
        second_bc.ldc(0);
        second_bc.ldc(1);
        second_bc.add();
        second_bc.ret();
        let mut second_vm = Vm::new(Program::new(
            vec![Value::Int(1), Value::Int(2)],
            second_bc.finish(),
        ));

        assert!(matches!(
            second_vm.load_aot_artifact(&encoded),
            Err(AotArtifactError::IncompatibleProgramHash { .. })
        ));
    }

    #[test]
    fn aot_artifact_roundtrips_into_standalone_vm() {
        let mut bc = BytecodeBuilder::new();
        bc.ldc(0);
        bc.stloc(3);
        bc.ldloc(3);
        bc.ret();
        let program = Program::with_imports_and_debug(
            vec![Value::array(vec![
                Value::Int(1),
                Value::map(vec![(Value::string("k"), Value::Bool(true))]),
            ])],
            bc.finish(),
            vec![crate::bytecode::HostImport {
                name: "print".to_string(),
                arity: 1,
                return_type: ValueType::Unknown,
            }],
            None,
        )
        .with_local_count(8);
        let mut vm = Vm::new(program.clone());
        vm.compile_aot().expect("aot compile should succeed");

        let encoded = vm
            .encode_aot_artifact()
            .expect("artifact encode should succeed");
        let standalone = Vm::new_from_aot_artifact_with_jit_config(&encoded, JitConfig::default())
            .expect("standalone artifact should load");

        assert_eq!(standalone.program().local_count, 8);
        assert_eq!(standalone.program().constants, program.constants);
        assert_eq!(standalone.program().imports, program.imports);
        assert_eq!(standalone.program().type_map, program.type_map);
        assert!(
            standalone.has_aot_program(),
            "standalone vm should install aot"
        );
    }
}
