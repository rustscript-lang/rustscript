use axum::http::{
    HeaderName, HeaderValue,
    header::{CONTENT_LENGTH, HOST},
};
use edge_abi::symbols::websocket;
use pd_edge_host_function::pd_edge_host_function;
use tokio_tungstenite::{connect_async, tungstenite::client::IntoClientRequest};
use url::Url;
use vm::{CallOutcome, Value, Vm, VmError};

use super::super::SharedProxyVmContext;
use super::super::http::{
    HttpOutboundRequestNode, allocate_outbound_exchange_handle, build_upstream_url,
    default_upstream_exchange_handle, is_hop_by_hop_header, outbound_exchange_exists,
    outbound_exchange_response_available,
};
use super::state::{OutboundWebSocketIoState, SharedWebSocketIo, WebSocketConnectionState};

const DOWNSTREAM_CONNECTION_HANDLE: i64 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WebSocketHandle {
    Downstream,
    DefaultUpstream,
    OutboundExchange(i64),
}

#[derive(Clone)]
struct PreparedOutboundWebSocket {
    url: String,
    host_header: Option<String>,
    headers: axum::http::HeaderMap,
    tls_present: bool,
    tls_requires_custom_client: bool,
    requested_subprotocols: Vec<String>,
}

fn decode_connection(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<WebSocketHandle, VmError> {
    if connection == DOWNSTREAM_CONNECTION_HANDLE {
        return Ok(WebSocketHandle::Downstream);
    }
    if connection == default_upstream_exchange_handle() {
        return Ok(WebSocketHandle::DefaultUpstream);
    }
    if outbound_exchange_exists(context, connection) {
        return Ok(WebSocketHandle::OutboundExchange(connection));
    }
    Err(VmError::HostError(format!(
        "invalid websocket connection handle {connection}; reserved handles are 0 (downstream), 1 (default upstream), and allocated handles start at 2",
    )))
}

fn parse_header(name: String, value: String) -> Result<(HeaderName, HeaderValue), VmError> {
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;
    let header_value = HeaderValue::from_str(&value)
        .map_err(|_| VmError::HostError(format!("invalid header value '{value}'")))?;
    Ok((header_name, header_value))
}

fn parse_subprotocols(raw: &str) -> Result<Vec<String>, VmError> {
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let protocols = raw
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if protocols.is_empty() {
        return Err(VmError::HostError(
            "websocket::connection::set_subprotocols requires at least one non-empty protocol"
                .to_string(),
        ));
    }
    Ok(protocols)
}

fn is_valid_websocket_target(value: &str) -> bool {
    if value.is_empty() || value.chars().any(|ch| ch.is_whitespace()) {
        return false;
    }
    if let Ok(url) = Url::parse(value) {
        return matches!(url.scheme(), "http" | "https" | "ws" | "wss")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none();
    }
    let Some((host, port)) = value.rsplit_once(':') else {
        return false;
    };
    if host.is_empty() || port.is_empty() || host.contains(':') {
        return false;
    }
    port.parse::<u16>().ok().is_some_and(|port| port != 0)
}

fn build_websocket_url(
    target: &str,
    path: &str,
    query: &str,
) -> Result<(String, Option<String>), VmError> {
    let (url, host_header) = build_upstream_url(target, path, query);
    let mut parsed = Url::parse(&url).map_err(|err| {
        VmError::HostError(format!(
            "failed to build websocket target URL from '{target}': {err}"
        ))
    })?;
    match parsed.scheme() {
        "http" => {
            parsed
                .set_scheme("ws")
                .map_err(|_| VmError::HostError("failed to convert http URL to ws".to_string()))?;
        }
        "https" => {
            parsed.set_scheme("wss").map_err(|_| {
                VmError::HostError("failed to convert https URL to wss".to_string())
            })?;
        }
        "ws" | "wss" => {}
        scheme => {
            return Err(VmError::HostError(format!(
                "unsupported websocket target scheme '{scheme}'; expected ws, wss, http, or https",
            )));
        }
    }
    Ok((parsed.to_string(), host_header))
}

fn with_outbound_connection_mut<T>(
    context: &SharedProxyVmContext,
    connection: i64,
    mutate: impl FnOnce(
        &mut HttpOutboundRequestNode,
        &mut super::super::transport::TcpFlowState,
        &mut super::super::transport::TlsFlowState,
        &mut WebSocketConnectionState,
    ) -> T,
) -> Result<T, VmError> {
    let mut guard = context.lock().expect("vm context lock poisoned");
    if connection == default_upstream_exchange_handle() {
        let crate::abi_impl::ProxyVmContext {
            outbound_request,
            tcp_dag,
            tls_dag,
            default_upstream_websocket,
            ..
        } = &mut *guard;
        return Ok(mutate(
            outbound_request,
            &mut tcp_dag.default_upstream,
            &mut tls_dag.default_upstream,
            default_upstream_websocket,
        ));
    }

    let exchange = guard
        .outbound_exchanges
        .get_mut(&connection)
        .ok_or_else(|| VmError::HostError(format!("unknown websocket handle {connection}")))?;
    Ok(mutate(
        &mut exchange.request,
        &mut exchange.tcp_dag,
        &mut exchange.tls_dag,
        &mut exchange.websocket_dag,
    ))
}

fn connection_state(
    context: &SharedProxyVmContext,
    connection: WebSocketHandle,
) -> WebSocketConnectionState {
    let guard = context.lock().expect("vm context lock poisoned");
    match connection {
        WebSocketHandle::Downstream => guard.downstream_websocket.clone(),
        WebSocketHandle::DefaultUpstream => guard.default_upstream_websocket.clone(),
        WebSocketHandle::OutboundExchange(handle) => guard
            .outbound_exchanges
            .get(&handle)
            .expect("exchange handle should exist while websocket is in use")
            .websocket_dag
            .clone(),
    }
}

fn prepare_outbound_socket_target(
    context: &SharedProxyVmContext,
    connection: i64,
    target: String,
) -> Result<(), VmError> {
    with_outbound_connection_mut(
        context,
        connection,
        |request, tcp_flow, tls_flow, websocket| {
            request.target = Some(target.clone());
            tcp_flow.configure();
            tls_flow.observe_target(&target);
            websocket.prepare_outbound();
        },
    )?;
    Ok(())
}

fn prepare_outbound_header(
    context: &SharedProxyVmContext,
    connection: i64,
    name: String,
    value: String,
) -> Result<(), VmError> {
    let (header_name, header_value) = parse_header(name, value)?;
    with_outbound_connection_mut(
        context,
        connection,
        |request, _tcp_flow, _tls_flow, websocket| {
            request.headers.insert(header_name, header_value);
            websocket.prepare_outbound();
        },
    )?;
    Ok(())
}

fn prepared_outbound_websocket(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<PreparedOutboundWebSocket, VmError> {
    let guard = context.lock().expect("vm context lock poisoned");
    let (request, tls_flow, websocket) = if connection == default_upstream_exchange_handle() {
        (
            guard.outbound_request.clone(),
            guard.tls_dag.default_upstream.clone(),
            guard.default_upstream_websocket.clone(),
        )
    } else {
        let exchange = guard
            .outbound_exchanges
            .get(&connection)
            .ok_or_else(|| VmError::HostError(format!("unknown websocket handle {connection}")))?;
        (
            exchange.request.clone(),
            exchange.tls_dag.clone(),
            exchange.websocket_dag.clone(),
        )
    };

    let target = request.target.ok_or_else(|| {
        VmError::HostError(
            "websocket target is unavailable before websocket::connection::set_target".to_string(),
        )
    })?;
    let (url, host_header) = build_websocket_url(&target, &request.path, &request.query)?;
    Ok(PreparedOutboundWebSocket {
        url,
        host_header,
        headers: request.headers,
        tls_present: tls_flow.is_present(),
        tls_requires_custom_client: tls_flow.requires_custom_client(),
        requested_subprotocols: websocket.requested_subprotocols().to_vec(),
    })
}

fn store_connected_websocket(
    context: &SharedProxyVmContext,
    connection: i64,
    io: SharedWebSocketIo,
    negotiated_subprotocol: Option<String>,
) -> Result<(), VmError> {
    let mut guard = context.lock().expect("vm context lock poisoned");
    if connection == default_upstream_exchange_handle() {
        guard.tcp_dag.default_upstream.mark_connected();
        if guard.tls_dag.default_upstream.is_present() {
            guard.tls_dag.default_upstream.note_handshake_prepared();
            guard.tls_dag.default_upstream.note_client_hello_sent();
            guard.tls_dag.default_upstream.note_server_hello_received();
            guard
                .tls_dag
                .default_upstream
                .note_server_certificate_received(None);
            guard
                .tls_dag
                .default_upstream
                .note_server_certificate_verified();
            guard
                .tls_dag
                .default_upstream
                .mark_handshake_complete(Some("http/1.1".to_string()));
        }
        guard
            .default_upstream_websocket
            .mark_open(io, negotiated_subprotocol);
        return Ok(());
    }

    let exchange = guard
        .outbound_exchanges
        .get_mut(&connection)
        .ok_or_else(|| VmError::HostError(format!("unknown websocket handle {connection}")))?;
    exchange.tcp_dag.mark_connected();
    if exchange.tls_dag.is_present() {
        exchange.tls_dag.note_handshake_prepared();
        exchange.tls_dag.note_client_hello_sent();
        exchange.tls_dag.note_server_hello_received();
        exchange.tls_dag.note_server_certificate_received(None);
        exchange.tls_dag.note_server_certificate_verified();
        exchange
            .tls_dag
            .mark_handshake_complete(Some("http/1.1".to_string()));
    }
    exchange.websocket_dag.mark_open(io, negotiated_subprotocol);
    Ok(())
}

fn store_failed_websocket(
    context: &SharedProxyVmContext,
    connection: i64,
    message: String,
) -> Result<(), VmError> {
    let mut guard = context.lock().expect("vm context lock poisoned");
    if connection == default_upstream_exchange_handle() {
        if guard.tls_dag.default_upstream.is_present() {
            guard.tls_dag.default_upstream.mark_failed();
        }
        guard.default_upstream_websocket.mark_failed(message);
        return Ok(());
    }
    let exchange = guard
        .outbound_exchanges
        .get_mut(&connection)
        .ok_or_else(|| VmError::HostError(format!("unknown websocket handle {connection}")))?;
    if exchange.tls_dag.is_present() {
        exchange.tls_dag.mark_failed();
    }
    exchange.websocket_dag.mark_failed(message);
    Ok(())
}

fn note_websocket_handshake_started(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<(), VmError> {
    with_outbound_connection_mut(
        context,
        connection,
        |_request, _tcp_flow, _tls_flow, websocket| {
            websocket.note_handshake_started();
        },
    )?;
    Ok(())
}

fn websocket_io(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<SharedWebSocketIo, VmError> {
    let state = connection_state(context, decode_connection(context, connection)?);
    state.io().ok_or_else(|| {
        VmError::HostError(format!(
            "websocket connection handle {connection} is not open",
        ))
    })
}

fn websocket_operation_on_downstream() -> VmError {
    VmError::HostError(
        "downstream websocket sessions are not yet executable in the current one-shot HTTP runtime; only outbound websocket handles support frame IO".to_string(),
    )
}

pub(crate) fn websocket_connection_mode(context: &SharedProxyVmContext, connection: i64) -> bool {
    let Ok(handle) = decode_connection(context, connection) else {
        return false;
    };
    connection_state(context, handle).is_websocket_mode()
}

fn refresh_connection_close_state(context: &SharedProxyVmContext, connection: i64) {
    let mut state = context.lock().expect("vm context lock poisoned");
    match connection {
        1 => state.default_upstream_websocket.refresh_close_state(),
        0 => state.downstream_websocket.refresh_close_state(),
        handle => {
            if let Some(exchange) = state.outbound_exchanges.get_mut(&handle) {
                exchange.websocket_dag.refresh_close_state();
            }
        }
    }
}

pub(crate) async fn ensure_outbound_websocket_connection_open(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<(), VmError> {
    if outbound_exchange_response_available(context, connection) {
        return Err(VmError::HostError(format!(
            "websocket connection handle {connection} cannot enter websocket mode after the HTTP response has started",
        )));
    }
    match decode_connection(context, connection)? {
        WebSocketHandle::Downstream => return Err(websocket_operation_on_downstream()),
        handle @ (WebSocketHandle::DefaultUpstream | WebSocketHandle::OutboundExchange(_)) => {
            let state = connection_state(context, handle);
            if state.is_open() {
                return Ok(());
            }
        }
    }

    let prepared = prepared_outbound_websocket(context, connection)?;
    if prepared.tls_present && prepared.tls_requires_custom_client {
        let message = format!(
            "websocket connection handle {connection} cannot apply custom TLS session settings yet; websocket TLS currently supports only the default verifier/client configuration",
        );
        let _ = store_failed_websocket(context, connection, message.clone());
        return Err(VmError::HostError(message));
    }

    note_websocket_handshake_started(context, connection)?;

    let mut request = prepared.url.clone().into_client_request().map_err(|err| {
        let message = format!("failed to create websocket handshake request: {err}");
        let _ = store_failed_websocket(context, connection, message.clone());
        VmError::HostError(message)
    })?;
    if let Some(host) = prepared.host_header {
        let value = HeaderValue::from_str(&host)
            .map_err(|_| VmError::HostError(format!("invalid host header '{host}'")))?;
        request.headers_mut().insert(HOST, value);
    }
    for (name, value) in &prepared.headers {
        if name == HOST || name == CONTENT_LENGTH {
            continue;
        }
        if is_hop_by_hop_header(name)
            || matches!(
                name.as_str(),
                "sec-websocket-key" | "sec-websocket-version" | "sec-websocket-extensions"
            )
        {
            continue;
        }
        request.headers_mut().insert(name.clone(), value.clone());
    }
    if !prepared.requested_subprotocols.is_empty()
        && !request.headers().contains_key("sec-websocket-protocol")
    {
        let raw = prepared.requested_subprotocols.join(", ");
        let value = HeaderValue::from_str(&raw).map_err(|_| {
            VmError::HostError(format!("invalid websocket subprotocol list '{raw}'"))
        })?;
        request
            .headers_mut()
            .insert("sec-websocket-protocol", value);
    }

    let (stream, response) = connect_async(request).await.map_err(|err| {
        let message = format!("websocket connection handle {connection} failed to connect: {err}");
        let _ = store_failed_websocket(context, connection, message.clone());
        VmError::HostError(message)
    })?;
    let negotiated_subprotocol = response
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let io = std::sync::Arc::new(tokio::sync::Mutex::new(OutboundWebSocketIoState::new(
        stream,
    )));
    store_connected_websocket(context, connection, io, negotiated_subprotocol)?;
    Ok(())
}

pub(crate) fn validate_outbound_websocket_binary_connection(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<(), VmError> {
    match decode_connection(context, connection)? {
        WebSocketHandle::Downstream => Err(websocket_operation_on_downstream()),
        WebSocketHandle::DefaultUpstream | WebSocketHandle::OutboundExchange(_) => Ok(()),
    }
}

pub(crate) async fn write_websocket_binary_bytes(
    context: &SharedProxyVmContext,
    connection: i64,
    payload: &[u8],
) -> Result<usize, VmError> {
    validate_outbound_websocket_binary_connection(context, connection)?;
    ensure_outbound_websocket_connection_open(context, connection).await?;
    let io = websocket_io(context, connection)?;
    let mut io = io.lock().await;
    io.send_binary_bytes(payload).await
}

pub(crate) async fn read_websocket_binary_bytes(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<Option<Vec<u8>>, VmError> {
    validate_outbound_websocket_binary_connection(context, connection)?;
    ensure_outbound_websocket_connection_open(context, connection).await?;
    let io = websocket_io(context, connection)?;
    let mut io = io.lock().await;
    let payload = io.read_binary_bytes().await?;
    drop(io);
    refresh_connection_close_state(context, connection);
    Ok(payload)
}

pub(crate) async fn websocket_binary_eof(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<bool, VmError> {
    validate_outbound_websocket_binary_connection(context, connection)?;
    let mut state = connection_state(context, decode_connection(context, connection)?);
    Ok(state.eof())
}

pub(crate) async fn close_websocket_binary_stream(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<(), VmError> {
    if websocket_binary_eof(context, connection).await? {
        return Ok(());
    }
    ensure_outbound_websocket_connection_open(context, connection).await?;
    {
        let mut state = context.lock().expect("vm context lock poisoned");
        match connection {
            1 => state.default_upstream_websocket.note_closing(),
            0 => state.downstream_websocket.note_closing(),
            handle => {
                if let Some(exchange) = state.outbound_exchanges.get_mut(&handle) {
                    exchange.websocket_dag.note_closing();
                }
            }
        }
    }
    let io = websocket_io(context, connection)?;
    let mut io = io.lock().await;
    io.close(1000, "proxy-write-complete".to_string()).await?;
    let close_reason = io.close_reason().map(str::to_string);
    let close_code = io.close_code();
    drop(io);
    let mut state = context.lock().expect("vm context lock poisoned");
    match connection {
        1 => state
            .default_upstream_websocket
            .mark_closed(close_code, close_reason),
        0 => state
            .downstream_websocket
            .mark_closed(close_code, close_reason),
        handle => {
            if let Some(exchange) = state.outbound_exchanges.get_mut(&handle) {
                exchange.websocket_dag.mark_closed(close_code, close_reason);
            }
        }
    }
    Ok(())
}

#[pd_edge_host_function(name = websocket::connection::NEW.name, scope = websocket)]
async fn connection_new(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let handle = allocate_outbound_exchange_handle(&context)?;
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

#[pd_edge_host_function(name = websocket::connection::DOWNSTREAM.name, scope = websocket)]
async fn connection_downstream(
    _vm: &mut Vm,
    _context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::Int(
        DOWNSTREAM_CONNECTION_HANDLE,
    )]))
}

#[pd_edge_host_function(name = websocket::connection::DEFAULT_UPSTREAM.name, scope = websocket)]
async fn connection_default_upstream(
    _vm: &mut Vm,
    _context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::Int(
        default_upstream_exchange_handle(),
    )]))
}

#[pd_edge_host_function(name = websocket::connection::IS_PRESENT.name, scope = websocket)]
async fn connection_is_present(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    let state = connection_state(&context, decode_connection(&context, connection)?);
    Ok(CallOutcome::Return(vec![Value::Bool(state.is_present())]))
}

#[pd_edge_host_function(name = websocket::connection::SET_TARGET.name, scope = websocket)]
async fn connection_set_target(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    target: String,
) -> Result<CallOutcome, VmError> {
    if !is_valid_websocket_target(&target) {
        return Err(VmError::HostError(format!(
            "websocket target must be host:port or http(s)/ws(s) URL, got '{target}'",
        )));
    }
    match decode_connection(&context, connection)? {
        WebSocketHandle::Downstream => return Err(websocket_operation_on_downstream()),
        WebSocketHandle::DefaultUpstream | WebSocketHandle::OutboundExchange(_) => {
            prepare_outbound_socket_target(&context, connection, target)?;
        }
    }
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = websocket::connection::SET_HEADER.name, scope = websocket)]
async fn connection_set_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    name: String,
    value: String,
) -> Result<CallOutcome, VmError> {
    match decode_connection(&context, connection)? {
        WebSocketHandle::Downstream => return Err(websocket_operation_on_downstream()),
        WebSocketHandle::DefaultUpstream | WebSocketHandle::OutboundExchange(_) => {
            prepare_outbound_header(&context, connection, name, value)?;
        }
    }
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = websocket::connection::SET_SUBPROTOCOLS.name, scope = websocket)]
async fn connection_set_subprotocols(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    protocols: String,
) -> Result<CallOutcome, VmError> {
    let protocols = parse_subprotocols(&protocols)?;
    match decode_connection(&context, connection)? {
        WebSocketHandle::Downstream => return Err(websocket_operation_on_downstream()),
        WebSocketHandle::DefaultUpstream | WebSocketHandle::OutboundExchange(_) => {
            with_outbound_connection_mut(
                &context,
                connection,
                |_request, _tcp_flow, _tls_flow, websocket| {
                    websocket.set_requested_subprotocols(protocols);
                },
            )?;
        }
    }
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = websocket::connection::CONNECT.name, scope = websocket)]
async fn connection_connect(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    ensure_outbound_websocket_connection_open(&context, connection).await?;
    Ok(CallOutcome::Return(vec![Value::Bool(true)]))
}

#[pd_edge_host_function(name = websocket::connection::GET_PHASE.name, scope = websocket)]
async fn connection_get_phase(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    let mut state = connection_state(&context, decode_connection(&context, connection)?);
    state.refresh_close_state();
    Ok(CallOutcome::Return(vec![Value::string(
        state.phase().as_str(),
    )]))
}

#[pd_edge_host_function(name = websocket::connection::GET_SUBPROTOCOL.name, scope = websocket)]
async fn connection_get_subprotocol(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    let state = connection_state(&context, decode_connection(&context, connection)?);
    Ok(CallOutcome::Return(vec![Value::string(
        state.negotiated_subprotocol(),
    )]))
}

#[pd_edge_host_function(name = websocket::connection::SEND_TEXT.name, scope = websocket)]
async fn connection_send_text(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    text: String,
) -> Result<CallOutcome, VmError> {
    ensure_outbound_websocket_connection_open(&context, connection).await?;
    let io = websocket_io(&context, connection)?;
    let mut io = io.lock().await;
    let sent = io.send_text(text).await?;
    Ok(CallOutcome::Return(vec![Value::Int(sent as i64)]))
}

#[pd_edge_host_function(name = websocket::connection::READ_TEXT.name, scope = websocket)]
async fn connection_read_text(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    ensure_outbound_websocket_connection_open(&context, connection).await?;
    let io = websocket_io(&context, connection)?;
    let mut io = io.lock().await;
    let text = io.read_text().await?.unwrap_or_default();
    drop(io);
    refresh_connection_close_state(&context, connection);
    Ok(CallOutcome::Return(vec![Value::string(text)]))
}

#[pd_edge_host_function(
    name = websocket::connection::SEND_BINARY_BASE64.name,
    scope = websocket
)]
async fn connection_send_binary_base64(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    payload: String,
) -> Result<CallOutcome, VmError> {
    ensure_outbound_websocket_connection_open(&context, connection).await?;
    let io = websocket_io(&context, connection)?;
    let mut io = io.lock().await;
    let sent = io.send_binary_base64(payload).await?;
    Ok(CallOutcome::Return(vec![Value::Int(sent as i64)]))
}

#[pd_edge_host_function(
    name = websocket::connection::READ_BINARY_BASE64.name,
    scope = websocket
)]
async fn connection_read_binary_base64(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    ensure_outbound_websocket_connection_open(&context, connection).await?;
    let io = websocket_io(&context, connection)?;
    let mut io = io.lock().await;
    let payload = io.read_binary_base64().await?.unwrap_or_default();
    drop(io);
    refresh_connection_close_state(&context, connection);
    Ok(CallOutcome::Return(vec![Value::string(payload)]))
}

#[pd_edge_host_function(name = websocket::connection::EOF.name, scope = websocket)]
async fn connection_eof(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    let mut state = connection_state(&context, decode_connection(&context, connection)?);
    let eof = state.eof();
    Ok(CallOutcome::Return(vec![Value::Bool(eof)]))
}

#[pd_edge_host_function(name = websocket::connection::CLOSE.name, scope = websocket)]
async fn connection_close(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    code: i64,
    reason: String,
) -> Result<CallOutcome, VmError> {
    ensure_outbound_websocket_connection_open(&context, connection).await?;
    let close_code = u16::try_from(code).map_err(|_| {
        VmError::HostError(format!(
            "websocket close code must be in the u16 range, got {code}",
        ))
    })?;
    let io = websocket_io(&context, connection)?;
    {
        let mut state = context.lock().expect("vm context lock poisoned");
        match connection {
            1 => state.default_upstream_websocket.note_closing(),
            0 => state.downstream_websocket.note_closing(),
            handle => {
                if let Some(exchange) = state.outbound_exchanges.get_mut(&handle) {
                    exchange.websocket_dag.note_closing();
                }
            }
        }
    }
    let mut io = io.lock().await;
    io.close(close_code, reason.clone()).await?;
    let close_reason = io.close_reason().map(str::to_string);
    let close_code = io.close_code();
    drop(io);
    let mut state = context.lock().expect("vm context lock poisoned");
    match connection {
        1 => state
            .default_upstream_websocket
            .mark_closed(close_code, close_reason),
        0 => state
            .downstream_websocket
            .mark_closed(close_code, close_reason),
        handle => {
            if let Some(exchange) = state.outbound_exchanges.get_mut(&handle) {
                exchange.websocket_dag.mark_closed(close_code, close_reason);
            }
        }
    }
    Ok(CallOutcome::Return(vec![]))
}
