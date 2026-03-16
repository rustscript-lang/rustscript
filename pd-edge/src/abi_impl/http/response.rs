use axum::http::HeaderName;
use edge_abi::symbols::http::response as http_response;
use pd_edge_host_function::pd_edge_host_function;
use vm::{CallOutcome, Value, Vm, VmError};

use super::{
    SharedProxyVmContext, ensure_outbound_exchange_response_started, headers_to_value_map,
    is_hop_by_hop_header, parse_header, parse_header_name, read_outbound_exchange_response_all,
};

fn apply_response_header_batch(
    context: &SharedProxyVmContext,
    headers: Value,
) -> Result<(), VmError> {
    match headers {
        Value::Null => Ok(()),
        Value::Array(values) => {
            if values.len() % 2 != 0 {
                return Err(VmError::HostError(
                    "response header batch arrays must contain alternating name/value string pairs"
                        .to_string(),
                ));
            }
            context.with_downstream_response_mut(|response| {
                for pair in values.chunks(2) {
                    let name = pair[0]
                        .clone()
                        .into_owned_string()
                        .expect("checked by host type conversion");
                    let value = pair[1]
                        .clone()
                        .into_owned_string()
                        .expect("checked by host type conversion");
                    let (header_name, header_value) =
                        parse_header(name, value).expect("checked before response mutation");
                    response.headers.insert(header_name, header_value);
                }
            });
            Ok(())
        }
        Value::Map(entries) => {
            context.with_downstream_response_mut(|response| {
                for (key, value) in entries.as_ref() {
                    let name = key
                        .clone()
                        .into_owned_string()
                        .expect("checked by host type conversion");
                    let value = value
                        .clone()
                        .into_owned_string()
                        .expect("checked by host type conversion");
                    let (header_name, header_value) =
                        parse_header(name, value).expect("checked before response mutation");
                    response.headers.insert(header_name, header_value);
                }
            });
            Ok(())
        }
        _ => Err(VmError::HostError(
            "response header batch must be null, an array of alternating strings, or a string map"
                .to_string(),
        )),
    }
}

fn validate_response_header_batch(headers: &Value) -> Result<(), VmError> {
    match headers {
        Value::Array(values) => {
            if values.len() % 2 != 0 {
                return Err(VmError::HostError(
                    "response header batch arrays must contain alternating name/value string pairs"
                        .to_string(),
                ));
            }
            for pair in values.chunks(2) {
                let name = pair[0].clone().into_owned_string().map_err(|_| {
                    VmError::HostError(
                        "response header batch array keys must be strings".to_string(),
                    )
                })?;
                let value = pair[1].clone().into_owned_string().map_err(|_| {
                    VmError::HostError(
                        "response header batch array values must be strings".to_string(),
                    )
                })?;
                let _ = parse_header(name, value)?;
            }
            Ok(())
        }
        Value::Map(entries) => {
            for (key, value) in entries.as_ref() {
                let name = key.clone().into_owned_string().map_err(|_| {
                    VmError::HostError("response header batch map keys must be strings".to_string())
                })?;
                let value = value.clone().into_owned_string().map_err(|_| {
                    VmError::HostError(
                        "response header batch map values must be strings".to_string(),
                    )
                })?;
                let _ = parse_header(name, value)?;
            }
            Ok(())
        }
        Value::Null => Ok(()),
        _ => Err(VmError::HostError(
            "response header batch must be null, an array of alternating strings, or a string map"
                .to_string(),
        )),
    }
}

/// Sets a header on the downstream HTTP response.
#[pd_edge_host_function(name = http_response::SET_HEADER.name, scope = http)]
fn set_response_header(
    context: SharedProxyVmContext,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    context.with_downstream_response_mut(|response| {
        response.headers.insert(header_name, header_value);
    });
    Ok(CallOutcome::Return(vec![]))
}

/// Sets a batch of downstream HTTP response headers from alternating string pairs or a string map.
#[pd_edge_host_function(name = http_response::SET_HEADERS.name, scope = http)]
fn set_response_headers(
    context: SharedProxyVmContext,
    headers: Value,
) -> Result<CallOutcome, VmError> {
    validate_response_header_batch(&headers)?;
    apply_response_header_batch(&context, headers)?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the body for the downstream HTTP response.
#[pd_edge_host_function(name = http_response::SET_BODY.name, scope = http)]
fn set_response_body(
    context: SharedProxyVmContext,
    body: String,
) -> Result<CallOutcome, VmError> {
    context.with_downstream_response_mut(|response| {
        response.body = Some(body.into_bytes());
    });
    Ok(CallOutcome::Return(vec![]))
}

/// Copies the full response from an outbound HTTP exchange into the downstream HTTP response and
/// overlays a batch of downstream headers.
#[pd_edge_host_function(name = http_response::APPLY_EXCHANGE_WITH_HEADERS.name, scope = http)]
async fn apply_exchange_to_response_with_headers(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    headers: Value,
) -> Result<CallOutcome, VmError> {
    validate_response_header_batch(&headers)?;
    let snapshot = ensure_outbound_exchange_response_started(&context, exchange).await?;
    let body = read_outbound_exchange_response_all(&context, exchange).await?;
    context.with_downstream_response_mut(|response| {
        response.status = Some(snapshot.status);
        response.body = Some(body);
        for (name, value) in &snapshot.headers {
            if !is_hop_by_hop_header(name) {
                response.headers.insert(name.clone(), value.clone());
            }
        }
    });
    apply_response_header_batch(&context, headers)?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the status code on the downstream HTTP response.
#[pd_edge_host_function(name = http_response::SET_STATUS.name, scope = http)]
fn set_response_status(
    context: SharedProxyVmContext,
    status: i64,
) -> Result<CallOutcome, VmError> {
    if !(100..=599).contains(&status) {
        return Err(VmError::HostError(format!(
            "status code must be in range 100..=599, got '{status}'",
        )));
    }
    context.with_downstream_response_mut(|response| {
        response.status = Some(status as u16);
    });
    Ok(CallOutcome::Return(vec![]))
}

/// Copies the full response from an outbound HTTP exchange into the downstream HTTP response.
#[pd_edge_host_function(name = http_response::APPLY_EXCHANGE.name, scope = http)]
async fn apply_exchange_to_response(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
) -> Result<CallOutcome, VmError> {
    let snapshot = ensure_outbound_exchange_response_started(&context, exchange).await?;
    let body = read_outbound_exchange_response_all(&context, exchange).await?;
    context.with_downstream_response_mut(|response| {
        response.status = Some(snapshot.status);
        response.body = Some(body);
        for (name, value) in &snapshot.headers {
            if !is_hop_by_hop_header(name) {
                response.headers.insert(name.clone(), value.clone());
            }
        }
    });
    Ok(CallOutcome::Return(vec![]))
}

/// Returns the status code for the downstream HTTP response.
#[pd_edge_host_function(name = http_response::GET_STATUS.name, scope = http)]
fn get_response_status(context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    let status = context.with_downstream_response(|response| response.status.unwrap_or(0));
    Ok(CallOutcome::Return(vec![Value::Int(status as i64)]))
}

/// Returns the full body for the downstream HTTP response as text.
#[pd_edge_host_function(name = http_response::GET_BODY.name, scope = http)]
fn get_response_body(context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    let value = context.with_downstream_response(|response| {
        String::from_utf8_lossy(response.body.as_deref().unwrap_or_default()).into_owned()
    });
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

/// Returns the first value for a header on the downstream HTTP response.
#[pd_edge_host_function(name = http_response::GET_HEADER.name, scope = http)]
fn get_response_header(
    context: SharedProxyVmContext,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;
    let value = context.with_downstream_response(|response| {
        response
            .headers
            .get(&header_name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string()
    });
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

/// Returns all headers on the downstream HTTP response as a map.
#[pd_edge_host_function(name = http_response::GET_HEADERS.name, scope = http)]
fn get_response_headers(context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![headers_to_value_map(
        &context.with_downstream_response(|response| response.headers.clone()),
    )]))
}

/// Adds a header value to the downstream HTTP response.
#[pd_edge_host_function(name = http_response::ADD_HEADER.name, scope = http)]
fn add_response_header(
    context: SharedProxyVmContext,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    context.with_downstream_response_mut(|response| {
        response.headers.append(header_name, header_value);
    });
    Ok(CallOutcome::Return(vec![]))
}

/// Clears all values for a header on the downstream HTTP response.
#[pd_edge_host_function(name = http_response::CLEAR_HEADER.name, scope = http)]
fn clear_response_header(
    context: SharedProxyVmContext,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = parse_header_name(name)?;
    context.with_downstream_response_mut(|response| {
        response.headers.remove(header_name);
    });
    Ok(CallOutcome::Return(vec![]))
}
