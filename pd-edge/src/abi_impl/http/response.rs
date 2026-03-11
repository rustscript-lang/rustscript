use axum::http::HeaderName;
use edge_abi::symbols::http::response as http_response;
use pd_edge_host_function::pd_edge_host_function;
use vm::bytecode::VmMap;
use vm::{CallOutcome, Value, Vm, VmError};

use super::super::{
    current_vm_context, headers_to_value_map, parse_header, parse_header_name, parse_headers_map,
};

#[pd_edge_host_function(name = http_response::SET_HEADER.name, scope = http)]
async fn set_response_header(
    _vm: &mut Vm,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_response_output();
    context.response_headers.insert(header_name, header_value);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_response::REMOVE_HEADER.name, scope = http)]
async fn remove_response_header(_vm: &mut Vm, name: String) -> Result<CallOutcome, VmError> {
    let header_name = parse_header_name(name)?;
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_response_output();
    context.response_headers.remove(header_name);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_response::SET_BODY.name, scope = http)]
async fn set_response_body(_vm: &mut Vm, body: String) -> Result<CallOutcome, VmError> {
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_response_output();
    context.response_content = Some(body);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_response::SET_STATUS.name, scope = http)]
async fn set_response_status(_vm: &mut Vm, status: i64) -> Result<CallOutcome, VmError> {
    if !(100..=599).contains(&status) {
        return Err(VmError::HostError(format!(
            "status code must be in range 100..=599, got '{status}'",
        )));
    }
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_response_output();
    context.response_status = Some(status as u16);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_response::GET_STATUS.name, scope = http)]
async fn get_response_status(_vm: &mut Vm) -> Result<CallOutcome, VmError> {
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_response_output();
    let status = context.response_status.unwrap_or(0);
    Ok(CallOutcome::Return(vec![Value::Int(status as i64)]))
}

#[pd_edge_host_function(name = http_response::GET_BODY.name, scope = http)]
async fn get_response_body(_vm: &mut Vm) -> Result<CallOutcome, VmError> {
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_response_output();
    let value = context.response_content.clone().unwrap_or_default();
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

#[pd_edge_host_function(name = http_response::GET_HEADER.name, scope = http)]
async fn get_response_header(_vm: &mut Vm, name: String) -> Result<CallOutcome, VmError> {
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_response_output();
    let value = context
        .response_headers
        .get(&header_name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

#[pd_edge_host_function(name = http_response::GET_HEADERS.name, scope = http)]
async fn get_response_headers(_vm: &mut Vm) -> Result<CallOutcome, VmError> {
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_response_output();
    Ok(CallOutcome::Return(vec![headers_to_value_map(
        &context.response_headers,
    )]))
}

#[pd_edge_host_function(name = http_response::ADD_HEADER.name, scope = http)]
async fn add_response_header(
    _vm: &mut Vm,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_response_output();
    context.response_headers.append(header_name, header_value);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_response::CLEAR_HEADER.name, scope = http)]
async fn clear_response_header(_vm: &mut Vm, name: String) -> Result<CallOutcome, VmError> {
    let header_name = parse_header_name(name)?;
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_response_output();
    context.response_headers.remove(header_name);
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_response::SET_HEADERS.name, scope = http)]
async fn set_response_headers(_vm: &mut Vm, headers: VmMap) -> Result<CallOutcome, VmError> {
    let headers = parse_headers_map(headers)?;
    let context = current_vm_context()?;
    let mut context = context.lock().expect("vm context lock poisoned");
    context.touch_response_output();
    for (name, values) in headers {
        context.response_headers.remove(name.clone());
        for value in values {
            context.response_headers.append(name.clone(), value);
        }
    }
    Ok(CallOutcome::Return(vec![]))
}
