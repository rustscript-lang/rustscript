use axum::http::HeaderName;
use edge_abi::symbols::http::response as http_response;
use pd_edge_host_function::pd_edge_host_function;
use vm::bytecode::VmMap;
use vm::{CallOutcome, Value, Vm, VmError};

use super::{
    SharedProxyVmContext, headers_to_value_map, parse_header, parse_header_name, parse_headers_map,
};

/// Sets a header on the downstream HTTP response.
#[pd_edge_host_function(name = http_response::SET_HEADER.name, scope = http)]
async fn set_response_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    let mut downstream = context.lock_downstream();
    downstream
        .response_output
        .headers
        .insert(header_name, header_value);
    Ok(CallOutcome::Return(vec![]))
}

/// Removes a header from the downstream HTTP response.
#[pd_edge_host_function(name = http_response::REMOVE_HEADER.name, scope = http)]
async fn remove_response_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = parse_header_name(name)?;
    let mut downstream = context.lock_downstream();
    downstream.response_output.headers.remove(header_name);
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the body for the downstream HTTP response.
#[pd_edge_host_function(name = http_response::SET_BODY.name, scope = http)]
async fn set_response_body(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    body: String,
) -> Result<CallOutcome, VmError> {
    let mut downstream = context.lock_downstream();
    downstream.response_output.body = Some(body.into_bytes());
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the status code on the downstream HTTP response.
#[pd_edge_host_function(name = http_response::SET_STATUS.name, scope = http)]
async fn set_response_status(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    status: i64,
) -> Result<CallOutcome, VmError> {
    if !(100..=599).contains(&status) {
        return Err(VmError::HostError(format!(
            "status code must be in range 100..=599, got '{status}'",
        )));
    }
    let mut downstream = context.lock_downstream();
    downstream.response_output.status = Some(status as u16);
    Ok(CallOutcome::Return(vec![]))
}

/// Returns the status code for the downstream HTTP response.
#[pd_edge_host_function(name = http_response::GET_STATUS.name, scope = http)]
async fn get_response_status(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let downstream = context.lock_downstream();
    let status = downstream.response_output.status.unwrap_or(0);
    Ok(CallOutcome::Return(vec![Value::Int(status as i64)]))
}

/// Returns the full body for the downstream HTTP response as text.
#[pd_edge_host_function(name = http_response::GET_BODY.name, scope = http)]
async fn get_response_body(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let downstream = context.lock_downstream();
    let value = String::from_utf8_lossy(
        downstream
            .response_output
            .body
            .as_deref()
            .unwrap_or_default(),
    )
    .into_owned();
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

/// Returns the first value for a header on the downstream HTTP response.
#[pd_edge_host_function(name = http_response::GET_HEADER.name, scope = http)]
async fn get_response_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;
    let downstream = context.lock_downstream();
    let value = downstream
        .response_output
        .headers
        .get(&header_name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

/// Returns all headers on the downstream HTTP response as a map.
#[pd_edge_host_function(name = http_response::GET_HEADERS.name, scope = http)]
async fn get_response_headers(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let downstream = context.lock_downstream();
    Ok(CallOutcome::Return(vec![headers_to_value_map(
        &downstream.response_output.headers,
    )]))
}

/// Adds a header value to the downstream HTTP response.
#[pd_edge_host_function(name = http_response::ADD_HEADER.name, scope = http)]
async fn add_response_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    let mut downstream = context.lock_downstream();
    downstream
        .response_output
        .headers
        .append(header_name, header_value);
    Ok(CallOutcome::Return(vec![]))
}

/// Clears all values for a header on the downstream HTTP response.
#[pd_edge_host_function(name = http_response::CLEAR_HEADER.name, scope = http)]
async fn clear_response_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = parse_header_name(name)?;
    let mut downstream = context.lock_downstream();
    downstream.response_output.headers.remove(header_name);
    Ok(CallOutcome::Return(vec![]))
}

/// Replaces the headers on the downstream HTTP response with the provided map.
#[pd_edge_host_function(name = http_response::SET_HEADERS.name, scope = http)]
async fn set_response_headers(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    headers: VmMap,
) -> Result<CallOutcome, VmError> {
    let headers = parse_headers_map(headers)?;
    let mut downstream = context.lock_downstream();
    for (name, values) in headers {
        downstream.response_output.headers.remove(name.clone());
        for value in values {
            downstream
                .response_output
                .headers
                .append(name.clone(), value);
        }
    }
    Ok(CallOutcome::Return(vec![]))
}
