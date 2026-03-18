#![cfg_attr(not(feature = "http"), allow(dead_code))]

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, OnceLock, Weak},
};

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use parking_lot::Mutex;
use vm::bytecode::VmMap;
use vm::{Value, VmError};

const HEADER_BATCH_CACHE_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum HeaderBatchCacheKey {
    Array(usize),
    Map(usize),
}

#[derive(Debug)]
enum HeaderBatchCacheSource {
    Array(Weak<Vec<Value>>),
    Map(Weak<VmMap>),
}

#[derive(Debug)]
struct CachedHeaderBatch {
    source: HeaderBatchCacheSource,
    headers: HeaderMap,
}

static HEADER_BATCH_CACHE: OnceLock<Mutex<HashMap<HeaderBatchCacheKey, CachedHeaderBatch>>> =
    OnceLock::new();

fn header_batch_cache() -> &'static Mutex<HashMap<HeaderBatchCacheKey, CachedHeaderBatch>> {
    HEADER_BATCH_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn header_batch_cache_key(headers: &Value) -> Option<HeaderBatchCacheKey> {
    match headers {
        Value::Array(values) => Some(HeaderBatchCacheKey::Array(Arc::as_ptr(values) as usize)),
        Value::Map(entries) => Some(HeaderBatchCacheKey::Map(Arc::as_ptr(entries) as usize)),
        _ => None,
    }
}

fn header_batch_cache_source(headers: &Value) -> Option<HeaderBatchCacheSource> {
    match headers {
        Value::Array(values) => Some(HeaderBatchCacheSource::Array(Arc::downgrade(values))),
        Value::Map(entries) => Some(HeaderBatchCacheSource::Map(Arc::downgrade(entries))),
        _ => None,
    }
}

fn cached_header_batch_matches(source: &HeaderBatchCacheSource, headers: &Value) -> bool {
    match (source, headers) {
        (HeaderBatchCacheSource::Array(cached), Value::Array(values)) => cached
            .upgrade()
            .is_some_and(|current| Arc::ptr_eq(&current, values)),
        (HeaderBatchCacheSource::Map(cached), Value::Map(entries)) => cached
            .upgrade()
            .is_some_and(|current| Arc::ptr_eq(&current, entries)),
        _ => false,
    }
}

fn lookup_cached_header_batch(headers: &Value) -> Option<HeaderMap> {
    let key = header_batch_cache_key(headers)?;
    let mut guard = header_batch_cache().lock();
    let cached = guard.get(&key)?;
    if cached_header_batch_matches(&cached.source, headers) {
        return Some(cached.headers.clone());
    }
    guard.remove(&key);
    None
}

fn store_cached_header_batch(headers: &Value, parsed: &HeaderMap) {
    let (Some(key), Some(source)) = (
        header_batch_cache_key(headers),
        header_batch_cache_source(headers),
    ) else {
        return;
    };

    let mut guard = header_batch_cache().lock();
    if guard.len() >= HEADER_BATCH_CACHE_CAPACITY {
        guard.retain(|_, cached| match &cached.source {
            HeaderBatchCacheSource::Array(values) => values.strong_count() > 0,
            HeaderBatchCacheSource::Map(entries) => entries.strong_count() > 0,
        });
    }
    if guard.len() >= HEADER_BATCH_CACHE_CAPACITY
        && let Some(key_to_remove) = guard.keys().next().copied()
    {
        guard.remove(&key_to_remove);
    }
    guard.insert(
        key,
        CachedHeaderBatch {
            source,
            headers: parsed.clone(),
        },
    );
}

pub(super) fn parse_string_header_batch(
    headers: Value,
    batch_name: &'static str,
) -> Result<HeaderMap, VmError> {
    match headers {
        Value::Null => Ok(HeaderMap::new()),
        Value::Array(values) => {
            if let Some(parsed) = lookup_cached_header_batch(&Value::Array(values.clone())) {
                return Ok(parsed);
            }
            if values.len() % 2 != 0 {
                return Err(VmError::HostError(format!(
                    "{batch_name} arrays must contain alternating name/value string pairs",
                )));
            }
            let mut parsed = HeaderMap::new();
            for pair in values.chunks(2) {
                let Value::String(name) = &pair[0] else {
                    return Err(VmError::HostError(format!(
                        "{batch_name} array keys must be strings",
                    )));
                };
                let Value::String(value) = &pair[1] else {
                    return Err(VmError::HostError(format!(
                        "{batch_name} array values must be strings",
                    )));
                };
                let (header_name, header_value) = parse_header(name.as_str(), value.as_str())?;
                parsed.insert(header_name, header_value);
            }
            store_cached_header_batch(&Value::Array(values.clone()), &parsed);
            Ok(parsed)
        }
        Value::Map(entries) => {
            if let Some(parsed) = lookup_cached_header_batch(&Value::Map(entries.clone())) {
                return Ok(parsed);
            }
            let mut parsed = HeaderMap::new();
            for (key, value) in entries.as_ref() {
                let Value::String(name) = key else {
                    return Err(VmError::HostError(format!(
                        "{batch_name} map keys must be strings",
                    )));
                };
                let Value::String(value) = value else {
                    return Err(VmError::HostError(format!(
                        "{batch_name} map values must be strings",
                    )));
                };
                let (header_name, header_value) = parse_header(name.as_str(), value.as_str())?;
                parsed.insert(header_name, header_value);
            }
            store_cached_header_batch(&Value::Map(entries.clone()), &parsed);
            Ok(parsed)
        }
        _ => Err(VmError::HostError(format!(
            "{batch_name} must be null, an array of alternating strings, or a string map",
        ))),
    }
}

pub(super) fn parse_header_name(name: impl AsRef<str>) -> Result<HeaderName, VmError> {
    let name = name.as_ref();
    HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))
}

pub(super) fn parse_header(
    name: impl AsRef<str>,
    value: impl AsRef<str>,
) -> Result<(HeaderName, HeaderValue), VmError> {
    let name = name.as_ref();
    let value = value.as_ref();
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;
    let header_value = HeaderValue::from_str(value)
        .map_err(|_| VmError::HostError(format!("invalid header value '{value}'")))?;
    Ok((header_name, header_value))
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

pub(super) fn request_path_with_query(path: &str, query: &str) -> String {
    if query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{query}")
    }
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
