use regex::Regex;

use super::VmArray;
use crate::vm::{Value, VmError, VmResult};
use pd_host_function::pd_host_function;

#[pd_host_function(name = "re::match")]
pub(super) fn builtin_re_match(pattern: &str, text: &str) -> VmResult<bool> {
    let regex = Regex::new(pattern)
        .map_err(|err| VmError::HostError(format!("re_match invalid pattern: {err}")))?;
    Ok(regex.is_match(text))
}

#[pd_host_function(name = "re::find")]
pub(super) fn builtin_re_find(pattern: &str, text: &str) -> VmResult<Option<String>> {
    let regex = Regex::new(pattern)
        .map_err(|err| VmError::HostError(format!("re_find invalid pattern: {err}")))?;
    Ok(regex.find(text).map(|matched| matched.as_str().to_string()))
}

#[pd_host_function(name = "re::replace")]
pub(super) fn builtin_re_replace(pattern: &str, text: &str, replacement: &str) -> VmResult<String> {
    let regex = Regex::new(pattern)
        .map_err(|err| VmError::HostError(format!("re_replace invalid pattern: {err}")))?;
    Ok(regex.replace_all(text, replacement).into_owned())
}

#[pd_host_function(name = "re::split")]
pub(super) fn builtin_re_split(pattern: &str, text: &str) -> VmResult<VmArray> {
    let regex = Regex::new(pattern)
        .map_err(|err| VmError::HostError(format!("re_split invalid pattern: {err}")))?;
    Ok(regex
        .split(text)
        .map(|part| Value::string(part.to_string()))
        .collect::<Vec<_>>())
}

#[pd_host_function(name = "re::captures")]
pub(super) fn builtin_re_captures(pattern: &str, text: &str) -> VmResult<VmArray> {
    let regex = Regex::new(pattern)
        .map_err(|err| VmError::HostError(format!("re_captures invalid pattern: {err}")))?;
    let Some(captures) = regex.captures(text) else {
        return Ok(Vec::new());
    };

    let mut groups = Vec::with_capacity(captures.len());
    for index in 0..captures.len() {
        let group_value = match captures.get(index) {
            Some(group) => Value::string(group.as_str().to_string()),
            None => Value::Null,
        };
        groups.push(group_value);
    }
    Ok(groups)
}
