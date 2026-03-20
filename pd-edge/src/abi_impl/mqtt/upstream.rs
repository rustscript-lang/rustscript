use std::{sync::Arc, time::Duration};

use axum::http::uri::Authority;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use edge_abi::symbols::mqtt;
use pd_edge_host_function::pd_edge_host_function;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, lookup_host};
use tokio::time::timeout;
#[cfg(feature = "tls")]
use tokio_rustls::{TlsConnector, rustls::pki_types::ServerName};
use vm::{CallOutcome, Value, Vm, VmError};

use super::codec::{
    MqttIncomingPacket, decode_packet, encode_connect_packet, encode_disconnect_packet,
    encode_pingreq_packet, encode_puback_packet, encode_publish_packet, encode_subscribe_packet,
    encode_unsubscribe_packet,
};
use super::model::{
    MqttCarrierAttachment, MqttConnectConfig, MqttConnectionState, MqttEvent, MqttPhase, MqttScheme,
};
use crate::abi_impl::SharedProxyVmContext;
use crate::abi_impl::http::state::{
    allocate_mqtt_connection_handle, allocate_tcp_stream_handle,
    default_upstream_mqtt_connection_handle, mqtt_connection_exists,
};
use crate::abi_impl::transport::SharedTcpStreamIo;
#[cfg(feature = "tls")]
use crate::abi_impl::transport::{SharedTlsStreamIo, TlsFlowState, build_dynamic_client_config};
use crate::abi_impl::value_bytes::value_to_bytes;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MqttHandle {
    DefaultUpstream,
    Dynamic(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KeepAliveReadWait {
    timeout: Duration,
    ping_response_pending: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CarrierReadChunk {
    Data(Vec<u8>),
    TimedOut,
    Closed,
}

fn decode_connection(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<MqttHandle, VmError> {
    if connection == default_upstream_mqtt_connection_handle() {
        return Ok(MqttHandle::DefaultUpstream);
    }
    if mqtt_connection_exists(context, connection) {
        return Ok(MqttHandle::Dynamic(connection));
    }
    Err(VmError::HostError(format!(
        "invalid mqtt connection handle {connection}; reserved handles are 1 (default upstream) and allocated handles start at 2",
    )))
}

fn connection_state(context: &SharedProxyVmContext, handle: MqttHandle) -> MqttConnectionState {
    let guard = context.lock_mqtt();
    match handle {
        MqttHandle::DefaultUpstream => guard.default_upstream_mqtt.clone(),
        MqttHandle::Dynamic(handle) => guard
            .mqtt_connections
            .get(&handle)
            .expect("mqtt connection should exist while handle is in use")
            .clone(),
    }
}

fn with_connection_state_mut<T>(
    context: &SharedProxyVmContext,
    connection: i64,
    mutate: impl FnOnce(&mut MqttConnectionState) -> Result<T, VmError>,
) -> Result<T, VmError> {
    let handle = decode_connection(context, connection)?;
    let mut guard = context.lock_mqtt();
    match handle {
        MqttHandle::DefaultUpstream => mutate(&mut guard.default_upstream_mqtt),
        MqttHandle::Dynamic(handle) => mutate(
            guard
                .mqtt_connections
                .get_mut(&handle)
                .expect("mqtt connection should exist while handle is in use"),
        ),
    }
}

fn with_configurable_connection_state_mut<T>(
    context: &SharedProxyVmContext,
    connection: i64,
    mutate: impl FnOnce(&mut MqttConnectionState) -> Result<T, VmError>,
) -> Result<T, VmError> {
    with_connection_state_mut(context, connection, |state| {
        if matches!(
            state.phase(),
            MqttPhase::CarrierAttached | MqttPhase::ConnectSent | MqttPhase::Open
        ) {
            return Err(VmError::HostError(
                "mqtt connection configuration is read-only after the carrier is attached"
                    .to_string(),
            ));
        }
        mutate(state)
    })
}

fn active_connection_carrier(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<MqttCarrierAttachment, VmError> {
    with_connection_state_mut(context, connection, |state| {
        state.carrier().ok_or_else(|| {
            VmError::HostError("mqtt connection has no attached carrier".to_string())
        })
    })
}

fn note_connection_activity(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<(), VmError> {
    with_connection_state_mut(context, connection, |state| {
        state.note_packet_activity();
        Ok(())
    })
}

fn note_ping_response_received(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<(), VmError> {
    with_connection_state_mut(context, connection, |state| {
        state.note_ping_response_received();
        Ok(())
    })
}

fn note_ping_request_sent(context: &SharedProxyVmContext, connection: i64) -> Result<(), VmError> {
    with_connection_state_mut(context, connection, |state| {
        state.note_ping_request_sent();
        Ok(())
    })
}

fn keep_alive_read_wait(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<Option<KeepAliveReadWait>, VmError> {
    with_connection_state_mut(context, connection, |state| {
        Ok(state
            .next_keep_alive_wait()
            .map(|timeout| KeepAliveReadWait {
                timeout,
                ping_response_pending: state.ping_response_pending(),
            }))
    })
}

fn terminal_connection_message(
    context: &SharedProxyVmContext,
    connection: i64,
    closed_message: &str,
) -> Result<String, VmError> {
    let current = connection_state(context, decode_connection(context, connection)?);
    Ok(match current.phase() {
        MqttPhase::Failed => current
            .failure_message()
            .unwrap_or_else(|| closed_message.to_string()),
        _ => closed_message.to_string(),
    })
}

fn terminal_connection_event(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<Option<Value>, VmError> {
    let current = connection_state(context, decode_connection(context, connection)?);
    Ok(match current.phase() {
        MqttPhase::Failed => Some(
            MqttEvent::Failed {
                reason: current
                    .failure_message()
                    .unwrap_or_else(|| "mqtt connection failed".to_string()),
            }
            .into_value(),
        ),
        MqttPhase::Closed => Some(
            MqttEvent::Closed {
                reason: "connection-closed".to_string(),
            }
            .into_value(),
        ),
        _ => None,
    })
}

fn normalize_target(host: &str, port: i64) -> Result<(String, String, u16), VmError> {
    if host.is_empty() || host.chars().any(char::is_whitespace) {
        return Err(VmError::HostError(format!(
            "mqtt target host must be non-empty and contain no whitespace, got '{host}'",
        )));
    }
    let port = u16::try_from(port).map_err(|_| {
        VmError::HostError(format!(
            "mqtt target port must be between 1 and 65535, got {port}",
        ))
    })?;
    if port == 0 {
        return Err(VmError::HostError(
            "mqtt target port must be between 1 and 65535, got 0".to_string(),
        ));
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
    let parsed = Authority::from_maybe_shared(authority.clone()).map_err(|_| {
        VmError::HostError(format!("invalid mqtt target host='{host}' port={port}",))
    })?;
    Ok((
        authority,
        parsed.host().trim_matches(['[', ']']).to_string(),
        port,
    ))
}

fn connection_snapshot(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<MqttConnectConfig, VmError> {
    with_connection_state_mut(context, connection, |state| {
        let host = state.host().ok_or_else(|| {
            VmError::HostError("mqtt target host must be configured before connect".to_string())
        })?;
        let port = state
            .port()
            .unwrap_or_else(|| state.scheme().default_port());
        Ok(MqttConnectConfig {
            scheme: state.scheme(),
            host: host.to_string(),
            port,
            client_id: state.ensure_client_id(),
            username: state.username().map(str::to_string),
            password: state.password().map(str::to_string),
            keep_alive_secs: state.keep_alive_secs(),
            clean_start: state.clean_start(),
        })
    })
}

async fn connect_tcp_carrier(
    context: &SharedProxyVmContext,
    host: &str,
    port: u16,
) -> Result<i64, VmError> {
    let handle = allocate_tcp_stream_handle(context)?;
    let (authority, normalized_host, normalized_port) = normalize_target(host, i64::from(port))?;
    {
        let mut transport = context.lock_transport();
        let state = transport
            .tcp_streams
            .get_mut(&handle)
            .expect("mqtt tcp handle should exist after allocation");
        state.set_target(authority.clone(), normalized_host, normalized_port);
    }

    let resolved = lookup_host(authority.as_str()).await.map_err(|err| {
        let message = format!("failed to resolve mqtt target '{authority}': {err}");
        let mut transport = context.lock_transport();
        if let Some(state) = transport.tcp_streams.get_mut(&handle) {
            state.mark_failed(message.clone());
        }
        VmError::HostError(message)
    })?;

    let mut last_error = None;
    for peer_addr in resolved {
        let socket = if peer_addr.is_ipv4() {
            TcpSocket::new_v4()
        } else {
            TcpSocket::new_v6()
        }
        .map_err(|err| VmError::HostError(format!("failed to create mqtt tcp socket: {err}")))?;

        match socket.connect(peer_addr).await {
            Ok(stream) => {
                let local_addr = stream
                    .local_addr()
                    .map_err(|err| {
                        VmError::HostError(format!("failed to read mqtt tcp local address: {err}",))
                    })?
                    .to_string();
                let peer_addr = stream
                    .peer_addr()
                    .map_err(|err| {
                        VmError::HostError(format!("failed to read mqtt tcp peer address: {err}",))
                    })?
                    .to_string();
                let io = Arc::new(tokio::sync::Mutex::new(Some(stream)));
                let mut transport = context.lock_transport();
                if let Some(state) = transport.tcp_streams.get_mut(&handle) {
                    state.mark_connected(local_addr, peer_addr);
                }
                transport.tcp_stream_ios.insert(handle, io);
                return Ok(handle);
            }
            Err(err) => {
                last_error = Some(err.to_string());
            }
        }
    }

    let message =
        last_error.unwrap_or_else(|| "no resolved mqtt target addresses were usable".to_string());
    {
        let mut transport = context.lock_transport();
        if let Some(state) = transport.tcp_streams.get_mut(&handle) {
            state.mark_failed(message.clone());
        }
    }
    Err(VmError::HostError(format!(
        "failed to connect mqtt tcp carrier {handle}: {message}",
    )))
}

#[cfg(feature = "tls")]
async fn upgrade_tcp_carrier_to_tls(
    context: &SharedProxyVmContext,
    stream: i64,
    host: &str,
) -> Result<i64, VmError> {
    {
        let mut transport = context.lock_transport();
        let flow = transport
            .dynamic_tls_sessions
            .get_or_insert_with(stream, TlsFlowState::for_dynamic_socket);
        flow.observe_socket_target(host);
        flow.note_handshake_prepared();
        flow.note_client_hello_sent();
    }

    let flow = {
        let transport = context.lock_transport();
        transport
            .dynamic_tls_sessions
            .get(&stream)
            .cloned()
            .expect("mqtt tls flow should exist before handshake")
    };
    let peer_name = flow.peer_name().to_string();
    if peer_name.is_empty() {
        return Err(VmError::HostError(format!(
            "mqtt tls session handle {stream} has no peer name",
        )));
    }
    let server_name = ServerName::try_from(peer_name.clone()).map_err(|err| {
        VmError::HostError(format!(
            "invalid mqtt tls peer name '{peer_name}' for stream {stream}: {err}",
        ))
    })?;

    let tcp_stream = {
        let io = {
            let mut transport = context.lock_transport();
            transport.tcp_stream_ios.remove(&stream).ok_or_else(|| {
                VmError::HostError(format!(
                    "mqtt tcp carrier {stream} must be connected before tls upgrade",
                ))
            })?
        };
        let mut guard = io.lock().await;
        guard.take().ok_or_else(|| {
            VmError::HostError(format!(
                "mqtt tcp carrier {stream} is already in use during tls upgrade",
            ))
        })?
    };

    let connector = TlsConnector::from(Arc::new(build_dynamic_client_config(&flow)?));
    match connector.connect(server_name, tcp_stream).await {
        Ok(tls_stream) => {
            let negotiated_alpn = tls_stream
                .get_ref()
                .1
                .alpn_protocol()
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned());
            let peer_certificate_der = tls_stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|certs| certs.first().cloned())
                .map(|certificate| certificate.to_vec());
            let mut transport = context.lock_transport();
            if let Some(flow) = transport.dynamic_tls_sessions.get_mut(&stream) {
                flow.note_server_hello_received();
                flow.note_server_certificate_received(peer_certificate_der);
                if flow.verify_peer() && flow.verify_hostname() {
                    flow.note_server_certificate_verified();
                } else {
                    flow.note_verification_skipped();
                }
                flow.mark_handshake_complete(negotiated_alpn);
            }
            transport
                .dynamic_tls_session_ios
                .insert(stream, Arc::new(tokio::sync::Mutex::new(Some(tls_stream))));
            Ok(stream)
        }
        Err(err) => {
            let mut transport = context.lock_transport();
            if let Some(flow) = transport.dynamic_tls_sessions.get_mut(&stream) {
                flow.mark_failed();
            }
            if let Some(state) = transport.tcp_streams.get_mut(&stream) {
                state.mark_failed(format!("mqtt tls handshake failed: {err}"));
            }
            Err(VmError::HostError(format!(
                "mqtt tls handshake failed for stream {stream}: {err}",
            )))
        }
    }
}

async fn attach_carrier(
    context: &SharedProxyVmContext,
    config: &MqttConnectConfig,
) -> Result<MqttCarrierAttachment, VmError> {
    let stream = connect_tcp_carrier(context, &config.host, config.port).await?;
    if config.scheme.uses_tls() {
        #[cfg(feature = "tls")]
        {
            let session = upgrade_tcp_carrier_to_tls(context, stream, &config.host).await?;
            let mut transport = context.lock_transport();
            if let Some(state) = transport.tcp_streams.get_mut(&session) {
                state.mark_mqtt_attached();
            }
            return Ok(MqttCarrierAttachment::Tls { session });
        }
        #[cfg(not(feature = "tls"))]
        {
            let _ = stream;
            return Err(VmError::HostError(
                "mqtts requires the tls feature".to_string(),
            ));
        }
    }

    let mut transport = context.lock_transport();
    if let Some(state) = transport.tcp_streams.get_mut(&stream) {
        state.mark_mqtt_attached();
    }
    Ok(MqttCarrierAttachment::Tcp { stream })
}

fn active_tcp_io(
    context: &SharedProxyVmContext,
    stream: i64,
) -> Result<SharedTcpStreamIo, VmError> {
    context
        .lock_transport()
        .tcp_stream_ios
        .get(&stream)
        .cloned()
        .ok_or_else(|| VmError::HostError(format!("mqtt tcp carrier {stream} is unavailable")))
}

#[cfg(feature = "tls")]
fn active_tls_io(
    context: &SharedProxyVmContext,
    session: i64,
) -> Result<SharedTlsStreamIo, VmError> {
    context
        .lock_transport()
        .dynamic_tls_session_ios
        .get(&session)
        .cloned()
        .ok_or_else(|| VmError::HostError(format!("mqtt tls carrier {session} is unavailable")))
}

async fn write_carrier_bytes(
    context: &SharedProxyVmContext,
    carrier: &MqttCarrierAttachment,
    bytes: &[u8],
) -> Result<(), VmError> {
    match *carrier {
        MqttCarrierAttachment::Tcp { stream } => {
            let io = active_tcp_io(context, stream)?;
            let mut guard = io.lock().await;
            let stream_io = guard.as_mut().ok_or_else(|| {
                VmError::HostError(format!("mqtt tcp carrier {stream} is already in use",))
            })?;
            stream_io.write_all(bytes).await.map_err(|err| {
                VmError::HostError(format!("mqtt tcp write failed for stream {stream}: {err}"))
            })?;
            stream_io.flush().await.map_err(|err| {
                VmError::HostError(format!("mqtt tcp flush failed for stream {stream}: {err}"))
            })?;
        }
        #[cfg(feature = "tls")]
        MqttCarrierAttachment::Tls { session } => {
            let io = active_tls_io(context, session)?;
            let mut guard = io.lock().await;
            let stream_io = guard.as_mut().ok_or_else(|| {
                VmError::HostError(format!("mqtt tls carrier {session} is already in use",))
            })?;
            stream_io.write_all(bytes).await.map_err(|err| {
                VmError::HostError(format!(
                    "mqtt tls write failed for session {session}: {err}"
                ))
            })?;
            stream_io.flush().await.map_err(|err| {
                VmError::HostError(format!(
                    "mqtt tls flush failed for session {session}: {err}"
                ))
            })?;
        }
    }
    Ok(())
}

async fn write_connection_packet(
    context: &SharedProxyVmContext,
    connection: i64,
    bytes: &[u8],
) -> Result<(), VmError> {
    let carrier = active_connection_carrier(context, connection)?;
    write_carrier_bytes(context, &carrier, bytes).await?;
    note_connection_activity(context, connection)?;
    Ok(())
}

async fn write_connection_ping_request(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<(), VmError> {
    let carrier = active_connection_carrier(context, connection)?;
    write_carrier_bytes(context, &carrier, &encode_pingreq_packet()?).await?;
    note_ping_request_sent(context, connection)?;
    Ok(())
}

async fn read_carrier_chunk(
    context: &SharedProxyVmContext,
    carrier: &MqttCarrierAttachment,
    max_bytes: usize,
    timeout_after: Option<Duration>,
) -> Result<CarrierReadChunk, VmError> {
    let mut buffer = vec![0u8; max_bytes];
    let read = match *carrier {
        MqttCarrierAttachment::Tcp { stream } => {
            let io = active_tcp_io(context, stream)?;
            let mut guard = io.lock().await;
            let stream_io = guard.as_mut().ok_or_else(|| {
                VmError::HostError(format!("mqtt tcp carrier {stream} is already in use",))
            })?;
            match timeout_after {
                Some(timeout_after) => {
                    match timeout(timeout_after, stream_io.read(&mut buffer)).await {
                        Ok(read) => read.map_err(|err| {
                            VmError::HostError(format!(
                                "mqtt tcp read failed for stream {stream}: {err}"
                            ))
                        })?,
                        Err(_) => return Ok(CarrierReadChunk::TimedOut),
                    }
                }
                None => stream_io.read(&mut buffer).await.map_err(|err| {
                    VmError::HostError(format!("mqtt tcp read failed for stream {stream}: {err}"))
                })?,
            }
        }
        #[cfg(feature = "tls")]
        MqttCarrierAttachment::Tls { session } => {
            let io = active_tls_io(context, session)?;
            let mut guard = io.lock().await;
            let stream_io = guard.as_mut().ok_or_else(|| {
                VmError::HostError(format!("mqtt tls carrier {session} is already in use",))
            })?;
            match timeout_after {
                Some(timeout_after) => {
                    match timeout(timeout_after, stream_io.read(&mut buffer)).await {
                        Ok(read) => read.map_err(|err| {
                            VmError::HostError(format!(
                                "mqtt tls read failed for session {session}: {err}"
                            ))
                        })?,
                        Err(_) => return Ok(CarrierReadChunk::TimedOut),
                    }
                }
                None => stream_io.read(&mut buffer).await.map_err(|err| {
                    VmError::HostError(format!("mqtt tls read failed for session {session}: {err}"))
                })?,
            }
        }
    };
    if read == 0 {
        return Ok(CarrierReadChunk::Closed);
    }
    buffer.truncate(read);
    Ok(CarrierReadChunk::Data(buffer))
}

async fn close_carrier(
    context: &SharedProxyVmContext,
    carrier: &MqttCarrierAttachment,
    failure_message: Option<&str>,
) -> Result<(), VmError> {
    match *carrier {
        MqttCarrierAttachment::Tcp { stream } => {
            let io = {
                let mut transport = context.lock_transport();
                let io = transport.tcp_stream_ios.remove(&stream);
                if let Some(state) = transport.tcp_streams.get_mut(&stream) {
                    if let Some(message) = failure_message {
                        state.mark_failed(message.to_string());
                    } else {
                        state.mark_closed();
                    }
                }
                io
            };
            if let Some(io) = io {
                let mut guard = io.lock().await;
                if let Some(mut stream_io) = guard.take() {
                    let _ = stream_io.shutdown().await;
                }
            }
        }
        #[cfg(feature = "tls")]
        MqttCarrierAttachment::Tls { session } => {
            let io = {
                let mut transport = context.lock_transport();
                let io = transport.dynamic_tls_session_ios.remove(&session);
                if let Some(flow) = transport.dynamic_tls_sessions.get_mut(&session) {
                    if failure_message.is_some() {
                        flow.mark_failed();
                    } else {
                        flow.mark_closed();
                    }
                }
                if let Some(state) = transport.tcp_streams.get_mut(&session) {
                    if let Some(message) = failure_message {
                        state.mark_failed(message.to_string());
                    } else {
                        state.mark_closed();
                    }
                }
                io
            };
            if let Some(io) = io {
                let mut guard = io.lock().await;
                if let Some(mut stream_io) = guard.take() {
                    let _ = stream_io.shutdown().await;
                }
            }
        }
    }
    Ok(())
}

async fn close_connection_carrier(
    context: &SharedProxyVmContext,
    connection: i64,
    failure_message: Option<&str>,
) -> Result<(), VmError> {
    let carrier = with_connection_state_mut(context, connection, |state| Ok(state.take_carrier()))?;
    if let Some(carrier) = carrier {
        close_carrier(context, &carrier, failure_message).await?;
    }
    Ok(())
}

async fn read_next_packet(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<Option<MqttIncomingPacket>, VmError> {
    let (carrier, mut buffer) = with_connection_state_mut(context, connection, |state| {
        let carrier = state.carrier().ok_or_else(|| {
            VmError::HostError("mqtt connection has no attached carrier".to_string())
        })?;
        Ok((carrier, std::mem::take(&mut state.pending_read_buffer)))
    })?;

    loop {
        if let Some((packet, consumed)) = decode_packet(&buffer)? {
            let remainder = buffer.split_off(consumed);
            with_connection_state_mut(context, connection, |state| {
                state.pending_read_buffer = remainder;
                Ok(())
            })?;
            return Ok(Some(packet));
        }

        if let Some(keep_alive_wait) = keep_alive_read_wait(context, connection)?
            && keep_alive_wait.timeout.is_zero()
        {
            if keep_alive_wait.ping_response_pending {
                let message =
                    "mqtt keepalive expired while waiting for broker activity".to_string();
                with_connection_state_mut(context, connection, |state| {
                    state.pending_read_buffer = buffer;
                    state.mark_failed(message.clone());
                    Ok(())
                })?;
                close_connection_carrier(context, connection, Some(&message)).await?;
                return Ok(None);
            }
            write_connection_ping_request(context, connection).await?;
            continue;
        }

        let keep_alive_wait = keep_alive_read_wait(context, connection)?;
        match read_carrier_chunk(
            context,
            &carrier,
            4096,
            keep_alive_wait.map(|wait| wait.timeout),
        )
        .await?
        {
            CarrierReadChunk::Data(chunk) => {
                note_connection_activity(context, connection)?;
                buffer.extend_from_slice(&chunk);
            }
            CarrierReadChunk::TimedOut => {
                let keep_alive_wait = keep_alive_wait.ok_or_else(|| {
                    VmError::HostError(
                        "mqtt read timed out without an active keepalive policy".to_string(),
                    )
                })?;
                if keep_alive_wait.ping_response_pending {
                    let message =
                        "mqtt keepalive expired while waiting for broker activity".to_string();
                    with_connection_state_mut(context, connection, |state| {
                        state.pending_read_buffer = buffer;
                        state.mark_failed(message.clone());
                        Ok(())
                    })?;
                    close_connection_carrier(context, connection, Some(&message)).await?;
                    return Ok(None);
                }
                write_connection_ping_request(context, connection).await?;
            }
            CarrierReadChunk::Closed => {
                with_connection_state_mut(context, connection, |state| {
                    state.pending_read_buffer = buffer;
                    state.mark_closed();
                    Ok(())
                })?;
                return Ok(None);
            }
        }
    }
}

fn queue_event(
    context: &SharedProxyVmContext,
    connection: i64,
    event: MqttEvent,
) -> Result<(), VmError> {
    with_connection_state_mut(context, connection, |state| {
        state.pending_events.push_back(event);
        Ok(())
    })
}

fn pop_queued_event(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<Option<MqttEvent>, VmError> {
    with_connection_state_mut(context, connection, |state| {
        Ok(state.pending_events.pop_front())
    })
}

async fn handle_unsolicited_packet(
    context: &SharedProxyVmContext,
    connection: i64,
    packet: MqttIncomingPacket,
) -> Result<(), VmError> {
    match packet {
        MqttIncomingPacket::Publish {
            topic,
            payload,
            qos,
            retain,
            dup,
            packet_id,
        } => {
            if qos == 1 {
                let packet_id = packet_id.ok_or_else(|| {
                    VmError::HostError("mqtt qos1 publish is missing a packet id".to_string())
                })?;
                write_connection_packet(context, connection, &encode_puback_packet(packet_id)?)
                    .await?;
            }
            queue_event(
                context,
                connection,
                MqttEvent::Publish {
                    topic,
                    payload,
                    qos,
                    retain,
                    dup,
                },
            )?;
        }
        MqttIncomingPacket::Disconnect { reason_code } => {
            let reason = format!("remote-disconnect:{reason_code}");
            with_connection_state_mut(context, connection, |state| {
                state.mark_closed();
                Ok(())
            })?;
            queue_event(context, connection, MqttEvent::Closed { reason })?;
        }
        MqttIncomingPacket::PingResp => note_ping_response_received(context, connection)?,
        MqttIncomingPacket::ConnAck { .. }
        | MqttIncomingPacket::PubAck { .. }
        | MqttIncomingPacket::SubAck { .. }
        | MqttIncomingPacket::UnsubAck { .. } => {}
    }
    Ok(())
}

async fn wait_for_puback(
    context: &SharedProxyVmContext,
    connection: i64,
    expected_packet_id: u16,
) -> Result<(), VmError> {
    loop {
        match read_next_packet(context, connection).await? {
            Some(MqttIncomingPacket::PubAck {
                packet_id,
                reason_code,
            }) if packet_id == expected_packet_id => {
                if reason_code >= 0x80 {
                    return Err(VmError::HostError(format!(
                        "mqtt publish rejected with reason code {reason_code}",
                    )));
                }
                return Ok(());
            }
            Some(packet) => handle_unsolicited_packet(context, connection, packet).await?,
            None => {
                return Err(VmError::HostError(terminal_connection_message(
                    context,
                    connection,
                    "mqtt connection closed while waiting for puback",
                )?));
            }
        }
    }
}

async fn wait_for_suback(
    context: &SharedProxyVmContext,
    connection: i64,
    expected_packet_id: u16,
) -> Result<(), VmError> {
    loop {
        match read_next_packet(context, connection).await? {
            Some(MqttIncomingPacket::SubAck {
                packet_id,
                reason_codes,
            }) if packet_id == expected_packet_id => {
                if reason_codes.iter().any(|code| *code >= 0x80) {
                    return Err(VmError::HostError(format!(
                        "mqtt subscribe rejected with reason codes {:?}",
                        reason_codes,
                    )));
                }
                return Ok(());
            }
            Some(packet) => handle_unsolicited_packet(context, connection, packet).await?,
            None => {
                return Err(VmError::HostError(terminal_connection_message(
                    context,
                    connection,
                    "mqtt connection closed while waiting for suback",
                )?));
            }
        }
    }
}

async fn wait_for_unsuback(
    context: &SharedProxyVmContext,
    connection: i64,
    expected_packet_id: u16,
) -> Result<(), VmError> {
    loop {
        match read_next_packet(context, connection).await? {
            Some(MqttIncomingPacket::UnsubAck {
                packet_id,
                reason_codes,
            }) if packet_id == expected_packet_id => {
                if reason_codes.iter().any(|code| *code >= 0x80) {
                    return Err(VmError::HostError(format!(
                        "mqtt unsubscribe rejected with reason codes {:?}",
                        reason_codes,
                    )));
                }
                return Ok(());
            }
            Some(packet) => handle_unsolicited_packet(context, connection, packet).await?,
            None => {
                return Err(VmError::HostError(terminal_connection_message(
                    context,
                    connection,
                    "mqtt connection closed while waiting for unsuback",
                )?));
            }
        }
    }
}

async fn ensure_connection_open(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<bool, VmError> {
    if matches!(
        connection_state(context, decode_connection(context, connection)?).phase(),
        MqttPhase::Open
    ) {
        return Ok(true);
    }

    close_connection_carrier(context, connection, None).await?;
    let config = connection_snapshot(context, connection)?;
    let carrier = attach_carrier(context, &config).await?;
    with_connection_state_mut(context, connection, |state| {
        state.attach_carrier(carrier.clone());
        Ok(())
    })?;

    let connect_packet = encode_connect_packet(&config)?;
    if let Err(err) = write_connection_packet(context, connection, &connect_packet).await {
        with_connection_state_mut(context, connection, |state| {
            state.mark_failed(err.to_string());
            Ok(())
        })?;
        close_carrier(context, &carrier, Some(&err.to_string())).await?;
        return Err(err);
    }
    with_connection_state_mut(context, connection, |state| {
        state.note_connect_sent();
        Ok(())
    })?;

    match read_next_packet(context, connection).await? {
        Some(MqttIncomingPacket::ConnAck { reason_code }) if reason_code < 0x80 => {
            with_connection_state_mut(context, connection, |state| {
                state.mark_open();
                Ok(())
            })?;
            Ok(true)
        }
        Some(MqttIncomingPacket::ConnAck { reason_code }) => {
            let message = format!("mqtt connect rejected with reason code {reason_code}");
            with_connection_state_mut(context, connection, |state| {
                state.mark_failed(message.clone());
                Ok(())
            })?;
            close_carrier(context, &carrier, Some(&message)).await?;
            Err(VmError::HostError(message))
        }
        Some(packet) => {
            let message = format!("expected mqtt connack, received {packet:?}");
            with_connection_state_mut(context, connection, |state| {
                state.mark_failed(message.clone());
                Ok(())
            })?;
            close_carrier(context, &carrier, Some(&message)).await?;
            Err(VmError::HostError(message))
        }
        None => {
            let message = "mqtt connection closed before connack".to_string();
            with_connection_state_mut(context, connection, |state| {
                state.mark_failed(message.clone());
                Ok(())
            })?;
            close_carrier(context, &carrier, Some(&message)).await?;
            Err(VmError::HostError(message))
        }
    }
}

fn parse_qos(qos: i64) -> Result<u8, VmError> {
    match qos {
        0 => Ok(0),
        1 => Ok(1),
        2 => Err(VmError::HostError(
            "mqtt qos 2 is not supported in this milestone".to_string(),
        )),
        _ => Err(VmError::HostError(format!(
            "mqtt qos must be 0 or 1, got {qos}",
        ))),
    }
}

async fn publish_payload(
    context: &SharedProxyVmContext,
    connection: i64,
    topic: String,
    payload: Vec<u8>,
    qos: i64,
    retain: bool,
) -> Result<bool, VmError> {
    if topic.is_empty() {
        return Err(VmError::HostError(
            "mqtt publish topic must not be empty".to_string(),
        ));
    }
    ensure_connection_open(context, connection).await?;
    let qos = parse_qos(qos)?;
    let packet_id = if qos == 0 {
        None
    } else {
        Some(with_connection_state_mut(context, connection, |state| {
            Ok(state.next_packet_id())
        })?)
    };
    write_connection_packet(
        context,
        connection,
        &encode_publish_packet(&topic, &payload, qos, retain, packet_id)?,
    )
    .await?;
    if let Some(packet_id) = packet_id {
        wait_for_puback(context, connection, packet_id).await?;
    }
    Ok(true)
}

async fn subscribe_filter(
    context: &SharedProxyVmContext,
    connection: i64,
    filter: String,
    qos: i64,
) -> Result<bool, VmError> {
    if filter.is_empty() {
        return Err(VmError::HostError(
            "mqtt subscription filter must not be empty".to_string(),
        ));
    }
    ensure_connection_open(context, connection).await?;
    let qos = parse_qos(qos)?;
    let packet_id =
        with_connection_state_mut(context, connection, |state| Ok(state.next_packet_id()))?;
    write_connection_packet(
        context,
        connection,
        &encode_subscribe_packet(&filter, qos, packet_id)?,
    )
    .await?;
    wait_for_suback(context, connection, packet_id).await?;
    Ok(true)
}

async fn unsubscribe_filter(
    context: &SharedProxyVmContext,
    connection: i64,
    filter: String,
) -> Result<bool, VmError> {
    if filter.is_empty() {
        return Err(VmError::HostError(
            "mqtt unsubscribe filter must not be empty".to_string(),
        ));
    }
    ensure_connection_open(context, connection).await?;
    let packet_id =
        with_connection_state_mut(context, connection, |state| Ok(state.next_packet_id()))?;
    write_connection_packet(
        context,
        connection,
        &encode_unsubscribe_packet(&filter, packet_id)?,
    )
    .await?;
    wait_for_unsuback(context, connection, packet_id).await?;
    Ok(true)
}

async fn read_next_event_value(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<Value, VmError> {
    if let Some(event) = pop_queued_event(context, connection)? {
        return Ok(event.into_value());
    }

    if let Some(event) = terminal_connection_event(context, connection)? {
        return Ok(event);
    }

    ensure_connection_open(context, connection).await?;
    loop {
        if let Some(event) = pop_queued_event(context, connection)? {
            return Ok(event.into_value());
        }

        match read_next_packet(context, connection).await? {
            Some(packet) => handle_unsolicited_packet(context, connection, packet).await?,
            None => {
                if let Some(event) = terminal_connection_event(context, connection)? {
                    return Ok(event);
                }
            }
        }
    }
}

/// Allocates an MQTT connection handle.
#[pd_edge_host_function(name = mqtt::connection::NEW.name, scope = mqtt)]
async fn connection_new(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let handle = allocate_mqtt_connection_handle(&context)?;
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

/// Returns the default upstream handle for the MQTT connection.
#[pd_edge_host_function(name = mqtt::connection::DEFAULT_UPSTREAM.name, scope = mqtt)]
async fn connection_default_upstream(
    _vm: &mut Vm,
    _context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::Int(
        default_upstream_mqtt_connection_handle(),
    )]))
}

/// Returns whether the MQTT connection handle is present.
#[pd_edge_host_function(name = mqtt::connection::IS_PRESENT.name, scope = mqtt)]
async fn connection_is_present(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    let present = connection_state(&context, decode_connection(&context, connection)?).is_present();
    Ok(CallOutcome::Return(vec![Value::Bool(present)]))
}

/// Sets the scheme for the MQTT connection.
#[pd_edge_host_function(name = mqtt::connection::SET_SCHEME.name, scope = mqtt)]
async fn connection_set_scheme(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    scheme: String,
) -> Result<CallOutcome, VmError> {
    let scheme = MqttScheme::parse(&scheme)?;
    with_configurable_connection_state_mut(&context, connection, |state| {
        state.set_scheme(scheme);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the target endpoint for the MQTT connection.
#[pd_edge_host_function(name = mqtt::connection::SET_TARGET.name, scope = mqtt)]
async fn connection_set_target(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    host: String,
    port: i64,
) -> Result<CallOutcome, VmError> {
    let (_, normalized_host, port) = normalize_target(&host, port)?;
    with_configurable_connection_state_mut(&context, connection, |state| {
        state.set_target(normalized_host, port);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the client identifier for the MQTT connection.
#[pd_edge_host_function(name = mqtt::connection::SET_CLIENT_ID.name, scope = mqtt)]
async fn connection_set_client_id(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    client_id: String,
) -> Result<CallOutcome, VmError> {
    with_configurable_connection_state_mut(&context, connection, |state| {
        state.set_client_id(client_id);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the username for the MQTT connection.
#[pd_edge_host_function(name = mqtt::connection::SET_USERNAME.name, scope = mqtt)]
async fn connection_set_username(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    username: String,
) -> Result<CallOutcome, VmError> {
    with_configurable_connection_state_mut(&context, connection, |state| {
        state.set_username(username);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the password for the MQTT connection.
#[pd_edge_host_function(name = mqtt::connection::SET_PASSWORD.name, scope = mqtt)]
async fn connection_set_password(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    password: String,
) -> Result<CallOutcome, VmError> {
    with_configurable_connection_state_mut(&context, connection, |state| {
        state.set_password(password);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the keep-alive interval for the MQTT connection.
#[pd_edge_host_function(name = mqtt::connection::SET_KEEP_ALIVE_SECS.name, scope = mqtt)]
async fn connection_set_keep_alive_secs(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    keep_alive_secs: i64,
) -> Result<CallOutcome, VmError> {
    let keep_alive_secs = u16::try_from(keep_alive_secs).map_err(|_| {
        VmError::HostError(format!(
            "mqtt keep alive must be between 0 and 65535 seconds, got {keep_alive_secs}",
        ))
    })?;
    with_configurable_connection_state_mut(&context, connection, |state| {
        state.set_keep_alive_secs(keep_alive_secs);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Enables or disables clean start for the MQTT connection.
#[pd_edge_host_function(name = mqtt::connection::SET_CLEAN_START.name, scope = mqtt)]
async fn connection_set_clean_start(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    enabled: bool,
) -> Result<CallOutcome, VmError> {
    with_configurable_connection_state_mut(&context, connection, |state| {
        state.set_clean_start(enabled);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Connects the MQTT session over its attached transport carrier.
#[pd_edge_host_function(name = mqtt::connection::CONNECT.name, scope = mqtt)]
async fn connection_connect(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    let connected = ensure_connection_open(&context, connection).await?;
    Ok(CallOutcome::Return(vec![Value::Bool(connected)]))
}

/// Reports the current lifecycle phase for the MQTT connection.
#[pd_edge_host_function(name = mqtt::connection::GET_PHASE.name, scope = mqtt)]
async fn connection_get_phase(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    let phase = connection_state(&context, decode_connection(&context, connection)?)
        .phase()
        .as_str();
    Ok(CallOutcome::Return(vec![Value::string(phase)]))
}

/// Sends an MQTT DISCONNECT and closes the carrier.
#[pd_edge_host_function(name = mqtt::connection::DISCONNECT.name, scope = mqtt)]
async fn connection_disconnect(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    reason_code: i64,
    reason_text: String,
) -> Result<CallOutcome, VmError> {
    let reason_code = u8::try_from(reason_code).map_err(|_| {
        VmError::HostError(format!(
            "mqtt disconnect reason code must be between 0 and 255, got {reason_code}",
        ))
    })?;
    let write_result = if connection_state(&context, decode_connection(&context, connection)?)
        .carrier()
        .is_some()
    {
        let packet = encode_disconnect_packet(reason_code, &reason_text)?;
        write_connection_packet(&context, connection, &packet).await
    } else {
        Ok(())
    };
    close_connection_carrier(&context, connection, None).await?;
    with_connection_state_mut(&context, connection, |state| {
        state.mark_closed();
        Ok(())
    })?;
    write_result?;
    Ok(CallOutcome::Return(vec![]))
}

/// Publishes a UTF-8 text payload on the MQTT connection.
#[pd_edge_host_function(name = mqtt::connection::PUBLISH_TEXT.name, scope = mqtt)]
async fn connection_publish_text(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    topic: String,
    payload: String,
    qos: i64,
    retain: bool,
) -> Result<CallOutcome, VmError> {
    let published = publish_payload(
        &context,
        connection,
        topic,
        payload.into_bytes(),
        qos,
        retain,
    )
    .await?;
    Ok(CallOutcome::Return(vec![Value::Bool(published)]))
}

/// Publishes a binary payload on the MQTT connection.
#[pd_edge_host_function(name = mqtt::connection::PUBLISH_BINARY.name, scope = mqtt)]
async fn connection_publish_binary(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    topic: String,
    payload: Value,
    qos: i64,
    retain: bool,
) -> Result<CallOutcome, VmError> {
    let payload = value_to_bytes(&payload, "mqtt::connection::publish_binary payload")?.to_vec();
    let published = publish_payload(&context, connection, topic, payload, qos, retain).await?;
    Ok(CallOutcome::Return(vec![Value::Bool(published)]))
}

/// Publishes a base64-encoded binary payload on the MQTT connection.
#[pd_edge_host_function(
    name = mqtt::connection::PUBLISH_BINARY_BASE64.name,
    scope = mqtt
)]
async fn connection_publish_binary_base64(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    topic: String,
    payload: String,
    qos: i64,
    retain: bool,
) -> Result<CallOutcome, VmError> {
    let payload = STANDARD.decode(payload).map_err(|err| {
        VmError::HostError(format!("mqtt binary payload must be base64 encoded: {err}",))
    })?;
    let published = publish_payload(&context, connection, topic, payload, qos, retain).await?;
    Ok(CallOutcome::Return(vec![Value::Bool(published)]))
}

/// Subscribes the MQTT connection to a topic filter.
#[pd_edge_host_function(name = mqtt::connection::SUBSCRIBE.name, scope = mqtt)]
async fn connection_subscribe(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    filter: String,
    qos: i64,
) -> Result<CallOutcome, VmError> {
    let subscribed = subscribe_filter(&context, connection, filter, qos).await?;
    Ok(CallOutcome::Return(vec![Value::Bool(subscribed)]))
}

/// Removes a topic filter subscription from the MQTT connection.
#[pd_edge_host_function(name = mqtt::connection::UNSUBSCRIBE.name, scope = mqtt)]
async fn connection_unsubscribe(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    filter: String,
) -> Result<CallOutcome, VmError> {
    let unsubscribed = unsubscribe_filter(&context, connection, filter).await?;
    Ok(CallOutcome::Return(vec![Value::Bool(unsubscribed)]))
}

/// Reads the next MQTT event from the connection.
#[pd_edge_host_function(name = mqtt::connection::READ_EVENT.name, scope = mqtt)]
async fn connection_read_event(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    let event = read_next_event_value(&context, connection).await?;
    Ok(CallOutcome::Return(vec![event]))
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use axum::http::HeaderMap;

    use super::*;
    use crate::abi_impl::transport::TcpSocketPhase;
    use crate::abi_impl::{ProxyVmContext, RateLimiterStore};

    fn test_context() -> SharedProxyVmContext {
        Arc::new(ProxyVmContext::from_request_headers(
            HeaderMap::new(),
            Arc::new(RateLimiterStore::new()),
        ))
    }

    async fn read_socket_packet(stream: &mut tokio::net::TcpStream) -> Result<Vec<u8>, VmError> {
        let mut first = [0u8; 1];
        stream
            .read_exact(&mut first)
            .await
            .map_err(|err| VmError::HostError(format!("broker read fixed header failed: {err}")))?;
        let mut encoded_len = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            stream.read_exact(&mut byte).await.map_err(|err| {
                VmError::HostError(format!("broker read remaining length failed: {err}"))
            })?;
            encoded_len.push(byte[0]);
            if byte[0] & 0x80 == 0 {
                break;
            }
        }
        let (remaining_len, _) = super::super::codec::decode_variable_int(&encoded_len)?
            .ok_or_else(|| VmError::HostError("incomplete mqtt remaining length".to_string()))?;
        let mut body = vec![0u8; remaining_len];
        stream
            .read_exact(&mut body)
            .await
            .map_err(|err| VmError::HostError(format!("broker read body failed: {err}")))?;
        let mut packet = vec![first[0]];
        packet.extend_from_slice(&encoded_len);
        packet.extend_from_slice(&body);
        Ok(packet)
    }

    fn packet_body(packet: &[u8]) -> &[u8] {
        let (_, encoded_len) = super::super::codec::decode_variable_int(&packet[1..])
            .expect("remaining length should decode")
            .expect("remaining length should be complete");
        &packet[1 + encoded_len..]
    }

    fn decode_publish_topic_and_payload(packet: &[u8]) -> (String, Vec<u8>) {
        let body = packet_body(packet);
        let mut offset = 0usize;
        let topic = super::super::codec::decode_u16(body, &mut offset, "publish topic len")
            .expect("topic len should decode");
        let topic_len = usize::from(topic);
        let topic = std::str::from_utf8(&body[offset..offset + topic_len])
            .expect("topic should be utf8")
            .to_string();
        offset += topic_len;
        offset += 1;
        (topic, body[offset..].to_vec())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mqtt_feature_supports_outbound_connect_publish_subscribe() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mqtt listener should bind");
        let addr = listener.local_addr().expect("listener addr");
        let broker = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept should succeed");

            let connect = read_socket_packet(&mut stream)
                .await
                .expect("connect packet should arrive");
            assert_eq!(connect[0], 0x10);
            stream
                .write_all(&[0x20, 0x03, 0x00, 0x00, 0x00])
                .await
                .expect("connack should write");

            let subscribe = read_socket_packet(&mut stream)
                .await
                .expect("subscribe packet should arrive");
            assert_eq!(subscribe[0], 0x82);
            let subscribe_body = packet_body(&subscribe);
            let packet_id = u16::from_be_bytes([subscribe_body[0], subscribe_body[1]]);
            stream
                .write_all(&[
                    0x90,
                    0x04,
                    (packet_id >> 8) as u8,
                    packet_id as u8,
                    0x00,
                    0x00,
                ])
                .await
                .expect("suback should write");
            stream
                .write_all(
                    &encode_publish_packet("sensor/temp", b"21.5", 0, false, None)
                        .expect("server publish"),
                )
                .await
                .expect("publish should write");

            let publish = read_socket_packet(&mut stream)
                .await
                .expect("client publish should arrive");
            assert_eq!(publish[0] >> 4, 3);
            let (topic, payload) = decode_publish_topic_and_payload(&publish);
            assert_eq!(topic, "device/telemetry");
            assert_eq!(payload, b"hello".to_vec());

            let disconnect = read_socket_packet(&mut stream)
                .await
                .expect("disconnect packet should arrive");
            assert_eq!(disconnect[0], 0xE0);
        });

        let context = test_context();
        let connection = allocate_mqtt_connection_handle(&context).expect("mqtt handle");
        with_configurable_connection_state_mut(&context, connection, |state| {
            state.set_scheme(MqttScheme::Mqtt);
            state.set_target("127.0.0.1".to_string(), addr.port());
            state.set_client_id("client-a".to_string());
            Ok(())
        })
        .expect("state should configure");

        assert!(
            ensure_connection_open(&context, connection)
                .await
                .expect("connect should succeed")
        );
        assert_eq!(
            connection_state(
                &context,
                decode_connection(&context, connection).expect("decode")
            )
            .phase(),
            MqttPhase::Open
        );
        let carrier = connection_state(
            &context,
            decode_connection(&context, connection).expect("decode"),
        )
        .carrier()
        .expect("carrier should be attached");
        let stream = match carrier {
            MqttCarrierAttachment::Tcp { stream } => stream,
            #[cfg(feature = "tls")]
            MqttCarrierAttachment::Tls { session } => session,
        };
        assert!(matches!(
            context
                .lock_transport()
                .tcp_streams
                .get(&stream)
                .expect("mqtt tcp stream should exist")
                .phase(),
            TcpSocketPhase::AttachedMqtt
        ));

        assert!(
            subscribe_filter(&context, connection, "sensor/#".to_string(), 0)
                .await
                .expect("subscribe should succeed")
        );
        let event = read_next_event_value(&context, connection)
            .await
            .expect("event should read");
        let Value::Map(entries) = event else {
            panic!("expected mqtt event map");
        };
        assert_eq!(
            entries.get(&Value::string("kind")),
            Some(&Value::string("publish"))
        );
        assert_eq!(
            entries.get(&Value::string("topic")),
            Some(&Value::string("sensor/temp"))
        );
        assert_eq!(
            entries.get(&Value::string("payload_text")),
            Some(&Value::string("21.5"))
        );

        assert!(
            publish_payload(
                &context,
                connection,
                "device/telemetry".to_string(),
                b"hello".to_vec(),
                0,
                false,
            )
            .await
            .expect("publish should succeed")
        );

        let disconnect_packet =
            encode_disconnect_packet(0, "").expect("disconnect packet should encode");
        write_carrier_bytes(&context, &carrier, &disconnect_packet)
            .await
            .expect("disconnect should write");
        close_connection_carrier(&context, connection, None)
            .await
            .expect("carrier should close");
        with_connection_state_mut(&context, connection, |state| {
            state.mark_closed();
            Ok(())
        })
        .expect("state should close");

        broker.await.expect("broker should finish");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mqtt_feature_sends_pingreq_during_idle_read_event() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mqtt listener should bind");
        let addr = listener.local_addr().expect("listener addr");
        let broker = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept should succeed");

            let connect = read_socket_packet(&mut stream)
                .await
                .expect("connect packet should arrive");
            assert_eq!(connect[0], 0x10);
            stream
                .write_all(&[0x20, 0x03, 0x00, 0x00, 0x00])
                .await
                .expect("connack should write");

            tokio::time::sleep(Duration::from_millis(1_100)).await;

            let pingreq = read_socket_packet(&mut stream)
                .await
                .expect("pingreq should arrive");
            assert_eq!(pingreq, vec![0xC0, 0x00]);
            stream
                .write_all(&[0xD0, 0x00])
                .await
                .expect("pingresp should write");
            stream
                .write_all(
                    &encode_publish_packet("sensor/idle", b"awake", 0, false, None)
                        .expect("server publish"),
                )
                .await
                .expect("publish should write");

            let disconnect = read_socket_packet(&mut stream)
                .await
                .expect("disconnect packet should arrive");
            assert_eq!(disconnect[0], 0xE0);
        });

        let context = test_context();
        let connection = allocate_mqtt_connection_handle(&context).expect("mqtt handle");
        with_configurable_connection_state_mut(&context, connection, |state| {
            state.set_scheme(MqttScheme::Mqtt);
            state.set_target("127.0.0.1".to_string(), addr.port());
            state.set_client_id("client-keepalive".to_string());
            state.set_keep_alive_secs(1);
            Ok(())
        })
        .expect("state should configure");

        assert!(
            ensure_connection_open(&context, connection)
                .await
                .expect("connect should succeed")
        );

        let event = read_next_event_value(&context, connection)
            .await
            .expect("event should read");
        let Value::Map(entries) = event else {
            panic!("expected mqtt event map");
        };
        assert_eq!(
            entries.get(&Value::string("kind")),
            Some(&Value::string("publish"))
        );
        assert_eq!(
            entries.get(&Value::string("topic")),
            Some(&Value::string("sensor/idle"))
        );
        assert_eq!(
            entries.get(&Value::string("payload_text")),
            Some(&Value::string("awake"))
        );

        let carrier = connection_state(
            &context,
            decode_connection(&context, connection).expect("decode"),
        )
        .carrier()
        .expect("carrier should be attached");
        let disconnect_packet =
            encode_disconnect_packet(0, "").expect("disconnect packet should encode");
        write_carrier_bytes(&context, &carrier, &disconnect_packet)
            .await
            .expect("disconnect should write");
        close_connection_carrier(&context, connection, None)
            .await
            .expect("carrier should close");
        with_connection_state_mut(&context, connection, |state| {
            state.mark_closed();
            Ok(())
        })
        .expect("state should close");

        broker.await.expect("broker should finish");
    }
}
