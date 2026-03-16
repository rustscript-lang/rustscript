use std::{
    io,
    io::BufReader,
    sync::{Arc, OnceLock},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use edge_abi::symbols::tls;
use pd_edge_host_function::pd_edge_host_function;
use tokio_rustls::{
    LazyConfigAcceptor, TlsConnector,
    rustls::{
        self, ClientConfig, RootCertStore, ServerConfig, SignatureScheme,
        client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
        server::WebPkiClientVerifier,
        version::{TLS12, TLS13},
    },
};
use vm::{CallOutcome, Value, Vm, VmError};

use super::super::SharedProxyVmContext;
use super::super::http::{
    ensure_outbound_exchange_response_started, ensure_upstream_response_started,
    outbound_exchange_exists, outbound_exchange_response_available, outbound_exchange_tls_flow,
    tcp_stream_exists, upstream_response_available,
};
use super::super::websocket::{
    ensure_outbound_websocket_connection_open, websocket_connection_mode,
};
use super::state::tls_session_cache_key;
use super::state::{
    DownstreamTlsServerStart, ReplayPrefixedIo, TlsFlowState, TlsProtocolVersion, TlsSessionRef,
    decode_tls_session_handle,
};
use crate::abi_impl::transport::HTTP11_ALPN_PROTOCOL;
use crate::lock_metrics::LockMetricKey;
use rcgen::generate_simple_self_signed;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TlsSessionHandle {
    Reserved(TlsSessionRef),
    Dynamic(i64),
    OutboundExchange(i64),
}

fn decode_session(
    context: &SharedProxyVmContext,
    session: i64,
) -> Result<TlsSessionHandle, VmError> {
    if let Some(reserved) = decode_tls_session_handle(session) {
        if matches!(reserved, TlsSessionRef::Downstream) {
            context.note_downstream_transport_access();
        }
        return Ok(TlsSessionHandle::Reserved(reserved));
    }
    if tcp_stream_exists(context, session) {
        return Ok(TlsSessionHandle::Dynamic(session));
    }
    if outbound_exchange_exists(context, session) {
        return Ok(TlsSessionHandle::OutboundExchange(session));
    }
    Err(VmError::HostError(format!(
        "invalid tls session handle {session}; use 0/1 for reserved sessions, a dynamic tcp handle from tcp::stream::new(), or an outbound exchange handle",
    )))
}

fn session_flow(context: &SharedProxyVmContext, session: TlsSessionHandle) -> TlsFlowState {
    match session {
        TlsSessionHandle::OutboundExchange(handle) => outbound_exchange_tls_flow(context, handle)
            .expect("exchange handle should exist while tls session is in use"),
        TlsSessionHandle::Dynamic(handle) => {
            let guard = context.lock_transport();
            guard
                .dynamic_tls_sessions
                .get(&handle)
                .cloned()
                .unwrap_or_else(TlsFlowState::for_dynamic_socket)
        }
        TlsSessionHandle::Reserved(TlsSessionRef::Downstream) => {
            let guard = context.lock_transport();
            guard.tls_dag.downstream.clone()
        }
        TlsSessionHandle::Reserved(TlsSessionRef::DefaultUpstream) => {
            let guard = context.lock_transport();
            guard.tls_dag.default_upstream.clone()
        }
    }
}

fn apply_cached_session(
    context: &SharedProxyVmContext,
    session: TlsSessionHandle,
) -> Result<bool, VmError> {
    let (cache, key) = {
        let Some(cache) = context.services().tls_session_cache() else {
            return Ok(false);
        };
        let (target, flow) = match session {
            TlsSessionHandle::Reserved(TlsSessionRef::Downstream) => return Ok(false),
            TlsSessionHandle::Reserved(TlsSessionRef::DefaultUpstream) => {
                let target = context
                    .with_default_upstream_exchange(|exchange| exchange.request.target.clone());
                let flow = {
                    let transport = context.lock_transport();
                    transport.tls_dag.default_upstream.clone()
                };
                (target, flow)
            }
            TlsSessionHandle::Dynamic(_) => return Ok(false),
            TlsSessionHandle::OutboundExchange(handle) => {
                let exchanges = context.lock_exchanges();
                let Some(exchange) = exchanges.exchanges.get(&handle) else {
                    return Ok(false);
                };
                (
                    exchange.request.target.clone(),
                    exchange.transport.tls_flow.clone(),
                )
            }
        };
        let Some(target) = target else {
            return Ok(false);
        };
        let Some(key) = tls_session_cache_key(&target, &flow) else {
            return Ok(false);
        };
        (cache, key)
    };

    let cached = {
        cache.peek_cloned(
            &key,
            LockMetricKey::TlsSessionCache,
            "tls session cache lock poisoned",
        )
    };
    let Some(cached) = cached else {
        return Ok(false);
    };

    match session {
        TlsSessionHandle::Reserved(TlsSessionRef::Downstream) => return Ok(false),
        TlsSessionHandle::Reserved(TlsSessionRef::DefaultUpstream) => {
            context
                .lock_transport()
                .tls_dag
                .default_upstream
                .mark_session_reused(&cached);
        }
        TlsSessionHandle::Dynamic(handle) => {
            let mut guard = context.lock_transport();
            let flow = guard
                .dynamic_tls_sessions
                .entry(handle)
                .or_insert_with(TlsFlowState::for_dynamic_socket);
            flow.mark_session_reused(&cached);
        }
        TlsSessionHandle::OutboundExchange(handle) => {
            let mut exchanges = context.lock_exchanges();
            let Some(exchange) = exchanges.exchanges.get_mut(&handle) else {
                return Ok(false);
            };
            exchange.transport.tls_flow.mark_session_reused(&cached);
        }
    }
    Ok(true)
}

fn with_configurable_session_mut<T>(
    context: &SharedProxyVmContext,
    session: i64,
    mutate: impl FnOnce(&mut TlsFlowState) -> Result<T, VmError>,
) -> Result<T, VmError> {
    let session = decode_session(context, session)?;
    match session {
        TlsSessionHandle::Reserved(TlsSessionRef::Downstream) => {
            if !downstream_tls_session_is_configurable(context) {
                return Err(VmError::HostError(
                    "downstream tls session is read-only in the current runtime state".to_string(),
                ));
            }
            let mut guard = context.lock_transport();
            mutate(&mut guard.tls_dag.downstream)
        }
        TlsSessionHandle::Reserved(TlsSessionRef::DefaultUpstream) => {
            if upstream_response_available(context) {
                return Err(VmError::HostError(
                    "default upstream tls session is read-only after the exchange has started"
                        .to_string(),
                ));
            }
            let mut guard = context.lock_transport();
            mutate(&mut guard.tls_dag.default_upstream)
        }
        TlsSessionHandle::Dynamic(handle) => {
            let mut guard = context.lock_transport();
            let io_present = guard.dynamic_tls_session_ios.contains_key(&handle);
            let flow = guard
                .dynamic_tls_sessions
                .entry(handle)
                .or_insert_with(TlsFlowState::for_dynamic_socket);
            if flow.handshake_complete() || io_present {
                return Err(VmError::HostError(format!(
                    "dynamic tls session handle {handle} is read-only after the handshake completes",
                )));
            }
            mutate(flow)
        }
        TlsSessionHandle::OutboundExchange(handle) => {
            if outbound_exchange_response_available(context, handle) {
                return Err(VmError::HostError(format!(
                    "tls session handle {handle} is read-only after the exchange has started",
                )));
            }
            let mut guard = context.lock_exchanges();
            let exchange = guard
                .exchanges
                .get_mut(&handle)
                .expect("exchange handle should exist while tls session is in use");
            mutate(&mut exchange.transport.tls_flow)
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

fn ensure_rustls_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

#[derive(Clone, Debug)]
struct DefaultSelfSignedServerIdentity {
    certificate_chain_der: Vec<Vec<u8>>,
    private_key_der: Vec<u8>,
}

impl DefaultSelfSignedServerIdentity {
    fn generate() -> Result<Self, String> {
        let certificate =
            generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
                .map_err(|err| format!("failed to generate self-signed cert: {err}"))?;
        let certificate_der = certificate
            .serialize_der()
            .map_err(|err| format!("failed to serialize cert der: {err}"))?;
        Ok(Self {
            certificate_chain_der: vec![certificate_der],
            private_key_der: certificate.serialize_private_key_der(),
        })
    }

    fn certificate_chain(&self) -> Vec<CertificateDer<'static>> {
        self.certificate_chain_der
            .iter()
            .cloned()
            .map(CertificateDer::from)
            .collect()
    }

    fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.private_key_der.clone()))
    }
}

fn default_self_signed_server_identity() -> Result<Arc<DefaultSelfSignedServerIdentity>, String> {
    static IDENTITY: OnceLock<Result<Arc<DefaultSelfSignedServerIdentity>, String>> =
        OnceLock::new();
    IDENTITY
        .get_or_init(|| {
            ensure_rustls_provider();
            DefaultSelfSignedServerIdentity::generate().map(Arc::new)
        })
        .clone()
}

pub(crate) fn build_default_self_signed_server_config(
    alpn_protocols: Vec<Vec<u8>>,
) -> io::Result<Arc<ServerConfig>> {
    let identity = default_self_signed_server_identity().map_err(io::Error::other)?;
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(identity.certificate_chain(), identity.private_key())
        .map_err(|err| io::Error::other(format!("failed to build rustls config: {err}")))?;
    config.alpn_protocols = alpn_protocols;
    Ok(Arc::new(config))
}

fn build_root_store(flow: &TlsFlowState) -> Result<RootCertStore, VmError> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(bundle) = flow.trusted_certificate_pem() {
        let mut reader = BufReader::new(bundle.as_bytes());
        for certificate in rustls_pemfile::certs(&mut reader) {
            let certificate = certificate.map_err(|err| {
                VmError::HostError(format!("failed to parse trusted certificate bundle: {err}"))
            })?;
            roots.add(certificate).map_err(|err| {
                VmError::HostError(format!(
                    "failed to add trusted certificate to root store: {err}",
                ))
            })?;
        }
    }
    Ok(roots)
}

fn load_client_cert_chain(
    certificate_pem: Option<&str>,
) -> Result<Vec<CertificateDer<'static>>, VmError> {
    let Some(certificate_pem) = certificate_pem else {
        return Ok(Vec::new());
    };
    let mut reader = BufReader::new(certificate_pem.as_bytes());
    let chain = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, io::Error>>()
        .map_err(|err| {
            VmError::HostError(format!("failed to parse client certificate chain: {err}"))
        })?;
    if chain.is_empty() {
        return Err(VmError::HostError(
            "client certificate chain is empty".to_string(),
        ));
    }
    Ok(chain)
}

fn load_client_private_key(
    private_key_pem: Option<&str>,
) -> Result<rustls::pki_types::PrivateKeyDer<'static>, VmError> {
    let Some(private_key_pem) = private_key_pem else {
        return Err(VmError::HostError(
            "client private key is unavailable".to_string(),
        ));
    };
    let mut reader = BufReader::new(private_key_pem.as_bytes());
    rustls_pemfile::private_key(&mut reader)
        .map_err(|err| VmError::HostError(format!("failed to parse client private key: {err}")))?
        .ok_or_else(|| VmError::HostError("client private key is unavailable".to_string()))
}

fn supported_protocol_versions(
    flow: &TlsFlowState,
) -> Result<Vec<&'static rustls::SupportedProtocolVersion>, VmError> {
    if matches!(
        flow.min_version(),
        Some(TlsProtocolVersion::Tls1_0) | Some(TlsProtocolVersion::Tls1_1)
    ) || matches!(
        flow.max_version(),
        Some(TlsProtocolVersion::Tls1_0) | Some(TlsProtocolVersion::Tls1_1)
    ) {
        return Err(VmError::HostError(
            "manual tls transport only supports TLS 1.2 or newer".to_string(),
        ));
    }
    let min = flow.min_version().unwrap_or(TlsProtocolVersion::Tls1_2);
    let max = flow.max_version().unwrap_or(TlsProtocolVersion::Tls1_3);
    if min > max {
        return Err(VmError::HostError(
            "tls min version cannot be greater than max version".to_string(),
        ));
    }

    let mut versions = Vec::new();
    if min <= TlsProtocolVersion::Tls1_2 && max >= TlsProtocolVersion::Tls1_2 {
        versions.push(&TLS12);
    }
    if min <= TlsProtocolVersion::Tls1_3 && max >= TlsProtocolVersion::Tls1_3 {
        versions.push(&TLS13);
    }
    if versions.is_empty() {
        return Err(VmError::HostError(
            "tls version constraints left no supported protocol versions".to_string(),
        ));
    }
    Ok(versions)
}

struct PermissiveServerCertVerifier {
    delegate: Arc<dyn ServerCertVerifier>,
}

impl std::fmt::Debug for PermissiveServerCertVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PermissiveServerCertVerifier")
    }
}

impl PermissiveServerCertVerifier {
    fn new(roots: RootCertStore) -> Self {
        let delegate = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
            .build()
            .expect("webpki verifier should build");
        Self { delegate }
    }
}

impl ServerCertVerifier for PermissiveServerCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.delegate.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.delegate.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.delegate.supported_verify_schemes()
    }
}

fn build_dynamic_client_config(flow: &TlsFlowState) -> Result<ClientConfig, VmError> {
    ensure_rustls_provider();
    let versions = supported_protocol_versions(flow)?;
    let builder =
        ClientConfig::builder_with_provider(rustls::crypto::aws_lc_rs::default_provider().into())
            .with_protocol_versions(&versions)
            .map_err(|err| {
                VmError::HostError(format!("failed to configure tls versions: {err}"))
            })?;

    let mut config = if flow.verify_peer() && flow.verify_hostname() {
        match (flow.client_certificate_pem(), flow.client_private_key_pem()) {
            (Some(_), Some(_)) => builder
                .with_root_certificates(build_root_store(flow)?)
                .with_client_auth_cert(
                    load_client_cert_chain(flow.client_certificate_pem())?,
                    load_client_private_key(flow.client_private_key_pem())?,
                )
                .map_err(|err| {
                    VmError::HostError(format!(
                        "failed to configure client certificate for tls session: {err}",
                    ))
                })?,
            (Some(_), None) | (None, Some(_)) => {
                return Err(VmError::HostError(
                    "client certificate and private key must both be configured".to_string(),
                ));
            }
            (None, None) => builder
                .with_root_certificates(build_root_store(flow)?)
                .with_no_client_auth(),
        }
    } else {
        let verifier = Arc::new(PermissiveServerCertVerifier::new(build_root_store(flow)?));
        let dangerous = builder
            .dangerous()
            .with_custom_certificate_verifier(verifier);
        match (flow.client_certificate_pem(), flow.client_private_key_pem()) {
            (Some(_), Some(_)) => dangerous
                .with_client_auth_cert(
                    load_client_cert_chain(flow.client_certificate_pem())?,
                    load_client_private_key(flow.client_private_key_pem())?,
                )
                .map_err(|err| {
                    VmError::HostError(format!(
                        "failed to configure client certificate for tls session: {err}",
                    ))
                })?,
            (Some(_), None) | (None, Some(_)) => {
                return Err(VmError::HostError(
                    "client certificate and private key must both be configured".to_string(),
                ));
            }
            (None, None) => dangerous.with_no_client_auth(),
        }
    };

    config.enable_sni = flow.sni_enabled();
    config.alpn_protocols = if !flow.desired_alpn().is_empty() {
        flow.desired_alpn()
            .iter()
            .map(|protocol| protocol.as_bytes().to_vec())
            .collect()
    } else {
        vec![HTTP11_ALPN_PROTOCOL.as_bytes().to_vec()]
    };
    Ok(config)
}

fn build_downstream_server_identity(
    flow: &TlsFlowState,
) -> Result<
    (
        Vec<CertificateDer<'static>>,
        rustls::pki_types::PrivateKeyDer<'static>,
    ),
    VmError,
> {
    match (flow.server_certificate_pem(), flow.server_private_key_pem()) {
        (Some(_), Some(_)) => Ok((
            load_client_cert_chain(flow.server_certificate_pem())?,
            load_client_private_key(flow.server_private_key_pem())?,
        )),
        (Some(_), None) | (None, Some(_)) => Err(VmError::HostError(
            "downstream tls handshake requires both server certificate and private key when either is configured".to_string(),
        )),
        (None, None) => {
            // Reuse one fallback identity for the lifetime of the process instead of
            // regenerating a fresh self-signed certificate on every downstream handshake.
            let identity =
                default_self_signed_server_identity().map_err(VmError::HostError)?;
            Ok((identity.certificate_chain(), identity.private_key()))
        }
    }
}

fn build_downstream_server_config(flow: &TlsFlowState) -> Result<ServerConfig, VmError> {
    ensure_rustls_provider();
    let versions = supported_protocol_versions(flow)?;
    let builder =
        ServerConfig::builder_with_provider(rustls::crypto::aws_lc_rs::default_provider().into())
            .with_protocol_versions(&versions)
            .map_err(|err| {
                VmError::HostError(format!("failed to configure tls versions: {err}"))
            })?;

    let builder = if flow.verify_peer() && flow.trusted_certificate_pem().is_some() {
        let verifier = WebPkiClientVerifier::builder(build_root_store(flow)?.into())
            .build()
            .map_err(|err| {
                VmError::HostError(format!(
                    "failed to configure downstream client certificate verifier: {err}",
                ))
            })?;
        builder.with_client_cert_verifier(verifier)
    } else {
        builder.with_no_client_auth()
    };

    let (certificate_chain, private_key) = build_downstream_server_identity(flow)?;
    let mut config = builder
        .with_single_cert(certificate_chain, private_key)
        .map_err(|err| {
            VmError::HostError(format!(
                "failed to configure downstream tls server certificate: {err}",
            ))
        })?;
    config.alpn_protocols = flow
        .desired_alpn()
        .iter()
        .map(|protocol| protocol.as_bytes().to_vec())
        .collect();
    Ok(config)
}

fn downstream_tls_session_is_configurable(context: &SharedProxyVmContext) -> bool {
    let guard = context.lock_transport();
    (guard.downstream_tcp_io.is_some() || guard.downstream_tls_server_start.is_some())
        && !guard.tls_dag.downstream.handshake_complete()
}

async fn take_dynamic_tcp_stream_for_tls(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<tokio::net::TcpStream, VmError> {
    let io = {
        let mut guard = context.lock_transport();
        guard.tcp_stream_ios.remove(&handle).ok_or_else(|| {
            VmError::HostError(format!(
                "dynamic tcp stream handle {handle} must be connected before starting tls",
            ))
        })?
    };

    let mut guard = io.lock().await;
    guard.take().ok_or_else(|| {
        VmError::HostError(format!(
            "dynamic tcp stream handle {handle} is already in use",
        ))
    })
}

/// Creates a TLS session handle from a connected TCP stream.
#[pd_edge_host_function(name = tls::session::FROM_SOCKET.name, scope = transport)]
async fn session_from_socket(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    stream: i64,
) -> Result<CallOutcome, VmError> {
    let session = decode_session(&context, stream)?;
    let handle = match session {
        TlsSessionHandle::Reserved(TlsSessionRef::Downstream) => {
            let pending_or_open = {
                let guard = context.lock_transport();
                guard.downstream_tls_server_start.is_some()
                    || guard.downstream_tls_io.is_some()
                    || (guard.downstream_tcp_io.is_none() && guard.tls_dag.downstream.is_present())
            };
            if pending_or_open {
                TlsSessionRef::Downstream.handle()
            } else {
                let (raw_io, preread) = {
                    let mut guard = context.lock_transport();
                    let raw_io = guard.downstream_tcp_io.take().ok_or_else(|| {
                        VmError::HostError(
                            "downstream tls session requires an attached downstream tcp transport"
                                .to_string(),
                        )
                    })?;
                    let preread = std::mem::take(&mut guard.downstream_preread_buffer);
                    (raw_io, preread)
                };
                let tcp_stream = {
                    let mut guard = raw_io.lock().await;
                    guard.take().ok_or_else(|| {
                        VmError::HostError("downstream tcp transport is already in use".to_string())
                    })?
                };
                let mut acceptor = Box::pin(LazyConfigAcceptor::new(
                    rustls::server::Acceptor::default(),
                    ReplayPrefixedIo::new(preread, tcp_stream),
                ));
                match acceptor.as_mut().await {
                    Ok(start) => {
                        let start = DownstreamTlsServerStart::new(start);
                        let server_name = start.client_hello_server_name();
                        let mut guard = context.lock_transport();
                        guard
                            .tls_dag
                            .downstream
                            .observe_downstream_client_hello(server_name);
                        guard.downstream_tls_server_start = Some(start);
                        TlsSessionRef::Downstream.handle()
                    }
                    Err(err) => {
                        if let Some(stream) = acceptor.as_mut().get_mut().take_io() {
                            let (prefix, raw_stream) = stream.into_parts();
                            {
                                let mut raw_guard = raw_io.lock().await;
                                *raw_guard = Some(raw_stream);
                            }
                            let mut guard = context.lock_transport();
                            guard.downstream_tcp_io = Some(raw_io);
                            guard.downstream_preread_buffer = prefix;
                        }
                        return Err(VmError::HostError(format!(
                            "failed to observe downstream tls client hello: {err}",
                        )));
                    }
                }
            }
        }
        TlsSessionHandle::Reserved(reserved) => reserved.handle(),
        TlsSessionHandle::Dynamic(handle) => {
            let mut guard = context.lock_transport();
            let target = guard
                .tcp_streams
                .get(&handle)
                .and_then(|state| state.target().map(str::to_string));
            let flow = guard
                .dynamic_tls_sessions
                .entry(handle)
                .or_insert_with(TlsFlowState::for_dynamic_socket);
            if let Some(target) = target {
                flow.observe_socket_target(&target);
            }
            handle
        }
        TlsSessionHandle::OutboundExchange(handle) => handle,
    };
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

/// Returns whether the TLS session handle is present.
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

/// Runs the TLS handshake for the TLS session.
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
        TlsSessionHandle::Dynamic(handle) => {
            let flow = session_flow(&context, session);
            let peer_name = if flow.peer_name().is_empty() {
                return Err(VmError::HostError(format!(
                    "dynamic tls session handle {handle} has no peer name; attach it to an http exchange with a target before calling tls::session::handshake",
                )));
            } else {
                flow.peer_name().to_string()
            };
            let server_name = ServerName::try_from(peer_name.clone()).map_err(|err| {
                VmError::HostError(format!(
                    "invalid tls peer name '{peer_name}' for dynamic session {handle}: {err}",
                ))
            })?;

            {
                let mut guard = context.lock_transport();
                let flow = guard
                    .dynamic_tls_sessions
                    .entry(handle)
                    .or_insert_with(TlsFlowState::for_dynamic_socket);
                flow.note_handshake_prepared();
                flow.note_client_hello_sent();
            }

            let tcp_stream = take_dynamic_tcp_stream_for_tls(&context, handle).await?;
            let config = build_dynamic_client_config(&flow)?;
            let connector = TlsConnector::from(Arc::new(config));
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

                    {
                        let mut guard = context.lock_transport();
                        if let Some(flow) = guard.dynamic_tls_sessions.get_mut(&handle) {
                            flow.note_server_hello_received();
                            flow.note_server_certificate_received(peer_certificate_der.clone());
                            if flow.verify_peer() && flow.verify_hostname() {
                                flow.note_server_certificate_verified();
                            } else {
                                flow.note_verification_skipped();
                            }
                            if !flow.accepts_negotiated_alpn(negotiated_alpn.as_deref()) {
                                flow.mark_failed();
                                return Err(VmError::HostError(format!(
                                    "tls ALPN mismatch: requested [{}], negotiated {}",
                                    flow.desired_alpn().join(", "),
                                    negotiated_alpn.as_deref().unwrap_or("none"),
                                )));
                            }
                            flow.mark_handshake_complete(negotiated_alpn.clone());
                        }
                        if let Some(state) = guard.tcp_streams.get_mut(&handle) {
                            state.mark_upgraded_tls();
                        }
                        guard
                            .dynamic_tls_session_ios
                            .insert(handle, Arc::new(tokio::sync::Mutex::new(Some(tls_stream))));
                    }
                }
                Err(err) => {
                    let mut guard = context.lock_transport();
                    if let Some(flow) = guard.dynamic_tls_sessions.get_mut(&handle) {
                        flow.mark_failed();
                    }
                    if let Some(state) = guard.tcp_streams.get_mut(&handle) {
                        state.mark_failed(format!("tls handshake failed: {err}"));
                    }
                    return Err(VmError::HostError(format!(
                        "dynamic tls handshake failed for handle {handle}: {err}",
                    )));
                }
            }
        }
        TlsSessionHandle::Reserved(TlsSessionRef::Downstream) => {
            let start = {
                let mut guard = context.lock_transport();
                guard.downstream_tls_server_start.take().ok_or_else(|| {
                    VmError::HostError(
                        "downstream tls session has no pending client hello; call tls::session::from_socket on the downstream tcp stream first".to_string(),
                    )
                })?
            };
            let flow = session_flow(&context, session);
            let config = build_downstream_server_config(&flow)?;
            match start.into_inner().into_stream(Arc::new(config)).await {
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

                    let mut guard = context.lock_transport();
                    guard.tcp_dag.downstream.mark_connected();
                    guard.downstream_read_eof = false;
                    let flow = &mut guard.tls_dag.downstream;
                    flow.note_server_hello_received();
                    flow.note_server_certificate_received(peer_certificate_der.clone());
                    if flow.verify_peer() && flow.trusted_certificate_pem().is_some() {
                        flow.note_server_certificate_verified();
                    } else {
                        flow.note_verification_skipped();
                    }
                    if !flow.accepts_negotiated_alpn(negotiated_alpn.as_deref()) {
                        flow.mark_failed();
                        return Err(VmError::HostError(format!(
                            "downstream tls ALPN mismatch: requested [{}], negotiated {}",
                            flow.desired_alpn().join(", "),
                            negotiated_alpn.as_deref().unwrap_or("none"),
                        )));
                    }
                    flow.mark_handshake_complete(negotiated_alpn);
                    guard.downstream_tls_io =
                        Some(Arc::new(tokio::sync::Mutex::new(Some(tls_stream))));
                }
                Err(err) => {
                    let mut guard = context.lock_transport();
                    guard
                        .tcp_dag
                        .downstream
                        .mark_failed(format!("downstream tls handshake failed: {err}",));
                    guard.tls_dag.downstream.mark_failed();
                    return Err(VmError::HostError(format!(
                        "downstream tls handshake failed: {err}",
                    )));
                }
            }
        }
    }
    let ready = session_flow(&context, session).handshake_complete();
    Ok(CallOutcome::Return(vec![Value::Bool(ready)]))
}

/// Sets the ALPN protocol list for the TLS session.
#[pd_edge_host_function(name = tls::session::SET_ALPN.name, scope = transport)]
async fn session_set_alpn(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
    protocols: String,
) -> Result<CallOutcome, VmError> {
    let protocols = parse_alpn_list(&protocols)?;
    with_configurable_session_mut(&context, session, |flow| {
        flow.set_desired_alpn(protocols);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Enables or disables certificate verification for the TLS session.
#[pd_edge_host_function(name = tls::session::SET_VERIFY.name, scope = transport)]
async fn session_set_verify(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
    verify: bool,
) -> Result<CallOutcome, VmError> {
    with_configurable_session_mut(&context, session, |flow| {
        flow.set_verify_peer(verify);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Enables or disables hostname verification for the TLS session.
#[pd_edge_host_function(name = tls::session::SET_VERIFY_HOSTNAME.name, scope = transport)]
async fn session_set_verify_hostname(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
    verify: bool,
) -> Result<CallOutcome, VmError> {
    with_configurable_session_mut(&context, session, |flow| {
        flow.set_verify_hostname(verify);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets a trusted CA certificate for the TLS session.
#[pd_edge_host_function(name = tls::session::SET_TRUSTED_CERTIFICATE.name, scope = transport)]
async fn session_set_trusted_certificate(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
    certificate_pem: String,
) -> Result<CallOutcome, VmError> {
    with_configurable_session_mut(&context, session, |flow| {
        flow.set_trusted_certificate_pem(certificate_pem);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the client certificate for the TLS session.
#[pd_edge_host_function(name = "tls::session::set_client_certificate", scope = transport)]
async fn session_set_client_certificate(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
    certificate_pem: String,
) -> Result<CallOutcome, VmError> {
    with_configurable_session_mut(&context, session, |flow| {
        flow.set_client_certificate_pem(certificate_pem);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the client private key for the TLS session.
#[pd_edge_host_function(name = "tls::session::set_client_private_key", scope = transport)]
async fn session_set_client_private_key(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
    private_key_pem: String,
) -> Result<CallOutcome, VmError> {
    with_configurable_session_mut(&context, session, |flow| {
        flow.set_client_private_key_pem(private_key_pem);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the server certificate for the TLS session.
#[pd_edge_host_function(name = "tls::session::set_server_certificate", scope = transport)]
async fn session_set_server_certificate(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
    certificate_pem: String,
) -> Result<CallOutcome, VmError> {
    with_configurable_session_mut(&context, session, |flow| {
        flow.set_server_certificate_pem(certificate_pem);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the server private key for the TLS session.
#[pd_edge_host_function(name = "tls::session::set_server_private_key", scope = transport)]
async fn session_set_server_private_key(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
    private_key_pem: String,
) -> Result<CallOutcome, VmError> {
    with_configurable_session_mut(&context, session, |flow| {
        flow.set_server_private_key_pem(private_key_pem);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Enables or disables SNI for the TLS session.
#[pd_edge_host_function(name = tls::session::SET_SNI.name, scope = transport)]
async fn session_set_sni(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
    enabled: bool,
) -> Result<CallOutcome, VmError> {
    with_configurable_session_mut(&context, session, |flow| {
        flow.set_sni_enabled(enabled);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the minimum TLS version for the TLS session.
#[pd_edge_host_function(name = tls::session::SET_MIN_VERSION.name, scope = transport)]
async fn session_set_min_version(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
    version: String,
) -> Result<CallOutcome, VmError> {
    let version = parse_tls_version_label(&version)?;
    with_configurable_session_mut(&context, session, |flow| {
        flow.set_min_version(Some(version));
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the maximum TLS version for the TLS session.
#[pd_edge_host_function(name = tls::session::SET_MAX_VERSION.name, scope = transport)]
async fn session_set_max_version(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
    version: String,
) -> Result<CallOutcome, VmError> {
    let version = parse_tls_version_label(&version)?;
    with_configurable_session_mut(&context, session, |flow| {
        flow.set_max_version(Some(version));
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Returns the peer certificate name for the TLS session.
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

/// Returns the negotiated ALPN protocol for the TLS session.
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

/// Returns the current phase for the TLS session.
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

/// Returns the peer certificate for the TLS session.
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

/// Returns whether the TLS session reused a previous TLS session.
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
