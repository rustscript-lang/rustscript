use super::{
    helpers::{headers_to_value_map, parse_header, parse_header_name, parse_string_header_batch},
    state::{
        DownstreamResponseStreamWriteMode, SharedProxyVmContext,
        ensure_outbound_exchange_response_started, finish_downstream_response_stream,
        is_hop_by_hop_header, materialize_downstream_response_body_source,
        read_downstream_response_trailers, start_downstream_response_stream,
        sync_response_output_body_headers, write_downstream_response_stream_bytes,
    },
};
use axum::http::{HeaderMap, HeaderName};
use edge_abi::symbols::http::response as http_response;
use pd_edge_host_function::pd_edge_host_function;
use vm::{CallOutcome, Value, Vm, VmError};

pub(crate) fn parse_response_header_batch(headers: Value) -> Result<HeaderMap, VmError> {
    parse_string_header_batch(headers, "response header batch")
}

fn apply_response_header_batch(
    context: &SharedProxyVmContext,
    headers: HeaderMap,
) -> Result<(), VmError> {
    context.insert_downstream_response_headers(headers)
}

fn validate_response_status(status: i64) -> Result<u16, VmError> {
    if !(100..=599).contains(&status) {
        return Err(VmError::HostError(format!(
            "status code must be in range 100..=599, got '{status}'",
        )));
    }
    Ok(status as u16)
}

/// Returns the status code for the downstream HTTP response.
#[pd_edge_host_function(name = http_response::GET_STATUS.name, scope = http)]
fn get_response_status(context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    let status = context.with_downstream_response(|response| response.status.unwrap_or(0));
    Ok(CallOutcome::Return(vec![Value::Int(status as i64)]))
}

/// Returns the full body for the downstream HTTP response as text.
#[pd_edge_host_function(name = http_response::GET_BODY.name, scope = http)]
async fn get_response_body(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    materialize_downstream_response_body_source(&context).await?;
    context.note_downstream_response_body_read();
    let value = context.with_downstream_response(|response| {
        if response.body_stream.is_some() {
            return Err(VmError::HostError(
                "http::response::get_body is unavailable after response streaming begins"
                    .to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(response.body.as_deref().unwrap_or_default()).into_owned())
    })?;
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

/// Returns the first trailer value for the downstream HTTP response.
#[pd_edge_host_function(name = "http::response::get_trailer", scope = http)]
async fn get_response_trailer(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid trailer name '{name}'")))?;
    let trailers = read_downstream_response_trailers(&context).await?;
    let value = trailers
        .get(&header_name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

/// Returns all trailers on the downstream HTTP response as a map.
#[pd_edge_host_function(name = "http::response::get_trailers", scope = http)]
async fn get_response_trailers(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let trailers = read_downstream_response_trailers(&context).await?;
    Ok(CallOutcome::Return(vec![headers_to_value_map(&trailers)]))
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

/// Sets a header on the downstream HTTP response.
#[pd_edge_host_function(name = http_response::SET_HEADER.name, scope = http)]
fn set_response_header(
    context: SharedProxyVmContext,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    context.insert_downstream_response_header(header_name, header_value)?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets a batch of downstream HTTP response headers from alternating string pairs or a string map.
#[pd_edge_host_function(name = http_response::SET_HEADERS.name, scope = http)]
fn set_response_headers(
    context: SharedProxyVmContext,
    headers: Value,
) -> Result<CallOutcome, VmError> {
    let parsed = parse_response_header_batch(headers)?;
    apply_response_header_batch(&context, parsed)?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the body for the downstream HTTP response.
#[pd_edge_host_function(name = http_response::SET_BODY.name, scope = http)]
fn set_response_body(context: SharedProxyVmContext, body: String) -> Result<CallOutcome, VmError> {
    context.note_downstream_response_body_mutated();
    context.with_downstream_response_mut(|response| -> Result<(), VmError> {
        if response.body_stream.is_some() {
            return Err(VmError::HostError(
                "http::response::set_body is unavailable after response streaming begins"
                    .to_string(),
            ));
        }
        response.body_source_exchange = None;
        response.body = Some(body.into_bytes());
        sync_response_output_body_headers(response);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Starts a streaming downstream HTTP response body.
#[pd_edge_host_function(name = http_response::stream::START.name, scope = http)]
fn start_response_stream(context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    start_downstream_response_stream(&context)?;
    Ok(CallOutcome::Return(vec![]))
}

/// Writes a chunk to the streaming downstream HTTP response body.
#[pd_edge_host_function(name = http_response::stream::WRITE.name, scope = http)]
fn write_response_stream(
    context: SharedProxyVmContext,
    chunk: String,
) -> Result<CallOutcome, VmError> {
    write_downstream_response_stream_bytes(
        &context,
        chunk.as_bytes(),
        DownstreamResponseStreamWriteMode::ExplicitImmediate,
    )?;
    Ok(CallOutcome::Return(vec![Value::Int(chunk.len() as i64)]))
}

/// Finishes the streaming downstream HTTP response body.
#[pd_edge_host_function(name = http_response::stream::FINISH.name, scope = http)]
fn finish_response_streaming(context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    finish_downstream_response_stream(&context)?;
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
    let parsed_headers = parse_response_header_batch(headers)?;
    let snapshot = ensure_outbound_exchange_response_started(&context, exchange).await?;
    context.with_downstream_response_mut(|response| -> Result<(), VmError> {
        if response.body_stream.is_some() {
            return Err(VmError::HostError(
                "http::response::apply_exchange_with_headers is unavailable after response streaming begins".to_string(),
            ));
        }
        response.status = Some(snapshot.status);
        response.body = None;
        response.body_source_exchange = Some(exchange);
        for (name, value) in snapshot.headers.iter() {
            if !is_hop_by_hop_header(name) {
                response.headers.insert(name.clone(), value.clone());
            }
        }
        Ok(())
    })?;
    apply_response_header_batch(&context, parsed_headers)?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the status code on the downstream HTTP response.
#[pd_edge_host_function(name = http_response::SET_STATUS.name, scope = http)]
fn set_response_status(context: SharedProxyVmContext, status: i64) -> Result<CallOutcome, VmError> {
    context.set_downstream_response_status(validate_response_status(status)?)?;
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
    context.with_downstream_response_mut(|response| -> Result<(), VmError> {
        if response.body_stream.is_some() {
            return Err(VmError::HostError(
                "http::response::apply_exchange is unavailable after response streaming begins"
                    .to_string(),
            ));
        }
        response.status = Some(snapshot.status);
        response.body = None;
        response.body_source_exchange = Some(exchange);
        for (name, value) in snapshot.headers.iter() {
            if !is_hop_by_hop_header(name) {
                response.headers.insert(name.clone(), value.clone());
            }
        }
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Adds a header value to the downstream HTTP response.
#[pd_edge_host_function(name = http_response::ADD_HEADER.name, scope = http)]
fn add_response_header(
    context: SharedProxyVmContext,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    context.note_downstream_response_headers_mutated();
    context.with_downstream_response_mut(|response| -> Result<(), VmError> {
        if response.stream_committed() {
            return Err(VmError::HostError(
                "downstream response headers are immutable after response streaming begins"
                    .to_string(),
            ));
        }
        response.headers.append(header_name, header_value);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Clears all values for a header on the downstream HTTP response.
#[pd_edge_host_function(name = http_response::CLEAR_HEADER.name, scope = http)]
fn clear_response_header(
    context: SharedProxyVmContext,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = parse_header_name(name)?;
    context.note_downstream_response_headers_mutated();
    context.with_downstream_response_mut(|response| -> Result<(), VmError> {
        if response.stream_committed() {
            return Err(VmError::HostError(
                "downstream response headers are immutable after response streaming begins"
                    .to_string(),
            ));
        }
        response.headers.remove(header_name);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}
