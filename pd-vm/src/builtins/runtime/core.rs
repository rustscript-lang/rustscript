use super::{AnyValue, UnknownValue, VmArray, VmMap, arg, return_values};
use crate::bytecode::unwrap_or_clone_shared;
use crate::vm::{Value, VmError, VmResult};
use pd_host_function::pd_host_function;
use rt_format::{Format, FormatArgument, NoNamedArguments, ParsedFormat, Specifier};

/// Return the length of a string, array, or map.
#[pd_host_function(name = "len")]
pub(super) fn builtin_len_string_impl(text: &str) -> i64 {
    text.chars().count() as i64
}

/// Return the length of an array.
#[pd_host_function(name = "len")]
pub(super) fn builtin_len_array_impl(items: VmArray) -> i64 {
    items.len() as i64
}

/// Return the number of entries in a map.
#[pd_host_function(name = "len")]
pub(super) fn builtin_len_map_impl(entries: VmMap) -> i64 {
    entries.len() as i64
}

pub(super) fn builtin_len(args: &[Value]) -> VmResult<Vec<Value>> {
    let value = arg::<&Value>(args, 0, "len value")?;
    match value {
        Value::String(text) => Ok(return_values(builtin_len_string_impl(text.as_str()))),
        Value::Array(values) => Ok(return_values(values.len())),
        Value::Map(entries) => Ok(return_values(entries.len())),
        _ => Err(VmError::TypeMismatch("string/array/map")),
    }
}

fn slice_bounds(start: i64, length: i64) -> VmResult<Option<(usize, usize)>> {
    if start < 0 || length <= 0 {
        return Ok(None);
    }
    let start = usize::try_from(start).map_err(|_| {
        VmError::HostError("slice start overflow while converting to usize".to_string())
    })?;
    let length = usize::try_from(length).map_err(|_| {
        VmError::HostError("slice length overflow while converting to usize".to_string())
    })?;
    Ok(Some((start, length)))
}

fn take_arg(args: &mut [Value], index: usize, label: &str) -> VmResult<Value> {
    args.get_mut(index)
        .map(|value| std::mem::replace(value, Value::Null))
        .ok_or_else(|| VmError::HostError(format!("missing argument: {label}")))
}

/// Slice a string from the given start and length.
#[pd_host_function(name = "slice")]
pub(super) fn builtin_slice_string_impl(text: &str, start: i64, length: i64) -> VmResult<String> {
    let Some((start, length)) = slice_bounds(start, length)? else {
        return Ok(String::new());
    };
    Ok(text.chars().skip(start).take(length).collect::<String>())
}

/// Slice an array from the given start and length.
#[pd_host_function(name = "slice")]
pub(super) fn builtin_slice_array_impl(
    items: VmArray,
    start: i64,
    length: i64,
) -> VmResult<VmArray> {
    let Some((start, length)) = slice_bounds(start, length)? else {
        return Ok(Vec::new());
    };
    Ok(items
        .into_iter()
        .skip(start)
        .take(length)
        .collect::<Vec<_>>())
}

pub(super) fn builtin_slice(args: &[Value]) -> VmResult<Vec<Value>> {
    let source = arg::<&Value>(args, 0, "slice source")?;
    let start = arg::<i64>(args, 1, "slice start")?;
    let length = arg::<i64>(args, 2, "slice length")?;
    match source {
        Value::String(text) => {
            builtin_slice_string_impl(text.as_str(), start, length).map(return_values)
        }
        Value::Array(values) => {
            let Some((start, length)) = slice_bounds(start, length)? else {
                return Ok(return_values(Vec::<Value>::new()));
            };
            Ok(return_values(
                values
                    .iter()
                    .skip(start)
                    .take(length)
                    .cloned()
                    .collect::<Vec<_>>(),
            ))
        }
        _ => Err(VmError::TypeMismatch("string/array")),
    }
}

/// Concatenate two strings.
#[pd_host_function(name = "concat")]
pub(super) fn builtin_concat_string_impl(left: &str, right: &str) -> String {
    let mut out = String::with_capacity(left.len() + right.len());
    out.push_str(left);
    out.push_str(right);
    out
}

/// Concatenate two arrays.
#[pd_host_function(name = "concat")]
pub(super) fn builtin_concat_array_impl(mut left: VmArray, right: VmArray) -> VmArray {
    left.extend(right);
    left
}

pub(super) fn builtin_concat(args: &[Value]) -> VmResult<Vec<Value>> {
    let left = arg::<&Value>(args, 0, "concat left")?;
    let right = arg::<&Value>(args, 1, "concat right")?;
    match (left, right) {
        (Value::String(left), Value::String(right)) => Ok(return_values(
            builtin_concat_string_impl(left.as_str(), right.as_str()),
        )),
        (Value::Array(left), Value::Array(right)) => {
            let mut values = Vec::with_capacity(left.len() + right.len());
            values.extend(left.iter().cloned());
            values.extend(right.iter().cloned());
            Ok(return_values(values))
        }
        _ => Err(VmError::TypeMismatch("string/string or array/array")),
    }
}

/// Create an empty array.
#[pd_host_function(name = "array_new")]
pub(super) fn builtin_array_new_impl() -> VmArray {
    Vec::new()
}

/// Append a value to an array and return the updated array.
#[pd_host_function(name = "array_push")]
pub(super) fn builtin_array_push_typed_impl(mut items: VmArray, value: AnyValue) -> VmArray {
    items.push(value);
    items
}

pub(super) fn builtin_array_push(args: &mut [Value]) -> VmResult<Vec<Value>> {
    let items = match take_arg(args, 0, "array_push array")? {
        Value::Array(values) => unwrap_or_clone_shared(values),
        _ => return Err(VmError::TypeMismatch("array")),
    };
    let value = take_arg(args, 1, "array_push value")?;
    Ok(return_values(builtin_array_push_typed_impl(items, value)))
}

/// Create an empty map.
#[pd_host_function(name = "map_new")]
pub(super) fn builtin_map_new_impl() -> VmMap {
    VmMap::new()
}

/// Read a string entry.
#[pd_host_function(name = "get")]
pub(super) fn builtin_get_string_impl(text: &str, index: i64) -> VmResult<String> {
    if index < 0 {
        return Err(VmError::HostError(
            "string index must be non-negative".to_string(),
        ));
    }
    let index = usize::try_from(index)
        .map_err(|_| VmError::HostError("string index overflow".to_string()))?;
    text.chars()
        .nth(index)
        .map(|ch| ch.to_string())
        .ok_or_else(|| VmError::HostError(format!("string index {index} out of bounds")))
}

/// Read an array element by index.
#[pd_host_function(name = "get")]
pub(super) fn builtin_get_array_impl(items: VmArray, index: i64) -> VmResult<UnknownValue> {
    if index < 0 {
        return Err(VmError::HostError(
            "array index must be non-negative".to_string(),
        ));
    }
    let index = usize::try_from(index)
        .map_err(|_| VmError::HostError("array index overflow".to_string()))?;
    let mut items = items;
    if index >= items.len() {
        return Err(VmError::HostError(format!(
            "array index {index} out of bounds"
        )));
    }
    Ok(items.swap_remove(index))
}

/// Read a map value by key.
#[pd_host_function(name = "get")]
pub(super) fn builtin_get_map_impl(entries: VmMap, key: AnyValue) -> VmResult<UnknownValue> {
    entries
        .get(&key)
        .cloned()
        .ok_or_else(|| VmError::HostError("map key not found".to_string()))
}

/// Check whether an array contains a valid index.
#[pd_host_function(name = "has")]
pub(super) fn builtin_has_array_impl(items: VmArray, index: i64) -> bool {
    if index < 0 {
        return false;
    }
    usize::try_from(index)
        .ok()
        .is_some_and(|index| index < items.len())
}

/// Check whether a map contains a key.
#[pd_host_function(name = "has")]
pub(super) fn builtin_has_map_impl(entries: VmMap, key: AnyValue) -> bool {
    entries.get(&key).is_some()
}

pub(super) fn builtin_has(args: &[Value]) -> VmResult<Vec<Value>> {
    let container = arg::<&Value>(args, 0, "has container")?;
    let key = arg::<&Value>(args, 1, "has key")?;
    match container {
        Value::Array(values) => {
            let index = key.as_int()?;
            let present = if index < 0 {
                false
            } else {
                usize::try_from(index)
                    .ok()
                    .is_some_and(|index| index < values.len())
            };
            Ok(return_values(present))
        }
        Value::Map(entries) => Ok(return_values(entries.get(key).is_some())),
        _ => Err(VmError::TypeMismatch("array/map")),
    }
}

pub(super) fn builtin_get(args: &[Value]) -> VmResult<Vec<Value>> {
    let container = arg::<&Value>(args, 0, "get container")?;
    let key = arg::<&Value>(args, 1, "get key")?;
    match container {
        Value::Array(values) => {
            let index = key.as_int()?;
            if index < 0 {
                return Err(VmError::HostError(
                    "array index must be non-negative".to_string(),
                ));
            }
            let index = usize::try_from(index)
                .map_err(|_| VmError::HostError("array index overflow".to_string()))?;
            Ok(return_values(values.get(index).cloned().ok_or_else(
                || VmError::HostError(format!("array index {index} out of bounds")),
            )?))
        }
        Value::Map(entries) => {
            Ok(return_values(entries.get(key).cloned().ok_or_else(
                || VmError::HostError("map key not found".to_string()),
            )?))
        }
        Value::String(text) => {
            builtin_get_string_impl(text.as_str(), key.as_int()?).map(return_values)
        }
        _ => Err(VmError::TypeMismatch("array/map/string")),
    }
}

/// Return the runtime type name of a value.
#[pd_host_function(name = "type")]
pub(super) fn builtin_type_of_impl(value: &AnyValue) -> String {
    match value {
        Value::Null => "null",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Bool(_) => "bool",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Map(_) => "map",
    }
    .to_string()
}

/// Convert a value into a display string.
#[pd_host_function(name = "__to_string")]
pub(super) fn builtin_to_string_impl(value: &AnyValue) -> String {
    render_value_for_display(value)
}

/// Render a format template against an array of positional values.
#[pd_host_function(name = "__format_template")]
pub(super) fn builtin_format_template_impl(template: &str, values: VmArray) -> VmResult<String> {
    ParsedFormat::parse(template, values.as_slice(), &NoNamedArguments)
        .map(|parsed| parsed.to_string())
        .map_err(|offset| {
            VmError::HostError(format!(
                "format string and arguments are incompatible at byte {offset}: {template}"
            ))
        })
}

fn render_value_for_display(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Int(v) => v.to_string(),
        Value::Float(v) => v.to_string(),
        Value::Bool(v) => v.to_string(),
        Value::String(v) => v.as_str().to_string(),
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
    }
}

impl FormatArgument for Value {
    fn supports_format(&self, specifier: &Specifier) -> bool {
        match self {
            Value::Null => matches!(specifier.format, Format::Display | Format::Debug),
            Value::Int(_) => true,
            Value::Float(_) => matches!(
                specifier.format,
                Format::Display | Format::Debug | Format::LowerExp | Format::UpperExp
            ),
            Value::Bool(_) => matches!(specifier.format, Format::Display | Format::Debug),
            Value::String(_) => matches!(specifier.format, Format::Display | Format::Debug),
            Value::Array(_) | Value::Map(_) => {
                matches!(specifier.format, Format::Display | Format::Debug)
            }
        }
    }

    fn fmt_display(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Value::Null => f.write_str("null"),
            Value::Int(value) => std::fmt::Display::fmt(value, f),
            Value::Float(value) => std::fmt::Display::fmt(value, f),
            Value::Bool(value) => std::fmt::Display::fmt(value, f),
            Value::String(value) => std::fmt::Display::fmt(value.as_str(), f),
            Value::Array(_) | Value::Map(_) => f.write_str(render_value_for_display(self).as_str()),
        }
    }

    fn fmt_debug(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Value::Null => f.write_str("null"),
            Value::Int(value) => std::fmt::Debug::fmt(value, f),
            Value::Float(value) => std::fmt::Debug::fmt(value, f),
            Value::Bool(value) => std::fmt::Debug::fmt(value, f),
            Value::String(value) => std::fmt::Debug::fmt(value.as_str(), f),
            Value::Array(values) => {
                let mut list = f.debug_list();
                for value in values.iter() {
                    list.entry(value);
                }
                list.finish()
            }
            Value::Map(entries) => {
                let mut map = f.debug_map();
                for (key, value) in entries.iter() {
                    map.entry(key, value);
                }
                map.finish()
            }
        }
    }

    fn fmt_octal(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Value::Int(value) => std::fmt::Octal::fmt(value, f),
            _ => Err(std::fmt::Error),
        }
    }

    fn fmt_lower_hex(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Value::Int(value) => std::fmt::LowerHex::fmt(value, f),
            _ => Err(std::fmt::Error),
        }
    }

    fn fmt_upper_hex(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Value::Int(value) => std::fmt::UpperHex::fmt(value, f),
            _ => Err(std::fmt::Error),
        }
    }

    fn fmt_binary(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Value::Int(value) => std::fmt::Binary::fmt(value, f),
            _ => Err(std::fmt::Error),
        }
    }

    fn fmt_lower_exp(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Value::Int(value) => std::fmt::LowerExp::fmt(value, f),
            Value::Float(value) => std::fmt::LowerExp::fmt(value, f),
            _ => Err(std::fmt::Error),
        }
    }

    fn fmt_upper_exp(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Value::Int(value) => std::fmt::UpperExp::fmt(value, f),
            Value::Float(value) => std::fmt::UpperExp::fmt(value, f),
            _ => Err(std::fmt::Error),
        }
    }

    fn to_usize(&self) -> Result<usize, ()> {
        match self {
            Value::Int(value) => usize::try_from(*value).map_err(|_| ()),
            _ => Err(()),
        }
    }
}

/// Update an array entry and return the updated array.
#[pd_host_function(name = "set")]
pub(super) fn builtin_set_array_impl(
    mut items: VmArray,
    index: i64,
    value: AnyValue,
) -> VmResult<VmArray> {
    if index < 0 {
        return Err(VmError::HostError(
            "array index must be non-negative".to_string(),
        ));
    }
    let index = usize::try_from(index)
        .map_err(|_| VmError::HostError("array index overflow".to_string()))?;
    if index < items.len() {
        items[index] = value;
    } else if index == items.len() {
        items.push(value);
    } else {
        return Err(VmError::HostError(format!(
            "array index {index} out of bounds"
        )));
    }
    Ok(items)
}

/// Update a map entry and return the updated map.
#[pd_host_function(name = "set")]
pub(super) fn builtin_set_map_impl(mut entries: VmMap, key: AnyValue, value: AnyValue) -> VmMap {
    if matches!(value, Value::Null) {
        entries.remove(&key);
    } else {
        entries.insert(key, value);
    }
    entries
}

pub(super) fn builtin_set(args: &mut [Value]) -> VmResult<Vec<Value>> {
    let container = take_arg(args, 0, "set container")?;
    let key = take_arg(args, 1, "set key")?;
    let value = take_arg(args, 2, "set value")?;
    match container {
        Value::Array(values) => {
            builtin_set_array_impl(unwrap_or_clone_shared(values), key.as_int()?, value)
                .map(return_values)
        }
        Value::Map(entries) => Ok(return_values(builtin_set_map_impl(
            unwrap_or_clone_shared(entries),
            key,
            value,
        ))),
        _ => Err(VmError::TypeMismatch("array/map")),
    }
}

/// Return an array of container keys or indices.
#[pd_host_function(name = "keys")]
pub(super) fn builtin_keys_array_impl(items: VmArray) -> VmArray {
    (0..items.len())
        .map(|index| Value::Int(index as i64))
        .collect::<Vec<_>>()
}

/// Return an array of map keys.
#[pd_host_function(name = "keys")]
pub(super) fn builtin_keys_map_impl(entries: VmMap) -> VmArray {
    entries.into_iter().map(|(key, _)| key).collect::<Vec<_>>()
}

pub(super) fn builtin_keys(args: &[Value]) -> VmResult<Vec<Value>> {
    let container = arg::<&Value>(args, 0, "keys container")?;
    match container {
        Value::Array(values) => Ok(return_values(
            (0..values.len())
                .map(|index| Value::Int(index as i64))
                .collect::<Vec<_>>(),
        )),
        Value::Map(entries) => Ok(return_values(
            entries
                .iter()
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>(),
        )),
        _ => Err(VmError::TypeMismatch("array/map")),
    }
}

/// Return the number of entries in an array or map.
#[pd_host_function(name = "count")]
pub(super) fn builtin_count_array_impl(items: VmArray) -> i64 {
    items.len() as i64
}

/// Return the number of entries in a map.
#[pd_host_function(name = "count")]
pub(super) fn builtin_count_map_impl(entries: VmMap) -> i64 {
    entries.len() as i64
}

pub(super) fn builtin_count(args: &[Value]) -> VmResult<Vec<Value>> {
    let container = arg::<&Value>(args, 0, "count container")?;
    match container {
        Value::Array(values) => Ok(return_values(values.len())),
        Value::Map(entries) => Ok(return_values(entries.len())),
        _ => Err(VmError::TypeMismatch("array/map")),
    }
}

/// Abort execution if the condition is false.
#[pd_host_function(name = "assert")]
pub(super) fn builtin_assert_impl(condition: bool) -> VmResult<()> {
    if condition {
        Ok(())
    } else {
        Err(VmError::HostError("assertion failed".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn array_push_detaches_shared_array_before_write() {
        let shared = Value::array(vec![Value::Int(1)]);
        let alias = shared.clone();

        let mut args = [shared, Value::Int(2)];
        let out = builtin_array_push(&mut args).expect("array push should work");
        let [Value::Array(result)] = out.as_slice() else {
            panic!("expected array result");
        };
        let Value::Array(alias_values) = &alias else {
            panic!("expected array alias");
        };

        assert_eq!(alias_values.as_ref(), &vec![Value::Int(1)]);
        assert_eq!(result.as_ref(), &vec![Value::Int(1), Value::Int(2)]);
        assert!(
            !Arc::ptr_eq(alias_values, result),
            "mutating a shared array should detach backing storage"
        );
    }

    #[test]
    fn set_detaches_shared_array_before_write() {
        let shared = Value::array(vec![Value::Int(1), Value::Int(2)]);
        let alias = shared.clone();

        let mut args = [shared, Value::Int(0), Value::Int(9)];
        let out = builtin_set(&mut args).expect("array set should work");
        let [Value::Array(result)] = out.as_slice() else {
            panic!("expected array result");
        };
        let Value::Array(alias_values) = &alias else {
            panic!("expected array alias");
        };

        assert_eq!(alias_values.as_ref(), &vec![Value::Int(1), Value::Int(2)]);
        assert_eq!(result.as_ref(), &vec![Value::Int(9), Value::Int(2)]);
        assert!(
            !Arc::ptr_eq(alias_values, result),
            "mutating a shared array should detach backing storage"
        );
    }

    #[test]
    fn set_detaches_shared_map_before_write() {
        let shared = Value::map(vec![(Value::string("k"), Value::Int(1))]);
        let alias = shared.clone();

        let mut args = [shared, Value::string("k"), Value::Int(9)];
        let out = builtin_set(&mut args).expect("map set should work");
        let [Value::Map(result)] = out.as_slice() else {
            panic!("expected map result");
        };
        let Value::Map(alias_entries) = &alias else {
            panic!("expected map alias");
        };

        assert_eq!(alias_entries.len(), 1);
        assert_eq!(alias_entries.get(&Value::string("k")), Some(&Value::Int(1)));
        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&Value::string("k")), Some(&Value::Int(9)));
        assert!(
            !Arc::ptr_eq(alias_entries, result),
            "mutating a shared map should detach backing storage"
        );
    }

    #[test]
    fn set_map_null_removes_entry() {
        let shared = Value::map(vec![(Value::string("drop"), Value::Int(1))]);
        let alias = shared.clone();

        let mut args = [shared, Value::string("drop"), Value::Null];
        let out = builtin_set(&mut args).expect("map null set should work");
        let [Value::Map(result)] = out.as_slice() else {
            panic!("expected map result");
        };
        let Value::Map(alias_entries) = &alias else {
            panic!("expected map alias");
        };

        assert_eq!(alias_entries.len(), 1);
        assert_eq!(
            alias_entries.get(&Value::string("drop")),
            Some(&Value::Int(1))
        );
        assert_eq!(result.len(), 0);
        assert_eq!(result.get(&Value::string("drop")), None);
        assert!(
            !Arc::ptr_eq(alias_entries, result),
            "mutating a shared map should detach backing storage"
        );
    }

    #[test]
    fn has_map_uses_identity_for_heap_keys() {
        let key = Value::array(vec![Value::Int(1), Value::Int(2)]);
        let alias = key.clone();
        let structural_peer = Value::array(vec![Value::Int(1), Value::Int(2)]);
        let map = VmMap::from(vec![(key, Value::Bool(true))]);

        assert!(builtin_has_map_impl(map.clone(), alias));
        assert!(!builtin_has_map_impl(map, structural_peer));
    }

    #[test]
    fn has_dispatch_uses_identity_for_heap_keys() {
        let key = Value::array(vec![Value::Int(1), Value::Int(2)]);
        let alias = key.clone();
        let structural_peer = Value::array(vec![Value::Int(1), Value::Int(2)]);
        let map = Value::map(vec![(key, Value::Bool(true))]);

        let alias_result = builtin_has(&[map.clone(), alias]).expect("builtin has should succeed");
        let [Value::Bool(alias_present)] = alias_result.as_slice() else {
            panic!("expected bool result");
        };
        assert!(*alias_present, "shared heap key should match by identity");

        let peer_result = builtin_has(&[map, structural_peer]).expect("builtin has should succeed");
        let [Value::Bool(peer_present)] = peer_result.as_slice() else {
            panic!("expected bool result");
        };
        assert!(
            !peer_present,
            "structural peer should not match a map key stored by heap identity"
        );
    }

    #[test]
    fn len_and_count_dispatch_return_shared_container_sizes() {
        let array = Value::array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        let map = Value::map(vec![
            (Value::string("a"), Value::Int(1)),
            (Value::string("b"), Value::Int(2)),
        ]);

        let array_len = builtin_len(&[array.clone()]).expect("array len should succeed");
        let [Value::Int(array_len)] = array_len.as_slice() else {
            panic!("expected int result");
        };
        assert_eq!(*array_len, 3);

        let array_count = builtin_count(&[array]).expect("array count should succeed");
        let [Value::Int(array_count)] = array_count.as_slice() else {
            panic!("expected int result");
        };
        assert_eq!(*array_count, 3);

        let map_len = builtin_len(&[map.clone()]).expect("map len should succeed");
        let [Value::Int(map_len)] = map_len.as_slice() else {
            panic!("expected int result");
        };
        assert_eq!(*map_len, 2);

        let map_count = builtin_count(&[map]).expect("map count should succeed");
        let [Value::Int(map_count)] = map_count.as_slice() else {
            panic!("expected int result");
        };
        assert_eq!(*map_count, 2);
    }
}
