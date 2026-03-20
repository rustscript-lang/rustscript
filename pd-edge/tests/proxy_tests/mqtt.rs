use super::support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct ConnectPacket {
    client_id: String,
    username: Option<String>,
    password: Option<String>,
    keep_alive_secs: u16,
    clean_start: bool,
}

struct PublishPacket {
    topic: String,
    payload: Vec<u8>,
    qos: u8,
    retain: bool,
    packet_id: Option<u16>,
}

struct SubscribePacket {
    packet_id: u16,
    filter: String,
    qos: u8,
}

fn sample_mqtt_program_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("mqtt")
        .join("upstream")
        .join("sample_mqtt_publish_program.rss")
}

async fn upload_sample_mqtt_program(client: &reqwest::Client, admin_addr: SocketAddr) {
    let compiled =
        compile_edge_source_file(sample_mqtt_program_path().as_path()).expect("sample should compile");
    let upload = upload_program(client, admin_addr, &compiled.program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);
}

async fn read_mqtt_packet(stream: &mut tokio::net::TcpStream) -> std::io::Result<Vec<u8>> {
    let mut first = [0u8; 1];
    stream.read_exact(&mut first).await?;

    let mut encoded_len = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await?;
        encoded_len.push(byte[0]);
        if byte[0] & 0x80 == 0 {
            break;
        }
    }

    let (remaining_len, _) =
        decode_variable_int(&encoded_len).expect("remaining length should decode");
    let mut body = vec![0u8; remaining_len];
    stream.read_exact(&mut body).await?;

    let mut packet = vec![first[0]];
    packet.extend_from_slice(&encoded_len);
    packet.extend_from_slice(&body);
    Ok(packet)
}

fn decode_variable_int(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut multiplier = 1usize;
    let mut value = 0usize;
    for (index, byte) in bytes.iter().copied().enumerate().take(4) {
        value += usize::from(byte & 0x7f) * multiplier;
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
        multiplier *= 128;
    }
    None
}

fn encode_variable_int(mut value: usize) -> Vec<u8> {
    let mut encoded = Vec::new();
    loop {
        let mut byte = (value % 128) as u8;
        value /= 128;
        if value > 0 {
            byte |= 0x80;
        }
        encoded.push(byte);
        if value == 0 {
            return encoded;
        }
    }
}

fn packet_body(packet: &[u8]) -> &[u8] {
    let (_, encoded_len) =
        decode_variable_int(&packet[1..]).expect("remaining length should be complete");
    &packet[1 + encoded_len..]
}

fn encode_utf8_field(value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    let len = u16::try_from(bytes.len()).expect("mqtt field should fit into u16");
    let mut encoded = Vec::with_capacity(2 + bytes.len());
    encoded.extend_from_slice(&len.to_be_bytes());
    encoded.extend_from_slice(bytes);
    encoded
}

fn decode_utf8_field(bytes: &[u8], offset: &mut usize) -> String {
    let len = u16::from_be_bytes([bytes[*offset], bytes[*offset + 1]]) as usize;
    *offset += 2;
    let value = std::str::from_utf8(&bytes[*offset..*offset + len])
        .expect("mqtt field should be utf8")
        .to_string();
    *offset += len;
    value
}

fn decode_connect_packet(packet: &[u8]) -> ConnectPacket {
    assert_eq!(packet[0], 0x10);
    let body = packet_body(packet);
    let mut offset = 0usize;
    let protocol_name = decode_utf8_field(body, &mut offset);
    assert_eq!(protocol_name, "MQTT");
    assert_eq!(body[offset], 0x05);
    offset += 1;
    let flags = body[offset];
    offset += 1;
    let keep_alive_secs = u16::from_be_bytes([body[offset], body[offset + 1]]);
    offset += 2;
    let (properties_len, properties_len_size) =
        decode_variable_int(&body[offset..]).expect("connect properties length should decode");
    offset += properties_len_size + properties_len;
    let client_id = decode_utf8_field(body, &mut offset);
    let username = if flags & 0b1000_0000 != 0 {
        Some(decode_utf8_field(body, &mut offset))
    } else {
        None
    };
    let password = if flags & 0b0100_0000 != 0 {
        Some(decode_utf8_field(body, &mut offset))
    } else {
        None
    };

    ConnectPacket {
        client_id,
        username,
        password,
        keep_alive_secs,
        clean_start: flags & 0b0000_0010 != 0,
    }
}

fn decode_publish_packet(packet: &[u8]) -> PublishPacket {
    let header = packet[0];
    assert_eq!(header >> 4, 3);
    let body = packet_body(packet);
    let mut offset = 0usize;
    let topic = decode_utf8_field(body, &mut offset);
    let qos = (header >> 1) & 0x03;
    let packet_id = if qos > 0 {
        let packet_id = u16::from_be_bytes([body[offset], body[offset + 1]]);
        offset += 2;
        Some(packet_id)
    } else {
        None
    };
    let (properties_len, properties_len_size) =
        decode_variable_int(&body[offset..]).expect("publish properties length should decode");
    offset += properties_len_size + properties_len;

    PublishPacket {
        topic,
        payload: body[offset..].to_vec(),
        qos,
        retain: header & 0x01 != 0,
        packet_id,
    }
}

fn decode_subscribe_packet(packet: &[u8]) -> SubscribePacket {
    assert_eq!(packet[0], 0x82);
    let body = packet_body(packet);
    let mut offset = 0usize;
    let packet_id = u16::from_be_bytes([body[offset], body[offset + 1]]);
    offset += 2;
    let (properties_len, properties_len_size) =
        decode_variable_int(&body[offset..]).expect("subscribe properties length should decode");
    offset += properties_len_size + properties_len;
    let filter = decode_utf8_field(body, &mut offset);
    let qos = body[offset] & 0x03;

    SubscribePacket {
        packet_id,
        filter,
        qos,
    }
}

fn encode_publish_packet(
    topic: &str,
    payload: &[u8],
    qos: u8,
    retain: bool,
    packet_id: Option<u16>,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&encode_utf8_field(topic));
    if let Some(packet_id) = packet_id {
        body.extend_from_slice(&packet_id.to_be_bytes());
    }
    body.push(0);
    body.extend_from_slice(payload);

    let mut packet = vec![0b0011_0000 | ((qos & 0x03) << 1)];
    if retain {
        packet[0] |= 0b0000_0001;
    }
    packet.extend_from_slice(&encode_variable_int(body.len()));
    packet.extend_from_slice(&body);
    packet
}

async fn write_connack(stream: &mut tokio::net::TcpStream) -> std::io::Result<()> {
    stream.write_all(&[0x20, 0x03, 0x00, 0x00, 0x00]).await
}

async fn write_puback(stream: &mut tokio::net::TcpStream, packet_id: u16) -> std::io::Result<()> {
    stream
        .write_all(&[
            0x40,
            0x04,
            (packet_id >> 8) as u8,
            packet_id as u8,
            0x00,
            0x00,
        ])
        .await
}

async fn write_suback(
    stream: &mut tokio::net::TcpStream,
    packet_id: u16,
    granted_qos: u8,
) -> std::io::Result<()> {
    stream
        .write_all(&[
            0x90,
            0x04,
            (packet_id >> 8) as u8,
            packet_id as u8,
            0x00,
            granted_qos & 0x03,
        ])
        .await
}

#[tokio::test]
async fn sample_http_mqtt_program_publishes_over_http_proxy() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mqtt listener should bind");
    let broker_addr = listener.local_addr().expect("mqtt listener addr");
    let broker = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("broker should accept");

        let connect = decode_connect_packet(
            &read_mqtt_packet(&mut stream)
                .await
                .expect("connect packet should arrive"),
        );
        assert_eq!(connect.client_id, "proxy-publish-test");
        assert_eq!(connect.username.as_deref(), Some("user-a"));
        assert_eq!(connect.password.as_deref(), Some("pass-a"));
        assert_eq!(connect.keep_alive_secs, 30);
        assert!(connect.clean_start);
        write_connack(&mut stream)
            .await
            .expect("connack should write");

        let publish = decode_publish_packet(
            &read_mqtt_packet(&mut stream)
                .await
                .expect("publish packet should arrive"),
        );
        assert_eq!(publish.topic, "device/telemetry");
        assert_eq!(publish.payload, b"hello from http".to_vec());
        assert_eq!(publish.qos, 1);
        assert!(publish.retain);
        let packet_id = publish
            .packet_id
            .expect("qos1 publish should include packet id");
        write_puback(&mut stream, packet_id)
            .await
            .expect("puback should write");

        let disconnect = read_mqtt_packet(&mut stream)
            .await
            .expect("disconnect packet should arrive");
        assert_eq!(disconnect[0], 0xE0);
    });

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    upload_sample_mqtt_program(&client, admin_addr).await;

    let response = client
        .post(format!("http://{data_addr}/mqtt/publish"))
        .header("x-mqtt-host", "127.0.0.1")
        .header("x-mqtt-port", broker_addr.port().to_string())
        .header("x-mqtt-handle", "default")
        .header("x-mqtt-topic", "device/telemetry")
        .header("x-mqtt-qos", "1")
        .header("x-mqtt-retain", "true")
        .header("x-mqtt-client-id", "proxy-publish-test")
        .header("x-mqtt-username", "user-a")
        .header("x-mqtt-password", "pass-a")
        .body("hello from http")
        .send()
        .await
        .expect("request should complete");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-mqtt-handle")
            .and_then(|value| value.to_str().ok()),
        Some("default-upstream")
    );
    assert_eq!(
        response
            .headers()
            .get("x-mqtt-phase-before-configure")
            .and_then(|value| value.to_str().ok()),
        Some("inactive")
    );
    assert_eq!(
        response
            .headers()
            .get("x-mqtt-phase-before-connect")
            .and_then(|value| value.to_str().ok()),
        Some("configured")
    );
    assert_eq!(
        response
            .headers()
            .get("x-mqtt-phase-after-connect")
            .and_then(|value| value.to_str().ok()),
        Some("open")
    );
    assert_eq!(
        response
            .headers()
            .get("x-mqtt-publish-topic")
            .and_then(|value| value.to_str().ok()),
        Some("device/telemetry")
    );
    assert_eq!(
        response
            .headers()
            .get("x-mqtt-phase-after-disconnect")
            .and_then(|value| value.to_str().ok()),
        Some("closed")
    );
    assert_eq!(
        response.text().await.expect("body should read"),
        "mqtt publish completed"
    );

    broker.await.expect("broker should finish");
    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn sample_http_mqtt_program_subscribes_and_reads_event_over_http_proxy() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mqtt listener should bind");
    let broker_addr = listener.local_addr().expect("mqtt listener addr");
    let broker = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("broker should accept");

        let connect = decode_connect_packet(
            &read_mqtt_packet(&mut stream)
                .await
                .expect("connect packet should arrive"),
        );
        assert_eq!(connect.client_id, "pd-edge-http-mqtt-sample");
        assert_eq!(connect.keep_alive_secs, 30);
        assert!(connect.clean_start);
        write_connack(&mut stream)
            .await
            .expect("connack should write");

        let subscribe = decode_subscribe_packet(
            &read_mqtt_packet(&mut stream)
                .await
                .expect("subscribe packet should arrive"),
        );
        assert_eq!(subscribe.filter, "sensor/#");
        assert_eq!(subscribe.qos, 0);
        write_suback(&mut stream, subscribe.packet_id, 0)
            .await
            .expect("suback should write");
        stream
            .write_all(&encode_publish_packet(
                "sensor/temp",
                b"21.5",
                0,
                false,
                None,
            ))
            .await
            .expect("publish should write");

        let disconnect = read_mqtt_packet(&mut stream)
            .await
            .expect("disconnect packet should arrive");
        assert_eq!(disconnect[0], 0xE0);
    });

    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    upload_sample_mqtt_program(&client, admin_addr).await;

    let response = client
        .get(format!("http://{data_addr}/mqtt/subscribe"))
        .header("x-mqtt-host", "127.0.0.1")
        .header("x-mqtt-port", broker_addr.port().to_string())
        .header("x-mqtt-subscribe", "sensor/#")
        .send()
        .await
        .expect("request should complete");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-mqtt-handle")
            .and_then(|value| value.to_str().ok()),
        Some("new")
    );
    assert_eq!(
        response
            .headers()
            .get("x-mqtt-phase-before-configure")
            .and_then(|value| value.to_str().ok()),
        Some("inactive")
    );
    assert_eq!(
        response
            .headers()
            .get("x-mqtt-phase-before-connect")
            .and_then(|value| value.to_str().ok()),
        Some("configured")
    );
    assert_eq!(
        response
            .headers()
            .get("x-mqtt-phase-after-connect")
            .and_then(|value| value.to_str().ok()),
        Some("open")
    );
    assert_eq!(
        response
            .headers()
            .get("x-mqtt-subscribe")
            .and_then(|value| value.to_str().ok()),
        Some("sensor/#")
    );
    assert_eq!(
        response
            .headers()
            .get("x-mqtt-event-kind")
            .and_then(|value| value.to_str().ok()),
        Some("publish")
    );
    assert_eq!(
        response
            .headers()
            .get("x-mqtt-event-topic")
            .and_then(|value| value.to_str().ok()),
        Some("sensor/temp")
    );
    assert_eq!(
        response
            .headers()
            .get("x-mqtt-phase-after-disconnect")
            .and_then(|value| value.to_str().ok()),
        Some("closed")
    );
    assert_eq!(response.text().await.expect("body should read"), "21.5");

    broker.await.expect("broker should finish");
    data_handle.abort();
    admin_handle.abort();
}
