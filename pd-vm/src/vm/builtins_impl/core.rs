use super::super::{Value, VmError, VmResult};
use rt_format::{Format, FormatArgument, NoNamedArguments, ParsedFormat, Specifier};

pub(super) fn builtin_len(args: &[Value]) -> VmResult<Vec<Value>> {
    let value = args
        .first()
        .ok_or_else(|| VmError::HostError("missing argument to len".to_string()))?;
    let len = match value {
        Value::String(text) => text.chars().count() as i64,
        Value::Array(values) => values.len() as i64,
        Value::Map(entries) => entries.len() as i64,
        _ => return Err(VmError::TypeMismatch("string/array/map")),
    };
    Ok(vec![Value::Int(len)])
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
    let len = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing slice length".to_string()))?
        .as_int()?;

    if start < 0 || len <= 0 {
        return match source {
            Value::String(_) => Ok(vec![Value::String(String::new())]),
            Value::Array(_) => Ok(vec![Value::Array(Vec::new())]),
            _ => Err(VmError::TypeMismatch("string/array")),
        };
    }

    let start = usize::try_from(start).map_err(|_| {
        VmError::HostError("slice start overflow while converting to usize".to_string())
    })?;
    let len = usize::try_from(len).map_err(|_| {
        VmError::HostError("slice length overflow while converting to usize".to_string())
    })?;
    match source {
        Value::String(text) => {
            let out = text.chars().skip(start).take(len).collect::<String>();
            Ok(vec![Value::String(out)])
        }
        Value::Array(values) => {
            let out = values.into_iter().skip(start).take(len).collect::<Vec<_>>();
            Ok(vec![Value::Array(out)])
        }
        _ => Err(VmError::TypeMismatch("string/array")),
    }
}

pub(super) fn builtin_concat(args: Vec<Value>) -> VmResult<Vec<Value>> {
    let mut iter = args.into_iter();
    let lhs = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing left argument to concat".to_string()))?;
    let rhs = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing right argument to concat".to_string()))?;
    match (lhs, rhs) {
        (Value::String(lhs), Value::String(rhs)) => {
            let mut out = String::with_capacity(lhs.len() + rhs.len());
            out.push_str(&lhs);
            out.push_str(&rhs);
            Ok(vec![Value::String(out)])
        }
        (Value::Array(lhs), Value::Array(rhs)) => {
            let mut out = lhs;
            out.extend(rhs);
            Ok(vec![Value::Array(out)])
        }
        _ => Err(VmError::TypeMismatch("string/string or array/array")),
    }
}

pub(super) fn builtin_array_push(args: Vec<Value>) -> VmResult<Vec<Value>> {
    let mut iter = args.into_iter();
    let mut out = match iter
        .next()
        .ok_or_else(|| VmError::HostError("missing array argument".to_string()))?
    {
        Value::Array(values) => values,
        _ => return Err(VmError::TypeMismatch("array")),
    };
    let value = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing value argument".to_string()))?;
    out.push(value);
    Ok(vec![Value::Array(out)])
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
            let index = key.as_int()?;
            if index < 0 {
                return Err(VmError::HostError(
                    "array index must be non-negative".to_string(),
                ));
            }
            let index = usize::try_from(index)
                .map_err(|_| VmError::HostError("array index overflow".to_string()))?;
            let mut values = values;
            if index >= values.len() {
                return Err(VmError::HostError(format!(
                    "array index {index} out of bounds"
                )));
            }
            let value = values.swap_remove(index);
            Ok(vec![value])
        }
        Value::Map(entries) => {
            for (existing_key, value) in entries {
                if existing_key == key {
                    return Ok(vec![value]);
                }
            }
            Err(VmError::HostError("map key not found".to_string()))
        }
        Value::String(text) => {
            let index = key.as_int()?;
            if index < 0 {
                return Err(VmError::HostError(
                    "string index must be non-negative".to_string(),
                ));
            }
            let index = usize::try_from(index)
                .map_err(|_| VmError::HostError("string index overflow".to_string()))?;
            let value = text
                .chars()
                .nth(index)
                .map(|ch| Value::String(ch.to_string()))
                .ok_or_else(|| VmError::HostError(format!("string index {index} out of bounds")))?;
            Ok(vec![value])
        }
        _ => Err(VmError::TypeMismatch("array/map/string")),
    }
}

pub(super) fn builtin_type_of(args: &[Value]) -> VmResult<Vec<Value>> {
    let value = args
        .first()
        .ok_or_else(|| VmError::HostError("missing argument to type_of".to_string()))?;
    let ty = match value {
        Value::Null => "null",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Bool(_) => "bool",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Map(_) => "map",
    };
    Ok(vec![Value::String(ty.to_string())])
}

pub(super) fn builtin_to_string(args: &[Value]) -> VmResult<Vec<Value>> {
    let value = args
        .first()
        .ok_or_else(|| VmError::HostError("missing argument to __to_string".to_string()))?;
    let text = render_value_for_display(value);
    Ok(vec![Value::String(text)])
}

pub(super) fn builtin_format_template(args: Vec<Value>) -> VmResult<Vec<Value>> {
    let mut iter = args.into_iter();
    let template = match iter.next() {
        Some(Value::String(value)) => value,
        Some(_) => return Err(VmError::TypeMismatch("string")),
        None => {
            return Err(VmError::HostError(
                "missing template argument to __format_template".to_string(),
            ));
        }
    };
    let positional = match iter.next() {
        Some(Value::Array(values)) => values,
        Some(_) => return Err(VmError::TypeMismatch("array")),
        None => {
            return Err(VmError::HostError(
                "missing positional arguments to __format_template".to_string(),
            ));
        }
    };
    let rendered = ParsedFormat::parse(template.as_str(), positional.as_slice(), &NoNamedArguments)
        .map(|parsed| parsed.to_string())
        .map_err(|offset| {
            VmError::HostError(format!(
                "format string and arguments are incompatible at byte {offset}: {template}"
            ))
        })?;
    Ok(vec![Value::String(rendered)])
}

fn render_value_for_display(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Int(v) => v.to_string(),
        Value::Float(v) => v.to_string(),
        Value::Bool(v) => v.to_string(),
        Value::String(v) => v.clone(),
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
            Value::String(value) => std::fmt::Display::fmt(value, f),
            Value::Array(_) | Value::Map(_) => f.write_str(render_value_for_display(self).as_str()),
        }
    }

    fn fmt_debug(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Value::Null => f.write_str("null"),
            Value::Int(value) => std::fmt::Debug::fmt(value, f),
            Value::Float(value) => std::fmt::Debug::fmt(value, f),
            Value::Bool(value) => std::fmt::Debug::fmt(value, f),
            Value::String(value) => std::fmt::Debug::fmt(value, f),
            Value::Array(values) => {
                let mut list = f.debug_list();
                for value in values {
                    list.entry(value);
                }
                list.finish()
            }
            Value::Map(entries) => {
                let mut map = f.debug_map();
                for (key, value) in entries {
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
            let index = key.as_int()?;
            if index < 0 {
                return Err(VmError::HostError(
                    "array index must be non-negative".to_string(),
                ));
            }
            let index = usize::try_from(index)
                .map_err(|_| VmError::HostError("array index overflow".to_string()))?;
            let mut out = values;
            if index < out.len() {
                out[index] = value;
            } else if index == out.len() {
                out.push(value);
            } else {
                return Err(VmError::HostError(format!(
                    "array index {index} out of bounds"
                )));
            }
            Ok(vec![Value::Array(out)])
        }
        Value::Map(entries) => {
            let mut out = entries;
            if let Some((_, existing_value)) = out
                .iter_mut()
                .find(|(existing_key, _)| *existing_key == key)
            {
                *existing_value = value;
            } else {
                out.push((key, value));
            }
            Ok(vec![Value::Map(out)])
        }
        _ => Err(VmError::TypeMismatch("array/map")),
    }
}

pub(super) fn builtin_keys(args: Vec<Value>) -> VmResult<Vec<Value>> {
    let container = args
        .into_iter()
        .next()
        .ok_or_else(|| VmError::HostError("missing container argument".to_string()))?;

    let keys = match container {
        Value::Array(values) => (0..values.len())
            .map(|index| Value::Int(index as i64))
            .collect::<Vec<_>>(),
        Value::Map(entries) => entries.into_iter().map(|(key, _)| key).collect::<Vec<_>>(),
        _ => return Err(VmError::TypeMismatch("array/map")),
    };
    Ok(vec![Value::Array(keys)])
}

pub(super) fn builtin_count(args: &[Value]) -> VmResult<Vec<Value>> {
    let container = args
        .first()
        .ok_or_else(|| VmError::HostError("missing container argument".to_string()))?;
    let count = match container {
        Value::Array(values) => values.len() as i64,
        Value::Map(entries) => entries.len() as i64,
        _ => return Err(VmError::TypeMismatch("array/map")),
    };
    Ok(vec![Value::Int(count)])
}

pub(super) fn builtin_assert(args: &[Value]) -> VmResult<Vec<Value>> {
    let condition = args
        .first()
        .ok_or_else(|| VmError::HostError("missing argument: assert condition".to_string()))?
        .as_bool()?;
    if condition {
        Ok(Vec::new())
    } else {
        Err(VmError::HostError("assertion failed".to_string()))
    }
}
