use alloc::string::String;
use alloc::vec::Vec;

use super::{HostImport, Program, Value, ValueType, WireError};

const MAGIC: [u8; 4] = *b"VMBC";
const VERSION_V8: u16 = 8;
const FLAGS: u16 = 0;
const MAX_SCHEMA_DEPTH: usize = 64;

pub fn decode_program(bytes: &[u8]) -> Result<Program, WireError> {
    let mut cursor = Cursor::new(bytes);
    let magic = cursor.read_array::<4>()?;
    if magic != MAGIC {
        return Err(WireError::InvalidMagic(magic));
    }

    let version = cursor.read_u16()?;
    if version != VERSION_V8 {
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

    skip_type_map(&mut cursor)?;
    skip_debug_info(&mut cursor)?;
    if !cursor.is_empty() {
        return Err(WireError::TrailingBytes);
    }

    Ok(Program::new(constants, code, imports))
}

fn reserve<T>(items: &mut Vec<T>, field: &'static str, count: usize) -> Result<(), WireError> {
    items
        .try_reserve_exact(count)
        .map_err(|_| WireError::LengthTooLarge(field, count))
}

fn read_value_type(raw: u8) -> Result<ValueType, WireError> {
    ValueType::try_from(raw).map_err(|()| WireError::InvalidValueType(raw))
}

fn skip_type_map(cursor: &mut Cursor<'_>) -> Result<(), WireError> {
    match cursor.read_u8()? {
        0 => Ok(()),
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
            Ok(())
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
