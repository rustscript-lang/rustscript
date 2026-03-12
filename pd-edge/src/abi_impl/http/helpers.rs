#![cfg_attr(not(feature = "http"), allow(dead_code))]

use std::collections::BTreeMap;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use url::Url;
use vm::{Value, VmError, bytecode::VmMap};

pub(super) fn parse_header_name(name: String) -> Result<HeaderName, VmError> {
    HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))
}

pub(super) fn parse_header(
    name: String,
    value: String,
) -> Result<(HeaderName, HeaderValue), VmError> {
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;
    let header_value = HeaderValue::from_str(&value)
        .map_err(|_| VmError::HostError(format!("invalid header value '{value}'")))?;
    Ok((header_name, header_value))
}

pub(super) fn parse_headers_map(
    entries: VmMap,
) -> Result<Vec<(HeaderName, Vec<HeaderValue>)>, VmError> {
    let mut parsed = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        let name = match key {
            Value::String(name) => name.to_string(),
            _ => {
                return Err(VmError::HostError(
                    "header map keys must be strings".to_string(),
                ));
            }
        };
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;

        let values = match value {
            Value::String(single) => vec![single.to_string()],
            Value::Array(values) => {
                let mut collected = Vec::with_capacity(values.len());
                for value in values.iter() {
                    match value {
                        Value::String(item) => collected.push(item.to_string()),
                        _ => {
                            return Err(VmError::HostError(
                                "header map values must be strings or arrays of strings"
                                    .to_string(),
                            ));
                        }
                    }
                }
                collected
            }
            _ => {
                return Err(VmError::HostError(
                    "header map values must be strings or arrays of strings".to_string(),
                ));
            }
        };

        let mut header_values = Vec::with_capacity(values.len());
        for value in values {
            let header_value = HeaderValue::from_str(&value)
                .map_err(|_| VmError::HostError(format!("invalid header value '{value}'")))?;
            header_values.push(header_value);
        }
        parsed.push((header_name, header_values));
    }
    Ok(parsed)
}

pub(super) fn request_path_with_query(path: &str, query: &str) -> String {
    if query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{query}")
    }
}

pub(super) fn headers_to_value_map(headers: &HeaderMap) -> Value {
    let mut values = BTreeMap::<String, Vec<String>>::new();
    for (name, value) in headers {
        let header_name = name.as_str().to_string();
        let header_value = value.to_str().unwrap_or_default().to_string();
        values.entry(header_name).or_default().push(header_value);
    }
    Value::map(
        values
            .into_iter()
            .map(|(name, values)| {
                let value = if values.len() == 1 {
                    Value::string(values[0].clone())
                } else {
                    Value::array(values.into_iter().map(Value::string).collect())
                };
                (Value::string(name), value)
            })
            .collect(),
    )
}

pub(super) fn query_to_value_map(query: &str) -> Value {
    let mut values = BTreeMap::<String, Vec<String>>::new();
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        values
            .entry(name.into_owned())
            .or_default()
            .push(value.into_owned());
    }
    Value::map(
        values
            .into_iter()
            .map(|(name, values)| {
                let value = if values.len() == 1 {
                    Value::string(values[0].clone())
                } else {
                    Value::array(values.into_iter().map(Value::string).collect())
                };
                (Value::string(name), value)
            })
            .collect(),
    )
}

pub(super) fn serialize_query_pairs(pairs: Vec<(String, String)>) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(&key, &value);
    }
    serializer.finish()
}

pub(super) fn is_valid_request_path(value: &str) -> bool {
    !value.is_empty()
        && value.starts_with('/')
        && !value.contains('?')
        && !value.contains('#')
        && !value.chars().any(|ch| ch.is_whitespace())
}

pub(super) fn is_valid_upstream(value: &str) -> bool {
    if value.is_empty()
        || value.contains('/')
        || value.contains('?')
        || value.contains('#')
        || value.chars().any(|ch| ch.is_whitespace())
    {
        if let Ok(url) = Url::parse(value) {
            if url.scheme() != "http" && url.scheme() != "https" {
                return false;
            }
            if url.host_str().is_none() {
                return false;
            }
            if !url.username().is_empty() || url.password().is_some() {
                return false;
            }
            return true;
        }
        return false;
    }

    let Some((host, port)) = value.rsplit_once(':') else {
        return false;
    };
    if host.is_empty() || port.is_empty() || host.contains(':') {
        return false;
    }
    match port.parse::<u16>() {
        Ok(port) => port != 0,
        Err(_) => false,
    }
}
