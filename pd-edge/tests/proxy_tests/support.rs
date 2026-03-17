pub(crate) use std::{
    io::Read,
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

#[cfg(feature = "http3")]
pub(crate) use crate::http3_support::send_http3_request;
pub(crate) use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderValue, Request, Response, StatusCode},
    routing::{any, post},
};
pub(crate) use base64::{Engine as _, engine::general_purpose::STANDARD};
#[cfg(feature = "webrtc")]
pub(crate) use edge::sample_echo::spawn_webrtc_echo_server;
#[cfg(feature = "http3")]
pub(crate) use edge::serve_http3_proxy;
pub(crate) use edge::{
    ActiveControlPlaneConfig, CommandResultPayload, ControlPlaneCommand, EdgeCommandResult,
    EdgePollRequest, EdgePollResponse, FN_HTTP_RESPONSE_SET_BODY, FN_HTTP_RESPONSE_SET_HEADER,
    ProxyVmContext, RateLimiterStore, SharedState, VmAsyncOpBridge, build_admin_app,
    build_http_proxy_app, compile_edge_source_file, enter_edge_host_context, function_by_name,
    new_shared_vm_async_ops, register_http_plane_host_module, serve_http_proxy, serve_https_proxy,
    serve_transport_proxy, spawn_active_control_plane_client,
};
#[cfg(feature = "websocket")]
pub(crate) use futures_util::{SinkExt, StreamExt};
pub(crate) use tokio::{sync::Notify, task::JoinHandle, time::timeout};
#[cfg(feature = "tls")]
pub(crate) use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        self,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};
#[cfg(feature = "websocket")]
pub(crate) use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{Request as WsRequest, Response as WsResponse},
        http::HeaderValue as WsHeaderValue,
    },
};
pub(crate) use vm::{
    BytecodeBuilder, Program, Value, Vm, VmError, VmStatus, compile_source, encode_program,
};

pub(crate) const SAMPLE_PROXY_UPSTREAM_PORT: u16 = 18080;
pub(crate) const SAMPLE_REQUEST_TRANSFORM_UPSTREAM_PORT: u16 = 18081;
pub(crate) const SAMPLE_SSE_UPSTREAM_PORT: u16 = 18082;
pub(crate) const SAMPLE_SUBREQUEST_PRIMARY_PORT: u16 = 18083;
pub(crate) const SAMPLE_SUBREQUEST_SECONDARY_PORT: u16 = 18483;
pub(crate) const SAMPLE_IO_UPSTREAM_PORT: u16 = 18084;
pub(crate) const SAMPLE_TRANSPORT_UPSTREAM_HTTP_PORT: u16 = 18085;
pub(crate) const SAMPLE_TRANSPORT_UPSTREAM_HTTPS_PORT: u16 = 18485;
pub(crate) const SAMPLE_TUNNEL_UPSTREAM_HTTP_PORT: u16 = 18086;
pub(crate) const SAMPLE_TUNNEL_UPSTREAM_HTTPS_PORT: u16 = 18486;
pub(crate) const SAMPLE_FORWARD_UPSTREAM_HTTPS_PORT: u16 = 18487;
pub(crate) const SAMPLE_FORWARD_PROXY_PORT: u16 = 18090;
pub(crate) const SAMPLE_WEBRTC_SIGNAL_TEXT_PORT: u16 = 18087;
pub(crate) const SAMPLE_WEBRTC_SIGNAL_BINARY_PORT: u16 = 18088;
pub(crate) const SAMPLE_WEBSOCKET_SSE_BRIDGE_PORT: u16 = 18089;
#[cfg(feature = "websocket")]
pub(crate) const SAMPLE_WEBSOCKET_UPSTREAM_PORT: u16 = 18091;
#[cfg(all(feature = "tls", feature = "http2"))]
pub(crate) const SAMPLE_UPSTREAM_HTTP2_PORT: u16 = 18444;
#[cfg(all(feature = "tls", feature = "http3"))]
pub(crate) const SAMPLE_UPSTREAM_HTTP3_PORT: u16 = 18445;

pub(crate) fn loopback_addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

#[cfg(feature = "tls")]
pub(crate) fn ensure_rustls_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

pub(crate) async fn spawn_server_on(
    app: Router,
    bind_addr: SocketAddr,
) -> (SocketAddr, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("server should run");
    });
    (addr, handle)
}

pub(crate) async fn spawn_server(app: Router) -> (SocketAddr, JoinHandle<()>) {
    spawn_server_on(app, loopback_addr(0)).await
}

pub(crate) async fn spawn_proxy(
    max_program_bytes: usize,
) -> (SocketAddr, SocketAddr, JoinHandle<()>, JoinHandle<()>) {
    let state = SharedState::new(max_program_bytes);
    spawn_proxy_with_state(state).await
}

pub(crate) async fn spawn_proxy_with_state(
    state: SharedState,
) -> (SocketAddr, SocketAddr, JoinHandle<()>, JoinHandle<()>) {
    let data_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let data_addr = data_listener
        .local_addr()
        .expect("listener should have addr");
    let data_handle = tokio::spawn({
        let state = state.clone();
        async move {
            serve_http_proxy(data_listener, state)
                .await
                .expect("data plane server should run");
        }
    });
    let (admin_addr, admin_handle) = spawn_server(build_admin_app(state)).await;
    (data_addr, admin_addr, data_handle, admin_handle)
}

#[cfg(feature = "http3")]
pub(crate) async fn spawn_http3_proxy(
    max_program_bytes: usize,
) -> (SocketAddr, SocketAddr, JoinHandle<()>, JoinHandle<()>) {
    let state = SharedState::new(max_program_bytes);
    spawn_http3_proxy_with_state(state).await
}

#[cfg(feature = "http3")]
pub(crate) async fn spawn_http3_proxy_with_state(
    state: SharedState,
) -> (SocketAddr, SocketAddr, JoinHandle<()>, JoinHandle<()>) {
    let data_listener = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("http3 listener should bind");
    let data_addr = data_listener
        .local_addr()
        .expect("http3 listener should have addr");
    let data_handle = tokio::spawn({
        let state = state.clone();
        async move {
            serve_http3_proxy(data_listener, state)
                .await
                .expect("http3 data plane server should run");
        }
    });
    let (admin_addr, admin_handle) = spawn_server(build_admin_app(state)).await;
    (data_addr, admin_addr, data_handle, admin_handle)
}

pub(crate) async fn spawn_http_https_proxy(
    max_program_bytes: usize,
) -> (
    SocketAddr,
    SocketAddr,
    SocketAddr,
    JoinHandle<()>,
    JoinHandle<()>,
    JoinHandle<()>,
) {
    let state = SharedState::new(max_program_bytes);
    spawn_http_https_proxy_with_state(state).await
}

pub(crate) async fn spawn_http_https_proxy_with_state(
    state: SharedState,
) -> (
    SocketAddr,
    SocketAddr,
    SocketAddr,
    JoinHandle<()>,
    JoinHandle<()>,
    JoinHandle<()>,
) {
    let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("http listener should bind");
    let http_addr = http_listener
        .local_addr()
        .expect("http listener should have addr");
    let https_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("https listener should bind");
    let https_addr = https_listener
        .local_addr()
        .expect("https listener should have addr");
    let http_handle = tokio::spawn({
        let state = state.clone();
        async move {
            serve_http_proxy(http_listener, state)
                .await
                .expect("http data plane server should run");
        }
    });
    let https_handle = tokio::spawn({
        let state = state.clone();
        async move {
            serve_https_proxy(https_listener, state)
                .await
                .expect("https data plane server should run");
        }
    });
    let (admin_addr, admin_handle) = spawn_server(build_admin_app(state)).await;
    (
        http_addr,
        https_addr,
        admin_addr,
        http_handle,
        https_handle,
        admin_handle,
    )
}

pub(crate) async fn spawn_transport_proxy(
    max_program_bytes: usize,
) -> (SocketAddr, SocketAddr, JoinHandle<()>, JoinHandle<()>) {
    let state = SharedState::new(max_program_bytes);
    let data_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let data_addr = data_listener
        .local_addr()
        .expect("listener should have addr");
    let data_handle = tokio::spawn({
        let state = state.clone();
        async move {
            serve_transport_proxy(data_listener, state)
                .await
                .expect("transport plane server should run");
        }
    });
    let (admin_addr, admin_handle) = spawn_server(build_admin_app(state)).await;
    (data_addr, admin_addr, data_handle, admin_handle)
}

pub(crate) async fn spawn_chunked_upstream_on(
    chunks: Vec<&'static str>,
    bind_addr: SocketAddr,
) -> (SocketAddr, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    let chunks = chunks
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<String>>();
    let handle = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.expect("accept should succeed");
            let response_chunks = chunks.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};

                let mut buffer = [0u8; 4096];
                let _ = stream.read(&mut buffer).await;
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("response head should write");
                for chunk in response_chunks {
                    let frame = format!("{:X}\r\n{}\r\n", chunk.len(), chunk);
                    stream
                        .write_all(frame.as_bytes())
                        .await
                        .expect("chunk should write");
                    stream.flush().await.expect("chunk flush should succeed");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                stream
                    .write_all(b"0\r\n\r\n")
                    .await
                    .expect("terminator should write");
                let _ = stream.shutdown().await;
            });
        }
    });
    (addr, handle)
}

pub(crate) async fn spawn_connect_forward_proxy() -> (SocketAddr, JoinHandle<()>) {
    edge::sample_echo::spawn_connect_forward_proxy("127.0.0.1:0".parse().expect("valid addr"))
        .await
        .expect("forward proxy should start")
}

pub(crate) async fn spawn_connect_forward_proxy_on(
    bind_addr: SocketAddr,
) -> (SocketAddr, JoinHandle<()>) {
    edge::sample_echo::spawn_connect_forward_proxy(bind_addr)
        .await
        .expect("forward proxy should start")
}

pub(crate) async fn run_edge_program_direct(
    program: Program,
    context: Arc<ProxyVmContext>,
) -> Result<(), VmError> {
    let async_ops = new_shared_vm_async_ops();
    let mut vm = Vm::new(program);
    vm.set_async_bridge(Box::new(VmAsyncOpBridge::new(async_ops.clone())));
    register_http_plane_host_module(&mut vm, context.clone(), async_ops.clone())?;

    let mut status = {
        let _host_context = enter_edge_host_context(context.clone(), async_ops.clone());
        vm.run()?
    };

    loop {
        match status {
            VmStatus::Halted => return Ok(()),
            VmStatus::Waiting(_op_id) => {
                vm.await_waiting_host_op().await?;
                let _host_context = enter_edge_host_context(context.clone(), async_ops.clone());
                status = vm.resume()?;
            }
            other => {
                return Err(VmError::HostError(format!(
                    "unexpected vm status while running direct edge test: {other:?}",
                )));
            }
        }
    }
}

pub(crate) async fn spawn_sse_upstream_on(
    lines: Vec<&'static str>,
    bind_addr: SocketAddr,
) -> (SocketAddr, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    let lines = lines
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<String>>();
    let handle = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.expect("accept should succeed");
            let response_lines = lines.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};

                let mut buffer = [0u8; 4096];
                let _ = stream.read(&mut buffer).await;
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("response head should write");
                for line in response_lines {
                    let frame = format!("{:X}\r\n{}\r\n", line.len(), line);
                    stream
                        .write_all(frame.as_bytes())
                        .await
                        .expect("sse frame should write");
                    stream.flush().await.expect("sse flush should succeed");
                    tokio::time::sleep(Duration::from_millis(15)).await;
                }
                stream
                    .write_all(b"0\r\n\r\n")
                    .await
                    .expect("terminator should write");
                let _ = stream.shutdown().await;
            });
        }
    });
    (addr, handle)
}

pub(crate) async fn spawn_sse_upstream(lines: Vec<&'static str>) -> (SocketAddr, JoinHandle<()>) {
    spawn_sse_upstream_on(lines, loopback_addr(0)).await
}

#[cfg(feature = "websocket")]
pub(crate) async fn spawn_websocket_echo_upstream_on(
    bind_addr: SocketAddr,
) -> (SocketAddr, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    let handle = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("accept should succeed");
            tokio::spawn(async move {
                let observed_client_tag = Arc::new(Mutex::new(None::<String>));
                let observed_client_tag_for_callback = Arc::clone(&observed_client_tag);
                let callback = move |request: &WsRequest, mut response: WsResponse| {
                    let requested = request
                        .headers()
                        .get("sec-websocket-protocol")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("");
                    let client_tag = request
                        .headers()
                        .get("x-client-tag")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    *observed_client_tag_for_callback
                        .lock()
                        .expect("websocket client tag lock should not poison") = client_tag;
                    if requested
                        .split(',')
                        .map(str::trim)
                        .any(|protocol| protocol == "chat")
                    {
                        response
                            .headers_mut()
                            .insert("sec-websocket-protocol", WsHeaderValue::from_static("chat"));
                    }
                    Ok(response)
                };

                let mut websocket = accept_hdr_async(stream, callback)
                    .await
                    .expect("websocket accept should succeed");
                let client_tag = observed_client_tag
                    .lock()
                    .expect("websocket client tag lock should not poison")
                    .clone();
                while let Some(message) = websocket.next().await {
                    match message.expect("websocket message should decode") {
                        Message::Text(text) => {
                            let mut reply = format!("echo:{text}");
                            if let Some(tag) = client_tag.as_deref() {
                                reply.push_str("|tag:");
                                reply.push_str(tag);
                            }
                            websocket
                                .send(Message::Text(reply.into()))
                                .await
                                .expect("text reply should send");
                        }
                        Message::Binary(payload) => {
                            websocket
                                .send(Message::Binary(payload))
                                .await
                                .expect("binary reply should send");
                        }
                        Message::Ping(payload) => {
                            websocket
                                .send(Message::Pong(payload))
                                .await
                                .expect("pong should send");
                        }
                        Message::Pong(_) => {}
                        Message::Close(frame) => {
                            let _ = websocket.close(frame).await;
                            break;
                        }
                        _ => {}
                    }
                }
            });
        }
    });
    (addr, handle)
}

#[cfg(feature = "websocket")]
pub(crate) async fn spawn_websocket_echo_upstream() -> (SocketAddr, JoinHandle<()>) {
    spawn_websocket_echo_upstream_on(loopback_addr(0)).await
}

#[cfg(feature = "tls")]
pub(crate) async fn spawn_https_echo_upstream_on(
    bind_addr: SocketAddr,
) -> (SocketAddr, JoinHandle<()>) {
    ensure_rustls_provider();
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("certificate should generate");
    let mut server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(
                cert.serialize_der().expect("certificate should serialize"),
            )],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.serialize_private_key_der())),
        )
        .expect("server config should build");
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];

    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    let handle = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("accept should succeed");
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(tls_stream) = acceptor.accept(stream).await else {
                    return;
                };
                let io = hyper_util::rt::TokioIo::new(tls_stream);
                let service = hyper::service::service_fn(
                    |request: hyper::Request<hyper::body::Incoming>| async move {
                        let method = request.method().clone();
                        let version = request.version();
                        let path = request.uri().path().to_string();
                        let body = http_body_util::BodyExt::collect(request.into_body())
                            .await
                            .expect("https echo request body should collect")
                            .to_bytes();
                        let echoed_body = if body.is_empty() {
                            axum::body::Bytes::from(format!("echo:https:{}:{path}", method))
                        } else {
                            body
                        };

                        let mut response =
                            hyper::Response::new(http_body_util::Full::new(echoed_body.clone()));
                        *response.status_mut() = hyper::StatusCode::OK;
                        response.headers_mut().insert(
                            hyper::header::CONTENT_TYPE,
                            HeaderValue::from_static("text/plain; charset=utf-8"),
                        );
                        response
                            .headers_mut()
                            .insert("x-echo-protocol", HeaderValue::from_static("https"));
                        response.headers_mut().insert(
                            "x-echo-method",
                            HeaderValue::from_str(method.as_str())
                                .expect("method header should serialize"),
                        );
                        response.headers_mut().insert(
                            "x-echo-path",
                            HeaderValue::from_str(&path).expect("path header should serialize"),
                        );
                        response.headers_mut().insert(
                            "x-echo-bytes",
                            HeaderValue::from_str(&echoed_body.len().to_string())
                                .expect("byte count should serialize"),
                        );
                        response.headers_mut().insert(
                            "x-echo-http-version",
                            HeaderValue::from_static(match version {
                                hyper::Version::HTTP_09 => "0.9",
                                hyper::Version::HTTP_10 => "1.0",
                                hyper::Version::HTTP_11 => "1.1",
                                hyper::Version::HTTP_2 => "2",
                                hyper::Version::HTTP_3 => "3",
                                _ => "unknown",
                            }),
                        );
                        Ok::<_, std::convert::Infallible>(response)
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });
    (addr, handle)
}

#[cfg(feature = "tls")]
pub(crate) async fn spawn_https_echo_upstream() -> (SocketAddr, JoinHandle<()>) {
    spawn_https_echo_upstream_on(loopback_addr(0)).await
}

#[cfg(all(feature = "tls", feature = "http2"))]
fn boxed_http2_full_body(
    text: &'static str,
) -> http_body_util::combinators::BoxBody<axum::body::Bytes, std::convert::Infallible> {
    http_body_util::BodyExt::boxed(http_body_util::Full::new(axum::body::Bytes::from_static(
        text.as_bytes(),
    )))
}

#[cfg(all(feature = "tls", feature = "http2"))]
fn boxed_http2_delayed_body(
    text: &'static str,
    delay: Duration,
) -> http_body_util::combinators::BoxBody<axum::body::Bytes, std::convert::Infallible> {
    let bytes = axum::body::Bytes::from_static(text.as_bytes());
    let body = http_body_util::StreamBody::new(futures_util::stream::once(async move {
        tokio::time::sleep(delay).await;
        Ok::<hyper::body::Frame<axum::body::Bytes>, std::convert::Infallible>(
            hyper::body::Frame::data(bytes),
        )
    }));
    http_body_util::BodyExt::boxed(body)
}

#[cfg(all(feature = "tls", feature = "http2"))]
fn boxed_http2_full_body_owned(
    text: String,
) -> http_body_util::combinators::BoxBody<axum::body::Bytes, std::convert::Infallible> {
    http_body_util::BodyExt::boxed(http_body_util::Full::new(axum::body::Bytes::from(text)))
}

#[cfg(all(feature = "tls", feature = "http2"))]
fn boxed_http2_delayed_body_owned(
    text: String,
    delay: Duration,
) -> http_body_util::combinators::BoxBody<axum::body::Bytes, std::convert::Infallible> {
    let bytes = axum::body::Bytes::from(text.into_bytes());
    let body = http_body_util::StreamBody::new(futures_util::stream::once(async move {
        tokio::time::sleep(delay).await;
        Ok::<hyper::body::Frame<axum::body::Bytes>, std::convert::Infallible>(
            hyper::body::Frame::data(bytes),
        )
    }));
    http_body_util::BodyExt::boxed(body)
}

#[cfg(all(feature = "tls", feature = "http3"))]
#[derive(Clone, Copy)]
enum Http3FixtureKind {
    Multiplex,
    Sample,
}

#[cfg(all(feature = "tls", feature = "http3"))]
type Http3FixtureSendStream =
    h3::server::RequestStream<h3_quinn::SendStream<axum::body::Bytes>, axum::body::Bytes>;

#[cfg(all(feature = "tls", feature = "http3"))]
type Http3FixtureRecvStream = h3::server::RequestStream<h3_quinn::RecvStream, axum::body::Bytes>;

#[cfg(all(feature = "tls", feature = "http3"))]
fn build_http3_upstream_server_config() -> quinn::ServerConfig {
    ensure_rustls_provider();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("http3 upstream certificate should generate");
    let mut server_crypto = rustls::ServerConfig::builder_with_provider(
        rustls::crypto::aws_lc_rs::default_provider().into(),
    )
    .with_protocol_versions(&[&rustls::version::TLS13])
    .expect("http3 upstream TLS versions should configure")
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from(
            cert.serialize_der()
                .expect("http3 upstream certificate should serialize"),
        )],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.serialize_private_key_der())),
    )
    .expect("http3 upstream server config should build");
    server_crypto.alpn_protocols = vec![b"h3".to_vec()];
    let quic_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)
        .expect("http3 upstream QUIC config should build");
    quinn::ServerConfig::with_crypto(Arc::new(quic_crypto))
}

#[cfg(all(feature = "tls", feature = "http3"))]
async fn read_http3_fixture_body(mut stream: Http3FixtureRecvStream) -> axum::body::Bytes {
    use hyper::body::Buf;

    let mut body = Vec::new();
    while let Some(mut chunk) = stream
        .recv_data()
        .await
        .expect("http3 fixture request body should read")
    {
        body.extend_from_slice(chunk.copy_to_bytes(chunk.remaining()).as_ref());
    }
    axum::body::Bytes::from(body)
}

#[cfg(all(feature = "tls", feature = "http3"))]
async fn write_http3_fixture_response(
    stream: &mut Http3FixtureSendStream,
    body: axum::body::Bytes,
    delay: Option<Duration>,
) {
    let response = hyper::Response::builder()
        .status(hyper::StatusCode::OK)
        .header("x-upstream-http-version", "3")
        .body(())
        .expect("http3 fixture response head should build");
    stream
        .send_response(response)
        .await
        .expect("http3 fixture response head should send");
    if let Some(delay) = delay {
        tokio::time::sleep(delay).await;
    }
    if !body.is_empty() {
        stream
            .send_data(body)
            .await
            .expect("http3 fixture response body should send");
    }
    stream
        .finish()
        .await
        .expect("http3 fixture response should finish");
}

#[cfg(all(feature = "tls", feature = "http3"))]
async fn serve_http3_fixture_connection(connection: quinn::Connection, kind: Http3FixtureKind) {
    let mut h3_conn = h3::server::builder()
        .build(h3_quinn::Connection::new(connection))
        .await
        .expect("http3 fixture connection should initialize");

    loop {
        match h3_conn.accept().await {
            Ok(Some(resolver)) => {
                tokio::spawn(async move {
                    let (request, stream) = resolver
                        .resolve_request()
                        .await
                        .expect("http3 fixture request should resolve");
                    let (parts, _) = request.into_parts();
                    let path = parts.uri.path().to_string();
                    let method = parts.method.to_string();
                    let tag = parts
                        .headers
                        .get("x-demo-request")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    let (mut send_stream, recv_stream) = stream.split();
                    let request_body = read_http3_fixture_body(recv_stream).await;

                    let (response_body, delay) = match kind {
                        Http3FixtureKind::Multiplex => match path.as_str() {
                            "/slow" => (
                                axum::body::Bytes::from_static(b"slow-body"),
                                Some(Duration::from_millis(75)),
                            ),
                            "/fast" => (axum::body::Bytes::from_static(b"fast-body"), None),
                            _ => (axum::body::Bytes::from_static(b"fallback-body"), None),
                        },
                        Http3FixtureKind::Sample => {
                            let payload = format!(
                                "{method}|{path}|{tag}|{}",
                                String::from_utf8_lossy(&request_body)
                            );
                            let delay = if path == "/slow" {
                                Some(Duration::from_millis(75))
                            } else {
                                None
                            };
                            (axum::body::Bytes::from(payload), delay)
                        }
                    };

                    write_http3_fixture_response(&mut send_stream, response_body, delay).await;
                });
            }
            Ok(None) => break,
            Err(err) => {
                if err.is_h3_no_error() {
                    break;
                }
                panic!("http3 fixture connection should stay healthy: {err}");
            }
        }
    }
}

#[cfg(all(feature = "tls", feature = "http3"))]
async fn spawn_https_http3_upstream_fixture(
    kind: Http3FixtureKind,
    bind_addr: SocketAddr,
) -> (
    SocketAddr,
    Arc<std::sync::atomic::AtomicUsize>,
    JoinHandle<()>,
) {
    let socket = tokio::net::UdpSocket::bind(bind_addr)
        .await
        .expect("http3 upstream socket should bind");
    let addr = socket
        .local_addr()
        .expect("http3 upstream socket should have addr");
    let endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(build_http3_upstream_server_config()),
        socket
            .into_std()
            .expect("http3 upstream socket should convert"),
        Arc::new(quinn::TokioRuntime),
    )
    .expect("http3 upstream endpoint should build");
    let connection_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handle = tokio::spawn({
        let connection_count = Arc::clone(&connection_count);
        async move {
            while let Some(incoming) = endpoint.accept().await {
                let connection_count = Arc::clone(&connection_count);
                tokio::spawn(async move {
                    let connection = incoming
                        .await
                        .expect("http3 upstream QUIC handshake should succeed");
                    connection_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    serve_http3_fixture_connection(connection, kind).await;
                });
            }
        }
    });
    (addr, connection_count, handle)
}

#[cfg(all(feature = "tls", feature = "http3"))]
pub(crate) async fn spawn_https_http3_multiplex_upstream() -> (
    SocketAddr,
    Arc<std::sync::atomic::AtomicUsize>,
    JoinHandle<()>,
) {
    spawn_https_http3_upstream_fixture(Http3FixtureKind::Multiplex, loopback_addr(0)).await
}

#[cfg(all(feature = "tls", feature = "http3"))]
pub(crate) async fn spawn_https_http3_sample_upstream_on(
    bind_addr: SocketAddr,
) -> (
    SocketAddr,
    Arc<std::sync::atomic::AtomicUsize>,
    JoinHandle<()>,
) {
    spawn_https_http3_upstream_fixture(Http3FixtureKind::Sample, bind_addr).await
}

#[cfg(all(feature = "tls", feature = "http2"))]
pub(crate) async fn spawn_https_http2_multiplex_upstream() -> (
    SocketAddr,
    Arc<std::sync::atomic::AtomicUsize>,
    JoinHandle<()>,
) {
    ensure_rustls_provider();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("certificate should generate");
    let mut server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(
                cert.serialize_der().expect("certificate should serialize"),
            )],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.serialize_private_key_der())),
        )
        .expect("server config should build");
    server_config.alpn_protocols = vec![b"h2".to_vec()];

    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    let connection_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handle = tokio::spawn({
        let connection_count = Arc::clone(&connection_count);
        async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept should succeed");
                connection_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let tls_stream = acceptor
                        .accept(stream)
                        .await
                        .expect("http2 tls accept should succeed");
                    let service = hyper::service::service_fn(
                        |request: hyper::Request<hyper::body::Incoming>| async move {
                            let path = request.uri().path().to_string();
                            let body = match path.as_str() {
                                "/slow" => {
                                    boxed_http2_delayed_body("slow-body", Duration::from_millis(75))
                                }
                                "/fast" => boxed_http2_full_body("fast-body"),
                                _ => boxed_http2_full_body("fallback-body"),
                            };
                            let mut response = hyper::Response::new(body);
                            response
                                .headers_mut()
                                .insert("x-upstream-http-version", HeaderValue::from_static("2"));
                            Ok::<_, std::convert::Infallible>(response)
                        },
                    );
                    let io = hyper_util::rt::TokioIo::new(tls_stream);
                    let builder = hyper::server::conn::http2::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    );
                    if let Err(err) = builder.serve_connection(io, service).await {
                        panic!("http2 upstream connection should serve: {err}");
                    }
                });
            }
        }
    });
    (addr, connection_count, handle)
}

#[cfg(all(feature = "tls", feature = "http2"))]
pub(crate) async fn spawn_https_http2_sample_upstream() -> (
    SocketAddr,
    Arc<std::sync::atomic::AtomicUsize>,
    JoinHandle<()>,
) {
    ensure_rustls_provider();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("certificate should generate");
    let mut server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(
                cert.serialize_der().expect("certificate should serialize"),
            )],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.serialize_private_key_der())),
        )
        .expect("server config should build");
    server_config.alpn_protocols = vec![b"h2".to_vec()];

    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let listener = tokio::net::TcpListener::bind(loopback_addr(0))
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    let connection_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handle = tokio::spawn({
        let connection_count = Arc::clone(&connection_count);
        async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept should succeed");
                connection_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let tls_stream = acceptor
                        .accept(stream)
                        .await
                        .expect("http2 tls accept should succeed");
                    let service = hyper::service::service_fn(
                        |request: hyper::Request<hyper::body::Incoming>| async move {
                            let (parts, body) = request.into_parts();
                            let path = parts.uri.path().to_string();
                            let method = parts.method.to_string();
                            let tag = parts
                                .headers
                                .get("x-demo-request")
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or("")
                                .to_string();
                            let body = http_body_util::BodyExt::collect(body)
                                .await
                                .expect("sample http2 request body should collect")
                                .to_bytes();
                            let payload =
                                format!("{method}|{path}|{tag}|{}", String::from_utf8_lossy(&body));
                            let body = if path == "/slow" {
                                boxed_http2_delayed_body_owned(payload, Duration::from_millis(75))
                            } else {
                                boxed_http2_full_body_owned(payload)
                            };
                            let mut response = hyper::Response::new(body);
                            response
                                .headers_mut()
                                .insert("x-upstream-http-version", HeaderValue::from_static("2"));
                            Ok::<_, std::convert::Infallible>(response)
                        },
                    );
                    let io = hyper_util::rt::TokioIo::new(tls_stream);
                    let builder = hyper::server::conn::http2::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    );
                    if let Err(err) = builder.serve_connection(io, service).await {
                        panic!("http2 sample upstream connection should serve: {err}");
                    }
                });
            }
        }
    });
    (addr, connection_count, handle)
}

#[cfg(all(feature = "tls", feature = "http2"))]
pub(crate) async fn spawn_https_http2_sample_upstream_on(
    bind_addr: SocketAddr,
) -> (
    SocketAddr,
    Arc<std::sync::atomic::AtomicUsize>,
    JoinHandle<()>,
) {
    ensure_rustls_provider();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("certificate should generate");
    let mut server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(
                cert.serialize_der().expect("certificate should serialize"),
            )],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.serialize_private_key_der())),
        )
        .expect("server config should build");
    server_config.alpn_protocols = vec![b"h2".to_vec()];

    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    let connection_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handle = tokio::spawn({
        let connection_count = Arc::clone(&connection_count);
        async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept should succeed");
                connection_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let tls_stream = acceptor
                        .accept(stream)
                        .await
                        .expect("http2 tls accept should succeed");
                    let service = hyper::service::service_fn(
                        |request: hyper::Request<hyper::body::Incoming>| async move {
                            let (parts, body) = request.into_parts();
                            let path = parts.uri.path().to_string();
                            let method = parts.method.to_string();
                            let tag = parts
                                .headers
                                .get("x-demo-request")
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or("")
                                .to_string();
                            let body = http_body_util::BodyExt::collect(body)
                                .await
                                .expect("sample http2 request body should collect")
                                .to_bytes();
                            let payload =
                                format!("{method}|{path}|{tag}|{}", String::from_utf8_lossy(&body));
                            let body = if path == "/slow" {
                                boxed_http2_delayed_body_owned(payload, Duration::from_millis(75))
                            } else {
                                boxed_http2_full_body_owned(payload)
                            };
                            let mut response = hyper::Response::new(body);
                            response
                                .headers_mut()
                                .insert("x-upstream-http-version", HeaderValue::from_static("2"));
                            Ok::<_, std::convert::Infallible>(response)
                        },
                    );
                    let io = hyper_util::rt::TokioIo::new(tls_stream);
                    let builder = hyper::server::conn::http2::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    );
                    if let Err(err) = builder.serve_connection(io, service).await {
                        panic!("http2 sample upstream connection should serve: {err}");
                    }
                });
            }
        }
    });
    (addr, connection_count, handle)
}

#[cfg(feature = "tls")]
#[derive(Clone)]
pub(crate) struct TlsTestMaterials {
    pub(crate) ca_pem: String,
    pub(crate) ca_der: Vec<u8>,
    pub(crate) server_cert_der: Vec<u8>,
    pub(crate) server_key_der: Vec<u8>,
    pub(crate) client_cert_pem: String,
    pub(crate) client_key_pem: String,
}

#[cfg(feature = "tls")]
pub(crate) fn source_string_literal(value: &str) -> String {
    serde_json::to_string(value).expect("source literal should serialize")
}

#[cfg(feature = "tls")]
pub(crate) fn build_ca_signed_tls_materials() -> TlsTestMaterials {
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new());
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::DigitalSignature,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    let ca = rcgen::Certificate::from_params(ca_params).expect("ca certificate should build");

    let mut server_params = rcgen::CertificateParams::new(vec!["localhost".to_string()]);
    server_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let server =
        rcgen::Certificate::from_params(server_params).expect("server certificate should build");

    let mut client_params = rcgen::CertificateParams::new(vec!["client.local".to_string()]);
    client_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    let client =
        rcgen::Certificate::from_params(client_params).expect("client certificate should build");

    TlsTestMaterials {
        ca_pem: ca.serialize_pem().expect("ca pem should serialize"),
        ca_der: ca.serialize_der().expect("ca der should serialize"),
        server_cert_der: server
            .serialize_der_with_signer(&ca)
            .expect("server cert should serialize"),
        server_key_der: server.serialize_private_key_der(),
        client_cert_pem: client
            .serialize_pem_with_signer(&ca)
            .expect("client cert should serialize"),
        client_key_pem: client.serialize_private_key_pem(),
    }
}

#[cfg(feature = "tls")]
pub(crate) async fn spawn_tls_echo_server(
    mut server_config: rustls::ServerConfig,
    require_client_certificate: bool,
) -> (SocketAddr, JoinHandle<()>) {
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    let handle = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("accept should succeed");
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};

                let mut stream = acceptor
                    .accept(stream)
                    .await
                    .expect("tls accept should succeed");
                let client_cert_count = stream
                    .get_ref()
                    .1
                    .peer_certificates()
                    .map(|certificates| certificates.len())
                    .unwrap_or(0);
                let mut request = Vec::new();
                let mut buffer = [0u8; 2048];
                let mut expected_body_len = None;

                loop {
                    let read = match stream.read(&mut buffer).await {
                        Ok(read) => read,
                        Err(err)
                            if matches!(
                                err.kind(),
                                std::io::ErrorKind::BrokenPipe
                                    | std::io::ErrorKind::ConnectionAborted
                                    | std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::UnexpectedEof
                            ) =>
                        {
                            break;
                        }
                        Err(err) => panic!("request read should succeed: {err}"),
                    };
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if expected_body_len.is_none()
                        && let Some(header_end) =
                            request.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        let header_end = header_end + 4;
                        let headers = String::from_utf8_lossy(&request[..header_end]);
                        let content_length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                if !name.eq_ignore_ascii_case("content-length") {
                                    return None;
                                }
                                value.trim().parse::<usize>().ok()
                            })
                            .unwrap_or(0);
                        expected_body_len = Some(header_end + content_length);
                    }
                    if let Some(total_len) = expected_body_len
                        && request.len() >= total_len
                    {
                        break;
                    }
                }

                let body = if let Some(header_end) =
                    request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    request[header_end + 4..].to_vec()
                } else {
                    Vec::new()
                };
                let body = String::from_utf8_lossy(&body).into_owned();
                let response_body = if require_client_certificate {
                    format!("mtls:{client_cert_count}:{body}")
                } else {
                    body
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .unwrap_or_else(|err| {
                        if matches!(
                            err.kind(),
                            std::io::ErrorKind::BrokenPipe
                                | std::io::ErrorKind::ConnectionAborted
                                | std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::UnexpectedEof
                        ) {
                            return;
                        }
                        panic!("response should write: {err}");
                    });
                if let Err(err) = stream.flush().await
                    && !matches!(
                        err.kind(),
                        std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::UnexpectedEof
                    )
                {
                    panic!("response should flush: {err}");
                }
                let _ = stream.shutdown().await;
            });
        }
    });
    (addr, handle)
}

#[cfg(feature = "tls")]
pub(crate) async fn spawn_ca_signed_https_echo_upstream(
    materials: &TlsTestMaterials,
    require_client_certificate: bool,
) -> (SocketAddr, JoinHandle<()>) {
    ensure_rustls_provider();

    let builder = rustls::ServerConfig::builder();
    let server_config = if require_client_certificate {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(materials.ca_der.clone()))
            .expect("ca cert should be trusted");
        let client_verifier = rustls::server::WebPkiClientVerifier::builder(roots.into())
            .build()
            .expect("client verifier should build");
        builder.with_client_cert_verifier(client_verifier)
    } else {
        builder.with_no_client_auth()
    }
    .with_single_cert(
        vec![CertificateDer::from(materials.server_cert_der.clone())],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(materials.server_key_der.clone())),
    )
    .expect("server config should build");

    spawn_tls_echo_server(server_config, require_client_certificate).await
}

pub(crate) fn build_short_circuit_program(body: &str, header: Option<(&str, &str)>) -> Program {
    let mut constants = Vec::new();
    let mut bc = BytecodeBuilder::new();

    if let Some((name, value)) = header {
        let name_index = constants.len() as u32;
        constants.push(Value::string(name));
        let value_index = constants.len() as u32;
        constants.push(Value::string(value));
        bc.ldc(name_index);
        bc.ldc(value_index);
        bc.call(FN_HTTP_RESPONSE_SET_HEADER, 2);
    }

    let body_index = constants.len() as u32;
    constants.push(Value::string(body));
    bc.ldc(body_index);
    bc.call(FN_HTTP_RESPONSE_SET_BODY, 1);
    bc.ret();

    Program::new(constants, bc.finish())
}

pub(crate) fn build_upstream_program(upstream: &str, header: Option<(&str, &str)>) -> Program {
    let mut constants = Vec::new();
    let mut bc = BytecodeBuilder::new();
    let (scheme, remainder) = if let Some(value) = upstream.strip_prefix("https://") {
        ("https", value)
    } else if let Some(value) = upstream.strip_prefix("http://") {
        ("http", value)
    } else {
        ("http", upstream)
    };
    let (authority, path_and_query) = if let Some((authority, rest)) = remainder.split_once('/') {
        (authority, Some(format!("/{rest}")))
    } else {
        (remainder, None)
    };
    let (path, query) = if let Some(path_and_query) = path_and_query.as_deref() {
        if let Some((path, query)) = path_and_query.split_once('?') {
            (Some(path.to_string()), Some(query.to_string()))
        } else {
            (Some(path_and_query.to_string()), None)
        }
    } else {
        (None, None)
    };
    let (host, port) = authority
        .rsplit_once(':')
        .expect("upstream authority should include a port");
    let port = port
        .parse::<i64>()
        .expect("upstream authority port should be numeric");

    let exchange_index = constants.len() as u32;
    constants.push(Value::Int(1));
    let host_index = constants.len() as u32;
    constants.push(Value::string(host));
    let port_index = constants.len() as u32;
    constants.push(Value::Int(port));

    if scheme == "https" {
        let scheme_index = constants.len() as u32;
        constants.push(Value::string("https"));
        bc.ldc(exchange_index);
        bc.ldc(scheme_index);
        bc.call(
            function_by_name("http::exchange::set_scheme")
                .expect("http::exchange::set_scheme should exist")
                .index,
            2,
        );
    }

    bc.ldc(exchange_index);
    bc.ldc(host_index);
    bc.ldc(port_index);
    bc.call(
        function_by_name("http::exchange::set_target")
            .expect("http::exchange::set_target should exist")
            .index,
        3,
    );

    if let Some(path) = path {
        let path_index = constants.len() as u32;
        constants.push(Value::string(path));
        bc.ldc(exchange_index);
        bc.ldc(path_index);
        bc.call(
            function_by_name("http::exchange::set_path")
                .expect("http::exchange::set_path should exist")
                .index,
            2,
        );
    }

    if let Some(query) = query {
        let query_index = constants.len() as u32;
        constants.push(Value::string(query));
        bc.ldc(exchange_index);
        bc.ldc(query_index);
        bc.call(
            function_by_name("http::exchange::set_query")
                .expect("http::exchange::set_query should exist")
                .index,
            2,
        );
    }

    if let Some((name, value)) = header {
        let name_index = constants.len() as u32;
        constants.push(Value::string(name));
        let value_index = constants.len() as u32;
        constants.push(Value::string(value));
        bc.ldc(name_index);
        bc.ldc(value_index);
        bc.call(FN_HTTP_RESPONSE_SET_HEADER, 2);
    }

    bc.ret();
    Program::new(constants, bc.finish())
}

pub(crate) async fn upload_program(
    client: &reqwest::Client,
    admin_addr: SocketAddr,
    program: &Program,
) -> reqwest::Response {
    let bytes = encode_program(program).expect("encode should succeed");
    client
        .put(format!("http://{admin_addr}/program"))
        .header("content-type", "application/octet-stream")
        .body(bytes)
        .send()
        .await
        .expect("upload request should complete")
}

pub(crate) fn reserve_tcp_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind should succeed");
    let addr = listener.local_addr().expect("local addr should exist");
    drop(listener);
    addr
}

pub(crate) async fn send_pdb_continue(addr: SocketAddr) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(addr))
        .await
        .expect("pdb helper connect timed out")
        .expect("debugger tcp should accept connections");

    let mut banner = [0u8; 512];
    let _ = timeout(Duration::from_millis(300), stream.read(&mut banner)).await;
    timeout(Duration::from_secs(1), stream.write_all(b"continue\n"))
        .await
        .expect("pdb helper write timed out")
        .expect("pdb continue command should be written");
}
