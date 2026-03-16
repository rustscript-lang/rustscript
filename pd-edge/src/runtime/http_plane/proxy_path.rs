use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{Response, StatusCode},
    middleware,
    routing::any,
};
#[cfg(feature = "http3")]
use futures_util::stream::try_unfold;
#[cfg(feature = "http3")]
use http_body_util::BodyExt;
#[cfg(feature = "http3")]
use hyper::body::Buf;
use hyper::body::Incoming;
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder as AutoBuilder,
};
use tokio::net::TcpListener;
#[cfg(feature = "http3")]
use tokio::net::UdpSocket;
use tokio::sync::{Mutex as AsyncMutex, oneshot};
#[cfg(feature = "tls")]
use tokio_rustls::{
    LazyConfigAcceptor, TlsAcceptor,
    rustls::{self, ServerConfig},
};
#[cfg(feature = "http3")]
use tower::Service;
use tower::ServiceExt;
use tracing::warn;
use uuid::Uuid;
use vm::VmError;

#[cfg(feature = "tls")]
use super::super::transport_plane::serve_transport_connection_with_listener_goal;
use super::super::vm_runner::{VmDebugInvocation, VmExecutionError, execute_vm_with_context};
use super::super::{LoadedProgram, SharedState};
use super::shared::access_log_middleware;
#[cfg(feature = "http3")]
use crate::abi_impl::{
    DownstreamHttp3ConnectionTracker, Http3DownstreamStreamAttachment, build_quic_server_config,
};
use crate::{
    abi_impl::http::{
        DownstreamConnectionMetadata, DownstreamHttpListenerGoal, InlineDownstreamHttpResponse,
        PromotedDownstreamTransport, build_downstream_http_request_context,
        resolve_http_graph_response, take_promoted_downstream_transport,
    },
    abi_impl::{
        DownstreamHttp2ConnectionTracker, Http2DownstreamStreamAttachment, ProxyVmContext,
        SharedProxyVmContext, build_default_self_signed_server_config,
        register_http_plane_host_module,
    },
    debug_session::{request_uses_blocking_debugger, request_will_attach_debugger},
    logging::category_program,
};
#[cfg(feature = "http3")]
use {
    axum::body::Bytes,
    rcgen::generate_simple_self_signed,
    rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
};

pub fn build_http_proxy_app(state: SharedState) -> Router {
    Router::new()
        .fallback(any(data_plane_handler))
        .layer(middleware::from_fn(access_log_middleware))
        .with_state(state)
}

async fn serve_http_connection<S>(
    app: Router,
    state: SharedState,
    stream: S,
    peer_addr: SocketAddr,
    connection_metadata: Option<DownstreamConnectionMetadata>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let io = TokioIo::new(stream);
    let tracker = DownstreamHttp2ConnectionTracker::new(
        state.downstream_http2_sessions.clone(),
        peer_addr.to_string(),
    );
    let service = {
        let connection_tracker = tracker.clone();
        hyper::service::service_fn(move |request: hyper::Request<Incoming>| {
            let app = app.clone();
            let tracker = connection_tracker.clone();
            let connection_metadata = connection_metadata.clone();
            async move {
                let version = request.version();
                let path = request.uri().path().to_string();
                let attachment = tracker.observe_request(version, &path);
                let mut request = request;
                let on_upgrade = hyper::upgrade::on(&mut request);
                if let Some(ref attachment) = attachment {
                    request.extensions_mut().insert(attachment.clone());
                }
                if let Some(connection_metadata) = connection_metadata {
                    request.extensions_mut().insert(connection_metadata);
                }
                request.extensions_mut().insert(on_upgrade);
                let request = request.map(Body::new);
                let response = app.oneshot(request).await;
                if response.is_ok() {
                    tracker.note_response_head(attachment.as_ref());
                    tracker.finish_request(attachment.as_ref(), None);
                } else {
                    tracker.finish_request(
                        attachment.as_ref(),
                        Some("data plane request handling failed".to_string()),
                    );
                }
                response
            }
        })
    };

    let result = AutoBuilder::new(TokioExecutor::new())
        .serve_connection_with_upgrades(io, service)
        .await;
    tracker.finish_connection(result.err().map(|err| err.to_string()));
}

#[cfg(feature = "http3")]
fn generate_http3_proxy_quic_server_config() -> std::io::Result<quinn::ServerConfig> {
    let certificate =
        generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .map_err(|err| {
                std::io::Error::other(format!(
                    "failed to generate self-signed http3 proxy cert: {err}"
                ))
            })?;
    let certificate_der = certificate
        .serialize_der()
        .map_err(|err| std::io::Error::other(format!("failed to serialize cert der: {err}")))?;
    let private_key_der = certificate.serialize_private_key_der();
    build_quic_server_config(
        vec![CertificateDer::from(certificate_der)],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(private_key_der)),
        vec![b"h3".to_vec()],
    )
}

#[cfg(feature = "http3")]
type Http3ServerSendStream = h3::server::RequestStream<h3_quinn::SendStream<Bytes>, Bytes>;

#[cfg(feature = "http3")]
type Http3ServerRecvStream = h3::server::RequestStream<h3_quinn::RecvStream, Bytes>;

#[cfg(feature = "http3")]
fn body_from_http3_request_stream(stream: Http3ServerRecvStream) -> Body {
    Body::from_stream(try_unfold(stream, |mut stream| async move {
        match stream.recv_data().await {
            Ok(Some(mut chunk)) => {
                Ok::<_, std::io::Error>(Some((chunk.copy_to_bytes(chunk.remaining()), stream)))
            }
            Ok(None) => Ok(None),
            Err(err) => Err(std::io::Error::other(format!(
                "failed to read downstream http3 request body: {err}",
            ))),
        }
    }))
}

#[cfg(feature = "http3")]
async fn write_http3_response(
    stream: &mut Http3ServerSendStream,
    response: Response<Body>,
) -> Result<(), String> {
    let (parts, mut body) = response.into_parts();
    let mut head = axum::http::Response::builder().status(parts.status);
    for (name, value) in &parts.headers {
        head = head.header(name, value);
    }
    let head = head
        .body(())
        .map_err(|err| format!("failed to build downstream http3 response: {err}"))?;
    stream
        .send_response(head)
        .await
        .map_err(|err| format!("failed to send downstream http3 response head: {err}"))?;

    while let Some(frame) = body.frame().await {
        let frame =
            frame.map_err(|err| format!("failed to read downstream response body frame: {err}"))?;
        match frame.into_data() {
            Ok(bytes) => {
                if !bytes.is_empty() {
                    stream.send_data(bytes).await.map_err(|err| {
                        format!("failed to send downstream http3 response body: {err}")
                    })?;
                }
            }
            Err(frame) => {
                if let Ok(trailers) = frame.into_trailers() {
                    stream.send_trailers(trailers).await.map_err(|err| {
                        format!("failed to send downstream http3 response trailers: {err}")
                    })?;
                }
            }
        }
    }

    stream
        .finish()
        .await
        .map_err(|err| format!("failed to finalize downstream http3 response: {err}"))
}

#[cfg(feature = "http3")]
async fn serve_http3_connection(
    app: Router,
    state: SharedState,
    local_addr: SocketAddr,
    connection: quinn::Connection,
) {
    let peer_addr = connection.remote_address();
    let connection_metadata = DownstreamConnectionMetadata {
        local_addr,
        peer_addr,
        secure: true,
    };
    let tracker = DownstreamHttp3ConnectionTracker::new(
        state.downstream_http3_sessions.clone(),
        peer_addr.to_string(),
    );
    let mut h3_conn = match h3::server::builder()
        .build(h3_quinn::Connection::new(connection))
        .await
    {
        Ok(connection) => connection,
        Err(err) => {
            tracker.finish_connection(Some(err.to_string()));
            return;
        }
    };

    loop {
        match h3_conn.accept().await {
            Ok(Some(resolver)) => {
                let mut app = app.clone();
                let tracker = tracker.clone();
                let connection_metadata = connection_metadata.clone();
                tokio::spawn(async move {
                    let (request, stream) = match resolver.resolve_request().await {
                        Ok(value) => value,
                        Err(err) => {
                            warn!(
                                "{} downstream http3 request resolution failed: {err}",
                                category_program()
                            );
                            return;
                        }
                    };
                    let stream_id = stream.id().into_inner();
                    let attachment = tracker.observe_request(request.uri().path(), stream_id);
                    let (parts, _) = request.into_parts();
                    let (mut send_stream, recv_stream) = stream.split();
                    let mut request =
                        Request::from_parts(parts, body_from_http3_request_stream(recv_stream));
                    if let Some(ref attachment) = attachment {
                        request.extensions_mut().insert(attachment.clone());
                    }
                    request.extensions_mut().insert(connection_metadata);

                    match app.call(request).await {
                        Ok(response) => {
                            tracker.note_response_head(attachment.as_ref());
                            match write_http3_response(&mut send_stream, response).await {
                                Ok(()) => tracker.finish_request(attachment.as_ref(), None),
                                Err(err) => {
                                    tracker.finish_request(attachment.as_ref(), Some(err.clone()));
                                    warn!(
                                        "{} downstream http3 response write failed: {err}",
                                        category_program()
                                    );
                                }
                            }
                        }
                        Err(err) => {
                            let message = format!("data plane request handling failed: {err}");
                            tracker.finish_request(attachment.as_ref(), Some(message.clone()));
                            let _ = write_http3_response(
                                &mut send_stream,
                                text_response(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    "internal server error",
                                ),
                            )
                            .await;
                        }
                    }
                });
            }
            Ok(None) => {
                tracker.finish_connection(None);
                break;
            }
            Err(err) => {
                let error = if err.is_h3_no_error() {
                    None
                } else {
                    Some(err.to_string())
                };
                tracker.finish_connection(error.clone());
                if let Some(message) = error {
                    warn!(
                        "{} downstream http3 connection closed with error: {message}",
                        category_program()
                    );
                }
                break;
            }
        }
    }
}

#[cfg(feature = "tls")]
fn generate_https_proxy_tls_server_config() -> std::io::Result<std::sync::Arc<ServerConfig>> {
    #[cfg(feature = "http2")]
    let alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    #[cfg(not(feature = "http2"))]
    let alpn_protocols = vec![b"http/1.1".to_vec()];
    build_default_self_signed_server_config(alpn_protocols)
}

#[cfg(feature = "tls")]
fn https_listener_needs_transport_handoff(program: Option<&LoadedProgram>) -> bool {
    let Some(program) = program else {
        return false;
    };

    let mut imports_attach_transport = false;
    let mut imports_downstream_tcp = false;
    let mut imports_transport_prelude = false;

    for import in &program.program.imports {
        match import.name.as_str() {
            "http::downstream::attach_transport" => imports_attach_transport = true,
            "tcp::stream::downstream" => imports_downstream_tcp = true,
            "tcp::stream::peek" | "tls::session::handshake" => {
                imports_transport_prelude = true;
            }
            _ => {}
        }
    }

    imports_transport_prelude || (imports_attach_transport && imports_downstream_tcp)
}

#[cfg(feature = "tls")]
async fn serve_https_http_connection(
    app: Router,
    state: SharedState,
    stream: tokio::net::TcpStream,
    peer_addr: SocketAddr,
    local_addr: SocketAddr,
    tls_server_config: Arc<ServerConfig>,
) {
    let acceptor = TlsAcceptor::from(tls_server_config);
    match acceptor.accept(stream).await {
        Ok(tls_stream) => {
            let connection_metadata = DownstreamConnectionMetadata {
                local_addr,
                peer_addr,
                secure: true,
            };
            serve_http_connection(app, state, tls_stream, peer_addr, Some(connection_metadata))
                .await;
        }
        Err(err) => {
            warn!(
                "{} downstream https accept failed: {err}",
                category_program()
            );
        }
    }
}

struct CapturedPromotedHttpRequest {
    request: crate::abi_impl::HttpRequestContext,
    http2_attachment: Option<Http2DownstreamStreamAttachment>,
    http1_upgrade: Option<hyper::upgrade::OnUpgrade>,
}

enum DownstreamHttpAutoPromotion {
    None,
    Eligible(DownstreamHttpListenerGoal),
    Blocked,
}

fn downstream_http_auto_promotion(context: &SharedProxyVmContext) -> DownstreamHttpAutoPromotion {
    if context.lock_downstream().downstream_carrier_ref.is_some() {
        return DownstreamHttpAutoPromotion::None;
    }

    let transport = context.lock_transport();
    let goal = transport.downstream_listener_goal;
    if !goal.promotes_into_http() {
        return DownstreamHttpAutoPromotion::None;
    }

    #[cfg(feature = "tls")]
    let tls_touched =
        transport.downstream_tls_server_start.is_some() || transport.downstream_tls_io.is_some();
    #[cfg(not(feature = "tls"))]
    let tls_touched = false;

    if transport.downstream_transport_accessed
        || transport.tcp_dag.downstream.observed_io()
        || !transport.downstream_preread_buffer.is_empty()
        || tls_touched
        || transport.downstream_tcp_io.is_none()
    {
        DownstreamHttpAutoPromotion::Blocked
    } else {
        DownstreamHttpAutoPromotion::Eligible(goal)
    }
}

fn blocked_downstream_http_auto_promotion(host_name: &str) -> VmError {
    VmError::HostError(format!(
        "{host_name} requires http::downstream::attach_transport() after raw downstream transport or TLS prelude use",
    ))
}

pub(crate) fn scoped_http_host_call_can_run_synchronously(
    context: &SharedProxyVmContext,
    host_name: &str,
) -> Result<bool, VmError> {
    match downstream_http_auto_promotion(context) {
        DownstreamHttpAutoPromotion::None => Ok(true),
        DownstreamHttpAutoPromotion::Eligible(_) => Ok(false),
        DownstreamHttpAutoPromotion::Blocked => {
            Err(blocked_downstream_http_auto_promotion(host_name))
        }
    }
}

async fn promote_captured_downstream_transport_into_http_request(
    context: SharedProxyVmContext,
    promoted: PromotedDownstreamTransport,
) -> Result<(), VmError> {
    let connection_metadata = match &promoted {
        PromotedDownstreamTransport::Tcp(_) => context.downstream_connection_metadata(false)?,
        #[cfg(feature = "tls")]
        PromotedDownstreamTransport::Tls(_) => context.downstream_connection_metadata(true)?,
    };
    let request_id =
        context.with_request_head(|request_head| request_head.request_id().to_string());
    let downstream_http_sessions = context.services().downstream_http_sessions();
    let (captured_tx, captured_rx) =
        oneshot::channel::<Result<CapturedPromotedHttpRequest, String>>();
    let (response_tx, response_rx) = oneshot::channel::<InlineDownstreamHttpResponse>();
    context.begin_inline_downstream_http_response(response_tx)?;

    match promoted {
        PromotedDownstreamTransport::Tcp(stream) => {
            tokio::spawn(run_inline_promoted_http_connection(
                stream,
                connection_metadata,
                downstream_http_sessions,
                request_id,
                captured_tx,
                response_rx,
            ));
        }
        #[cfg(feature = "tls")]
        PromotedDownstreamTransport::Tls(stream) => {
            tokio::spawn(run_inline_promoted_http_connection(
                *stream,
                connection_metadata,
                downstream_http_sessions,
                request_id,
                captured_tx,
                response_rx,
            ));
        }
    }

    let captured = captured_rx.await.map_err(|_| {
        VmError::HostError(
            "downstream http promotion closed before a request was captured".to_string(),
        )
    })?;
    let captured = captured.map_err(VmError::HostError)?;
    context.promote_downstream_http_request(
        captured.request,
        captured.http2_attachment,
        captured.http1_upgrade,
    );
    Ok(())
}

#[cfg(feature = "tls")]
async fn take_goal_promoted_downstream_transport(
    context: &SharedProxyVmContext,
    goal: DownstreamHttpListenerGoal,
) -> Result<PromotedDownstreamTransport, VmError> {
    if !goal.requires_tls() {
        return take_promoted_downstream_transport(context).await;
    }

    let server_config = context
        .services()
        .downstream_tls_termination()
        .ok_or_else(|| {
            VmError::HostError(
                "downstream https listener is missing tls termination configuration".to_string(),
            )
        })?;
    let tcp_io = {
        let mut transport = context.lock_transport();
        transport.downstream_tcp_io.take().ok_or_else(|| {
            VmError::HostError(
                "downstream HTTP promotion requires an attached downstream tcp transport"
                    .to_string(),
            )
        })?
    };
    let tcp_stream = {
        let mut guard = tcp_io.lock().await;
        guard.take().ok_or_else(|| {
            VmError::HostError("downstream tcp transport is already in use".to_string())
        })?
    };
    let mut acceptor = Box::pin(LazyConfigAcceptor::new(
        rustls::server::Acceptor::default(),
        crate::abi_impl::ReplayPrefixedIo::new(Vec::new(), tcp_stream),
    ));
    let start = acceptor.as_mut().await.map_err(|err| {
        let message = format!("downstream tls handshake failed before http attach: {err}");
        {
            let mut transport = context.lock_transport();
            transport.tcp_dag.downstream.mark_failed(message.clone());
            transport.tls_dag.downstream.mark_failed();
        }
        VmError::HostError(message)
    })?;
    let server_name = start.client_hello().server_name().map(str::to_string);
    {
        let mut transport = context.lock_transport();
        transport
            .tls_dag
            .downstream
            .observe_downstream_client_hello(server_name);
    }

    let tls_stream = start.into_stream(server_config).await.map_err(|err| {
        let message = format!("downstream tls handshake failed before http attach: {err}");
        {
            let mut transport = context.lock_transport();
            transport.tcp_dag.downstream.mark_failed(message.clone());
            transport.tls_dag.downstream.mark_failed();
        }
        VmError::HostError(message)
    })?;
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
        let mut transport = context.lock_transport();
        transport.tcp_dag.downstream.mark_connected();
        transport.downstream_read_eof = false;
        let flow = &mut transport.tls_dag.downstream;
        flow.note_server_hello_received();
        flow.note_server_certificate_received(peer_certificate_der);
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
    }

    Ok(PromotedDownstreamTransport::Tls(Box::new(
        crate::abi_impl::ReplayPrefixedIo::new(Vec::new(), tls_stream),
    )))
}

#[cfg(not(feature = "tls"))]
async fn take_goal_promoted_downstream_transport(
    context: &SharedProxyVmContext,
    _goal: DownstreamHttpListenerGoal,
) -> Result<PromotedDownstreamTransport, VmError> {
    take_promoted_downstream_transport(context).await
}

pub(crate) async fn auto_promote_downstream_listener_goal_into_http_request(
    context: SharedProxyVmContext,
    host_name: &str,
) -> Result<(), VmError> {
    match downstream_http_auto_promotion(&context) {
        DownstreamHttpAutoPromotion::None => Ok(()),
        DownstreamHttpAutoPromotion::Eligible(goal) => {
            let promoted = take_goal_promoted_downstream_transport(&context, goal).await?;
            promote_captured_downstream_transport_into_http_request(context, promoted).await
        }
        DownstreamHttpAutoPromotion::Blocked => {
            Err(blocked_downstream_http_auto_promotion(host_name))
        }
    }
}

pub(crate) async fn maybe_auto_promote_downstream_listener_goal_into_http_request(
    context: &SharedProxyVmContext,
) -> Result<bool, VmError> {
    let DownstreamHttpAutoPromotion::Eligible(goal) = downstream_http_auto_promotion(context)
    else {
        return Ok(false);
    };
    let promoted = take_goal_promoted_downstream_transport(context, goal).await?;
    promote_captured_downstream_transport_into_http_request(context.clone(), promoted).await?;
    Ok(true)
}

pub(crate) async fn promote_transport_context_into_http_request(
    context: SharedProxyVmContext,
) -> Result<(), VmError> {
    if let DownstreamHttpAutoPromotion::Eligible(goal) = downstream_http_auto_promotion(&context) {
        let promoted = take_goal_promoted_downstream_transport(&context, goal).await?;
        return promote_captured_downstream_transport_into_http_request(context, promoted).await;
    }

    let promoted = take_promoted_downstream_transport(&context).await?;
    promote_captured_downstream_transport_into_http_request(context, promoted).await
}

async fn run_inline_promoted_http_connection<S>(
    stream: S,
    connection_metadata: DownstreamConnectionMetadata,
    downstream_http_sessions: Option<crate::abi_impl::SharedHttpDownstreamSessions>,
    request_id: String,
    captured_tx: oneshot::Sender<Result<CapturedPromotedHttpRequest, String>>,
    response_rx: oneshot::Receiver<InlineDownstreamHttpResponse>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let tracker = DownstreamHttp2ConnectionTracker::new(
        downstream_http_sessions
            .unwrap_or_else(|| crate::abi_impl::new_shared_http_downstream_sessions(1)),
        connection_metadata.peer_addr.to_string(),
    );
    let capture_sender = Arc::new(Mutex::new(Some(captured_tx)));
    let response_receiver = Arc::new(AsyncMutex::new(Some(response_rx)));
    let request_claimed = Arc::new(AtomicBool::new(false));
    let metadata = Some(connection_metadata.clone());
    let service = {
        let tracker = tracker.clone();
        let capture_sender = capture_sender.clone();
        let response_receiver = response_receiver.clone();
        let request_claimed = request_claimed.clone();
        let request_id = request_id.clone();
        hyper::service::service_fn(move |request: hyper::Request<Incoming>| {
            let tracker = tracker.clone();
            let capture_sender = capture_sender.clone();
            let response_receiver = response_receiver.clone();
            let request_claimed = request_claimed.clone();
            let request_id = request_id.clone();
            let connection_metadata = metadata.clone();
            async move {
                if request_claimed.swap(true, Ordering::AcqRel) {
                    return Ok::<_, std::convert::Infallible>(text_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "downstream http session already attached",
                    ));
                }

                let version = request.version();
                let path = request.uri().path().to_string();
                let attachment = tracker.observe_request(version, &path);
                let mut request = request;
                let on_upgrade = hyper::upgrade::on(&mut request);
                let request = request.map(Body::new);
                let (parts, body) = request.into_parts();
                let captured_request = CapturedPromotedHttpRequest {
                    request: build_downstream_http_request_context(
                        request_id,
                        parts,
                        body,
                        connection_metadata.as_ref(),
                    ),
                    http2_attachment: attachment.clone(),
                    http1_upgrade: (!matches!(version, axum::http::Version::HTTP_2))
                        .then_some(on_upgrade),
                };

                if let Some(sender) = capture_sender
                    .lock()
                    .expect("inline http capture sender lock poisoned")
                    .take()
                {
                    let _ = sender.send(Ok(captured_request));
                }

                let response_receiver = {
                    let mut guard = response_receiver.lock().await;
                    guard.take()
                };
                let Some(response_receiver) = response_receiver else {
                    tracker.finish_request(
                        attachment.as_ref(),
                        Some("inline downstream http response receiver missing".to_string()),
                    );
                    return Ok::<_, std::convert::Infallible>(text_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal server error",
                    ));
                };
                let response_result = response_receiver.await;

                match response_result {
                    Ok(resolved) => {
                        tracker.note_response_head(attachment.as_ref());
                        tracker.finish_request(attachment.as_ref(), None);
                        if let Some(plan) = resolved.post_response_plan {
                            tokio::spawn(async move {
                                if let Err(err) = plan.run().await {
                                    warn!(
                                        "{} downstream post-response transport failed: {err}",
                                        category_program()
                                    );
                                }
                            });
                        }
                        Ok::<_, std::convert::Infallible>(resolved.response)
                    }
                    Err(_) => {
                        tracker.finish_request(
                            attachment.as_ref(),
                            Some("inline downstream http response was dropped".to_string()),
                        );
                        Ok::<_, std::convert::Infallible>(text_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "internal server error",
                        ))
                    }
                }
            }
        })
    };

    let result = AutoBuilder::new(TokioExecutor::new())
        .serve_connection_with_upgrades(TokioIo::new(stream), service)
        .await;

    if let Err(ref err) = result
        && let Some(sender) = capture_sender
            .lock()
            .expect("inline http capture sender lock poisoned")
            .take()
    {
        let _ = sender.send(Err(format!(
            "downstream http promotion failed before request capture: {err}",
        )));
    }
    tracker.finish_connection(result.as_ref().err().map(|err| err.to_string()));
}

pub async fn serve_http_proxy(listener: TcpListener, state: SharedState) -> std::io::Result<()> {
    #[cfg(not(feature = "http2"))]
    {
        return axum::serve(listener, build_http_proxy_app(state)).await;
    }

    #[cfg(feature = "http2")]
    {
        let app = build_http_proxy_app(state.clone());
        loop {
            let (stream, peer_addr) = listener.accept().await?;
            let app = app.clone();
            let state = state.clone();
            tokio::spawn(async move {
                serve_http_connection(app, state, stream, peer_addr, None).await;
            });
        }
    }
}

#[cfg(feature = "tls")]
pub async fn serve_https_proxy(listener: TcpListener, state: SharedState) -> std::io::Result<()> {
    let tls_server_config = generate_https_proxy_tls_server_config()?;
    let app = build_http_proxy_app(state.clone());
    let local_addr = listener.local_addr()?;
    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let state = state.clone();
        let tls_server_config = tls_server_config.clone();
        let app = app.clone();
        let use_transport_handoff = {
            let guard = state.active_program.read().await;
            https_listener_needs_transport_handoff(guard.as_deref())
        };
        tokio::spawn(async move {
            if use_transport_handoff {
                serve_transport_connection_with_listener_goal(
                    stream,
                    peer_addr,
                    state,
                    DownstreamHttpListenerGoal::Https,
                    Some(tls_server_config),
                )
                .await;
            } else {
                serve_https_http_connection(
                    app,
                    state,
                    stream,
                    peer_addr,
                    local_addr,
                    tls_server_config,
                )
                .await;
            }
        });
    }
}

#[cfg(not(feature = "tls"))]
pub async fn serve_https_proxy(listener: TcpListener, state: SharedState) -> std::io::Result<()> {
    super::super::transport_plane::serve_transport_proxy(listener, state).await
}

#[cfg(feature = "http3")]
pub async fn serve_http3_proxy(listener: UdpSocket, state: SharedState) -> std::io::Result<()> {
    let local_addr = listener.local_addr()?;
    let endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(generate_http3_proxy_quic_server_config()?),
        listener.into_std()?,
        Arc::new(quinn::TokioRuntime),
    )?;
    let app = build_http_proxy_app(state.clone());

    while let Some(incoming) = endpoint.accept().await {
        let app = app.clone();
        let state = state.clone();
        tokio::spawn(async move {
            match incoming.await {
                Ok(connection) => serve_http3_connection(app, state, local_addr, connection).await,
                Err(err) => {
                    warn!(
                        "{} downstream http3 QUIC accept failed: {err}",
                        category_program()
                    );
                }
            }
        });
    }

    Ok(())
}

async fn handle_data_plane_request(state: SharedState, request: Request) -> Response<Body> {
    let started = Instant::now();

    state.record_data_plane_request();

    let snapshot = {
        let guard = state.active_program.read().await;
        guard.clone()
    };

    let Some(program) = snapshot else {
        warn!("{} no program loaded; returning 404", category_program());
        return finalize_data_plane_response(
            &state,
            started,
            text_response(StatusCode::NOT_FOUND, "not found"),
            0,
        );
    };

    let (mut parts, body) = request.into_parts();
    let downstream_http2_attachment = parts
        .extensions
        .get::<Http2DownstreamStreamAttachment>()
        .cloned();
    #[cfg(feature = "http3")]
    let downstream_http3_attachment = parts
        .extensions
        .get::<Http3DownstreamStreamAttachment>()
        .cloned();
    let connection_metadata = parts
        .extensions
        .get::<DownstreamConnectionMetadata>()
        .cloned();
    let downstream_http1_upgrade = parts.extensions.remove::<hyper::upgrade::OnUpgrade>();
    let vm_context = {
        let request_id = Uuid::new_v4().to_string();
        let request_path = parts.uri.path().to_string();
        let vm_request = build_downstream_http_request_context(
            request_id.clone(),
            parts,
            body,
            connection_metadata.as_ref(),
        );
        let debug = VmDebugInvocation {
            attach_debugger: request_will_attach_debugger(
                &state.debug_session,
                &vm_request.headers,
                &request_path,
            ),
            force_threading: request_uses_blocking_debugger(
                &state.debug_session,
                &vm_request.headers,
                &request_path,
            ),
            request_headers: vm_request.headers.clone(),
            request_path,
            request_id,
        };
        let mut vm_context = ProxyVmContext::from_http_request_with_services(
            vm_request,
            state.runtime_services.clone(),
        );
        if let Some(attachment) = &downstream_http2_attachment {
            vm_context.attach_downstream_http2_stream(attachment);
        }
        #[cfg(feature = "http3")]
        if let Some(attachment) = &downstream_http3_attachment {
            vm_context.attach_downstream_http3_stream(attachment);
        }
        if let Some(upgrade) = downstream_http1_upgrade {
            vm_context.attach_downstream_http1_upgrade(upgrade);
        }
        let vm_context = Arc::new(vm_context);
        match execute_vm_with_context(
            &program,
            vm_context.clone(),
            state.debug_session.clone(),
            debug,
            register_http_plane_host_module,
            state.vm_execution,
        )
        .await
        {
            Ok(()) => {}
            Err(VmExecutionError::HostRegistration(err)) => {
                state.record_vm_execution_error();
                warn!(
                    "{} failed to register host module: {err}",
                    category_program()
                );
                return finalize_data_plane_response(
                    &state,
                    started,
                    text_response(StatusCode::INTERNAL_SERVER_ERROR, "internal server error"),
                    0,
                );
            }
            Err(VmExecutionError::Vm(err)) => {
                state.record_vm_execution_error();
                warn!("{} vm execution error: {err}", category_program());
                return finalize_data_plane_response(
                    &state,
                    started,
                    text_response(StatusCode::INTERNAL_SERVER_ERROR, "internal server error"),
                    0,
                );
            }
        }

        vm_context
    };

    let resolved = resolve_http_graph_response(&vm_context).await;
    let crate::abi_impl::http::ResolvedHttpGraphResponse {
        response,
        upstream_latency_ms,
        post_response_plan,
    } = resolved;
    if let Some(plan) = post_response_plan {
        tokio::spawn(async move {
            if let Err(err) = plan.run().await {
                warn!(
                    "{} downstream post-response transport failed: {err}",
                    category_program()
                );
            }
        });
    }
    finalize_data_plane_response(&state, started, response, upstream_latency_ms)
}

async fn data_plane_handler(State(state): State<SharedState>, request: Request) -> Response<Body> {
    handle_data_plane_request(state, request).await
}

fn finalize_data_plane_response(
    state: &SharedState,
    started: Instant,
    response: Response<Body>,
    upstream_latency_ms: u64,
) -> Response<Body> {
    state.record_data_plane_status(response.status().as_u16());
    let elapsed_ms = started.elapsed().as_millis();
    let total_latency_ms = u64::try_from(elapsed_ms).unwrap_or(u64::MAX);
    state.record_data_plane_latency_ms(total_latency_ms, upstream_latency_ms);
    response
}

fn text_response(status: StatusCode, text: &str) -> Response<Body> {
    let mut response = Response::new(Body::from(text.to_string()));
    *response.status_mut() = status;
    response
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "http2")]
    use axum::http::Version;
    #[cfg(feature = "http2")]
    use http_body_util::{BodyExt, Full};
    #[cfg(feature = "http2")]
    use vm::{compile_source, encode_program};

    #[cfg(feature = "http2")]
    use super::*;
    #[cfg(feature = "http2")]
    use crate::abi_impl::Http2SessionFrontier;
    #[cfg(feature = "http2")]
    use crate::runtime::apply_program_from_bytes;

    #[cfg(feature = "http2")]
    #[tokio::test(flavor = "current_thread")]
    async fn downstream_http2_session_store_tracks_streams_outside_vm_context() {
        let state = SharedState::new(1024 * 1024);
        let source = r#"
            use http;
            use runtime;

            runtime::sleep(50);
            http::response::set_body(http::request::get_path());
        "#;
        let compiled = compile_source(source).expect("source should compile");
        let program = encode_program(&compiled.program).expect("program should encode");
        let report = apply_program_from_bytes(&state, &program).await;
        assert!(report.applied, "program should apply");

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("listener should have addr");
        let server = tokio::spawn(serve_http_proxy(listener, state.clone()));

        let stream = tokio::net::TcpStream::connect(addr)
            .await
            .expect("http2 client should connect");
        let io = TokioIo::new(stream);
        let (sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .handshake(io)
            .await
            .expect("http2 client handshake should succeed");
        let connection_task = tokio::spawn(async move {
            connection
                .await
                .expect("http2 client connection should run");
        });

        let first_request = {
            let mut sender = sender.clone();
            tokio::spawn(async move {
                sender
                    .send_request(
                        hyper::Request::builder()
                            .method("GET")
                            .uri(format!("http://{addr}/slow-a"))
                            .version(Version::HTTP_2)
                            .header("host", format!("{addr}"))
                            .body(Full::new(axum::body::Bytes::new()))
                            .expect("first request should build"),
                    )
                    .await
                    .expect("first request should complete")
            })
        };
        let second_request = {
            let mut sender = sender.clone();
            tokio::spawn(async move {
                sender
                    .send_request(
                        hyper::Request::builder()
                            .method("GET")
                            .uri(format!("http://{addr}/slow-b"))
                            .version(Version::HTTP_2)
                            .header("host", format!("{addr}"))
                            .body(Full::new(axum::body::Bytes::new()))
                            .expect("second request should build"),
                    )
                    .await
                    .expect("second request should complete")
            })
        };

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let snapshot = {
            let guard = state
                .downstream_http2_sessions
                .lock()
                .expect("http downstream session store lock poisoned");
            assert_eq!(guard.len(), 1, "one downstream h2 session should exist");
            guard
                .snapshot_values()
                .into_iter()
                .next()
                .expect("session should exist")
        };
        assert_eq!(snapshot.frontier, Http2SessionFrontier::Open);
        assert_eq!(snapshot.total_streams, 2);
        assert_eq!(snapshot.active_streams, 2);
        assert_eq!(snapshot.streams.len(), 2);

        let first_response = first_request.await.expect("first join should succeed");
        let second_response = second_request.await.expect("second join should succeed");
        assert_eq!(
            BodyExt::collect(first_response.into_body())
                .await
                .expect("first body should collect")
                .to_bytes()
                .as_ref(),
            b"/slow-a"
        );
        assert_eq!(
            BodyExt::collect(second_response.into_body())
                .await
                .expect("second body should collect")
                .to_bytes()
                .as_ref(),
            b"/slow-b"
        );

        drop(sender);
        connection_task.abort();
        server.abort();
    }
}
