use axum::http::HeaderName;
use edge_abi::symbols::http::upstream::response as http_upstream_response;
use pd_edge_host_function::pd_edge_host_function;
use vm::{CallOutcome, Value, Vm, VmError};

use super::super::super::{current_vm_context, headers_to_value_map};

#[pd_edge_host_function(name = http_upstream_response::GET_STATUS.name, scope = http)]
async fn get_upstream_response_status(_vm: &mut Vm) -> Result<CallOutcome, VmError> {
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_upstream_response();
    let status = context.upstream_response_status.unwrap_or(0);
    Ok(CallOutcome::Return(vec![Value::Int(status as i64)]))
}

#[pd_edge_host_function(name = http_upstream_response::GET_HEADER.name, scope = http)]
async fn get_upstream_response_header(_vm: &mut Vm, name: String) -> Result<CallOutcome, VmError> {
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_upstream_response();
    let value = context
        .upstream_response_headers
        .get(&header_name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

#[pd_edge_host_function(name = http_upstream_response::GET_HEADERS.name, scope = http)]
async fn get_upstream_response_headers(_vm: &mut Vm) -> Result<CallOutcome, VmError> {
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_upstream_response();
    Ok(CallOutcome::Return(vec![headers_to_value_map(
        &context.upstream_response_headers,
    )]))
}

#[pd_edge_host_function(name = http_upstream_response::GET_BODY.name, scope = http)]
async fn get_upstream_response_body(_vm: &mut Vm) -> Result<CallOutcome, VmError> {
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_upstream_response();
    let value = context
        .upstream_response_content
        .clone()
        .unwrap_or_default();
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}
