use regex::Regex;

use super::super::{Value, VmError, VmResult};
use super::arg_string;

pub(super) fn builtin_re_is_match(args: &[Value]) -> VmResult<Vec<Value>> {
    let pattern = arg_string(args, 0, "re_is_match pattern")?;
    let text = arg_string(args, 1, "re_is_match text")?;
    let regex = Regex::new(pattern)
        .map_err(|err| VmError::HostError(format!("re_is_match invalid pattern: {err}")))?;
    Ok(vec![Value::Bool(regex.is_match(text))])
}

pub(super) fn builtin_re_find(args: &[Value]) -> VmResult<Vec<Value>> {
    let pattern = arg_string(args, 0, "re_find pattern")?;
    let text = arg_string(args, 1, "re_find text")?;
    let regex = Regex::new(pattern)
        .map_err(|err| VmError::HostError(format!("re_find invalid pattern: {err}")))?;
    let value = match regex.find(text) {
        Some(matched) => Value::string(matched.as_str().to_string()),
        None => Value::Null,
    };
    Ok(vec![value])
}

pub(super) fn builtin_re_replace(args: &[Value]) -> VmResult<Vec<Value>> {
    let pattern = arg_string(args, 0, "re_replace pattern")?;
    let text = arg_string(args, 1, "re_replace text")?;
    let replacement = arg_string(args, 2, "re_replace replacement")?;
    let regex = Regex::new(pattern)
        .map_err(|err| VmError::HostError(format!("re_replace invalid pattern: {err}")))?;
    let replaced = regex.replace_all(text, replacement).into_owned();
    Ok(vec![Value::string(replaced)])
}

pub(super) fn builtin_re_split(args: &[Value]) -> VmResult<Vec<Value>> {
    let pattern = arg_string(args, 0, "re_split pattern")?;
    let text = arg_string(args, 1, "re_split text")?;
    let regex = Regex::new(pattern)
        .map_err(|err| VmError::HostError(format!("re_split invalid pattern: {err}")))?;
    let parts = regex
        .split(text)
        .map(|part| Value::string(part.to_string()))
        .collect::<Vec<_>>();
    Ok(vec![Value::array(parts)])
}

pub(super) fn builtin_re_captures(args: &[Value]) -> VmResult<Vec<Value>> {
    let pattern = arg_string(args, 0, "re_captures pattern")?;
    let text = arg_string(args, 1, "re_captures text")?;
    let regex = Regex::new(pattern)
        .map_err(|err| VmError::HostError(format!("re_captures invalid pattern: {err}")))?;
    let Some(captures) = regex.captures(text) else {
        return Ok(vec![Value::array(Vec::new())]);
    };

    let mut groups = Vec::with_capacity(captures.len());
    for index in 0..captures.len() {
        let group_value = match captures.get(index) {
            Some(group) => Value::string(group.as_str().to_string()),
            None => Value::Null,
        };
        groups.push(group_value);
    }
    Ok(vec![Value::array(groups)])
}
