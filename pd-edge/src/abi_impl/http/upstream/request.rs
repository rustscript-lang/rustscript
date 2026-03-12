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
    context.with_default_upstream_request_mut(|request| {
        request.query = query.to_string();
    });
    Ok(CallOutcome::Return(vec![]))
}

/// Sets a header on the upstream HTTP request.
#[pd_edge_host_function(name = http_upstream_request::SET_HEADER.name, scope = http)]
async fn set_upstream_request_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    context.with_default_upstream_request_mut(|request| {
        request.headers.insert(header_name, header_value);
    });
    Ok(CallOutcome::Return(vec![]))
}

/// Removes a header from the upstream HTTP request.
#[pd_edge_host_function(name = http_upstream_request::REMOVE_HEADER.name, scope = http)]
async fn remove_upstream_request_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = parse_header_name(name)?;
    context.with_default_upstream_request_mut(|request| {
        request.headers.remove(header_name);
    });
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the HTTP method on the upstream HTTP request.
#[pd_edge_host_function(name = http_upstream_request::SET_METHOD.name, scope = http)]
async fn set_upstream_request_method(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    method: String,
) -> Result<CallOutcome, VmError> {
    let parsed = Method::from_bytes(method.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid http method '{method}'")))?;
    context.with_default_upstream_request_mut(|request| {
        request.method = parsed;
    });
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the request path on the upstream HTTP request.
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
    context.with_default_upstream_request_mut(|request| {
        request.path = path;
    });
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the decoded query string on the upstream HTTP request.
#[pd_edge_host_function(name = http_upstream_request::SET_QUERY.name, scope = http)]
async fn set_upstream_request_query(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    raw_query: String,
) -> Result<CallOutcome, VmError> {
    apply_upstream_query(context, raw_query).await
}

/// Sets the target endpoint for the upstream HTTP request.
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
    let target = upstream.clone();
    context.with_default_upstream_request_mut(|request| {
        request.target = Some(upstream);
    });
    configure_upstream_transport_for_target(&context, &target);
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the body for the upstream HTTP request.
#[pd_edge_host_function(name = http_upstream_request::SET_BODY.name, scope = http)]
async fn set_upstream_request_body(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    body: String,
) -> Result<CallOutcome, VmError> {
    context.with_default_upstream_request_mut(|request| {
        request.body_override = Some(body.into_bytes());
    });
    Ok(CallOutcome::Return(vec![]))
}

/// Adds a header value to the upstream HTTP request.
#[pd_edge_host_function(name = http_upstream_request::ADD_HEADER.name, scope = http)]
async fn add_upstream_request_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    context.with_default_upstream_request_mut(|request| {
        request.headers.append(header_name, header_value);
    });
    Ok(CallOutcome::Return(vec![]))
}

/// Clears all values for a header on the upstream HTTP request.
#[pd_edge_host_function(name = http_upstream_request::CLEAR_HEADER.name, scope = http)]
async fn clear_upstream_request_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = parse_header_name(name)?;
    context.with_default_upstream_request_mut(|request| {
        request.headers.remove(header_name);
    });
    Ok(CallOutcome::Return(vec![]))
}

/// Replaces the headers on the upstream HTTP request with the provided map.
#[pd_edge_host_function(name = http_upstream_request::SET_HEADERS.name, scope = http)]
async fn set_upstream_request_headers(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    headers: VmMap,
) -> Result<CallOutcome, VmError> {
    let headers = parse_headers_map(headers)?;
    context.with_default_upstream_request_mut(|request| {
        for (name, values) in headers {
            request.headers.remove(name.clone());
            for value in values {
                request.headers.append(name.clone(), value);
            }
        }
    });
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the raw query string on the upstream HTTP request.
#[pd_edge_host_function(name = http_upstream_request::SET_RAW_QUERY.name, scope = http)]
async fn set_upstream_request_raw_query(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    raw_query: String,
) -> Result<CallOutcome, VmError> {
    apply_upstream_query(context, raw_query).await
}

/// Sets a query parameter on the upstream HTTP request.
#[pd_edge_host_function(name = http_upstream_request::SET_QUERY_ARG.name, scope = http)]
async fn set_upstream_request_query_arg(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    key: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    context.with_default_upstream_request_mut(|request| {
        let mut pairs = url::form_urlencoded::parse(request.query.as_bytes())
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        pairs.retain(|(name, _)| name != &key);
        pairs.push((key, value));
        request.query = serialize_query_pairs(pairs);
    });
    Ok(CallOutcome::Return(vec![]))
}
