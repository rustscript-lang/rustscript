use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};

use super::super::{Value, VmError, VmResult};
use super::arg_string;
use crate::builtins::{
    BuiltinFunction, BuiltinNamespace, BuiltinNamespaceMember, BuiltinNamespaceRegistry,
};

pub(super) fn builtin_json_encode(args: &[Value]) -> VmResult<Vec<Value>> {
    let value = args
        .first()
        .ok_or_else(|| VmError::HostError("missing argument to json_encode".to_string()))?;
    let json_value = vm_to_json_value(value)?;
    let text = serde_json::to_string(&json_value)
        .map_err(|err| VmError::HostError(format!("json_encode failed: {err}")))?;
    Ok(vec![Value::String(text)])
}

pub(super) fn builtin_json_decode(args: &[Value]) -> VmResult<Vec<Value>> {
    let text = arg_string(args, 0, "json_decode input")?;
    let json_value = serde_json::from_str::<JsonValue>(text)
        .map_err(|err| VmError::HostError(format!("json_decode failed: {err}")))?;
    Ok(vec![json_to_vm_value(json_value)?])
}

fn vm_to_json_value(value: &Value) -> VmResult<JsonValue> {
    match value {
        Value::Null => Ok(JsonValue::Null),
        Value::Int(value) => Ok(JsonValue::Number((*value).into())),
        Value::Float(value) => {
            let number = JsonNumber::from_f64(*value).ok_or_else(|| {
                VmError::HostError("json_encode does not support NaN or infinity".to_string())
            })?;
            Ok(JsonValue::Number(number))
        }
        Value::Bool(value) => Ok(JsonValue::Bool(*value)),
        Value::String(value) => Ok(JsonValue::String(value.clone())),
        Value::Array(values) => values
            .iter()
            .map(vm_to_json_value)
            .collect::<VmResult<Vec<_>>>()
            .map(JsonValue::Array),
        Value::Map(entries) => {
            let mut out = JsonMap::new();
            for (key, value) in entries {
                let key = match key {
                    Value::String(key) => key,
                    _ => {
                        return Err(VmError::HostError(
                            "json_encode map keys must be strings".to_string(),
                        ));
                    }
                };
                if out.contains_key(key) {
                    return Err(VmError::HostError(format!(
                        "json_encode map keys must be unique strings; duplicate key '{key}'"
                    )));
                }
                out.insert(key.clone(), vm_to_json_value(value)?);
            }
            Ok(JsonValue::Object(out))
        }
    }
}

fn json_to_vm_value(value: JsonValue) -> VmResult<Value> {
    match value {
        JsonValue::Null => Ok(Value::Null),
        JsonValue::Bool(value) => Ok(Value::Bool(value)),
        JsonValue::Number(value) => {
            if let Some(int_value) = value.as_i64() {
                return Ok(Value::Int(int_value));
            }
            if let Some(float_value) = value.as_f64() {
                return Ok(Value::Float(float_value));
            }
            Err(VmError::HostError(
                "json_decode number is out of supported range".to_string(),
            ))
        }
        JsonValue::String(value) => Ok(Value::String(value)),
        JsonValue::Array(values) => values
            .into_iter()
            .map(json_to_vm_value)
            .collect::<VmResult<Vec<_>>>()
            .map(Value::Array),
        JsonValue::Object(entries) => entries
            .into_iter()
            .map(|(key, value)| Ok((Value::String(key), json_to_vm_value(value)?)))
            .collect::<VmResult<Vec<_>>>()
            .map(Value::Map),
    }
}

const NAMESPACE_MEMBERS: &[BuiltinNamespaceMember] = &[
    BuiltinNamespaceMember::new("encode", BuiltinFunction::JsonEncode),
    BuiltinNamespaceMember::new("decode", BuiltinFunction::JsonDecode),
];

pub(crate) fn register_builtin_namespace(registry: &mut BuiltinNamespaceRegistry) {
    registry.register(BuiltinNamespace::new("json", NAMESPACE_MEMBERS, false));
}
