use axum::http::{HeaderName, Method};
use edge_abi::symbols::http::exchange as http_exchange;
use pd_edge_host_function::pd_edge_host_function;
use vm::{CallOutcome, Value, Vm, VmError};

#[cfg(feature = "tls")]
use super::attach_outbound_exchange_tls_transport;
use super::{
    HttpVersionPreference, SharedProxyVmContext, allocate_outbound_exchange_handle,
    attach_outbound_exchange_tcp_transport, default_upstream_exchange_handle,
    ensure_outbound_exchange_response_started, headers_to_value_map, is_valid_request_path,
    is_valid_upstream, outbound_exchange_exists, outbound_exchange_response_eof, parse_header,
    parse_header_name, read_outbound_exchange_response_all,
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
    let mut exchanges = context.lock_exchanges();
    if handle == default_upstream_exchange_handle() {
        let request = &mut exchanges
            .exchanges
            .get_mut(&handle)
            .expect("default upstream exchange should exist")
            .request;
        let mut transport = context.lock_transport();
        let mut tcp_flow = transport.tcp_dag.default_upstream.clone();
        let mut tls_flow = transport.tls_dag.default_upstream.clone();
        let result = mutate(request, &mut tcp_flow, &mut tls_flow);
        transport.tcp_dag.default_upstream = tcp_flow;
        transport.tls_dag.default_upstream = tls_flow;
        return Ok(result);
    }

    let exchange = exchanges
        .exchanges
        .get_mut(&handle)
        .ok_or_else(|| unknown_exchange_handle(handle))?;
    Ok(mutate(
        &mut exchange.request,
        &mut exchange.transport.tcp_flow,
        &mut exchange.transport.tls_flow,
    ))
}

fn apply_exchange_query(
    context: &SharedProxyVmContext,
    handle: i64,
    query: String,
) -> Result<CallOutcome, VmError> {
    ensure_known_exchange_handle(context, handle)?;
    let normalized_query = query.strip_prefix('?').unwrap_or(query.as_str());
    if normalized_query.contains('#') || normalized_query.chars().any(|ch| ch.is_whitespace()) {
        return Err(VmError::HostError(format!(
            "query must not contain whitespace or '#', got '{query}'",
        )));
    }

    with_exchange_request_mut(context, handle, |request, _tcp_flow, _tls_flow| {
        request.query = normalized_query.to_string();
    })?;
    Ok(CallOutcome::Return(vec![]))
}

fn parse_version_preference(label: &str) -> Result<HttpVersionPreference, VmError> {
    HttpVersionPreference::parse(label).ok_or_else(|| {
        VmError::HostError(format!(
            "invalid http version preference '{label}'; expected auto, 1.1, 2, or 3",
        ))
    })
}

/// Allocates an outbound HTTP exchange handle.
#[pd_edge_host_function(name = http_exchange::NEW.name, scope = http)]
async fn new_exchange(_vm: &mut Vm, context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    let handle = allocate_outbound_exchange_handle(&context)?;
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

/// Returns the default upstream handle for the outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::DEFAULT_UPSTREAM.name, scope = http)]
async fn default_upstream_exchange(
    _vm: &mut Vm,
    _context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::Int(
        default_upstream_exchange_handle(),
    )]))
}

/// Sends the outbound HTTP exchange and starts its response stream.
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

/// Sets a header on the outbound HTTP exchange.
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

/// Sets the HTTP method on the outbound HTTP exchange.
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

/// Sets the request path on the outbound HTTP exchange.
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

/// Sets the decoded query string on the outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::SET_QUERY.name, scope = http)]
async fn set_exchange_query(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    query: String,
) -> Result<CallOutcome, VmError> {
    apply_exchange_query(&context, exchange, query)
}

/// Sets the preferred HTTP version for the outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::SET_VERSION.name, scope = http)]
async fn set_exchange_version(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    version: String,
) -> Result<CallOutcome, VmError> {
    let parsed = parse_version_preference(&version)?;
    with_exchange_request_mut(&context, exchange, |request, _tcp_flow, _tls_flow| {
        request.version_preference = parsed;
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Returns the configured HTTP version preference for the outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::GET_VERSION.name, scope = http)]
async fn get_exchange_version(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
) -> Result<CallOutcome, VmError> {
    let version =
        with_exchange_request_mut(&context, exchange, |request, _tcp_flow, _tls_flow| {
            request.version_preference.as_str().to_string()
        })?;
    Ok(CallOutcome::Return(vec![Value::string(version)]))
}

/// Sets the target endpoint for the outbound HTTP exchange.
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

/// Attaches a TCP stream as the transport for an outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::ATTACH_TCP.name, scope = http)]
async fn attach_exchange_tcp(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    stream: i64,
) -> Result<CallOutcome, VmError> {
    ensure_known_exchange_handle(&context, exchange)?;
    attach_outbound_exchange_tcp_transport(&context, exchange, stream)?;
    Ok(CallOutcome::Return(vec![]))
}

#[cfg(feature = "tls")]
/// Attaches a TLS plaintext session as the transport for an outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::ATTACH_TLS_PLAINTEXT.name, scope = http)]
async fn attach_exchange_tls_plaintext(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    session: i64,
) -> Result<CallOutcome, VmError> {
    ensure_known_exchange_handle(&context, exchange)?;
    attach_outbound_exchange_tls_transport(&context, exchange, session)?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the body for the outbound HTTP exchange.
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

/// Adds a header value to the outbound HTTP exchange.
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

/// Clears all values for a header on the outbound HTTP exchange.
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

/// Sets a query parameter on the outbound HTTP exchange.
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

/// Returns the status code for the outbound HTTP exchange.
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

/// Returns the first value for a header on the outbound HTTP exchange.
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

/// Returns all headers on the outbound HTTP exchange as a map.
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

/// Returns the full body for the outbound HTTP exchange as text.
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

/// Returns the HTTP version for the outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::GET_HTTP_VERSION.name, scope = http)]
async fn get_exchange_http_version(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
) -> Result<CallOutcome, VmError> {
    let snapshot = ensure_outbound_exchange_response_started(&context, exchange).await?;
    Ok(CallOutcome::Return(vec![Value::string(
        snapshot.http_version.clone(),
    )]))
}

/// Reads the next body chunk from the outbound HTTP exchange.
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

/// Returns whether the body stream for the outbound HTTP exchange is exhausted.
#[pd_edge_host_function(name = http_exchange::body::EOF.name, scope = http_extension)]
async fn get_exchange_body_eof(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
) -> Result<CallOutcome, VmError> {
    let eof = outbound_exchange_response_eof(&context, exchange).await?;
    Ok(CallOutcome::Return(vec![Value::Bool(eof)]))
}
