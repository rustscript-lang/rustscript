use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use edge_abi::symbols::udp;
use pd_edge_host_function::pd_edge_host_function;
use tokio::net::{UdpSocket, lookup_host};
use url::Url;
use vm::{CallOutcome, Value, Vm, VmError};

use super::super::SharedProxyVmContext;
use super::super::http::{
    allocate_udp_socket_handle, default_upstream_udp_socket_handle, udp_socket_exists,
};
use super::state::{SharedUdpSocketIo, UdpSocketRef, UdpSocketState, decode_udp_socket_handle};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UdpSocketHandle {
    Downstream,
    DefaultUpstream,
    Dynamic(i64),
}

fn decode_socket(context: &SharedProxyVmContext, socket: i64) -> Result<UdpSocketHandle, VmError> {
    if let Some(reserved) = decode_udp_socket_handle(socket) {
        return Ok(match reserved {
            UdpSocketRef::Downstream => UdpSocketHandle::Downstream,
            UdpSocketRef::DefaultUpstream => UdpSocketHandle::DefaultUpstream,
        });
    }
    if udp_socket_exists(context, socket) {
        return Ok(UdpSocketHandle::Dynamic(socket));
    }
    Err(VmError::HostError(format!(
        "invalid udp socket handle {socket}; reserved handles are 0 (downstream), 1 (default upstream), and allocated handles start at 2",
    )))
}

fn udp_socket_operation_on_downstream() -> VmError {
    VmError::HostError(
        "downstream UDP sockets are unavailable in the current one-shot HTTP runtime".to_string(),
    )
}

fn parse_positive_chunk_size(max_bytes: i64) -> Result<usize, VmError> {
    if max_bytes <= 0 {
        return Err(VmError::HostError(format!(
            "udp recv size must be positive, got {max_bytes}",
        )));
    }
    usize::try_from(max_bytes).map_err(|_| {
        VmError::HostError(format!(
            "udp recv size is too large for this runtime: {max_bytes}",
        ))
    })
}

fn normalize_udp_target(value: &str) -> Result<String, VmError> {
    if value.is_empty() || value.chars().any(|ch| ch.is_whitespace()) {
        return Err(VmError::HostError(format!(
            "udp target must be host:port or udp://host:port, got '{value}'",
        )));
    }
    if let Ok(url) = Url::parse(value) {
        if !url.scheme().eq_ignore_ascii_case("udp") {
            return Err(VmError::HostError(format!(
                "udp target scheme must be udp, got '{}'",
                url.scheme()
            )));
        }
        let host = url.host_str().ok_or_else(|| {
            VmError::HostError(format!("udp target is missing a host: '{value}'"))
        })?;
        let port = url.port().ok_or_else(|| {
            VmError::HostError(format!("udp target is missing a port: '{value}'"))
        })?;
        if !url.path().is_empty() && url.path() != "/" {
            return Err(VmError::HostError(format!(
                "udp target must not contain a path, got '{value}'",
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
        "udp target must be host:port or udp://host:port, got '{value}'",
    )))
}

fn socket_state(context: &SharedProxyVmContext, socket: UdpSocketHandle) -> UdpSocketState {
    let guard = context.lock().expect("vm context lock poisoned");
    match socket {
        UdpSocketHandle::Downstream => UdpSocketState::default(),
        UdpSocketHandle::DefaultUpstream => guard.default_upstream_udp_socket.clone(),
        UdpSocketHandle::Dynamic(handle) => guard
            .udp_sockets
            .get(&handle)
            .expect("udp socket should exist while handle is in use")
            .clone(),
    }
}

fn with_mutable_udp_socket_state<T>(
    context: &SharedProxyVmContext,
    socket: i64,
    mutate: impl FnOnce(&mut UdpSocketState, &mut Option<SharedUdpSocketIo>) -> Result<T, VmError>,
) -> Result<T, VmError> {
    let handle = decode_socket(context, socket)?;
    let mut guard = context.lock().expect("vm context lock poisoned");
    match handle {
        UdpSocketHandle::Downstream => Err(udp_socket_operation_on_downstream()),
        UdpSocketHandle::DefaultUpstream => {
            let crate::abi_impl::ProxyVmContext {
                default_upstream_udp_socket,
                default_upstream_udp_io,
                ..
            } = &mut *guard;
            mutate(default_upstream_udp_socket, default_upstream_udp_io)
        }
        UdpSocketHandle::Dynamic(handle) => {
            let mut io_slot = guard.udp_socket_ios.remove(&handle);
            let state = guard
                .udp_sockets
                .get_mut(&handle)
                .expect("udp socket should exist while handle is in use");
            let result = mutate(state, &mut io_slot);
            if let Some(io) = io_slot {
                guard.udp_socket_ios.insert(handle, io);
            } else {
                guard.udp_socket_ios.remove(&handle);
            }
            result
        }
    }
}

fn clear_udp_socket_io(context: &SharedProxyVmContext, socket: UdpSocketHandle) {
    let mut guard = context.lock().expect("vm context lock poisoned");
    match socket {
        UdpSocketHandle::Downstream => {}
        UdpSocketHandle::DefaultUpstream => {
            guard.default_upstream_udp_io = None;
        }
        UdpSocketHandle::Dynamic(handle) => {
            guard.udp_socket_ios.remove(&handle);
        }
    }
}

fn active_udp_socket_io(
    context: &SharedProxyVmContext,
    socket: UdpSocketHandle,
) -> Option<SharedUdpSocketIo> {
    let guard = context.lock().expect("vm context lock poisoned");
    match socket {
        UdpSocketHandle::Downstream => None,
        UdpSocketHandle::DefaultUpstream => guard.default_upstream_udp_io.clone(),
        UdpSocketHandle::Dynamic(handle) => guard.udp_socket_ios.get(&handle).cloned(),
    }
}

fn store_connected_udp_socket(
    context: &SharedProxyVmContext,
    socket: UdpSocketHandle,
    io: SharedUdpSocketIo,
    local_addr: String,
    peer_addr: String,
) {
    let mut guard = context.lock().expect("vm context lock poisoned");
    match socket {
        UdpSocketHandle::Downstream => {}
        UdpSocketHandle::DefaultUpstream => {
            guard
                .default_upstream_udp_socket
                .mark_connected(local_addr, peer_addr);
            guard.default_upstream_udp_io = Some(io);
        }
        UdpSocketHandle::Dynamic(handle) => {
            if let Some(state) = guard.udp_sockets.get_mut(&handle) {
                state.mark_connected(local_addr, peer_addr);
            }
            guard.udp_socket_ios.insert(handle, io);
        }
    }
}

fn store_failed_udp_socket(
    context: &SharedProxyVmContext,
    socket: UdpSocketHandle,
    message: impl Into<String>,
) {
    let message = message.into();
    let mut guard = context.lock().expect("vm context lock poisoned");
    match socket {
        UdpSocketHandle::Downstream => {}
        UdpSocketHandle::DefaultUpstream => {
            guard.default_upstream_udp_socket.mark_failed(message);
            guard.default_upstream_udp_io = None;
        }
        UdpSocketHandle::Dynamic(handle) => {
            if let Some(state) = guard.udp_sockets.get_mut(&handle) {
                state.mark_failed(message);
            }
            guard.udp_socket_ios.remove(&handle);
        }
    }
}

async fn ensure_udp_socket_connected(
    context: &SharedProxyVmContext,
    socket: i64,
) -> Result<SharedUdpSocketIo, VmError> {
    let handle = decode_socket(context, socket)?;
    if let Some(io) = active_udp_socket_io(context, handle) {
        return Ok(io);
    }

    let state = socket_state(context, handle);
    let target = state.target().ok_or_else(|| {
        VmError::HostError("udp target is unavailable before udp::socket::set_target".to_string())
    })?;
    let peer_addr = lookup_host(target)
        .await
        .map_err(|err| {
            VmError::HostError(format!("failed to resolve udp target '{target}': {err}"))
        })?
        .next()
        .ok_or_else(|| {
            VmError::HostError(format!("udp target '{target}' resolved to no addresses"))
        })?;

    let bind_addr = match state.bind_address() {
        Some(address) => address.to_string(),
        None if peer_addr.is_ipv4() => "0.0.0.0:0".to_string(),
        None => "[::]:0".to_string(),
    };
    let socket_io = UdpSocket::bind(&bind_addr).await.map_err(|err| {
        VmError::HostError(format!("failed to bind udp socket '{bind_addr}': {err}"))
    })?;
    socket_io.connect(peer_addr).await.map_err(|err| {
        VmError::HostError(format!(
            "failed to connect udp socket to {peer_addr}: {err}"
        ))
    })?;
    let local_addr = socket_io
        .local_addr()
        .map_err(|err| VmError::HostError(format!("failed to read udp local address: {err}")))?
        .to_string();
    let io = Arc::new(tokio::sync::Mutex::new(socket_io));
    store_connected_udp_socket(
        context,
        handle,
        io.clone(),
        local_addr,
        peer_addr.to_string(),
    );
    Ok(io)
}

#[pd_edge_host_function(name = udp::socket::NEW.name, scope = transport)]
async fn socket_new(_vm: &mut Vm, context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    let handle = allocate_udp_socket_handle(&context)?;
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

#[pd_edge_host_function(name = udp::socket::DOWNSTREAM.name, scope = transport)]
async fn socket_downstream(
    _vm: &mut Vm,
    _context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::Int(
        UdpSocketRef::Downstream.handle(),
    )]))
}

#[pd_edge_host_function(name = udp::socket::DEFAULT_UPSTREAM.name, scope = transport)]
async fn socket_default_upstream(
    _vm: &mut Vm,
    _context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::Int(
        default_upstream_udp_socket_handle(),
    )]))
}

#[pd_edge_host_function(name = udp::socket::IS_PRESENT.name, scope = transport)]
async fn socket_is_present(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    socket: i64,
) -> Result<CallOutcome, VmError> {
    let handle = decode_socket(&context, socket)?;
    let present = match handle {
        UdpSocketHandle::Downstream => false,
        _ => socket_state(&context, handle).is_present(),
    };
    Ok(CallOutcome::Return(vec![Value::Bool(present)]))
}

#[pd_edge_host_function(name = udp::socket::BIND.name, scope = transport)]
async fn socket_bind(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    socket: i64,
    local_addr: String,
) -> Result<CallOutcome, VmError> {
    decode_socket(&context, socket)?;
    with_mutable_udp_socket_state(&context, socket, |state, io| {
        *io = None;
        state.set_bind_address(local_addr);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = udp::socket::SET_TARGET.name, scope = transport)]
async fn socket_set_target(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    socket: i64,
    target: String,
) -> Result<CallOutcome, VmError> {
    let normalized = normalize_udp_target(&target)?;
    decode_socket(&context, socket)?;
    with_mutable_udp_socket_state(&context, socket, |state, io| {
        *io = None;
        state.set_target(normalized);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = udp::socket::CONNECT.name, scope = transport)]
async fn socket_connect(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    socket: i64,
) -> Result<CallOutcome, VmError> {
    ensure_udp_socket_connected(&context, socket)
        .await
        .map_err(|err| {
            store_failed_udp_socket(
                &context,
                decode_socket(&context, socket).unwrap_or(UdpSocketHandle::Downstream),
                err.to_string(),
            );
            err
        })?;
    Ok(CallOutcome::Return(vec![Value::Bool(true)]))
}

#[pd_edge_host_function(name = udp::socket::GET_PHASE.name, scope = transport)]
async fn socket_get_phase(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    socket: i64,
) -> Result<CallOutcome, VmError> {
    let phase = match decode_socket(&context, socket)? {
        UdpSocketHandle::Downstream => "inactive".to_string(),
        handle => socket_state(&context, handle).phase().as_str().to_string(),
    };
    Ok(CallOutcome::Return(vec![Value::string(phase)]))
}

#[pd_edge_host_function(name = udp::socket::GET_LOCAL_ADDR.name, scope = transport)]
async fn socket_get_local_addr(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    socket: i64,
) -> Result<CallOutcome, VmError> {
    let address = match decode_socket(&context, socket)? {
        UdpSocketHandle::Downstream => String::new(),
        handle => socket_state(&context, handle).local_address().to_string(),
    };
    Ok(CallOutcome::Return(vec![Value::string(address)]))
}

#[pd_edge_host_function(name = udp::socket::GET_PEER_ADDR.name, scope = transport)]
async fn socket_get_peer_addr(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    socket: i64,
) -> Result<CallOutcome, VmError> {
    let address = match decode_socket(&context, socket)? {
        UdpSocketHandle::Downstream => String::new(),
        handle => socket_state(&context, handle).peer_address().to_string(),
    };
    Ok(CallOutcome::Return(vec![Value::string(address)]))
}

#[pd_edge_host_function(name = udp::socket::SEND_TEXT.name, scope = transport)]
async fn socket_send_text(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    socket: i64,
    text: String,
) -> Result<CallOutcome, VmError> {
    let io = ensure_udp_socket_connected(&context, socket)
        .await
        .map_err(|err| {
            store_failed_udp_socket(
                &context,
                decode_socket(&context, socket).unwrap_or(UdpSocketHandle::Downstream),
                err.to_string(),
            );
            err
        })?;
    let io = io.lock().await;
    let sent = io
        .send(text.as_bytes())
        .await
        .map_err(|err| VmError::HostError(format!("failed to send udp datagram: {err}")))?;
    Ok(CallOutcome::Return(vec![Value::Int(sent as i64)]))
}

#[pd_edge_host_function(name = udp::socket::RECV_TEXT.name, scope = transport)]
async fn socket_recv_text(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    socket: i64,
    max_bytes: i64,
) -> Result<CallOutcome, VmError> {
    let max_bytes = parse_positive_chunk_size(max_bytes)?;
    let io = ensure_udp_socket_connected(&context, socket).await?;
    let mut buffer = vec![0u8; max_bytes];
    let received = {
        let io = io.lock().await;
        io.recv(&mut buffer)
            .await
            .map_err(|err| VmError::HostError(format!("failed to receive udp datagram: {err}")))?
    };
    buffer.truncate(received);
    Ok(CallOutcome::Return(vec![Value::string(
        String::from_utf8_lossy(&buffer).into_owned(),
    )]))
}

#[pd_edge_host_function(name = udp::socket::SEND_BINARY_BASE64.name, scope = transport)]
async fn socket_send_binary_base64(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    socket: i64,
    payload: String,
) -> Result<CallOutcome, VmError> {
    let bytes = STANDARD.decode(payload).map_err(|err| {
        VmError::HostError(format!("udp binary payload must be base64 encoded: {err}",))
    })?;
    let io = ensure_udp_socket_connected(&context, socket).await?;
    let sent = {
        let io = io.lock().await;
        io.send(&bytes)
            .await
            .map_err(|err| VmError::HostError(format!("failed to send udp datagram: {err}")))?
    };
    Ok(CallOutcome::Return(vec![Value::Int(sent as i64)]))
}

#[pd_edge_host_function(name = udp::socket::RECV_BINARY_BASE64.name, scope = transport)]
async fn socket_recv_binary_base64(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    socket: i64,
    max_bytes: i64,
) -> Result<CallOutcome, VmError> {
    let max_bytes = parse_positive_chunk_size(max_bytes)?;
    let io = ensure_udp_socket_connected(&context, socket).await?;
    let mut buffer = vec![0u8; max_bytes];
    let received = {
        let io = io.lock().await;
        io.recv(&mut buffer)
            .await
            .map_err(|err| VmError::HostError(format!("failed to receive udp datagram: {err}")))?
    };
    buffer.truncate(received);
    Ok(CallOutcome::Return(vec![Value::string(
        STANDARD.encode(buffer),
    )]))
}

#[pd_edge_host_function(name = udp::socket::CLOSE.name, scope = transport)]
async fn socket_close(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    socket: i64,
) -> Result<CallOutcome, VmError> {
    let handle = decode_socket(&context, socket)?;
    clear_udp_socket_io(&context, handle);
    with_mutable_udp_socket_state(&context, socket, |state, _io| {
        state.mark_closed();
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}
