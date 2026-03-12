#[cfg(feature = "http")]
use std::convert::Infallible;
#[cfg(any(feature = "tls", feature = "webrtc"))]
use std::sync::Arc;
#[cfg(feature = "webrtc")]
use std::{collections::HashMap, sync::Mutex};
use std::{io, net::SocketAddr};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
    task::JoinHandle,
};
use tracing::warn;

#[cfg(feature = "webrtc")]
use ::webrtc::{
    api::{
        APIBuilder, interceptor_registry::register_default_interceptors, media_engine::MediaEngine,
        setting_engine::SettingEngine,
    },
    data_channel::{RTCDataChannel, data_channel_message::DataChannelMessage},
    interceptor::registry::Registry,
    peer_connection::{
        RTCPeerConnection, configuration::RTCConfiguration,
        peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
};
#[cfg(feature = "http")]
use axum::body::Bytes;
#[cfg(feature = "websocket")]
use futures_util::{SinkExt, StreamExt};
#[cfg(feature = "http")]
use http_body_util::{BodyExt, Full};
#[cfg(all(feature = "http", not(feature = "http2")))]
use hyper::server::conn::http1;
#[cfg(feature = "http")]
use hyper::{
    Request, Response, StatusCode,
    body::Incoming,
    header::{CONTENT_TYPE, HeaderValue},
    service::service_fn,
};
#[cfg(feature = "http")]
use hyper_util::rt::TokioIo;
#[cfg(all(feature = "http", feature = "http2"))]
use hyper_util::{rt::TokioExecutor, server::conn::auto::Builder as AutoBuilder};
#[cfg(feature = "tls")]
use rcgen::generate_simple_self_signed;
#[cfg(feature = "tls")]
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};
#[cfg(feature = "websocket")]
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{Request as WsRequest, Response as WsResponse},
        http::HeaderValue as WsHeaderValue,
    },
};
#[cfg(feature = "webrtc")]
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleEchoServerConfig {
    pub tcp_addr: SocketAddr,
    pub udp_addr: SocketAddr,
    pub tls_addr: SocketAddr,
    pub http_addr: SocketAddr,
    pub https_addr: SocketAddr,
    pub websocket_addr: SocketAddr,
    pub websocket_tls_addr: SocketAddr,
    pub webrtc_addr: SocketAddr,
    pub forward_proxy_addr: SocketAddr,
}

impl Default for SampleEchoServerConfig {
    fn default() -> Self {
        Self {
            tcp_addr: "127.0.0.1:7001".parse().expect("valid tcp addr"),
            udp_addr: "127.0.0.1:7002".parse().expect("valid udp addr"),
            tls_addr: "127.0.0.1:7003".parse().expect("valid tls addr"),
            http_addr: "127.0.0.1:7004".parse().expect("valid http addr"),
            https_addr: "127.0.0.1:7005".parse().expect("valid https addr"),
            websocket_addr: "127.0.0.1:7006".parse().expect("valid websocket addr"),
            websocket_tls_addr: "127.0.0.1:7007"
                .parse()
                .expect("valid secure websocket addr"),
            webrtc_addr: "127.0.0.1:7008".parse().expect("valid webrtc addr"),
            forward_proxy_addr: "127.0.0.1:7009".parse().expect("valid forward proxy addr"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleEchoAddresses {
    pub tcp: SocketAddr,
    pub udp: SocketAddr,
    pub tls: Option<SocketAddr>,
    pub http: Option<SocketAddr>,
    pub https: Option<SocketAddr>,
    pub websocket: Option<SocketAddr>,
    pub websocket_tls: Option<SocketAddr>,
    pub webrtc: Option<SocketAddr>,
    pub forward_proxy: SocketAddr,
}

pub struct SampleEchoServer {
    pub addresses: SampleEchoAddresses,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for SampleEchoServer {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

pub async fn spawn_sample_echo_server(
    config: SampleEchoServerConfig,
) -> io::Result<SampleEchoServer> {
    let mut tasks = Vec::new();

    let (tcp, tcp_task) = spawn_tcp_echo_server(config.tcp_addr).await?;
    tasks.push(tcp_task);

    let (udp, udp_task) = spawn_udp_echo_server(config.udp_addr).await?;
    tasks.push(udp_task);

    #[cfg(feature = "http")]
    let (http, http_task) = spawn_http_echo_server(config.http_addr).await?;
    #[cfg(feature = "http")]
    tasks.push(http_task);

    #[cfg(feature = "tls")]
    let shared_stream_tls_config = generate_self_signed_tls_server_config(http1_alpn_protocols())?;
    #[cfg(feature = "tls")]
    let shared_https_tls_config = generate_sample_https_tls_server_config()?;

    #[cfg(feature = "tls")]
    let (tls, tls_task) =
        spawn_tls_echo_server_with_config(config.tls_addr, shared_stream_tls_config.clone())
            .await?;
    #[cfg(feature = "tls")]
    tasks.push(tls_task);

    #[cfg(feature = "tls")]
    let (https, https_task) =
        spawn_https_echo_server_with_config(config.https_addr, shared_https_tls_config).await?;
    #[cfg(feature = "tls")]
    tasks.push(https_task);

    #[cfg(feature = "websocket")]
    let (websocket, websocket_task) = spawn_websocket_echo_server(config.websocket_addr).await?;
    #[cfg(feature = "websocket")]
    tasks.push(websocket_task);

    #[cfg(all(feature = "websocket", feature = "tls"))]
    let (websocket_tls, websocket_tls_task) = spawn_secure_websocket_echo_server_with_config(
        config.websocket_tls_addr,
        shared_stream_tls_config,
    )
    .await?;
    #[cfg(all(feature = "websocket", feature = "tls"))]
    tasks.push(websocket_tls_task);

    #[cfg(feature = "webrtc")]
    let (webrtc, webrtc_task) = spawn_webrtc_echo_server(config.webrtc_addr).await?;
    #[cfg(feature = "webrtc")]
    tasks.push(webrtc_task);

    let (forward_proxy, forward_proxy_task) =
        spawn_connect_forward_proxy(config.forward_proxy_addr).await?;
    tasks.push(forward_proxy_task);

    Ok(SampleEchoServer {
        addresses: SampleEchoAddresses {
            tcp,
            udp,
            #[cfg(feature = "tls")]
            tls: Some(tls),
            #[cfg(not(feature = "tls"))]
            tls: None,
            #[cfg(feature = "http")]
            http: Some(http),
            #[cfg(not(feature = "http"))]
            http: None,
            #[cfg(feature = "tls")]
            https: Some(https),
            #[cfg(not(feature = "tls"))]
            https: None,
            #[cfg(feature = "websocket")]
            websocket: Some(websocket),
            #[cfg(not(feature = "websocket"))]
            websocket: None,
            #[cfg(all(feature = "websocket", feature = "tls"))]
            websocket_tls: Some(websocket_tls),
            #[cfg(not(all(feature = "websocket", feature = "tls")))]
            websocket_tls: None,
            #[cfg(feature = "webrtc")]
            webrtc: Some(webrtc),
            #[cfg(not(feature = "webrtc"))]
            webrtc: None,
            forward_proxy,
        },
        tasks,
    })
}

pub async fn spawn_tcp_echo_server(addr: SocketAddr) -> io::Result<(SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((mut stream, _)) => {
                    tokio::spawn(async move {
                        let mut buffer = [0u8; 8192];
                        loop {
                            match stream.read(&mut buffer).await {
                                Ok(0) => break,
                                Ok(read) => {
                                    if let Err(err) = stream.write_all(&buffer[..read]).await {
                                        warn!("sample tcp echo write failed: {err}");
                                        break;
                                    }
                                }
                                Err(err) => {
                                    warn!("sample tcp echo read failed: {err}");
                                    break;
                                }
                            }
                        }
                    });
                }
                Err(err) => {
                    warn!("sample tcp echo accept failed: {err}");
                    break;
                }
            }
        }
    });
    Ok((local_addr, handle))
}

pub async fn spawn_udp_echo_server(addr: SocketAddr) -> io::Result<(SocketAddr, JoinHandle<()>)> {
    let socket = UdpSocket::bind(addr).await?;
    let local_addr = socket.local_addr()?;
    let handle = tokio::spawn(async move {
        let mut buffer = [0u8; 65_535];
        loop {
            match socket.recv_from(&mut buffer).await {
                Ok((read, peer)) => {
                    if let Err(err) = socket.send_to(&buffer[..read], peer).await {
                        warn!("sample udp echo send failed: {err}");
                    }
                }
                Err(err) => {
                    warn!("sample udp echo recv failed: {err}");
                    break;
                }
            }
        }
    });
    Ok((local_addr, handle))
}

pub async fn spawn_connect_forward_proxy(
    addr: SocketAddr,
) -> io::Result<(SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((mut downstream, _)) => {
                    tokio::spawn(async move {
                        use tokio::io::copy_bidirectional;

                        let mut request = Vec::new();
                        let mut buffer = [0u8; 1024];
                        loop {
                            match downstream.read(&mut buffer).await {
                                Ok(0) => return,
                                Ok(read) => {
                                    request.extend_from_slice(&buffer[..read]);
                                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                                        break;
                                    }
                                }
                                Err(err) => {
                                    warn!("sample forward proxy request read failed: {err}");
                                    return;
                                }
                            }
                        }

                        let request_text = String::from_utf8_lossy(&request);
                        let Some(first_line) = request_text.lines().next() else {
                            return;
                        };
                        let mut parts = first_line.split_whitespace();
                        let method = parts.next().unwrap_or("");
                        let authority = parts.next().unwrap_or("");

                        if !method.eq_ignore_ascii_case("CONNECT") || authority.is_empty() {
                            let response = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                            if let Err(err) = downstream.write_all(response).await {
                                warn!("sample forward proxy bad request response failed: {err}");
                            }
                            let _ = downstream.shutdown().await;
                            return;
                        }

                        let mut upstream = match tokio::net::TcpStream::connect(authority).await {
                            Ok(stream) => stream,
                            Err(err) => {
                                warn!("sample forward proxy upstream connect failed: {err}");
                                let response = b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                                if let Err(write_err) = downstream.write_all(response).await {
                                    warn!(
                                        "sample forward proxy bad gateway response failed: {write_err}"
                                    );
                                }
                                let _ = downstream.shutdown().await;
                                return;
                            }
                        };

                        let response = b"HTTP/1.1 200 Connection Established\r\nProxy-Agent: pd-edge-sample-echo-server\r\n\r\n";
                        if let Err(err) = downstream.write_all(response).await {
                            warn!("sample forward proxy connect response failed: {err}");
                            let _ = downstream.shutdown().await;
                            let _ = upstream.shutdown().await;
                            return;
                        }

                        if let Err(err) = copy_bidirectional(&mut downstream, &mut upstream).await {
                            warn!("sample forward proxy tunnel failed: {err}");
                        }
                        let _ = downstream.shutdown().await;
                        let _ = upstream.shutdown().await;
                    });
                }
                Err(err) => {
                    warn!("sample forward proxy accept failed: {err}");
                    break;
                }
            }
        }
    });
    Ok((local_addr, handle))
}

#[cfg(feature = "http")]
pub async fn spawn_http_echo_server(addr: SocketAddr) -> io::Result<(SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        serve_http_echo_connection(io, "http").await;
                    });
                }
                Err(err) => {
                    warn!("sample http echo accept failed: {err}");
                    break;
                }
            }
        }
    });
    Ok((local_addr, handle))
}

#[cfg(feature = "tls")]
pub async fn spawn_tls_echo_server(addr: SocketAddr) -> io::Result<(SocketAddr, JoinHandle<()>)> {
    let tls_config = generate_self_signed_tls_server_config(http1_alpn_protocols())?;
    spawn_tls_echo_server_with_config(addr, tls_config).await
}

#[cfg(feature = "tls")]
pub async fn spawn_https_echo_server(addr: SocketAddr) -> io::Result<(SocketAddr, JoinHandle<()>)> {
    let tls_config = generate_sample_https_tls_server_config()?;
    spawn_https_echo_server_with_config(addr, tls_config).await
}

#[cfg(feature = "tls")]
async fn spawn_tls_echo_server_with_config(
    addr: SocketAddr,
    tls_config: Arc<ServerConfig>,
) -> io::Result<(SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    let acceptor = TlsAcceptor::from(tls_config);
    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let acceptor = acceptor.clone();
                    tokio::spawn(async move {
                        let mut stream = match acceptor.accept(stream).await {
                            Ok(stream) => stream,
                            Err(err) => {
                                warn!("sample tls echo handshake failed: {err}");
                                return;
                            }
                        };
                        let mut buffer = [0u8; 8192];
                        loop {
                            match stream.read(&mut buffer).await {
                                Ok(0) => break,
                                Ok(read) => {
                                    if let Err(err) = stream.write_all(&buffer[..read]).await {
                                        warn!("sample tls echo write failed: {err}");
                                        break;
                                    }
                                }
                                Err(err) => {
                                    warn!("sample tls echo read failed: {err}");
                                    break;
                                }
                            }
                        }
                    });
                }
                Err(err) => {
                    warn!("sample tls echo accept failed: {err}");
                    break;
                }
            }
        }
    });
    Ok((local_addr, handle))
}

#[cfg(feature = "tls")]
async fn spawn_https_echo_server_with_config(
    addr: SocketAddr,
    tls_config: Arc<ServerConfig>,
) -> io::Result<(SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    let acceptor = TlsAcceptor::from(tls_config);
    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let acceptor = acceptor.clone();
                    tokio::spawn(async move {
                        let tls_stream = match acceptor.accept(stream).await {
                            Ok(stream) => stream,
                            Err(err) => {
                                warn!("sample https echo handshake failed: {err}");
                                return;
                            }
                        };
                        let io = TokioIo::new(tls_stream);
                        serve_http_echo_connection(io, "https").await;
                    });
                }
                Err(err) => {
                    warn!("sample https echo accept failed: {err}");
                    break;
                }
            }
        }
    });
    Ok((local_addr, handle))
}

#[cfg(feature = "websocket")]
pub async fn spawn_websocket_echo_server(
    addr: SocketAddr,
) -> io::Result<(SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(async move {
                        if let Err(err) = run_websocket_echo_session(stream).await {
                            warn!("sample websocket echo session failed: {err}");
                        }
                    });
                }
                Err(err) => {
                    warn!("sample websocket echo accept failed: {err}");
                    break;
                }
            }
        }
    });
    Ok((local_addr, handle))
}

#[cfg(all(feature = "websocket", feature = "tls"))]
pub async fn spawn_secure_websocket_echo_server(
    addr: SocketAddr,
) -> io::Result<(SocketAddr, JoinHandle<()>)> {
    let tls_config = generate_self_signed_tls_server_config(http1_alpn_protocols())?;
    spawn_secure_websocket_echo_server_with_config(addr, tls_config).await
}

#[cfg(all(feature = "websocket", feature = "tls"))]
async fn spawn_secure_websocket_echo_server_with_config(
    addr: SocketAddr,
    tls_config: Arc<ServerConfig>,
) -> io::Result<(SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    let acceptor = TlsAcceptor::from(tls_config);
    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let acceptor = acceptor.clone();
                    tokio::spawn(async move {
                        let tls_stream = match acceptor.accept(stream).await {
                            Ok(stream) => stream,
                            Err(err) => {
                                warn!("sample secure websocket handshake failed: {err}");
                                return;
                            }
                        };
                        if let Err(err) = run_websocket_echo_session(tls_stream).await {
                            warn!("sample secure websocket session failed: {err}");
                        }
                    });
                }
                Err(err) => {
                    warn!("sample secure websocket accept failed: {err}");
                    break;
                }
            }
        }
    });
    Ok((local_addr, handle))
}

#[cfg(feature = "webrtc")]
pub async fn spawn_webrtc_echo_server(
    addr: SocketAddr,
) -> io::Result<(SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    let peers = Arc::new(Mutex::new(HashMap::<String, Arc<RTCPeerConnection>>::new()));
    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let peers = peers.clone();
                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let service = service_fn(move |request| {
                            let peers = peers.clone();
                            handle_webrtc_signal_request(peers, request)
                        });
                        if let Err(err) = http1::Builder::new().serve_connection(io, service).await
                        {
                            warn!("sample webrtc signaling connection failed: {err}");
                        }
                    });
                }
                Err(err) => {
                    warn!("sample webrtc signaling accept failed: {err}");
                    break;
                }
            }
        }
    });
    Ok((local_addr, handle))
}

#[cfg(feature = "http")]
async fn serve_http_echo_connection<S>(io: TokioIo<S>, protocol: &'static str)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service = service_fn(move |request| handle_http_echo_request(protocol, request));

    #[cfg(feature = "http2")]
    {
        if let Err(err) = AutoBuilder::new(TokioExecutor::new())
            .serve_connection(io, service)
            .await
        {
            warn!("sample {protocol} echo connection failed: {err}");
        }
    }

    #[cfg(not(feature = "http2"))]
    {
        if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
            warn!("sample {protocol} echo connection failed: {err}");
        }
    }
}

#[cfg(feature = "http")]
type EchoResponse = Response<Full<Bytes>>;

#[cfg(feature = "http")]
fn build_echo_response(status: StatusCode, protocol: &'static str, body: Bytes) -> EchoResponse {
    let mut response = Response::new(Full::new(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert("x-echo-protocol", HeaderValue::from_static(protocol));
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

#[cfg(feature = "http")]
async fn handle_http_echo_request(
    protocol: &'static str,
    request: Request<Incoming>,
) -> Result<EchoResponse, Infallible> {
    let method = request.method().clone();
    let version = request.version();
    let path = request.uri().path().to_string();
    let body = match request.into_body().collect().await {
        Ok(body) => body.to_bytes(),
        Err(err) => {
            let body = Bytes::from(format!("failed to read request body: {err}"));
            return Ok(build_echo_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                protocol,
                body,
            ));
        }
    };

    let echoed_body = if body.is_empty() {
        Bytes::from(format!("echo:{protocol}:{}:{path}", method.as_str()))
    } else {
        body
    };

    let mut response = build_echo_response(StatusCode::OK, protocol, echoed_body.clone());
    if let Ok(value) = HeaderValue::from_str(method.as_str()) {
        response.headers_mut().insert("x-echo-method", value);
    }
    if let Ok(value) = HeaderValue::from_str(&path) {
        response.headers_mut().insert("x-echo-path", value);
    }
    if let Ok(value) = HeaderValue::from_str(&echoed_body.len().to_string()) {
        response.headers_mut().insert("x-echo-bytes", value);
    }
    response.headers_mut().insert(
        "x-echo-http-version",
        HeaderValue::from_static(http_version_label(version)),
    );
    Ok(response)
}

#[cfg(feature = "http")]
fn http_version_label(version: hyper::Version) -> &'static str {
    match version {
        hyper::Version::HTTP_09 => "0.9",
        hyper::Version::HTTP_10 => "1.0",
        hyper::Version::HTTP_11 => "1.1",
        hyper::Version::HTTP_2 => "2",
        hyper::Version::HTTP_3 => "3",
        _ => "unknown",
    }
}

#[cfg(any(feature = "tls", feature = "webrtc"))]
fn ensure_rustls_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        #[cfg(feature = "webrtc")]
        {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        }
        #[cfg(all(feature = "tls", not(feature = "webrtc")))]
        {
            let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
        }
    });
}

#[cfg(feature = "tls")]
fn generate_self_signed_tls_server_config(
    alpn_protocols: Vec<Vec<u8>>,
) -> io::Result<Arc<ServerConfig>> {
    ensure_rustls_provider();
    let certificate =
        generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .map_err(|err| {
                io::Error::other(format!("failed to generate self-signed cert: {err}"))
            })?;
    let certificate_der = certificate
        .serialize_der()
        .map_err(|err| io::Error::other(format!("failed to serialize cert der: {err}")))?;
    let private_key_der = certificate.serialize_private_key_der();
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(certificate_der)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(private_key_der)),
        )
        .map_err(|err| io::Error::other(format!("failed to build rustls config: {err}")))?;
    config.alpn_protocols = alpn_protocols;
    Ok(Arc::new(config))
}

#[cfg(feature = "tls")]
fn http1_alpn_protocols() -> Vec<Vec<u8>> {
    vec![b"http/1.1".to_vec()]
}

#[cfg(feature = "tls")]
fn generate_sample_https_tls_server_config() -> io::Result<Arc<ServerConfig>> {
    #[cfg(feature = "http2")]
    {
        return generate_self_signed_tls_server_config(vec![b"h2".to_vec(), b"http/1.1".to_vec()]);
    }

    #[cfg(not(feature = "http2"))]
    {
        generate_self_signed_tls_server_config(http1_alpn_protocols())
    }
}

#[cfg(feature = "websocket")]
#[allow(clippy::result_large_err)]
fn negotiate_chat_subprotocol(
    request: &WsRequest,
    mut response: WsResponse,
) -> Result<WsResponse, tokio_tungstenite::tungstenite::handshake::server::ErrorResponse> {
    let requested = request
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
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
}

#[cfg(feature = "websocket")]
async fn run_websocket_echo_session<S>(
    stream: S,
) -> Result<(), tokio_tungstenite::tungstenite::Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut websocket = accept_hdr_async(stream, negotiate_chat_subprotocol).await?;
    while let Some(message) = websocket.next().await {
        match message? {
            Message::Text(text) => {
                websocket.send(Message::Text(text)).await?;
            }
            Message::Binary(payload) => {
                websocket.send(Message::Binary(payload)).await?;
            }
            Message::Ping(payload) => {
                websocket.send(Message::Pong(payload)).await?;
            }
            Message::Pong(_) => {}
            Message::Close(frame) => {
                let _ = websocket.close(frame).await;
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(feature = "webrtc")]
type SharedWebRtcPeerMap = Arc<Mutex<HashMap<String, Arc<RTCPeerConnection>>>>;

#[cfg(feature = "webrtc")]
async fn handle_webrtc_signal_request(
    peers: SharedWebRtcPeerMap,
    request: Request<Incoming>,
) -> Result<EchoResponse, Infallible> {
    match (request.method().clone(), request.uri().path().to_string()) {
        (hyper::Method::GET, path) if path == "/" => Ok(build_echo_response(
            StatusCode::OK,
            "webrtc",
            Bytes::from("POST /offer with a WebRTC offer SDP JSON body to receive an answer."),
        )),
        (hyper::Method::POST, path) if path == "/offer" => {
            let body = match request.into_body().collect().await {
                Ok(body) => body.to_bytes(),
                Err(err) => {
                    let body = Bytes::from(format!("failed to read webrtc offer body: {err}"));
                    return Ok(build_echo_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "webrtc",
                        body,
                    ));
                }
            };
            let offer = match String::from_utf8(body.to_vec()) {
                Ok(offer) => offer,
                Err(err) => {
                    let body =
                        Bytes::from(format!("webrtc offer body must be valid utf-8 JSON: {err}"));
                    return Ok(build_echo_response(StatusCode::BAD_REQUEST, "webrtc", body));
                }
            };
            match build_webrtc_answer(peers, &offer).await {
                Ok(answer) => {
                    let mut response =
                        build_echo_response(StatusCode::OK, "webrtc", Bytes::from(answer));
                    response.headers_mut().insert(
                        CONTENT_TYPE,
                        HeaderValue::from_static("application/json; charset=utf-8"),
                    );
                    Ok(response)
                }
                Err(err) => Ok(build_echo_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "webrtc",
                    Bytes::from(err),
                )),
            }
        }
        _ => Ok(build_echo_response(
            StatusCode::NOT_FOUND,
            "webrtc",
            Bytes::from("not found"),
        )),
    }
}

#[cfg(feature = "webrtc")]
async fn build_webrtc_answer(
    peers: SharedWebRtcPeerMap,
    offer_json: &str,
) -> Result<String, String> {
    ensure_rustls_provider();
    let offer = serde_json::from_str::<RTCSessionDescription>(offer_json)
        .map_err(|err| format!("webrtc offer must be valid JSON: {err}"))?;
    let peer_id = Uuid::new_v4().to_string();
    let peer = create_webrtc_echo_peer(peer_id.clone(), peers.clone())
        .await
        .map_err(|err| format!("failed to create webrtc echo peer: {err}"))?;
    peer.set_remote_description(offer)
        .await
        .map_err(|err| format!("failed to set webrtc offer: {err}"))?;
    let answer = peer
        .create_answer(None)
        .await
        .map_err(|err| format!("failed to create webrtc answer: {err}"))?;
    let mut gather_complete = peer.gathering_complete_promise().await;
    peer.set_local_description(answer)
        .await
        .map_err(|err| format!("failed to set webrtc local answer: {err}"))?;
    let _ = gather_complete.recv().await;
    let local = peer
        .local_description()
        .await
        .ok_or_else(|| "webrtc local answer is unavailable".to_string())?;
    let answer_json = serde_json::to_string(&local)
        .map_err(|err| format!("failed to serialize webrtc answer: {err}"))?;
    peers
        .lock()
        .expect("webrtc peer map lock poisoned")
        .insert(peer_id, peer);
    Ok(answer_json)
}

#[cfg(feature = "webrtc")]
async fn create_webrtc_echo_peer(
    peer_id: String,
    peers: SharedWebRtcPeerMap,
) -> Result<Arc<RTCPeerConnection>, String> {
    let mut media_engine = MediaEngine::default();
    media_engine
        .register_default_codecs()
        .map_err(|err| format!("failed to register webrtc codecs: {err}"))?;
    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media_engine)
        .map_err(|err| format!("failed to register webrtc interceptors: {err}"))?;
    let mut setting_engine = SettingEngine::default();
    setting_engine.set_include_loopback_candidate(true);
    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_setting_engine(setting_engine)
        .build();
    let peer = Arc::new(
        api.new_peer_connection(RTCConfiguration::default())
            .await
            .map_err(|err| format!("failed to create peer connection: {err}"))?,
    );

    let peers_for_state = peers.clone();
    let peer_id_for_state = peer_id.clone();
    peer.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
        let peers = peers_for_state.clone();
        let peer_id = peer_id_for_state.clone();
        Box::pin(async move {
            if matches!(
                state,
                RTCPeerConnectionState::Failed
                    | RTCPeerConnectionState::Closed
                    | RTCPeerConnectionState::Disconnected
            ) {
                peers
                    .lock()
                    .expect("webrtc peer map lock poisoned")
                    .remove(&peer_id);
            }
        })
    }));

    peer.on_data_channel(Box::new(move |data_channel: Arc<RTCDataChannel>| {
        Box::pin(async move {
            attach_webrtc_echo_channel(data_channel).await;
        })
    }));

    Ok(peer)
}

#[cfg(feature = "webrtc")]
async fn attach_webrtc_echo_channel(data_channel: Arc<RTCDataChannel>) {
    let echo_channel = data_channel.clone();
    data_channel.on_message(Box::new(move |message: DataChannelMessage| {
        let data_channel = echo_channel.clone();
        Box::pin(async move {
            if message.is_string {
                let text = String::from_utf8_lossy(&message.data).into_owned();
                let _ = data_channel.send_text(text).await;
            } else {
                let _ = data_channel.send(&Bytes::from(message.data.to_vec())).await;
            }
        })
    }));
}
