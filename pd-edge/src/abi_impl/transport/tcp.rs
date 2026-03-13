use std::sync::Arc;

use edge_abi::symbols::tcp;
use pd_edge_host_function::pd_edge_host_function;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpSocket, lookup_host},
};
use url::Url;
use vm::{CallOutcome, Value, Vm, VmError};

use super::super::SharedProxyVmContext;
use super::super::http::{
    allocate_tcp_stream_handle, append_outbound_exchange_body, append_response_output_body_bytes,
    outbound_exchange_exists, outbound_exchange_response_eof,
    read_outbound_exchange_response_next_chunk, read_request_body_next_chunk,
    read_upstream_response_next_chunk, request_body_eof, tcp_stream_exists, upstream_response_eof,
};
use super::state::{
    SharedTcpStreamIo, TcpSocketPhase, TcpSocketState, TcpStreamRef, decode_tcp_stream_handle,
};
#[cfg(feature = "tls")]
use crate::abi_impl::transport::SharedServerTlsStreamIo;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TcpStreamHandle {
    Reserved(TcpStreamRef),
    Dynamic(i64),
    OutboundExchange(i64),
}

fn decode_stream(context: &SharedProxyVmContext, stream: i64) -> Result<TcpStreamHandle, VmError> {
    if let Some(reserved) = decode_tcp_stream_handle(stream) {
        return Ok(TcpStreamHandle::Reserved(reserved));
    }
    if tcp_stream_exists(context, stream) {
        return Ok(TcpStreamHandle::Dynamic(stream));
    }
    if outbound_exchange_exists(context, stream) {
        return Ok(TcpStreamHandle::OutboundExchange(stream));
    }
    Err(VmError::HostError(format!(
        "invalid tcp stream handle {stream}; use 0/1 for reserved handles, tcp::stream::new() for dynamic sockets, or an outbound exchange handle",
    )))
}

fn decode_chunk_size(max_bytes: i64) -> Result<usize, VmError> {
    if max_bytes <= 0 {
        return Err(VmError::HostError(format!(
            "tcp::stream::read max_bytes must be positive, got {max_bytes}",
        )));
    }
    usize::try_from(max_bytes).map_err(|_| {
        VmError::HostError(format!(
            "tcp::stream::read max_bytes is too large for this runtime: {max_bytes}",
        ))
    })
}

fn normalize_tcp_target(value: &str) -> Result<String, VmError> {
    if value.is_empty() || value.chars().any(|ch| ch.is_whitespace()) {
        return Err(VmError::HostError(format!(
            "tcp target must be host:port or tcp://host:port, got '{value}'",
        )));
    }
    if value.contains("://")
        && let Ok(url) = Url::parse(value)
    {
        if !url.scheme().eq_ignore_ascii_case("tcp") {
            return Err(VmError::HostError(format!(
                "tcp target scheme must be tcp, got '{}'",
                url.scheme()
            )));
        }
        let host = url.host_str().ok_or_else(|| {
            VmError::HostError(format!("tcp target is missing a host: '{value}'"))
        })?;
        let port = url.port().ok_or_else(|| {
            VmError::HostError(format!("tcp target is missing a port: '{value}'"))
        })?;
        if !url.path().is_empty() && url.path() != "/" {
            return Err(VmError::HostError(format!(
                "tcp target must not contain a path, got '{value}'",
            )));
        }
        return Ok(if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        });
    }
    if value.rsplit_once(':').is_some() {
        return Ok(value.to_string());
    }
    Err(VmError::HostError(format!(
        "tcp target must be host:port or tcp://host:port, got '{value}'",
    )))
}

fn mutable_dynamic_tcp_stream_only() -> VmError {
    VmError::HostError(
        "this tcp operation only supports dynamic sockets returned by tcp::stream::new()"
            .to_string(),
    )
}

fn with_mutable_dynamic_tcp_socket_state<T>(
    context: &SharedProxyVmContext,
    stream: i64,
    mutate: impl FnOnce(&mut TcpSocketState, &mut Option<SharedTcpStreamIo>) -> Result<T, VmError>,
) -> Result<T, VmError> {
    let TcpStreamHandle::Dynamic(handle) = decode_stream(context, stream)? else {
        return Err(mutable_dynamic_tcp_stream_only());
    };
    let mut guard = context.lock_transport();
    let mut io_slot = guard.tcp_stream_ios.remove(&handle);
    let state = guard
        .tcp_streams
        .get_mut(&handle)
        .expect("dynamic tcp stream should exist while in use");
    let result = mutate(state, &mut io_slot);
    if let Some(io) = io_slot {
        guard.tcp_stream_ios.insert(handle, io);
    } else {
        guard.tcp_stream_ios.remove(&handle);
    }
    result
}

fn active_dynamic_tcp_stream_io(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Option<SharedTcpStreamIo> {
    let guard = context.lock_transport();
    guard.tcp_stream_ios.get(&handle).cloned()
}

fn dynamic_tcp_socket_state(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<TcpSocketState, VmError> {
    let guard = context.lock_transport();
    guard
        .tcp_streams
        .get(&handle)
        .cloned()
        .ok_or_else(|| VmError::HostError(format!("unknown dynamic tcp stream handle {handle}")))
}

fn active_downstream_tcp_io(context: &SharedProxyVmContext) -> Option<SharedTcpStreamIo> {
    let guard = context.lock_transport();
    guard.downstream_tcp_io.clone()
}

#[cfg(feature = "tls")]
fn active_downstream_tls_io(context: &SharedProxyVmContext) -> Option<SharedServerTlsStreamIo> {
    let guard = context.lock_transport();
    guard.downstream_tls_io.clone()
}

#[cfg(feature = "tls")]
fn downstream_tls_handshake_pending(context: &SharedProxyVmContext) -> bool {
    context.lock_transport().downstream_tls_server_start.is_some()
}

fn downstream_attached_local_addr(context: &SharedProxyVmContext) -> String {
    context
        .lock_transport()
        .downstream_local_addr
        .clone()
        .unwrap_or_default()
}

fn downstream_attached_peer_addr(context: &SharedProxyVmContext) -> String {
    context
        .lock_transport()
        .downstream_peer_addr
        .clone()
        .unwrap_or_default()
}

fn mark_attached_downstream_read_eof(context: &SharedProxyVmContext) {
    let mut guard = context.lock_transport();
    guard.downstream_read_eof = true;
}

fn clear_attached_downstream_read_eof(context: &SharedProxyVmContext) {
    let mut guard = context.lock_transport();
    guard.downstream_read_eof = false;
}

fn attached_downstream_eof(context: &SharedProxyVmContext) -> bool {
    context.lock_transport().downstream_read_eof
}

fn mark_attached_downstream_failed(context: &SharedProxyVmContext, message: impl Into<String>) {
    let message = message.into();
    let mut guard = context.lock_transport();
    guard.tcp_dag.downstream.mark_failed(message);
    #[cfg(feature = "tls")]
    {
        if guard.tls_dag.downstream.is_present() || guard.downstream_tls_server_start.is_some() {
            guard.tls_dag.downstream.mark_failed();
        }
        guard.downstream_tls_server_start = None;
        guard.downstream_tls_io = None;
    }
    guard.downstream_tcp_io = None;
}

async fn read_attached_downstream_tcp(
    context: &SharedProxyVmContext,
    io: SharedTcpStreamIo,
    max_bytes: usize,
) -> Result<Vec<u8>, VmError> {
    let mut guard = io.lock().await;
    let stream_io = guard.as_mut().ok_or_else(|| {
        VmError::HostError("downstream tcp transport is unavailable".to_string())
    })?;
    let mut buffer = vec![0u8; max_bytes];
    let read = stream_io
        .read(&mut buffer)
        .await
        .map_err(|err| VmError::HostError(format!("downstream tcp read failed: {err}")))?;
    if read == 0 {
        mark_attached_downstream_read_eof(context);
        Ok(Vec::new())
    } else {
        clear_attached_downstream_read_eof(context);
        buffer.truncate(read);
        Ok(buffer)
    }
}

#[cfg(feature = "tls")]
async fn read_attached_downstream_tls(
    context: &SharedProxyVmContext,
    io: SharedServerTlsStreamIo,
    max_bytes: usize,
) -> Result<Vec<u8>, VmError> {
    let mut guard = io.lock().await;
    let stream_io = guard.as_mut().ok_or_else(|| {
        VmError::HostError("downstream tls plaintext transport is unavailable".to_string())
    })?;
    let mut buffer = vec![0u8; max_bytes];
    let read = stream_io
        .read(&mut buffer)
        .await
        .map_err(|err| VmError::HostError(format!("downstream tls read failed: {err}")))?;
    if read == 0 {
        mark_attached_downstream_read_eof(context);
        Ok(Vec::new())
    } else {
        clear_attached_downstream_read_eof(context);
        buffer.truncate(read);
        Ok(buffer)
    }
}

async fn write_attached_downstream_tcp(
    io: SharedTcpStreamIo,
    bytes: &[u8],
) -> Result<(), VmError> {
    let mut guard = io.lock().await;
    let stream_io = guard.as_mut().ok_or_else(|| {
        VmError::HostError("downstream tcp transport is unavailable".to_string())
    })?;
    stream_io
        .write_all(bytes)
        .await
        .map_err(|err| VmError::HostError(format!("downstream tcp write failed: {err}")))?;
    stream_io
        .flush()
        .await
        .map_err(|err| VmError::HostError(format!("downstream tcp flush failed: {err}")))?;
    Ok(())
}

#[cfg(feature = "tls")]
async fn write_attached_downstream_tls(
    io: SharedServerTlsStreamIo,
    bytes: &[u8],
) -> Result<(), VmError> {
    let mut guard = io.lock().await;
    let stream_io = guard.as_mut().ok_or_else(|| {
        VmError::HostError("downstream tls plaintext transport is unavailable".to_string())
    })?;
    stream_io
        .write_all(bytes)
        .await
        .map_err(|err| VmError::HostError(format!("downstream tls write failed: {err}")))?;
    stream_io
        .flush()
        .await
        .map_err(|err| VmError::HostError(format!("downstream tls flush failed: {err}")))?;
    Ok(())
}

fn store_connected_dynamic_tcp_stream(
    context: &SharedProxyVmContext,
    handle: i64,
    io: SharedTcpStreamIo,
    local_addr: String,
    peer_addr: String,
) {
    let mut guard = context.lock_transport();
    if let Some(state) = guard.tcp_streams.get_mut(&handle) {
        state.mark_connected(local_addr, peer_addr);
    }
    guard.tcp_stream_ios.insert(handle, io);
}

fn mark_dynamic_tcp_stream_failed(
    context: &SharedProxyVmContext,
    handle: i64,
    message: impl Into<String>,
) {
    let message = message.into();
    let mut guard = context.lock_transport();
    if let Some(state) = guard.tcp_streams.get_mut(&handle) {
        state.mark_failed(message);
    }
    guard.tcp_stream_ios.remove(&handle);
}

fn mark_dynamic_tcp_stream_eof(context: &SharedProxyVmContext, handle: i64) {
    let mut guard = context.lock_transport();
    if let Some(state) = guard.tcp_streams.get_mut(&handle) {
        state.mark_read_eof();
    }
}

fn clear_dynamic_tcp_stream_eof(context: &SharedProxyVmContext, handle: i64) {
    let mut guard = context.lock_transport();
    if let Some(state) = guard.tcp_streams.get_mut(&handle) {
        state.clear_read_eof();
    }
}

fn note_stream_read(context: &SharedProxyVmContext, stream: TcpStreamHandle) {
    match stream {
        TcpStreamHandle::Reserved(TcpStreamRef::Downstream) => {
            context.lock_transport().tcp_dag.downstream.note_read()
        }
        TcpStreamHandle::Reserved(TcpStreamRef::DefaultUpstream) => context
            .lock_transport()
            .tcp_dag
            .default_upstream
            .note_read(),
        TcpStreamHandle::OutboundExchange(handle) => {
            let mut exchanges = context.lock_exchanges();
            let exchange = exchanges
                .exchanges
                .get_mut(&handle)
                .expect("exchange handle should exist while stream is in use");
            exchange.transport.tcp_flow.note_read();
        }
        TcpStreamHandle::Dynamic(_) => {}
    }
}

fn note_stream_write(context: &SharedProxyVmContext, stream: TcpStreamHandle) {
    match stream {
        TcpStreamHandle::Reserved(TcpStreamRef::Downstream) => {
            context.lock_transport().tcp_dag.downstream.note_write()
        }
        TcpStreamHandle::Reserved(TcpStreamRef::DefaultUpstream) => context
            .lock_transport()
            .tcp_dag
            .default_upstream
            .note_write(),
        TcpStreamHandle::OutboundExchange(handle) => {
            let mut exchanges = context.lock_exchanges();
            let exchange = exchanges
                .exchanges
                .get_mut(&handle)
                .expect("exchange handle should exist while stream is in use");
            exchange.transport.tcp_flow.note_write();
        }
        TcpStreamHandle::Dynamic(_) => {}
    }
}

async fn ensure_dynamic_tcp_stream_connected(
    context: &SharedProxyVmContext,
    stream: i64,
) -> Result<SharedTcpStreamIo, VmError> {
    let TcpStreamHandle::Dynamic(handle) = decode_stream(context, stream)? else {
        return Err(mutable_dynamic_tcp_stream_only());
    };
    if let Some(io) = active_dynamic_tcp_stream_io(context, handle) {
        return Ok(io);
    }

    let state = dynamic_tcp_socket_state(context, handle)?;
    match state.phase() {
        TcpSocketPhase::UpgradedTls => {
            return Err(VmError::HostError(format!(
                "dynamic tcp stream handle {handle} is already owned by the tls DAG",
            )));
        }
        TcpSocketPhase::AttachedHttp => {
            return Err(VmError::HostError(format!(
                "dynamic tcp stream handle {handle} is already attached to an http exchange",
            )));
        }
        TcpSocketPhase::AttachedProxy => {
            return Err(VmError::HostError(format!(
                "dynamic tcp stream handle {handle} is already attached to a proxy tunnel",
            )));
        }
        TcpSocketPhase::Closed => {
            return Err(VmError::HostError(format!(
                "dynamic tcp stream handle {handle} is closed",
            )));
        }
        TcpSocketPhase::Failed => {
            return Err(VmError::HostError(format!(
                "dynamic tcp stream handle {handle} is failed",
            )));
        }
        TcpSocketPhase::Inactive
        | TcpSocketPhase::Bound
        | TcpSocketPhase::Configured
        | TcpSocketPhase::Connected => {}
    }

    let target = state.target().ok_or_else(|| {
        VmError::HostError(format!(
            "dynamic tcp stream handle {handle} has no target; call tcp::stream::set_target first",
        ))
    })?;
    let bind_addr = state.bind_address().map(str::to_string);

    let resolved = lookup_host(target).await.map_err(|err| {
        VmError::HostError(format!("failed to resolve tcp target '{target}': {err}"))
    })?;

    let mut last_error = None;
    for peer_addr in resolved {
        let socket = if peer_addr.is_ipv4() {
            TcpSocket::new_v4()
        } else {
            TcpSocket::new_v6()
        }
        .map_err(|err| VmError::HostError(format!("failed to create tcp socket: {err}")))?;

        if let Some(bind_addr) = bind_addr.as_deref() {
            let bind_addr = bind_addr.parse::<std::net::SocketAddr>().map_err(|err| {
                VmError::HostError(format!("invalid tcp bind address '{bind_addr}': {err}"))
            })?;
            if bind_addr.is_ipv4() != peer_addr.is_ipv4() {
                last_error = Some(format!(
                    "tcp bind address family does not match resolved peer {peer_addr}",
                ));
                continue;
            }
            socket.bind(bind_addr).map_err(|err| {
                VmError::HostError(format!("failed to bind tcp socket to {bind_addr}: {err}"))
            })?;
        }

        match socket.connect(peer_addr).await {
            Ok(stream_io) => {
                let local_addr = stream_io
                    .local_addr()
                    .map_err(|err| {
                        VmError::HostError(format!("failed to read local tcp addr: {err}"))
                    })?
                    .to_string();
                let peer_addr = stream_io
                    .peer_addr()
                    .map_err(|err| {
                        VmError::HostError(format!("failed to read peer tcp addr: {err}"))
                    })?
                    .to_string();
                let io = Arc::new(tokio::sync::Mutex::new(Some(stream_io)));
                store_connected_dynamic_tcp_stream(
                    context,
                    handle,
                    io.clone(),
                    local_addr,
                    peer_addr,
                );
                return Ok(io);
            }
            Err(err) => {
                last_error = Some(err.to_string());
            }
        }
    }

    let message =
        last_error.unwrap_or_else(|| "no resolved tcp target addresses were usable".to_string());
    mark_dynamic_tcp_stream_failed(context, handle, &message);
    Err(VmError::HostError(format!(
        "failed to connect tcp stream handle {handle}: {message}",
    )))
}

fn tcp_flow_phase_label(flow: &super::state::TcpFlowState) -> &'static str {
    flow.phase_label()
}

fn append_downstream_response(context: &SharedProxyVmContext, text: &str) {
    append_response_output_body_bytes(context, text.as_bytes());
}

/// Returns the TCP stream handle for the current downstream flow.
#[pd_edge_host_function(name = tcp::stream::DOWNSTREAM.name, scope = transport)]
async fn stream_downstream(
    _vm: &mut Vm,
    _context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::Int(
        TcpStreamRef::Downstream.handle(),
    )]))
}

/// Returns the default upstream handle for the TCP stream.
#[pd_edge_host_function(name = tcp::stream::DEFAULT_UPSTREAM.name, scope = transport)]
async fn stream_default_upstream(
    _vm: &mut Vm,
    _context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::Int(
        TcpStreamRef::DefaultUpstream.handle(),
    )]))
}

/// Allocates a TCP stream handle.
#[pd_edge_host_function(name = tcp::stream::NEW.name, scope = transport)]
async fn stream_new(_vm: &mut Vm, context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    let handle = allocate_tcp_stream_handle(&context)?;
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

/// Returns whether the TCP stream handle is present.
#[pd_edge_host_function(name = tcp::stream::IS_PRESENT.name, scope = transport)]
async fn stream_is_present(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    stream: i64,
) -> Result<CallOutcome, VmError> {
    let present = match decode_stream(&context, stream)? {
        TcpStreamHandle::Reserved(_) | TcpStreamHandle::OutboundExchange(_) => true,
        TcpStreamHandle::Dynamic(handle) => {
            dynamic_tcp_socket_state(&context, handle)?.is_present()
        }
    };
    Ok(CallOutcome::Return(vec![Value::Bool(present)]))
}

/// Binds the TCP stream to a local address.
#[pd_edge_host_function(name = tcp::stream::BIND.name, scope = transport)]
async fn stream_bind(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    stream: i64,
    local_addr: String,
) -> Result<CallOutcome, VmError> {
    with_mutable_dynamic_tcp_socket_state(&context, stream, |state, io| {
        state.set_bind_address(local_addr);
        *io = None;
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the target endpoint for the TCP stream.
#[pd_edge_host_function(name = tcp::stream::SET_TARGET.name, scope = transport)]
async fn stream_set_target(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    stream: i64,
    target: String,
) -> Result<CallOutcome, VmError> {
    let target = normalize_tcp_target(&target)?;
    with_mutable_dynamic_tcp_socket_state(&context, stream, |state, io| {
        state.set_target(target);
        *io = None;
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Attempts to connect the TCP stream.
#[pd_edge_host_function(name = tcp::stream::CONNECT.name, scope = transport)]
async fn stream_connect(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    stream: i64,
) -> Result<CallOutcome, VmError> {
    ensure_dynamic_tcp_stream_connected(&context, stream).await?;
    Ok(CallOutcome::Return(vec![Value::Bool(true)]))
}

/// Reports the current lifecycle phase for a TCP stream handle.
#[pd_edge_host_function(name = tcp::stream::GET_PHASE.name, scope = transport)]
async fn stream_get_phase(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    stream: i64,
) -> Result<CallOutcome, VmError> {
    let phase = match decode_stream(&context, stream)? {
        TcpStreamHandle::Reserved(TcpStreamRef::Downstream) => {
            let guard = context.lock_transport();
            tcp_flow_phase_label(&guard.tcp_dag.downstream)
        }
        TcpStreamHandle::Reserved(TcpStreamRef::DefaultUpstream) => {
            let guard = context.lock_transport();
            tcp_flow_phase_label(&guard.tcp_dag.default_upstream)
        }
        TcpStreamHandle::OutboundExchange(handle) => {
            let guard = context.lock_exchanges();
            let exchange = guard
                .exchanges
                .get(&handle)
                .expect("exchange should exist while stream handle is in use");
            tcp_flow_phase_label(&exchange.transport.tcp_flow)
        }
        TcpStreamHandle::Dynamic(handle) => {
            dynamic_tcp_socket_state(&context, handle)?.phase().as_str()
        }
    };
    Ok(CallOutcome::Return(vec![Value::string(phase)]))
}

/// Returns the local address for the TCP stream.
#[pd_edge_host_function(name = tcp::stream::GET_LOCAL_ADDR.name, scope = transport)]
async fn stream_get_local_addr(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    stream: i64,
) -> Result<CallOutcome, VmError> {
    let local_addr = match decode_stream(&context, stream)? {
        TcpStreamHandle::Reserved(TcpStreamRef::Downstream) => {
            downstream_attached_local_addr(&context)
        }
        TcpStreamHandle::Dynamic(handle) => {
            dynamic_tcp_socket_state(&context, handle)?
                .local_address()
                .to_string()
        }
        _ => String::new(),
    };
    Ok(CallOutcome::Return(vec![Value::string(local_addr)]))
}

/// Returns the peer address for the TCP stream.
#[pd_edge_host_function(name = tcp::stream::GET_PEER_ADDR.name, scope = transport)]
async fn stream_get_peer_addr(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    stream: i64,
) -> Result<CallOutcome, VmError> {
    let peer_addr = match decode_stream(&context, stream)? {
        TcpStreamHandle::Reserved(TcpStreamRef::Downstream) => {
            downstream_attached_peer_addr(&context)
        }
        TcpStreamHandle::Reserved(TcpStreamRef::DefaultUpstream) => {
            let guard = context.lock_transport();
            guard.tls_dag.default_upstream.peer_name().to_string()
        }
        TcpStreamHandle::OutboundExchange(handle) => {
            let guard = context.lock_exchanges();
            let exchange = guard
                .exchanges
                .get(&handle)
                .expect("exchange should exist while stream handle is in use");
            exchange.transport.tls_flow.peer_name().to_string()
        }
        TcpStreamHandle::Dynamic(handle) => dynamic_tcp_socket_state(&context, handle)?
            .peer_address()
            .to_string(),
    };
    Ok(CallOutcome::Return(vec![Value::string(peer_addr)]))
}

/// Reads text from the TCP stream.
#[pd_edge_host_function(name = tcp::stream::READ.name, scope = transport)]
async fn stream_read(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    stream: i64,
    max_bytes: i64,
) -> Result<CallOutcome, VmError> {
    let stream = decode_stream(&context, stream)?;
    let max_bytes = decode_chunk_size(max_bytes)?;
    note_stream_read(&context, stream);

    let chunk = match stream {
        TcpStreamHandle::Reserved(TcpStreamRef::Downstream) => {
            #[cfg(feature = "tls")]
            if let Some(io) = active_downstream_tls_io(&context) {
                match read_attached_downstream_tls(&context, io, max_bytes).await {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        mark_attached_downstream_failed(&context, err.to_string());
                        return Err(err);
                    }
                }
            } else if let Some(io) = active_downstream_tcp_io(&context) {
                match read_attached_downstream_tcp(&context, io, max_bytes).await {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        mark_attached_downstream_failed(&context, err.to_string());
                        return Err(err);
                    }
                }
            } else if downstream_tls_handshake_pending(&context) {
                return Err(VmError::HostError(
                    "downstream tcp stream is pending tls handshake; call tls::session::handshake or close the stream".to_string(),
                ));
            } else {
                read_request_body_next_chunk(&context, max_bytes).await?
            }
            #[cfg(not(feature = "tls"))]
            if let Some(io) = active_downstream_tcp_io(&context) {
                match read_attached_downstream_tcp(&context, io, max_bytes).await {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        mark_attached_downstream_failed(&context, err.to_string());
                        return Err(err);
                    }
                }
            } else {
                read_request_body_next_chunk(&context, max_bytes).await?
            }
        }
        TcpStreamHandle::Reserved(TcpStreamRef::DefaultUpstream) => {
            read_upstream_response_next_chunk(&context, max_bytes).await?
        }
        TcpStreamHandle::OutboundExchange(handle) => {
            read_outbound_exchange_response_next_chunk(&context, handle, max_bytes).await?
        }
        TcpStreamHandle::Dynamic(handle) => {
            let io = ensure_dynamic_tcp_stream_connected(&context, handle).await?;
            let mut guard = io.lock().await;
            let stream_io = guard.as_mut().ok_or_else(|| {
                VmError::HostError(format!(
                    "dynamic tcp stream handle {handle} has no active transport",
                ))
            })?;
            let mut buffer = vec![0u8; max_bytes];
            let read = stream_io
                .read(&mut buffer)
                .await
                .map_err(|err| VmError::HostError(format!("tcp read failed: {err}")))?;
            if read == 0 {
                mark_dynamic_tcp_stream_eof(&context, handle);
                Vec::new()
            } else {
                clear_dynamic_tcp_stream_eof(&context, handle);
                buffer.truncate(read);
                buffer
            }
        }
    };
    Ok(CallOutcome::Return(vec![Value::string(
        String::from_utf8_lossy(&chunk).into_owned(),
    )]))
}

/// Writes text to the TCP stream.
#[pd_edge_host_function(name = tcp::stream::WRITE.name, scope = transport)]
async fn stream_write(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    stream: i64,
    text: String,
) -> Result<CallOutcome, VmError> {
    let stream_handle = decode_stream(&context, stream)?;
    note_stream_write(&context, stream_handle);
    match stream_handle {
        TcpStreamHandle::Reserved(TcpStreamRef::Downstream) => {
            #[cfg(feature = "tls")]
            if let Some(io) = active_downstream_tls_io(&context) {
                if let Err(err) = write_attached_downstream_tls(io, text.as_bytes()).await
                {
                    mark_attached_downstream_failed(&context, err.to_string());
                    return Err(err);
                }
            } else if let Some(io) = active_downstream_tcp_io(&context) {
                if let Err(err) = write_attached_downstream_tcp(io, text.as_bytes()).await
                {
                    mark_attached_downstream_failed(&context, err.to_string());
                    return Err(err);
                }
            } else if downstream_tls_handshake_pending(&context) {
                return Err(VmError::HostError(
                    "downstream tcp stream is pending tls handshake; call tls::session::handshake before writing plaintext".to_string(),
                ));
            } else {
                append_downstream_response(&context, &text)
            }
            #[cfg(not(feature = "tls"))]
            if let Some(io) = active_downstream_tcp_io(&context) {
                if let Err(err) = write_attached_downstream_tcp(io, text.as_bytes()).await
                {
                    mark_attached_downstream_failed(&context, err.to_string());
                    return Err(err);
                }
            } else {
                append_downstream_response(&context, &text)
            }
        }
        TcpStreamHandle::Reserved(TcpStreamRef::DefaultUpstream) => {
            append_outbound_exchange_body(&context, TcpStreamRef::DefaultUpstream.handle(), &text)?
        }
        TcpStreamHandle::OutboundExchange(handle) => {
            append_outbound_exchange_body(&context, handle, &text)?
        }
        TcpStreamHandle::Dynamic(handle) => {
            let io = ensure_dynamic_tcp_stream_connected(&context, handle).await?;
            let mut guard = io.lock().await;
            let stream_io = guard.as_mut().ok_or_else(|| {
                VmError::HostError(format!(
                    "dynamic tcp stream handle {handle} has no active transport",
                ))
            })?;
            stream_io
                .write_all(text.as_bytes())
                .await
                .map_err(|err| VmError::HostError(format!("tcp write failed: {err}")))?;
            stream_io
                .flush()
                .await
                .map_err(|err| VmError::HostError(format!("tcp flush failed: {err}")))?;
            clear_dynamic_tcp_stream_eof(&context, handle);
        }
    }
    Ok(CallOutcome::Return(vec![Value::Int(text.len() as i64)]))
}

/// Returns whether the TCP stream has reached EOF.
#[pd_edge_host_function(name = tcp::stream::EOF.name, scope = transport)]
async fn stream_eof(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    stream: i64,
) -> Result<CallOutcome, VmError> {
    let eof = match decode_stream(&context, stream)? {
        TcpStreamHandle::Reserved(TcpStreamRef::Downstream) => {
            if active_downstream_tcp_io(&context).is_some()
                || {
                    #[cfg(feature = "tls")]
                    {
                        active_downstream_tls_io(&context).is_some()
                            || downstream_tls_handshake_pending(&context)
                    }
                    #[cfg(not(feature = "tls"))]
                    {
                        false
                    }
                }
            {
                attached_downstream_eof(&context)
            } else {
                request_body_eof(&context).await?
            }
        }
        TcpStreamHandle::Reserved(TcpStreamRef::DefaultUpstream) => {
            upstream_response_eof(&context).await?
        }
        TcpStreamHandle::OutboundExchange(handle) => {
            outbound_exchange_response_eof(&context, handle).await?
        }
        TcpStreamHandle::Dynamic(handle) => {
            let state = dynamic_tcp_socket_state(&context, handle)?;
            matches!(state.phase(), TcpSocketPhase::Closed) || state.read_eof()
        }
    };
    Ok(CallOutcome::Return(vec![Value::Bool(eof)]))
}

/// Closes the TCP stream.
#[pd_edge_host_function(name = tcp::stream::CLOSE.name, scope = transport)]
async fn stream_close(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    stream: i64,
) -> Result<CallOutcome, VmError> {
    match decode_stream(&context, stream)? {
        TcpStreamHandle::Reserved(TcpStreamRef::Downstream) => {
            #[cfg(feature = "tls")]
            let (tcp_io, tls_io) = {
                let mut guard = context.lock_transport();
                let tcp_io = guard.downstream_tcp_io.take();
                let tls_io = guard.downstream_tls_io.take();
                guard.downstream_tls_server_start = None;
                if guard.tls_dag.downstream.is_present() {
                    guard.tls_dag.downstream.mark_closed();
                }
                guard.downstream_read_eof = true;
                guard.tcp_dag.downstream.mark_closed();
                (tcp_io, tls_io)
            };
            #[cfg(not(feature = "tls"))]
            let tcp_io = {
                let mut guard = context.lock_transport();
                let tcp_io = guard.downstream_tcp_io.take();
                guard.downstream_read_eof = true;
                guard.tcp_dag.downstream.mark_closed();
                tcp_io
            };
            #[cfg(feature = "tls")]
            {
                if let Some(io) = tls_io {
                    let mut guard = io.lock().await;
                    if let Some(mut stream_io) = guard.take() {
                        let _ = stream_io.shutdown().await;
                    }
                }
            }
            if let Some(io) = tcp_io {
                let mut guard = io.lock().await;
                if let Some(mut stream_io) = guard.take() {
                    let _ = stream_io.shutdown().await;
                }
            }
        }
        TcpStreamHandle::Reserved(_) => {
            return Err(mutable_dynamic_tcp_stream_only());
        }
        TcpStreamHandle::Dynamic(_) => {
            with_mutable_dynamic_tcp_socket_state(&context, stream, |state, io| {
                *io = None;
                state.mark_closed();
                Ok(())
            })?;
        }
        TcpStreamHandle::OutboundExchange(_) => return Err(mutable_dynamic_tcp_stream_only()),
    }
    Ok(CallOutcome::Return(vec![]))
}
