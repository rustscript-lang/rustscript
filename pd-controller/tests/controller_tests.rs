use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use edge::{
    CommandResultPayload, ControlPlaneCommand, EdgeCommandResult, EdgePollRequest,
    EdgePollResponse, EdgeTrafficSample, ProgramApplyReport, TelemetrySnapshot,
};
use pd_controller::{ControllerConfig, ControllerState, EdgeDetailResponse, build_controller_app};
use tokio::task::JoinHandle;
use uuid::Uuid;
use vm::decode_program;

static TEST_STATE_PATH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

async fn spawn_controller(
    config: ControllerConfig,
) -> (SocketAddr, JoinHandle<()>, ControllerState) {
    let state = ControllerState::new(config);
    let app = build_controller_app(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("controller should run");
    });
    (addr, handle, state)
}

fn empty_telemetry() -> TelemetrySnapshot {
    TelemetrySnapshot {
        uptime_seconds: 0,
        program_loaded: false,
        debug_session_active: false,
        debug_session_attached: false,
        debug_session_current_line: None,
        debug_session_request_id: None,
        data_requests_total: 0,
        vm_execution_errors_total: 0,
        program_apply_success_total: 0,
        program_apply_failure_total: 0,
        control_rpc_polls_success_total: 0,
        control_rpc_polls_error_total: 0,
        control_rpc_results_success_total: 0,
        control_rpc_results_error_total: 0,
    }
}

fn unique_state_path(test_name: &str) -> PathBuf {
    let seq = TEST_STATE_PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("pd-controller-{test_name}-{now}-{seq}.json"))
}

fn snapshot_sidecar_paths(state_path: &Path) -> (PathBuf, PathBuf) {
    let parent = state_path
        .parent()
        .map(ToOwned::to_owned)
        .unwrap_or_else(std::env::temp_dir);
    let stem = state_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("state");
    (
        parent.join(format!("{stem}.programs.json")),
        parent.join(format!("{stem}.timeseries.bin")),
    )
}

#[tokio::test]
async fn poll_delivers_enqueued_command_and_results_are_queryable() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let enqueue = client
        .post(format!("http://{addr}/v1/edges/dp-1/commands/ping"))
        .header("content-type", "application/json")
        .body(r#"{"command_id":"cmd-ping-1","payload":"hello"}"#)
        .send()
        .await
        .expect("enqueue request should complete");
    assert_eq!(enqueue.status(), reqwest::StatusCode::ACCEPTED);

    let poll = client
        .post(format!("http://{addr}/rpc/v1/edge/poll"))
        .json(&EdgePollRequest {
            edge_id: "dp-1".to_string(),
            edge_name: Some("friendly-edge-1".to_string()),
            telemetry: empty_telemetry(),
            traffic_sample: Some(EdgeTrafficSample {
                requests_total: 10,
                status_2xx_total: 8,
                status_3xx_total: 1,
                status_4xx_total: 1,
                status_5xx_total: 0,
                latency_p50_ms: 0,
                latency_p90_ms: 0,
                latency_p99_ms: 0,
            }),
        })
        .send()
        .await
        .expect("poll should complete");
    assert_eq!(poll.status(), reqwest::StatusCode::OK);
    let poll_body = poll
        .json::<EdgePollResponse>()
        .await
        .expect("poll body should decode");
    match poll_body.command {
        Some(ControlPlaneCommand::Ping {
            command_id,
            payload,
        }) => {
            assert_eq!(command_id, "cmd-ping-1");
            assert_eq!(payload.as_deref(), Some("hello"));
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let no_command = client
        .post(format!("http://{addr}/rpc/v1/edge/poll"))
        .json(&EdgePollRequest {
            edge_id: "dp-1".to_string(),
            edge_name: Some("friendly-edge-1".to_string()),
            telemetry: empty_telemetry(),
            traffic_sample: Some(EdgeTrafficSample {
                requests_total: 16,
                status_2xx_total: 13,
                status_3xx_total: 1,
                status_4xx_total: 2,
                status_5xx_total: 0,
                latency_p50_ms: 0,
                latency_p90_ms: 0,
                latency_p99_ms: 0,
            }),
        })
        .send()
        .await
        .expect("poll should complete");
    let no_command_body = no_command
        .json::<EdgePollResponse>()
        .await
        .expect("poll body should decode");
    assert!(no_command_body.command.is_none());

    let result = EdgeCommandResult {
        edge_id: "dp-1".to_string(),
        edge_name: Some("friendly-edge-1".to_string()),
        command_id: "cmd-ping-1".to_string(),
        ok: true,
        result: CommandResultPayload::Pong {
            payload: Some("hello".to_string()),
        },
        telemetry: empty_telemetry(),
    };
    let post_result = client
        .post(format!("http://{addr}/rpc/v1/edge/result"))
        .json(&result)
        .send()
        .await
        .expect("result post should complete");
    assert_eq!(post_result.status(), reqwest::StatusCode::NO_CONTENT);

    let detail = client
        .get(format!("http://{addr}/v1/edges/dp-1"))
        .send()
        .await
        .expect("detail request should complete");
    assert_eq!(detail.status(), reqwest::StatusCode::OK);
    let detail_body = detail
        .json::<EdgeDetailResponse>()
        .await
        .expect("detail body should decode");
    assert_eq!(detail_body.summary.total_polls, 2);
    assert_eq!(detail_body.summary.total_results, 1);
    assert!(detail_body.summary.last_seen_unix_ms.is_some());
    assert_eq!(detail_body.summary.edge_name, "friendly-edge-1");
    assert!(Uuid::parse_str(&detail_body.summary.edge_id).is_ok());
    assert_eq!(detail_body.summary.sync_status, "not_synced");
    assert_eq!(detail_body.traffic_series.len(), 2);
    assert_eq!(detail_body.traffic_series[1].requests, 6);
    assert_eq!(detail_body.traffic_series[1].status_2xx, 5);

    let results = client
        .get(format!("http://{addr}/v1/edges/dp-1/results?limit=1"))
        .send()
        .await
        .expect("results request should complete");
    assert_eq!(results.status(), reqwest::StatusCode::OK);
    let results_json = results
        .json::<serde_json::Value>()
        .await
        .expect("results body should decode");
    let command_id = results_json["results"][0]["command_id"]
        .as_str()
        .expect("command_id should be a string");
    assert_eq!(command_id, "cmd-ping-1");

    handle.abort();
}

#[tokio::test]
async fn binary_program_upload_enqueues_apply_program_command() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();
    let binary = vec![0x56, 0x4D, 0x42, 0x43, 0x01, 0x02, 0x03];

    let enqueue = client
        .put(format!("http://{addr}/v1/edges/dp-2/program"))
        .header("content-type", "application/octet-stream")
        .body(binary.clone())
        .send()
        .await
        .expect("enqueue request should complete");
    assert_eq!(enqueue.status(), reqwest::StatusCode::ACCEPTED);

    let poll = client
        .post(format!("http://{addr}/rpc/v1/edge/poll"))
        .json(&EdgePollRequest {
            edge_id: "dp-2".to_string(),
            edge_name: None,
            telemetry: empty_telemetry(),
            traffic_sample: None,
        })
        .send()
        .await
        .expect("poll should complete");
    assert_eq!(poll.status(), reqwest::StatusCode::OK);
    let poll_body = poll
        .json::<EdgePollResponse>()
        .await
        .expect("poll body should decode");

    match poll_body.command {
        Some(ControlPlaneCommand::ApplyProgram { program_base64, .. }) => {
            let decoded = STANDARD
                .decode(program_base64.as_bytes())
                .expect("base64 payload should decode");
            assert_eq!(decoded, binary);
        }
        other => panic!("unexpected command payload: {other:?}"),
    }

    handle.abort();
}

#[tokio::test]
async fn poll_traffic_series_dedupes_when_counters_do_not_change() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    for sample in [
        EdgeTrafficSample {
            requests_total: 10,
            status_2xx_total: 8,
            status_3xx_total: 1,
            status_4xx_total: 1,
            status_5xx_total: 0,
            latency_p50_ms: 10,
            latency_p90_ms: 20,
            latency_p99_ms: 30,
        },
        EdgeTrafficSample {
            requests_total: 10,
            status_2xx_total: 8,
            status_3xx_total: 1,
            status_4xx_total: 1,
            status_5xx_total: 0,
            latency_p50_ms: 10,
            latency_p90_ms: 20,
            latency_p99_ms: 30,
        },
        EdgeTrafficSample {
            requests_total: 12,
            status_2xx_total: 10,
            status_3xx_total: 1,
            status_4xx_total: 1,
            status_5xx_total: 0,
            latency_p50_ms: 11,
            latency_p90_ms: 22,
            latency_p99_ms: 33,
        },
    ] {
        let response = client
            .post(format!("http://{addr}/rpc/v1/edge/poll"))
            .json(&EdgePollRequest {
                edge_id: "dp-dedupe-traffic".to_string(),
                edge_name: None,
                telemetry: empty_telemetry(),
                traffic_sample: Some(sample),
            })
            .send()
            .await
            .expect("poll should complete");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }

    let detail = client
        .get(format!("http://{addr}/v1/edges/dp-dedupe-traffic"))
        .send()
        .await
        .expect("detail request should complete");
    assert_eq!(detail.status(), reqwest::StatusCode::OK);
    let detail_body = detail
        .json::<EdgeDetailResponse>()
        .await
        .expect("detail body should decode");
    assert_eq!(detail_body.traffic_series.len(), 2);
    assert_eq!(detail_body.traffic_series[1].requests, 2);
    assert_eq!(detail_body.traffic_series[1].status_2xx, 2);

    handle.abort();
}

#[tokio::test]
async fn result_telemetry_does_not_override_poll_telemetry_snapshot() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let mut poll_telemetry = empty_telemetry();
    poll_telemetry.uptime_seconds = 11;
    poll_telemetry.program_loaded = true;
    poll_telemetry.control_rpc_polls_success_total = 3;

    let poll = client
        .post(format!("http://{addr}/rpc/v1/edge/poll"))
        .json(&EdgePollRequest {
            edge_id: "dp-telemetry-dedupe".to_string(),
            edge_name: None,
            telemetry: poll_telemetry.clone(),
            traffic_sample: None,
        })
        .send()
        .await
        .expect("poll should complete");
    assert_eq!(poll.status(), reqwest::StatusCode::OK);

    let mut result_telemetry = empty_telemetry();
    result_telemetry.uptime_seconds = 99;
    result_telemetry.control_rpc_results_success_total = 77;

    let post_result = client
        .post(format!("http://{addr}/rpc/v1/edge/result"))
        .json(&EdgeCommandResult {
            edge_id: "dp-telemetry-dedupe".to_string(),
            edge_name: None,
            command_id: "cmd-telemetry-dedupe".to_string(),
            ok: true,
            result: CommandResultPayload::Pong { payload: None },
            telemetry: result_telemetry,
        })
        .send()
        .await
        .expect("result post should complete");
    assert_eq!(post_result.status(), reqwest::StatusCode::NO_CONTENT);

    let detail = client
        .get(format!("http://{addr}/v1/edges/dp-telemetry-dedupe"))
        .send()
        .await
        .expect("detail request should complete");
    assert_eq!(detail.status(), reqwest::StatusCode::OK);
    let detail_body = detail
        .json::<EdgeDetailResponse>()
        .await
        .expect("detail body should decode");
    let snapshot = detail_body
        .summary
        .last_telemetry
        .expect("poll telemetry snapshot should be available");
    assert_eq!(snapshot.uptime_seconds, poll_telemetry.uptime_seconds);
    assert_eq!(
        snapshot.control_rpc_polls_success_total,
        poll_telemetry.control_rpc_polls_success_total
    );

    handle.abort();
}

#[tokio::test]
async fn ui_blocks_and_deploy_endpoints_work() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let blocks = client
        .get(format!("http://{addr}/v1/ui/blocks"))
        .send()
        .await
        .expect("blocks request should complete");
    assert_eq!(blocks.status(), reqwest::StatusCode::OK);
    let blocks_json = blocks
        .json::<serde_json::Value>()
        .await
        .expect("blocks payload should decode");
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("set_response_content"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("string_concat"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("math_add"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("array_push"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("map_set"))
    );
    let blocks = blocks_json["blocks"]
        .as_array()
        .expect("blocks should be an array");
    let get_header = blocks
        .iter()
        .find(|item| item["id"].as_str() == Some("get_header"))
        .expect("get_header block should exist");
    assert_eq!(
        get_header["category"].as_str(),
        Some("http_request"),
        "get_header should be request-http scoped"
    );
    let set_response_content = blocks
        .iter()
        .find(|item| item["id"].as_str() == Some("set_response_content"))
        .expect("set_response_content block should exist");
    assert_eq!(
        set_response_content["category"].as_str(),
        Some("http_response"),
        "set_response_content should be response-http scoped"
    );
    let set_upstream = blocks
        .iter()
        .find(|item| item["id"].as_str() == Some("set_upstream"))
        .expect("set_upstream block should exist");
    assert_eq!(
        set_upstream["category"].as_str(),
        Some("routing"),
        "set_upstream should be routing scoped"
    );

    let deploy = client
        .post(format!("http://{addr}/v1/ui/deploy"))
        .json(&serde_json::json!({
            "edge_id": "dp-ui-1",
            "flavor": "rustscript",
            "blocks": [
                {
                    "block_id": "set_response_content",
                    "values": {
                        "value": "hello from ui deploy"
                    }
                }
            ]
        }))
        .send()
        .await
        .expect("deploy request should complete");
    assert_eq!(deploy.status(), reqwest::StatusCode::ACCEPTED);
    let deploy_json = deploy
        .json::<serde_json::Value>()
        .await
        .expect("deploy payload should decode");
    let command_id = deploy_json["command_id"]
        .as_str()
        .expect("command_id should be present")
        .to_string();

    let poll = client
        .post(format!("http://{addr}/rpc/v1/edge/poll"))
        .json(&EdgePollRequest {
            edge_id: "dp-ui-1".to_string(),
            edge_name: None,
            telemetry: empty_telemetry(),
            traffic_sample: None,
        })
        .send()
        .await
        .expect("poll should complete");
    assert_eq!(poll.status(), reqwest::StatusCode::OK);
    let poll_body = poll
        .json::<EdgePollResponse>()
        .await
        .expect("poll body should decode");
    match poll_body.command {
        Some(ControlPlaneCommand::ApplyProgram {
            command_id: polled_command_id,
            program_base64,
        }) => {
            assert_eq!(polled_command_id, command_id);
            let decoded = STANDARD
                .decode(program_base64.as_bytes())
                .expect("program base64 should decode");
            let program =
                decode_program(&decoded).expect("decoded payload should be a valid program");
            assert!(
                !program.code.is_empty(),
                "compiled program should include bytecode instructions"
            );
        }
        other => panic!("unexpected command payload: {other:?}"),
    }

    handle.abort();
}

#[tokio::test]
async fn ui_deploy_compiles_graph_code_for_all_flavors() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let flavors = [
        ("rustscript", "dp-ui-rss", "use vm;"),
        ("javascript", "dp-ui-js", "import * as vm from \"vm\";"),
        ("lua", "dp-ui-lua", "local vm = require(\"vm\")"),
        ("scheme", "dp-ui-scm", "(require (prefix-in vm. \"vm\"))"),
    ];

    for (flavor, edge_id, expected_prelude) in flavors {
        let deploy = client
            .post(format!("http://{addr}/v1/ui/deploy"))
            .json(&serde_json::json!({
                "edge_id": edge_id,
                "flavor": flavor,
                "blocks": [
                    {
                        "block_id": "set_response_content",
                        "values": {
                            "value": "hello from ui deploy"
                        }
                    }
                ]
            }))
            .send()
            .await
            .expect("deploy request should complete");
        assert_eq!(
            deploy.status(),
            reqwest::StatusCode::ACCEPTED,
            "deploy should compile for flavor {flavor}"
        );
        let deploy_json = deploy
            .json::<serde_json::Value>()
            .await
            .expect("deploy payload should decode");
        let source = deploy_json["source"][flavor]
            .as_str()
            .expect("flavor source should be present");
        assert!(
            source.contains(expected_prelude),
            "generated source should include vm prelude for flavor {flavor}, got: {source}"
        );
    }

    handle.abort();
}

#[tokio::test]
async fn ui_render_extended_value_blocks_work_with_flow_graph() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let render = client
        .post(format!("http://{addr}/v1/ui/render"))
        .json(&serde_json::json!({
            "nodes": [
                {
                    "id": "n1",
                    "block_id": "const_string",
                    "values": { "var": "first", "value": "hello " }
                },
                {
                    "id": "n2",
                    "block_id": "const_string",
                    "values": { "var": "second", "value": "world" }
                },
                {
                    "id": "n3",
                    "block_id": "string_concat",
                    "values": { "var": "joined", "left": "left", "right": "right" }
                },
                {
                    "id": "n4",
                    "block_id": "string_length",
                    "values": { "var": "joined_len", "value": "value" }
                },
                {
                    "id": "n5",
                    "block_id": "const_number",
                    "values": { "var": "status_base", "value": "200" }
                },
                {
                    "id": "n6",
                    "block_id": "math_add",
                    "values": { "var": "status_plus_len", "lhs": "1", "rhs": "1" }
                },
                {
                    "id": "n7",
                    "block_id": "array_new",
                    "values": { "var": "items" }
                },
                {
                    "id": "n8",
                    "block_id": "array_push",
                    "values": { "var": "items_with_msg", "array": "$items", "value": "item" }
                },
                {
                    "id": "n9",
                    "block_id": "array_get",
                    "values": { "var": "first_item", "array": "$items_with_msg", "index": "0" }
                },
                {
                    "id": "n10",
                    "block_id": "map_new",
                    "values": { "var": "result_map" }
                },
                {
                    "id": "n11",
                    "block_id": "map_set",
                    "values": { "var": "result_map_next", "map": "$result_map", "key": "body", "value": "value" }
                },
                {
                    "id": "n12",
                    "block_id": "map_get",
                    "values": { "var": "response_body", "map": "$result_map_next", "key": "body" }
                },
                {
                    "id": "n13",
                    "block_id": "set_response_status",
                    "values": { "status": "200" }
                },
                {
                    "id": "n14",
                    "block_id": "set_response_content",
                    "values": { "value": "fallback" }
                }
            ],
            "edges": [
                {
                    "source": "n1",
                    "source_output": "value",
                    "target": "n3",
                    "target_input": "left"
                },
                {
                    "source": "n2",
                    "source_output": "value",
                    "target": "n3",
                    "target_input": "right"
                },
                {
                    "source": "n3",
                    "source_output": "value",
                    "target": "n4",
                    "target_input": "value"
                },
                {
                    "source": "n5",
                    "source_output": "value",
                    "target": "n6",
                    "target_input": "lhs"
                },
                {
                    "source": "n4",
                    "source_output": "value",
                    "target": "n6",
                    "target_input": "rhs"
                },
                {
                    "source": "n7",
                    "source_output": "value",
                    "target": "n8",
                    "target_input": "array"
                },
                {
                    "source": "n3",
                    "source_output": "value",
                    "target": "n8",
                    "target_input": "value"
                },
                {
                    "source": "n8",
                    "source_output": "value",
                    "target": "n9",
                    "target_input": "array"
                },
                {
                    "source": "n10",
                    "source_output": "value",
                    "target": "n11",
                    "target_input": "map"
                },
                {
                    "source": "n9",
                    "source_output": "value",
                    "target": "n11",
                    "target_input": "value"
                },
                {
                    "source": "n11",
                    "source_output": "value",
                    "target": "n12",
                    "target_input": "map"
                },
                {
                    "source": "n6",
                    "source_output": "value",
                    "target": "n14",
                    "target_input": "value"
                },
                {
                    "source": "n13",
                    "source_output": "next",
                    "target": "n14",
                    "target_input": "__flow"
                }
            ]
        }))
        .send()
        .await
        .expect("render request should complete");

    assert_eq!(render.status(), reqwest::StatusCode::OK);
    let render_json = render
        .json::<serde_json::Value>()
        .await
        .expect("render payload should decode");
    let rustscript = render_json["source"]["rustscript"]
        .as_str()
        .expect("rustscript source should be a string");
    assert!(
        rustscript.contains("let joined = first + second;"),
        "expected string concat line, got: {rustscript}"
    );
    assert!(
        rustscript.contains("let joined_len = len(joined);"),
        "expected string length line, got: {rustscript}"
    );
    assert!(
        rustscript.contains("let status_plus_len = status_base + joined_len;"),
        "expected math add line, got: {rustscript}"
    );
    assert!(
        rustscript.contains("let items = [];"),
        "expected array_new line, got: {rustscript}"
    );
    assert!(
        rustscript.contains("let items_with_msg = items;"),
        "expected array_push clone line, got: {rustscript}"
    );
    assert!(
        rustscript.contains("items_with_msg[len(items_with_msg)] = joined;"),
        "expected array_push append line, got: {rustscript}"
    );
    assert!(
        rustscript.contains("let first_item = (items_with_msg)[0];"),
        "expected array_get line, got: {rustscript}"
    );
    assert!(
        rustscript.contains("let result_map = {};"),
        "expected map_new line, got: {rustscript}"
    );
    assert!(
        rustscript.contains("let result_map_next = result_map;"),
        "expected map_set clone line, got: {rustscript}"
    );
    assert!(
        rustscript.contains("result_map_next.body = first_item;"),
        "expected map_set assignment line, got: {rustscript}"
    );
    assert!(
        rustscript.contains("let response_body = (result_map_next).body;"),
        "expected map_get access line, got: {rustscript}"
    );
    assert!(
        !rustscript.contains("array_push("),
        "expected array_push line, got: {rustscript}"
    );
    assert!(
        rustscript.contains("vm::set_response_content(status_plus_len);"),
        "expected data edge into flow action, got: {rustscript}"
    );

    handle.abort();
}

#[tokio::test]
async fn ui_render_string_slice_emits_range_syntax_in_all_flavors() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let render = client
        .post(format!("http://{addr}/v1/ui/render"))
        .json(&serde_json::json!({
            "nodes": [
                {
                    "id": "n1",
                    "block_id": "const_string",
                    "values": { "var": "text", "value": "abcdef" }
                },
                {
                    "id": "n2",
                    "block_id": "string_slice",
                    "values": { "var": "text_slice", "value": "value", "start": "", "end": "-1" }
                }
            ],
            "edges": [
                {
                    "source": "n1",
                    "source_output": "value",
                    "target": "n2",
                    "target_input": "value"
                }
            ]
        }))
        .send()
        .await
        .expect("render request should complete");

    assert_eq!(render.status(), reqwest::StatusCode::OK);
    let render_json = render
        .json::<serde_json::Value>()
        .await
        .expect("render payload should decode");

    let rustscript = render_json["source"]["rustscript"]
        .as_str()
        .expect("rustscript source should be a string");
    assert!(
        rustscript.contains("let text_slice = (text)[:-1];"),
        "expected rustscript bracket slice output, got: {rustscript}"
    );

    let javascript = render_json["source"]["javascript"]
        .as_str()
        .expect("javascript source should be a string");
    assert!(
        javascript.contains("let text_slice = (text)[:-1];"),
        "expected javascript bracket slice output, got: {javascript}"
    );

    let lua = render_json["source"]["lua"]
        .as_str()
        .expect("lua source should be a string");
    assert!(
        lua.contains("local text_slice = (text)[:-1]"),
        "expected lua bracket slice output, got: {lua}"
    );

    let scheme = render_json["source"]["scheme"]
        .as_str()
        .expect("scheme source should be a string");
    assert!(
        scheme.contains("(define text_slice (slice-to text -1))"),
        "expected scheme range slice helper output, got: {scheme}"
    );

    handle.abort();
}

#[tokio::test]
async fn ui_render_graph_connections_produce_identifier_expressions() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let render = client
        .post(format!("http://{addr}/v1/ui/render"))
        .json(&serde_json::json!({
            "nodes": [
                {
                    "id": "n1",
                    "block_id": "get_header",
                    "values": { "var": "client_id", "name": "x-client-id" }
                },
                {
                    "id": "n2",
                    "block_id": "set_response_content",
                    "values": { "value": "fallback" }
                }
            ],
            "edges": [
                {
                    "source": "n1",
                    "source_output": "value",
                    "target": "n2",
                    "target_input": "value"
                }
            ]
        }))
        .send()
        .await
        .expect("render request should complete");
    assert_eq!(render.status(), reqwest::StatusCode::OK);
    let render_json = render
        .json::<serde_json::Value>()
        .await
        .expect("render payload should decode");
    let rustscript = render_json["source"]["rustscript"]
        .as_str()
        .expect("rustscript source should be a string");
    assert!(
        rustscript.contains("vm::set_response_content(client_id);"),
        "expected downstream value to reference connected identifier, got: {rustscript}"
    );

    handle.abort();
}

#[tokio::test]
async fn ui_render_rate_limit_flow_branches_to_actions() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let render = client
        .post(format!("http://{addr}/v1/ui/render"))
        .json(&serde_json::json!({
            "nodes": [
                {
                    "id": "n1",
                    "block_id": "get_header",
                    "values": { "var": "client_id", "name": "x-client-id" }
                },
                {
                    "id": "n2",
                    "block_id": "rate_limit_if_else",
                    "values": { "key_expr": "$client_id", "limit": "3", "window_seconds": "60" }
                },
                {
                    "id": "n3",
                    "block_id": "set_response_content",
                    "values": { "value": "request allowed" }
                },
                {
                    "id": "n4",
                    "block_id": "set_response_status",
                    "values": { "status": "429" }
                },
                {
                    "id": "n5",
                    "block_id": "set_response_content",
                    "values": { "value": "rate limit exceeded" }
                }
            ],
            "edges": [
                {
                    "source": "n1",
                    "source_output": "value",
                    "target": "n2",
                    "target_input": "key_expr"
                },
                {
                    "source": "n2",
                    "source_output": "allowed",
                    "target": "n3",
                    "target_input": "__flow"
                },
                {
                    "source": "n2",
                    "source_output": "blocked",
                    "target": "n4",
                    "target_input": "__flow"
                },
                {
                    "source": "n4",
                    "source_output": "__ignored",
                    "target": "n5",
                    "target_input": "__flow"
                }
            ]
        }))
        .send()
        .await
        .expect("render request should complete");

    assert_eq!(render.status(), reqwest::StatusCode::BAD_REQUEST);
    let err = render
        .json::<serde_json::Value>()
        .await
        .expect("error payload should decode");
    assert!(
        err["error"]
            .as_str()
            .unwrap_or_default()
            .contains("source output"),
        "unexpected error payload: {err}"
    );

    let render = client
        .post(format!("http://{addr}/v1/ui/render"))
        .json(&serde_json::json!({
            "nodes": [
                {
                    "id": "n1",
                    "block_id": "get_header",
                    "values": { "var": "client_id", "name": "x-client-id" }
                },
                {
                    "id": "n2",
                    "block_id": "rate_limit_if_else",
                    "values": { "key_expr": "$client_id", "limit": "3", "window_seconds": "60" }
                },
                {
                    "id": "n3",
                    "block_id": "set_response_content",
                    "values": { "value": "request allowed" }
                },
                {
                    "id": "n4",
                    "block_id": "set_response_status",
                    "values": { "status": "429" }
                }
            ],
            "edges": [
                {
                    "source": "n1",
                    "source_output": "value",
                    "target": "n2",
                    "target_input": "key_expr"
                },
                {
                    "source": "n2",
                    "source_output": "allowed",
                    "target": "n3",
                    "target_input": "__flow"
                },
                {
                    "source": "n2",
                    "source_output": "blocked",
                    "target": "n4",
                    "target_input": "__flow"
                }
            ]
        }))
        .send()
        .await
        .expect("render request should complete");

    assert_eq!(render.status(), reqwest::StatusCode::OK);
    let render_json = render
        .json::<serde_json::Value>()
        .await
        .expect("render payload should decode");
    let rustscript = render_json["source"]["rustscript"]
        .as_str()
        .expect("rustscript source should be a string");
    assert!(
        rustscript.contains("if vm::rate_limit_allow(client_id, 3, 60) {"),
        "expected rate limit branch in rustscript, got: {rustscript}"
    );
    assert!(
        rustscript.contains("vm::set_response_status(429);"),
        "expected blocked branch to set status, got: {rustscript}"
    );

    handle.abort();
}

#[tokio::test]
async fn ui_render_plain_if_and_loop_flow() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let render = client
        .post(format!("http://{addr}/v1/ui/render"))
        .json(&serde_json::json!({
            "nodes": [
                {
                    "id": "n1",
                    "block_id": "const_number",
                    "values": { "var": "lhs_value", "value": "3" }
                },
                {
                    "id": "n2",
                    "block_id": "const_number",
                    "values": { "var": "rhs_value", "value": "3" }
                },
                {
                    "id": "n3",
                    "block_id": "if",
                    "values": { "lhs": "left", "rhs": "right" }
                },
                {
                    "id": "n4",
                    "block_id": "loop",
                    "values": { "count": "2" }
                },
                {
                    "id": "n5",
                    "block_id": "set_header",
                    "values": { "name": "x-loop", "value": "tick" }
                },
                {
                    "id": "n6",
                    "block_id": "set_response_content",
                    "values": { "value": "if true done" }
                },
                {
                    "id": "n7",
                    "block_id": "set_response_status",
                    "values": { "status": "403" }
                }
            ],
            "edges": [
                {
                    "source": "n1",
                    "source_output": "value",
                    "target": "n3",
                    "target_input": "lhs"
                },
                {
                    "source": "n2",
                    "source_output": "value",
                    "target": "n3",
                    "target_input": "rhs"
                },
                {
                    "source": "n3",
                    "source_output": "true",
                    "target": "n4",
                    "target_input": "__flow"
                },
                {
                    "source": "n3",
                    "source_output": "false",
                    "target": "n7",
                    "target_input": "__flow"
                },
                {
                    "source": "n4",
                    "source_output": "body",
                    "target": "n5",
                    "target_input": "__flow"
                },
                {
                    "source": "n4",
                    "source_output": "done",
                    "target": "n6",
                    "target_input": "__flow"
                }
            ]
        }))
        .send()
        .await
        .expect("render request should complete");

    assert_eq!(render.status(), reqwest::StatusCode::OK);
    let render_json = render
        .json::<serde_json::Value>()
        .await
        .expect("render payload should decode");
    let rustscript = render_json["source"]["rustscript"]
        .as_str()
        .expect("rustscript source should be a string");
    assert!(
        rustscript.contains("if lhs_value == rhs_value {"),
        "expected plain if compare in rustscript, got: {rustscript}"
    );
    assert!(
        rustscript.contains("for (let i = 0; i < 2; i = i + 1) {"),
        "expected plain loop in rustscript, got: {rustscript}"
    );
    assert!(
        rustscript.contains("vm::set_header(\"x-loop\", \"tick\");"),
        "expected loop body action in rustscript, got: {rustscript}"
    );
    assert!(
        rustscript.contains("vm::set_response_content(\"if true done\");"),
        "expected loop done action in rustscript, got: {rustscript}"
    );
    assert!(
        rustscript.contains("vm::set_response_status(403);"),
        "expected if false branch action in rustscript, got: {rustscript}"
    );

    handle.abort();
}

#[tokio::test]
async fn ui_render_if_without_false_edge_omits_else_block() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let render = client
        .post(format!("http://{addr}/v1/ui/render"))
        .json(&serde_json::json!({
            "nodes": [
                {
                    "id": "n1",
                    "block_id": "const_number",
                    "values": { "var": "lhs_value", "value": "3" }
                },
                {
                    "id": "n2",
                    "block_id": "const_number",
                    "values": { "var": "rhs_value", "value": "3" }
                },
                {
                    "id": "n3",
                    "block_id": "if",
                    "values": { "lhs": "left", "rhs": "right" }
                },
                {
                    "id": "n4",
                    "block_id": "set_response_content",
                    "values": { "value": "if true done" }
                }
            ],
            "edges": [
                {
                    "source": "n1",
                    "source_output": "value",
                    "target": "n3",
                    "target_input": "lhs"
                },
                {
                    "source": "n2",
                    "source_output": "value",
                    "target": "n3",
                    "target_input": "rhs"
                },
                {
                    "source": "n3",
                    "source_output": "true",
                    "target": "n4",
                    "target_input": "__flow"
                }
            ]
        }))
        .send()
        .await
        .expect("render request should complete");

    assert_eq!(render.status(), reqwest::StatusCode::OK);
    let render_json = render
        .json::<serde_json::Value>()
        .await
        .expect("render payload should decode");
    let rustscript = render_json["source"]["rustscript"]
        .as_str()
        .expect("rustscript source should be a string");
    assert!(
        rustscript.contains("if lhs_value == rhs_value {"),
        "expected plain if compare in rustscript, got: {rustscript}"
    );
    assert!(
        !rustscript.contains("} else {"),
        "expected no else block when false edge is missing, got: {rustscript}"
    );

    handle.abort();
}

#[tokio::test]
async fn programs_can_be_versioned_and_applied_to_edge() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let created = client
        .post(format!("http://{addr}/v1/programs"))
        .json(&serde_json::json!({
            "name": "edge program"
        }))
        .send()
        .await
        .expect("create program should complete");
    assert_eq!(created.status(), reqwest::StatusCode::CREATED);
    let created_json = created
        .json::<serde_json::Value>()
        .await
        .expect("create payload should decode");
    let program_id = created_json["program_id"]
        .as_str()
        .expect("program_id should be set")
        .to_string();
    assert!(Uuid::parse_str(&program_id).is_ok());

    let created_version = client
        .post(format!("http://{addr}/v1/programs/{program_id}/versions"))
        .json(&serde_json::json!({
            "flavor": "rustscript",
            "nodes": [
                {
                    "id": "n1",
                    "block_id": "set_response_content",
                    "values": { "value": "from stored version" }
                }
            ],
            "edges": []
        }))
        .send()
        .await
        .expect("create version should complete");
    assert_eq!(created_version.status(), reqwest::StatusCode::CREATED);
    let created_version_json = created_version
        .json::<serde_json::Value>()
        .await
        .expect("version payload should decode");
    assert_eq!(created_version_json["detail"]["version"], 1);

    let programs = client
        .get(format!("http://{addr}/v1/programs"))
        .send()
        .await
        .expect("program list should complete");
    assert_eq!(programs.status(), reqwest::StatusCode::OK);
    let programs_json = programs
        .json::<serde_json::Value>()
        .await
        .expect("program list should decode");
    assert!(
        programs_json["programs"]
            .as_array()
            .expect("programs should be array")
            .iter()
            .any(|item| item["program_id"].as_str() == Some(program_id.as_str()))
    );

    let apply = client
        .post(format!(
            "http://{addr}/v1/edges/dp-program-1/commands/apply-program-version"
        ))
        .json(&serde_json::json!({
            "program_id": program_id,
            "version": 1
        }))
        .send()
        .await
        .expect("apply version should complete");
    assert_eq!(apply.status(), reqwest::StatusCode::ACCEPTED);

    let poll = client
        .post(format!("http://{addr}/rpc/v1/edge/poll"))
        .json(&EdgePollRequest {
            edge_id: "dp-program-1".to_string(),
            edge_name: None,
            telemetry: empty_telemetry(),
            traffic_sample: None,
        })
        .send()
        .await
        .expect("poll should complete");
    assert_eq!(poll.status(), reqwest::StatusCode::OK);
    let poll_body = poll
        .json::<EdgePollResponse>()
        .await
        .expect("poll body should decode");
    let polled_command_id = match poll_body.command {
        Some(ControlPlaneCommand::ApplyProgram {
            command_id,
            program_base64,
        }) => {
            let decoded = STANDARD
                .decode(program_base64.as_bytes())
                .expect("program base64 should decode");
            let program =
                decode_program(&decoded).expect("decoded payload should be a valid program");
            assert!(
                !program.code.is_empty(),
                "compiled program should include bytecode instructions"
            );
            command_id
        }
        other => panic!("unexpected command payload: {other:?}"),
    };

    let post_result = client
        .post(format!("http://{addr}/rpc/v1/edge/result"))
        .json(&EdgeCommandResult {
            edge_id: "dp-program-1".to_string(),
            edge_name: None,
            command_id: polled_command_id,
            ok: true,
            result: CommandResultPayload::ApplyProgram {
                report: ProgramApplyReport {
                    applied: true,
                    constants: Some(0),
                    code_bytes: Some(0),
                    local_count: Some(0),
                    message: Some("applied".to_string()),
                },
            },
            telemetry: empty_telemetry(),
        })
        .send()
        .await
        .expect("result post should complete");
    assert_eq!(post_result.status(), reqwest::StatusCode::NO_CONTENT);

    let detail = client
        .get(format!("http://{addr}/v1/edges/dp-program-1"))
        .send()
        .await
        .expect("detail request should complete");
    assert_eq!(detail.status(), reqwest::StatusCode::OK);
    let detail_body = detail
        .json::<EdgeDetailResponse>()
        .await
        .expect("detail body should decode");
    let applied = detail_body
        .summary
        .applied_program
        .expect("applied program should be present");
    assert_eq!(applied.name, "edge program");
    assert_eq!(applied.version, 1);

    handle.abort();
}

#[tokio::test]
async fn controller_persists_programs_applied_versions_and_traffic_series() {
    let state_path = unique_state_path("persistence");
    let config = ControllerConfig {
        state_path: Some(state_path.clone()),
        ..ControllerConfig::default()
    };

    let (addr, handle, _state) = spawn_controller(config.clone()).await;
    let client = reqwest::Client::new();

    let created = client
        .post(format!("http://{addr}/v1/programs"))
        .json(&serde_json::json!({
            "name": "persisted edge program"
        }))
        .send()
        .await
        .expect("create program should complete");
    assert_eq!(created.status(), reqwest::StatusCode::CREATED);
    let created_json = created
        .json::<serde_json::Value>()
        .await
        .expect("create payload should decode");
    let program_id = created_json["program_id"]
        .as_str()
        .expect("program_id should be set")
        .to_string();

    let created_version = client
        .post(format!("http://{addr}/v1/programs/{program_id}/versions"))
        .json(&serde_json::json!({
            "flavor": "rustscript",
            "nodes": [
                {
                    "id": "n1",
                    "block_id": "set_response_content",
                    "values": { "value": "persisted version" }
                }
            ],
            "edges": []
        }))
        .send()
        .await
        .expect("create version should complete");
    assert_eq!(created_version.status(), reqwest::StatusCode::CREATED);

    let apply = client
        .post(format!(
            "http://{addr}/v1/edges/dp-persist-1/commands/apply-program-version"
        ))
        .json(&serde_json::json!({
            "program_id": program_id,
            "version": 1
        }))
        .send()
        .await
        .expect("apply version should complete");
    assert_eq!(apply.status(), reqwest::StatusCode::ACCEPTED);

    let poll = client
        .post(format!("http://{addr}/rpc/v1/edge/poll"))
        .json(&EdgePollRequest {
            edge_id: "dp-persist-1".to_string(),
            edge_name: None,
            telemetry: empty_telemetry(),
            traffic_sample: Some(EdgeTrafficSample {
                requests_total: 10,
                status_2xx_total: 9,
                status_3xx_total: 0,
                status_4xx_total: 1,
                status_5xx_total: 0,
                latency_p50_ms: 0,
                latency_p90_ms: 0,
                latency_p99_ms: 0,
            }),
        })
        .send()
        .await
        .expect("poll should complete");
    assert_eq!(poll.status(), reqwest::StatusCode::OK);
    let poll_body = poll
        .json::<EdgePollResponse>()
        .await
        .expect("poll body should decode");
    let command_id = match poll_body.command {
        Some(ControlPlaneCommand::ApplyProgram { command_id, .. }) => command_id,
        other => panic!("unexpected command payload: {other:?}"),
    };

    let second_poll = client
        .post(format!("http://{addr}/rpc/v1/edge/poll"))
        .json(&EdgePollRequest {
            edge_id: "dp-persist-1".to_string(),
            edge_name: None,
            telemetry: empty_telemetry(),
            traffic_sample: Some(EdgeTrafficSample {
                requests_total: 16,
                status_2xx_total: 14,
                status_3xx_total: 0,
                status_4xx_total: 2,
                status_5xx_total: 0,
                latency_p50_ms: 0,
                latency_p90_ms: 0,
                latency_p99_ms: 0,
            }),
        })
        .send()
        .await
        .expect("second poll should complete");
    assert_eq!(second_poll.status(), reqwest::StatusCode::OK);

    let post_result = client
        .post(format!("http://{addr}/rpc/v1/edge/result"))
        .json(&EdgeCommandResult {
            edge_id: "dp-persist-1".to_string(),
            edge_name: None,
            command_id,
            ok: true,
            result: CommandResultPayload::ApplyProgram {
                report: ProgramApplyReport {
                    applied: true,
                    constants: Some(0),
                    code_bytes: Some(0),
                    local_count: Some(0),
                    message: Some("applied".to_string()),
                },
            },
            telemetry: empty_telemetry(),
        })
        .send()
        .await
        .expect("result post should complete");
    assert_eq!(post_result.status(), reqwest::StatusCode::NO_CONTENT);

    handle.abort();
    let (programs_path, timeseries_path) = snapshot_sidecar_paths(&state_path);
    assert!(state_path.exists(), "core state file should exist");
    assert!(programs_path.exists(), "program snapshot should exist");
    assert!(timeseries_path.exists(), "timeseries snapshot should exist");

    let core_snapshot = fs::read_to_string(&state_path).expect("core state should be readable");
    assert!(
        !core_snapshot.contains("\"programs\""),
        "core state should not embed programs: {core_snapshot}"
    );
    assert!(
        !core_snapshot.contains("\"traffic_points\""),
        "core state should not embed traffic points: {core_snapshot}"
    );

    let programs_snapshot =
        fs::read_to_string(&programs_path).expect("program snapshot should be readable");
    assert!(
        programs_snapshot.contains("\"programs\""),
        "program snapshot should contain programs payload: {programs_snapshot}"
    );

    let timeseries_snapshot =
        fs::read(&timeseries_path).expect("timeseries snapshot should be readable");
    assert!(
        timeseries_snapshot.starts_with(b"PDTS"),
        "timeseries snapshot should start with binary magic header"
    );

    let (restarted_addr, restarted_handle, _restarted_state) = spawn_controller(config).await;
    let restarted_client = reqwest::Client::new();

    let programs = restarted_client
        .get(format!("http://{restarted_addr}/v1/programs"))
        .send()
        .await
        .expect("program list should complete");
    assert_eq!(programs.status(), reqwest::StatusCode::OK);
    let programs_json = programs
        .json::<serde_json::Value>()
        .await
        .expect("program list should decode");
    assert!(
        programs_json["programs"]
            .as_array()
            .expect("programs should be array")
            .iter()
            .any(|item| {
                item["name"] == "persisted edge program" && item["latest_version"] == 1
            }),
        "expected persisted program and version in list: {programs_json}"
    );

    let detail = restarted_client
        .get(format!("http://{restarted_addr}/v1/edges/dp-persist-1"))
        .send()
        .await
        .expect("detail request should complete");
    assert_eq!(detail.status(), reqwest::StatusCode::OK);
    let detail_body = detail
        .json::<EdgeDetailResponse>()
        .await
        .expect("detail body should decode");
    let applied = detail_body
        .summary
        .applied_program
        .expect("applied program should be present");
    assert_eq!(applied.name, "persisted edge program");
    assert_eq!(applied.version, 1);
    assert_eq!(detail_body.traffic_series.len(), 2);
    assert_eq!(detail_body.traffic_series[1].requests, 6);

    restarted_handle.abort();

    let _ = fs::remove_file(&state_path);
    let _ = fs::remove_file(&programs_path);
    let _ = fs::remove_file(&timeseries_path);
}
