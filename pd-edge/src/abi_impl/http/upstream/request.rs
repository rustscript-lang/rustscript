use axum::http::Method;
use edge_abi::symbols::http::upstream::request as http_upstream_request;
use pd_edge_host_function::pd_edge_host_function;
use vm::bytecode::VmMap;
use vm::{CallOutcome, Vm, VmError};

use super::super::super::{
    current_vm_context, is_valid_request_path, is_valid_upstream, parse_header, parse_header_name,
    parse_headers_map, serialize_query_pairs,
};

fn apply_upstream_query(raw_query: String) -> Result<CallOutcome, VmError> {
    let query = raw_query.strip_prefix('?').unwrap_or(raw_query.as_str());
    if query.contains('#') || query.chars().any(|ch| ch.is_whitespace()) {
        return Err(VmError::HostError(format!(
            "query must not contain whitespace or '#', got '{raw_query}'",
        )));
    }
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_upstream_request();
    context.outbound_request_query = query.to_string();
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_upstream_request::SET_HEADER.name, scope = http)]
async fn set_upstream_request_header(
    _vm: &mut Vm,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_upstream_request();
    context
        .outbound_request_headers
        .insert(header_name, header_value);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_upstream_request::REMOVE_HEADER.name, scope = http)]
async fn remove_upstream_request_header(
    _vm: &mut Vm,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = parse_header_name(name)?;
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_upstream_request();
    context.outbound_request_headers.remove(header_name);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_upstream_request::SET_METHOD.name, scope = http)]
async fn set_upstream_request_method(_vm: &mut Vm, method: String) -> Result<CallOutcome, VmError> {
    let parsed = Method::from_bytes(method.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid http method '{method}'")))?;
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_upstream_request();
    context.outbound_request_method = parsed;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_upstream_request::SET_PATH.name, scope = http)]
async fn set_upstream_request_path(_vm: &mut Vm, path: String) -> Result<CallOutcome, VmError> {
    if !is_valid_request_path(&path) {
        return Err(VmError::HostError(format!(
            "path must start with '/' and must not contain whitespace, '?', or '#', got '{path}'",
        )));
    }
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_upstream_request();
    context.outbound_request_path = path;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_upstream_request::SET_QUERY.name, scope = http)]
async fn set_upstream_request_query(
    _vm: &mut Vm,
    raw_query: String,
) -> Result<CallOutcome, VmError> {
    apply_upstream_query(raw_query)
}

#[pd_edge_host_function(name = http_upstream_request::SET_TARGET.name, scope = http)]
async fn set_upstream_request_target(
    _vm: &mut Vm,
    upstream: String,
) -> Result<CallOutcome, VmError> {
    if !is_valid_upstream(&upstream) {
        return Err(VmError::HostError(format!(
            "upstream must be host:port or http(s)://host[:port][/path], got '{upstream}'",
        )));
    }
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_upstream_request();
    context.upstream = Some(upstream);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_upstream_request::SET_BODY.name, scope = http)]
async fn set_upstream_request_body(_vm: &mut Vm, body: String) -> Result<CallOutcome, VmError> {
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_upstream_request();
    context.outbound_request_body = body.into_bytes();
    context.outbound_request_body_overridden = true;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_upstream_request::ADD_HEADER.name, scope = http)]
async fn add_upstream_request_header(
    _vm: &mut Vm,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_upstream_request();
    context
        .outbound_request_headers
        .append(header_name, header_value);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_upstream_request::CLEAR_HEADER.name, scope = http)]
async fn clear_upstream_request_header(_vm: &mut Vm, name: String) -> Result<CallOutcome, VmError> {
    let header_name = parse_header_name(name)?;
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_upstream_request();
    context.outbound_request_headers.remove(header_name);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_upstream_request::SET_HEADERS.name, scope = http)]
async fn set_upstream_request_headers(
    _vm: &mut Vm,
    headers: VmMap,
) -> Result<CallOutcome, VmError> {
    let headers = parse_headers_map(headers)?;
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_upstream_request();
    for (name, values) in headers {
        context.outbound_request_headers.remove(name.clone());
        for value in values {
            context.outbound_request_headers.append(name.clone(), value);
        }
    }
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_upstream_request::SET_RAW_QUERY.name, scope = http)]
async fn set_upstream_request_raw_query(
    _vm: &mut Vm,
    raw_query: String,
) -> Result<CallOutcome, VmError> {
    apply_upstream_query(raw_query)
}

#[pd_edge_host_function(name = http_upstream_request::SET_QUERY_ARG.name, scope = http)]
async fn set_upstream_request_query_arg(
    _vm: &mut Vm,
    key: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_upstream_request();
    let mut pairs = url::form_urlencoded::parse(context.outbound_request_query.as_bytes())
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    pairs.retain(|(name, _)| name != &key);
    pairs.push((key, value));
    context.outbound_request_query = serialize_query_pairs(pairs);
    Ok(CallOutcome::Return(vec![]))
}
