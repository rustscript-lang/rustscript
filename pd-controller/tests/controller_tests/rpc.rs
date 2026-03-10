use super::support::*;

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
            upstream_latency_p50_ms: 7,
            upstream_latency_p90_ms: 15,
            upstream_latency_p99_ms: 21,
            edge_latency_p50_ms: 3,
            edge_latency_p90_ms: 5,
            edge_latency_p99_ms: 9,
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
            upstream_latency_p50_ms: 7,
            upstream_latency_p90_ms: 15,
            upstream_latency_p99_ms: 21,
            edge_latency_p50_ms: 3,
            edge_latency_p90_ms: 5,
            edge_latency_p99_ms: 9,
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
            upstream_latency_p50_ms: 8,
            upstream_latency_p90_ms: 16,
            upstream_latency_p99_ms: 24,
            edge_latency_p50_ms: 3,
            edge_latency_p90_ms: 6,
            edge_latency_p99_ms: 9,
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
    assert_eq!(detail_body.traffic_series[1].upstream_latency_p50_ms, 8);
    assert_eq!(detail_body.traffic_series[1].upstream_latency_p90_ms, 16);
    assert_eq!(detail_body.traffic_series[1].upstream_latency_p99_ms, 24);
    assert_eq!(detail_body.traffic_series[1].edge_latency_p50_ms, 3);
    assert_eq!(detail_body.traffic_series[1].edge_latency_p90_ms, 6);
    assert_eq!(detail_body.traffic_series[1].edge_latency_p99_ms, 9);

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
