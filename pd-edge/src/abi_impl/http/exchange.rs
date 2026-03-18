use axum::http::{HeaderName, Method};
use edge_abi::symbols::http::exchange as http_exchange;
use pd_edge_host_function::pd_edge_host_function;
use vm::{CallOutcome, Value, Vm, VmError};

#[cfg(feature = "tls")]
use super::attach_outbound_exchange_tls_transport;
use super::{
    HttpUpstreamScheme, HttpVersionPreference, SharedProxyVmContext,
    allocate_outbound_exchange_handle, attach_outbound_exchange_tcp_transport,
    default_upstream_exchange_handle, ensure_outbound_exchange_response_started,
    headers_to_value_map, is_valid_request_path, lookup_cached_header_batch,
    outbound_exchange_exists, outbound_exchange_response_eof, parse_header, parse_header_name,
    read_outbound_exchange_response_all, read_outbound_exchange_response_next_chunk,
    read_outbound_exchange_response_trailers, serialize_query_pairs, store_cached_header_batch,
};
use crate::abi_impl::schedule_current_future_call;

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
    mutate: impl FnOnce(&mut super::state::HttpOutboundRequestNode) -> T,
) -> Result<T, VmError> {
    let request_head = if handle == default_upstream_exchange_handle() {
        Some(context.with_request_head(Clone::clone))
    } else {
        None
    };
    let mut exchanges = context.lock_exchanges();
    let exchange = exchanges
        .exchanges
        .get_mut(&handle)
        .ok_or_else(|| unknown_exchange_handle(handle))?;
    if let Some(request_head) = request_head.as_ref() {
        exchange
            .request
            .materialize_inherited_request_head(request_head);
    }
    Ok(mutate(&mut exchange.request))
}

fn with_exchange_request<T>(
    context: &SharedProxyVmContext,
    handle: i64,
    read: impl FnOnce(&super::state::HttpOutboundRequestNode) -> T,
) -> Result<T, VmError> {
    let exchanges = context.lock_exchanges();
    let exchange = exchanges
        .exchanges
        .get(&handle)
        .ok_or_else(|| unknown_exchange_handle(handle))?;
    Ok(read(&exchange.request))
}

fn apply_header_batch(
    request: &mut super::state::HttpOutboundRequestNode,
    headers: Value,
) -> Result<(), VmError> {
    match headers {
        Value::Null => Ok(()),
        Value::Array(values) => {
            if let Some(parsed) = lookup_cached_header_batch(&Value::Array(values.clone())) {
                for (name, value) in &parsed {
                    request.insert_header(name.clone(), value.clone());
                }
                return Ok(());
            }
            if values.len() % 2 != 0 {
                return Err(VmError::HostError(
                    "header batch arrays must contain alternating name/value string pairs"
                        .to_string(),
                ));
            }
            let mut parsed = axum::http::HeaderMap::new();
            for pair in values.chunks(2) {
                let Value::String(name) = &pair[0] else {
                    return Err(VmError::HostError(
                        "header batch array keys must be strings".to_string(),
                    ));
                };
                let Value::String(value) = &pair[1] else {
                    return Err(VmError::HostError(
                        "header batch array values must be strings".to_string(),
                    ));
                };
                let (header_name, header_value) = parse_header(name.as_str(), value.as_str())?;
                parsed.insert(header_name, header_value);
            }
            store_cached_header_batch(&Value::Array(values.clone()), &parsed);
            for (name, value) in &parsed {
                request.insert_header(name.clone(), value.clone());
            }
            Ok(())
        }
        Value::Map(entries) => {
            if let Some(parsed) = lookup_cached_header_batch(&Value::Map(entries.clone())) {
                for (name, value) in &parsed {
                    request.headers.insert(name.clone(), value.clone());
                }
                return Ok(());
            }
            let mut parsed = axum::http::HeaderMap::new();
            for (key, value) in entries.as_ref() {
                let Value::String(name) = key else {
                    return Err(VmError::HostError(
                        "header batch map keys must be strings".to_string(),
                    ));
                };
                let Value::String(value) = value else {
                    return Err(VmError::HostError(
                        "header batch map values must be strings".to_string(),
                    ));
                };
                let (header_name, header_value) = parse_header(name.as_str(), value.as_str())?;
                parsed.insert(header_name, header_value);
            }
            store_cached_header_batch(&Value::Map(entries.clone()), &parsed);
            for (name, value) in &parsed {
                request.headers.insert(name.clone(), value.clone());
            }
            Ok(())
        }
        _ => Err(VmError::HostError(
            "header batch must be null, an array of alternating strings, or a string map"
                .to_string(),
        )),
    }
}

fn exchange_response_values_outcome<F>(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    extract: F,
) -> Result<CallOutcome, VmError>
where
    F: FnOnce(super::state::HttpUpstreamResponseSnapshot) -> Result<Vec<Value>, VmError>
        + Send
        + 'static,
{
    let snapshot = {
        let guard = context.lock_exchanges();
        let exchange_state = guard
            .exchanges
            .get(&exchange)
            .ok_or_else(|| unknown_exchange_handle(exchange))?;
        match &exchange_state.response {
            super::state::HttpUpstreamResponseNode::Ready(snapshot) => Some(snapshot.clone()),
            super::state::HttpUpstreamResponseNode::NotStarted => None,
        }
    };
    if let Some(snapshot) = snapshot {
        return Ok(CallOutcome::Return(extract(snapshot)?));
    }

    schedule_current_future_call(vm, async move {
        let snapshot = ensure_outbound_exchange_response_started(&context, exchange).await?;
        extract(snapshot)
    })
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

    with_exchange_request_mut(context, handle, |request| {
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

fn parse_upstream_port(port: i64) -> Result<u16, VmError> {
    u16::try_from(port)
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| VmError::HostError(format!("invalid upstream port '{port}'")))
}

pub(crate) fn prepare_default_upstream_request(
    context: &SharedProxyVmContext,
    host: String,
    port: i64,
    version: String,
    headers: Value,
) -> Result<(), VmError> {
    let version = parse_version_preference(&version)?;
    let port = parse_upstream_port(port)?;
    let mut exchanges = context.lock_exchanges();
    let exchange = exchanges
        .exchanges
        .get_mut(&default_upstream_exchange_handle())
        .ok_or_else(|| unknown_exchange_handle(default_upstream_exchange_handle()))?;
    let (target_scheme, target_host) = {
        let request = &mut exchange.request;
        request.set_target_host_port(&host, port)?;
        request.version_preference = version;
        apply_header_batch(request, headers)?;
        (request.target_scheme, request.target_host.clone())
    };
    let mut transport = context.lock_transport();
    transport.tcp_dag.default_upstream.configure();
    transport.tls_dag.default_upstream.observe_target_parts(
        matches!(target_scheme, HttpUpstreamScheme::Https),
        target_host,
    );
    Ok(())
}

/// Allocates an outbound HTTP exchange handle.
#[pd_edge_host_function(name = http_exchange::NEW.name, scope = http)]
fn new_exchange(context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    let handle = allocate_outbound_exchange_handle(&context)?;
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

/// Returns the default upstream handle for the outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::DEFAULT_UPSTREAM.name, scope = http)]
fn default_upstream_exchange(_context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::Int(
        default_upstream_exchange_handle(),
    )]))
}

/// Configures the inherited default upstream request target, version, and header batch.
#[pd_edge_host_function(name = http_exchange::PREPARE_DEFAULT_UPSTREAM.name, scope = http)]
fn prepare_default_upstream(
    context: SharedProxyVmContext,
    host: String,
    port: i64,
    version: String,
    headers: Value,
) -> Result<CallOutcome, VmError> {
    prepare_default_upstream_request(&context, host, port, version, headers)?;
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
fn set_exchange_header(
    context: SharedProxyVmContext,
    exchange: i64,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    with_exchange_request_mut(&context, exchange, |request| {
        request.headers.insert(header_name, header_value);
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the HTTP method on the outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::SET_METHOD.name, scope = http)]
fn set_exchange_method(
    context: SharedProxyVmContext,
    exchange: i64,
    method: String,
) -> Result<CallOutcome, VmError> {
    let parsed = Method::from_bytes(method.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid http method '{method}'")))?;
    with_exchange_request_mut(&context, exchange, |request| {
        request.method = parsed;
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the request path on the outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::SET_PATH.name, scope = http)]
fn set_exchange_path(
    context: SharedProxyVmContext,
    exchange: i64,
    path: String,
) -> Result<CallOutcome, VmError> {
    if !is_valid_request_path(&path) {
        return Err(VmError::HostError(format!(
            "path must start with '/' and must not contain whitespace, '?', or '#', got '{path}'",
        )));
    }
    with_exchange_request_mut(&context, exchange, |request| {
        request.path = path;
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the decoded query string on the outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::SET_QUERY.name, scope = http)]
fn set_exchange_query(
    context: SharedProxyVmContext,
    exchange: i64,
    query: String,
) -> Result<CallOutcome, VmError> {
    apply_exchange_query(&context, exchange, query)
}

/// Sets the preferred HTTP version for the outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::SET_VERSION.name, scope = http)]
fn set_exchange_version(
    context: SharedProxyVmContext,
    exchange: i64,
    version: String,
) -> Result<CallOutcome, VmError> {
    let parsed = parse_version_preference(&version)?;
    with_exchange_request_mut(&context, exchange, |request| {
        request.version_preference = parsed;
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Returns the configured HTTP version preference for the outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::GET_VERSION.name, scope = http)]
fn get_exchange_version(
    context: SharedProxyVmContext,
    exchange: i64,
) -> Result<CallOutcome, VmError> {
    let version = with_exchange_request(&context, exchange, |request| {
        request.version_preference.as_str().to_string()
    })?;
    Ok(CallOutcome::Return(vec![Value::string(version)]))
}

/// Sets the target endpoint for the outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::SET_TARGET.name, scope = http)]
fn set_exchange_target(
    context: SharedProxyVmContext,
    exchange: i64,
    host: String,
    port: i64,
) -> Result<CallOutcome, VmError> {
    let port = parse_upstream_port(port)?;
    let (target_scheme, target_host) = with_exchange_request_mut(&context, exchange, |request| {
        request.set_target_host_port(&host, port)?;
        Ok((request.target_scheme, request.target_host.clone()))
    })??;

    if exchange == default_upstream_exchange_handle() {
        let mut transport = context.lock_transport();
        transport.tcp_dag.default_upstream.configure();
        transport.tls_dag.default_upstream.observe_target_parts(
            matches!(target_scheme, HttpUpstreamScheme::Https),
            target_host,
        );
    } else {
        let mut exchanges = context.lock_exchanges();
        let exchange_state = exchanges
            .exchanges
            .get_mut(&exchange)
            .ok_or_else(|| unknown_exchange_handle(exchange))?;
        exchange_state.transport.tcp_flow.configure();
        exchange_state.transport.tls_flow.observe_target_parts(
            matches!(target_scheme, HttpUpstreamScheme::Https),
            target_host,
        );
    }
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the request scheme for the outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::SET_SCHEME.name, scope = http)]
fn set_exchange_scheme(
    context: SharedProxyVmContext,
    exchange: i64,
    scheme: String,
) -> Result<CallOutcome, VmError> {
    let scheme = HttpUpstreamScheme::parse(&scheme)?;
    let (target_scheme, target_host) = with_exchange_request_mut(&context, exchange, |request| {
        request.set_target_scheme(scheme)?;
        Ok((request.target_scheme, request.target_host.clone()))
    })??;

    if target_host.is_some() {
        if exchange == default_upstream_exchange_handle() {
            context
                .lock_transport()
                .tls_dag
                .default_upstream
                .observe_target_parts(
                    matches!(target_scheme, HttpUpstreamScheme::Https),
                    target_host,
                );
        } else {
            let mut exchanges = context.lock_exchanges();
            let exchange_state = exchanges
                .exchanges
                .get_mut(&exchange)
                .ok_or_else(|| unknown_exchange_handle(exchange))?;
            exchange_state.transport.tls_flow.observe_target_parts(
                matches!(target_scheme, HttpUpstreamScheme::Https),
                target_host,
            );
        }
    }

    Ok(CallOutcome::Return(vec![]))
}

/// Attaches a TCP stream as the transport for an outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::ATTACH_TCP.name, scope = http)]
fn attach_exchange_tcp(
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
fn attach_exchange_tls_plaintext(
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
fn set_exchange_body(
    context: SharedProxyVmContext,
    exchange: i64,
    body: String,
) -> Result<CallOutcome, VmError> {
    with_exchange_request_mut(&context, exchange, |request| {
        request.body_override = Some(body.into_bytes());
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Adds a header value to the outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::ADD_HEADER.name, scope = http)]
fn add_exchange_header(
    context: SharedProxyVmContext,
    exchange: i64,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    with_exchange_request_mut(&context, exchange, |request| {
        request.headers.append(header_name, header_value);
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Clears all values for a header on the outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::CLEAR_HEADER.name, scope = http)]
fn clear_exchange_header(
    context: SharedProxyVmContext,
    exchange: i64,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = parse_header_name(name)?;
    with_exchange_request_mut(&context, exchange, |request| {
        request.headers.remove(header_name);
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets a query parameter on the outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::SET_QUERY_ARG.name, scope = http)]
fn set_exchange_query_arg(
    context: SharedProxyVmContext,
    exchange: i64,
    key: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    with_exchange_request_mut(&context, exchange, |request| {
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
fn get_exchange_status(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
) -> Result<CallOutcome, VmError> {
    exchange_response_values_outcome(vm, context, exchange, |snapshot| {
        Ok(vec![Value::Int(snapshot.status as i64)])
    })
}

/// Returns the first value for a header on the outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::GET_HEADER.name, scope = http)]
fn get_exchange_header(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;
    exchange_response_values_outcome(vm, context, exchange, move |snapshot| {
        let value = snapshot
            .headers
            .get(&header_name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        Ok(vec![Value::string(value)])
    })
}

/// Returns all headers on the outbound HTTP exchange as a map.
#[pd_edge_host_function(name = http_exchange::GET_HEADERS.name, scope = http)]
fn get_exchange_headers(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
) -> Result<CallOutcome, VmError> {
    exchange_response_values_outcome(vm, context, exchange, |snapshot| {
        Ok(vec![headers_to_value_map(&snapshot.headers)])
    })
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

/// Returns the first trailer value for the outbound HTTP exchange.
#[pd_edge_host_function(name = "http::exchange::get_trailer", scope = http)]
async fn get_exchange_trailer(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid trailer name '{name}'")))?;
    let trailers = read_outbound_exchange_response_trailers(&context, exchange).await?;
    let value = trailers
        .get(&header_name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

/// Returns all trailers on the outbound HTTP exchange as a map.
#[pd_edge_host_function(name = "http::exchange::get_trailers", scope = http)]
async fn get_exchange_trailers(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
) -> Result<CallOutcome, VmError> {
    let trailers = read_outbound_exchange_response_trailers(&context, exchange).await?;
    Ok(CallOutcome::Return(vec![headers_to_value_map(&trailers)]))
}

/// Returns the HTTP version for the outbound HTTP exchange.
#[pd_edge_host_function(name = http_exchange::GET_HTTP_VERSION.name, scope = http)]
fn get_exchange_http_version(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
) -> Result<CallOutcome, VmError> {
    exchange_response_values_outcome(vm, context, exchange, |snapshot| {
        Ok(vec![Value::string(snapshot.http_version)])
    })
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

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, mpsc},
        time::Duration,
    };

    use axum::http::HeaderMap;

    use super::*;
    use crate::abi_impl::{ProxyVmContext, RateLimiterStore};

    fn test_context() -> SharedProxyVmContext {
        Arc::new(ProxyVmContext::from_request_headers(
            HeaderMap::new(),
            Arc::new(RateLimiterStore::new()),
        ))
    }

    #[test]
    fn default_upstream_request_only_mutation_does_not_wait_for_transport_lock() {
        let context = test_context();
        let transport_guard = context.lock_transport();
        let thread_context = context.clone();
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let completed = set_exchange_header_impl(
                thread_context,
                default_upstream_exchange_handle(),
                "x-test".to_string(),
                "value".to_string(),
            )
            .is_ok();
            tx.send(completed)
                .expect("request-only exchange mutation should report completion");
        });

        let completed = rx.recv_timeout(Duration::from_millis(100));
        drop(transport_guard);
        handle
            .join()
            .expect("request-only exchange mutation thread should join");

        assert!(
            completed.expect(
                "request-only default upstream mutation should complete without waiting for transport"
            ),
            "request-only default upstream mutation should succeed"
        );
        assert_eq!(
            context.with_default_upstream_exchange(|exchange| {
                exchange
                    .request
                    .headers
                    .get("x-test")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string)
            }),
            Some("value".to_string())
        );
    }

    #[test]
    fn setting_default_upstream_target_updates_transport_state_in_place() {
        let context = test_context();
        set_exchange_scheme_impl(
            context.clone(),
            default_upstream_exchange_handle(),
            "https".to_string(),
        )
        .expect("setting default upstream scheme should succeed");

        set_exchange_target_impl(
            context.clone(),
            default_upstream_exchange_handle(),
            "origin.example.com".to_string(),
            443,
        )
        .expect("setting default upstream target should succeed");

        assert_eq!(
            context.with_default_upstream_exchange(|exchange| exchange.request.target.clone()),
            Some("https://origin.example.com:443".to_string())
        );
        let transport = context.lock_transport();
        assert!(transport.tcp_dag.default_upstream.is_configured());
        assert!(transport.tls_dag.default_upstream.is_present());
        assert_eq!(
            transport.tls_dag.default_upstream.peer_name(),
            "origin.example.com"
        );
    }

    #[test]
    fn setting_default_upstream_scheme_rewrites_existing_target() {
        let context = test_context();

        set_exchange_target_impl(
            context.clone(),
            default_upstream_exchange_handle(),
            "origin.example.com".to_string(),
            8080,
        )
        .expect("setting default upstream target should succeed");
        set_exchange_scheme_impl(
            context.clone(),
            default_upstream_exchange_handle(),
            "https".to_string(),
        )
        .expect("setting default upstream scheme should succeed");

        assert_eq!(
            context.with_default_upstream_exchange(|exchange| exchange.request.target.clone()),
            Some("https://origin.example.com:8080".to_string())
        );
    }

    #[test]
    fn prepare_default_upstream_batches_target_version_and_headers() {
        let context = test_context();
        let headers = Value::array(vec![
            Value::string("x-first"),
            Value::string("one"),
            Value::string("x-second"),
            Value::string("two"),
        ]);

        let outcome = prepare_default_upstream_impl(
            context.clone(),
            "origin.example.com".to_string(),
            8080,
            "2".to_string(),
            headers,
        )
        .expect("batched default upstream prepare should succeed");

        assert_eq!(outcome, CallOutcome::Return(vec![Value::Int(1)]));
        assert_eq!(
            context.with_default_upstream_exchange(|exchange| exchange.request.target.clone()),
            Some("http://origin.example.com:8080".to_string())
        );
        assert_eq!(
            context.with_default_upstream_exchange(|exchange| {
                exchange.request.version_preference.as_str().to_string()
            }),
            "2".to_string()
        );
        assert_eq!(
            context.with_default_upstream_exchange(|exchange| {
                let mut request = exchange.request.clone();
                request
                    .materialize_inherited_request_head(&context.with_request_head(Clone::clone));
                request
                    .headers
                    .get("x-first")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string)
            }),
            Some("one".to_string())
        );
        assert_eq!(
            context.with_default_upstream_exchange(|exchange| {
                let mut request = exchange.request.clone();
                request
                    .materialize_inherited_request_head(&context.with_request_head(Clone::clone));
                request
                    .headers
                    .get("x-second")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string)
            }),
            Some("two".to_string())
        );
    }
}
