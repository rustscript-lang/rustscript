use alloc::string::String;
use alloc::vec::Vec;

use super::{
    CallableKind, CallablePrototype, CallableTarget, CaptureBindingMode, ExportedCallable,
    FunctionRegion, HostImport, Program, RootCallableBinding, ScriptFunction, Value, ValueType,
    WireError,
};

const MAGIC: [u8; 4] = *b"VMBC";
const VERSION_V10: u16 = 10;
const FLAGS: u16 = 0;
const MAX_SCHEMA_DEPTH: usize = 64;

pub fn decode_program(bytes: &[u8]) -> Result<Program, WireError> {
    let mut cursor = Cursor::new(bytes);
    let magic = cursor.read_array::<4>()?;
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
    let mut constants = Vec::new();
    reserve(&mut constants, "constants", constant_count)?;
    for _ in 0..constant_count {
        let value = match cursor.read_u8()? {
            0 => Value::Int(cursor.read_i64()?),
            1 => Value::Bool(cursor.read_bool()?),
            2 => Value::string(cursor.read_string()?),
            3 => Value::Float(cursor.read_f64()?),
            4 => Value::Null,
            5 => Value::bytes(cursor.read_blob()?.to_vec()),
            tag => return Err(WireError::InvalidConstantTag(tag)),
        };
        constants.push(value);
    }

    let code = cursor.read_blob()?.to_vec();
    let import_count = cursor.read_u32()? as usize;
    let mut imports = Vec::new();
    reserve(&mut imports, "imports", import_count)?;
    for _ in 0..import_count {
        imports.push(HostImport {
            name: cursor.read_string()?,
            arity: cursor.read_u8()?,
            return_type: read_value_type(cursor.read_u8()?)?,
        });
    }

    let encoded_local_count = skip_type_map(&mut cursor)?;
    skip_debug_info(&mut cursor)?;
    let (
        script_functions,
        callable_prototypes,
        function_regions,
        root_callable_bindings,
        exported_callables,
    ) = read_callable_metadata(&mut cursor)?;
    if !cursor.is_empty() {
        return Err(WireError::TrailingBytes);
    }

    let program = Program::new(constants, code, imports);
    let program = match encoded_local_count {
        Some(local_count) => program.with_local_count(local_count),
        None => program,
    };
    Ok(program.with_callable_metadata(
        script_functions,
        callable_prototypes,
        function_regions,
        root_callable_bindings,
        exported_callables,
    ))
}

fn reserve<T>(items: &mut Vec<T>, field: &'static str, count: usize) -> Result<(), WireError> {
    items
        .try_reserve_exact(count)
        .map_err(|_| WireError::LengthTooLarge(field, count))
}

fn read_value_type(raw: u8) -> Result<ValueType, WireError> {
    ValueType::try_from(raw).map_err(|()| WireError::InvalidValueType(raw))
}

fn skip_type_map(cursor: &mut Cursor<'_>) -> Result<Option<usize>, WireError> {
    match cursor.read_u8()? {
        0 => Ok(None),
        1 => {
            cursor.read_bool()?;
            let local_count = cursor.read_u32()? as usize;
            for _ in 0..local_count {
                read_value_type(cursor.read_u8()?)?;
            }
            for _ in 0..local_count {
                match cursor.read_u8()? {
                    0 => {}
                    1 => skip_schema(cursor, 0)?,
                    value => return Err(WireError::InvalidBool(value)),
                }
            }
            skip_bool_vector(cursor, local_count)?;
            skip_bool_vector(cursor, local_count)?;

            let operand_count = cursor.read_u32()? as usize;
            for _ in 0..operand_count {
                cursor.read_u32()?;
                read_value_type(cursor.read_u8()?)?;
                read_value_type(cursor.read_u8()?)?;
            }
            Ok(Some(local_count))
        }
        value => Err(WireError::InvalidTypeMapFlag(value)),
    }
}

fn skip_bool_vector(cursor: &mut Cursor<'_>, expected: usize) -> Result<(), WireError> {
    let count = cursor.read_u32()? as usize;
    if count != expected {
        return Err(WireError::TrailingBytes);
    }
    for _ in 0..count {
        cursor.read_bool()?;
    }
    Ok(())
}

fn skip_schema(cursor: &mut Cursor<'_>, depth: usize) -> Result<(), WireError> {
    if depth >= MAX_SCHEMA_DEPTH {
        return Err(WireError::SchemaTooDeep);
    }
    let nested_depth = depth + 1;
    match cursor.read_u8()? {
        0..=7 => Ok(()),
        8 => cursor.skip_string(),
        9 => {
            cursor.skip_string()?;
            let count = cursor.read_u32()? as usize;
            for _ in 0..count {
                skip_schema(cursor, nested_depth)?;
            }
            Ok(())
        }
        10 | 13 | 16 => skip_schema(cursor, nested_depth),
        11 => {
            let count = cursor.read_u32()? as usize;
            for _ in 0..count {
                skip_schema(cursor, nested_depth)?;
            }
            Ok(())
        }
        12 => {
            let count = cursor.read_u32()? as usize;
            for _ in 0..count {
                skip_schema(cursor, nested_depth)?;
            }
            skip_schema(cursor, nested_depth)
        }
        14 => {
            let count = cursor.read_u32()? as usize;
            for _ in 0..count {
                cursor.skip_string()?;
                skip_schema(cursor, nested_depth)?;
            }
            Ok(())
        }
        15 => {
            let count = cursor.read_u32()? as usize;
            for _ in 0..count {
                skip_schema(cursor, nested_depth)?;
            }
            skip_schema(cursor, nested_depth)
        }
        value => Err(WireError::InvalidValueType(value)),
    }
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
    let mut script_functions = Vec::new();
    reserve(&mut script_functions, "script functions", function_count)?;
    for _ in 0..function_count {
        script_functions.push(ScriptFunction {
            entry_ip: cursor.read_u32()?,
            end_ip: cursor.read_u32()?,
        });
    }

    let prototype_count = cursor.read_u32()? as usize;
    let mut prototypes = Vec::new();
    reserve(&mut prototypes, "callable prototypes", prototype_count)?;
    for _ in 0..prototype_count {
        let kind = match cursor.read_u8()? {
            0 => CallableKind::FunctionItem,
            1 => CallableKind::Closure,
            2 => CallableKind::HostFunction,
            value => return Err(WireError::InvalidValueType(value)),
        };
        let target_tag = cursor.read_u8()?;
        let target_id = cursor.read_u32()?;
        let target = match target_tag {
            0 => CallableTarget::ScriptFunction(target_id),
            1 => CallableTarget::HostImport(u16::try_from(target_id).map_err(|_| {
                WireError::LengthTooLarge("host callable target", target_id as usize)
            })?),
            value => return Err(WireError::InvalidValueType(value)),
        };
        let arity = cursor.read_u8()?;
        let frame_local_count = cursor.read_u32()? as usize;
        let parameter_count = cursor.read_u32()? as usize;
        let mut parameter_slots = Vec::new();
        reserve(&mut parameter_slots, "callable parameters", parameter_count)?;
        for _ in 0..parameter_count {
            parameter_slots.push(cursor.read_u16()?);
        }
        let capture_source_count = cursor.read_u32()? as usize;
        let mut capture_source_slots = Vec::new();
        reserve(
            &mut capture_source_slots,
            "callable capture sources",
            capture_source_count,
        )?;
        for _ in 0..capture_source_count {
            capture_source_slots.push(cursor.read_u16()?);
        }
        let capture_count = cursor.read_u32()? as usize;
        let mut capture_slots = Vec::new();
        reserve(&mut capture_slots, "callable captures", capture_count)?;
        for _ in 0..capture_count {
            capture_slots.push(cursor.read_u16()?);
        }
        let capture_mode_count = cursor.read_u32()? as usize;
        let mut capture_modes = Vec::new();
        reserve(
            &mut capture_modes,
            "callable capture modes",
            capture_mode_count,
        )?;
        for _ in 0..capture_mode_count {
            capture_modes.push(match cursor.read_u8()? {
                0 => CaptureBindingMode::Copy,
                1 => CaptureBindingMode::Borrow,
                2 => CaptureBindingMode::BorrowMut,
                3 => CaptureBindingMode::Move,
                other => return Err(WireError::InvalidCaptureBindingMode(other)),
            });
        }
        let self_slot = cursor.read_bool()?.then(|| cursor.read_u16()).transpose()?;
        if cursor.read_bool()? {
            skip_schema(cursor, 0)?;
        }
        prototypes.push(CallablePrototype {
            kind,
            target,
            arity,
            frame_local_count,
            parameter_slots,
            capture_source_slots,
            capture_slots,
            capture_modes,
            self_slot,
        });
    }

    let region_count = cursor.read_u32()? as usize;
    let mut regions = Vec::new();
    reserve(&mut regions, "function regions", region_count)?;
    for _ in 0..region_count {
        let start_ip = cursor.read_u32()?;
        let end_ip = cursor.read_u32()?;
        let prototype_id = cursor.read_bool()?.then(|| cursor.read_u32()).transpose()?;
        regions.push(FunctionRegion {
            start_ip,
            end_ip,
            prototype_id,
        });
    }

    let binding_count = cursor.read_u32()? as usize;
    let mut bindings = Vec::new();
    reserve(&mut bindings, "root callable bindings", binding_count)?;
    for _ in 0..binding_count {
        bindings.push(RootCallableBinding {
            local_slot: cursor.read_u16()?,
            prototype_id: cursor.read_u32()?,
        });
    }
    let export_count = cursor.read_u32()? as usize;
    let mut exported_callables = Vec::new();
    reserve(&mut exported_callables, "exported callables", export_count)?;
    for _ in 0..export_count {
        exported_callables.push(ExportedCallable {
            name: cursor.read_string()?,
            local_slot: cursor.read_u16()?,
        });
    }
    Ok((
        script_functions,
        prototypes,
        regions,
        bindings,
        exported_callables,
    ))
}

fn skip_debug_info(cursor: &mut Cursor<'_>) -> Result<(), WireError> {
    match cursor.read_u8()? {
        0 => Ok(()),
        1 => {
            match cursor.read_u8()? {
                0 => {}
                1 => cursor.skip_string()?,
                value => return Err(WireError::InvalidDebugFlag(value)),
            }

            let line_count = cursor.read_u32()? as usize;
            cursor.skip_count("debug lines", line_count, 8)?;

            let function_count = cursor.read_u32()? as usize;
            for _ in 0..function_count {
                cursor.skip_string()?;
                let arg_count = cursor.read_u32()? as usize;
                for _ in 0..arg_count {
                    cursor.skip_string()?;
                    cursor.read_u8()?;
                }
            }

            let local_count = cursor.read_u32()? as usize;
            for _ in 0..local_count {
                cursor.skip_string()?;
                cursor.read_u8()?;
                skip_optional_u32(cursor)?;
                skip_optional_u32(cursor)?;
            }
            Ok(())
        }
        value => Err(WireError::InvalidDebugFlag(value)),
    }
}

fn skip_optional_u32(cursor: &mut Cursor<'_>) -> Result<(), WireError> {
    match cursor.read_u8()? {
        0 => Ok(()),
        1 => {
            cursor.read_u32()?;
            Ok(())
        }
        value => Err(WireError::InvalidDebugFlag(value)),
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

    fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }

    fn read_u8(&mut self) -> Result<u8, WireError> {
        let value = *self
            .bytes
            .get(self.offset)
            .ok_or(WireError::UnexpectedEof)?;
        self.offset += 1;
        Ok(value)
    }

    fn read_bool(&mut self) -> Result<bool, WireError> {
        match self.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(WireError::InvalidBool(value)),
        }
    }

    fn read_u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_le_bytes(self.read_array()?))
    }

    fn read_u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.read_array()?))
    }

    fn read_i64(&mut self) -> Result<i64, WireError> {
        Ok(i64::from_le_bytes(self.read_array()?))
    }

    fn read_f64(&mut self) -> Result<f64, WireError> {
        Ok(f64::from_le_bytes(self.read_array()?))
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        let bytes = self.read_exact(N)?;
        bytes.try_into().map_err(|_| WireError::UnexpectedEof)
    }

    fn read_exact(&mut self, length: usize) -> Result<&'a [u8], WireError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(WireError::LengthTooLarge("payload", length))?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or(WireError::UnexpectedEof)?;
        self.offset = end;
        Ok(bytes)
    }

    fn read_blob(&mut self) -> Result<&'a [u8], WireError> {
        let length = self.read_u32()? as usize;
        self.read_exact(length)
    }

    fn read_string(&mut self) -> Result<String, WireError> {
        String::from_utf8(self.read_blob()?.to_vec()).map_err(|_| WireError::InvalidUtf8)
    }

    fn skip_string(&mut self) -> Result<(), WireError> {
        self.read_blob().map(|_| ())
    }

    fn skip_count(
        &mut self,
        field: &'static str,
        count: usize,
        item_size: usize,
    ) -> Result<(), WireError> {
        let length = count
            .checked_mul(item_size)
            .ok_or(WireError::LengthTooLarge(field, count))?;
        self.read_exact(length).map(|_| ())
    }
}
