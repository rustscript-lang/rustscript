use axum::http::HeaderName;
use edge_abi::symbols::http::response as http_response;
use pd_edge_host_function::pd_edge_host_function;
use vm::bytecode::VmMap;
use vm::{CallOutcome, Value, Vm, VmError};

use super::{
    SharedProxyVmContext, headers_to_value_map, parse_header, parse_header_name, parse_headers_map,
};

#[pd_edge_host_function(name = http_response::SET_HEADER.name, scope = http)]
async fn set_response_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context
        .response_output
        .headers
        .insert(header_name, header_value);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_response::REMOVE_HEADER.name, scope = http)]
async fn remove_response_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = parse_header_name(name)?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.response_output.headers.remove(header_name);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_response::SET_BODY.name, scope = http)]
async fn set_response_body(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    body: String,
) -> Result<CallOutcome, VmError> {
    let mut context = context.lock().expect("vm context lock poisoned");
    context.response_output.body = Some(body.into_bytes());
    Ok(CallOutcome::Return(vec![]))
}

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
    let mut context = context.lock().expect("vm context lock poisoned");
    context.response_output.status = Some(status as u16);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_response::GET_STATUS.name, scope = http)]
async fn get_response_status(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let context = context.lock().expect("vm context lock poisoned");
    let status = context.response_output.status.unwrap_or(0);
    Ok(CallOutcome::Return(vec![Value::Int(status as i64)]))
}

#[pd_edge_host_function(name = http_response::GET_BODY.name, scope = http)]
async fn get_response_body(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let context = context.lock().expect("vm context lock poisoned");
    let value =
        String::from_utf8_lossy(context.response_output.body.as_deref().unwrap_or_default())
            .into_owned();
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

#[pd_edge_host_function(name = http_response::GET_HEADER.name, scope = http)]
async fn get_response_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;
    let context = context.lock().expect("vm context lock poisoned");
    let value = context
        .response_output
        .headers
        .get(&header_name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

#[pd_edge_host_function(name = http_response::GET_HEADERS.name, scope = http)]
async fn get_response_headers(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let context = context.lock().expect("vm context lock poisoned");
    Ok(CallOutcome::Return(vec![headers_to_value_map(
        &context.response_output.headers,
    )]))
}

#[pd_edge_host_function(name = http_response::ADD_HEADER.name, scope = http)]
async fn add_response_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context
        .response_output
        .headers
        .append(header_name, header_value);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_response::CLEAR_HEADER.name, scope = http)]
async fn clear_response_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = parse_header_name(name)?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.response_output.headers.remove(header_name);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_response::SET_HEADERS.name, scope = http)]
async fn set_response_headers(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    headers: VmMap,
) -> Result<CallOutcome, VmError> {
    let headers = parse_headers_map(headers)?;
    let mut context = context.lock().expect("vm context lock poisoned");
    for (name, values) in headers {
        context.response_output.headers.remove(name.clone());
        for value in values {
            context.response_output.headers.append(name.clone(), value);
        }
    }
    Ok(CallOutcome::Return(vec![]))
}
