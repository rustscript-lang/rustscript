//! Schema-directed JSON projection for compiler-private typed decode/encode.
//!
//! The public parser stays the strict unique-key visitor. This module only
//! projects that raw value onto a lossless `TypeSchema`, and encodes a typed
//! value back to natural JSON.

use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};

use crate::compiler::ir::{StructDecl, TypeSchema};
use crate::option::{none_carrier, payload, some_carrier};
use crate::vm::{Value, VmError, VmResult};

const MAX_JSON_DEPTH: usize = 64;
const MAX_JSON_NODES: usize = crate::host_api::MAX_HOST_SCHEMA_NODES;

pub(super) struct JsonNames<'a> {
    pub host: &'a std::collections::HashMap<String, TypeSchema>,
    pub guest: &'a std::collections::HashMap<String, StructDecl>,
}

pub(super) fn project_decoded(
    raw: &Value,
    descriptor: &[u8],
    names: JsonNames<'_>,
) -> VmResult<Value> {
    let schema = prepare_json_schema(descriptor, &names)?;
    charge_value(raw, &mut 0)?;
    project_value(raw, &schema, Position::Value, 0, &names)
}

pub(super) fn encode_typed(
    value: &Value,
    descriptor: &[u8],
    names: JsonNames<'_>,
) -> VmResult<String> {
    let schema = prepare_json_schema(descriptor, &names)?;
    charge_value(value, &mut 0)?;
    let json = encode_value(value, &schema, Position::Value, 0, &names)?;
    serde_json::to_string(&json)
        .map_err(|err| VmError::HostError(format!("typed json encode: {err}")))
}

fn decode_schema(descriptor: &[u8]) -> VmResult<TypeSchema> {
    crate::vmbc::decode_type_schema_bytes(descriptor)
        .map_err(|err| VmError::HostError(format!("typed json descriptor: {err}")))
}

fn prepare_json_schema(descriptor: &[u8], names: &JsonNames<'_>) -> VmResult<TypeSchema> {
    let schema = decode_schema(descriptor)?;
    let mut nodes = 0;
    let mut stack = Vec::new();
    let expanded = expand_named(&schema, names, &mut stack, &mut nodes)?;
    admit_json_schema(&expanded, &mut 0)?;
    Ok(expanded)
}

fn expand_named(
    schema: &TypeSchema,
    names: &JsonNames<'_>,
    stack: &mut Vec<String>,
    nodes: &mut usize,
) -> VmResult<TypeSchema> {
    charge_node(nodes)?;
    match schema {
        TypeSchema::Named(name, args) => {
            let key = format!("{name}/{}", args.len());
            let mut rendered_args = String::new();
            for arg in args {
                rendered_args.push_str(&format!("/{arg:?}"));
            }
            let key = format!("{key}{rendered_args}");
            if stack.iter().any(|seen| seen == &key) {
                return Ok(schema.clone());
            }
            let reentries = stack
                .iter()
                .filter(|seen| seen.starts_with(&format!("{name}/")))
                .count();
            if reentries >= 2 {
                return Ok(schema.clone());
            }
            let Some(body) =
                crate::vm::resolve_named_struct_body(names.host, names.guest, name, args)
            else {
                return Err(VmError::HostError(format!(
                    "typed json named schema '{name}' is not in the host catalog or guest program"
                )));
            };
            stack.push(key);
            let expanded = expand_named(&body, names, stack, nodes)?;
            stack.pop();
            Ok(expanded)
        }
        TypeSchema::Option(inner) => Ok(TypeSchema::Option(Box::new(expand_named(
            inner, names, stack, nodes,
        )?))),
        TypeSchema::Array(inner) => Ok(TypeSchema::Array(Box::new(expand_named(
            inner, names, stack, nodes,
        )?))),
        TypeSchema::Map(inner) => Ok(TypeSchema::Map(Box::new(expand_named(
            inner, names, stack, nodes,
        )?))),
        TypeSchema::Optional(inner) => Ok(TypeSchema::Optional(Box::new(expand_named(
            inner, names, stack, nodes,
        )?))),
        TypeSchema::ArrayTuple(items) => Ok(TypeSchema::ArrayTuple(
            items
                .iter()
                .map(|item| expand_named(item, names, stack, nodes))
                .collect::<VmResult<Vec<_>>>()?,
        )),
        TypeSchema::Object(fields) => {
            let mut expanded = std::collections::HashMap::new();
            for (name, field) in fields {
                expanded.insert(name.clone(), expand_named(field, names, stack, nodes)?);
            }
            Ok(TypeSchema::Object(expanded))
        }
        TypeSchema::Result { ok, err } => Ok(TypeSchema::Result {
            ok: Box::new(expand_named(ok, names, stack, nodes)?),
            err: Box::new(expand_named(err, names, stack, nodes)?),
        }),
        TypeSchema::Callable { params, result } => Ok(TypeSchema::Callable {
            params: params
                .iter()
                .map(|param| expand_named(param, names, stack, nodes))
                .collect::<VmResult<Vec<_>>>()?,
            result: Box::new(expand_named(result, names, stack, nodes)?),
        }),
        TypeSchema::ArrayTupleRest { prefix, rest } => Ok(TypeSchema::ArrayTupleRest {
            prefix: prefix
                .iter()
                .map(|item| expand_named(item, names, stack, nodes))
                .collect::<VmResult<Vec<_>>>()?,
            rest: Box::new(expand_named(rest, names, stack, nodes)?),
        }),
        other => Ok(other.clone()),
    }
}

fn admit_json_schema(schema: &TypeSchema, nodes: &mut usize) -> VmResult<()> {
    charge_node(nodes)?;
    match schema {
        TypeSchema::Result { .. }
        | TypeSchema::Bytes
        | TypeSchema::Callable { .. }
        | TypeSchema::Resource(_)
        | TypeSchema::Null
        | TypeSchema::Optional(_)
        | TypeSchema::Unknown
        | TypeSchema::GenericParam(_) => Err(unsupported(schema)),
        TypeSchema::Named(_, _) => Ok(()),
        TypeSchema::Option(inner) | TypeSchema::Array(inner) | TypeSchema::Map(inner) => {
            admit_json_schema(inner, nodes)
        }
        TypeSchema::ArrayTuple(items) => {
            for item in items {
                admit_json_schema(item, nodes)?;
            }
            Ok(())
        }
        TypeSchema::ArrayTupleRest { prefix, rest } => {
            for item in prefix {
                admit_json_schema(item, nodes)?;
            }
            admit_json_schema(rest, nodes)
        }
        TypeSchema::Object(fields) => {
            for field in fields.values() {
                admit_json_schema(field, nodes)?;
            }
            Ok(())
        }
        TypeSchema::Int
        | TypeSchema::Bool
        | TypeSchema::Float
        | TypeSchema::Number
        | TypeSchema::String
        | TypeSchema::Unit => Ok(()),
    }
}

fn charge_node(nodes: &mut usize) -> VmResult<()> {
    *nodes += 1;
    if *nodes > MAX_JSON_NODES {
        Err(VmError::HostError(
            "typed json schema exceeds the node budget".to_string(),
        ))
    } else {
        Ok(())
    }
}

fn charge_value(value: &Value, nodes: &mut usize) -> VmResult<()> {
    charge_node(nodes)?;
    match value {
        Value::Array(values) => {
            for value in values.iter() {
                charge_value(value, nodes)?;
            }
            Ok(())
        }
        Value::Map(entries) => {
            for (key, value) in entries.iter() {
                charge_value(key, nodes)?;
                charge_value(value, nodes)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Position {
    Value,
    Field,
}

fn project_value(
    raw: &Value,
    schema: &TypeSchema,
    position: Position,
    depth: usize,
    names: &JsonNames<'_>,
) -> VmResult<Value> {
    let depth = next_depth(depth)?;
    match schema {
        TypeSchema::Named(name, args) => {
            let Some(body) =
                crate::vm::resolve_named_struct_body(names.host, names.guest, name, args)
            else {
                return Err(unsupported(schema));
            };
            project_value(raw, &body, position, depth, names)
        }
        TypeSchema::Option(inner) => project_option(raw, inner, position, depth, names),
        TypeSchema::Int => project_int(raw),
        TypeSchema::Bool => match raw {
            Value::Bool(value) => Ok(Value::Bool(*value)),
            _ => Err(VmError::TypeMismatch("json bool")),
        },
        TypeSchema::Float | TypeSchema::Number => project_float(raw),
        TypeSchema::String => match raw {
            Value::String(value) => Ok(Value::String(value.clone())),
            _ => Err(VmError::TypeMismatch("json string")),
        },
        TypeSchema::Unit => match raw {
            Value::Null => Ok(Value::Null),
            _ => Err(VmError::TypeMismatch("json unit")),
        },
        TypeSchema::Array(inner) => project_array(raw, inner, depth, names),
        TypeSchema::ArrayTuple(items) => project_tuple(raw, items, depth, names),
        TypeSchema::Map(inner) => project_map(raw, inner, depth, names),
        TypeSchema::Object(fields) => project_object(raw, fields, depth, names),
        other => Err(unsupported(other)),
    }
}

fn project_option(
    raw: &Value,
    inner: &TypeSchema,
    position: Position,
    depth: usize,
    names: &JsonNames<'_>,
) -> VmResult<Value> {
    if position == Position::Value && null_ambiguous(inner) {
        return Err(ambiguous_option());
    }
    if matches!(raw, Value::Null) {
        return Ok(none_carrier());
    }
    Ok(some_carrier(project_value(
        raw,
        inner,
        Position::Value,
        depth,
        names,
    )?))
}

fn project_int(raw: &Value) -> VmResult<Value> {
    match raw {
        Value::Int(value) => Ok(Value::Int(*value)),
        Value::Float(value) => exact_i64(*value)
            .map(Value::Int)
            .ok_or(VmError::TypeMismatch("json int")),
        _ => Err(VmError::TypeMismatch("json int")),
    }
}

fn exact_i64(value: f64) -> Option<i64> {
    if !value.is_finite() || value.fract() != 0.0 {
        return None;
    }
    // i64::MAX is not an exact f64. Values at or above 2^63 are not i64.
    if value < i64::MIN as f64 || value >= 9223372036854775808.0 {
        return None;
    }
    let converted = value as i64;
    (converted as f64 == value).then_some(converted)
}

fn project_float(raw: &Value) -> VmResult<Value> {
    match raw {
        Value::Float(value) if value.is_finite() => Ok(Value::Float(*value)),
        Value::Int(value) => Ok(Value::Float(*value as f64)),
        _ => Err(VmError::TypeMismatch("json number")),
    }
}

fn project_array(
    raw: &Value,
    inner: &TypeSchema,
    depth: usize,
    names: &JsonNames<'_>,
) -> VmResult<Value> {
    let Value::Array(values) = raw else {
        return Err(VmError::TypeMismatch("json array"));
    };
    let mut out = Vec::with_capacity(values.len());
    for value in values.iter() {
        out.push(project_value(value, inner, Position::Value, depth, names)?);
    }
    Ok(Value::array(out))
}

fn project_tuple(
    raw: &Value,
    items: &[TypeSchema],
    depth: usize,
    names: &JsonNames<'_>,
) -> VmResult<Value> {
    let Value::Array(values) = raw else {
        return Err(VmError::TypeMismatch("json tuple"));
    };
    if values.len() != items.len() {
        return Err(VmError::HostError(format!(
            "typed json tuple length {}, schema expects {}",
            values.len(),
            items.len()
        )));
    }
    let mut out = Vec::with_capacity(items.len());
    for (value, schema) in values.iter().zip(items) {
        out.push(project_value(value, schema, Position::Value, depth, names)?);
    }
    Ok(Value::array(out))
}

fn project_map(
    raw: &Value,
    inner: &TypeSchema,
    depth: usize,
    names: &JsonNames<'_>,
) -> VmResult<Value> {
    if map_value_ambiguous(inner) {
        return Err(VmError::HostError(
            "typed json rejects Map<Option<Unit>> and other null-ambiguous map values".to_string(),
        ));
    }
    let Value::Map(entries) = raw else {
        return Err(VmError::TypeMismatch("json map"));
    };
    let mut out = Vec::with_capacity(entries.len());
    for (key, value) in entries.iter() {
        let Value::String(_) = key else {
            return Err(VmError::HostError(
                "typed json map keys must be strings".to_string(),
            ));
        };
        out.push((
            key.clone(),
            project_value(value, inner, Position::Value, depth, names)?,
        ));
    }
    Ok(Value::map(out))
}

fn project_object(
    raw: &Value,
    fields: &std::collections::HashMap<String, TypeSchema>,
    depth: usize,
    names: &JsonNames<'_>,
) -> VmResult<Value> {
    let Value::Map(entries) = raw else {
        return Err(VmError::TypeMismatch("json object"));
    };
    let mut present = std::collections::HashMap::new();
    for (key, value) in entries.iter() {
        let Value::String(name) = key else {
            return Err(VmError::HostError(
                "typed json object keys must be strings".to_string(),
            ));
        };
        if present.insert(name.as_str().to_string(), value).is_some() {
            return Err(VmError::HostError(format!(
                "typed json object has a duplicate key '{}'",
                name.as_str()
            )));
        }
    }
    let mut out = Vec::new();
    for (name, schema) in fields {
        match present.get(name) {
            Some(value) => {
                out.push((
                    Value::string(name.clone()),
                    project_field(value, schema, depth, names)?,
                ));
            }
            None => out.push((Value::string(name.clone()), missing_field(schema)?)),
        }
    }
    Ok(Value::map(out))
}

fn project_field(
    raw: &Value,
    schema: &TypeSchema,
    depth: usize,
    names: &JsonNames<'_>,
) -> VmResult<Value> {
    match field_form(schema)? {
        FieldForm::OrdinaryOption(inner) => {
            if matches!(raw, Value::Null) {
                Ok(none_carrier())
            } else {
                Ok(some_carrier(project_value(
                    raw,
                    inner,
                    Position::Value,
                    depth,
                    names,
                )?))
            }
        }
        FieldForm::OptionUnit => match raw {
            Value::Null => Ok(some_carrier(Value::Null)),
            _ => Err(VmError::TypeMismatch("json unit")),
        },
        FieldForm::OptionOption(inner) => {
            if matches!(raw, Value::Null) {
                Ok(some_carrier(none_carrier()))
            } else {
                Ok(some_carrier(some_carrier(project_value(
                    raw,
                    inner,
                    Position::Value,
                    depth,
                    names,
                )?)))
            }
        }
        FieldForm::Value => project_value(raw, schema, Position::Field, depth, names),
    }
}

fn missing_field(schema: &TypeSchema) -> VmResult<Value> {
    match field_form(schema)? {
        FieldForm::OrdinaryOption(_) | FieldForm::OptionUnit | FieldForm::OptionOption(_) => {
            Ok(none_carrier())
        }
        FieldForm::Value => Err(VmError::HostError(
            "typed json object is missing a required field".to_string(),
        )),
    }
}

enum FieldForm<'a> {
    OrdinaryOption(&'a TypeSchema),
    OptionUnit,
    OptionOption(&'a TypeSchema),
    Value,
}

fn field_form(schema: &TypeSchema) -> VmResult<FieldForm<'_>> {
    match schema {
        TypeSchema::Option(inner) => match inner.as_ref() {
            TypeSchema::Unit => Ok(FieldForm::OptionUnit),
            TypeSchema::Option(nested) => {
                if null_ambiguous(nested) {
                    Err(ambiguous_option())
                } else {
                    Ok(FieldForm::OptionOption(nested))
                }
            }
            other if null_ambiguous(other) => Err(ambiguous_option()),
            other => Ok(FieldForm::OrdinaryOption(other)),
        },
        other
            if matches!(
                other,
                TypeSchema::Null
                    | TypeSchema::Optional(_)
                    | TypeSchema::Unknown
                    | TypeSchema::GenericParam(_)
                    | TypeSchema::Bytes
                    | TypeSchema::Callable { .. }
                    | TypeSchema::Resource(_)
                    | TypeSchema::Result { .. }
            ) =>
        {
            Err(unsupported(other))
        }
        _ => Ok(FieldForm::Value),
    }
}

fn encode_value(
    value: &Value,
    schema: &TypeSchema,
    position: Position,
    depth: usize,
    names: &JsonNames<'_>,
) -> VmResult<JsonValue> {
    let depth = next_depth(depth)?;
    match schema {
        TypeSchema::Named(name, args) => {
            let Some(body) =
                crate::vm::resolve_named_struct_body(names.host, names.guest, name, args)
            else {
                return Err(unsupported(schema));
            };
            return encode_value(value, &body, position, depth, names);
        }
        TypeSchema::Option(inner) => encode_option(value, inner, position, depth, names),
        TypeSchema::Int => match value {
            Value::Int(number) => Ok(JsonValue::Number((*number).into())),
            _ => Err(VmError::TypeMismatch("json int")),
        },
        TypeSchema::Bool => match value {
            Value::Bool(flag) => Ok(JsonValue::Bool(*flag)),
            _ => Err(VmError::TypeMismatch("json bool")),
        },
        TypeSchema::Float | TypeSchema::Number => encode_float(value),
        TypeSchema::String => match value {
            Value::String(text) => Ok(JsonValue::String(text.as_str().to_string())),
            _ => Err(VmError::TypeMismatch("json string")),
        },
        TypeSchema::Unit => match value {
            Value::Null => Ok(JsonValue::Null),
            _ => Err(VmError::TypeMismatch("json unit")),
        },
        TypeSchema::Array(inner) => encode_array(
            value,
            std::slice::from_ref(inner.as_ref()),
            true,
            depth,
            names,
        ),
        TypeSchema::ArrayTuple(items) => encode_array(value, items, false, depth, names),
        TypeSchema::Map(inner) => encode_map(value, inner, depth, names),
        TypeSchema::Object(fields) => encode_object(value, fields, depth, names),
        other => Err(unsupported(other)),
    }
}

fn encode_option(
    value: &Value,
    inner: &TypeSchema,
    position: Position,
    depth: usize,
    names: &JsonNames<'_>,
) -> VmResult<JsonValue> {
    if position == Position::Value && null_ambiguous(inner) {
        return Err(ambiguous_option());
    }
    let (some, payload_value) = payload(value)?;
    if !some {
        return Ok(JsonValue::Null);
    }
    let Some(payload_value) = payload_value else {
        return Err(VmError::TypeMismatch("Option Some payload"));
    };
    encode_value(payload_value, inner, Position::Value, depth, names)
}

fn encode_float(value: &Value) -> VmResult<JsonValue> {
    let number = match value {
        Value::Float(number) if number.is_finite() => *number,
        Value::Int(number) => *number as f64,
        _ => return Err(VmError::TypeMismatch("json number")),
    };
    Ok(JsonValue::Number(JsonNumber::from_f64(number).ok_or_else(
        || VmError::HostError("typed json encode does not support NaN or infinity".to_string()),
    )?))
}

fn encode_array(
    value: &Value,
    items: &[TypeSchema],
    repeat: bool,
    depth: usize,
    names: &JsonNames<'_>,
) -> VmResult<JsonValue> {
    let Value::Array(values) = value else {
        return Err(VmError::TypeMismatch("json array"));
    };
    if !repeat && values.len() != items.len() {
        return Err(VmError::HostError(format!(
            "typed json tuple length {}, schema expects {}",
            values.len(),
            items.len()
        )));
    }
    let mut out = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let schema = if repeat { &items[0] } else { &items[index] };
        out.push(encode_value(value, schema, Position::Value, depth, names)?);
    }
    Ok(JsonValue::Array(out))
}

fn encode_map(
    value: &Value,
    inner: &TypeSchema,
    depth: usize,
    names: &JsonNames<'_>,
) -> VmResult<JsonValue> {
    if map_value_ambiguous(inner) {
        return Err(VmError::HostError(
            "typed json rejects Map<Option<Unit>> and other null-ambiguous map values".to_string(),
        ));
    }
    let Value::Map(entries) = value else {
        return Err(VmError::TypeMismatch("json map"));
    };
    let mut out = JsonMap::new();
    for (key, value) in entries.iter() {
        let Value::String(key) = key else {
            return Err(VmError::HostError(
                "typed json map keys must be strings".to_string(),
            ));
        };
        let encoded = encode_value(value, inner, Position::Value, depth, names)?;
        out.insert(key.as_str().to_string(), encoded);
    }
    Ok(JsonValue::Object(out))
}

fn encode_object(
    value: &Value,
    fields: &std::collections::HashMap<String, TypeSchema>,
    depth: usize,
    names: &JsonNames<'_>,
) -> VmResult<JsonValue> {
    let Value::Map(entries) = value else {
        return Err(VmError::TypeMismatch("json object"));
    };
    let mut present = std::collections::HashMap::new();
    for (key, value) in entries.iter() {
        let Value::String(name) = key else {
            return Err(VmError::HostError(
                "typed json object keys must be strings".to_string(),
            ));
        };
        present.insert(name.as_str().to_string(), value);
    }
    let mut out = JsonMap::new();
    for (name, schema) in fields {
        let encoded = match present.get(name) {
            Some(value) => encode_field(value, schema, depth, names)?,
            None => match field_form(schema)? {
                FieldForm::OrdinaryOption(_)
                | FieldForm::OptionUnit
                | FieldForm::OptionOption(_) => None,
                FieldForm::Value => {
                    return Err(VmError::HostError(
                        "typed json object is missing a required field".to_string(),
                    ));
                }
            },
        };
        if let Some(encoded) = encoded {
            out.insert(name.clone(), encoded);
        }
    }
    Ok(JsonValue::Object(out))
}

fn encode_field(
    value: &Value,
    schema: &TypeSchema,
    depth: usize,
    names: &JsonNames<'_>,
) -> VmResult<Option<JsonValue>> {
    match field_form(schema)? {
        FieldForm::OrdinaryOption(inner) => {
            let (some, payload_value) = payload(value)?;
            if !some {
                return Ok(None);
            }
            let Some(payload_value) = payload_value else {
                return Err(VmError::TypeMismatch("Option Some payload"));
            };
            Ok(Some(encode_value(
                payload_value,
                inner,
                Position::Value,
                depth,
                names,
            )?))
        }
        FieldForm::OptionUnit => {
            let (some, payload_value) = payload(value)?;
            if !some {
                return Ok(None);
            }
            match payload_value {
                Some(Value::Null) => Ok(Some(JsonValue::Null)),
                Some(_) => Err(VmError::TypeMismatch("json unit")),
                None => Err(VmError::TypeMismatch("Option Some payload")),
            }
        }
        FieldForm::OptionOption(inner) => {
            let (outer_some, outer_payload) = payload(value)?;
            if !outer_some {
                return Ok(None);
            }
            let Some(outer_payload) = outer_payload else {
                return Err(VmError::TypeMismatch("Option Some payload"));
            };
            let (inner_some, inner_payload) = payload(outer_payload)?;
            if !inner_some {
                return Ok(Some(JsonValue::Null));
            }
            let Some(inner_payload) = inner_payload else {
                return Err(VmError::TypeMismatch("Option Some payload"));
            };
            Ok(Some(encode_value(
                inner_payload,
                inner,
                Position::Value,
                depth,
                names,
            )?))
        }
        FieldForm::Value => Ok(Some(encode_value(
            value,
            schema,
            Position::Field,
            depth,
            names,
        )?)),
    }
}

fn next_depth(depth: usize) -> VmResult<usize> {
    if depth >= MAX_JSON_DEPTH {
        return Err(VmError::HostError(
            "typed json schema exceeds the nesting budget".to_string(),
        ));
    }
    Ok(depth + 1)
}

fn null_ambiguous(schema: &TypeSchema) -> bool {
    matches!(
        schema,
        TypeSchema::Unit
            | TypeSchema::Option(_)
            | TypeSchema::Null
            | TypeSchema::Optional(_)
            | TypeSchema::Unknown
            | TypeSchema::GenericParam(_)
    )
}

fn map_value_ambiguous(schema: &TypeSchema) -> bool {
    match schema {
        TypeSchema::Option(inner) => null_ambiguous(inner),
        TypeSchema::Null
        | TypeSchema::Optional(_)
        | TypeSchema::Unknown
        | TypeSchema::GenericParam(_) => true,
        _ => false,
    }
}

fn ambiguous_option() -> VmError {
    VmError::HostError(
        "typed json rejects value-position Option<Unit> or nested Option".to_string(),
    )
}

fn unsupported(schema: &TypeSchema) -> VmError {
    VmError::HostError(format!("typed json has no natural encoding for {schema:?}"))
}

pub(super) fn render_typed(
    value: &Value,
    descriptor: &[u8],
    names: JsonNames<'_>,
) -> VmResult<String> {
    let schema = decode_schema(descriptor)?;
    let mut nodes = 0;
    let mut stack = Vec::new();
    let schema = expand_named(&schema, &names, &mut stack, &mut nodes)?;
    render_value(value, &schema, 0, &names)
}

fn render_value(
    value: &Value,
    schema: &TypeSchema,
    depth: usize,
    names: &JsonNames<'_>,
) -> VmResult<String> {
    let depth = next_depth(depth)?;
    match schema {
        TypeSchema::Named(name, args) => {
            let Some(body) =
                crate::vm::resolve_named_struct_body(names.host, names.guest, name, args)
            else {
                return Err(unsupported(schema));
            };
            return render_value(value, &body, depth, names);
        }
        TypeSchema::Option(inner) => {
            let (some, payload_value) = payload(value)?;
            if !some {
                return Ok("None".to_string());
            }
            let Some(payload_value) = payload_value else {
                return Err(VmError::TypeMismatch("Option Some payload"));
            };
            let rendered = render_value(payload_value, inner, depth, names)?;
            Ok(format!("Some({rendered})"))
        }
        TypeSchema::Unit => match value {
            Value::Null => Ok("()".to_string()),
            _ => Err(VmError::TypeMismatch("json unit")),
        },
        TypeSchema::Int => match value {
            Value::Int(number) => Ok(number.to_string()),
            _ => Err(VmError::TypeMismatch("json int")),
        },
        TypeSchema::Bool => match value {
            Value::Bool(flag) => Ok(flag.to_string()),
            _ => Err(VmError::TypeMismatch("json bool")),
        },
        TypeSchema::String => match value {
            Value::String(text) => Ok(text.as_str().to_string()),
            _ => Err(VmError::TypeMismatch("json string")),
        },
        TypeSchema::Float | TypeSchema::Number => match value {
            Value::Float(number) if number.is_finite() => Ok(number.to_string()),
            Value::Int(number) => Ok((*number as f64).to_string()),
            _ => Err(VmError::TypeMismatch("json number")),
        },
        TypeSchema::Bytes => match value {
            Value::Bytes(bytes) => Ok(render_bytes(bytes.as_ref())),
            _ => Err(VmError::TypeMismatch("bytes")),
        },
        TypeSchema::Resource(_) => match value {
            Value::Int(token) => Ok(format!("resource<{token}>")),
            _ => Err(VmError::TypeMismatch("resource")),
        },
        TypeSchema::Callable { .. } => match value {
            Value::Callable(callable) => Ok(format!("<callable#{}>", callable.prototype_id)),
            _ => Err(VmError::TypeMismatch("callable")),
        },
        TypeSchema::Result { ok, err } => {
            let (is_ok, payload_value) = crate::result::payload(value)?;
            let rendered = render_value(payload_value, if is_ok { ok } else { err }, depth, names)?;
            Ok(if is_ok {
                format!("Ok({rendered})")
            } else {
                format!("Err({rendered})")
            })
        }
        TypeSchema::Array(inner) => match value {
            Value::Array(values) => {
                let mut parts = Vec::with_capacity(values.len());
                for value in values.iter() {
                    parts.push(render_value(value, inner, depth, names)?);
                }
                Ok(format!("[{}]", parts.join(", ")))
            }
            _ => Err(VmError::TypeMismatch("array")),
        },
        TypeSchema::Map(inner) => match value {
            Value::Map(entries) => {
                let mut parts = Vec::with_capacity(entries.len());
                for (key, value) in entries.iter() {
                    let Value::String(key) = key else {
                        return Err(VmError::TypeMismatch("map key"));
                    };
                    parts.push(format!(
                        "{}: {}",
                        key.as_str(),
                        render_value(value, inner, depth, names)?
                    ));
                }
                Ok(format!("{{{}}}", parts.join(", ")))
            }
            _ => Err(VmError::TypeMismatch("map")),
        },
        TypeSchema::Object(fields) => match value {
            Value::Map(entries) => {
                let mut parts = Vec::new();
                for (name, field) in fields {
                    let Some(field_value) = entries.get(&Value::string(name)) else {
                        continue;
                    };
                    parts.push(format!(
                        "{name}: {}",
                        render_value(field_value, field, depth, names)?
                    ));
                }
                Ok(format!("{{{}}}", parts.join(", ")))
            }
            _ => Err(VmError::TypeMismatch("object")),
        },
        other => Err(unsupported(other)),
    }
}

fn render_bytes(bytes: &[u8]) -> String {
    let parts = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!("b[{parts}]")
}
