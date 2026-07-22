use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;

use crate::builtins::BuiltinFunction;
use crate::bytecode::{
    CallableKind, CallablePrototype, CallableTarget, CaptureBindingMode, ExportedCallable,
    FunctionRegion, RootCallableBinding, ScriptFunction, TypeMap, ValueType,
};
use crate::compiler::ir::TypeSchema;
use crate::debug_info::{ArgInfo, DebugFunction, DebugInfo, LineInfo, LocalInfo};
use crate::vm::{HostImport, OpCode, Program, Value};

const MAGIC: [u8; 4] = *b"VMBC";
const VERSION_V10: u16 = 10;
const FLAGS: u16 = 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    UnexpectedEof,
    InvalidMagic([u8; 4]),
    UnsupportedVersion(u16),
    UnsupportedFlags(u16),
    InvalidConstantTag(u8),
    InvalidBool(u8),
    InvalidTypeMapFlag(u8),
    InvalidDebugFlag(u8),
    InvalidValueType(u8),
    InvalidCaptureBindingMode(u8),
    InvalidUtf8,
    StringTooLong(usize),
    CodeTooLong(usize),
    UnsupportedConstantType(&'static str),
    LengthTooLarge(&'static str, usize),
    TrailingBytes,
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::UnexpectedEof => write!(f, "unexpected end of input"),
            WireError::InvalidMagic(found) => write!(f, "invalid magic: {found:?}"),
            WireError::UnsupportedVersion(version) => {
                write!(f, "unsupported version: {version}")
            }
            WireError::UnsupportedFlags(flags) => write!(f, "unsupported flags: {flags}"),
            WireError::InvalidConstantTag(tag) => write!(f, "invalid constant tag: {tag}"),
            WireError::InvalidBool(value) => write!(f, "invalid bool value: {value}"),
            WireError::InvalidTypeMapFlag(value) => write!(f, "invalid type-map flag: {value}"),
            WireError::InvalidDebugFlag(value) => write!(f, "invalid debug flag: {value}"),
            WireError::InvalidValueType(value) => write!(f, "invalid value type: {value}"),
            WireError::InvalidCaptureBindingMode(value) => {
                write!(f, "invalid capture binding mode: {value}")
            }
            WireError::InvalidUtf8 => write!(f, "invalid utf-8 string"),
            WireError::StringTooLong(len) => write!(f, "string too long: {len}"),
            WireError::CodeTooLong(len) => write!(f, "code too long: {len}"),
            WireError::UnsupportedConstantType(kind) => {
                write!(f, "unsupported constant type for wire format: {kind}")
            }
            WireError::LengthTooLarge(field, len) => {
                write!(f, "{field} length too large: {len}")
            }
            WireError::TrailingBytes => write!(f, "trailing bytes after program payload"),
        }
    }
}

impl std::error::Error for WireError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    TruncatedOperand {
        offset: usize,
        opcode: u8,
        expected_bytes: usize,
    },
    InvalidOpcode {
        offset: usize,
        opcode: u8,
    },
    InvalidConstant {
        offset: usize,
        index: u32,
    },
    InvalidCall {
        offset: usize,
        index: u16,
    },
    InvalidCallArity {
        offset: usize,
        index: u16,
        expected: u8,
        got: u8,
    },
    InvalidJumpTarget {
        offset: usize,
        target: u32,
    },
    InvalidCallableMetadata(&'static str),
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationError::TruncatedOperand {
                offset,
                opcode,
                expected_bytes,
            } => write!(
                f,
                "truncated operand at offset {offset} for opcode {opcode:#04x}, expected {expected_bytes} bytes",
            ),
            ValidationError::InvalidOpcode { offset, opcode } => {
                write!(f, "invalid opcode {opcode:#04x} at offset {offset}")
            }
            ValidationError::InvalidConstant { offset, index } => write!(
                f,
                "invalid constant index {index} for ldc instruction at offset {offset}",
            ),
            ValidationError::InvalidCall { offset, index } => {
                write!(f, "invalid call index {index} at offset {offset}")
            }
            ValidationError::InvalidCallArity {
                offset,
                index,
                expected,
                got,
            } => write!(
                f,
                "invalid call arity {got} for import index {index} at offset {offset}, expected {expected}",
            ),
            ValidationError::InvalidJumpTarget { offset, target } => write!(
                f,
                "invalid jump target {target} referenced by instruction at offset {offset}",
            ),
            ValidationError::InvalidCallableMetadata(message) => {
                write!(f, "invalid callable metadata: {message}")
            }
        }
    }
}

impl std::error::Error for ValidationError {}

const MAX_CONSTANT_DEPTH: usize = 64;

fn write_constant(value: &Value, out: &mut Vec<u8>, depth: usize) -> Result<(), WireError> {
    if depth >= MAX_CONSTANT_DEPTH {
        return Err(WireError::LengthTooLarge("constant nesting depth", depth));
    }
    match value {
        Value::Int(value) => {
            out.push(0);
            out.extend_from_slice(&value.to_le_bytes());
        }
        Value::Bool(value) => {
            out.push(1);
            out.push(u8::from(*value));
        }
        Value::String(value) => {
            out.push(2);
            write_u32_len("constant string", value.len(), out)?;
            out.extend_from_slice(value.as_bytes());
        }
        Value::Float(value) => {
            out.push(3);
            out.extend_from_slice(&value.to_le_bytes());
        }
        Value::Null => out.push(4),
        Value::Bytes(value) => {
            out.push(5);
            write_u32_len("constant bytes", value.len(), out)?;
            out.extend_from_slice(value.as_slice());
        }
        Value::Array(values) => {
            out.push(6);
            write_u32_count("constant array", values.len(), out)?;
            for value in values.iter() {
                write_constant(value, out, depth + 1)?;
            }
        }
        Value::Map(entries) => {
            out.push(7);
            write_u32_count("constant map", entries.len(), out)?;
            for (key, value) in entries.iter() {
                write_constant(key, out, depth + 1)?;
                write_constant(value, out, depth + 1)?;
            }
        }
        Value::Callable(_) => return Err(WireError::UnsupportedConstantType("callable")),
    }
    Ok(())
}

fn read_constant(cursor: &mut Cursor<'_>, depth: usize) -> Result<Value, WireError> {
    if depth >= MAX_CONSTANT_DEPTH {
        return Err(WireError::LengthTooLarge("constant nesting depth", depth));
    }
    match cursor.read_u8()? {
        0 => Ok(Value::Int(cursor.read_i64()?)),
        1 => match cursor.read_u8()? {
            0 => Ok(Value::Bool(false)),
            1 => Ok(Value::Bool(true)),
            other => Err(WireError::InvalidBool(other)),
        },
        2 => {
            let len = cursor.read_u32()? as usize;
            let bytes = cursor.read_exact(len)?;
            let text = String::from_utf8(bytes.to_vec()).map_err(|_| WireError::InvalidUtf8)?;
            Ok(Value::string(text))
        }
        3 => Ok(Value::Float(cursor.read_f64()?)),
        4 => Ok(Value::Null),
        5 => {
            let len = cursor.read_u32()? as usize;
            Ok(Value::bytes(cursor.read_exact(len)?.to_vec()))
        }
        6 => {
            let count = cursor.read_u32()? as usize;
            let mut values = Vec::with_capacity(count);
            for _ in 0..count {
                values.push(read_constant(cursor, depth + 1)?);
            }
            Ok(Value::array(values))
        }
        7 => {
            let count = cursor.read_u32()? as usize;
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                entries.push((
                    read_constant(cursor, depth + 1)?,
                    read_constant(cursor, depth + 1)?,
                ));
            }
            Ok(Value::map(entries))
        }
        tag => Err(WireError::InvalidConstantTag(tag)),
    }
}

pub fn encode_program(program: &Program) -> Result<Vec<u8>, WireError> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION_V10.to_le_bytes());
    out.extend_from_slice(&FLAGS.to_le_bytes());
    write_u32_count("constants", program.constants.len(), &mut out)?;

    for constant in &program.constants {
        write_constant(constant, &mut out, 0)?;
    }

    write_u32_len("code", program.code.len(), &mut out)?;
    out.extend_from_slice(&program.code);

    write_u32_count("imports", program.imports.len(), &mut out)?;
    for import in &program.imports {
        write_string("import name", &import.name, &mut out)?;
        out.push(import.arity);
        out.push(import.return_type as u8);
    }

    write_type_map(&mut out, program.type_map.as_ref())?;
    write_debug_info(&mut out, program.debug.as_ref())?;
    write_callable_metadata(&mut out, program)?;

    Ok(out)
}

pub fn decode_program(bytes: &[u8]) -> Result<Program, WireError> {
    let mut cursor = Cursor::new(bytes);

    let magic = cursor.read_exact_array::<4>()?;
    if magic != MAGIC {
        return Err(WireError::InvalidMagic(magic));
    }

    let version = cursor.read_u16()?;
    if version != VERSION_V10 {
        return Err(WireError::UnsupportedVersion(version));
    }

    let flags = cursor.read_u16()?;
    if flags != FLAGS {
        return Err(WireError::UnsupportedFlags(flags));
    }

    let constant_count = cursor.read_u32()? as usize;
    let mut constants = Vec::with_capacity(constant_count);
    for _ in 0..constant_count {
        constants.push(read_constant(&mut cursor, 0)?);
    }

    let code_len = cursor.read_u32()? as usize;
    let code = cursor.read_exact(code_len)?.to_vec();
    let import_count = cursor.read_u32()? as usize;
    let mut imports = Vec::with_capacity(import_count);
    for _ in 0..import_count {
        imports.push(HostImport {
            name: cursor.read_string()?,
            arity: cursor.read_u8()?,
            return_type: read_value_type(cursor.read_u8()?)?,
        });
    }
    let type_map = read_type_map(&mut cursor)?;
    let debug = read_debug_info(&mut cursor)?;
    let (
        script_functions,
        callable_prototypes,
        function_regions,
        root_callable_bindings,
        exported_callables,
    ) = read_callable_metadata(&mut cursor)?;

    if !cursor.is_eof() {
        return Err(WireError::TrailingBytes);
    }

    let mut program = Program::with_imports_and_debug(constants, code, imports, debug);
    program.type_map = type_map;
    program.script_functions = script_functions;
    program.callable_prototypes = callable_prototypes;
    program.function_regions = function_regions;
    program.root_callable_bindings = root_callable_bindings;
    program.exported_callables = exported_callables;
    let type_map_local_count = program
        .type_map
        .as_ref()
        .map_or(0, |type_map| type_map.local_types.len());
    let callable_local_count = program
        .root_callable_bindings
        .iter()
        .map(|binding| binding.local_slot as usize + 1)
        .chain(
            program
                .exported_callables
                .iter()
                .map(|exported| exported.local_slot as usize + 1),
        )
        .max()
        .unwrap_or(0);
    program.local_count = program
        .local_count
        .max(type_map_local_count)
        .max(callable_local_count);
    Ok(program)
}

pub fn validate_program(program: &Program, host_fn_count: u16) -> Result<(), ValidationError> {
    analyze_program(program, Some(host_fn_count)).map(|_| ())
}

pub fn infer_local_count(program: &Program) -> Result<usize, ValidationError> {
    let analysis = analyze_program(program, None)?;
    Ok(match analysis.max_local_index {
        Some(index) => index as usize + 1,
        None => 0,
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DisassembleOptions {
    pub show_source: bool,
}

pub fn disassemble_vmbc(bytes: &[u8]) -> Result<String, WireError> {
    disassemble_vmbc_with_options(bytes, DisassembleOptions::default())
}

pub fn disassemble_vmbc_with_options(
    bytes: &[u8],
    options: DisassembleOptions,
) -> Result<String, WireError> {
    let program = decode_program(bytes)?;
    Ok(disassemble_program_with_options(&program, options))
}

pub fn disassemble_program(program: &Program) -> String {
    disassemble_program_with_options(program, DisassembleOptions::default())
}

pub fn disassemble_program_with_options(program: &Program, options: DisassembleOptions) -> String {
    let mut out = String::new();
    let _ = writeln!(&mut out, "constants ({}):", program.constants.len());
    for (index, constant) in program.constants.iter().enumerate() {
        let _ = writeln!(&mut out, "  [{index:04}] {constant:?}");
    }

    let _ = writeln!(&mut out, "imports ({}):", program.imports.len());
    for (index, import) in program.imports.iter().enumerate() {
        let _ = writeln!(&mut out, "  [{index:04}] {}/{}", import.name, import.arity);
    }
    let _ = writeln!(&mut out, "code ({} bytes):", program.code.len());
    let mut source_annotations = source_annotations(program, options.show_source);
    if options.show_source && source_annotations.is_none() {
        let _ = writeln!(&mut out, "      ; source: <none>");
    }
    let code = &program.code;
    let mut ip = 0usize;
    while ip < code.len() {
        let start = ip;
        if let Some(lines_at_offset) = source_annotations
            .as_mut()
            .and_then(|annotations| annotations.remove(&start))
        {
            for (line, text) in lines_at_offset {
                let _ = writeln!(&mut out, "      ; src {line:04}  {text}");
            }
        }
        let opcode = code[ip];
        ip += 1;

        let mut instruction = String::new();
        let mut truncated = false;
        match opcode {
            x if x == OpCode::Nop as u8 => instruction.push_str("nop"),
            x if x == OpCode::Ret as u8 => instruction.push_str("ret"),
            x if x == OpCode::Ldc as u8 => {
                if let Some(index) = read_u32(code, &mut ip) {
                    instruction.push_str(&format!("ldc {index}"));
                    if let Some(value) = program.constants.get(index as usize) {
                        instruction.push_str(&format!(" ; const[{index}]={value:?}"));
                    }
                } else {
                    instruction.push_str("ldc <truncated>");
                    truncated = true;
                }
            }
            x if x == OpCode::Add as u8 => instruction.push_str("add"),
            x if x == OpCode::Sub as u8 => instruction.push_str("sub"),
            x if x == OpCode::Mul as u8 => instruction.push_str("mul"),
            x if x == OpCode::Div as u8 => instruction.push_str("div"),
            x if x == OpCode::Neg as u8 => instruction.push_str("neg"),
            x if x == OpCode::Not as u8 => instruction.push_str("not"),
            x if x == OpCode::Ceq as u8 => instruction.push_str("ceq"),
            x if x == OpCode::Clt as u8 => instruction.push_str("clt"),
            x if x == OpCode::Cgt as u8 => instruction.push_str("cgt"),
            x if x == OpCode::Br as u8 => {
                if let Some(target) = read_u32(code, &mut ip) {
                    instruction.push_str(&format!("br {target}"));
                } else {
                    instruction.push_str("br <truncated>");
                    truncated = true;
                }
            }
            x if x == OpCode::Brfalse as u8 => {
                if let Some(target) = read_u32(code, &mut ip) {
                    instruction.push_str(&format!("brfalse {target}"));
                } else {
                    instruction.push_str("brfalse <truncated>");
                    truncated = true;
                }
            }
            x if x == OpCode::Pop as u8 => instruction.push_str("pop"),
            x if x == OpCode::Dup as u8 => instruction.push_str("dup"),
            x if x == OpCode::Ldloc as u8 => {
                if let Some(index) = read_u8(code, &mut ip) {
                    instruction.push_str(&format!("ldloc {index}"));
                } else {
                    instruction.push_str("ldloc <truncated>");
                    truncated = true;
                }
            }
            x if x == OpCode::Stloc as u8 => {
                if let Some(index) = read_u8(code, &mut ip) {
                    instruction.push_str(&format!("stloc {index}"));
                } else {
                    instruction.push_str("stloc <truncated>");
                    truncated = true;
                }
            }
            x if x == OpCode::Call as u8 => {
                if let Some(index) = read_u16(code, &mut ip) {
                    if let Some(argc) = read_u8(code, &mut ip) {
                        instruction.push_str(&format!("call {index} {argc}"));
                        if let Some(comment) = format_call_target(program, index, argc) {
                            instruction.push_str(&format!(" ; {comment}"));
                        }
                    } else {
                        instruction.push_str("call <truncated>");
                        truncated = true;
                    }
                } else {
                    instruction.push_str("call <truncated>");
                    truncated = true;
                }
            }
            x if x == OpCode::CallValue as u8 => {
                if let Some(argc) = read_u8(code, &mut ip) {
                    instruction.push_str(&format!("callvalue {argc}"));
                } else {
                    instruction.push_str("callvalue <truncated>");
                    truncated = true;
                }
            }

            x if x == OpCode::Shl as u8 => instruction.push_str("shl"),
            x if x == OpCode::Shr as u8 => instruction.push_str("shr"),
            x if x == OpCode::Lshr as u8 => instruction.push_str("lshr"),
            x if x == OpCode::Mod as u8 => instruction.push_str("mod"),
            x if x == OpCode::And as u8 => instruction.push_str("and"),
            x if x == OpCode::Or as u8 => instruction.push_str("or"),
            other => instruction.push_str(&format!(".byte 0x{other:02X} ; invalid opcode")),
        }

        let encoded = format_hex_bytes(&code[start..ip]);
        let _ = writeln!(&mut out, "{start:04}\t{encoded:<14}\t{instruction}");
        if truncated {
            break;
        }
    }

    out
}

fn source_annotations(
    program: &Program,
    show_source: bool,
) -> Option<BTreeMap<usize, Vec<(u32, String)>>> {
    if !show_source {
        return None;
    }
    let debug = program.debug.as_ref()?;
    let source = debug.source.as_ref()?;
    let source_lines = source.lines().collect::<Vec<_>>();
    let mut first_offset_by_line = HashMap::<u32, u32>::new();
    for info in &debug.lines {
        first_offset_by_line.entry(info.line).or_insert(info.offset);
    }
    let mut pairs = first_offset_by_line
        .into_iter()
        .map(|(line, offset)| (offset, line))
        .collect::<Vec<_>>();
    pairs.sort_by_key(|(offset, line)| (*offset, *line));

    let mut annotations = BTreeMap::<usize, Vec<(u32, String)>>::new();
    for (offset, line) in pairs {
        let text = source_lines
            .get(line.saturating_sub(1) as usize)
            .copied()
            .unwrap_or("<missing source line>")
            .to_string();
        annotations
            .entry(offset as usize)
            .or_default()
            .push((line, text));
    }
    Some(annotations)
}

struct ProgramAnalysis {
    max_local_index: Option<u8>,
}

fn region_index_for_ip(regions: &[FunctionRegion], ip: usize) -> Option<usize> {
    regions
        .iter()
        .position(|region| (region.start_ip as usize) <= ip && ip < region.end_ip as usize)
}

fn validate_callable_metadata(program: &Program) -> Result<(), ValidationError> {
    let code_len = program.code.len();
    let mut previous_end = 0usize;
    for region in &program.function_regions {
        let start = region.start_ip as usize;
        let end = region.end_ip as usize;
        if start < previous_end || start >= end || end > code_len {
            return Err(ValidationError::InvalidCallableMetadata(
                "function regions overlap or exceed bytecode bounds",
            ));
        }
        if let Some(prototype_id) = region.prototype_id
            && prototype_id as usize >= program.callable_prototypes.len()
        {
            return Err(ValidationError::InvalidCallableMetadata(
                "function region references an invalid prototype",
            ));
        }
        previous_end = end;
    }
    if !program.function_regions.is_empty()
        && (program.function_regions[0].start_ip != 0 || previous_end != code_len)
    {
        return Err(ValidationError::InvalidCallableMetadata(
            "function regions do not cover the complete bytecode",
        ));
    }

    for prototype in &program.callable_prototypes {
        if matches!(prototype.target, CallableTarget::ScriptFunction(_))
            && prototype.parameter_slots.len() != prototype.arity as usize
            || prototype.capture_source_slots.len() != prototype.capture_slots.len()
            || prototype.capture_modes.len() != prototype.capture_slots.len()
            || prototype
                .parameter_slots
                .iter()
                .chain(prototype.capture_source_slots.iter())
                .chain(prototype.capture_slots.iter())
                .any(|slot| *slot as usize >= prototype.frame_local_count)
            || prototype
                .self_slot
                .is_some_and(|slot| slot as usize >= prototype.frame_local_count)
        {
            return Err(ValidationError::InvalidCallableMetadata(
                "callable frame layout is invalid",
            ));
        }
        match prototype.target {
            CallableTarget::ScriptFunction(id) if id as usize >= program.script_functions.len() => {
                return Err(ValidationError::InvalidCallableMetadata(
                    "callable references an invalid script function",
                ));
            }
            CallableTarget::HostImport(id)
                if id as usize >= program.imports.len()
                    && BuiltinFunction::from_call_index(id).is_none() =>
            {
                return Err(ValidationError::InvalidCallableMetadata(
                    "callable references an invalid host import",
                ));
            }
            _ => {}
        }
    }

    for binding in &program.root_callable_bindings {
        if binding.local_slot as usize >= program.local_count
            || binding.prototype_id as usize >= program.callable_prototypes.len()
        {
            return Err(ValidationError::InvalidCallableMetadata(
                "root callable binding is invalid",
            ));
        }
    }
    let mut export_names = HashSet::new();
    for exported in &program.exported_callables {
        if exported.name.is_empty()
            || exported.local_slot as usize >= program.local_count
            || !export_names.insert(exported.name.as_str())
        {
            return Err(ValidationError::InvalidCallableMetadata(
                "exported callable metadata is invalid",
            ));
        }
    }
    Ok(())
}

fn analyze_program(
    program: &Program,
    host_fn_count: Option<u16>,
) -> Result<ProgramAnalysis, ValidationError> {
    validate_callable_metadata(program)?;
    let mut ip = 0usize;
    let mut instruction_starts = HashSet::new();
    let mut jump_targets: Vec<(usize, u32)> = Vec::new();
    let mut max_local_index: Option<u8> = None;
    let code = &program.code;

    while ip < code.len() {
        let start = ip;
        instruction_starts.insert(start);
        let opcode = code[ip];
        ip += 1;

        match opcode {
            x if x == OpCode::Nop as u8 || x == OpCode::Ret as u8 => {}
            x if x == OpCode::Ldc as u8 => {
                let index = read_u32(code, &mut ip).ok_or(ValidationError::TruncatedOperand {
                    offset: start,
                    opcode,
                    expected_bytes: 4,
                })?;
                if index as usize >= program.constants.len() {
                    return Err(ValidationError::InvalidConstant {
                        offset: start,
                        index,
                    });
                }
            }
            x if x == OpCode::Add as u8
                || x == OpCode::Sub as u8
                || x == OpCode::Mul as u8
                || x == OpCode::Div as u8
                || x == OpCode::Shl as u8
                || x == OpCode::Shr as u8
                || x == OpCode::Lshr as u8
                || x == OpCode::Mod as u8
                || x == OpCode::And as u8
                || x == OpCode::Or as u8
                || x == OpCode::Neg as u8
                || x == OpCode::Not as u8
                || x == OpCode::Ceq as u8
                || x == OpCode::Clt as u8
                || x == OpCode::Cgt as u8
                || x == OpCode::Pop as u8
                || x == OpCode::Dup as u8 => {}
            x if x == OpCode::Br as u8 || x == OpCode::Brfalse as u8 => {
                let target = read_u32(code, &mut ip).ok_or(ValidationError::TruncatedOperand {
                    offset: start,
                    opcode,
                    expected_bytes: 4,
                })?;
                jump_targets.push((start, target));
            }
            x if x == OpCode::Ldloc as u8 || x == OpCode::Stloc as u8 => {
                let index = read_u8(code, &mut ip).ok_or(ValidationError::TruncatedOperand {
                    offset: start,
                    opcode,
                    expected_bytes: 1,
                })?;
                max_local_index = Some(max_local_index.map_or(index, |prev| prev.max(index)));
            }
            x if x == OpCode::Call as u8 => {
                let index = read_u16(code, &mut ip).ok_or(ValidationError::TruncatedOperand {
                    offset: start,
                    opcode,
                    expected_bytes: 3,
                })?;
                let argc = read_u8(code, &mut ip).ok_or(ValidationError::TruncatedOperand {
                    offset: start,
                    opcode,
                    expected_bytes: 3,
                })?;
                if let Some(builtin) = BuiltinFunction::from_call_index(index) {
                    if !builtin.accepts_arity(argc) {
                        return Err(ValidationError::InvalidCallArity {
                            offset: start,
                            index,
                            expected: builtin.arity(),
                            got: argc,
                        });
                    }
                    continue;
                }
                if program.imports.is_empty() {
                    if let Some(host_fn_count) = host_fn_count
                        && index >= host_fn_count
                    {
                        return Err(ValidationError::InvalidCall {
                            offset: start,
                            index,
                        });
                    }
                } else {
                    let Some(import) = program.imports.get(index as usize) else {
                        return Err(ValidationError::InvalidCall {
                            offset: start,
                            index,
                        });
                    };
                    if argc != import.arity {
                        return Err(ValidationError::InvalidCallArity {
                            offset: start,
                            index,
                            expected: import.arity,
                            got: argc,
                        });
                    }
                }
            }
            x if x == OpCode::CallValue as u8 => {
                read_u8(code, &mut ip).ok_or(ValidationError::TruncatedOperand {
                    offset: start,
                    opcode,
                    expected_bytes: 1,
                })?;
            }

            other => {
                return Err(ValidationError::InvalidOpcode {
                    offset: start,
                    opcode: other,
                });
            }
        }
    }

    for (offset, target) in &jump_targets {
        let target = *target as usize;
        if target >= code.len() || !instruction_starts.contains(&target) {
            return Err(ValidationError::InvalidJumpTarget {
                offset: *offset,
                target: target as u32,
            });
        }
        if !program.function_regions.is_empty()
            && region_index_for_ip(&program.function_regions, *offset)
                != region_index_for_ip(&program.function_regions, target)
        {
            return Err(ValidationError::InvalidJumpTarget {
                offset: *offset,
                target: target as u32,
            });
        }
    }

    for function in &program.script_functions {
        let entry = function.entry_ip as usize;
        let end = function.end_ip as usize;
        if !instruction_starts.contains(&entry)
            || end > code.len()
            || (end < code.len() && !instruction_starts.contains(&end))
        {
            return Err(ValidationError::InvalidCallableMetadata(
                "script function boundary is not an instruction boundary",
            ));
        }
    }

    Ok(ProgramAnalysis { max_local_index })
}

fn write_callable_metadata(out: &mut Vec<u8>, program: &Program) -> Result<(), WireError> {
    write_u32_count("script functions", program.script_functions.len(), out)?;
    for function in &program.script_functions {
        out.extend_from_slice(&function.entry_ip.to_le_bytes());
        out.extend_from_slice(&function.end_ip.to_le_bytes());
    }

    write_u32_count(
        "callable prototypes",
        program.callable_prototypes.len(),
        out,
    )?;
    for prototype in &program.callable_prototypes {
        out.push(match prototype.kind {
            CallableKind::FunctionItem => 0,
            CallableKind::Closure => 1,
            CallableKind::HostFunction => 2,
        });
        match prototype.target {
            CallableTarget::ScriptFunction(id) => {
                out.push(0);
                out.extend_from_slice(&id.to_le_bytes());
            }
            CallableTarget::HostImport(id) => {
                out.push(1);
                out.extend_from_slice(&u32::from(id).to_le_bytes());
            }
        }
        out.push(prototype.arity);
        write_u32_count("callable frame locals", prototype.frame_local_count, out)?;
        write_u16_list("callable parameters", &prototype.parameter_slots, out)?;
        write_u16_list(
            "callable capture sources",
            &prototype.capture_source_slots,
            out,
        )?;
        write_u16_list("callable captures", &prototype.capture_slots, out)?;
        write_u32_count("callable capture modes", prototype.capture_modes.len(), out)?;
        for mode in &prototype.capture_modes {
            out.push(*mode as u8);
        }
        match prototype.self_slot {
            Some(slot) => {
                out.push(1);
                out.extend_from_slice(&slot.to_le_bytes());
            }
            None => out.push(0),
        }
        match &prototype.schema {
            Some(schema) => {
                out.push(1);
                write_schema(schema, out)?;
            }
            None => out.push(0),
        }
    }

    write_u32_count("function regions", program.function_regions.len(), out)?;
    for region in &program.function_regions {
        out.extend_from_slice(&region.start_ip.to_le_bytes());
        out.extend_from_slice(&region.end_ip.to_le_bytes());
        match region.prototype_id {
            Some(id) => {
                out.push(1);
                out.extend_from_slice(&id.to_le_bytes());
            }
            None => out.push(0),
        }
    }

    write_u32_count(
        "root callable bindings",
        program.root_callable_bindings.len(),
        out,
    )?;
    for binding in &program.root_callable_bindings {
        out.extend_from_slice(&binding.local_slot.to_le_bytes());
        out.extend_from_slice(&binding.prototype_id.to_le_bytes());
    }
    write_u32_count("exported callables", program.exported_callables.len(), out)?;
    for exported in &program.exported_callables {
        write_string("exported callable name", &exported.name, out)?;
        out.extend_from_slice(&exported.local_slot.to_le_bytes());
    }
    Ok(())
}

fn write_u16_list(field: &'static str, values: &[u16], out: &mut Vec<u8>) -> Result<(), WireError> {
    write_u32_count(field, values.len(), out)?;
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
    Ok(())
}

type CallableMetadata = (
    Vec<ScriptFunction>,
    Vec<CallablePrototype>,
    Vec<FunctionRegion>,
    Vec<RootCallableBinding>,
    Vec<ExportedCallable>,
);

fn read_callable_metadata(cursor: &mut Cursor<'_>) -> Result<CallableMetadata, WireError> {
    let function_count = cursor.read_u32()? as usize;
    let mut script_functions = Vec::with_capacity(function_count);
    for _ in 0..function_count {
        script_functions.push(ScriptFunction {
            entry_ip: cursor.read_u32()?,
            end_ip: cursor.read_u32()?,
        });
    }

    let prototype_count = cursor.read_u32()? as usize;
    let mut callable_prototypes = Vec::with_capacity(prototype_count);
    for _ in 0..prototype_count {
        let kind = match cursor.read_u8()? {
            0 => CallableKind::FunctionItem,
            1 => CallableKind::Closure,
            2 => CallableKind::HostFunction,
            other => return Err(WireError::InvalidValueType(other)),
        };
        let target_tag = cursor.read_u8()?;
        let target_id = cursor.read_u32()?;
        let target = match target_tag {
            0 => CallableTarget::ScriptFunction(target_id),
            1 => CallableTarget::HostImport(
                u16::try_from(target_id).map_err(|_| WireError::InvalidValueType(target_tag))?,
            ),
            other => return Err(WireError::InvalidValueType(other)),
        };
        let arity = cursor.read_u8()?;
        let frame_local_count = cursor.read_u32()? as usize;
        let parameter_slots = read_u16_list(cursor)?;
        let capture_source_slots = read_u16_list(cursor)?;
        let capture_slots = read_u16_list(cursor)?;
        let capture_mode_count = cursor.read_u32()? as usize;
        let mut capture_modes = Vec::with_capacity(capture_mode_count);
        for _ in 0..capture_mode_count {
            capture_modes.push(match cursor.read_u8()? {
                0 => CaptureBindingMode::Copy,
                1 => CaptureBindingMode::Borrow,
                2 => CaptureBindingMode::BorrowMut,
                3 => CaptureBindingMode::Move,
                other => return Err(WireError::InvalidCaptureBindingMode(other)),
            });
        }
        let self_slot = match cursor.read_u8()? {
            0 => None,
            1 => Some(cursor.read_u16()?),
            other => return Err(WireError::InvalidBool(other)),
        };
        let schema = match cursor.read_u8()? {
            0 => None,
            1 => Some(read_schema(cursor)?),
            other => return Err(WireError::InvalidBool(other)),
        };
        callable_prototypes.push(CallablePrototype {
            kind,
            target,
            arity,
            frame_local_count,
            parameter_slots,
            capture_source_slots,
            capture_slots,
            capture_modes,
            self_slot,
            schema,
        });
    }

    let region_count = cursor.read_u32()? as usize;
    let mut function_regions = Vec::with_capacity(region_count);
    for _ in 0..region_count {
        let start_ip = cursor.read_u32()?;
        let end_ip = cursor.read_u32()?;
        let prototype_id = match cursor.read_u8()? {
            0 => None,
            1 => Some(cursor.read_u32()?),
            other => return Err(WireError::InvalidBool(other)),
        };
        function_regions.push(FunctionRegion {
            start_ip,
            end_ip,
            prototype_id,
        });
    }

    let binding_count = cursor.read_u32()? as usize;
    let mut root_callable_bindings = Vec::with_capacity(binding_count);
    for _ in 0..binding_count {
        root_callable_bindings.push(RootCallableBinding {
            local_slot: cursor.read_u16()?,
            prototype_id: cursor.read_u32()?,
        });
    }
    let export_count = cursor.read_u32()? as usize;
    let mut exported_callables = Vec::with_capacity(export_count);
    for _ in 0..export_count {
        exported_callables.push(ExportedCallable {
            name: cursor.read_string()?,
            local_slot: cursor.read_u16()?,
        });
    }
    Ok((
        script_functions,
        callable_prototypes,
        function_regions,
        root_callable_bindings,
        exported_callables,
    ))
}

fn read_u16_list(cursor: &mut Cursor<'_>) -> Result<Vec<u16>, WireError> {
    let len = cursor.read_u32()? as usize;
    let mut values = Vec::with_capacity(len);
    for _ in 0..len {
        values.push(cursor.read_u16()?);
    }
    Ok(values)
}

fn write_debug_info(out: &mut Vec<u8>, debug: Option<&DebugInfo>) -> Result<(), WireError> {
    match debug {
        None => {
            out.push(0);
            Ok(())
        }
        Some(debug) => {
            out.push(1);

            match &debug.source {
                None => out.push(0),
                Some(source) => {
                    out.push(1);
                    write_string("debug source", source, out)?;
                }
            }

            write_u32_count("debug lines", debug.lines.len(), out)?;
            for line in &debug.lines {
                out.extend_from_slice(&line.offset.to_le_bytes());
                out.extend_from_slice(&line.line.to_le_bytes());
            }

            write_u32_count("debug functions", debug.functions.len(), out)?;
            for function in &debug.functions {
                write_string("debug function name", &function.name, out)?;
                write_u32_count("debug function args", function.args.len(), out)?;
                for arg in &function.args {
                    write_string("debug arg name", &arg.name, out)?;
                    out.push(arg.position);
                }
            }

            write_u32_count("debug locals", debug.locals.len(), out)?;
            for local in &debug.locals {
                write_string("debug local name", &local.name, out)?;
                out.push(local.index);
                write_optional_u32(local.declared_line, out);
                write_optional_u32(local.last_line, out);
            }

            Ok(())
        }
    }
}

fn read_debug_info(cursor: &mut Cursor<'_>) -> Result<Option<DebugInfo>, WireError> {
    let flag = cursor.read_u8()?;
    match flag {
        0 => Ok(None),
        1 => {
            let source = match cursor.read_u8()? {
                0 => None,
                1 => Some(cursor.read_string()?),
                other => return Err(WireError::InvalidDebugFlag(other)),
            };

            let line_count = cursor.read_u32()? as usize;
            let mut lines = Vec::with_capacity(line_count);
            for _ in 0..line_count {
                lines.push(LineInfo {
                    offset: cursor.read_u32()?,
                    line: cursor.read_u32()?,
                });
            }

            let function_count = cursor.read_u32()? as usize;
            let mut functions = Vec::with_capacity(function_count);
            for _ in 0..function_count {
                let name = cursor.read_string()?;
                let arg_count = cursor.read_u32()? as usize;
                let mut args = Vec::with_capacity(arg_count);
                for _ in 0..arg_count {
                    args.push(ArgInfo {
                        name: cursor.read_string()?,
                        position: cursor.read_u8()?,
                    });
                }
                functions.push(DebugFunction { name, args });
            }

            let local_count = cursor.read_u32()? as usize;
            let mut locals = Vec::with_capacity(local_count);
            for _ in 0..local_count {
                locals.push(LocalInfo {
                    name: cursor.read_string()?,
                    index: cursor.read_u8()?,
                    declared_line: read_optional_u32(cursor)?,
                    last_line: read_optional_u32(cursor)?,
                });
            }

            Ok(Some(DebugInfo {
                source,
                lines,
                functions,
                locals,
            }))
        }
        other => Err(WireError::InvalidDebugFlag(other)),
    }
}

fn write_type_map(out: &mut Vec<u8>, type_map: Option<&TypeMap>) -> Result<(), WireError> {
    let Some(type_map) = type_map else {
        out.push(0);
        return Ok(());
    };

    out.push(1);
    out.push(u8::from(type_map.strict_types));
    write_u32_count("type map locals", type_map.local_types.len(), out)?;
    for ty in &type_map.local_types {
        out.push(*ty as u8);
    }
    for schema in &type_map.local_schemas {
        write_optional_schema(schema.as_ref(), out)?;
    }
    write_bool_slice("type map callable slots", &type_map.callable_slots, out)?;
    write_bool_slice("type map optional slots", &type_map.optional_slots, out)?;

    write_u32_count("type map operands", type_map.operand_types.len(), out)?;
    let mut operand_entries = type_map
        .operand_types
        .iter()
        .map(|(offset, pair)| (*offset, *pair))
        .collect::<Vec<_>>();
    operand_entries.sort_unstable_by_key(|(offset, _)| *offset);
    for (offset, (lhs, rhs)) in operand_entries {
        write_u32_count("type map operand offset", offset, out)?;
        out.push(lhs as u8);
        out.push(rhs as u8);
    }
    Ok(())
}

fn read_type_map(cursor: &mut Cursor<'_>) -> Result<Option<TypeMap>, WireError> {
    match cursor.read_u8()? {
        0 => Ok(None),
        1 => {
            let strict_types = match cursor.read_u8()? {
                0 => false,
                1 => true,
                other => return Err(WireError::InvalidBool(other)),
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
            let mut operand_types = HashMap::with_capacity(operand_count);
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
        other => Err(WireError::InvalidTypeMapFlag(other)),
    }
}

fn read_value_type(raw: u8) -> Result<ValueType, WireError> {
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
        9 => Ok(ValueType::Callable),
        other => Err(WireError::InvalidValueType(other)),
    }
}

fn write_optional_u32(value: Option<u32>, out: &mut Vec<u8>) {
    match value {
        Some(value) => {
            out.push(1);
            out.extend_from_slice(&value.to_le_bytes());
        }
        None => out.push(0),
    }
}

fn read_optional_u32(cursor: &mut Cursor<'_>) -> Result<Option<u32>, WireError> {
    match cursor.read_u8()? {
        0 => Ok(None),
        1 => Ok(Some(cursor.read_u32()?)),
        other => Err(WireError::InvalidDebugFlag(other)),
    }
}

fn write_bool_slice(
    field: &'static str,
    values: &[bool],
    out: &mut Vec<u8>,
) -> Result<(), WireError> {
    write_u32_count(field, values.len(), out)?;
    out.extend(values.iter().map(|value| u8::from(*value)));
    Ok(())
}

fn read_bool_vec(cursor: &mut Cursor<'_>, expected_len: usize) -> Result<Vec<bool>, WireError> {
    let count = cursor.read_u32()? as usize;
    if count != expected_len {
        return Err(WireError::TrailingBytes);
    }
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(match cursor.read_u8()? {
            0 => false,
            1 => true,
            other => return Err(WireError::InvalidBool(other)),
        });
    }
    Ok(values)
}

fn write_optional_schema(schema: Option<&TypeSchema>, out: &mut Vec<u8>) -> Result<(), WireError> {
    match schema {
        Some(schema) => {
            out.push(1);
            write_schema(schema, out)?;
        }
        None => out.push(0),
    }
    Ok(())
}

fn read_optional_schema(cursor: &mut Cursor<'_>) -> Result<Option<TypeSchema>, WireError> {
    match cursor.read_u8()? {
        0 => Ok(None),
        1 => Ok(Some(read_schema(cursor)?)),
        other => Err(WireError::InvalidBool(other)),
    }
}

fn write_schema(schema: &TypeSchema, out: &mut Vec<u8>) -> Result<(), WireError> {
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
            write_u32_count("schema type args", type_args.len(), out)?;
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
            write_u32_count("schema tuple items", items.len(), out)?;
            for item in items {
                write_schema(item, out)?;
            }
        }
        TypeSchema::ArrayTupleRest { prefix, rest } => {
            out.push(12);
            write_u32_count("schema tuple prefix", prefix.len(), out)?;
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
            write_u32_count("schema object fields", entries.len(), out)?;
            for (name, value) in entries {
                write_string("schema object field", name, out)?;
                write_schema(value, out)?;
            }
        }
        TypeSchema::Callable { params, result } => {
            out.push(15);
            write_u32_count("schema callable params", params.len(), out)?;
            for param in params {
                write_schema(param, out)?;
            }
            write_schema(result, out)?;
        }
    }
    Ok(())
}

fn read_schema(cursor: &mut Cursor<'_>) -> Result<TypeSchema, WireError> {
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
            let mut fields = HashMap::with_capacity(count);
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
        other => Err(WireError::InvalidValueType(other)),
    }
}

fn write_string(field: &'static str, value: &str, out: &mut Vec<u8>) -> Result<(), WireError> {
    write_u32_len(field, value.len(), out)?;
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn write_u32_len(field: &'static str, len: usize, out: &mut Vec<u8>) -> Result<(), WireError> {
    let len_u32 = u32::try_from(len).map_err(|_| WireError::LengthTooLarge(field, len))?;
    out.extend_from_slice(&len_u32.to_le_bytes());
    Ok(())
}

fn write_u32_count(field: &'static str, count: usize, out: &mut Vec<u8>) -> Result<(), WireError> {
    write_u32_len(field, count, out)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_u8(&mut self) -> Result<u8, WireError> {
        let value = self
            .bytes
            .get(self.offset)
            .ok_or(WireError::UnexpectedEof)?;
        self.offset += 1;
        Ok(*value)
    }

    fn read_u16(&mut self) -> Result<u16, WireError> {
        let bytes = self.read_exact_array::<2>()?;
        Ok(u16::from_le_bytes(bytes))
    }

    fn read_u32(&mut self) -> Result<u32, WireError> {
        let bytes = self.read_exact_array::<4>()?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_i64(&mut self) -> Result<i64, WireError> {
        let bytes = self.read_exact_array::<8>()?;
        Ok(i64::from_le_bytes(bytes))
    }

    fn read_f64(&mut self) -> Result<f64, WireError> {
        let bytes = self.read_exact_array::<8>()?;
        Ok(f64::from_le_bytes(bytes))
    }

    fn read_string(&mut self) -> Result<String, WireError> {
        let len = self.read_u32()? as usize;
        let bytes = self.read_exact(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| WireError::InvalidUtf8)
    }

    fn read_exact_array<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        let bytes = self.read_exact(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(bytes);
        Ok(out)
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], WireError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(WireError::UnexpectedEof)?;
        if end > self.bytes.len() {
            return Err(WireError::UnexpectedEof);
        }
        let slice = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(slice)
    }

    fn is_eof(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

fn read_u8(code: &[u8], ip: &mut usize) -> Option<u8> {
    let value = *code.get(*ip)?;
    *ip += 1;
    Some(value)
}

fn read_u16(code: &[u8], ip: &mut usize) -> Option<u16> {
    let bytes = code.get(*ip..(*ip + 2))?;
    *ip += 2;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(code: &[u8], ip: &mut usize) -> Option<u32> {
    let bytes = code.get(*ip..(*ip + 4))?;
    *ip += 4;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn format_hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::new();
    for (idx, byte) in bytes.iter().enumerate() {
        if idx > 0 {
            out.push(' ');
        }
        out.push_str(&format!("{byte:02X}"));
    }
    out
}

fn format_call_target(program: &Program, index: u16, argc: u8) -> Option<String> {
    if let Some(builtin) = BuiltinFunction::from_call_index(index) {
        return Some(format!("builtin {}/{}", builtin.name(), builtin.arity()));
    }
    program
        .imports
        .get(index as usize)
        .map(|import| format!("import {}/{} (argc={argc})", import.name, import.arity))
}
