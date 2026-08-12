use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::Message;
use vm::{
    HostFunctionRegistry, HttpConfig, HttpHostExt, Value, Vm, VmStatus, compile_source,
    default_host_callables,
};

fn websocket_config(port: u16) -> HttpConfig {
    HttpConfig {
        allowed_schemes: vec!["ws".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        connect_timeout: Duration::from_secs(1),
        request_timeout: Duration::from_secs(2),
        stream_idle_timeout: Duration::from_millis(500),
        websocket_close_timeout: Duration::from_millis(500),
        ..HttpConfig::default()
    }
}

fn map_field<'a>(value: &'a Value, key: &str) -> &'a Value {
    let Value::Map(map) = value else {
        panic!("expected map, got {value:?}");
    };
    map.get(&Value::string(key))
        .unwrap_or_else(|| panic!("map missing field {key}"))
}

async fn run_websocket(source: &str, config: HttpConfig) -> Result<Vm, vm::VmError> {
    let compiled = compile_source(source).expect("WebSocket source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.configure_http(config)?;
    HostFunctionRegistry::new().bind_vm_cached(&mut vm)?;
    let mut status = vm.run()?;
    loop {
        match status {
            VmStatus::Halted => return Ok(vm),
            VmStatus::Yielded => status = vm.resume()?,
            VmStatus::Waiting(_) => {
                vm.await_waiting_host_op().await?;
                status = vm.resume()?;
            }
        }
    }
}

#[test]
fn websocket_callable_is_feature_gated_with_typed_callback_metadata() {
    let callable = default_host_callables()
        .iter()
        .find(|callable| callable.name == "http::client::websocket")
        .expect("http-client must publish the WebSocket callable");
    assert_eq!(callable.signature.params.len(), 2);
    assert_eq!(callable.signature.params[0].ty.display_label(), "map");
    assert_eq!(
        callable.signature.params[1].ty.display_label(),
        "fn(map) -> map"
    );
    assert_eq!(callable.signature.return_type, "map");

    compile_source(
        r#"
        use http;
        fn on_socket(item: map) -> map { { action: "stop" } }
        http::client::websocket({ url: "ws://example.test/socket" }, on_socket);
        "#,
    )
    .expect("typed WebSocket callback should compile");
}

#[tokio::test(flavor = "current_thread")]
async fn handshake_preserves_host_headers_and_selected_protocol_then_stop_drops_socket() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let address = listener.local_addr().expect("listener address");
    let observed = Arc::new(Mutex::new(None));
    let observed_request = Arc::clone(&observed);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("client should connect");
        #[allow(clippy::result_large_err)]
        let callback = move |request: &Request, mut response: Response| {
            *observed_request.lock().expect("request lock") = Some((
                request.headers()["host"].to_str().unwrap().to_string(),
                request.headers()["x-test"].to_str().unwrap().to_string(),
                request.headers()["sec-websocket-protocol"]
                    .to_str()
                    .unwrap()
                    .to_string(),
            ));
            response.headers_mut().insert(
                "sec-websocket-protocol",
                "chat.v2".parse().expect("valid protocol"),
            );
            Ok(response)
        };
        let mut socket = tokio_tungstenite::accept_hdr_async(stream, callback)
            .await
            .expect("handshake should succeed");
        let terminal = tokio::time::timeout(Duration::from_millis(500), socket.next())
            .await
            .expect("client should drop promptly");
        assert!(terminal.is_none() || terminal.is_some_and(|item| item.is_err()));
    });
    let source = format!(
        r#"
        use http;
        use bytes;
        fn callback(item: map) -> map {{
            assert(item["kind"] == "open");
            assert(item["status"] == 101);
            assert(item["protocol"] == "chat.v2");
            {{ action: "stop" }}
        }}
        http::client::websocket({{
            url: "ws://{address}/socket?q=1",
            headers: {{ "x-test": "present" }},
            protocols: ["chat.v1", "chat.v2"]
        }}, callback);
        "#
    );
    let vm = run_websocket(&source, websocket_config(address.port()))
        .await
        .expect("WebSocket should stop after open");
    server.await.expect("server should finish");
    assert_eq!(
        map_field(&vm.stack()[0], "outcome"),
        &Value::string("stopped")
    );
    assert_eq!(map_field(&vm.stack()[0], "items"), &Value::Int(1));
    assert_eq!(
        observed.lock().expect("request lock").as_ref().unwrap(),
        &(
            format!("127.0.0.1:{}", address.port()),
            "present".to_string(),
            "chat.v1, chat.v2".to_string()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn callback_actions_are_applied_before_next_message_and_close_handshake_completes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("client should connect");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("handshake should succeed");
        assert_eq!(
            socket
                .next()
                .await
                .expect("text message")
                .expect("valid message"),
            Message::text("hello")
        );
        socket
            .send(Message::binary(vec![1, 2, 3]))
            .await
            .expect("binary should send");
        assert_eq!(
            socket.next().await.expect("pong").expect("valid pong"),
            Message::Ping(vec![9].into())
        );
        socket
            .send(Message::Pong(vec![9].into()))
            .await
            .expect("pong should send");
        socket
            .send(Message::text("done"))
            .await
            .expect("text should send");
        let close = socket
            .next()
            .await
            .expect("close message")
            .expect("valid close");
        let Message::Close(Some(frame)) = close else {
            panic!("expected close frame, got {close:?}");
        };
        assert_eq!(u16::from(frame.code), 1000);
        assert_eq!(frame.reason, "complete");
        socket
            .flush()
            .await
            .expect("close acknowledgment should flush");
    });
    let source = format!(
        r#"
        use http;
        use bytes;
        fn callback(item: map) -> map {{
            let action = if item["kind"] == "open" => {{
                {{ action: "send_text", text: "hello", data: bytes::from_array_u8([]), code: 1000, reason: "" }}
            }} else if item["kind"] == "binary" => {{
                assert(item["data"] == bytes::from_array_u8([1, 2, 3]));
                {{ action: "ping", text: "", data: bytes::from_array_u8([9]), code: 1000, reason: "" }}
            }} else if item["kind"] == "pong" => {{
                assert(item["data"] == bytes::from_array_u8([9]));
                {{ action: "continue", text: "", data: bytes::from_array_u8([]), code: 1000, reason: "" }}
            }} else if item["kind"] == "text" => {{
                assert(item["text"] == "done");
                {{ action: "close", text: "", data: bytes::from_array_u8([]), code: 1000, reason: "complete" }}
            }} else => {{
                {{ action: "continue", text: "", data: bytes::from_array_u8([]), code: 1000, reason: "" }}
            }};
            action
        }}
        http::client::websocket({{ url: "ws://{address}/" }}, callback);
        "#
    );
    let vm = run_websocket(&source, websocket_config(address.port()))
        .await
        .expect("WebSocket close handshake should complete");
    server.await.expect("server should finish");
    assert_eq!(
        map_field(&vm.stack()[0], "outcome"),
        &Value::string("closed")
    );
    assert_eq!(map_field(&vm.stack()[0], "items"), &Value::Int(5));
    assert_eq!(map_field(&vm.stack()[0], "bytes_received"), &Value::Int(7));
    assert_eq!(map_field(&vm.stack()[0], "bytes_sent"), &Value::Int(5));
}

#[tokio::test(flavor = "current_thread")]
async fn control_frames_do_not_consume_an_exhausted_application_byte_budget() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("client should connect");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("handshake should succeed");
        socket
            .send(Message::text("full"))
            .await
            .expect("application payload should send");
        assert_eq!(
            socket.next().await.expect("ping").expect("valid ping"),
            Message::Ping(vec![7, 8, 9].into())
        );
        socket
            .send(Message::Pong(vec![7, 8, 9].into()))
            .await
            .expect("pong should send");
        socket
            .send(Message::Close(Some(
                tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: 1000.into(),
                    reason: "done".into(),
                },
            )))
            .await
            .expect("close should send");
        assert!(matches!(
            socket.next().await.expect("close acknowledgment"),
            Ok(Message::Close(_))
        ));
    });
    let source = format!(
        r#"
        use http;
        use bytes;
        fn callback(item: map) -> map {{
            if item["kind"] == "text" => {{
                {{ action: "ping", data: bytes::from_array_u8([7, 8, 9]) }}
            }} else => {{
                {{ action: "continue" }}
            }}
        }}
        http::client::websocket({{ url: "ws://{address}/" }}, callback);
        "#
    );
    let mut config = websocket_config(address.port());
    config.max_stream_total_bytes = 4;
    let vm = run_websocket(&source, config)
        .await
        .expect("control traffic after the exact application limit should complete");
    server.await.expect("server should finish");
    assert_eq!(
        map_field(&vm.stack()[0], "outcome"),
        &Value::string("closed")
    );
    assert_eq!(map_field(&vm.stack()[0], "bytes_received"), &Value::Int(4));
    assert_eq!(map_field(&vm.stack()[0], "bytes_sent"), &Value::Int(0));
}

#[tokio::test(flavor = "current_thread")]
async fn application_payload_one_byte_over_the_total_budget_is_rejected() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("client should connect");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("handshake should succeed");
        socket
            .send(Message::text("limit"))
            .await
            .expect("application payload should send");
        let terminal = tokio::time::timeout(Duration::from_millis(500), socket.next())
            .await
            .expect("client should terminate promptly");
        assert!(terminal.is_none() || terminal.is_some_and(|item| item.is_err()));
    });
    let source = format!(
        r#"
        use http;
        fn callback(item: map) -> map {{ {{ action: "continue" }} }}
        http::client::websocket({{ url: "ws://{address}/" }}, callback);
        "#
    );
    let mut config = websocket_config(address.port());
    config.max_stream_total_bytes = 4;
    let error = match run_websocket(&source, config).await {
        Ok(_) => panic!("application payload above the total budget must fail"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("total byte limit"));
    server.await.expect("server should finish");
}

#[tokio::test(flavor = "current_thread")]
async fn fragmented_text_is_reassembled_before_callback_delivery() {
    use tokio_tungstenite::tungstenite::protocol::frame::Frame;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("client should connect");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("handshake should succeed");
        socket
            .send(Message::Frame(Frame::message(
                "hel",
                OpCode::Data(Data::Text),
                false,
            )))
            .await
            .expect("first fragment should send");
        socket
            .send(Message::Frame(Frame::message(
                "lo",
                OpCode::Data(Data::Continue),
                true,
            )))
            .await
            .expect("last fragment should send");
        let terminal = socket.next().await.expect("client should close");
        assert!(matches!(terminal, Ok(Message::Close(_))));
        socket.flush().await.expect("close response should flush");
    });
    let source = format!(
        r#"
        use http;
        fn callback(item: map) -> map {{
            if item["kind"] == "text" {{ assert(item["text"] == "hello"); }}
            let action = if item["kind"] == "text" => {{
                {{ action: "close", code: 1000, reason: "done" }}
            }} else => {{
                {{ action: "continue", code: 1000, reason: "" }}
            }};
            action
        }}
        http::client::websocket({{ url: "ws://{address}/" }}, callback);
        "#
    );
    let vm = run_websocket(&source, websocket_config(address.port()))
        .await
        .expect("fragmented text should complete");
    server.await.expect("server should finish");
    assert_eq!(map_field(&vm.stack()[0], "items"), &Value::Int(3));
}

#[tokio::test(flavor = "current_thread")]
async fn peer_close_is_delivered_once_and_continue_acknowledges_it() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("client should connect");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("handshake should succeed");
        socket
            .send(Message::Close(Some(
                tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: 1001.into(),
                    reason: "away".into(),
                },
            )))
            .await
            .expect("peer close should send");
        let terminal = socket
            .next()
            .await
            .expect("client should acknowledge close");
        assert!(matches!(terminal, Ok(Message::Close(_))));
    });
    let source = format!(
        r#"
        use http;
        fn callback(item: map) -> map {{
            if item["kind"] == "close" {{
                assert(item["code"] == 1001);
                assert(item["reason"] == "away");
            }}
            {{ action: "continue" }}
        }}
        http::client::websocket({{ url: "ws://{address}/" }}, callback);
        "#
    );
    let vm = run_websocket(&source, websocket_config(address.port()))
        .await
        .expect("peer close should complete");
    server.await.expect("server should finish");
    assert_eq!(
        map_field(&vm.stack()[0], "outcome"),
        &Value::string("closed")
    );
    assert_eq!(map_field(&vm.stack()[0], "items"), &Value::Int(2));
}

#[tokio::test(flavor = "current_thread")]
async fn peer_close_callback_close_validates_and_flushes_the_queued_acknowledgment() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("client should connect");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("handshake should succeed");
        socket
            .send(Message::Close(Some(
                tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: 1001.into(),
                    reason: "away".into(),
                },
            )))
            .await
            .expect("peer close should send");
        let acknowledgment = socket
            .next()
            .await
            .expect("client should acknowledge close")
            .expect("close acknowledgment should be valid");
        let Message::Close(Some(frame)) = acknowledgment else {
            panic!("expected close acknowledgment, got {acknowledgment:?}");
        };
        assert_eq!(u16::from(frame.code), 1001);
        assert_eq!(frame.reason, "away");
    });
    let source = format!(
        r#"
        use http;
        fn callback(item: map) -> map {{
            if item["kind"] == "close" => {{
                assert(item["code"] == 1001);
                assert(item["reason"] == "away");
                {{ action: "close", code: 1000, reason: "validated" }}
            }} else => {{
                {{ action: "continue", code: 1000, reason: "" }}
            }}
        }}
        http::client::websocket({{ url: "ws://{address}/" }}, callback);
        "#
    );
    let vm = run_websocket(&source, websocket_config(address.port()))
        .await
        .expect("peer close callback close should complete without a host write error");
    server.await.expect("server should finish");
    assert_eq!(
        map_field(&vm.stack()[0], "outcome"),
        &Value::string("closed")
    );
    assert_eq!(map_field(&vm.stack()[0], "items"), &Value::Int(2));
    assert_eq!(map_field(&vm.stack()[0], "bytes_received"), &Value::Int(0));
    assert_eq!(map_field(&vm.stack()[0], "bytes_sent"), &Value::Int(0));
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_action_is_reported_and_connection_is_dropped() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("client should connect");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("handshake should succeed");
        let terminal = tokio::time::timeout(Duration::from_millis(500), socket.next())
            .await
            .expect("client should drop promptly");
        assert!(terminal.is_none() || terminal.is_some_and(|item| item.is_err()));
    });
    let source = format!(
        r#"
        use http;
        use bytes;
        fn callback(item: map) -> map {{ {{ action: "send_binary", data: "wrong" }} }}
        http::client::websocket({{ url: "ws://{address}/" }}, callback);
        "#
    );
    let error = match run_websocket(&source, websocket_config(address.port())).await {
        Ok(_) => panic!("invalid action payload must fail"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("WebSocket send_binary action"));
    server.await.expect("server should finish");
}

#[tokio::test(flavor = "current_thread")]
async fn ping_callback_rejects_application_data_action() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("client should connect");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("handshake should succeed");
        socket
            .send(Message::Ping(vec![1].into()))
            .await
            .expect("ping should send");
        let terminal = tokio::time::timeout(Duration::from_millis(500), socket.next())
            .await
            .expect("client should terminate promptly");
        assert!(terminal.is_none() || terminal.is_some_and(|item| item.is_err()));
    });
    let source = format!(
        r#"
        use http;
        fn callback(item: map) -> map {{
            let action = if item["kind"] == "ping" => {{
                {{ action: "send_text", text: "forbidden" }}
            }} else => {{
                {{ action: "continue" }}
            }};
            action
        }}
        http::client::websocket({{ url: "ws://{address}/" }}, callback);
        "#
    );
    let error = match run_websocket(&source, websocket_config(address.port())).await {
        Ok(_) => panic!("ping application data send must fail"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("invalid for the current item"));
    server.await.expect("server should finish");
}

#[tokio::test(flavor = "current_thread")]
async fn idle_timeout_and_abnormal_eof_are_host_errors() {
    for abnormal_eof in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("client should connect");
            let socket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("handshake should succeed");
            if abnormal_eof {
                drop(socket);
            } else {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });
        let source = format!(
            r#"
            use http;
            fn callback(item: map) -> map {{ {{ action: "continue" }} }}
            http::client::websocket({{ url: "ws://{address}/" }}, callback);
            "#
        );
        let mut config = websocket_config(address.port());
        config.stream_idle_timeout = Duration::from_millis(30);
        let error = match run_websocket(&source, config).await {
            Ok(_) => panic!("terminal transport condition must fail"),
            Err(error) => error,
        };
        if abnormal_eof {
            assert!(
                error.to_string().contains("without a close handshake")
                    || error.to_string().contains("receive failed"),
                "unexpected error: {error}"
            );
        } else {
            assert!(error.to_string().contains("idle timeout"));
        }
        server.await.expect("server should finish");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn unoffered_selected_protocol_is_rejected() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("client should connect");
        #[allow(clippy::result_large_err)]
        let callback = |_request: &Request, mut response: Response| {
            response.headers_mut().insert(
                "sec-websocket-protocol",
                "unoffered".parse().expect("valid protocol"),
            );
            Ok(response)
        };
        let _ = tokio_tungstenite::accept_hdr_async(stream, callback).await;
    });
    let source = format!(
        r#"
        use http;
        fn callback(item: map) -> map {{ {{ action: "stop" }} }}
        http::client::websocket({{
            url: "ws://{address}/",
            protocols: ["offered"]
        }}, callback);
        "#
    );
    let error = match run_websocket(&source, websocket_config(address.port())).await {
        Ok(_) => panic!("unoffered protocol must fail"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("invalid subprotocol")
            || error.to_string().contains("unoffered protocol"),
        "unexpected error: {error}"
    );
    server.await.expect("server should finish");
}

#[tokio::test(flavor = "current_thread")]
async fn local_close_uses_close_timeout_instead_of_idle_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("client should connect");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("handshake should succeed");
        let close = socket.next().await.expect("client should send close");
        assert!(matches!(close, Ok(Message::Close(_))));
        tokio::time::sleep(Duration::from_millis(150)).await;
    });
    let source = format!(
        r#"
        use http;
        fn callback(item: map) -> map {{
            {{ action: "close", code: 1000, reason: "done" }}
        }}
        http::client::websocket({{ url: "ws://{address}/" }}, callback);
        "#
    );
    let mut config = websocket_config(address.port());
    config.stream_idle_timeout = Duration::from_millis(10);
    config.websocket_close_timeout = Duration::from_millis(40);
    let error = match run_websocket(&source, config).await {
        Ok(_) => panic!("missing close acknowledgment must time out"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("close handshake timed out"));
    server.await.expect("server should finish");
}

#[test]
fn request_validation_rejects_managed_headers_before_connection() {
    let cases = [
        "host",
        "upgrade",
        "connection",
        "sec-websocket-key",
        "sec-websocket-protocol",
    ];
    for header in cases {
        let source = format!(
            r#"
            use http;
            fn callback(item: map) -> map {{ {{ action: "stop" }} }}
            http::client::websocket({{
                url: "ws://127.0.0.1:9/",
                headers: {{ "{header}": "forbidden" }}
            }}, callback);
            "#
        );
        let compiled = compile_source(&source).expect("source should compile");
        let mut vm = Vm::new(compiled.program);
        vm.configure_http(websocket_config(9)).unwrap();
        HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();
        let error = vm
            .run()
            .expect_err("managed header must fail before connect");
        assert!(error.to_string().contains("managed by the client"));
    }
}

#[test]
fn timeout_ms_is_rejected_before_any_connection_attempt() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener should bind");
    listener
        .set_nonblocking(true)
        .expect("listener should become nonblocking");
    let address = listener.local_addr().expect("listener address");
    let source = format!(
        r#"
        use http;
        fn callback(item: map) -> map {{ {{ action: "stop" }} }}
        http::client::websocket({{
            url: "ws://{address}/",
            timeout_ms: 25
        }}, callback);
        "#
    );
    let compiled = compile_source(&source).expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.configure_http(websocket_config(address.port())).unwrap();
    HostFunctionRegistry::new().bind_vm_cached(&mut vm).unwrap();
    let error = vm
        .run()
        .expect_err("timeout_ms must fail during synchronous admission");
    assert!(
        error
            .to_string()
            .contains("no externally bounded streaming deadline is available"),
        "unexpected error: {error}"
    );
    let accept_error = listener
        .accept()
        .expect_err("admission failure must prevent a connection attempt");
    assert_eq!(accept_error.kind(), std::io::ErrorKind::WouldBlock);
}
