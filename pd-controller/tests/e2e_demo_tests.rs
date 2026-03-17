use std::{net::SocketAddr, time::Duration};

use edge::{
    ActiveControlPlaneConfig, CommandResultPayload, EdgeCommandResult, SharedState,
    build_http_proxy_app, spawn_active_control_plane_client,
};
use pd_controller::{
    ControllerConfig, ControllerState, EnqueueCommandResponse, build_controller_app,
};
use tokio::task::JoinHandle;
use vm::{Program, compile_source, encode_program};

#[derive(serde::Deserialize)]
struct ResultsResponse {
    results: Vec<EdgeCommandResult>,
}

async fn spawn_server(app: axum::Router) -> (SocketAddr, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have address");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("server should run");
    });
    (addr, handle)
}

fn build_short_circuit_program(body: &str) -> Program {
    compile_source(&format!("use http;\nhttp::response::set_body({body:?});\n"))
        .expect("short-circuit e2e source should compile")
        .program
}

#[tokio::test]
async fn e2e_controller_can_push_program_to_active_proxy_edge() {
    let controller_state = ControllerState::new(ControllerConfig {
        default_poll_interval_ms: 100,
        ..ControllerConfig::default()
    });
    let (controller_addr, controller_handle) =
        spawn_server(build_controller_app(controller_state)).await;

    let proxy_state = SharedState::new(1024 * 1024);
    let proxy_state_check = proxy_state.clone();
    let (_data_addr, data_handle) = spawn_server(build_http_proxy_app(proxy_state.clone())).await;
    let active_handle = spawn_active_control_plane_client(
        proxy_state,
        ActiveControlPlaneConfig {
            control_plane_url: format!("http://{controller_addr}"),
            edge_id: "e2e-edge-1".to_string(),
            edge_name: "e2e-edge-name".to_string(),
            poll_interval_ms: 100,
            request_timeout_ms: 2_000,
        },
    );

    let client = reqwest::Client::new();
    let program = build_short_circuit_program("hello-from-e2e");
    let program_bytes = encode_program(&program).expect("program encoding should succeed");

    let enqueue = client
        .put(format!(
            "http://{controller_addr}/v1/edges/e2e-edge-1/program"
        ))
        .header("content-type", "application/octet-stream")
        .body(program_bytes)
        .send()
        .await
        .expect("controller enqueue should complete");
    assert_eq!(enqueue.status(), reqwest::StatusCode::ACCEPTED);
    let enqueue_body = enqueue
        .json::<EnqueueCommandResponse>()
        .await
        .expect("enqueue response should decode");

    let mut apply_result_seen = false;
    for _ in 0..100 {
        let results = client
            .get(format!(
                "http://{controller_addr}/v1/edges/e2e-edge-1/results?limit=20"
            ))
            .send()
            .await
            .expect("results request should complete");
        assert_eq!(results.status(), reqwest::StatusCode::OK);
        let results_body = results
            .json::<ResultsResponse>()
            .await
            .expect("results response should decode");

        let matched = results_body.results.iter().find(|result| {
            result.command_id == enqueue_body.command_id
                && matches!(
                    result.result,
                    CommandResultPayload::ApplyProgram { ref report } if report.applied
                )
        });
        if matched.is_some() {
            apply_result_seen = true;
            break;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        apply_result_seen,
        "controller never observed successful apply_program result for enqueued command"
    );

    assert!(
        proxy_state_check.loaded_program_snapshot().is_some(),
        "edge never retained the applied program in shared state"
    );

    active_handle.abort();
    data_handle.abort();
    controller_handle.abort();
}
