use axum::http::Method;
use edge_abi::symbols::http::upstream::request as http_upstream_request;
use pd_edge_host_function::pd_edge_host_function;
use vm::bytecode::VmMap;
use vm::{CallOutcome, Vm, VmError};

use super::super::super::transport::configure_upstream_transport_for_target;
use super::super::{
    SharedProxyVmContext, is_valid_request_path, is_valid_upstream, parse_header,
    parse_header_name, parse_headers_map, serialize_query_pairs,
};

async fn apply_upstream_query(
    context: SharedProxyVmContext,
    raw_query: String,
) -> Result<CallOutcome, VmError> {
    let query = raw_query.strip_prefix('?').unwrap_or(raw_query.as_str());
    if query.contains('#') || query.chars().any(|ch| ch.is_whitespace()) {
        return Err(VmError::HostError(format!(
            "query must not contain whitespace or '#', got '{raw_query}'",
        )));
    }
    let mut context = context.lock().expect("vm context lock poisoned");
    context.outbound_request.query = query.to_string();
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_upstream_request::SET_HEADER.name, scope = http)]
async fn set_upstream_request_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context
        .outbound_request
        .headers
        .insert(header_name, header_value);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_upstream_request::REMOVE_HEADER.name, scope = http)]
async fn remove_upstream_request_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = parse_header_name(name)?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.outbound_request.headers.remove(header_name);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_upstream_request::SET_METHOD.name, scope = http)]
async fn set_upstream_request_method(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    method: String,
) -> Result<CallOutcome, VmError> {
    let parsed = Method::from_bytes(method.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid http method '{method}'")))?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.outbound_request.method = parsed;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_upstream_request::SET_PATH.name, scope = http)]
async fn set_upstream_request_path(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    path: String,
) -> Result<CallOutcome, VmError> {
    if !is_valid_request_path(&path) {
        return Err(VmError::HostError(format!(
            "path must start with '/' and must not contain whitespace, '?', or '#', got '{path}'",
        )));
    }
    let mut context = context.lock().expect("vm context lock poisoned");
    context.outbound_request.path = path;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_upstream_request::SET_QUERY.name, scope = http)]
async fn set_upstream_request_query(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    raw_query: String,
) -> Result<CallOutcome, VmError> {
    apply_upstream_query(context, raw_query).await
}

#[pd_edge_host_function(name = http_upstream_request::SET_TARGET.name, scope = http)]
async fn set_upstream_request_target(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    upstream: String,
) -> Result<CallOutcome, VmError> {
    if !is_valid_upstream(&upstream) {
        return Err(VmError::HostError(format!(
            "upstream must be host:port or http(s)://host[:port][/path], got '{upstream}'",
        )));
    }
    let mut context = context.lock().expect("vm context lock poisoned");
    context.outbound_request.target = Some(upstream);
    let target = context
        .outbound_request
        .target
        .clone()
        .expect("upstream target should be set");
    configure_upstream_transport_for_target(&mut context, &target);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_upstream_request::SET_BODY.name, scope = http)]
async fn set_upstream_request_body(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    body: String,
) -> Result<CallOutcome, VmError> {
    let mut context = context.lock().expect("vm context lock poisoned");
    context.outbound_request.body_override = Some(body.into_bytes());
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_upstream_request::ADD_HEADER.name, scope = http)]
async fn add_upstream_request_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context
        .outbound_request
        .headers
        .append(header_name, header_value);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_upstream_request::CLEAR_HEADER.name, scope = http)]
async fn clear_upstream_request_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = parse_header_name(name)?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.outbound_request.headers.remove(header_name);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_upstream_request::SET_HEADERS.name, scope = http)]
async fn set_upstream_request_headers(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    headers: VmMap,
) -> Result<CallOutcome, VmError> {
    let headers = parse_headers_map(headers)?;
    let mut context = context.lock().expect("vm context lock poisoned");
    for (name, values) in headers {
        context.outbound_request.headers.remove(name.clone());
        for value in values {
            context.outbound_request.headers.append(name.clone(), value);
        }
    }
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_upstream_request::SET_RAW_QUERY.name, scope = http)]
async fn set_upstream_request_raw_query(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    raw_query: String,
) -> Result<CallOutcome, VmError> {
    apply_upstream_query(context, raw_query).await
}

#[pd_edge_host_function(name = http_upstream_request::SET_QUERY_ARG.name, scope = http)]
async fn set_upstream_request_query_arg(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    key: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let mut context = context.lock().expect("vm context lock poisoned");
    let mut pairs = url::form_urlencoded::parse(context.outbound_request.query.as_bytes())
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    pairs.retain(|(name, _)| name != &key);
    pairs.push((key, value));
    context.outbound_request.query = serialize_query_pairs(pairs);
    Ok(CallOutcome::Return(vec![]))
}
