//! Pure TypeSchema wire codec. Program and host-schema encoders stay in
//! `vmbc`; this module is the only owner of TypeSchema tag order, depth and
//! blob checks, and it compiles without the VM runtime.

use std::collections::HashMap;
use std::hash::Hash;

use crate::host_api::ResourceTypeKey;

use super::ir::TypeSchema;

pub(crate) const MAX_WIRE_BLOB_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_WIRE_COUNT: usize = 1_000_000;
pub(crate) const MAX_WIRE_AGGREGATE_ITEMS: usize = 1_000_000;
pub(crate) const MAX_SCHEMA_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SchemaCodecError {
    UnexpectedEof,
    InvalidValueType(u8),
    InvalidResourceKey(String),
    InvalidUtf8,
    LengthTooLarge(&'static str, usize),
    TrailingBytes,
}

impl std::fmt::Display for SchemaCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaCodecError::UnexpectedEof => write!(f, "unexpected end of input"),
            SchemaCodecError::InvalidValueType(value) => write!(f, "invalid value type: {value}"),
            SchemaCodecError::InvalidResourceKey(reason) => {
                write!(f, "invalid resource key: {reason}")
            }
            SchemaCodecError::InvalidUtf8 => write!(f, "invalid utf-8 string"),
            SchemaCodecError::LengthTooLarge(field, len) => {
                write!(f, "{field} length too large: {len}")
            }
            SchemaCodecError::TrailingBytes => write!(f, "trailing bytes after program payload"),
        }
    }
}

impl std::error::Error for SchemaCodecError {}

pub(crate) trait SchemaReader {
    fn read_u8(&mut self) -> Result<u8, SchemaCodecError>;
    fn read_string(&mut self) -> Result<String, SchemaCodecError>;
    fn read_count(
        &mut self,
        field: &'static str,
        min_item_bytes: usize,
    ) -> Result<usize, SchemaCodecError>;
    fn read_count_with_overhead(
        &mut self,
        field: &'static str,
        min_item_bytes: usize,
        fixed_bytes: usize,
    ) -> Result<usize, SchemaCodecError>;
    fn debit_count(&mut self, field: &'static str, count: usize) -> Result<(), SchemaCodecError>;
    fn remaining(&self) -> usize;
}

pub(crate) fn encode_type_schema_bytes(schema: &TypeSchema) -> Result<Vec<u8>, SchemaCodecError> {
    let mut out = Vec::new();
    write_schema(schema, &mut out)?;
    Ok(out)
}

pub(crate) fn decode_type_schema_bytes(bytes: &[u8]) -> Result<TypeSchema, SchemaCodecError> {
    let mut cursor = SchemaCursor::new(bytes);
    let schema = read_schema(&mut cursor, 0)?;
    if cursor.remaining() != 0 {
        return Err(SchemaCodecError::TrailingBytes);
    }
    Ok(schema)
}

pub(crate) fn write_schema(schema: &TypeSchema, out: &mut Vec<u8>) -> Result<(), SchemaCodecError> {
    match schema {
        TypeSchema::Unknown => out.push(0),
        TypeSchema::Null => out.push(1),
        TypeSchema::Unit => out.push(20),
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
        TypeSchema::Option(inner) => {
            out.push(19);
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
            entries.sort_unstable_by_key(|(name, _)| *name);
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
        TypeSchema::Resource(key) => {
            out.push(17);
            write_string("schema resource key", key.as_str(), out)?;
        }
        TypeSchema::Result { ok, err } => {
            out.push(18);
            write_schema(ok, out)?;
            write_schema(err, out)?;
        }
    }
    Ok(())
}

pub(crate) fn read_schema(
    cursor: &mut impl SchemaReader,
    depth: usize,
) -> Result<TypeSchema, SchemaCodecError> {
    if depth >= MAX_SCHEMA_DEPTH {
        return Err(SchemaCodecError::LengthTooLarge(
            "schema nesting depth",
            depth,
        ));
    }
    cursor.debit_count("schema nodes", 1)?;
    let nested_depth = depth
        .checked_add(1)
        .ok_or(SchemaCodecError::LengthTooLarge(
            "schema nesting depth",
            depth,
        ))?;
    match cursor.read_u8()? {
        0 => Ok(TypeSchema::Unknown),
        1 => Ok(TypeSchema::Null),
        2 => Ok(TypeSchema::Int),
        3 => Ok(TypeSchema::Float),
        4 => Ok(TypeSchema::Number),
        5 => Ok(TypeSchema::Bool),
        6 => Ok(TypeSchema::String),
        7 => Ok(TypeSchema::Bytes),
        16 => Ok(TypeSchema::Optional(Box::new(read_schema(
            cursor,
            nested_depth,
        )?))),
        8 => Ok(TypeSchema::GenericParam(cursor.read_string()?)),
        9 => {
            let name = cursor.read_string()?;
            let count = cursor.read_count("schema type args", 1)?;
            let mut type_args = Vec::new();
            reserve_vec(&mut type_args, "schema type args", count)?;
            for _ in 0..count {
                type_args.push(read_schema(cursor, nested_depth)?);
            }
            Ok(TypeSchema::Named(name, type_args))
        }
        10 => Ok(TypeSchema::Array(Box::new(read_schema(
            cursor,
            nested_depth,
        )?))),
        11 => {
            let count = cursor.read_count("schema tuple items", 1)?;
            let mut items = Vec::new();
            reserve_vec(&mut items, "schema tuple items", count)?;
            for _ in 0..count {
                items.push(read_schema(cursor, nested_depth)?);
            }
            Ok(TypeSchema::ArrayTuple(items))
        }
        12 => {
            let count = cursor.read_count_with_overhead("schema tuple prefix", 1, 1)?;
            let mut prefix = Vec::new();
            reserve_vec(&mut prefix, "schema tuple prefix", count)?;
            for _ in 0..count {
                prefix.push(read_schema(cursor, nested_depth)?);
            }
            let rest = Box::new(read_schema(cursor, nested_depth)?);
            Ok(TypeSchema::ArrayTupleRest { prefix, rest })
        }
        13 => Ok(TypeSchema::Map(Box::new(read_schema(
            cursor,
            nested_depth,
        )?))),
        14 => {
            let count = cursor.read_count("schema object fields", 5)?;
            let mut fields = HashMap::new();
            reserve_map(&mut fields, "schema object fields", count)?;
            for _ in 0..count {
                let name = cursor.read_string()?;
                let value = read_schema(cursor, nested_depth)?;
                fields.insert(name, value);
            }
            Ok(TypeSchema::Object(fields))
        }
        15 => {
            let count = cursor.read_count_with_overhead("schema callable params", 1, 1)?;
            let mut params = Vec::new();
            reserve_vec(&mut params, "schema callable params", count)?;
            for _ in 0..count {
                params.push(read_schema(cursor, nested_depth)?);
            }
            let result = Box::new(read_schema(cursor, nested_depth)?);
            Ok(TypeSchema::Callable { params, result })
        }
        17 => {
            let key_text = cursor.read_string()?;
            let key = ResourceTypeKey::new(key_text)
                .map_err(|err| SchemaCodecError::InvalidResourceKey(err.to_string()))?;
            Ok(TypeSchema::Resource(key))
        }
        18 => Ok(TypeSchema::Result {
            ok: Box::new(read_schema(cursor, nested_depth)?),
            err: Box::new(read_schema(cursor, nested_depth)?),
        }),
        19 => Ok(TypeSchema::Option(Box::new(read_schema(
            cursor,
            nested_depth,
        )?))),
        20 => Ok(TypeSchema::Unit),
        other => Err(SchemaCodecError::InvalidValueType(other)),
    }
}

fn write_string(
    field: &'static str,
    value: &str,
    out: &mut Vec<u8>,
) -> Result<(), SchemaCodecError> {
    write_u32_len(field, value.len(), out)?;
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn write_u32_len(
    field: &'static str,
    len: usize,
    out: &mut Vec<u8>,
) -> Result<(), SchemaCodecError> {
    let len_u32 = u32::try_from(len).map_err(|_| SchemaCodecError::LengthTooLarge(field, len))?;
    out.extend_from_slice(&len_u32.to_le_bytes());
    Ok(())
}

fn write_u32_count(
    field: &'static str,
    count: usize,
    out: &mut Vec<u8>,
) -> Result<(), SchemaCodecError> {
    write_u32_len(field, count, out)
}

fn reserve_vec<T>(
    items: &mut Vec<T>,
    field: &'static str,
    count: usize,
) -> Result<(), SchemaCodecError> {
    items
        .try_reserve_exact(count)
        .map_err(|_| SchemaCodecError::LengthTooLarge(field, count))
}

fn reserve_map<K: Eq + Hash, V>(
    items: &mut HashMap<K, V>,
    field: &'static str,
    count: usize,
) -> Result<(), SchemaCodecError> {
    items
        .try_reserve(count)
        .map_err(|_| SchemaCodecError::LengthTooLarge(field, count))
}

struct SchemaCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
    remaining_budget: usize,
}

impl<'a> SchemaCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            offset: 0,
            remaining_budget: MAX_WIRE_AGGREGATE_ITEMS,
        }
    }

    fn read_u32(&mut self) -> Result<u32, SchemaCodecError> {
        let bytes = self.read_exact(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], SchemaCodecError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(SchemaCodecError::UnexpectedEof)?;
        if end > self.bytes.len() {
            return Err(SchemaCodecError::UnexpectedEof);
        }
        let slice = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(slice)
    }

    fn read_blob(&mut self, field: &'static str) -> Result<&'a [u8], SchemaCodecError> {
        let len = self.read_u32()? as usize;
        if len > MAX_WIRE_BLOB_BYTES {
            return Err(SchemaCodecError::LengthTooLarge(field, len));
        }
        self.read_exact(len)
    }

    fn validate_count_with_overhead(
        &self,
        field: &'static str,
        count: usize,
        min_item_bytes: usize,
        fixed_bytes: usize,
    ) -> Result<(), SchemaCodecError> {
        if count > MAX_WIRE_COUNT {
            return Err(SchemaCodecError::LengthTooLarge(field, count));
        }
        let item_bytes = count
            .checked_mul(min_item_bytes)
            .ok_or(SchemaCodecError::LengthTooLarge(field, count))?;
        let required_bytes = item_bytes
            .checked_add(fixed_bytes)
            .ok_or(SchemaCodecError::LengthTooLarge(field, count))?;
        if required_bytes > self.remaining() {
            return Err(SchemaCodecError::LengthTooLarge(field, count));
        }
        Ok(())
    }
}

impl SchemaReader for SchemaCursor<'_> {
    fn read_u8(&mut self) -> Result<u8, SchemaCodecError> {
        let value = self
            .bytes
            .get(self.offset)
            .ok_or(SchemaCodecError::UnexpectedEof)?;
        self.offset += 1;
        Ok(*value)
    }

    fn read_string(&mut self) -> Result<String, SchemaCodecError> {
        let bytes = self.read_blob("string")?;
        let text = std::str::from_utf8(bytes).map_err(|_| SchemaCodecError::InvalidUtf8)?;
        let mut owned = String::new();
        owned
            .try_reserve_exact(text.len())
            .map_err(|_| SchemaCodecError::LengthTooLarge("string", text.len()))?;
        owned.push_str(text);
        Ok(owned)
    }

    fn read_count(
        &mut self,
        field: &'static str,
        min_item_bytes: usize,
    ) -> Result<usize, SchemaCodecError> {
        self.read_count_with_overhead(field, min_item_bytes, 0)
    }

    fn read_count_with_overhead(
        &mut self,
        field: &'static str,
        min_item_bytes: usize,
        fixed_bytes: usize,
    ) -> Result<usize, SchemaCodecError> {
        let count = self.read_u32()? as usize;
        self.validate_count_with_overhead(field, count, min_item_bytes, fixed_bytes)?;
        self.debit_count(field, count)?;
        Ok(count)
    }

    fn debit_count(&mut self, field: &'static str, count: usize) -> Result<(), SchemaCodecError> {
        if count > MAX_WIRE_COUNT {
            return Err(SchemaCodecError::LengthTooLarge(field, count));
        }
        self.remaining_budget = self
            .remaining_budget
            .checked_sub(count)
            .ok_or(SchemaCodecError::LengthTooLarge(field, count))?;
        Ok(())
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Value;

    fn roundtrip(schema: &TypeSchema) -> TypeSchema {
        let bytes = encode_type_schema_bytes(schema).expect("schema should encode");
        decode_type_schema_bytes(&bytes).expect("schema should decode")
    }

    #[test]
    fn type_schema_tags_and_sorted_fields_keep_wire_bytes() {
        assert_eq!(
            encode_type_schema_bytes(&TypeSchema::Unit).unwrap(),
            vec![20]
        );
        assert_eq!(
            encode_type_schema_bytes(&TypeSchema::Option(Box::new(TypeSchema::Unit))).unwrap(),
            vec![19, 20]
        );
        assert_eq!(
            encode_type_schema_bytes(&TypeSchema::Result {
                ok: Box::new(TypeSchema::Int),
                err: Box::new(TypeSchema::String),
            })
            .unwrap(),
            vec![18, 2, 6]
        );
        assert_eq!(
            encode_type_schema_bytes(&TypeSchema::Map(Box::new(TypeSchema::Int))).unwrap(),
            vec![13, 2]
        );
        let mut fields = HashMap::new();
        fields.insert("b".to_string(), TypeSchema::Bool);
        fields.insert("a".to_string(), TypeSchema::Int);
        assert_eq!(
            encode_type_schema_bytes(&TypeSchema::Object(fields)).unwrap(),
            vec![14, 2, 0, 0, 0, 1, 0, 0, 0, b'a', 2, 1, 0, 0, 0, b'b', 5]
        );
        assert_eq!(
            encode_type_schema_bytes(&TypeSchema::Named("Box".to_string(), vec![TypeSchema::Int]))
                .unwrap(),
            vec![9, 3, 0, 0, 0, b'B', b'o', b'x', 1, 0, 0, 0, 2]
        );
        let key = ResourceTypeKey::new("io.file").unwrap();
        assert_eq!(
            encode_type_schema_bytes(&TypeSchema::Resource(key.clone())).unwrap(),
            vec![17, 7, 0, 0, 0, b'i', b'o', b'.', b'f', b'i', b'l', b'e']
        );
        assert_eq!(
            roundtrip(&TypeSchema::Resource(key)),
            TypeSchema::Resource(ResourceTypeKey::new("io.file").unwrap())
        );
    }

    #[test]
    fn type_schema_reader_keeps_depth_trailing_and_utf8_limits() {
        let mut nested = TypeSchema::Unit;
        for _ in 0..63 {
            nested = TypeSchema::Option(Box::new(nested));
        }
        assert_eq!(roundtrip(&nested), nested);

        let mut too_deep = TypeSchema::Unit;
        for _ in 0..64 {
            too_deep = TypeSchema::Option(Box::new(too_deep));
        }
        let encoded = encode_type_schema_bytes(&too_deep).unwrap();
        let error = decode_type_schema_bytes(&encoded).expect_err("depth 64 must fail");
        assert!(
            matches!(
                error,
                SchemaCodecError::LengthTooLarge("schema nesting depth", 64)
            ),
            "{error}"
        );

        let trailing = decode_type_schema_bytes(&[20, 0]).expect_err("trailing byte must fail");
        assert_eq!(trailing, SchemaCodecError::TrailingBytes);
        let tag = decode_type_schema_bytes(&[99]).expect_err("unknown tag must fail");
        assert_eq!(tag, SchemaCodecError::InvalidValueType(99));
        let utf8 =
            decode_type_schema_bytes(&[8, 1, 0, 0, 0, 0xff]).expect_err("invalid utf-8 must fail");
        assert_eq!(utf8, SchemaCodecError::InvalidUtf8);
    }

    fn constants_contain(program: &crate::Program, expected: &[u8]) -> bool {
        program.constants.iter().any(|value| match value {
            Value::Bytes(bytes) => bytes.as_slice() == expected,
            _ => false,
        })
    }

    #[test]
    fn compiler_only_json_and_print_embed_the_same_descriptor() {
        let unit = encode_type_schema_bytes(&TypeSchema::Unit).unwrap();
        let option_unit =
            encode_type_schema_bytes(&TypeSchema::Option(Box::new(TypeSchema::Unit))).unwrap();
        let encoded = crate::compile_source("use json; json::encode::<()>(());")
            .expect("compiler-only json encode should compile");
        assert!(
            constants_contain(&encoded.program, &unit),
            "json encode descriptor bytes diverged from the pure codec"
        );
        let printed = crate::compile_source("print(Option::Some(()));")
            .expect("compiler-only presentation should compile");
        assert!(
            constants_contain(&printed.program, &option_unit),
            "presentation descriptor bytes diverged from the pure codec"
        );
    }
}
