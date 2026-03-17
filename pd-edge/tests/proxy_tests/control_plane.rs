use super::support::*;

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
    let status = response.status();
    let body = response.text().await.expect("body should read");
    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert_eq!(body, "from-active-control-plane");

    active_handle.abort();
    data_handle.abort();
    rpc_handle.abort();
}
