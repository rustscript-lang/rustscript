use axum::http::HeaderName;
use edge_abi::symbols::http::upstream::response as http_upstream_response;
use pd_edge_host_function::pd_edge_host_function;
use vm::{CallOutcome, Value, Vm, VmError};

use super::super::{
    SharedProxyVmContext, ensure_upstream_response_started, headers_to_value_map,
    read_upstream_response_all, read_upstream_response_next_chunk,
    read_upstream_response_next_line, upstream_response_eof,
};

/// Enables buffered inspection for the upstream HTTP response.
#[pd_edge_host_function(name = http_upstream_response::ENABLE_PROCESSING.name, scope = http)]
async fn enable_upstream_response_processing(
    _vm: &mut Vm,
    _context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![]))
}

/// Returns the status code for the upstream HTTP response.
#[pd_edge_host_function(name = http_upstream_response::GET_STATUS.name, scope = http)]
async fn get_upstream_response_status(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let upstream_response = ensure_upstream_response_started(&context).await?;
    Ok(CallOutcome::Return(vec![Value::Int(
        upstream_response.status as i64,
    )]))
}

/// Returns the first value for a header on the upstream HTTP response.
#[pd_edge_host_function(name = http_upstream_response::GET_HEADER.name, scope = http)]
async fn get_upstream_response_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;
    let upstream_response = ensure_upstream_response_started(&context).await?;
    let value = upstream_response
        .headers
        .get(&header_name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

/// Returns all headers on the upstream HTTP response as a map.
#[pd_edge_host_function(name = http_upstream_response::GET_HEADERS.name, scope = http)]
async fn get_upstream_response_headers(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let upstream_response = ensure_upstream_response_started(&context).await?;
    Ok(CallOutcome::Return(vec![headers_to_value_map(
        &upstream_response.headers,
    )]))
}

/// Returns the full body for the upstream HTTP response as text.
#[pd_edge_host_function(name = http_upstream_response::GET_BODY.name, scope = http)]
async fn get_upstream_response_body(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let body = read_upstream_response_all(&context).await?;
    let value = String::from_utf8_lossy(&body).into_owned();
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

/// Returns the HTTP version for the upstream HTTP response.
#[pd_edge_host_function(name = http_upstream_response::GET_HTTP_VERSION.name, scope = http)]
async fn get_upstream_response_http_version(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let upstream_response = ensure_upstream_response_started(&context).await?;
    Ok(CallOutcome::Return(vec![Value::string(
        upstream_response.http_version.clone(),
    )]))
}

/// Reads the next body chunk from the upstream HTTP response.
#[pd_edge_host_function(
    name = http_upstream_response::body::NEXT_CHUNK.name,
    scope = http_extension
)]
async fn get_upstream_response_body_chunk(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    max_bytes: i64,
) -> Result<CallOutcome, VmError> {
    if max_bytes <= 0 {
        return Err(VmError::HostError(format!(
            "body chunk size must be > 0, got '{max_bytes}'",
        )));
    }
    let chunk = read_upstream_response_next_chunk(&context, max_bytes as usize).await?;
    Ok(CallOutcome::Return(vec![Value::string(
        String::from_utf8_lossy(&chunk).into_owned(),
    )]))
}

/// Reads the next body line from the upstream HTTP response.
#[pd_edge_host_function(
    name = http_upstream_response::body::NEXT_LINE.name,
    scope = http_extension
)]
async fn get_upstream_response_body_line(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let line = read_upstream_response_next_line(&context).await?;
    Ok(CallOutcome::Return(vec![Value::string(
        String::from_utf8_lossy(&line).into_owned(),
    )]))
}

/// Returns whether the body stream for the upstream HTTP response is exhausted.
#[pd_edge_host_function(name = http_upstream_response::body::EOF.name, scope = http_extension)]
async fn get_upstream_response_body_eof(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let eof = upstream_response_eof(&context).await?;
    Ok(CallOutcome::Return(vec![Value::Bool(eof)]))
}
