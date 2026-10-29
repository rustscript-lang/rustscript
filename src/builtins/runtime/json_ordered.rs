//! Ordered JSON object-entry wire codec.
//!
//! [`Value::Map`](crate::Value) is a hash map: the document order of a decoded
//! JSON object is gone the moment it becomes a VM map, and re-encoding a map
//! emits hash-bucket order (or, with a sorted serde map, key order). Neither can
//! answer "what order were these members written in?", which a session file's
//! `tasks`/`cookies` objects need.
//!
//! This module adds exactly that capability as a JSON wire codec, not as a new
//! container: a decoded object becomes an ordered array of
//! [`JsonObjectEntry`] members (`key` plus the member's verified JSON value
//! text), and the encoder writes the entries back in the order the guest
//! supplied. The guest keeps ordinary `json::decode` / `json::encode` for
//! values; only object *member order* travels through this contract.
//!
//! Both directions use the standard serde parser and serializer
//! (`serde_json::value::RawValue` for a member's raw text and `SerializeMap`
//! for ordered output); there is no hand-written JSON tokenizer, escape table,
//! or ordering sidecar here. The existing strict unique-key contract in
//! [`super::json`] validates the whole document (root **and** nested objects),
//! so a duplicate key is rejected exactly as `json::decode` rejects it.
//!
//! # Cost
//!
//! A decode parses the input twice with the standard parser (strict validation,
//! then ordered member extraction); an encode validates each member's value once
//! and serializes once. Total work stays linear in the input/output bytes plus
//! entries, and no direction recurses per entry, so a wide object cannot consume
//! the script frame stack.

use std::collections::HashSet;

use serde::de::{Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeMap, Serializer};
use serde_json::value::RawValue;

use super::typed::VmArrayRef;
use crate::host_api::{
    HostFunctionSchema, HostParamSchema, HostStructField, HostStructSchema, HostTypeSchema,
};
use crate::host_extension::HostNamedStruct;
use crate::vm::{Value, VmError, VmResult};
use pd_host_function::pd_host_function;

/// One ordered member of a JSON object.
///
/// `key` is the member name after standard JSON unescaping; `value_json` is the
/// member's value as verified, complete JSON text. The pair is a JSON wire
/// carrier: a caller that needs a member's value decodes `value_json` with
/// `json::decode` (or recurses into this codec when the member is itself an
/// object whose order matters). Both fields are required and resource-free.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonObjectEntry {
    /// The member name, unescaped.
    pub key: String,
    /// The member's value, as one complete and validated JSON value.
    pub value_json: String,
}

impl HostNamedStruct for JsonObjectEntry {
    const NAME: &'static str = "JsonObjectEntry";

    fn host_struct_fields() -> Vec<HostStructField> {
        vec![
            HostStructField::new("key", HostTypeSchema::String),
            HostStructField::new("value_json", HostTypeSchema::String),
        ]
    }
}

/// The `JsonObjectEntry` body this module publishes.
pub(super) fn json_object_entry() -> HostStructSchema {
    HostStructSchema::new(JsonObjectEntry::NAME, JsonObjectEntry::host_struct_fields())
        .with_description(
            "One ordered JSON object member: its key and its verified JSON value text.",
        )
}

/// Array-of-entry schema used by both directions.
fn json_object_entry_array() -> HostTypeSchema {
    HostTypeSchema::Array(Box::new(json_object_entry().as_type()))
}

/// Guest contract for `json::decode_object_entries`.
pub(super) fn decode_object_entries_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "json::decode_object_entries",
        vec![HostParamSchema::value("text", HostTypeSchema::String)],
        json_object_entry_array(),
    )
    .with_description("Decodes a JSON object into its members in document order.")
}

/// Guest contract for `json::encode_object_entries`.
pub(super) fn encode_object_entries_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "json::encode_object_entries",
        vec![HostParamSchema::value("entries", json_object_entry_array())],
        HostTypeSchema::String,
    )
    .with_description("Encodes ordered members as a JSON object in the given order.")
}

// ---------------------------------------------------------------- codec core

/// Extracts an object's members in document order, borrowing each member's raw
/// JSON value text from the input.
///
/// A non-object root is rejected here by the standard deserializer, and
/// `end()` rejects trailing input.
fn ordered_members(text: &str) -> Result<Vec<(String, &RawValue)>, serde_json::Error> {
    struct OrderedMembersVisitor;

    impl<'de> Visitor<'de> for OrderedMembersVisitor {
        type Value = Vec<(String, &'de RawValue)>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a JSON object")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut members = Vec::with_capacity(map.size_hint().unwrap_or(0));
            // Keys are owned: a borrowed key would reject the escaped member
            // names standard JSON allows. Values stay borrowed raw text, which
            // keeps their original member order and lexically intact form.
            while let Some(key) = map.next_key::<String>()? {
                let value = map.next_value::<&'de RawValue>()?;
                members.push((key, value));
            }
            Ok(members)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_str(text);
    let members = deserializer.deserialize_map(OrderedMembersVisitor)?;
    deserializer.end()?;
    Ok(members)
}

/// Decodes a JSON object into its members in document order.
///
/// The whole document is validated first by the strict unique-key parser that
/// `json::decode` uses, so root and nested duplicate keys, unsupported numbers,
/// bad escapes, and trailing input are rejected here exactly as they are there.
pub(super) fn decode_object_entries(text: &str) -> Result<Vec<JsonObjectEntry>, String> {
    let value = super::json::decode_strict_value(text)
        .map_err(|error| format!("json_decode_object_entries failed: {error}"))?;
    if !matches!(value, Value::Map(_)) {
        return Err(
            "json_decode_object_entries failed: the root value must be a JSON object".to_string(),
        );
    }
    let members = ordered_members(text)
        .map_err(|error| format!("json_decode_object_entries failed: {error}"))?;
    Ok(members
        .into_iter()
        .map(|(key, raw)| JsonObjectEntry {
            key,
            value_json: raw.get().to_string(),
        })
        .collect())
}

/// Encodes ordered members as a JSON object in the given order.
///
/// Every `value_json` must be exactly one valid JSON value under the same strict
/// contract the decoder applies (so nested duplicate keys and trailing input are
/// rejected instead of being spliced into the output), and entry keys must be
/// unique: an object cannot carry two members with the same name.
pub(super) fn encode_object_entries(entries: &[JsonObjectEntry]) -> Result<String, String> {
    let mut seen: HashSet<&str> = HashSet::with_capacity(entries.len());
    for entry in entries {
        if !seen.insert(entry.key.as_str()) {
            return Err(format!(
                "json_encode_object_entries failed: duplicate entry key '{}'",
                entry.key
            ));
        }
    }
    for (index, entry) in entries.iter().enumerate() {
        super::json::decode_strict_value(&entry.value_json).map_err(|error| {
            format!(
                "json_encode_object_entries failed: entry {index} value_json is not one valid \
                 JSON value: {error}"
            )
        })?;
    }

    let mut out = Vec::new();
    let mut serializer = serde_json::Serializer::new(&mut out);
    let mut map = serializer
        .serialize_map(Some(entries.len()))
        .map_err(|error| {
            format!("json_encode_object_entries failed: cannot start an object: {error}")
        })?;
    for entry in entries {
        // Re-validate on the owned copy so the serializer writes raw text that
        // this call itself verified.
        let raw = RawValue::from_string(entry.value_json.clone()).map_err(|error| {
            format!(
                "json_encode_object_entries failed: value_json is not one valid JSON value: {error}"
            )
        })?;
        map.serialize_entry(&entry.key, &raw).map_err(|error| {
            format!("json_encode_object_entries failed: cannot write an entry: {error}")
        })?;
    }
    map.end().map_err(|error| {
        format!("json_encode_object_entries failed: cannot close the object: {error}")
    })?;
    String::from_utf8(out).map_err(|error| {
        format!("json_encode_object_entries failed: the encoder produced invalid UTF-8: {error}")
    })
}

// ------------------------------------------------------------ guest functions

/// Decodes a JSON object into its members in document order.
#[pd_host_function(
    name = "json::decode_object_entries",
    contract = decode_object_entries_contract
)]
pub(super) fn builtin_json_decode_object_entries(text: &str) -> VmResult<Value> {
    let entries = decode_object_entries(text).map_err(VmError::HostError)?;
    Ok(Value::array(
        entries
            .into_iter()
            .map(|entry| {
                Value::map(vec![
                    (Value::string("key"), Value::string(entry.key)),
                    (Value::string("value_json"), Value::string(entry.value_json)),
                ])
            })
            .collect(),
    ))
}

/// Encodes ordered members as a JSON object in the given order.
#[pd_host_function(
    name = "json::encode_object_entries",
    contract = encode_object_entries_contract
)]
pub(super) fn builtin_json_encode_object_entries(entries: VmArrayRef<'_>) -> VmResult<String> {
    let entries = guest_entries(entries)?;
    encode_object_entries(&entries).map_err(VmError::HostError)
}

/// Reads the guest member array into owned entries.
///
/// The guest type system already rejects a member array that omits a required
/// field or mistypes one; this runtime check keeps the adapter safe for any
/// carrier that reaches it and reports the failure without echoing a value.
fn guest_entries(entries: VmArrayRef<'_>) -> VmResult<Vec<JsonObjectEntry>> {
    let mut decoded = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let Value::Map(map) = entry else {
            return Err(entry_failure(
                index,
                "expected a json.JsonObjectEntry object",
            ));
        };
        let key = match map.get(&Value::string("key")) {
            Some(Value::String(value)) => value.as_str().to_string(),
            Some(_) => return Err(entry_failure(index, "field 'key' must be a string")),
            None => return Err(entry_failure(index, "missing required field 'key'")),
        };
        let value_json = match map.get(&Value::string("value_json")) {
            Some(Value::String(value)) => value.as_str().to_string(),
            Some(_) => return Err(entry_failure(index, "field 'value_json' must be a string")),
            None => return Err(entry_failure(index, "missing required field 'value_json'")),
        };
        decoded.push(JsonObjectEntry { key, value_json });
    }
    Ok(decoded)
}

fn entry_failure(index: usize, reason: &str) -> VmError {
    VmError::HostError(format!(
        "json_encode_object_entries failed: entry {index}: {reason}"
    ))
}

// ---- published named bodies -----------------------------------------------

/// The named bodies this module publishes, in declaration order.
pub(super) const JSON_NAMED_STRUCTS: &[(&str, &str)] = &[(
    "JsonObjectEntry",
    "One ordered JSON object member: its key and its verified JSON value text.",
)];
