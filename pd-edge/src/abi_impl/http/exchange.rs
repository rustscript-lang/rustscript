use axum::http::{HeaderName, Method};
use edge_abi::symbols::http::exchange as http_exchange;
use pd_edge_host_function::pd_edge_host_function;
use vm::bytecode::VmMap;
use vm::{CallOutcome, Value, Vm, VmError};

use super::{
    SharedProxyVmContext, allocate_outbound_exchange_handle, default_upstream_exchange_handle,
    ensure_outbound_exchange_response_started, headers_to_value_map, is_valid_request_path,
    is_valid_upstream, outbound_exchange_exists, outbound_exchange_response_eof, parse_header,
    parse_header_name, parse_headers_map, read_outbound_exchange_response_all,
    read_outbound_exchange_response_next_chunk, serialize_query_pairs,
};

fn unknown_exchange_handle(handle: i64) -> VmError {
    VmError::HostError(format!("unknown outbound exchange handle {handle}"))
}

fn ensure_known_exchange_handle(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<i64, VmError> {
    if outbound_exchange_exists(context, handle) {
        Ok(handle)
    } else {
        Err(unknown_exchange_handle(handle))
    }
}

fn with_exchange_request_mut<T>(
    context: &SharedProxyVmContext,
    handle: i64,
    mutate: impl FnOnce(
        &mut super::state::HttpOutboundRequestNode,
        &mut super::super::transport::TcpFlowState,
        &mut super::super::transport::TlsFlowState,
    ) -> T,
) -> Result<T, VmError> {
    let mut guard = context.lock().expect("vm context lock poisoned");
    if handle == default_upstream_exchange_handle() {
        let super::state::ProxyVmContext {
            outbound_request,
            tcp_dag,
            tls_dag,
            ..
        } = &mut *guard;
        return Ok(mutate(
            outbound_request,
            &mut tcp_dag.default_upstream,
            &mut tls_dag.default_upstream,
        ));
    }

    let exchange = guard
        .outbound_exchanges
        .get_mut(&handle)
        .ok_or_else(|| unknown_exchange_handle(handle))?;
    Ok(mutate(
        &mut exchange.request,
        &mut exchange.tcp_dag,
        &mut exchange.tls_dag,
    ))
}

fn apply_exchange_query(
    context: &SharedProxyVmContext,
    handle: i64,
    raw_query: String,
) -> Result<CallOutcome, VmError> {
    ensure_known_exchange_handle(context, handle)?;
    let query = raw_query.strip_prefix('?').unwrap_or(raw_query.as_str());
    if query.contains('#') || query.chars().any(|ch| ch.is_whitespace()) {
        return Err(VmError::HostError(format!(
            "query must not contain whitespace or '#', got '{raw_query}'",
        )));
    }

    with_exchange_request_mut(context, handle, |request, _tcp_flow, _tls_flow| {
        request.query = query.to_string();
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_exchange::NEW.name, scope = http)]
async fn new_exchange(_vm: &mut Vm, context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    let handle = allocate_outbound_exchange_handle(&context)?;
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

#[pd_edge_host_function(name = http_exchange::DEFAULT_UPSTREAM.name, scope = http)]
async fn default_upstream_exchange(
    _vm: &mut Vm,
    _context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::Int(
        default_upstream_exchange_handle(),
    )]))
}

#[pd_edge_host_function(name = http_exchange::SEND.name, scope = http)]
async fn send_exchange(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
) -> Result<CallOutcome, VmError> {
    ensure_known_exchange_handle(&context, exchange)?;
    ensure_outbound_exchange_response_started(&context, exchange).await?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_exchange::SET_HEADER.name, scope = http)]
async fn set_exchange_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    with_exchange_request_mut(&context, exchange, |request, _tcp_flow, _tls_flow| {
        request.headers.insert(header_name, header_value);
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_exchange::REMOVE_HEADER.name, scope = http)]
async fn remove_exchange_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = parse_header_name(name)?;
    with_exchange_request_mut(&context, exchange, |request, _tcp_flow, _tls_flow| {
        request.headers.remove(header_name);
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_exchange::SET_METHOD.name, scope = http)]
async fn set_exchange_method(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    method: String,
) -> Result<CallOutcome, VmError> {
    let parsed = Method::from_bytes(method.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid http method '{method}'")))?;
    with_exchange_request_mut(&context, exchange, |request, _tcp_flow, _tls_flow| {
        request.method = parsed;
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_exchange::SET_PATH.name, scope = http)]
async fn set_exchange_path(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    path: String,
) -> Result<CallOutcome, VmError> {
    if !is_valid_request_path(&path) {
        return Err(VmError::HostError(format!(
            "path must start with '/' and must not contain whitespace, '?', or '#', got '{path}'",
        )));
    }
    with_exchange_request_mut(&context, exchange, |request, _tcp_flow, _tls_flow| {
        request.path = path;
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_exchange::SET_QUERY.name, scope = http)]
async fn set_exchange_query(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    query: String,
) -> Result<CallOutcome, VmError> {
    apply_exchange_query(&context, exchange, query)
}

#[pd_edge_host_function(name = http_exchange::SET_TARGET.name, scope = http)]
async fn set_exchange_target(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    upstream: String,
) -> Result<CallOutcome, VmError> {
    if !is_valid_upstream(&upstream) {
        return Err(VmError::HostError(format!(
            "upstream must be host:port or http(s)://host[:port][/path], got '{upstream}'",
        )));
    }
    with_exchange_request_mut(&context, exchange, |request, tcp_flow, tls_flow| {
        request.target = Some(upstream.clone());
        tcp_flow.configure();
        tls_flow.observe_target(&upstream);
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_exchange::SET_BODY.name, scope = http)]
async fn set_exchange_body(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    body: String,
) -> Result<CallOutcome, VmError> {
    with_exchange_request_mut(&context, exchange, |request, _tcp_flow, _tls_flow| {
        request.body_override = Some(body.into_bytes());
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_exchange::ADD_HEADER.name, scope = http)]
async fn add_exchange_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    with_exchange_request_mut(&context, exchange, |request, _tcp_flow, _tls_flow| {
        request.headers.append(header_name, header_value);
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_exchange::CLEAR_HEADER.name, scope = http)]
async fn clear_exchange_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = parse_header_name(name)?;
    with_exchange_request_mut(&context, exchange, |request, _tcp_flow, _tls_flow| {
        request.headers.remove(header_name);
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_exchange::SET_HEADERS.name, scope = http)]
async fn set_exchange_headers(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    headers: VmMap,
) -> Result<CallOutcome, VmError> {
    let headers = parse_headers_map(headers)?;
    with_exchange_request_mut(&context, exchange, |request, _tcp_flow, _tls_flow| {
        for (name, values) in headers {
            request.headers.remove(name.clone());
            for value in values {
                request.headers.append(name.clone(), value);
            }
        }
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_exchange::SET_RAW_QUERY.name, scope = http)]
async fn set_exchange_raw_query(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    raw_query: String,
) -> Result<CallOutcome, VmError> {
    apply_exchange_query(&context, exchange, raw_query)
}

#[pd_edge_host_function(name = http_exchange::SET_QUERY_ARG.name, scope = http)]
async fn set_exchange_query_arg(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    key: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    with_exchange_request_mut(&context, exchange, |request, _tcp_flow, _tls_flow| {
        let mut pairs = url::form_urlencoded::parse(request.query.as_bytes())
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        pairs.retain(|(name, _)| name != &key);
        pairs.push((key, value));
        request.query = serialize_query_pairs(pairs);
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = http_exchange::GET_STATUS.name, scope = http)]
async fn get_exchange_status(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
) -> Result<CallOutcome, VmError> {
    let snapshot = ensure_outbound_exchange_response_started(&context, exchange).await?;
    Ok(CallOutcome::Return(vec![Value::Int(
        snapshot.status as i64,
    )]))
}

#[pd_edge_host_function(name = http_exchange::GET_HEADER.name, scope = http)]
async fn get_exchange_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;
    let snapshot = ensure_outbound_exchange_response_started(&context, exchange).await?;
    let value = snapshot
        .headers
        .get(&header_name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

#[pd_edge_host_function(name = http_exchange::GET_HEADERS.name, scope = http)]
async fn get_exchange_headers(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
) -> Result<CallOutcome, VmError> {
    let snapshot = ensure_outbound_exchange_response_started(&context, exchange).await?;
    Ok(CallOutcome::Return(vec![headers_to_value_map(
        &snapshot.headers,
    )]))
}

#[pd_edge_host_function(name = http_exchange::GET_BODY.name, scope = http)]
async fn get_exchange_body(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
) -> Result<CallOutcome, VmError> {
    let body = read_outbound_exchange_response_all(&context, exchange).await?;
    Ok(CallOutcome::Return(vec![Value::string(
        String::from_utf8_lossy(&body).into_owned(),
    )]))
}

#[pd_edge_host_function(
    name = http_exchange::body::NEXT_CHUNK.name,
    scope = http_extension
)]
async fn get_exchange_body_next_chunk(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    max_bytes: i64,
) -> Result<CallOutcome, VmError> {
    if max_bytes <= 0 {
        return Err(VmError::HostError(format!(
            "body chunk size must be > 0, got '{max_bytes}'",
        )));
    }
    let chunk =
        read_outbound_exchange_response_next_chunk(&context, exchange, max_bytes as usize).await?;
    Ok(CallOutcome::Return(vec![Value::string(
        String::from_utf8_lossy(&chunk).into_owned(),
    )]))
}

#[pd_edge_host_function(name = http_exchange::body::EOF.name, scope = http_extension)]
async fn get_exchange_body_eof(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
) -> Result<CallOutcome, VmError> {
    let eof = outbound_exchange_response_eof(&context, exchange).await?;
    Ok(CallOutcome::Return(vec![Value::Bool(eof)]))
}
