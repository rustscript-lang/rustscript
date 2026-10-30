use std::collections::HashSet;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};

use super::{AnyValue, UnknownValue};
use crate::vm::{Value, VmError, VmResult};
use pd_host_function::pd_host_function;

/// Encodes a `Value` into a JSON string.
#[pd_host_function(name = "json::encode")]
pub(super) fn builtin_json_encode(value: &AnyValue) -> VmResult<String> {
    let json_value = vm_to_json_value(value)?;
    serde_json::to_string(&json_value)
        .map_err(|err| VmError::HostError(format!("json_encode failed: {err}")))
}

/// Decodes a JSON string into a `Value`.
/// Note: This function enforces a strict unique-keys contract for JSON objects.
/// If the JSON string contains duplicate keys, it will return an error instead of letting the last key win.
#[pd_host_function(name = "json::decode")]
pub(super) fn builtin_json_decode(text: &str) -> VmResult<UnknownValue> {
    decode_strict_value(text)
        .map_err(|err| VmError::HostError(format!("json_decode failed: {err}")))
}

/// Parses `text` as exactly one JSON value under the strict unique-key contract.
///
/// The ordered object-entry codec reuses this so both directions reject the
/// same inputs `json::decode` rejects: duplicate keys at any depth, unsupported
/// numbers, bad escapes, and trailing input.
pub(super) fn decode_strict_value(text: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str::<DecodedJsonValue>(text).map(|decoded| decoded.0)
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
        Value::String(value) => Ok(JsonValue::String(value.as_str().to_string())),
        Value::Bytes(_) => Err(VmError::HostError(
            "json_encode does not support bytes values".to_string(),
        )),
        Value::Array(values) => values
            .iter()
            .map(vm_to_json_value)
            .collect::<VmResult<Vec<_>>>()
            .map(JsonValue::Array),
        Value::Map(entries) => {
            let mut out = JsonMap::new();
            for (key, value) in entries.iter() {
                let key = match key {
                    Value::String(key) => key,
                    _ => {
                        return Err(VmError::HostError(
                            "json_encode map keys must be strings".to_string(),
                        ));
                    }
                };
                out.insert(key.as_str().to_string(), vm_to_json_value(value)?);
            }
            Ok(JsonValue::Object(out))
        }
        Value::Callable(_) => Err(VmError::HostError(
            "json_encode does not support callable values".to_string(),
        )),
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
        Ok(DecodedJsonValue(Value::string(value.to_string())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(DecodedJsonValue(Value::string(value)))
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
        Ok(DecodedJsonValue(Value::array(values)))
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
            entries.push((Value::string(key), value));
        }
        Ok(DecodedJsonValue(Value::map(entries)))
    }
}

// ---- standard host module ownership ---------------------------------------

/// The ordered-entry descriptors this module publishes as a guest surface.
const JSON_ENTRY_FUNCTIONS: &[fn() -> crate::host_extension::HostFunctionDescriptor] = &[
    super::json_validation::builtin_json_is_valid_descriptor,
    super::json_ordered::builtin_json_decode_object_entries_descriptor,
    super::json_ordered::builtin_json_encode_object_entries_descriptor,
];

/// The standard `json` host module: one descriptor owner per host
/// function.
///
/// `json::encode` / `json::decode` stay on the generated namespaced-builtin
/// path, so their static call indices are untouched. Only the ordered
/// object-entry codec publishes a guest catalog surface: it is compiled and
/// bound as a typed host import, which keeps the frozen static ID table and the
/// VMBC wire format unchanged.
pub(super) fn json_host_module() -> super::host_modules::StandardHostModule {
    use super::host_modules::StandardHostModule;

    const OWNED: &[fn() -> crate::host_extension::HostFunctionDescriptor] = &[
        builtin_json_encode_descriptor,
        builtin_json_decode_descriptor,
        super::json_validation::builtin_json_is_valid_descriptor,
        super::json_ordered::builtin_json_decode_object_entries_descriptor,
        super::json_ordered::builtin_json_encode_object_entries_descriptor,
    ];

    StandardHostModule {
        name: "json",
        catalog: || super::host_modules::catalog_module("json", JSON_ENTRY_FUNCTIONS, &[]),
        owned: OWNED,
        named_structs: super::json_ordered::JSON_NAMED_STRUCTS,
    }
}

/// Registers the ordered object-entry codec from the standard host catalog.
pub fn register_json_builtin_module(registry: &mut crate::HostFunctionRegistry) -> VmResult<()> {
    let catalog = crate::builtins::runtime::standard_host_catalog();
    register_json_builtin_module_from_catalog(registry, catalog.as_ref())
}

/// Registers the ordered object-entry codec using schemas from `catalog`.
///
/// `catalog` must be the snapshot the guest code was compiled against; the
/// installed adapters are the module's own descriptors, so a compile/bind pair
/// always agrees on import identity. A restricted registry keeps its
/// deny-by-default policy for every import this module does not declare.
pub fn register_json_builtin_module_from_catalog(
    registry: &mut crate::HostFunctionRegistry,
    catalog: &crate::host_api::HostApiCatalog,
) -> VmResult<()> {
    json_host_module()
        .catalog_module()
        .expect("the json module publishes a catalog surface")
        .install_from_catalog(registry, catalog)
        .map(|_| ())
}
