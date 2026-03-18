use std::sync::Arc;

use edge_abi::symbols::proxy as proxy_symbols;
use pd_edge_host_function::pd_edge_host_function;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Notify,
    task::yield_now,
    try_join,
};
use vm::{CallOutcome, Value, Vm, VmError};

use super::{
    SharedProxyVmContext,
    http::{exchange as http_exchange, response as http_response, state as http_state},
    transport::{TcpStreamRef, TlsSessionRef, decode_tcp_stream_handle, decode_tls_session_handle},
    websocket::{
        close_websocket_binary_stream, ensure_outbound_websocket_connection_open,
        read_websocket_binary_bytes, validate_outbound_websocket_binary_connection,
        websocket_negotiated_subprotocol, write_websocket_binary_bytes,
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProxyByteStreamEndpoint {
    HttpDownstream,
    DownstreamConnect,
    DownstreamWebSocketBinary,
    HttpExchange(i64),
    DynamicTcp(i64),
    DynamicTls(i64),
    WebSocketBinary(i64),
}

const RESERVED_HTTP_DOWNSTREAM_PROXY_STREAM_HANDLE: i64 = -1;
const RESERVED_DOWNSTREAM_CONNECT_PROXY_STREAM_HANDLE: i64 = -2;
const RESERVED_DEFAULT_UPSTREAM_PROXY_STREAM_HANDLE: i64 = -3;

#[derive(Clone)]
pub(crate) struct ProxyByteStreamState {
    endpoint: ProxyByteStreamEndpoint,
    write_observed: bool,
    write_closed: bool,
    write_close_notify: Option<Arc<Notify>>,
}

impl std::fmt::Debug for ProxyByteStreamState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyByteStreamState")
            .field("endpoint", &self.endpoint)
            .field("write_observed", &self.write_observed)
            .field("write_closed", &self.write_closed)
            .finish()
    }
}

impl ProxyByteStreamState {
    pub(crate) fn new(endpoint: ProxyByteStreamEndpoint) -> Self {
        Self {
            endpoint,
            write_observed: false,
            write_closed: false,
            write_close_notify: None,
        }
    }

    fn write_close_notify(&mut self) -> Arc<Notify> {
        self.write_close_notify
            .get_or_insert_with(|| Arc::new(Notify::new()))
            .clone()
    }
}

enum ProxyReadStep {
    Data(Vec<u8>),
    Eof,
    WaitingForWriteClose,
    Blocked,
}

fn unknown_proxy_stream_handle(handle: i64) -> VmError {
    VmError::HostError(format!("unknown proxy byte-stream handle {handle}"))
}

fn reserved_proxy_stream_handle(endpoint: &ProxyByteStreamEndpoint) -> Option<i64> {
    match endpoint {
        ProxyByteStreamEndpoint::HttpDownstream => {
            Some(RESERVED_HTTP_DOWNSTREAM_PROXY_STREAM_HANDLE)
        }
        ProxyByteStreamEndpoint::DownstreamConnect => {
            Some(RESERVED_DOWNSTREAM_CONNECT_PROXY_STREAM_HANDLE)
        }
        ProxyByteStreamEndpoint::HttpExchange(exchange)
            if *exchange == http_state::default_upstream_exchange_handle() =>
        {
            Some(RESERVED_DEFAULT_UPSTREAM_PROXY_STREAM_HANDLE)
        }
        _ => None,
    }
}

fn reserved_proxy_stream_endpoint(handle: i64) -> Option<ProxyByteStreamEndpoint> {
    match handle {
        RESERVED_HTTP_DOWNSTREAM_PROXY_STREAM_HANDLE => {
            Some(ProxyByteStreamEndpoint::HttpDownstream)
        }
        RESERVED_DOWNSTREAM_CONNECT_PROXY_STREAM_HANDLE => {
            Some(ProxyByteStreamEndpoint::DownstreamConnect)
        }
        RESERVED_DEFAULT_UPSTREAM_PROXY_STREAM_HANDLE => Some(
            ProxyByteStreamEndpoint::HttpExchange(http_state::default_upstream_exchange_handle()),
        ),
        _ => None,
    }
}

fn decode_chunk_size(max_bytes: i64) -> Result<usize, VmError> {
    if max_bytes <= 0 {
        return Err(VmError::HostError(format!(
            "proxy chunk size must be positive, got {max_bytes}",
        )));
    }
    usize::try_from(max_bytes).map_err(|_| {
        VmError::HostError(format!(
            "proxy chunk size is too large for this runtime: {max_bytes}",
        ))
    })
}

fn allocate_proxy_stream_handle(
    context: &SharedProxyVmContext,
    endpoint: ProxyByteStreamEndpoint,
) -> Result<i64, VmError> {
    let mut guard = context.lock_proxy();
    let handle = guard.next_proxy_stream_handle;
    if handle == i64::MAX {
        return Err(VmError::HostError(
            "proxy byte-stream handle space exhausted".to_string(),
        ));
    }
    guard.next_proxy_stream_handle = handle.saturating_add(1);
    guard
        .proxy_stream_handles
        .insert(handle, ProxyByteStreamState::new(endpoint));
    Ok(handle)
}

fn ensure_proxy_stream_state_mut(
    guard: &mut http_state::ProxyStreamRegistry,
    handle: i64,
) -> Result<&mut ProxyByteStreamState, VmError> {
    if guard.proxy_stream_handles.contains_key(&handle) {
        return guard
            .proxy_stream_handles
            .get_mut(&handle)
            .ok_or_else(|| unknown_proxy_stream_handle(handle));
    }
    if let Some(endpoint) = reserved_proxy_stream_endpoint(handle) {
        return Ok(guard
            .proxy_stream_handles
            .get_or_insert_with(handle, || ProxyByteStreamState::new(endpoint)));
    }
    Err(unknown_proxy_stream_handle(handle))
}

fn proxy_stream_state(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<ProxyByteStreamState, VmError> {
    let guard = context.lock_proxy();
    if let Some(stream) = guard.proxy_stream_handles.get(&handle).cloned() {
        return Ok(stream);
    }
    if let Some(endpoint) = reserved_proxy_stream_endpoint(handle) {
        return Ok(ProxyByteStreamState::new(endpoint));
    }
    Err(unknown_proxy_stream_handle(handle))
}

fn downstream_proxy_endpoint(context: &SharedProxyVmContext) -> ProxyByteStreamEndpoint {
    if context
        .with_request_head(|request_head| request_head.method() == axum::http::Method::CONNECT)
    {
        ProxyByteStreamEndpoint::DownstreamConnect
    } else if context.downstream_websocket().is_present() {
        ProxyByteStreamEndpoint::DownstreamWebSocketBinary
    } else {
        ProxyByteStreamEndpoint::HttpDownstream
    }
}

fn dynamic_tcp_proxy_io(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<super::transport::SharedTcpStreamIo, VmError> {
    context
        .lock_transport()
        .tcp_stream_ios
        .get(&handle)
        .cloned()
        .ok_or_else(|| {
            VmError::HostError(format!(
                "dynamic tcp stream handle {handle} has no active transport",
            ))
        })
}

#[cfg(feature = "tls")]
fn dynamic_tls_proxy_io(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<super::transport::SharedTlsStreamIo, VmError> {
    context
        .lock_transport()
        .dynamic_tls_session_ios
        .get(&handle)
        .cloned()
        .ok_or_else(|| {
            VmError::HostError(format!(
                "dynamic tls session handle {handle} has no active plaintext transport",
            ))
        })
}

fn mark_dynamic_tcp_proxy_read_eof(context: &SharedProxyVmContext, handle: i64) {
    let mut guard = context.lock_transport();
    if let Some(state) = guard.tcp_streams.get_mut(&handle) {
        state.mark_read_eof();
    }
}

fn clear_dynamic_tcp_proxy_read_eof(context: &SharedProxyVmContext, handle: i64) {
    let mut guard = context.lock_transport();
    if let Some(state) = guard.tcp_streams.get_mut(&handle) {
        state.clear_read_eof();
    }
}

fn mark_dynamic_tcp_proxy_failed(context: &SharedProxyVmContext, handle: i64, message: &str) {
    let mut guard = context.lock_transport();
    if let Some(state) = guard.tcp_streams.get_mut(&handle) {
        state.mark_failed(message.to_string());
    }
    guard.tcp_stream_ios.remove(&handle);
}

#[cfg(feature = "tls")]
fn mark_dynamic_tls_proxy_failed(context: &SharedProxyVmContext, handle: i64, message: &str) {
    let mut guard = context.lock_transport();
    if let Some(state) = guard.tcp_streams.get_mut(&handle) {
        state.mark_failed(message.to_string());
    }
    if let Some(flow) = guard.dynamic_tls_sessions.get_mut(&handle) {
        flow.mark_failed();
    }
    guard.dynamic_tls_session_ios.remove(&handle);
}

fn prepare_proxy_stream_write(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<ProxyByteStreamEndpoint, VmError> {
    let mut guard = context.lock_proxy();
    let stream = ensure_proxy_stream_state_mut(&mut guard, handle)?;
    if stream.write_closed {
        return Err(VmError::HostError(format!(
            "proxy byte-stream handle {handle} is write-closed",
        )));
    }
    stream.write_observed = true;
    Ok(stream.endpoint.clone())
}

fn mark_proxy_stream_write_closed(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<ProxyByteStreamEndpoint, VmError> {
    let (endpoint, notify) = {
        let mut guard = context.lock_proxy();
        let stream = ensure_proxy_stream_state_mut(&mut guard, handle)?;
        if stream.write_closed {
            (stream.endpoint.clone(), None)
        } else {
            stream.write_closed = true;
            (
                stream.endpoint.clone(),
                stream.write_close_notify.as_ref().cloned(),
            )
        }
    };
    if let Some(notify) = notify {
        notify.notify_waiters();
    }
    Ok(endpoint)
}

fn proxy_stream_write_closed(context: &SharedProxyVmContext, handle: i64) -> Result<bool, VmError> {
    let guard = context.lock_proxy();
    if let Some(stream) = guard.proxy_stream_handles.get(&handle) {
        return Ok(stream.write_closed);
    }
    if reserved_proxy_stream_endpoint(handle).is_some() {
        return Ok(false);
    }
    Err(unknown_proxy_stream_handle(handle))
}

fn proxy_stream_write_close_notify(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<Arc<Notify>, VmError> {
    let mut guard = context.lock_proxy();
    let stream = ensure_proxy_stream_state_mut(&mut guard, handle)?;
    Ok(stream.write_close_notify())
}

async fn wait_for_proxy_stream_write_close(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<(), VmError> {
    loop {
        let notify = proxy_stream_write_close_notify(context, handle)?;
        let notified = notify.notified();
        if proxy_stream_write_closed(context, handle)? {
            return Ok(());
        }
        notified.await;
    }
}

fn endpoint_from_tcp_stream(
    context: &SharedProxyVmContext,
    stream: i64,
) -> Result<ProxyByteStreamEndpoint, VmError> {
    if let Some(stream_ref) = decode_tcp_stream_handle(stream) {
        return Ok(match stream_ref {
            TcpStreamRef::Downstream => downstream_proxy_endpoint(context),
            TcpStreamRef::DefaultUpstream => {
                ProxyByteStreamEndpoint::HttpExchange(http_state::default_upstream_exchange_handle())
            }
        });
    }
    if http_state::tcp_stream_exists(context, stream) {
        return Ok(ProxyByteStreamEndpoint::DynamicTcp(stream));
    }
    if http_state::outbound_exchange_exists(context, stream) {
        return Ok(ProxyByteStreamEndpoint::HttpExchange(stream));
    }
    Err(VmError::HostError(format!(
        "invalid tcp stream handle {stream}; expected 0 (downstream), 1 (default upstream), a connected dynamic tcp handle, or an allocated outbound exchange handle",
    )))
}

fn tls_present_for_endpoint(
    context: &SharedProxyVmContext,
    endpoint: &ProxyByteStreamEndpoint,
) -> Result<bool, VmError> {
    match endpoint {
        ProxyByteStreamEndpoint::HttpDownstream => {
            let guard = context.lock_transport();
            Ok(guard.tls_dag.downstream.is_present())
        }
        ProxyByteStreamEndpoint::DownstreamConnect => {
            let guard = context.lock_transport();
            Ok(guard.tls_dag.downstream.is_present())
        }
        ProxyByteStreamEndpoint::DownstreamWebSocketBinary => Ok(false),
        ProxyByteStreamEndpoint::HttpExchange(handle) => {
            Ok(http_state::outbound_exchange_tls_flow(context, *handle)?.is_present())
        }
        ProxyByteStreamEndpoint::DynamicTcp(_) => Ok(false),
        ProxyByteStreamEndpoint::DynamicTls(handle) => context
            .lock_transport()
            .dynamic_tls_sessions
            .get(handle)
            .cloned()
            .map(|flow| flow.is_present())
            .ok_or_else(|| {
                VmError::HostError(format!(
                    "dynamic tls session handle {handle} is unavailable for proxy transport",
                ))
            }),
        ProxyByteStreamEndpoint::WebSocketBinary(_) => Ok(false),
    }
}

fn endpoint_from_tls_plaintext(
    context: &SharedProxyVmContext,
    session: i64,
) -> Result<ProxyByteStreamEndpoint, VmError> {
    let endpoint = if let Some(session_ref) = decode_tls_session_handle(session) {
        match session_ref {
            TlsSessionRef::Downstream => downstream_proxy_endpoint(context),
            TlsSessionRef::DefaultUpstream => {
                ProxyByteStreamEndpoint::HttpExchange(http_state::default_upstream_exchange_handle())
            }
        }
    } else if http_state::tcp_stream_exists(context, session) {
        ProxyByteStreamEndpoint::DynamicTls(session)
    } else if http_state::outbound_exchange_exists(context, session) {
        ProxyByteStreamEndpoint::HttpExchange(session)
    } else {
        return Err(VmError::HostError(format!(
            "invalid tls session handle {session}; expected 0 (downstream), 1 (default upstream), a connected dynamic tls handle, or an allocated outbound exchange handle",
        )));
    };

    if !tls_present_for_endpoint(context, &endpoint)? {
        return Err(VmError::HostError(format!(
            "tls plaintext stream is unavailable for handle {session} before a TLS session is present",
        )));
    }

    Ok(endpoint)
}

fn endpoint_from_websocket_binary(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<ProxyByteStreamEndpoint, VmError> {
    validate_outbound_websocket_binary_connection(context, connection)?;
    if connection != http_state::default_upstream_exchange_handle()
        && !http_state::outbound_exchange_exists(context, connection)
    {
        return Err(VmError::HostError(format!(
            "invalid websocket connection handle {connection}; expected 1 (default upstream) or an allocated outbound exchange handle",
        )));
    }
    Ok(ProxyByteStreamEndpoint::WebSocketBinary(connection))
}

async fn proxy_stream_read_step(
    context: &SharedProxyVmContext,
    handle: i64,
    max_bytes: usize,
) -> Result<ProxyReadStep, VmError> {
    let stream = proxy_stream_state(context, handle)?;
    match stream.endpoint {
        ProxyByteStreamEndpoint::HttpDownstream => {
            let chunk = http_state::read_request_body_next_chunk(context, max_bytes).await?;
            if chunk.is_empty() {
                Ok(ProxyReadStep::Eof)
            } else {
                Ok(ProxyReadStep::Data(chunk))
            }
        }
        ProxyByteStreamEndpoint::DownstreamConnect => Err(VmError::HostError(
            "downstream connect tunnels are only available through proxy::bridge".to_string(),
        )),
        ProxyByteStreamEndpoint::DownstreamWebSocketBinary => Err(VmError::HostError(
            "downstream websocket tunnels are only available through proxy::bridge".to_string(),
        )),
        ProxyByteStreamEndpoint::HttpExchange(exchange) => {
            if !http_state::outbound_exchange_response_available(context, exchange)
                && stream.write_observed
                && !stream.write_closed
            {
                return Ok(ProxyReadStep::WaitingForWriteClose);
            }
            let chunk = http_state::read_outbound_exchange_response_next_chunk(
                context, exchange, max_bytes,
            )
            .await?;
            if chunk.is_empty() {
                if http_state::outbound_exchange_response_eof(context, exchange).await? {
                    Ok(ProxyReadStep::Eof)
                } else {
                    Ok(ProxyReadStep::Blocked)
                }
            } else {
                Ok(ProxyReadStep::Data(chunk))
            }
        }
        ProxyByteStreamEndpoint::DynamicTcp(dynamic) => {
            let io = dynamic_tcp_proxy_io(context, dynamic)?;
            let mut guard = io.lock().await;
            let stream = guard.as_mut().ok_or_else(|| {
                VmError::HostError(format!(
                    "dynamic tcp stream handle {dynamic} has no active transport",
                ))
            })?;
            let mut buffer = vec![0u8; max_bytes];
            let read = stream.read(&mut buffer).await.map_err(|err| {
                let message = format!("proxy tcp read failed: {err}");
                mark_dynamic_tcp_proxy_failed(context, dynamic, &message);
                VmError::HostError(message)
            })?;
            if read == 0 {
                mark_dynamic_tcp_proxy_read_eof(context, dynamic);
                Ok(ProxyReadStep::Eof)
            } else {
                clear_dynamic_tcp_proxy_read_eof(context, dynamic);
                buffer.truncate(read);
                Ok(ProxyReadStep::Data(buffer))
            }
        }
        ProxyByteStreamEndpoint::DynamicTls(dynamic) => {
            let io = dynamic_tls_proxy_io(context, dynamic)?;
            let mut guard = io.lock().await;
            let stream = guard.as_mut().ok_or_else(|| {
                VmError::HostError(format!(
                    "dynamic tls session handle {dynamic} has no active plaintext transport",
                ))
            })?;
            let mut buffer = vec![0u8; max_bytes];
            let read = stream.read(&mut buffer).await.map_err(|err| {
                let message = format!("proxy tls plaintext read failed: {err}");
                mark_dynamic_tls_proxy_failed(context, dynamic, &message);
                VmError::HostError(message)
            })?;
            if read == 0 {
                mark_dynamic_tcp_proxy_read_eof(context, dynamic);
                Ok(ProxyReadStep::Eof)
            } else {
                clear_dynamic_tcp_proxy_read_eof(context, dynamic);
                buffer.truncate(read);
                Ok(ProxyReadStep::Data(buffer))
            }
        }
        ProxyByteStreamEndpoint::WebSocketBinary(connection) => {
            match read_websocket_binary_bytes(context, connection).await? {
                Some(bytes) => Ok(ProxyReadStep::Data(bytes)),
                None => Ok(ProxyReadStep::Eof),
            }
        }
    }
}

async fn proxy_stream_write_bytes(
    context: &SharedProxyVmContext,
    handle: i64,
    bytes: &[u8],
) -> Result<(), VmError> {
    if bytes.is_empty() {
        return Ok(());
    }
    let endpoint = prepare_proxy_stream_write(context, handle)?;
    match endpoint {
        ProxyByteStreamEndpoint::HttpDownstream => {
            http_state::append_response_output_body_bytes(context, bytes)?;
            Ok(())
        }
        ProxyByteStreamEndpoint::DownstreamConnect => Err(VmError::HostError(
            "downstream connect tunnels are only available through proxy::bridge".to_string(),
        )),
        ProxyByteStreamEndpoint::DownstreamWebSocketBinary => Err(VmError::HostError(
            "downstream websocket tunnels are only available through proxy::bridge".to_string(),
        )),
        ProxyByteStreamEndpoint::HttpExchange(exchange) => {
            http_state::append_outbound_exchange_body_bytes(context, exchange, bytes)
        }
        ProxyByteStreamEndpoint::DynamicTcp(dynamic) => {
            let io = dynamic_tcp_proxy_io(context, dynamic)?;
            let mut guard = io.lock().await;
            let stream = guard.as_mut().ok_or_else(|| {
                VmError::HostError(format!(
                    "dynamic tcp stream handle {dynamic} has no active transport",
                ))
            })?;
            stream.write_all(bytes).await.map_err(|err| {
                let message = format!("proxy tcp write failed: {err}");
                mark_dynamic_tcp_proxy_failed(context, dynamic, &message);
                VmError::HostError(message)
            })?;
            stream.flush().await.map_err(|err| {
                let message = format!("proxy tcp flush failed: {err}");
                mark_dynamic_tcp_proxy_failed(context, dynamic, &message);
                VmError::HostError(message)
            })?;
            clear_dynamic_tcp_proxy_read_eof(context, dynamic);
            Ok(())
        }
        ProxyByteStreamEndpoint::DynamicTls(dynamic) => {
            let io = dynamic_tls_proxy_io(context, dynamic)?;
            let mut guard = io.lock().await;
            let stream = guard.as_mut().ok_or_else(|| {
                VmError::HostError(format!(
                    "dynamic tls session handle {dynamic} has no active plaintext transport",
                ))
            })?;
            stream.write_all(bytes).await.map_err(|err| {
                let message = format!("proxy tls plaintext write failed: {err}");
                mark_dynamic_tls_proxy_failed(context, dynamic, &message);
                VmError::HostError(message)
            })?;
            stream.flush().await.map_err(|err| {
                let message = format!("proxy tls plaintext flush failed: {err}");
                mark_dynamic_tls_proxy_failed(context, dynamic, &message);
                VmError::HostError(message)
            })?;
            clear_dynamic_tcp_proxy_read_eof(context, dynamic);
            Ok(())
        }
        ProxyByteStreamEndpoint::WebSocketBinary(connection) => {
            write_websocket_binary_bytes(context, connection, bytes).await?;
            Ok(())
        }
    }
}

async fn proxy_stream_close_write(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<(), VmError> {
    let endpoint = mark_proxy_stream_write_closed(context, handle)?;
    match endpoint {
        ProxyByteStreamEndpoint::DynamicTcp(dynamic) => {
            let io = dynamic_tcp_proxy_io(context, dynamic)?;
            let mut guard = io.lock().await;
            if let Some(stream) = guard.as_mut() {
                stream.shutdown().await.map_err(|err| {
                    let message = format!("proxy tcp shutdown failed: {err}");
                    mark_dynamic_tcp_proxy_failed(context, dynamic, &message);
                    VmError::HostError(message)
                })?;
            }
        }
        ProxyByteStreamEndpoint::DynamicTls(dynamic) => {
            let io = dynamic_tls_proxy_io(context, dynamic)?;
            let mut guard = io.lock().await;
            if let Some(stream) = guard.as_mut() {
                stream.shutdown().await.map_err(|err| {
                    let message = format!("proxy tls plaintext shutdown failed: {err}");
                    mark_dynamic_tls_proxy_failed(context, dynamic, &message);
                    VmError::HostError(message)
                })?;
            }
        }
        ProxyByteStreamEndpoint::WebSocketBinary(connection) => {
            close_websocket_binary_stream(context, connection).await?;
        }
        ProxyByteStreamEndpoint::HttpDownstream
        | ProxyByteStreamEndpoint::DownstreamConnect
        | ProxyByteStreamEndpoint::DownstreamWebSocketBinary
        | ProxyByteStreamEndpoint::HttpExchange(_) => {}
    }
    Ok(())
}

async fn take_dynamic_tcp_connect_target(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<http_state::DownstreamConnectTunnelTarget, VmError> {
    let io = {
        let mut guard = context.lock_transport();
        let state = guard.tcp_streams.get_mut(&handle).ok_or_else(|| {
            VmError::HostError(format!(
                "dynamic tcp stream handle {handle} is unavailable for proxy tunnel attachment",
            ))
        })?;
        state.mark_proxy_attached();
        guard.tcp_stream_ios.remove(&handle).ok_or_else(|| {
            VmError::HostError(format!(
                "dynamic tcp stream handle {handle} has no active transport",
            ))
        })?
    };
    let mut guard = io.lock().await;
    let stream = guard.take().ok_or_else(|| {
        VmError::HostError(format!(
            "dynamic tcp stream handle {handle} is already in use",
        ))
    })?;
    Ok(http_state::DownstreamConnectTunnelTarget::Tcp { handle, stream })
}

#[cfg(feature = "tls")]
async fn take_dynamic_tls_connect_target(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<http_state::DownstreamConnectTunnelTarget, VmError> {
    let io = {
        let mut guard = context.lock_transport();
        let state = guard.tcp_streams.get_mut(&handle).ok_or_else(|| {
            VmError::HostError(format!(
                "dynamic tls session handle {handle} is unavailable for proxy tunnel attachment",
            ))
        })?;
        state.mark_proxy_attached();
        guard
            .dynamic_tls_session_ios
            .remove(&handle)
            .ok_or_else(|| {
                VmError::HostError(format!(
                    "dynamic tls session handle {handle} has no active plaintext transport",
                ))
            })?
    };
    let mut guard = io.lock().await;
    let stream = guard.take().ok_or_else(|| {
        VmError::HostError(format!(
            "dynamic tls session handle {handle} is already in use",
        ))
    })?;
    Ok(http_state::DownstreamConnectTunnelTarget::Tls {
        handle,
        stream: Box::new(stream),
    })
}

async fn schedule_downstream_connect_tunnel(
    context: &SharedProxyVmContext,
    left: i64,
    right: i64,
) -> Result<Option<String>, VmError> {
    let left_endpoint = proxy_stream_state(context, left)?.endpoint;
    let right_endpoint = proxy_stream_state(context, right)?.endpoint;

    let target = match (&left_endpoint, &right_endpoint) {
        (
            ProxyByteStreamEndpoint::DownstreamConnect,
            ProxyByteStreamEndpoint::DynamicTcp(handle),
        )
        | (
            ProxyByteStreamEndpoint::DynamicTcp(handle),
            ProxyByteStreamEndpoint::DownstreamConnect,
        ) => Some(take_dynamic_tcp_connect_target(context, *handle).await?),
        (
            ProxyByteStreamEndpoint::DownstreamConnect,
            ProxyByteStreamEndpoint::DynamicTls(handle),
        )
        | (
            ProxyByteStreamEndpoint::DynamicTls(handle),
            ProxyByteStreamEndpoint::DownstreamConnect,
        ) => {
            #[cfg(feature = "tls")]
            {
                Some(take_dynamic_tls_connect_target(context, *handle).await?)
            }
            #[cfg(not(feature = "tls"))]
            {
                let _ = handle;
                None
            }
        }
        (ProxyByteStreamEndpoint::DownstreamConnect, _)
        | (_, ProxyByteStreamEndpoint::DownstreamConnect) => {
            return Err(VmError::HostError(
                "downstream connect tunnels currently require a connected dynamic tcp::stream or tls::session peer wrapped with proxy::stream::from_tcp/from_tls_plaintext".to_string(),
            ));
        }
        _ => None,
    };

    let Some(target) = target else {
        return Ok(None);
    };
    let upgrade = context.downstream_http1_upgrade().ok_or_else(|| {
        VmError::HostError(
            "downstream connect tunnel requires an upgrade-capable HTTP/1.1 downstream connection"
                .to_string(),
        )
    })?;
    let plan = http_state::DownstreamPostResponsePlan::ConnectTunnel(Box::new(
        http_state::DownstreamConnectTunnelPlan::new(context.clone(), upgrade, target),
    ));
    context.schedule_downstream_post_response_plan(plan)?;
    Ok(Some("upgraded".to_string()))
}

async fn schedule_downstream_websocket_tunnel(
    context: &SharedProxyVmContext,
    left: i64,
    right: i64,
) -> Result<Option<String>, VmError> {
    let left_endpoint = proxy_stream_state(context, left)?.endpoint;
    let right_endpoint = proxy_stream_state(context, right)?.endpoint;

    let connection = match (&left_endpoint, &right_endpoint) {
        (
            ProxyByteStreamEndpoint::DownstreamWebSocketBinary,
            ProxyByteStreamEndpoint::WebSocketBinary(connection),
        )
        | (
            ProxyByteStreamEndpoint::WebSocketBinary(connection),
            ProxyByteStreamEndpoint::DownstreamWebSocketBinary,
        ) => Some(*connection),
        (ProxyByteStreamEndpoint::DownstreamWebSocketBinary, _)
        | (_, ProxyByteStreamEndpoint::DownstreamWebSocketBinary) => {
            return Err(VmError::HostError(
                "downstream websocket tunnels currently require a websocket connection wrapped with proxy::stream::from_websocket_binary".to_string(),
            ));
        }
        _ => None,
    };

    let Some(connection) = connection else {
        return Ok(None);
    };
    ensure_outbound_websocket_connection_open(context, connection).await?;
    let selected_subprotocol = websocket_negotiated_subprotocol(context, connection)?;
    context.with_downstream_websocket_mut(|websocket| websocket.note_handshake_started());
    let upgrade = context.downstream_http1_upgrade().ok_or_else(|| {
        VmError::HostError(
            "downstream websocket tunnel requires an upgrade-capable HTTP/1.1 downstream connection"
                .to_string(),
        )
    })?;
    let plan = http_state::DownstreamPostResponsePlan::WebSocketTunnel(
        http_state::DownstreamWebSocketTunnelPlan::new(
            context.clone(),
            upgrade,
            connection,
            selected_subprotocol,
        ),
    );
    context.schedule_downstream_post_response_plan(plan)?;
    Ok(Some("upgraded".to_string()))
}

async fn schedule_default_upstream_http_forward(
    context: &SharedProxyVmContext,
    left: i64,
    right: i64,
) -> Result<Option<String>, VmError> {
    let left_state = proxy_stream_state(context, left)?;
    let right_state = proxy_stream_state(context, right)?;
    let default_upstream = http_state::default_upstream_exchange_handle();

    let is_default_http_forward = matches!(
        (&left_state.endpoint, &right_state.endpoint),
        (
            ProxyByteStreamEndpoint::HttpDownstream,
            ProxyByteStreamEndpoint::HttpExchange(exchange),
        ) | (
            ProxyByteStreamEndpoint::HttpExchange(exchange),
            ProxyByteStreamEndpoint::HttpDownstream,
        ) if *exchange == default_upstream
            && !left_state.write_observed
            && !right_state.write_observed
    );

    if !is_default_http_forward {
        return Ok(None);
    }

    if !http_state::start_native_default_upstream_http_forward_response(context).await? {
        http_state::ensure_outbound_exchange_response_started(context, default_upstream).await?;
    }
    Ok(Some("forwarded".to_string()))
}

async fn forward_default_upstream_with_response_headers(
    context: &SharedProxyVmContext,
    response_headers: Value,
) -> Result<String, VmError> {
    let debug = std::env::var_os("PD_EDGE_DEBUG_FORWARD_DEFAULT").is_some();
    if debug {
        eprintln!("forward_default_upstream: enter");
    }
    let parsed_headers = http_response::parse_response_header_batch(response_headers)?;
    if debug {
        eprintln!(
            "forward_default_upstream: parsed headers count={}",
            parsed_headers.len()
        );
    }
    let default_upstream = http_state::default_upstream_exchange_handle();
    if !http_state::start_native_default_upstream_http_forward_response(context).await? {
        if debug {
            eprintln!("forward_default_upstream: native fast path unavailable");
        }
        http_state::ensure_outbound_exchange_response_started(context, default_upstream).await?;
    } else if debug {
        eprintln!("forward_default_upstream: native fast path ready");
    }
    if !parsed_headers.is_empty() {
        context.insert_downstream_response_headers(parsed_headers)?;
        if debug {
            eprintln!("forward_default_upstream: applied response headers");
        }
    }
    if debug {
        eprintln!("forward_default_upstream: return forwarded");
    }
    Ok("forwarded".to_string())
}

async fn try_native_forward(
    context: &SharedProxyVmContext,
    left: i64,
    right: i64,
) -> Result<Option<String>, VmError> {
    if let Some(status) = schedule_downstream_connect_tunnel(context, left, right).await? {
        return Ok(Some(status));
    }
    if let Some(status) = schedule_downstream_websocket_tunnel(context, left, right).await? {
        return Ok(Some(status));
    }
    schedule_default_upstream_http_forward(context, left, right).await
}

async fn drive_pipe_direction(
    context: SharedProxyVmContext,
    source: i64,
    destination: i64,
    max_bytes: usize,
) -> Result<(), VmError> {
    loop {
        match proxy_stream_read_step(&context, source, max_bytes).await? {
            ProxyReadStep::Data(chunk) => {
                if chunk.is_empty() {
                    continue;
                }
                proxy_stream_write_bytes(&context, destination, &chunk).await?;
                yield_now().await;
            }
            ProxyReadStep::Eof => {
                proxy_stream_close_write(&context, destination).await?;
                return Ok(());
            }
            ProxyReadStep::WaitingForWriteClose => {
                wait_for_proxy_stream_write_close(&context, source).await?;
            }
            ProxyReadStep::Blocked => {
                yield_now().await;
            }
        }
    }
}

async fn drive_pipe(
    context: SharedProxyVmContext,
    source: i64,
    destination: i64,
    max_bytes: usize,
) -> Result<String, VmError> {
    let source_state = proxy_stream_state(&context, source)?;
    if let ProxyByteStreamEndpoint::HttpExchange(exchange) = source_state.endpoint
        && !http_state::outbound_exchange_response_available(&context, exchange)
        && source_state.write_observed
        && !source_state.write_closed
    {
        return Err(VmError::HostError(format!(
            "proxy byte-stream handle {source} cannot be piped yet because its read side is waiting for its write side to close; use proxy::bridge or finish writing before piping from it",
        )));
    }
    drive_pipe_direction(context, source, destination, max_bytes).await?;
    Ok("eof".to_string())
}

async fn drive_bridge(
    context: SharedProxyVmContext,
    left: i64,
    right: i64,
    max_bytes: usize,
) -> Result<String, VmError> {
    if let Some(status) = try_native_forward(&context, left, right).await? {
        return Ok(status);
    }
    try_join!(
        drive_pipe_direction(context.clone(), left, right, max_bytes),
        drive_pipe_direction(context, right, left, max_bytes)
    )?;
    Ok("closed".to_string())
}

/// Returns the proxy byte stream handle for the current downstream flow.
#[pd_edge_host_function(name = proxy_symbols::stream::DOWNSTREAM.name, scope = proxy)]
fn stream_downstream(context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    let endpoint = downstream_proxy_endpoint(&context);
    let handle = if let Some(handle) = reserved_proxy_stream_handle(&endpoint) {
        handle
    } else {
        allocate_proxy_stream_handle(&context, endpoint)?
    };
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

/// Wraps an outbound HTTP exchange as a proxy byte stream.
#[pd_edge_host_function(name = proxy_symbols::stream::EXCHANGE.name, scope = proxy)]
fn stream_exchange(context: SharedProxyVmContext, exchange: i64) -> Result<CallOutcome, VmError> {
    if !http_state::outbound_exchange_exists(&context, exchange) {
        return Err(VmError::HostError(format!(
            "unknown outbound exchange handle {exchange}",
        )));
    }
    let endpoint = ProxyByteStreamEndpoint::HttpExchange(exchange);
    let handle = if let Some(handle) = reserved_proxy_stream_handle(&endpoint) {
        handle
    } else {
        allocate_proxy_stream_handle(&context, endpoint)?
    };
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

/// Wraps a TCP stream as a proxy byte stream.
#[pd_edge_host_function(name = proxy_symbols::stream::FROM_TCP.name, scope = proxy)]
fn stream_from_tcp(context: SharedProxyVmContext, stream: i64) -> Result<CallOutcome, VmError> {
    let endpoint = endpoint_from_tcp_stream(&context, stream)?;
    let handle = if let Some(handle) = reserved_proxy_stream_handle(&endpoint) {
        handle
    } else {
        allocate_proxy_stream_handle(&context, endpoint)?
    };
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

/// Wraps a TLS plaintext session as a proxy byte stream.
#[pd_edge_host_function(name = proxy_symbols::stream::FROM_TLS_PLAINTEXT.name, scope = proxy)]
fn stream_from_tls_plaintext(
    context: SharedProxyVmContext,
    session: i64,
) -> Result<CallOutcome, VmError> {
    let endpoint = endpoint_from_tls_plaintext(&context, session)?;
    let handle = if let Some(handle) = reserved_proxy_stream_handle(&endpoint) {
        handle
    } else {
        allocate_proxy_stream_handle(&context, endpoint)?
    };
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

/// Wraps a WebSocket connection as a proxy byte stream.
#[pd_edge_host_function(name = proxy_symbols::stream::FROM_WEBSOCKET_BINARY.name, scope = proxy)]
fn stream_from_websocket_binary(
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    let endpoint = endpoint_from_websocket_binary(&context, connection)?;
    let handle = allocate_proxy_stream_handle(&context, endpoint)?;
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

/// Copies bytes in one direction from `source` into `destination`.
///
/// This always uses the buffered proxy stream loop. On EOF from `source`, the destination write
/// side is closed.
#[pd_edge_host_function(name = proxy_symbols::PIPE.name, scope = proxy)]
async fn proxy_pipe(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    source: i64,
    destination: i64,
    max_bytes: i64,
) -> Result<CallOutcome, VmError> {
    let max_bytes = decode_chunk_size(max_bytes)?;
    let status = drive_pipe(context, source, destination, max_bytes).await?;
    Ok(CallOutcome::Return(vec![Value::string(status)]))
}

/// Relays bytes in both directions between `left` and `right`.
///
/// `proxy::bridge` first tries native forward/handoff pairs. If no native handoff is available,
/// it falls back to the buffered bidirectional proxy stream loop using `max_bytes` chunks.
#[pd_edge_host_function(name = proxy_symbols::BRIDGE.name, scope = proxy)]
async fn proxy_bridge(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    left: i64,
    right: i64,
    max_bytes: i64,
) -> Result<CallOutcome, VmError> {
    let max_bytes = decode_chunk_size(max_bytes)?;
    let status = drive_bridge(context, left, right, max_bytes).await?;
    Ok(CallOutcome::Return(vec![Value::string(status)]))
}

/// Performs a native runtime handoff between supported proxy stream pairs.
///
/// Supported pairs currently are:
/// - downstream CONNECT <-> dynamic TCP from `proxy::stream::from_tcp`
/// - downstream CONNECT <-> dynamic TLS plaintext from `proxy::stream::from_tls_plaintext`
/// - downstream binary WebSocket <-> outbound binary WebSocket from
///   `proxy::stream::from_websocket_binary`
/// - downstream HTTP body stream <-> default upstream HTTP exchange from
///   `proxy::stream::exchange(http::exchange::default_upstream())`, as long as neither stream
///   has already been written through the proxy stream API
///
/// Unsupported pairs return an error. Use `proxy::bridge` when you want buffered fallback.
#[pd_edge_host_function(name = proxy_symbols::FORWARD.name, scope = proxy)]
async fn proxy_forward(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    left: i64,
    right: i64,
) -> Result<CallOutcome, VmError> {
    let status = try_native_forward(&context, left, right).await?.ok_or_else(|| {
        VmError::HostError(
            "proxy::forward supports only downstream CONNECT<->dynamic tcp/tls, downstream websocket<->websocket, or downstream HTTP<->default upstream exchange native pairs; use proxy::bridge for the buffered fallback path".to_string(),
        )
    })?;
    Ok(CallOutcome::Return(vec![Value::string(status)]))
}

/// Performs a native forward from the current downstream HTTP request to the prepared default
/// upstream exchange and overlays downstream response headers.
#[pd_edge_host_function(name = proxy_symbols::FORWARD_DEFAULT_UPSTREAM.name, scope = proxy)]
async fn proxy_forward_default_upstream(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    response_headers: Value,
) -> Result<CallOutcome, VmError> {
    let status = forward_default_upstream_with_response_headers(&context, response_headers).await?;
    Ok(CallOutcome::Return(vec![Value::string(status)]))
}

#[pd_edge_host_function(
    name = proxy_symbols::PREPARE_AND_FORWARD_DEFAULT_UPSTREAM.name,
    scope = proxy
)]
/// Prepares the default upstream request target/header batch and forwards it in one call.
async fn proxy_prepare_and_forward_default_upstream(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    host: String,
    port: i64,
    version: String,
    request_headers: Value,
    response_headers: Value,
) -> Result<CallOutcome, VmError> {
    let parsed_response_headers = http_response::parse_response_header_batch(response_headers)?;
    http_exchange::prepare_default_upstream_request(
        &context,
        host,
        port,
        version,
        request_headers,
    )?;
    let status =
        if !http_state::start_native_default_upstream_http_forward_response(&context).await? {
            http_state::ensure_outbound_exchange_response_started(
                &context,
                http_state::default_upstream_exchange_handle(),
            )
            .await?;
            "forwarded".to_string()
        } else {
            "forwarded".to_string()
        };
    if !parsed_response_headers.is_empty() {
        context.insert_downstream_response_headers(parsed_response_headers)?;
    }
    Ok(CallOutcome::Return(vec![Value::string(status)]))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{body::Body, http::HeaderMap};

    use super::{
        ProxyByteStreamEndpoint, allocate_proxy_stream_handle, endpoint_from_tls_plaintext,
    };
    use crate::abi_impl::{
        RateLimiterStore,
        http::{
            ProxyVmContext, SharedProxyVmContext,
            state::{
                HttpRequestContext, LazyRequestId, RequestPortField, RequestStringField,
                default_upstream_exchange_handle,
            },
        },
        transport::TlsSessionRef,
    };

    fn test_context(body: &str) -> SharedProxyVmContext {
        let mut headers = HeaderMap::new();
        if !body.is_empty() {
            headers.insert(
                axum::http::header::CONTENT_LENGTH,
                body.len()
                    .to_string()
                    .parse()
                    .expect("content-length should parse"),
            );
        }
        Arc::new(ProxyVmContext::from_http_request(
            HttpRequestContext {
                request_id: LazyRequestId::from_string("req-1".to_string()),
                method: axum::http::Method::POST,
                path: RequestStringField::Static("/".to_string()),
                query: RequestStringField::Static(String::new()),
                http_version: RequestStringField::Static("1.1".to_string()),
                port: RequestPortField::Static(80),
                scheme: RequestStringField::Static("http".to_string()),
                host: RequestStringField::Static("example.com".to_string()),
                client_ip: RequestStringField::Static("127.0.0.1".to_string()),
                body: Body::from(body.to_string()),
                headers: headers.into(),
            },
            Arc::new(RateLimiterStore::new()),
        ))
    }

    #[test]
    fn proxy_stream_handles_allocate_in_dynamic_range() {
        let context = test_context("");
        let first = allocate_proxy_stream_handle(&context, ProxyByteStreamEndpoint::HttpDownstream)
            .expect("first stream should allocate");
        let second = allocate_proxy_stream_handle(
            &context,
            ProxyByteStreamEndpoint::HttpExchange(default_upstream_exchange_handle()),
        )
        .expect("second stream should allocate");

        assert!(first >= 4096);
        assert_eq!(second, first + 1);
    }

    #[test]
    fn tls_plaintext_stream_requires_tls_presence() {
        let context = test_context("");
        let error = endpoint_from_tls_plaintext(&context, TlsSessionRef::Downstream.handle())
            .expect_err("plaintext stream should reject plain downstream");
        assert!(
            error
                .to_string()
                .contains("tls plaintext stream is unavailable")
        );
    }
}
