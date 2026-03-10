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
        Value::Array(values) => Ok(return_values(builtin_len_array_impl(
            unwrap_or_clone_shared(values.clone()),
        ))),
        Value::Map(entries) => Ok(return_values(builtin_len_map_impl(unwrap_or_clone_shared(
            entries.clone(),
        )))),
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

pub(super) fn builtin_slice(args: Vec<Value>) -> VmResult<Vec<Value>> {
    let mut iter = args.into_iter();
    let source = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing source for slice".to_string()))?;
    let start = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing slice start".to_string()))?
        .as_int()?;
    let length = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing slice length".to_string()))?
        .as_int()?;
    match source {
        Value::String(text) => {
            builtin_slice_string_impl(text.as_str(), start, length).map(return_values)
        }
        Value::Array(values) => {
            builtin_slice_array_impl(unwrap_or_clone_shared(values), start, length)
                .map(return_values)
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

pub(super) fn builtin_concat(args: Vec<Value>) -> VmResult<Vec<Value>> {
    let mut iter = args.into_iter();
    let left = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing left argument to concat".to_string()))?;
    let right = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing right argument to concat".to_string()))?;
    match (left, right) {
        (Value::String(left), Value::String(right)) => Ok(return_values(
            builtin_concat_string_impl(left.as_str(), right.as_str()),
        )),
        (Value::Array(left), Value::Array(right)) => Ok(return_values(builtin_concat_array_impl(
            unwrap_or_clone_shared(left),
            unwrap_or_clone_shared(right),
        ))),
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

pub(super) fn builtin_array_push(args: Vec<Value>) -> VmResult<Vec<Value>> {
    let mut iter = args.into_iter();
    let items = match iter
        .next()
        .ok_or_else(|| VmError::HostError("missing array argument".to_string()))?
    {
        Value::Array(values) => unwrap_or_clone_shared(values),
        _ => return Err(VmError::TypeMismatch("array")),
    };
    let value = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing value argument".to_string()))?;
    Ok(return_values(builtin_array_push_typed_impl(items, value)))
}

/// Create an empty map.
#[pd_host_function(name = "map_new")]
pub(super) fn builtin_map_new_impl() -> VmMap {
    Vec::new()
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
    for (existing_key, value) in entries {
        if existing_key == key {
            return Ok(value);
        }
    }
    Err(VmError::HostError("map key not found".to_string()))
}

pub(super) fn builtin_get(args: Vec<Value>) -> VmResult<Vec<Value>> {
    let mut iter = args.into_iter();
    let container = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing container argument".to_string()))?;
    let key = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing key argument".to_string()))?;
    match container {
        Value::Array(values) => {
            builtin_get_array_impl(unwrap_or_clone_shared(values), key.as_int()?).map(return_values)
        }
        Value::Map(entries) => {
            builtin_get_map_impl(unwrap_or_clone_shared(entries), key).map(return_values)
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
    if let Some((_, existing_value)) = entries
        .iter_mut()
        .find(|(existing_key, _)| *existing_key == key)
    {
        *existing_value = value;
    } else {
        entries.push((key, value));
    }
    entries
}

pub(super) fn builtin_set(args: Vec<Value>) -> VmResult<Vec<Value>> {
    let mut iter = args.into_iter();
    let container = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing container argument".to_string()))?;
    let key = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing key argument".to_string()))?;
    let value = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing value argument".to_string()))?;
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

pub(super) fn builtin_keys(args: Vec<Value>) -> VmResult<Vec<Value>> {
    let container = args
        .into_iter()
        .next()
        .ok_or_else(|| VmError::HostError("missing container argument".to_string()))?;
    match container {
        Value::Array(values) => Ok(return_values(builtin_keys_array_impl(
            unwrap_or_clone_shared(values),
        ))),
        Value::Map(entries) => Ok(return_values(builtin_keys_map_impl(
            unwrap_or_clone_shared(entries),
        ))),
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
        Value::Array(values) => Ok(return_values(builtin_count_array_impl(
            unwrap_or_clone_shared(values.clone()),
        ))),
        Value::Map(entries) => Ok(return_values(builtin_count_map_impl(
            unwrap_or_clone_shared(entries.clone()),
        ))),
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

        let out = builtin_array_push(vec![shared, Value::Int(2)]).expect("array push should work");
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

        let out =
            builtin_set(vec![shared, Value::Int(0), Value::Int(9)]).expect("array set should work");
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

        let out = builtin_set(vec![shared, Value::string("k"), Value::Int(9)])
            .expect("map set should work");
        let [Value::Map(result)] = out.as_slice() else {
            panic!("expected map result");
        };
        let Value::Map(alias_entries) = &alias else {
            panic!("expected map alias");
        };

        assert_eq!(
            alias_entries.as_ref(),
            &vec![(Value::string("k"), Value::Int(1))]
        );
        assert_eq!(result.as_ref(), &vec![(Value::string("k"), Value::Int(9))]);
        assert!(
            !Arc::ptr_eq(alias_entries, result),
            "mutating a shared map should detach backing storage"
        );
    }
}
