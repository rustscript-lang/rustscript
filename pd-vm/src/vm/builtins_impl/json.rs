use std::collections::HashSet;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
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
    let json_value = serde_json::from_str::<DecodedJsonValue>(text)
        .map_err(|err| VmError::HostError(format!("json_decode failed: {err}")))?;
    Ok(vec![json_value.0])
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

struct DecodedJsonValue(Value);

impl<'de> Deserialize<'de> for DecodedJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(JsonValueVisitor)
    }
}

struct JsonValueVisitor;

impl<'de> Visitor<'de> for JsonValueVisitor {
    type Value = DecodedJsonValue;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a valid JSON value supported by pd-vm")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(DecodedJsonValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(DecodedJsonValue(Value::Int(value)))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if let Ok(value) = i64::try_from(value) {
            Ok(DecodedJsonValue(Value::Int(value)))
        } else {
            Ok(DecodedJsonValue(Value::Float(value as f64)))
        }
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if value.is_finite() {
            Ok(DecodedJsonValue(Value::Float(value)))
        } else {
            Err(E::custom("json_decode number is out of supported range"))
        }
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(DecodedJsonValue(Value::String(value.to_string())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(DecodedJsonValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(DecodedJsonValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(DecodedJsonValue(Value::Null))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(DecodedJsonValue(value)) = seq.next_element::<DecodedJsonValue>()? {
            values.push(value);
        }
        Ok(DecodedJsonValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(de::Error::custom(format!(
                    "json_decode duplicate object key '{key}'"
                )));
            }
            let DecodedJsonValue(value) = map.next_value::<DecodedJsonValue>()?;
            entries.push((Value::String(key), value));
        }
        Ok(DecodedJsonValue(Value::Map(entries)))
    }
}

const NAMESPACE_MEMBERS: &[BuiltinNamespaceMember] = &[
    BuiltinNamespaceMember::new("encode", BuiltinFunction::JsonEncode),
    BuiltinNamespaceMember::new("decode", BuiltinFunction::JsonDecode),
];

pub(crate) fn register_builtin_namespace(registry: &mut BuiltinNamespaceRegistry) {
    registry.register(BuiltinNamespace::new("json", NAMESPACE_MEMBERS, false));
}
