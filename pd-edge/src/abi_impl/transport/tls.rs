use base64::{Engine as _, engine::general_purpose::STANDARD};
use edge_abi::symbols::tls;
use pd_edge_host_function::pd_edge_host_function;
use vm::{CallOutcome, Value, Vm, VmError};

use super::super::SharedProxyVmContext;
use super::super::http::{
    ensure_outbound_exchange_response_started, ensure_upstream_response_started,
    outbound_exchange_exists, outbound_exchange_response_available, outbound_exchange_tls_flow,
    upstream_response_available,
};
use super::super::websocket::{
    ensure_outbound_websocket_connection_open, websocket_connection_mode,
};
use super::state::tls_session_cache_key;
use super::state::{TlsFlowState, TlsProtocolVersion, TlsSessionRef, decode_tls_session_handle};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TlsSessionHandle {
    Reserved(TlsSessionRef),
    OutboundExchange(i64),
}

fn decode_session(
    context: &SharedProxyVmContext,
    session: i64,
) -> Result<TlsSessionHandle, VmError> {
    if let Some(reserved) = decode_tls_session_handle(session) {
        return Ok(TlsSessionHandle::Reserved(reserved));
    }
    if outbound_exchange_exists(context, session) {
        return Ok(TlsSessionHandle::OutboundExchange(session));
    }
    Err(VmError::HostError(format!(
        "invalid tls session handle {session}; reserved handles are 0 (downstream), 1 (default upstream), and allocated outbound exchange handles start at 2",
    )))
}

fn session_flow(context: &SharedProxyVmContext, session: TlsSessionHandle) -> TlsFlowState {
    if let TlsSessionHandle::OutboundExchange(handle) = session {
        return outbound_exchange_tls_flow(context, handle)
            .expect("exchange handle should exist while tls session is in use");
    }

    let guard = context.lock().expect("vm context lock poisoned");
    match session {
        TlsSessionHandle::Reserved(TlsSessionRef::Downstream) => guard.tls_dag.downstream.clone(),
        TlsSessionHandle::Reserved(TlsSessionRef::DefaultUpstream) => {
            guard.tls_dag.default_upstream.clone()
        }
        TlsSessionHandle::OutboundExchange(_) => unreachable!("handled above"),
    }
}

fn apply_cached_session(
    context: &SharedProxyVmContext,
    session: TlsSessionHandle,
) -> Result<bool, VmError> {
    let (cache, key) = {
        let guard = context.lock().expect("vm context lock poisoned");
        let Some(cache) = guard.tls_session_cache.clone() else {
            return Ok(false);
        };
        let (target, flow) = match session {
            TlsSessionHandle::Reserved(TlsSessionRef::Downstream) => return Ok(false),
            TlsSessionHandle::Reserved(TlsSessionRef::DefaultUpstream) => (
                guard.outbound_request.target.as_deref(),
                &guard.tls_dag.default_upstream,
            ),
            TlsSessionHandle::OutboundExchange(handle) => {
                let Some(exchange) = guard.outbound_exchanges.get(&handle) else {
                    return Ok(false);
                };
                (exchange.request.target.as_deref(), &exchange.tls_dag)
            }
        };
        let Some(target) = target else {
            return Ok(false);
        };
        let Some(key) = tls_session_cache_key(target, flow) else {
            return Ok(false);
        };
        (cache, key)
    };

    let cached = {
        let guard = cache.lock().expect("tls session cache lock poisoned");
        guard.get(&key).cloned()
    };
    let Some(cached) = cached else {
        return Ok(false);
    };

    let mut guard = context.lock().expect("vm context lock poisoned");
    match session {
        TlsSessionHandle::Reserved(TlsSessionRef::Downstream) => return Ok(false),
        TlsSessionHandle::Reserved(TlsSessionRef::DefaultUpstream) => {
            guard.tls_dag.default_upstream.mark_session_reused(&cached);
        }
        TlsSessionHandle::OutboundExchange(handle) => {
            let Some(exchange) = guard.outbound_exchanges.get_mut(&handle) else {
                return Ok(false);
            };
            exchange.tls_dag.mark_session_reused(&cached);
        }
    }
    Ok(true)
}

fn with_configurable_outbound_session_mut<T>(
    context: &SharedProxyVmContext,
    session: i64,
    mutate: impl FnOnce(&mut TlsFlowState) -> Result<T, VmError>,
) -> Result<T, VmError> {
    let session = decode_session(context, session)?;
    match session {
        TlsSessionHandle::Reserved(TlsSessionRef::Downstream) => Err(VmError::HostError(
            "downstream tls session is read-only".to_string(),
        )),
        TlsSessionHandle::Reserved(TlsSessionRef::DefaultUpstream) => {
            if upstream_response_available(context) {
                return Err(VmError::HostError(
                    "default upstream tls session is read-only after the exchange has started"
                        .to_string(),
                ));
            }
            let mut guard = context.lock().expect("vm context lock poisoned");
            mutate(&mut guard.tls_dag.default_upstream)
        }
        TlsSessionHandle::OutboundExchange(handle) => {
            if outbound_exchange_response_available(context, handle) {
                return Err(VmError::HostError(format!(
                    "tls session handle {handle} is read-only after the exchange has started",
                )));
            }
            let mut guard = context.lock().expect("vm context lock poisoned");
            let exchange = guard
                .outbound_exchanges
                .get_mut(&handle)
                .expect("exchange handle should exist while tls session is in use");
            mutate(&mut exchange.tls_dag)
        }
    }
}

fn parse_alpn_list(raw: &str) -> Result<Vec<String>, VmError> {
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
            "tls::session::set_alpn requires at least one non-empty protocol".to_string(),
        ));
    }
    Ok(protocols)
}

fn parse_tls_version_label(label: &str) -> Result<TlsProtocolVersion, VmError> {
    let normalized = label
        .trim()
        .to_ascii_lowercase()
        .replace("tlsv", "")
        .replace("tls", "")
        .replace('_', ".")
        .replace(' ', "");
    match normalized.as_str() {
        "1.0" => Ok(TlsProtocolVersion::Tls1_0),
        "1.1" => Ok(TlsProtocolVersion::Tls1_1),
        "1.2" => Ok(TlsProtocolVersion::Tls1_2),
        "1.3" => Ok(TlsProtocolVersion::Tls1_3),
        _ => Err(VmError::HostError(format!(
            "unsupported TLS version '{label}'; expected one of 1.0, 1.1, 1.2, 1.3",
        ))),
    }
}

#[pd_edge_host_function(name = tls::session::FROM_SOCKET.name, scope = transport)]
async fn session_from_socket(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    stream: i64,
) -> Result<CallOutcome, VmError> {
    let session = decode_session(&context, stream)?;
    let handle = match session {
        TlsSessionHandle::Reserved(reserved) => reserved.handle(),
        TlsSessionHandle::OutboundExchange(handle) => handle,
    };
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

#[pd_edge_host_function(name = tls::session::IS_PRESENT.name, scope = transport)]
async fn session_is_present(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::Bool(
        session_flow(&context, decode_session(&context, session)?).is_present(),
    )]))
}

#[pd_edge_host_function(name = tls::session::HANDSHAKE.name, scope = transport)]
async fn session_handshake(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
) -> Result<CallOutcome, VmError> {
    let session = decode_session(&context, session)?;
    let flow = session_flow(&context, session);
    if !flow.is_present() {
        return Ok(CallOutcome::Return(vec![Value::Bool(false)]));
    }
    if flow.handshake_complete() {
        return Ok(CallOutcome::Return(vec![Value::Bool(true)]));
    }
    if apply_cached_session(&context, session)? {
        return Ok(CallOutcome::Return(vec![Value::Bool(true)]));
    }

    match session {
        TlsSessionHandle::Reserved(TlsSessionRef::DefaultUpstream) => {
            if websocket_connection_mode(&context, TlsSessionRef::DefaultUpstream.handle()) {
                ensure_outbound_websocket_connection_open(
                    &context,
                    TlsSessionRef::DefaultUpstream.handle(),
                )
                .await?;
            } else {
                ensure_upstream_response_started(&context).await?;
            }
        }
        TlsSessionHandle::OutboundExchange(handle) => {
            if websocket_connection_mode(&context, handle) {
                ensure_outbound_websocket_connection_open(&context, handle).await?;
            } else {
                ensure_outbound_exchange_response_started(&context, handle).await?;
            }
        }
        TlsSessionHandle::Reserved(TlsSessionRef::Downstream) => {}
    }
    let ready = session_flow(&context, session).handshake_complete();
    Ok(CallOutcome::Return(vec![Value::Bool(ready)]))
}

#[pd_edge_host_function(name = tls::session::SET_ALPN.name, scope = transport)]
async fn session_set_alpn(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
    protocols: String,
) -> Result<CallOutcome, VmError> {
    let protocols = parse_alpn_list(&protocols)?;
    with_configurable_outbound_session_mut(&context, session, |flow| {
        flow.set_desired_alpn(protocols);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = tls::session::SET_VERIFY.name, scope = transport)]
async fn session_set_verify(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
    verify: bool,
) -> Result<CallOutcome, VmError> {
    with_configurable_outbound_session_mut(&context, session, |flow| {
        flow.set_verify_peer(verify);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = tls::session::SET_VERIFY_HOSTNAME.name, scope = transport)]
async fn session_set_verify_hostname(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
    verify: bool,
) -> Result<CallOutcome, VmError> {
    with_configurable_outbound_session_mut(&context, session, |flow| {
        flow.set_verify_hostname(verify);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = tls::session::SET_TRUSTED_CERTIFICATE.name, scope = transport)]
async fn session_set_trusted_certificate(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
    certificate_pem: String,
) -> Result<CallOutcome, VmError> {
    with_configurable_outbound_session_mut(&context, session, |flow| {
        flow.set_trusted_certificate_pem(certificate_pem);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = tls::session::SET_CERTIFICATE.name, scope = transport)]
async fn session_set_certificate(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
    certificate_pem: String,
) -> Result<CallOutcome, VmError> {
    with_configurable_outbound_session_mut(&context, session, |flow| {
        flow.set_client_certificate_pem(certificate_pem);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = tls::session::SET_PRIVATE_KEY.name, scope = transport)]
async fn session_set_private_key(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
    private_key_pem: String,
) -> Result<CallOutcome, VmError> {
    with_configurable_outbound_session_mut(&context, session, |flow| {
        flow.set_client_private_key_pem(private_key_pem);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = tls::session::SET_SNI.name, scope = transport)]
async fn session_set_sni(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
    enabled: bool,
) -> Result<CallOutcome, VmError> {
    with_configurable_outbound_session_mut(&context, session, |flow| {
        flow.set_sni_enabled(enabled);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = tls::session::SET_MIN_VERSION.name, scope = transport)]
async fn session_set_min_version(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
    version: String,
) -> Result<CallOutcome, VmError> {
    let version = parse_tls_version_label(&version)?;
    with_configurable_outbound_session_mut(&context, session, |flow| {
        flow.set_min_version(Some(version));
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = tls::session::SET_MAX_VERSION.name, scope = transport)]
async fn session_set_max_version(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
    version: String,
) -> Result<CallOutcome, VmError> {
    let version = parse_tls_version_label(&version)?;
    with_configurable_outbound_session_mut(&context, session, |flow| {
        flow.set_max_version(Some(version));
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[pd_edge_host_function(name = tls::session::GET_PEER_NAME.name, scope = transport)]
async fn session_get_peer_name(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::string(
        session_flow(&context, decode_session(&context, session)?)
            .peer_name()
            .to_string(),
    )]))
}

#[pd_edge_host_function(name = tls::session::GET_SERVER_NAME.name, scope = transport)]
async fn session_get_server_name(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::string(
        session_flow(&context, decode_session(&context, session)?)
            .server_name()
            .to_string(),
    )]))
}

#[pd_edge_host_function(name = tls::session::GET_ALPN.name, scope = transport)]
async fn session_get_alpn(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::string(
        session_flow(&context, decode_session(&context, session)?)
            .alpn()
            .to_string(),
    )]))
}

#[pd_edge_host_function(name = tls::session::GET_PHASE.name, scope = transport)]
async fn session_get_phase(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::string(
        session_flow(&context, decode_session(&context, session)?)
            .phase_label()
            .to_string(),
    )]))
}

#[pd_edge_host_function(name = tls::session::GET_PEER_CERTIFICATE.name, scope = transport)]
async fn session_get_peer_certificate(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
) -> Result<CallOutcome, VmError> {
    let encoded = session_flow(&context, decode_session(&context, session)?)
        .peer_certificate_der()
        .map(|bytes| STANDARD.encode(bytes))
        .unwrap_or_default();
    Ok(CallOutcome::Return(vec![Value::string(encoded)]))
}

#[pd_edge_host_function(name = tls::session::IS_SESSION_REUSED.name, scope = transport)]
async fn session_is_session_reused(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::Bool(
        session_flow(&context, decode_session(&context, session)?).is_session_reused(),
    )]))
}
