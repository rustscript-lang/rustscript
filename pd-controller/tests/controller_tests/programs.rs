use super::support::*;

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
                upstream_latency_p50_ms: 0,
                upstream_latency_p90_ms: 0,
                upstream_latency_p99_ms: 0,
                edge_latency_p50_ms: 0,
                edge_latency_p90_ms: 0,
                edge_latency_p99_ms: 0,
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
                upstream_latency_p50_ms: 0,
                upstream_latency_p90_ms: 0,
                upstream_latency_p99_ms: 0,
                edge_latency_p50_ms: 0,
                edge_latency_p90_ms: 0,
                edge_latency_p99_ms: 0,
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
    let (programs_path, timeseries_path, recordings_path, debug_sessions_path) =
        snapshot_sidecar_paths(&state_path);
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
    let _ = fs::remove_file(&recordings_path);
    let _ = fs::remove_file(&debug_sessions_path);
}

#[tokio::test]
async fn program_and_version_endpoints_validate_error_paths() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let created_empty = client
        .post(format!("http://{addr}/v1/programs"))
        .json(&serde_json::json!({ "name": "   " }))
        .send()
        .await
        .expect("create program should complete");
    assert_eq!(created_empty.status(), reqwest::StatusCode::BAD_REQUEST);
    let created_empty_json = created_empty
        .json::<serde_json::Value>()
        .await
        .expect("error payload should decode");
    assert!(
        created_empty_json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("cannot be empty"),
        "unexpected error payload: {created_empty_json}"
    );

    let get_missing = client
        .get(format!("http://{addr}/v1/programs/missing-program"))
        .send()
        .await
        .expect("get program should complete");
    assert_eq!(get_missing.status(), reqwest::StatusCode::NOT_FOUND);

    let rename_missing = client
        .patch(format!("http://{addr}/v1/programs/missing-program"))
        .json(&serde_json::json!({ "name": "renamed" }))
        .send()
        .await
        .expect("rename request should complete");
    assert_eq!(rename_missing.status(), reqwest::StatusCode::NOT_FOUND);

    let delete_missing = client
        .delete(format!("http://{addr}/v1/programs/missing-program"))
        .send()
        .await
        .expect("delete request should complete");
    assert_eq!(delete_missing.status(), reqwest::StatusCode::NOT_FOUND);

    let created = client
        .post(format!("http://{addr}/v1/programs"))
        .json(&serde_json::json!({ "name": "validation target" }))
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

    let rename_empty = client
        .patch(format!("http://{addr}/v1/programs/{program_id}"))
        .json(&serde_json::json!({ "name": "" }))
        .send()
        .await
        .expect("rename request should complete");
    assert_eq!(rename_empty.status(), reqwest::StatusCode::BAD_REQUEST);
    let rename_empty_json = rename_empty
        .json::<serde_json::Value>()
        .await
        .expect("error payload should decode");
    assert!(
        rename_empty_json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("cannot be empty"),
        "unexpected error payload: {rename_empty_json}"
    );

    let missing_version = client
        .get(format!(
            "http://{addr}/v1/programs/{program_id}/versions/99"
        ))
        .send()
        .await
        .expect("get version should complete");
    assert_eq!(missing_version.status(), reqwest::StatusCode::NOT_FOUND);

    let create_version_unknown_program = client
        .post(format!(
            "http://{addr}/v1/programs/missing-program/versions"
        ))
        .json(&serde_json::json!({
            "flavor": "rustscript",
            "nodes": [
                {
                    "id": "n1",
                    "block_id": "set_response_content",
                    "values": { "value": "hello" }
                }
            ],
            "edges": []
        }))
        .send()
        .await
        .expect("create version should complete");
    assert_eq!(
        create_version_unknown_program.status(),
        reqwest::StatusCode::NOT_FOUND
    );

    let create_version_missing_source = client
        .post(format!("http://{addr}/v1/programs/{program_id}/versions"))
        .json(&serde_json::json!({
            "flow_synced": false,
            "flavor": "rustscript"
        }))
        .send()
        .await
        .expect("create version should complete");
    assert_eq!(
        create_version_missing_source.status(),
        reqwest::StatusCode::BAD_REQUEST
    );
    let create_version_missing_source_json = create_version_missing_source
        .json::<serde_json::Value>()
        .await
        .expect("error payload should decode");
    assert!(
        create_version_missing_source_json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("source is required"),
        "unexpected error payload: {create_version_missing_source_json}"
    );

    let create_version_missing_nodes = client
        .post(format!("http://{addr}/v1/programs/{program_id}/versions"))
        .json(&serde_json::json!({
            "flow_synced": true,
            "flavor": "rustscript",
            "nodes": [],
            "edges": []
        }))
        .send()
        .await
        .expect("create version should complete");
    assert_eq!(
        create_version_missing_nodes.status(),
        reqwest::StatusCode::BAD_REQUEST
    );
    let create_version_missing_nodes_json = create_version_missing_nodes
        .json::<serde_json::Value>()
        .await
        .expect("error payload should decode");
    assert!(
        create_version_missing_nodes_json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("at least one node"),
        "unexpected error payload: {create_version_missing_nodes_json}"
    );

    handle.abort();
}
#[tokio::test]
async fn apply_program_version_enqueue_validates_program_and_flavor() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let apply_unknown = client
        .post(format!(
            "http://{addr}/v1/edges/dp-apply-errors/commands/apply-program-version"
        ))
        .json(&serde_json::json!({
            "program_id": "missing-program"
        }))
        .send()
        .await
        .expect("apply version should complete");
    assert_eq!(apply_unknown.status(), reqwest::StatusCode::NOT_FOUND);

    let created = client
        .post(format!("http://{addr}/v1/programs"))
        .json(&serde_json::json!({ "name": "apply validation target" }))
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

    let apply_without_versions = client
        .post(format!(
            "http://{addr}/v1/edges/dp-apply-errors/commands/apply-program-version"
        ))
        .json(&serde_json::json!({
            "program_id": program_id
        }))
        .send()
        .await
        .expect("apply version should complete");
    assert_eq!(
        apply_without_versions.status(),
        reqwest::StatusCode::BAD_REQUEST
    );
    let apply_without_versions_json = apply_without_versions
        .json::<serde_json::Value>()
        .await
        .expect("error payload should decode");
    assert!(
        apply_without_versions_json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("no versions"),
        "unexpected error payload: {apply_without_versions_json}"
    );

    let created_version = client
        .post(format!("http://{addr}/v1/programs/{program_id}/versions"))
        .json(&serde_json::json!({
            "flavor": "rustscript",
            "nodes": [
                {
                    "id": "n1",
                    "block_id": "set_response_content",
                    "values": { "value": "from version" }
                }
            ],
            "edges": []
        }))
        .send()
        .await
        .expect("create version should complete");
    assert_eq!(created_version.status(), reqwest::StatusCode::CREATED);

    let apply_invalid_flavor = client
        .post(format!(
            "http://{addr}/v1/edges/dp-apply-errors/commands/apply-program-version"
        ))
        .json(&serde_json::json!({
            "program_id": program_id,
            "flavor": "not-a-real-flavor"
        }))
        .send()
        .await
        .expect("apply version should complete");
    assert_eq!(
        apply_invalid_flavor.status(),
        reqwest::StatusCode::BAD_REQUEST
    );
    let apply_invalid_flavor_json = apply_invalid_flavor
        .json::<serde_json::Value>()
        .await
        .expect("error payload should decode");
    assert!(
        apply_invalid_flavor_json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("flavor"),
        "unexpected error payload: {apply_invalid_flavor_json}"
    );

    handle.abort();
}

#[tokio::test]
async fn binary_program_upload_rejects_invalid_content_type_and_large_payloads() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let bad_content_type = client
        .put(format!("http://{addr}/v1/edges/dp-upload-errors/program"))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .expect("upload request should complete");
    assert_eq!(
        bad_content_type.status(),
        reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    let bad_content_type_json = bad_content_type
        .json::<serde_json::Value>()
        .await
        .expect("error payload should decode");
    assert!(
        bad_content_type_json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("application/octet-stream"),
        "unexpected error payload: {bad_content_type_json}"
    );

    const MAX_UPLOAD_BYTES: usize = 8 * 1024 * 1024;
    let oversized_payload = vec![0_u8; MAX_UPLOAD_BYTES + 1];
    let oversized = client
        .put(format!("http://{addr}/v1/edges/dp-upload-errors/program"))
        .header("content-type", "application/octet-stream")
        .body(oversized_payload)
        .send()
        .await
        .expect("upload request should complete");
    assert_eq!(oversized.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);

    let oversize_limit_error_payload = vec![0_u8; MAX_UPLOAD_BYTES + 2];
    let oversize_limit_error = client
        .put(format!("http://{addr}/v1/edges/dp-upload-errors/program"))
        .header("content-type", "application/octet-stream")
        .body(oversize_limit_error_payload)
        .send()
        .await
        .expect("upload request should complete");
    assert_eq!(
        oversize_limit_error.status(),
        reqwest::StatusCode::PAYLOAD_TOO_LARGE
    );

    handle.abort();
}
