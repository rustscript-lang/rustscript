//! Pure VM operations shared by the interpreter and native bridge.
//!
//! These operations describe language values only. They intentionally do not
//! know about builtin registration, host policies, or a concrete adapter.

use std::sync::Arc;

use crate::Value;
use crate::bytecode::{CallableKind, SharedArray, SharedMap};
use crate::vm::{VmError, VmResult};

pub(crate) fn string_contains(text: &str, needle: &str) -> bool {
    text.contains(needle)
}

pub(crate) fn string_replace_literal(text: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return text.to_string();
    }
    text.replace(needle, replacement)
}

pub(crate) fn string_lower_ascii(text: &str) -> String {
    let mut out = text.as_bytes().to_vec();
    for byte in &mut out {
        if byte.is_ascii_uppercase() {
            *byte = byte.to_ascii_lowercase();
        }
    }
    String::from_utf8(out).expect("ASCII-only byte changes preserve UTF-8")
}

pub(crate) fn string_split_literal(text: &str, delimiter: &str) -> Vec<Value> {
    if delimiter.is_empty() {
        return vec![Value::string(text.to_string())];
    }
    text.split(delimiter)
        .map(|part| Value::string(part.to_string()))
        .collect()
}

pub(crate) fn value_to_string(value: &Value) -> String {
    render_value_for_display(value)
}

fn render_value_for_display(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Int(v) => v.to_string(),
        Value::Float(v) => v.to_string(),
        Value::Bool(v) => v.to_string(),
        Value::String(v) => v.as_str().to_string(),
        Value::Bytes(v) => render_bytes_for_display(v.as_ref()),
        Value::Array(values) => {
            let parts = values
                .iter()
                .map(render_value_for_display)
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{parts}]")
        }
        Value::Map(entries) => {
            let parts = entries
                .iter()
                .map(|(key, value)| {
                    format!(
                        "{}: {}",
                        render_value_for_display(key),
                        render_value_for_display(value)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{parts}}}")
        }
        Value::Callable(callable) => match callable.kind {
            CallableKind::FunctionItem => format!("<fn#{}>", callable.prototype_id),
            CallableKind::Closure => format!("<closure#{}>", callable.prototype_id),
            CallableKind::HostFunction => format!("<host-fn#{}>", callable.prototype_id),
        },
    }
}

fn render_bytes_for_display(bytes: &[u8]) -> String {
    let preview_len = bytes.len().min(16);
    let mut preview = String::with_capacity(preview_len * 2);
    for byte in &bytes[..preview_len] {
        preview.push(hex_nibble(byte >> 4));
        preview.push(hex_nibble(byte & 0x0F));
    }
    if bytes.len() > preview_len {
        format!("bytes[len={} hex={}..]", bytes.len(), preview)
    } else {
        format!("bytes[len={} hex={}]", bytes.len(), preview)
    }
}

fn hex_nibble(value: u8) -> char {
    match value {
        0..=9 => char::from(b'0' + value),
        10..=15 => char::from(b'a' + (value - 10)),
        _ => unreachable!("hex nibble out of range"),
    }
}

pub(crate) fn ensure_supported_map_key(key: &Value) -> VmResult<()> {
    if matches!(key, Value::Callable(_)) {
        return Err(VmError::HostError(
            "callable values are not supported as map keys".to_string(),
        ));
    }
    Ok(())
}

fn set_array_shared(mut items: SharedArray, index: i64, value: Value) -> VmResult<SharedArray> {
    let items_mut = Arc::make_mut(&mut items);
    if index < 0 {
        return Err(VmError::HostError(
            "array index must be non-negative".to_string(),
        ));
    }
    let index = usize::try_from(index)
        .map_err(|_| VmError::HostError("array index overflow".to_string()))?;
    if index < items_mut.len() {
        items_mut[index] = value;
    } else if index == items_mut.len() {
        items_mut.push(value);
    } else {
        return Err(VmError::HostError(format!(
            "array index {index} out of bounds"
        )));
    }
    Ok(items)
}

pub(crate) fn set_owned(container: Value, key: Value, value: Value) -> VmResult<Value> {
    match container {
        Value::Array(values) => set_array_shared(values, key.as_int()?, value).map(Value::Array),
        Value::Map(entries) => {
            ensure_supported_map_key(&key)?;
            Ok(Value::Map(set_map_shared(entries, key, value)))
        }
        _ => Err(VmError::TypeMismatch("array/map")),
    }
}

pub(crate) fn set_map_shared(mut entries: SharedMap, key: Value, value: Value) -> SharedMap {
    let entries_mut = Arc::make_mut(&mut entries);
    if matches!(value, Value::Null) {
        entries_mut.remove(&key);
    } else {
        entries_mut.insert(key, value);
    }
    entries
}

pub(crate) fn array_push_shared(mut items: SharedArray, value: Value) -> SharedArray {
    Arc::make_mut(&mut items).push(value);
    items
}
