pub(crate) use std::{
    io::Read,
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

pub(crate) use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderValue, Request, Response, StatusCode},
    routing::{any, post},
};
pub(crate) use base64::{Engine as _, engine::general_purpose::STANDARD};
#[cfg(feature = "tls")]
use edge::sample_echo::spawn_https_echo_server;
#[cfg(feature = "webrtc")]
pub(crate) use edge::sample_echo::spawn_webrtc_echo_server;
pub(crate) use edge::{
    ActiveControlPlaneConfig, CommandResultPayload, ControlPlaneCommand, EdgeCommandResult,
    EdgePollRequest, EdgePollResponse, FN_HTTP_RESPONSE_SET_BODY, FN_HTTP_RESPONSE_SET_HEADER,
    FN_HTTP_UPSTREAM_REQUEST_SET_TARGET, ProxyVmContext, RateLimiterStore, SharedState,
    VmAsyncOpBridge, build_admin_app, build_http_proxy_app, compile_edge_source_file,
    enter_edge_host_context, new_shared_vm_async_ops, register_http_plane_host_module,
    serve_http_proxy, serve_transport_proxy, spawn_active_control_plane_client,
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

#[cfg(feature = "tls")]
pub(crate) fn ensure_rustls_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .expect("rustls crypto provider should install");
    });
}

pub(crate) async fn spawn_server(app: Router) -> (SocketAddr, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("server should run");
    });
    (addr, handle)
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

pub(crate) async fn spawn_chunked_upstream(
    chunks: Vec<&'static str>,
) -> (SocketAddr, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
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

pub(crate) async fn spawn_sse_upstream(lines: Vec<&'static str>) -> (SocketAddr, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
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

#[cfg(feature = "websocket")]
pub(crate) async fn spawn_websocket_echo_upstream() -> (SocketAddr, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
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

#[cfg(feature = "tls")]
pub(crate) async fn spawn_https_echo_upstream() -> (SocketAddr, JoinHandle<()>) {
    // Keep the baseline HTTPS fixture aligned with the sample binary so manual examples and tests
    // do not drift on TLS or HTTP behavior.
    spawn_https_echo_server("127.0.0.1:0".parse().expect("valid addr"))
        .await
        .expect("https echo should start")
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

    let upstream_index = constants.len() as u32;
    constants.push(Value::string(upstream));
    bc.ldc(upstream_index);
    bc.call(FN_HTTP_UPSTREAM_REQUEST_SET_TARGET, 1);

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
