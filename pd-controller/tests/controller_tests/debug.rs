use super::support::*;

#[tokio::test]
async fn start_debug_enqueue_validates_recording_mode_and_command_shape() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let missing_request_path = client
        .post(format!(
            "http://{addr}/v1/edges/dp-debug-validation/commands/start-debug"
        ))
        .json(&serde_json::json!({
            "mode": "recording"
        }))
        .send()
        .await
        .expect("start-debug request should complete");
    assert_eq!(
        missing_request_path.status(),
        reqwest::StatusCode::BAD_REQUEST
    );
    let missing_request_path_json = missing_request_path
        .json::<serde_json::Value>()
        .await
        .expect("error payload should decode");
    assert!(
        missing_request_path_json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("requires request_path"),
        "unexpected error payload: {missing_request_path_json}"
    );

    let zero_record_count = client
        .post(format!(
            "http://{addr}/v1/edges/dp-debug-validation/commands/start-debug"
        ))
        .json(&serde_json::json!({
            "mode": "recording",
            "request_path": "/api/orders",
            "record_count": 0
        }))
        .send()
        .await
        .expect("start-debug request should complete");
    assert_eq!(zero_record_count.status(), reqwest::StatusCode::BAD_REQUEST);
    let zero_record_count_json = zero_record_count
        .json::<serde_json::Value>()
        .await
        .expect("error payload should decode");
    assert!(
        zero_record_count_json["error"]
            .as_str()
            .unwrap_or_default()
            .contains(">= 1"),
        "unexpected error payload: {zero_record_count_json}"
    );

    let recording_start = client
        .post(format!(
            "http://{addr}/v1/edges/dp-debug-validation/commands/start-debug"
        ))
        .json(&serde_json::json!({
            "mode": "recording",
            "tcp_addr": "127.0.0.1:9001",
            "header_name": "x-debug-token",
            "request_path": "/api/orders",
            "record_count": 2,
            "stop_on_entry": true
        }))
        .send()
        .await
        .expect("start-debug request should complete");
    assert_eq!(recording_start.status(), reqwest::StatusCode::ACCEPTED);

    let poll_recording = client
        .post(format!("http://{addr}/rpc/v1/edge/poll"))
        .json(&EdgePollRequest {
            edge_id: "dp-debug-validation".to_string(),
            edge_name: None,
            telemetry: empty_telemetry(),
            traffic_sample: None,
        })
        .send()
        .await
        .expect("poll should complete");
    assert_eq!(poll_recording.status(), reqwest::StatusCode::OK);
    let poll_recording_body = poll_recording
        .json::<EdgePollResponse>()
        .await
        .expect("poll body should decode");
    match poll_recording_body.command {
        Some(ControlPlaneCommand::StartDebugSession {
            mode,
            tcp_addr,
            header_name,
            request_path,
            record_count,
            stop_on_entry,
            ..
        }) => {
            assert_eq!(mode, edge::DebugSessionMode::Recording);
            assert_eq!(request_path.as_deref(), Some("/api/orders"));
            assert_eq!(record_count, Some(2));
            assert_eq!(stop_on_entry, Some(true));
            assert_eq!(tcp_addr, None);
            assert_eq!(header_name, None);
        }
        other => panic!("unexpected command payload: {other:?}"),
    }

    let interactive_start = client
        .post(format!(
            "http://{addr}/v1/edges/dp-debug-validation/commands/start-debug"
        ))
        .json(&serde_json::json!({
            "tcp_addr": "127.0.0.1:9002",
            "header_name": "x-debug-token",
            "stop_on_entry": false
        }))
        .send()
        .await
        .expect("start-debug request should complete");
    assert_eq!(interactive_start.status(), reqwest::StatusCode::ACCEPTED);

    let poll_interactive = client
        .post(format!("http://{addr}/rpc/v1/edge/poll"))
        .json(&EdgePollRequest {
            edge_id: "dp-debug-validation".to_string(),
            edge_name: None,
            telemetry: empty_telemetry(),
            traffic_sample: None,
        })
        .send()
        .await
        .expect("poll should complete");
    assert_eq!(poll_interactive.status(), reqwest::StatusCode::OK);
    let poll_interactive_body = poll_interactive
        .json::<EdgePollResponse>()
        .await
        .expect("poll body should decode");
    match poll_interactive_body.command {
        Some(ControlPlaneCommand::StartDebugSession {
            mode,
            tcp_addr,
            header_name,
            request_path,
            record_count,
            stop_on_entry,
            ..
        }) => {
            assert_eq!(mode, edge::DebugSessionMode::Interactive);
            assert_eq!(tcp_addr.as_deref(), Some("127.0.0.1:9002"));
            assert_eq!(header_name.as_deref(), Some("x-debug-token"));
            assert_eq!(request_path, None);
            assert_eq!(record_count, Some(1));
            assert_eq!(stop_on_entry, Some(false));
        }
        other => panic!("unexpected command payload: {other:?}"),
    }

    handle.abort();
}
