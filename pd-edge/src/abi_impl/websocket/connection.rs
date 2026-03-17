use std::sync::{Arc, Mutex};

use axum::http::{
    HeaderName, HeaderValue,
    header::{CONTENT_LENGTH, HOST},
    uri::Authority,
};
use edge_abi::symbols::websocket;
use pd_edge_host_function::pd_edge_host_function;
use tokio_tungstenite::{
    accept_hdr_async, connect_async,
    tungstenite::{
        client::IntoClientRequest,
        handshake::server::{Request as WsRequest, Response as WsResponse},
        http::HeaderValue as WsHeaderValue,
    },
};
use vm::{CallOutcome, Value, Vm, VmError};

use super::super::SharedProxyVmContext;
use super::super::http::{
    HttpOutboundRequestNode, allocate_outbound_exchange_handle, default_upstream_exchange_handle,
    is_hop_by_hop_header, outbound_exchange_exists, outbound_exchange_response_available,
};
use super::state::{
    OutboundWebSocketIoState, SharedWebSocketIo, WebSocketConnectionState,
    WebSocketUpstreamScheme,
};
use crate::abi_impl::transport::{DownstreamReplayTcpStream, ReplayPrefixedIo};

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

fn format_websocket_authority(host: &str, port: i64) -> Result<(String, u16), VmError> {
    if host.is_empty() || host.chars().any(|ch| ch.is_whitespace()) {
        return Err(VmError::HostError(format!(
            "websocket target host must be non-empty and contain no whitespace, got '{host}'",
        )));
    }
    let port = u16::try_from(port).map_err(|_| {
        VmError::HostError(format!(
            "websocket target port must be between 1 and 65535, got {port}",
        ))
    })?;
    if port == 0 {
        return Err(VmError::HostError(format!(
            "websocket target port must be between 1 and 65535, got {port}",
        )));
    }
    let bare_host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    let authority = if bare_host.contains(':') {
        format!("[{bare_host}]:{port}")
    } else {
        format!("{bare_host}:{port}")
    };
    Authority::from_maybe_shared(authority)
        .map_err(|_| {
            VmError::HostError(format!(
                "invalid websocket target host='{host}' port={port}",
            ))
        })
        .map(|_| (bare_host.to_string(), port))
}

fn websocket_host_header(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn normalize_websocket_path(path: &str) -> Result<String, VmError> {
    let path = if path.is_empty() { "/" } else { path };
    if !path.starts_with('/') || path.chars().any(|ch| ch.is_whitespace()) {
        return Err(VmError::HostError(format!(
            "websocket path must start with '/' and contain no whitespace, got '{path}'",
        )));
    }
    Ok(path.to_string())
}

fn normalize_websocket_query(query: &str) -> Result<String, VmError> {
    if query.chars().any(|ch| ch.is_whitespace()) || query.contains('#') {
        return Err(VmError::HostError(format!(
            "websocket query must contain no whitespace or fragments, got '{query}'",
        )));
    }
    Ok(query.trim_start_matches('?').to_string())
}

fn build_websocket_url(
    scheme: WebSocketUpstreamScheme,
    host: &str,
    port: u16,
    path: &str,
    query: &str,
) -> Result<(String, Option<String>), VmError> {
    let path = normalize_websocket_path(path)?;
    let query = normalize_websocket_query(query)?;
    let authority = websocket_host_header(host, port);
    let url = if query.is_empty() {
        format!("{}://{authority}{path}", scheme.as_str())
    } else {
        format!("{}://{authority}{path}?{query}", scheme.as_str())
    };
    Ok((url, Some(authority)))
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
    if connection == default_upstream_exchange_handle() {
        let mut exchanges = context.lock_exchanges();
        let exchange = exchanges
            .exchanges
            .get_mut(&connection)
            .ok_or_else(|| VmError::HostError(format!("unknown websocket handle {connection}")))?;
        let mut transport = context.lock_transport();
        let mut tcp_flow = transport.tcp_dag.default_upstream.clone();
        let mut tls_flow = transport.tls_dag.default_upstream.clone();
        let result = mutate(
            &mut exchange.request,
            &mut tcp_flow,
            &mut tls_flow,
            &mut exchange.websocket_dag,
        );
        transport.tcp_dag.default_upstream = tcp_flow;
        transport.tls_dag.default_upstream = tls_flow;
        return Ok(result);
    }

    let mut exchanges = context.lock_exchanges();
    let exchange = exchanges
        .exchanges
        .get_mut(&connection)
        .ok_or_else(|| VmError::HostError(format!("unknown websocket handle {connection}")))?;
    Ok(mutate(
        &mut exchange.request,
        &mut exchange.transport.tcp_flow,
        &mut exchange.transport.tls_flow,
        &mut exchange.websocket_dag,
    ))
}

fn connection_state(
    context: &SharedProxyVmContext,
    connection: WebSocketHandle,
) -> WebSocketConnectionState {
    match connection {
        WebSocketHandle::Downstream => context.downstream_websocket(),
        WebSocketHandle::DefaultUpstream => {
            context.with_default_upstream_exchange(|exchange| exchange.websocket_dag.clone())
        }
        WebSocketHandle::OutboundExchange(handle) => context
            .lock_exchanges()
            .exchanges
            .get(&handle)
            .expect("exchange handle should exist while websocket is in use")
            .websocket_dag
            .clone(),
    }
}

fn prepare_outbound_socket_target(
    context: &SharedProxyVmContext,
    connection: i64,
    host: String,
    port: i64,
) -> Result<(), VmError> {
    let (host, port) = format_websocket_authority(&host, port)?;
    with_outbound_connection_mut(
        context,
        connection,
        |_request, tcp_flow, tls_flow, websocket| {
            websocket.set_target_host_port(host.clone(), port);
            tcp_flow.configure();
            tls_flow.observe_target_parts(
                websocket.target_scheme().uses_tls(),
                Some(host.clone()),
            );
        },
    )?;
    Ok(())
}

fn prepare_outbound_scheme(
    context: &SharedProxyVmContext,
    connection: i64,
    scheme: String,
) -> Result<(), VmError> {
    let scheme = WebSocketUpstreamScheme::parse(&scheme)?;
    with_outbound_connection_mut(context, connection, |_request, _tcp_flow, tls_flow, websocket| {
        websocket.set_target_scheme(scheme);
        let peer_name = websocket.target_host().map(str::to_string);
        tls_flow.observe_target_parts(scheme.uses_tls(), peer_name);
    })?;
    Ok(())
}

fn prepare_outbound_path(
    context: &SharedProxyVmContext,
    connection: i64,
    path: String,
) -> Result<(), VmError> {
    let path = normalize_websocket_path(&path)?;
    with_outbound_connection_mut(context, connection, |request, _tcp_flow, _tls_flow, websocket| {
        request.path = path;
        websocket.prepare_outbound();
    })?;
    Ok(())
}

fn prepare_outbound_query(
    context: &SharedProxyVmContext,
    connection: i64,
    query: String,
) -> Result<(), VmError> {
    let query = normalize_websocket_query(&query)?;
    with_outbound_connection_mut(context, connection, |request, _tcp_flow, _tls_flow, websocket| {
        request.query = query;
        websocket.prepare_outbound();
    })?;
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
    let (request, tls_flow, websocket) = if connection == default_upstream_exchange_handle() {
        let request = {
            let exchanges = context.lock_exchanges();
            let exchange = exchanges.exchanges.get(&connection).ok_or_else(|| {
                VmError::HostError(format!("unknown websocket handle {connection}"))
            })?;
            (exchange.request.clone(), exchange.websocket_dag.clone())
        };
        let tls_flow = context.lock_transport().tls_dag.default_upstream.clone();
        (request.0, tls_flow, request.1)
    } else {
        let exchanges = context.lock_exchanges();
        let exchange = exchanges
            .exchanges
            .get(&connection)
            .ok_or_else(|| VmError::HostError(format!("unknown websocket handle {connection}")))?;
        (
            exchange.request.clone(),
            exchange.transport.tls_flow.clone(),
            exchange.websocket_dag.clone(),
        )
    };

    let target_host = websocket.target_host().ok_or_else(|| {
        VmError::HostError(
            "websocket target is unavailable before websocket::connection::set_target".to_string(),
        )
    })?;
    let target_port = websocket.target_port().ok_or_else(|| {
        VmError::HostError(
            "websocket target is unavailable before websocket::connection::set_target".to_string(),
        )
    })?;
    let (url, host_header) = build_websocket_url(
        websocket.target_scheme(),
        target_host,
        target_port,
        &request.path,
        &request.query,
    )?;
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
    if connection == DOWNSTREAM_CONNECTION_HANDLE {
        let mut transport = context.lock_transport();
        transport.tcp_dag.downstream.mark_connected();
        drop(transport);
        context.with_downstream_websocket_mut(|websocket| {
            websocket.mark_open(io, negotiated_subprotocol);
        });
        return Ok(());
    }
    if connection == default_upstream_exchange_handle() {
        {
            let mut transport = context.lock_transport();
            transport.tcp_dag.default_upstream.mark_connected();
            if transport.tls_dag.default_upstream.is_present() {
                transport.tls_dag.default_upstream.note_handshake_prepared();
                transport.tls_dag.default_upstream.note_client_hello_sent();
                transport
                    .tls_dag
                    .default_upstream
                    .note_server_hello_received();
                transport
                    .tls_dag
                    .default_upstream
                    .note_server_certificate_received(None);
                transport
                    .tls_dag
                    .default_upstream
                    .note_server_certificate_verified();
                transport
                    .tls_dag
                    .default_upstream
                    .mark_handshake_complete(Some("http/1.1".to_string()));
            }
        }
        context.with_default_upstream_exchange_mut(|exchange| {
            exchange.websocket_dag.mark_open(io, negotiated_subprotocol);
        });
        return Ok(());
    }

    let mut exchanges = context.lock_exchanges();
    let exchange = exchanges
        .exchanges
        .get_mut(&connection)
        .ok_or_else(|| VmError::HostError(format!("unknown websocket handle {connection}")))?;
    exchange.transport.tcp_flow.mark_connected();
    if exchange.transport.tls_flow.is_present() {
        exchange.transport.tls_flow.note_handshake_prepared();
        exchange.transport.tls_flow.note_client_hello_sent();
        exchange.transport.tls_flow.note_server_hello_received();
        exchange
            .transport
            .tls_flow
            .note_server_certificate_received(None);
        exchange
            .transport
            .tls_flow
            .note_server_certificate_verified();
        exchange
            .transport
            .tls_flow
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
    if connection == DOWNSTREAM_CONNECTION_HANDLE {
        let mut transport = context.lock_transport();
        transport.tcp_dag.downstream.mark_failed(message.clone());
        if transport.tls_dag.downstream.is_present() {
            transport.tls_dag.downstream.mark_failed();
        }
        drop(transport);
        context.with_downstream_websocket_mut(|websocket| websocket.mark_failed(message));
        return Ok(());
    }
    if connection == default_upstream_exchange_handle() {
        let mut transport = context.lock_transport();
        if transport.tls_dag.default_upstream.is_present() {
            transport.tls_dag.default_upstream.mark_failed();
        }
        context.with_default_upstream_exchange_mut(|exchange| {
            exchange.websocket_dag.mark_failed(message);
        });
        return Ok(());
    }
    let mut exchanges = context.lock_exchanges();
    let exchange = exchanges
        .exchanges
        .get_mut(&connection)
        .ok_or_else(|| VmError::HostError(format!("unknown websocket handle {connection}")))?;
    if exchange.transport.tls_flow.is_present() {
        exchange.transport.tls_flow.mark_failed();
    }
    exchange.websocket_dag.mark_failed(message);
    Ok(())
}

fn note_websocket_handshake_started(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<(), VmError> {
    if connection == DOWNSTREAM_CONNECTION_HANDLE {
        context.with_downstream_websocket_mut(|websocket| websocket.note_handshake_started());
        return Ok(());
    }
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

fn downstream_websocket_transport_available(context: &SharedProxyVmContext) -> bool {
    let guard = context.lock_transport();
    guard.downstream_tcp_io.is_some() || {
        #[cfg(feature = "tls")]
        {
            guard.downstream_tls_io.is_some()
        }
        #[cfg(not(feature = "tls"))]
        {
            false
        }
    }
}

enum DownstreamWebSocketTransport {
    Tcp(ReplayPrefixedIo<tokio::net::TcpStream>),
    #[cfg(feature = "tls")]
    Tls(Box<ReplayPrefixedIo<tokio_rustls::server::TlsStream<DownstreamReplayTcpStream>>>),
}

async fn take_downstream_websocket_transport(
    context: &SharedProxyVmContext,
) -> Result<DownstreamWebSocketTransport, VmError> {
    #[cfg(feature = "tls")]
    {
        let (tls_io, preread) = {
            let mut guard = context.lock_transport();
            (
                guard.downstream_tls_io.take(),
                std::mem::take(&mut guard.downstream_preread_buffer),
            )
        };
        if let Some(io) = tls_io {
            let mut guard = io.lock().await;
            let stream = guard.take().ok_or_else(|| {
                VmError::HostError(
                    "downstream tls plaintext transport is already in use".to_string(),
                )
            })?;
            return Ok(DownstreamWebSocketTransport::Tls(Box::new(
                ReplayPrefixedIo::new(preread, stream),
            )));
        }
        if context
            .lock_transport()
            .downstream_tls_server_start
            .is_some()
        {
            return Err(VmError::HostError(
                "downstream websocket accept requires plaintext transport; complete tls::session::handshake first".to_string(),
            ));
        }
    }

    let (tcp_io, preread) = {
        let mut guard = context.lock_transport();
        (
            guard.downstream_tcp_io.take(),
            std::mem::take(&mut guard.downstream_preread_buffer),
        )
    };
    let Some(io) = tcp_io else {
        return Err(websocket_operation_on_downstream());
    };
    let mut guard = io.lock().await;
    let stream = guard.take().ok_or_else(|| {
        VmError::HostError("downstream tcp transport is already in use".to_string())
    })?;
    Ok(DownstreamWebSocketTransport::Tcp(ReplayPrefixedIo::new(
        preread, stream,
    )))
}

fn pick_downstream_subprotocol(request: &WsRequest, configured: &[String]) -> Option<String> {
    if configured.is_empty() {
        return None;
    }
    request
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .split(',')
                .map(str::trim)
                .find(|protocol| configured.iter().any(|configured| configured == protocol))
                .map(str::to_string)
        })
}

async fn accept_downstream_websocket(
    context: &SharedProxyVmContext,
) -> Result<(SharedWebSocketIo, Option<String>), VmError> {
    let configured_protocols = context
        .downstream_websocket()
        .requested_subprotocols()
        .to_vec();
    let selected_protocol = Arc::new(Mutex::new(None::<String>));

    let io = match take_downstream_websocket_transport(context).await? {
        DownstreamWebSocketTransport::Tcp(stream) => {
            let configured_protocols = configured_protocols.clone();
            let selected_for_callback = Arc::clone(&selected_protocol);
            let callback = move |request: &WsRequest, mut response: WsResponse| {
                let selected = pick_downstream_subprotocol(request, &configured_protocols);
                if let Some(protocol) = &selected
                    && let Ok(value) = WsHeaderValue::from_str(protocol)
                {
                    response
                        .headers_mut()
                        .insert("sec-websocket-protocol", value);
                }
                *selected_for_callback
                    .lock()
                    .expect("downstream websocket protocol lock should not poison") = selected;
                Ok(response)
            };
            let websocket = accept_hdr_async(stream, callback).await.map_err(|err| {
                VmError::HostError(format!("downstream websocket accept failed: {err}"))
            })?;
            Arc::new(tokio::sync::Mutex::new(
                OutboundWebSocketIoState::new_server_tcp(websocket),
            ))
        }
        #[cfg(feature = "tls")]
        DownstreamWebSocketTransport::Tls(stream) => {
            let stream = *stream;
            let configured_protocols = configured_protocols.clone();
            let selected_for_callback = Arc::clone(&selected_protocol);
            let callback = move |request: &WsRequest, mut response: WsResponse| {
                let selected = pick_downstream_subprotocol(request, &configured_protocols);
                if let Some(protocol) = &selected
                    && let Ok(value) = WsHeaderValue::from_str(protocol)
                {
                    response
                        .headers_mut()
                        .insert("sec-websocket-protocol", value);
                }
                *selected_for_callback
                    .lock()
                    .expect("downstream websocket protocol lock should not poison") = selected;
                Ok(response)
            };
            let websocket = accept_hdr_async(stream, callback).await.map_err(|err| {
                VmError::HostError(format!("downstream websocket accept failed: {err}"))
            })?;
            Arc::new(tokio::sync::Mutex::new(
                OutboundWebSocketIoState::new_server_tls(websocket),
            ))
        }
    };
    let negotiated_subprotocol = selected_protocol
        .lock()
        .expect("downstream websocket protocol lock should not poison")
        .clone();
    Ok((io, negotiated_subprotocol))
}

pub(crate) fn websocket_connection_mode(context: &SharedProxyVmContext, connection: i64) -> bool {
    let Ok(handle) = decode_connection(context, connection) else {
        return false;
    };
    connection_state(context, handle).is_websocket_mode()
}

pub(crate) fn websocket_negotiated_subprotocol(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<Option<String>, VmError> {
    let state = connection_state(context, decode_connection(context, connection)?);
    let protocol = state.negotiated_subprotocol();
    if protocol.is_empty() {
        Ok(None)
    } else {
        Ok(Some(protocol.to_string()))
    }
}

fn refresh_connection_close_state(context: &SharedProxyVmContext, connection: i64) {
    match connection {
        1 => {
            if let Some(exchange) = context.lock_exchanges().exchanges.get_mut(&connection) {
                exchange.websocket_dag.refresh_close_state();
            }
        }
        0 => context.with_downstream_websocket_mut(|websocket| websocket.refresh_close_state()),
        handle => {
            if let Some(exchange) = context.lock_exchanges().exchanges.get_mut(&handle) {
                exchange.websocket_dag.refresh_close_state();
            }
        }
    }
}

pub(crate) async fn ensure_outbound_websocket_connection_open(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<(), VmError> {
    if connection != DOWNSTREAM_CONNECTION_HANDLE
        && outbound_exchange_response_available(context, connection)
    {
        return Err(VmError::HostError(format!(
            "websocket connection handle {connection} cannot enter websocket mode after the HTTP response has started",
        )));
    }
    match decode_connection(context, connection)? {
        WebSocketHandle::Downstream => {
            let state = connection_state(context, WebSocketHandle::Downstream);
            if state.is_open() {
                return Ok(());
            }
            if !downstream_websocket_transport_available(context) {
                return Err(websocket_operation_on_downstream());
            }
            note_websocket_handshake_started(context, connection)?;
            match accept_downstream_websocket(context).await {
                Ok((io, negotiated_subprotocol)) => {
                    store_connected_websocket(context, connection, io, negotiated_subprotocol)?;
                    return Ok(());
                }
                Err(err) => {
                    let message = err.to_string();
                    let _ = store_failed_websocket(context, connection, message.clone());
                    return Err(err);
                }
            }
        }
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
    let io = std::sync::Arc::new(tokio::sync::Mutex::new(
        OutboundWebSocketIoState::new_client(stream),
    ));
    store_connected_websocket(context, connection, io, negotiated_subprotocol)?;
    Ok(())
}

pub(crate) fn validate_outbound_websocket_binary_connection(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<(), VmError> {
    match decode_connection(context, connection)? {
        WebSocketHandle::Downstream => {
            if downstream_websocket_transport_available(context)
                || context.downstream_websocket().is_open()
            {
                Ok(())
            } else {
                Err(websocket_operation_on_downstream())
            }
        }
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
        let mut exchanges = context.lock_exchanges();
        match connection {
            1 => exchanges
                .exchanges
                .get_mut(&connection)
                .expect("default upstream exchange should exist")
                .websocket_dag
                .note_closing(),
            0 => context.with_downstream_websocket_mut(|websocket| websocket.note_closing()),
            handle => {
                if let Some(exchange) = exchanges.exchanges.get_mut(&handle) {
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
    match connection {
        1 => context.with_default_upstream_exchange_mut(|exchange| {
            exchange.websocket_dag.mark_closed(close_code, close_reason);
        }),
        0 => context.with_downstream_websocket_mut(|websocket| {
            websocket.mark_closed(close_code, close_reason);
        }),
        handle => {
            if let Some(exchange) = context.lock_exchanges().exchanges.get_mut(&handle) {
                exchange.websocket_dag.mark_closed(close_code, close_reason);
            }
        }
    }
    Ok(())
}

/// Allocates a WebSocket connection handle.
#[pd_edge_host_function(name = websocket::connection::NEW.name, scope = websocket)]
async fn connection_new(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let handle = allocate_outbound_exchange_handle(&context)?;
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

/// Returns the WebSocket connection handle for the current downstream flow.
#[pd_edge_host_function(name = websocket::connection::DOWNSTREAM.name, scope = websocket)]
async fn connection_downstream(
    _vm: &mut Vm,
    _context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::Int(
        DOWNSTREAM_CONNECTION_HANDLE,
    )]))
}

/// Returns the default upstream handle for the WebSocket connection.
#[pd_edge_host_function(name = websocket::connection::DEFAULT_UPSTREAM.name, scope = websocket)]
async fn connection_default_upstream(
    _vm: &mut Vm,
    _context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::Int(
        default_upstream_exchange_handle(),
    )]))
}

/// Returns whether the WebSocket connection handle is present.
#[pd_edge_host_function(name = websocket::connection::IS_PRESENT.name, scope = websocket)]
async fn connection_is_present(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    let present = match decode_connection(&context, connection)? {
        WebSocketHandle::Downstream => {
            let state = connection_state(&context, WebSocketHandle::Downstream);
            state.is_present() || downstream_websocket_transport_available(&context)
        }
        handle => connection_state(&context, handle).is_present(),
    };
    Ok(CallOutcome::Return(vec![Value::Bool(present)]))
}

/// Sets the target endpoint for the WebSocket connection.
#[pd_edge_host_function(name = websocket::connection::SET_TARGET.name, scope = websocket)]
async fn connection_set_target(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    host: String,
    port: i64,
) -> Result<CallOutcome, VmError> {
    match decode_connection(&context, connection)? {
        WebSocketHandle::Downstream => return Err(websocket_operation_on_downstream()),
        WebSocketHandle::DefaultUpstream | WebSocketHandle::OutboundExchange(_) => {
            prepare_outbound_socket_target(&context, connection, host, port)?;
        }
    }
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the scheme for the WebSocket connection.
#[pd_edge_host_function(name = websocket::connection::SET_SCHEME.name, scope = websocket)]
async fn connection_set_scheme(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    scheme: String,
) -> Result<CallOutcome, VmError> {
    match decode_connection(&context, connection)? {
        WebSocketHandle::Downstream => return Err(websocket_operation_on_downstream()),
        WebSocketHandle::DefaultUpstream | WebSocketHandle::OutboundExchange(_) => {
            prepare_outbound_scheme(&context, connection, scheme)?;
        }
    }
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the path for the WebSocket connection.
#[pd_edge_host_function(name = websocket::connection::SET_PATH.name, scope = websocket)]
async fn connection_set_path(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    path: String,
) -> Result<CallOutcome, VmError> {
    match decode_connection(&context, connection)? {
        WebSocketHandle::Downstream => return Err(websocket_operation_on_downstream()),
        WebSocketHandle::DefaultUpstream | WebSocketHandle::OutboundExchange(_) => {
            prepare_outbound_path(&context, connection, path)?;
        }
    }
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the query string for the WebSocket connection.
#[pd_edge_host_function(name = websocket::connection::SET_QUERY.name, scope = websocket)]
async fn connection_set_query(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    query: String,
) -> Result<CallOutcome, VmError> {
    match decode_connection(&context, connection)? {
        WebSocketHandle::Downstream => return Err(websocket_operation_on_downstream()),
        WebSocketHandle::DefaultUpstream | WebSocketHandle::OutboundExchange(_) => {
            prepare_outbound_query(&context, connection, query)?;
        }
    }
    Ok(CallOutcome::Return(vec![]))
}

/// Sets a header on the WebSocket connection.
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

/// Sets the preferred subprotocol list for the WebSocket connection.
#[pd_edge_host_function(name = websocket::connection::SET_SUBPROTOCOLS.name, scope = websocket)]
async fn connection_set_subprotocols(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    protocols: String,
) -> Result<CallOutcome, VmError> {
    let protocols = parse_subprotocols(&protocols)?;
    match decode_connection(&context, connection)? {
        WebSocketHandle::Downstream => {
            if !downstream_websocket_transport_available(&context) {
                return Err(websocket_operation_on_downstream());
            }
            context.with_downstream_websocket_mut(|websocket| {
                websocket.set_requested_subprotocols(protocols);
            });
        }
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

/// Attempts to connect the WebSocket connection.
#[pd_edge_host_function(name = websocket::connection::CONNECT.name, scope = websocket)]
async fn connection_connect(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    ensure_outbound_websocket_connection_open(&context, connection).await?;
    Ok(CallOutcome::Return(vec![Value::Bool(true)]))
}

/// Returns the current phase for the WebSocket connection.
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

/// Returns the negotiated subprotocol for the WebSocket connection.
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

/// Sends a text message over the WebSocket connection.
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

/// Reads a text message from the WebSocket connection.
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

/// Sends a base64-encoded binary message over the WebSocket connection.
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

/// Reads a base64-encoded binary message from the WebSocket connection.
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

/// Returns whether the WebSocket connection has reached EOF.
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

/// Closes the WebSocket connection.
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
        let mut exchanges = context.lock_exchanges();
        match connection {
            1 => exchanges
                .exchanges
                .get_mut(&connection)
                .expect("default upstream exchange should exist")
                .websocket_dag
                .note_closing(),
            0 => context.with_downstream_websocket_mut(|websocket| websocket.note_closing()),
            handle => {
                if let Some(exchange) = exchanges.exchanges.get_mut(&handle) {
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
    match connection {
        1 => context.with_default_upstream_exchange_mut(|exchange| {
            exchange.websocket_dag.mark_closed(close_code, close_reason);
        }),
        0 => context.with_downstream_websocket_mut(|websocket| {
            websocket.mark_closed(close_code, close_reason);
        }),
        handle => {
            if let Some(exchange) = context.lock_exchanges().exchanges.get_mut(&handle) {
                exchange.websocket_dag.mark_closed(close_code, close_reason);
            }
        }
    }
    Ok(CallOutcome::Return(vec![]))
}
