use std::{
    io::Read,
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderValue, Request, Response, StatusCode},
    routing::{any, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use edge::{
    ActiveControlPlaneConfig, CommandResultPayload, ControlPlaneCommand, EdgeCommandResult,
    EdgePollRequest, EdgePollResponse, FN_HTTP_RESPONSE_SET_BODY, FN_HTTP_RESPONSE_SET_HEADER,
    FN_HTTP_UPSTREAM_REQUEST_SET_TARGET, SharedState, build_admin_app, build_http_proxy_app,
    compile_edge_source_file, spawn_active_control_plane_client,
};
use futures_util::{SinkExt, StreamExt};
use tokio::{sync::Notify, task::JoinHandle, time::timeout};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        self,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{Request as WsRequest, Response as WsResponse},
        http::HeaderValue as WsHeaderValue,
    },
};
use vm::{BytecodeBuilder, Program, Value, compile_source, encode_program};

fn ensure_rustls_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .expect("rustls crypto provider should install");
    });
}

async fn spawn_server(app: Router) -> (SocketAddr, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("server should run");
    });
    (addr, handle)
}

async fn spawn_proxy(
    max_program_bytes: usize,
) -> (SocketAddr, SocketAddr, JoinHandle<()>, JoinHandle<()>) {
    let state = SharedState::new(max_program_bytes);
    spawn_proxy_with_state(state).await
}

async fn spawn_proxy_with_state(
    state: SharedState,
) -> (SocketAddr, SocketAddr, JoinHandle<()>, JoinHandle<()>) {
    let (data_addr, data_handle) = spawn_server(build_http_proxy_app(state.clone())).await;
    let (admin_addr, admin_handle) = spawn_server(build_admin_app(state)).await;
    (data_addr, admin_addr, data_handle, admin_handle)
}

async fn spawn_chunked_upstream(chunks: Vec<&'static str>) -> (SocketAddr, JoinHandle<()>) {
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

async fn spawn_websocket_echo_upstream() -> (SocketAddr, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    let handle = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("accept should succeed");
            tokio::spawn(async move {
                let callback = |request: &WsRequest, mut response: WsResponse| {
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
                };

                let mut websocket = accept_hdr_async(stream, callback)
                    .await
                    .expect("websocket accept should succeed");
                while let Some(message) = websocket.next().await {
                    match message.expect("websocket message should decode") {
                        Message::Text(text) => {
                            websocket
                                .send(Message::Text(format!("echo:{text}").into()))
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

async fn spawn_https_echo_upstream() -> (SocketAddr, JoinHandle<()>) {
    ensure_rustls_provider();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("certificate should generate");
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(
                cert.serialize_der().expect("certificate should serialize"),
            )],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.serialize_private_key_der())),
        )
        .expect("server config should build");
    spawn_tls_echo_server(server_config, false).await
}

#[derive(Clone)]
struct TlsTestMaterials {
    ca_pem: String,
    ca_der: Vec<u8>,
    server_cert_der: Vec<u8>,
    server_key_der: Vec<u8>,
    client_cert_pem: String,
    client_key_pem: String,
}

fn source_string_literal(value: &str) -> String {
    serde_json::to_string(value).expect("source literal should serialize")
}

fn build_ca_signed_tls_materials() -> TlsTestMaterials {
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

async fn spawn_tls_echo_server(
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
                    let read = stream
                        .read(&mut buffer)
                        .await
                        .expect("request read should succeed");
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
                    .expect("response should write");
                stream.flush().await.expect("response should flush");
                let _ = stream.shutdown().await;
            });
        }
    });
    (addr, handle)
}

async fn spawn_ca_signed_https_echo_upstream(
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

fn build_short_circuit_program(body: &str, header: Option<(&str, &str)>) -> Program {
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

fn build_upstream_program(upstream: &str, header: Option<(&str, &str)>) -> Program {
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

async fn upload_program(
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

fn reserve_tcp_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind should succeed");
    let addr = listener.local_addr().expect("local addr should exist");
    drop(listener);
    addr
}

async fn send_pdb_continue(addr: SocketAddr) {
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

#[tokio::test]
async fn no_active_program_returns_404() {
    let (data_addr, _admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let response = client
        .get(format!("http://{data_addr}/anything"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn upload_valid_program_controls_subsequent_requests() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program = build_short_circuit_program("hello vm", None);

    let upload = upload_program(&client, admin_addr, &program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.expect("body should read"), "hello vm");

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn short_circuit_path_returns_200_body_and_headers() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program = build_short_circuit_program("payload", Some(("x-vm", "short")));

    let upload = upload_program(&client, admin_addr, &program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-vm")
            .and_then(|value| value.to_str().ok()),
        Some("short")
    );
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/plain")
    );
    assert_eq!(response.text().await.expect("body should read"), "payload");

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn upstream_path_proxies_method_path_query_and_body() {
    let upstream_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let (parts, body) = request.into_parts();
        let body = to_bytes(body, usize::MAX)
            .await
            .expect("body should be readable");
        let path_and_query = parts
            .uri
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/");
        let content = format!(
            "{}|{}|{}",
            parts.method,
            path_and_query,
            String::from_utf8_lossy(&body)
        );
        let mut response = Response::new(Body::from(content));
        response
            .headers_mut()
            .insert("x-upstream", HeaderValue::from_static("yes"));
        response
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program = build_upstream_program(&upstream_addr.to_string(), None);

    let upload = upload_program(&client, admin_addr, &program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/api/v1/items?x=1"))
        .body("ping")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-upstream")
            .and_then(|value| value.to_str().ok()),
        Some("yes")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "POST|/api/v1/items?x=1|ping"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn upstream_accepts_full_url_with_path() {
    let upstream_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let path = request
            .uri()
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/");
        Response::new(Body::from(path.to_string()))
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program = build_upstream_program(&format!("http://{upstream_addr}/fixed"), None);

    let upload = upload_program(&client, admin_addr, &program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/other?x=1"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.expect("body should read"), "/fixed");

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn vm_response_headers_are_applied_on_short_circuit_and_proxied_paths() {
    let upstream_app = Router::new().fallback(any(|_request: Request<Body>| async move {
        let mut response = Response::new(Body::from("upstream"));
        response
            .headers_mut()
            .insert("x-vm", HeaderValue::from_static("from-upstream"));
        response
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let short_program = build_short_circuit_program("short", Some(("x-vm", "from-vm-short")));
    let upload_short = upload_program(&client, admin_addr, &short_program).await;
    assert_eq!(upload_short.status(), StatusCode::NO_CONTENT);
    let short_response = client
        .get(format!("http://{data_addr}/"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(
        short_response
            .headers()
            .get("x-vm")
            .and_then(|value| value.to_str().ok()),
        Some("from-vm-short")
    );

    let proxied_program =
        build_upstream_program(&upstream_addr.to_string(), Some(("x-vm", "from-vm-proxy")));
    let upload_proxy = upload_program(&client, admin_addr, &proxied_program).await;
    assert_eq!(upload_proxy.status(), StatusCode::NO_CONTENT);
    let proxied_response = client
        .get(format!("http://{data_addr}/"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(proxied_response.status(), StatusCode::OK);
    assert_eq!(
        proxied_response
            .headers()
            .get("x-vm")
            .and_then(|value| value.to_str().ok()),
        Some("from-vm-proxy")
    );
    assert_eq!(
        proxied_response.text().await.expect("body should read"),
        "upstream"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn invalid_upload_returns_400_and_keeps_previous_program() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let original = build_short_circuit_program("old", None);
    let upload_ok = upload_program(&client, admin_addr, &original).await;
    assert_eq!(upload_ok.status(), StatusCode::NO_CONTENT);

    let upload_bad = client
        .put(format!("http://{admin_addr}/program"))
        .header("content-type", "application/octet-stream")
        .body(vec![0u8, 1, 2, 3, 4])
        .send()
        .await
        .expect("upload should complete");
    assert_eq!(upload_bad.status(), StatusCode::BAD_REQUEST);

    let response = client
        .get(format!("http://{data_addr}/"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.expect("body should read"), "old");

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn in_flight_request_uses_old_program_after_swap() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    let started_for_handler = started.clone();
    let release_for_handler = release.clone();
    let upstream_app = Router::new().fallback(any(move |_request: Request<Body>| {
        let started = started_for_handler.clone();
        let release = release_for_handler.clone();
        async move {
            started.notify_one();
            release.notified().await;
            Response::new(Body::from("old"))
        }
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let old_program = build_upstream_program(&upstream_addr.to_string(), None);
    let upload_old = upload_program(&client, admin_addr, &old_program).await;
    assert_eq!(upload_old.status(), StatusCode::NO_CONTENT);

    let in_flight_client = client.clone();
    let in_flight_url = format!("http://{data_addr}/slow");
    let in_flight = tokio::spawn(async move {
        let response = in_flight_client
            .get(in_flight_url)
            .send()
            .await
            .expect("in-flight request should complete");
        let status = response.status();
        let body = response.text().await.expect("in-flight body should read");
        (status, body)
    });

    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("upstream should receive in-flight request");

    let new_program = build_short_circuit_program("new", None);
    let upload_new = upload_program(&client, admin_addr, &new_program).await;
    assert_eq!(upload_new.status(), StatusCode::NO_CONTENT);

    release.notify_waiters();

    let (status, body) = in_flight.await.expect("join should succeed");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "old");

    let next_response = client
        .get(format!("http://{data_addr}/next"))
        .send()
        .await
        .expect("next request should complete");
    assert_eq!(next_response.status(), StatusCode::OK);
    assert_eq!(next_response.text().await.expect("body should read"), "new");

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn upstream_unreachable_returns_502() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let closed_addr = listener.local_addr().expect("listener should have addr");
    drop(listener);

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let program = build_upstream_program(&closed_addr.to_string(), None);
    let upload = upload_program(&client, admin_addr, &program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn debug_session_lifecycle_endpoints_work() {
    let (_data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let debug_addr = reserve_tcp_addr();

    let start_response = client
        .put(format!("http://{admin_addr}/debug/session"))
        .header("content-type", "application/json")
        .body(format!(
            r#"{{"header_name":"x-debug","header_value":"on","tcp_addr":"{debug_addr}","stop_on_entry":true}}"#
        ))
        .send()
        .await
        .expect("start debug request should complete");
    assert_eq!(start_response.status(), StatusCode::CREATED);

    let status_response = client
        .get(format!("http://{admin_addr}/debug/session"))
        .send()
        .await
        .expect("status request should complete");
    assert_eq!(status_response.status(), StatusCode::OK);
    let status_body = status_response
        .text()
        .await
        .expect("status body should read");
    assert!(status_body.contains(r#""active":true"#));
    assert!(status_body.contains(r#""header_name":"x-debug""#));

    let stop_response = client
        .delete(format!("http://{admin_addr}/debug/session"))
        .send()
        .await
        .expect("stop request should complete");
    assert_eq!(stop_response.status(), StatusCode::NO_CONTENT);

    let status_after_stop = client
        .get(format!("http://{admin_addr}/debug/session"))
        .send()
        .await
        .expect("status request should complete");
    assert_eq!(status_after_stop.status(), StatusCode::OK);
    let status_after_stop_body = status_after_stop
        .text()
        .await
        .expect("status body should read");
    assert!(status_after_stop_body.contains(r#""active":false"#));

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn tiny_language_can_enforce_simple_rate_limit() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = r#"
        use http;
        use rate_limit;

        if rate_limit::allow(http::request::get_header("x-client-id"), 2, 60) {
            http::response::set_header("x-vm", "allowed");
            http::response::set_body("ok");
        } else {
            http::response::set_header("x-vm", "rate-limited");
            http::response::set_body("blocked");
        }
    "#;
    let compiled = compile_source(source).expect("source should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    for _ in 0..2 {
        let response = client
            .get(format!("http://{data_addr}/"))
            .header("x-client-id", "abc")
            .send()
            .await
            .expect("request should complete");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("x-vm")
                .and_then(|value| value.to_str().ok()),
            Some("allowed")
        );
        assert_eq!(response.text().await.expect("body should read"), "ok");
    }

    let blocked = client
        .get(format!("http://{data_addr}/"))
        .header("x-client-id", "abc")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(blocked.status(), StatusCode::OK);
    assert_eq!(
        blocked
            .headers()
            .get("x-vm")
            .and_then(|value| value.to_str().ok()),
        Some("rate-limited")
    );
    assert_eq!(blocked.text().await.expect("body should read"), "blocked");

    let other_key = client
        .get(format!("http://{data_addr}/"))
        .header("x-client-id", "xyz")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(other_key.status(), StatusCode::OK);
    assert_eq!(
        other_key
            .headers()
            .get("x-vm")
            .and_then(|value| value.to_str().ok()),
        Some("allowed")
    );

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn http_prefixed_host_abi_can_rewrite_request_and_short_circuit() {
    let upstream_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let method = request.method().clone();
        let path = request
            .uri()
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/");
        let added = request
            .headers()
            .get("x-added")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        Response::new(Body::from(format!("{method}|{path}|{added}")))
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = format!(
        r#"
        use http;
        use rate_limit;

        let client_id = http::request::get_header("x-client-id");
        if rate_limit::allow(client_id, 1, 60) {{
            http::upstream::request::set_path("/rewritten");
            http::upstream::request::set_query("from=vm");
            http::upstream::request::set_header("x-added", "yes");
            http::upstream::request::set_target("{upstream_addr}");
        }} else {{
            http::response::set_status(429);
            http::response::set_body("blocked");
        }}
    "#
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let first = client
        .get(format!("http://{data_addr}/anything?x=1"))
        .header("x-client-id", "abc")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(
        first.text().await.expect("body should read"),
        "GET|/rewritten?from=vm|yes"
    );

    let second = client
        .get(format!("http://{data_addr}/anything?x=1"))
        .header("x-client-id", "abc")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(second.text().await.expect("body should read"), "blocked");

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn http_request_body_can_be_rewritten_before_proxying() {
    let upstream_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let (parts, body) = request.into_parts();
        let body = to_bytes(body, usize::MAX)
            .await
            .expect("body should be readable");
        let path = parts
            .uri
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/");
        Response::new(Body::from(format!(
            "{}|{}",
            path,
            String::from_utf8_lossy(&body)
        )))
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = format!(
        r#"
        use http;

        http::upstream::request::set_body("rewritten-body");
        http::upstream::request::set_target("{upstream_addr}");
    "#
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/payload"))
        .body("original-body")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.text().await.expect("body should read"),
        "/payload|rewritten-body"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn http_request_body_chunk_api_reads_in_chunks() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = r#"
        use http;

        let first = http::request::body::next_chunk(4);
        let second = http::request::body::next_chunk(4);
        let rest = http::request::body::next_chunk(64);
        let done = http::request::body::eof();
        if done {
            http::response::set_body(first + second + rest);
        } else {
            http::response::set_body("body-not-finished");
        }
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/chunked"))
        .body("abcdefghij")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.text().await.expect("body should read"),
        "abcdefghij"
    );

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn sample_proxy_program_streams_or_buffers_upstream_body() {
    let (upstream_addr, upstream_handle) = spawn_chunked_upstream(vec!["ab", "cd", "ef"]).await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("sample_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let upstream_target = format!("http://{upstream_addr}/sample");
    let streaming = client
        .get(format!("http://{data_addr}/proxy"))
        .header("x-upstream-target", &upstream_target)
        .header("Streaming", "1")
        .send()
        .await
        .expect("streaming request should complete");
    assert_eq!(streaming.status(), StatusCode::OK);
    assert_eq!(
        streaming
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/plain")
    );
    assert_eq!(
        streaming
            .headers()
            .get("x-stream")
            .and_then(|value| value.to_str().ok()),
        None
    );
    assert_eq!(
        streaming.text().await.expect("streaming body should read"),
        "abAcdAefA"
    );

    let buffered = client
        .get(format!("http://{data_addr}/proxy"))
        .header("x-upstream-target", &upstream_target)
        .send()
        .await
        .expect("buffered request should complete");
    assert_eq!(buffered.status(), StatusCode::OK);
    assert_eq!(
        buffered
            .headers()
            .get("x-stream")
            .and_then(|value| value.to_str().ok()),
        Some("false")
    );
    assert_eq!(
        buffered.text().await.expect("buffered body should read"),
        "abcdef"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn sample_transport_proxy_program_streams_plain_http_body() {
    let (upstream_addr, upstream_handle) = spawn_chunked_upstream(vec!["ab", "cd", "ef"]).await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("sample_transport_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let upstream_target = format!("http://{upstream_addr}/sample");
    let response = client
        .get(format!("http://{data_addr}/proxy"))
        .header("x-upstream-target", &upstream_target)
        .header("Streaming", "1")
        .send()
        .await
        .expect("streaming request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/plain")
    );
    assert_eq!(
        response
            .headers()
            .get("x-peer-name")
            .and_then(|value| value.to_str().ok()),
        None
    );
    assert_eq!(
        response
            .headers()
            .get("x-upload-pipe")
            .and_then(|value| value.to_str().ok()),
        Some("eof")
    );
    assert_eq!(
        response.text().await.expect("response body should read"),
        "abAcdAefA"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn sample_transport_proxy_program_handles_https_tls_session() {
    let (upstream_addr, upstream_handle) = spawn_https_echo_upstream().await;
    let mut state = SharedState::new(1024 * 1024);
    state.client = reqwest::Client::builder()
        .tls_info(true)
        .danger_accept_invalid_certs(true)
        .build()
        .expect("tls test client should build");
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy_with_state(state).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("sample_transport_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let upstream_target = format!("https://localhost:{}/echo", upstream_addr.port());
    let response = client
        .post(format!("http://{data_addr}/proxy"))
        .header("x-upstream-target", &upstream_target)
        .body("secure-payload")
        .send()
        .await
        .expect("https request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-stream")
            .and_then(|value| value.to_str().ok()),
        Some("false")
    );
    assert_eq!(
        response
            .headers()
            .get("x-peer-name")
            .and_then(|value| value.to_str().ok()),
        Some("localhost")
    );
    assert_eq!(
        response
            .headers()
            .get("x-upload-pipe")
            .and_then(|value| value.to_str().ok()),
        Some("eof")
    );
    assert_eq!(
        response
            .headers()
            .get("x-upstream-alpn")
            .and_then(|value| value.to_str().ok()),
        Some("http/1.1")
    );
    assert_eq!(
        response.text().await.expect("response body should read"),
        "secure-payload"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn sample_tunnel_proxy_program_tunnels_plain_http_body() {
    let upstream_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let body = to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        Response::new(Body::from(format!(
            "tunnel:{}",
            String::from_utf8_lossy(&body)
        )))
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("sample_tunnel_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/tunnel"))
        .header("x-upstream-target", format!("http://{upstream_addr}/echo"))
        .body("abcdefghij")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-proxy-status")
            .and_then(|value| value.to_str().ok()),
        Some("closed")
    );
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/plain")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "tunnel:abcdefghij"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn sample_tunnel_proxy_program_tunnels_https_body_via_tls_plaintext_stream() {
    let (upstream_addr, upstream_handle) = spawn_https_echo_upstream().await;
    let mut state = SharedState::new(1024 * 1024);
    state.client = reqwest::Client::builder()
        .tls_info(true)
        .danger_accept_invalid_certs(true)
        .build()
        .expect("tls test client should build");
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy_with_state(state).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("sample_tunnel_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/tls-tunnel"))
        .header(
            "x-upstream-target",
            format!("https://localhost:{}/echo", upstream_addr.port()),
        )
        .body("secure-payload")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-proxy-status")
            .and_then(|value| value.to_str().ok()),
        Some("closed")
    );
    assert_eq!(
        response
            .headers()
            .get("x-peer-name")
            .and_then(|value| value.to_str().ok()),
        Some("localhost")
    );
    assert_eq!(
        response
            .headers()
            .get("x-upstream-alpn")
            .and_then(|value| value.to_str().ok()),
        Some("http/1.1")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "secure-payload"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn proxy_pipe_forwards_dynamic_exchange_response_via_proxy_stream_handle() {
    let upstream_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let body = to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        Response::new(Body::from(format!(
            "dynamic:{}",
            String::from_utf8_lossy(&body)
        )))
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use proxy;

        let exchange = http::exchange::new();
        http::exchange::set_target(exchange, "http://{upstream_addr}/dynamic");
        http::exchange::set_body(exchange, "payload");

        let response = proxy::stream::exchange(exchange);
        let downstream = proxy::stream::downstream();
        let status = proxy::pipe(response, downstream, 5);
        http::response::set_header("x-proxy-status", status);
        http::response::set_status(http::exchange::get_status(exchange));

        let content_type = http::exchange::get_header(exchange, "content-type");
        if content_type != "" {{
            http::response::set_header("content-type", content_type);
        }}
    "#
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/dynamic-proxy"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-proxy-status")
            .and_then(|value| value.to_str().ok()),
        Some("eof")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "dynamic:payload"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn tls_session_can_disable_verification_and_expose_handshake_phase() {
    let (upstream_addr, upstream_handle) = spawn_https_echo_upstream().await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tcp;
        use tls;

        let exchange = http::exchange::default_upstream();
        http::exchange::set_target(exchange, "https://localhost:{}/echo");
        http::exchange::set_body(exchange, "phase-body");

        let session = tls::session::from_socket(exchange);
        tls::session::set_verify(session, false);
        tls::session::handshake(session);

        http::response::set_header("x-phase", tls::session::get_phase(session));
        http::response::set_header("x-peer-cert", tls::session::get_peer_certificate(session));
        http::response::set_body(http::exchange::get_body(exchange));
    "#,
        upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/tls-phase"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-phase")
            .and_then(|value| value.to_str().ok()),
        Some("plaintext-ready")
    );
    assert!(
        response
            .headers()
            .get("x-peer-cert")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| !value.is_empty())
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "phase-body"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn tls_session_accepts_custom_trusted_certificate_bundle() {
    let materials = build_ca_signed_tls_materials();
    let (upstream_addr, upstream_handle) =
        spawn_ca_signed_https_echo_upstream(&materials, false).await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tls;

        let exchange = http::exchange::default_upstream();
        http::exchange::set_target(exchange, "https://localhost:{}/echo");
        http::exchange::set_body(exchange, "trusted-body");

        let session = tls::session::from_socket(exchange);
        tls::session::set_trusted_certificate(session, {});
        tls::session::handshake(session);

        http::response::set_header("x-phase", tls::session::get_phase(session));
        http::response::set_header("x-alpn", tls::session::get_alpn(session));
        http::response::set_body(http::exchange::get_body(exchange));
    "#,
        upstream_addr.port(),
        source_string_literal(&materials.ca_pem),
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/tls-ca"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-phase")
            .and_then(|value| value.to_str().ok()),
        Some("plaintext-ready")
    );
    assert_eq!(
        response
            .headers()
            .get("x-alpn")
            .and_then(|value| value.to_str().ok()),
        Some("http/1.1")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "trusted-body"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn tls_session_supports_client_certificate_authentication() {
    let materials = build_ca_signed_tls_materials();
    let (upstream_addr, upstream_handle) =
        spawn_ca_signed_https_echo_upstream(&materials, true).await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tls;

        let exchange = http::exchange::default_upstream();
        http::exchange::set_target(exchange, "https://localhost:{}/echo");
        http::exchange::set_body(exchange, "mtls-body");

        let session = tls::session::from_socket(exchange);
        tls::session::set_trusted_certificate(session, {});
        tls::session::set_certificate(session, {});
        tls::session::set_private_key(session, {});
        tls::session::handshake(session);

        http::response::set_header("x-phase", tls::session::get_phase(session));
        http::response::set_body(http::exchange::get_body(exchange));
    "#,
        upstream_addr.port(),
        source_string_literal(&materials.ca_pem),
        source_string_literal(&materials.client_cert_pem),
        source_string_literal(&materials.client_key_pem),
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/tls-mtls"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-phase")
            .and_then(|value| value.to_str().ok()),
        Some("plaintext-ready")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "mtls:1:mtls-body"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn tls_session_rejects_alpn_policy_mismatch() {
    let (upstream_addr, upstream_handle) = spawn_https_echo_upstream().await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tls;

        let exchange = http::exchange::default_upstream();
        http::exchange::set_target(exchange, "https://localhost:{}/echo");
        let session = tls::session::from_socket(exchange);
        tls::session::set_verify(session, false);
        tls::session::set_alpn(session, "h2");
        tls::session::handshake(session);
    "#,
        upstream_addr.port()
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/tls-alpn-mismatch"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn sample_subrequest_proxy_program_fans_out_across_default_and_dynamic_exchanges() {
    let plain_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let body = to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        Response::new(Body::from(format!(
            "plain:{}",
            String::from_utf8_lossy(&body)
        )))
    }));
    let (plain_addr, plain_handle) = spawn_server(plain_app).await;
    let (secure_addr, secure_handle) = spawn_https_echo_upstream().await;

    let mut state = SharedState::new(1024 * 1024);
    state.client = reqwest::Client::builder()
        .tls_info(true)
        .danger_accept_invalid_certs(true)
        .build()
        .expect("tls test client should build");
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy_with_state(state).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("sample_subrequest_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/fanout"))
        .header("x-primary-target", format!("http://{plain_addr}/plain"))
        .header(
            "x-secondary-target",
            format!("https://localhost:{}/secure", secure_addr.port()),
        )
        .send()
        .await
        .expect("fanout request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-secondary-peer")
            .and_then(|value| value.to_str().ok()),
        Some("localhost")
    );
    assert_eq!(
        response
            .headers()
            .get("x-secondary-alpn")
            .and_then(|value| value.to_str().ok()),
        Some("http/1.1")
    );
    assert_eq!(
        response.text().await.expect("response body should read"),
        "plain:alpha|beta"
    );

    plain_handle.abort();
    secure_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn sample_websocket_proxy_program_round_trips_text_frames() {
    let (upstream_addr, upstream_handle) = spawn_websocket_echo_upstream().await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("sample_websocket_proxy_program.rss");
    let compiled = compile_edge_source_file(&program_path).expect("sample should compile");

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/ws"))
        .header("x-ws-target", format!("ws://{upstream_addr}/echo"))
        .header("x-ws-message", "hello")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-ws-phase")
            .and_then(|value| value.to_str().ok()),
        Some("closed")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ws-protocol")
            .and_then(|value| value.to_str().ok()),
        Some("chat")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "echo:hello"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn websocket_connection_can_round_trip_binary_frames() {
    let (upstream_addr, upstream_handle) = spawn_websocket_echo_upstream().await;
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let payload = STANDARD.encode(b"bin-payload");
    let source = format!(
        r#"
        use http;
        use websocket;

        let connection = websocket::connection::default_upstream();
        websocket::connection::set_target(connection, "ws://{upstream_addr}/binary");
        websocket::connection::connect(connection);
        websocket::connection::send_binary_base64(connection, "{payload}");
        let echoed = websocket::connection::read_binary_base64(connection);
        http::response::set_header("x-phase", websocket::connection::get_phase(connection));
        websocket::connection::close(connection, 1000, "binary-complete");
        http::response::set_body(echoed);
    "#
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/ws-binary"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-phase")
            .and_then(|value| value.to_str().ok()),
        Some("open")
    );
    assert_eq!(response.text().await.expect("body should read"), payload);

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn downstream_websocket_handle_exposes_upgrade_candidate_phase() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = r#"
        use http;
        use websocket;

        let downstream = websocket::connection::downstream();
        if websocket::connection::is_present(downstream) {
            http::response::set_header("x-phase", websocket::connection::get_phase(downstream));
            http::response::set_body("upgrade");
        } else {
            http::response::set_body("plain");
        }
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/ws-downstream"))
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-phase")
            .and_then(|value| value.to_str().ok()),
        Some("upgrade-observed")
    );
    assert_eq!(response.text().await.expect("body should read"), "upgrade");

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn http_exchange_supports_multiple_dynamic_subrequests_in_one_vm_run() {
    let first_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let body = to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        Response::new(Body::from(format!(
            "first:{}",
            String::from_utf8_lossy(&body)
        )))
    }));
    let second_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let body = to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        Response::new(Body::from(format!(
            "second:{}",
            String::from_utf8_lossy(&body)
        )))
    }));
    let (first_addr, first_handle) = spawn_server(first_app).await;
    let (second_addr, second_handle) = spawn_server(second_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tcp;

        let first = http::exchange::new();
        let second = http::exchange::new();
        if first == second {{
            http::response::set_status(500);
            http::response::set_body("same-handle");
        }} else {{
            http::exchange::set_target(first, "http://{first_addr}/one");
            http::exchange::set_target(second, "http://{second_addr}/two");
            tcp::stream::write(first, "one");
            tcp::stream::write(second, "two");
            http::response::set_body(
                http::exchange::get_body(first) + "|" + http::exchange::get_body(second)
            );
        }}
    "#
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/subrequests"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.text().await.expect("body should read"),
        "first:one|second:two"
    );

    first_handle.abort();
    second_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn dynamic_exchange_rejects_write_after_response_has_started() {
    let upstream_app = Router::new().fallback(any(|_request: Request<Body>| async move {
        Response::new(Body::from("upstream"))
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tcp;

        let exchange = http::exchange::new();
        http::exchange::set_target(exchange, "{upstream_addr}");
        http::exchange::get_status(exchange);
        tcp::stream::write(exchange, "late");
    "#
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/late-dynamic-write"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn transport_downstream_tls_session_reflects_forwarded_https_metadata() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = r#"
        use http;
        use tcp;
        use tls;

        let sock = tcp::stream::downstream();
        let session = tls::session::from_socket(sock);
        if tls::session::is_present(session) {
            http::response::set_header("x-tls", "true");
            http::response::set_header("x-server-name", tls::session::get_server_name(session));
            http::response::set_header("x-alpn", tls::session::get_alpn(session));
        } else {
            http::response::set_header("x-tls", "false");
        }
        http::response::set_body("ok");
    "#;
    let compiled = compile_source(source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/tls"))
        .header("x-forwarded-proto", "https")
        .header("host", "app.example.test:443")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-tls")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        response
            .headers()
            .get("x-server-name")
            .and_then(|value| value.to_str().ok()),
        Some("app.example.test")
    );
    assert_eq!(
        response
            .headers()
            .get("x-alpn")
            .and_then(|value| value.to_str().ok()),
        Some("http/1.1")
    );
    assert_eq!(response.text().await.expect("body should read"), "ok");

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn transport_default_upstream_socket_accepts_multiple_writes_before_exchange() {
    let upstream_app = Router::new().fallback(any(|request: Request<Body>| async move {
        let body = to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        Response::new(Body::from(String::from_utf8_lossy(&body).into_owned()))
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tcp;

        http::upstream::request::set_target("{upstream_addr}");
        let downstream = tcp::stream::downstream();
        let upstream = tcp::stream::default_upstream();
        while !tcp::stream::eof(downstream) {{
            let chunk = tcp::stream::read(downstream, 3);
            if chunk != "" {{
                tcp::stream::write(upstream, chunk);
            }}
        }}
        http::response::set_body(http::upstream::response::get_body());
    "#
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .post(format!("http://{data_addr}/echo"))
        .body("abcdefghij")
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.text().await.expect("body should read"),
        "abcdefghij"
    );

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn transport_default_upstream_socket_rejects_write_after_response_has_started() {
    let upstream_app = Router::new().fallback(any(|_request: Request<Body>| async move {
        Response::new(Body::from("upstream"))
    }));
    let (upstream_addr, upstream_handle) = spawn_server(upstream_app).await;

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let source = format!(
        r#"
        use http;
        use tcp;

        http::upstream::request::set_target("{upstream_addr}");
        let upstream = tcp::stream::default_upstream();
        http::upstream::response::get_status();
        tcp::stream::write(upstream, "late");
    "#
    );
    let compiled = compile_source(&source).expect("source should compile");
    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/late-write"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    upstream_handle.abort();
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn debug_attached_request_does_not_block_non_debug_requests() {
    struct AbortProxyOnDrop {
        data: JoinHandle<()>,
        admin: JoinHandle<()>,
    }

    impl Drop for AbortProxyOnDrop {
        fn drop(&mut self) {
            self.data.abort();
            self.admin.abort();
        }
    }

    struct AbortTaskOnDrop<T> {
        handle: Option<JoinHandle<T>>,
    }

    impl<T> AbortTaskOnDrop<T> {
        fn new(handle: JoinHandle<T>) -> Self {
            Self {
                handle: Some(handle),
            }
        }

        async fn join(mut self) -> Result<T, tokio::task::JoinError> {
            self.handle.take().expect("join handle should exist").await
        }
    }

    impl<T> Drop for AbortTaskOnDrop<T> {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                handle.abort();
            }
        }
    }

    timeout(Duration::from_secs(10), async {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let _abort_proxy = AbortProxyOnDrop {
        data: data_handle,
        admin: admin_handle,
    };
    let client = reqwest::Client::new();

    let program = build_short_circuit_program("ok", None);
    let upload = upload_program(&client, admin_addr, &program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let debug_addr = reserve_tcp_addr();
    let start_response = client
        .put(format!("http://{admin_addr}/debug/session"))
        .header("content-type", "application/json")
        .body(format!(
            r#"{{"header_name":"x-debug","header_value":"on","tcp_addr":"{debug_addr}","stop_on_entry":true}}"#
        ))
        .send()
        .await
        .expect("start debug request should complete");
    assert_eq!(start_response.status(), StatusCode::CREATED);

    let debug_client = client.clone();
    let debug_request = AbortTaskOnDrop::new(tokio::spawn(async move {
        let response = debug_client
            .get(format!("http://{data_addr}/debug-target"))
            .header("x-debug", "on")
            .send()
            .await
            .expect("debug request should complete");
        let status = response.status();
        let body = response.text().await.expect("body should read");
        (status, body)
    }));

    tokio::time::sleep(Duration::from_millis(150)).await;

    let non_debug = timeout(
        Duration::from_secs(2),
        client.get(format!("http://{data_addr}/normal")).send(),
    )
    .await
    .expect("non-debug request timed out")
    .expect("non-debug request should complete");
    assert_eq!(non_debug.status(), StatusCode::OK);
    assert_eq!(non_debug.text().await.expect("body should read"), "ok");

    send_pdb_continue(debug_addr).await;

    let (status, body) = timeout(Duration::from_secs(2), debug_request.join())
        .await
        .expect("debug request timed out")
        .expect("debug task should join");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");

    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn uploaded_program_with_locals_executes_successfully() {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let source = r#"
        use http;

        let body = "from-local";
        http::response::set_body(body);
    "#;
    let compiled = compile_source(source).expect("source should compile");
    assert!(
        compiled.program.debug.is_some(),
        "compiled source should carry debug info"
    );

    let upload = upload_program(&client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let response = client
        .get(format!("http://{data_addr}/"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.text().await.expect("body should read"),
        "from-local"
    );

    data_handle.abort();
    admin_handle.abort();
}

#[derive(Clone)]
struct MockControlPlaneState {
    command: Arc<Mutex<Option<ControlPlaneCommand>>>,
    results: Arc<Mutex<Vec<EdgeCommandResult>>>,
    notified: Arc<Notify>,
}

impl MockControlPlaneState {
    fn new(command: Option<ControlPlaneCommand>) -> Self {
        Self {
            command: Arc::new(Mutex::new(command)),
            results: Arc::new(Mutex::new(Vec::new())),
            notified: Arc::new(Notify::new()),
        }
    }
}

async fn poll_rpc_handler(
    State(state): State<MockControlPlaneState>,
    Json(_request): Json<EdgePollRequest>,
) -> Json<EdgePollResponse> {
    let command = state.command.lock().expect("command lock").take();
    Json(EdgePollResponse {
        command,
        poll_interval_ms: 100,
    })
}

async fn result_rpc_handler(
    State(state): State<MockControlPlaneState>,
    Json(result): Json<EdgeCommandResult>,
) -> StatusCode {
    state.results.lock().expect("results lock").push(result);
    state.notified.notify_waiters();
    StatusCode::NO_CONTENT
}

#[tokio::test]
async fn active_control_plane_can_push_program_and_receive_result() {
    let program = build_short_circuit_program("from-active-control-plane", None);
    let program_bytes = encode_program(&program).expect("encode should succeed");
    let command = ControlPlaneCommand::ApplyProgram {
        command_id: "cmd-apply-1".to_string(),
        program_base64: STANDARD.encode(program_bytes),
    };

    let mock_state = MockControlPlaneState::new(Some(command));
    let control_app = Router::new()
        .route("/rpc/v1/edge/poll", post(poll_rpc_handler))
        .route("/rpc/v1/edge/result", post(result_rpc_handler))
        .with_state(mock_state.clone());
    let (rpc_addr, rpc_handle) = spawn_server(control_app).await;

    let state = SharedState::new(1024 * 1024);
    let (data_addr, data_handle) = spawn_server(build_http_proxy_app(state.clone())).await;
    let active_handle = spawn_active_control_plane_client(
        state.clone(),
        ActiveControlPlaneConfig {
            control_plane_url: format!("http://{rpc_addr}"),
            edge_id: "dp-test-1".to_string(),
            edge_name: "dp-test-friendly".to_string(),
            poll_interval_ms: 100,
            request_timeout_ms: 2_000,
        },
    );

    timeout(Duration::from_secs(3), mock_state.notified.notified())
        .await
        .expect("control plane should receive a result");

    let first_result = {
        let results = mock_state.results.lock().expect("results lock");
        results
            .first()
            .cloned()
            .expect("at least one result should exist")
    };
    assert_eq!(first_result.command_id, "cmd-apply-1");
    assert!(first_result.ok);
    match first_result.result {
        CommandResultPayload::ApplyProgram { report } => {
            assert!(report.applied);
            assert_eq!(report.message, None);
        }
        other => panic!("unexpected result payload: {other:?}"),
    }

    let client = reqwest::Client::new();
    let response = client
        .get(format!("http://{data_addr}/"))
        .send()
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.text().await.expect("body should read"),
        "from-active-control-plane"
    );

    active_handle.abort();
    data_handle.abort();
    rpc_handle.abort();
}

#[tokio::test]
async fn debug_session_is_removed_after_debugger_disconnects() {
    timeout(Duration::from_secs(10), async {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let program = build_short_circuit_program("ok", None);
    let upload = upload_program(&client, admin_addr, &program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let debug_addr = reserve_tcp_addr();
    let start_response = client
        .put(format!("http://{admin_addr}/debug/session"))
        .header("content-type", "application/json")
        .body(format!(
            r#"{{"header_name":"x-debug","header_value":"on","tcp_addr":"{debug_addr}","stop_on_entry":true}}"#
        ))
        .send()
        .await
        .expect("start debug request should complete");
    assert_eq!(start_response.status(), StatusCode::CREATED);

    let debug_client = client.clone();
    let debug_request = tokio::spawn(async move {
        let response = debug_client
            .get(format!("http://{data_addr}/debug-target"))
            .header("x-debug", "on")
            .send()
            .await
            .expect("debug request should complete");
        let status = response.status();
        let body = response.text().await.expect("body should read");
        (status, body)
    });

    tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(debug_addr).expect("debugger tcp should accept");
        let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
        let mut buffer = [0u8; 256];
        let _ = stream.read(&mut buffer);
    })
    .await
    .expect("debugger helper should not panic");

    let (status, body) = timeout(Duration::from_secs(2), debug_request)
        .await
        .expect("debug request timed out")
        .expect("debug task should join");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");

    let mut inactive = false;
    for _ in 0..20 {
        let response = client
            .get(format!("http://{admin_addr}/debug/session"))
            .send()
            .await
            .expect("status request should complete");
        let body = response.text().await.expect("status body should read");
        if body.contains(r#""active":false"#) {
            inactive = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(inactive, "debug session should be cleaned up after detach");

    data_handle.abort();
    admin_handle.abort();
    })
    .await
    .expect("test timed out");
}
