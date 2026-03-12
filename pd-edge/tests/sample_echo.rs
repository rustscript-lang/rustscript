#[cfg(feature = "http2")]
use edge::sample_echo::{SampleEchoServerConfig, spawn_sample_echo_server};
use edge::sample_echo::{spawn_connect_forward_proxy, spawn_tcp_echo_server};
#[cfg(feature = "http2")]
use http_body_util::{BodyExt, Full};
#[cfg(feature = "http2")]
use hyper::Request;
#[cfg(feature = "http2")]
use hyper_util::rt::{TokioExecutor, TokioIo};
#[cfg(all(feature = "tls", feature = "http2"))]
use reqwest::{StatusCode, Version};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::{Duration, timeout},
};

#[tokio::test]
async fn spawn_connect_forward_proxy_tunnels_bytes_after_connect() {
    let (upstream_addr, upstream_handle) =
        spawn_tcp_echo_server("127.0.0.1:0".parse().expect("valid addr"))
            .await
            .expect("tcp echo should start");
    let (proxy_addr, proxy_handle) =
        spawn_connect_forward_proxy("127.0.0.1:0".parse().expect("valid addr"))
            .await
            .expect("forward proxy should start");

    let mut client = timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(proxy_addr),
    )
    .await
    .expect("proxy connect timed out")
    .expect("proxy should accept connections");
    let connect_request =
        format!("CONNECT {upstream_addr} HTTP/1.1\r\nHost: {upstream_addr}\r\n\r\n");
    client
        .write_all(connect_request.as_bytes())
        .await
        .expect("connect request should write");

    let mut connect_response = [0u8; 256];
    let read = timeout(Duration::from_secs(2), client.read(&mut connect_response))
        .await
        .expect("connect response timed out")
        .expect("connect response should read");
    let connect_response = String::from_utf8_lossy(&connect_response[..read]);
    assert!(
        connect_response.starts_with("HTTP/1.1 200 Connection Established"),
        "unexpected CONNECT response: {connect_response}"
    );

    client
        .write_all(b"hello-through-proxy")
        .await
        .expect("payload should write");

    let mut echoed = [0u8; 64];
    let read = timeout(Duration::from_secs(2), client.read(&mut echoed))
        .await
        .expect("echo read timed out")
        .expect("echo should read");
    assert_eq!(&echoed[..read], b"hello-through-proxy");

    proxy_handle.abort();
    upstream_handle.abort();
}

#[cfg(feature = "http2")]
#[tokio::test]
async fn sample_echo_server_http_listener_accepts_cleartext_http2_prior_knowledge() {
    let server = spawn_sample_echo_server(zero_addr_sample_echo_config())
        .await
        .expect("sample echo server should start");
    let addr = server
        .addresses
        .http
        .expect("http listener should be present");

    let stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("http2 client should connect");
    let io = TokioIo::new(stream);
    let (mut sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake(io)
        .await
        .expect("http2 client handshake should succeed");
    let connection_handle = tokio::spawn(async move {
        connection
            .await
            .expect("http2 client connection should run");
    });

    let host = addr.to_string();
    let request = Request::builder()
        .method("POST")
        .uri(format!("http://{addr}/echo"))
        .version(hyper::Version::HTTP_2)
        .header("host", &host)
        .body(Full::new(axum::body::Bytes::from_static(b"h2c-body")))
        .expect("http2 request should build");
    let response = sender
        .send_request(request)
        .await
        .expect("http2 request should complete");
    assert_eq!(response.status(), hyper::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-echo-http-version")
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
    assert_eq!(
        response
            .headers()
            .get("x-echo-protocol")
            .and_then(|value| value.to_str().ok()),
        Some("http")
    );
    let body = response
        .into_body()
        .collect()
        .await
        .expect("http2 response body should collect")
        .to_bytes();
    assert_eq!(body.as_ref(), b"h2c-body");

    connection_handle.abort();
    drop(server);
}

#[cfg(all(feature = "tls", feature = "http2"))]
#[tokio::test]
async fn sample_echo_server_https_listener_negotiates_http2_over_tls() {
    let server = spawn_sample_echo_server(zero_addr_sample_echo_config())
        .await
        .expect("sample echo server should start");
    let addr = server
        .addresses
        .https
        .expect("https listener should be present");

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("https h2 client should build");
    let response = client
        .post(format!("https://localhost:{}/echo", addr.port()))
        .body("tls-h2-body")
        .send()
        .await
        .expect("https h2 request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.version(), Version::HTTP_2);
    assert_eq!(
        response
            .headers()
            .get("x-echo-http-version")
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
    assert_eq!(
        response
            .headers()
            .get("x-echo-protocol")
            .and_then(|value| value.to_str().ok()),
        Some("https")
    );
    assert_eq!(
        response.text().await.expect("https h2 body should read"),
        "tls-h2-body"
    );

    drop(server);
}

#[cfg(feature = "http2")]
fn zero_addr_sample_echo_config() -> SampleEchoServerConfig {
    let any = "127.0.0.1:0".parse().expect("valid wildcard addr");
    SampleEchoServerConfig {
        tcp_addr: any,
        udp_addr: any,
        tls_addr: any,
        http_addr: any,
        https_addr: any,
        websocket_addr: any,
        websocket_tls_addr: any,
        webrtc_addr: any,
        forward_proxy_addr: any,
    }
}
