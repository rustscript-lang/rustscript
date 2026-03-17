use super::support::*;

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
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("get_request_method"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("set_request_path"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("clear_response_header"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("rate_limit_allow"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("get_request_headers"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("set_request_query_arg"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("get_request_body_next_chunk"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("get_request_body_eof"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("runtime_sleep"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("json_encode"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("json_decode"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("get_response_headers"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("http_exchange_new"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("tcp_stream_new"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("tls_session_from_socket"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("websocket_connection_new"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("webrtc_connection_new"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("udp_socket_new"))
    );
    assert!(
        blocks_json["blocks"]
            .as_array()
            .expect("blocks should be an array")
            .iter()
            .any(|item| item["id"].as_str() == Some("proxy_stream_exchange"))
    );
    let blocks = blocks_json["blocks"]
        .as_array()
        .expect("blocks should be an array");
    for removed_id in [
        "get_request_raw_query",
        "remove_request_header",
        "set_request_headers",
        "set_request_raw_query",
        "remove_response_header",
        "set_response_headers",
    ] {
        assert!(
            !blocks
                .iter()
                .any(|item| item["id"].as_str() == Some(removed_id)),
            "{removed_id} block should not exist"
        );
    }
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
    let get_request_method = blocks
        .iter()
        .find(|item| item["id"].as_str() == Some("get_request_method"))
        .expect("get_request_method block should exist");
    assert_eq!(
        get_request_method["category"].as_str(),
        Some("http_request"),
        "get_request_method should be request-http scoped"
    );
    let clear_response_header = blocks
        .iter()
        .find(|item| item["id"].as_str() == Some("clear_response_header"))
        .expect("clear_response_header block should exist");
    assert_eq!(
        clear_response_header["category"].as_str(),
        Some("http_response"),
        "clear_response_header should be response-http scoped"
    );
    let runtime_sleep = blocks
        .iter()
        .find(|item| item["id"].as_str() == Some("runtime_sleep"))
        .expect("runtime_sleep block should exist");
    assert_eq!(
        runtime_sleep["category"].as_str(),
        Some("runtime"),
        "runtime_sleep should be runtime scoped"
    );
    let json_encode = blocks
        .iter()
        .find(|item| item["id"].as_str() == Some("json_encode"))
        .expect("json_encode block should exist");
    assert_eq!(
        json_encode["category"].as_str(),
        Some("json"),
        "json_encode should be json scoped"
    );
    let http_exchange_new = blocks
        .iter()
        .find(|item| item["id"].as_str() == Some("http_exchange_new"))
        .expect("http_exchange_new block should exist");
    assert_eq!(
        http_exchange_new["category"].as_str(),
        Some("http_exchange"),
        "http_exchange_new should be exchange scoped"
    );
    let tcp_stream_new = blocks
        .iter()
        .find(|item| item["id"].as_str() == Some("tcp_stream_new"))
        .expect("tcp_stream_new block should exist");
    assert_eq!(
        tcp_stream_new["category"].as_str(),
        Some("tcp_stream"),
        "tcp_stream_new should be tcp scoped"
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
        (
            "rustscript",
            "dp-ui-rss",
            "use vm;",
            reqwest::StatusCode::ACCEPTED,
        ),
        (
            "javascript",
            "dp-ui-js",
            "import * as vm from \"vm\";",
            reqwest::StatusCode::ACCEPTED,
        ),
        (
            "lua",
            "dp-ui-lua",
            "local vm = require(\"vm\")",
            reqwest::StatusCode::ACCEPTED,
        ),
        (
            "scheme",
            "dp-ui-scm",
            "(require (prefix-in vm. \"vm\"))",
            reqwest::StatusCode::ACCEPTED,
        ),
    ];

    for (flavor, edge_id, expected_prelude, expected_status) in flavors {
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
            expected_status,
            "unexpected deploy status for flavor {flavor}"
        );

        let payload = deploy
            .json::<serde_json::Value>()
            .await
            .expect("deploy payload should decode");
        if expected_status == reqwest::StatusCode::ACCEPTED {
            let source = payload["source"][flavor]
                .as_str()
                .expect("flavor source should be present");
            assert!(
                source.contains(expected_prelude),
                "generated source should include vm prelude for flavor {flavor}, got: {source}"
            );
        } else {
            let message = payload["error"]
                .as_str()
                .expect("error payload should include message");
            assert!(
                message.contains("source compile failed"),
                "deploy should fail during compile, got: {message}"
            );
        }
    }

    handle.abort();
}

#[tokio::test]
async fn ui_deploy_compiles_edge_stdlib_wrapper_blocks() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let deploy = client
        .post(format!("http://{addr}/v1/ui/deploy"))
        .json(&serde_json::json!({
            "edge_id": "dp-ui-stdlib-wrapper",
            "flavor": "rustscript",
            "blocks": [
                {
                    "block_id": "set_upstream",
                    "values": {
                        "host": "127.0.0.1",
                        "port": "8088"
                    }
                },
                {
                    "block_id": "http_upstream_as_stream",
                    "values": {
                        "var": "upstream_stream"
                    }
                }
            ]
        }))
        .send()
        .await
        .expect("deploy request should complete");

    assert_eq!(deploy.status(), reqwest::StatusCode::ACCEPTED);
    let payload = deploy
        .json::<serde_json::Value>()
        .await
        .expect("deploy payload should decode");
    let rustscript = payload["source"]["rustscript"]
        .as_str()
        .expect("rustscript source should be present");
    assert!(rustscript.contains("use edge::http::upstream as upstream;"));
    assert!(rustscript.contains("use edge::http::upstream::request as upstream_request;"));
    assert!(rustscript.contains("upstream_request::set_target(\"127.0.0.1\", 8088);"));
    assert!(rustscript.contains("let upstream_stream = upstream::as_stream();"));

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
        rustscript.contains("let joined_len = (joined).length;"),
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
        rustscript.contains("let mut items_with_msg = (items).copy();"),
        "expected borrow-safe array_push clone line, got: {rustscript}"
    );
    assert!(
        rustscript.contains("items_with_msg[items_with_msg.length] = joined;"),
        "expected array_push append line, got: {rustscript}"
    );
    assert!(
        rustscript.contains("let first_item = ((items_with_msg)[0]).copy();"),
        "expected borrow-safe array_get line, got: {rustscript}"
    );
    assert!(
        rustscript.contains("let result_map = {};"),
        "expected map_new line, got: {rustscript}"
    );
    assert!(
        rustscript.contains("let mut result_map_next = (result_map).copy();"),
        "expected borrow-safe map_set clone line, got: {rustscript}"
    );
    assert!(
        rustscript.contains("result_map_next.body = first_item;"),
        "expected map_set assignment line, got: {rustscript}"
    );
    assert!(
        rustscript.contains("let response_body = ((result_map_next).body).copy();"),
        "expected borrow-safe map_get access line, got: {rustscript}"
    );
    assert!(
        !rustscript.contains("array_push("),
        "expected array_push line, got: {rustscript}"
    );
    assert!(
        rustscript.contains("vm::http::response::set_body(status_plus_len);"),
        "expected data edge into flow action, got: {rustscript}"
    );
    if let Err(err) = compile_source_with_flavor(rustscript, SourceFlavor::RustScript) {
        panic!("expected generated rustscript to compile, got: {err}\nsource:\n{rustscript}");
    }

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
        rustscript.contains("vm::http::response::set_body(client_id);"),
        "expected downstream value to reference connected identifier, got: {rustscript}"
    );

    handle.abort();
}

#[tokio::test]
async fn ui_render_set_upstream_uses_connected_identifier_expression() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let render = client
        .post(format!("http://{addr}/v1/ui/render"))
        .json(&serde_json::json!({
            "nodes": [
                {
                    "id": "n1",
                    "block_id": "get_header",
                    "values": { "var": "target_upstream", "name": "x-upstream" }
                },
                {
                    "id": "n2",
                    "block_id": "set_upstream",
                    "values": { "host": "127.0.0.1", "port": "8088" }
                },
                {
                    "id": "n3",
                    "block_id": "set_response_content",
                    "values": { "value": "proxied" }
                }
            ],
            "edges": [
                {
                    "source": "n1",
                    "source_output": "value",
                    "target": "n2",
                    "target_input": "host"
                },
                {
                    "source": "n2",
                    "source_output": "next",
                    "target": "n3",
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
        rustscript.contains("use edge::http::upstream::request as upstream_request;"),
        "expected upstream wrapper import in rustscript, got: {rustscript}"
    );
    assert!(
        rustscript.contains("upstream_request::set_target(target_upstream, 8088);"),
        "expected set_upstream to use connected identifier in rustscript, got: {rustscript}"
    );
    assert!(
        !rustscript.contains("upstream_request::set_target(\"$target_upstream\", 8088);"),
        "set_upstream should not treat connected identifier as quoted literal, got: {rustscript}"
    );

    handle.abort();
}

#[tokio::test]
async fn ui_render_new_http_blocks_generate_expected_calls() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let render = client
        .post(format!("http://{addr}/v1/ui/render"))
        .json(&serde_json::json!({
            "blocks": [
                { "block_id": "const_string", "values": { "var": "method_next", "value": "PATCH" } },
                { "block_id": "const_string", "values": { "var": "path_next", "value": "/rewritten" } },
                { "block_id": "const_string", "values": { "var": "query_next", "value": "from=ui" } },
                { "block_id": "get_request_id", "values": { "var": "req_id" } },
                { "block_id": "get_request_method", "values": { "var": "req_method" } },
                { "block_id": "get_request_path", "values": { "var": "req_path" } },
                { "block_id": "get_request_query", "values": { "var": "req_query" } },
                { "block_id": "get_request_path_with_query", "values": { "var": "req_path_query" } },
                { "block_id": "get_request_query_arg", "values": { "var": "req_token", "name": "token" } },
                { "block_id": "get_request_query_args", "values": { "var": "req_query_args" } },
                { "block_id": "get_request_scheme", "values": { "var": "req_scheme" } },
                { "block_id": "get_request_host", "values": { "var": "req_host" } },
                { "block_id": "get_request_http_version", "values": { "var": "req_http_version" } },
                { "block_id": "get_request_port", "values": { "var": "req_port" } },
                { "block_id": "get_request_client_ip", "values": { "var": "req_ip" } },
                { "block_id": "get_request_body", "values": { "var": "req_body" } },
                { "block_id": "get_request_body_next_chunk", "values": { "var": "req_chunk", "max_bytes": "8" } },
                { "block_id": "get_request_body_eof", "values": { "var": "req_body_done" } },
                { "block_id": "get_request_headers", "values": { "var": "req_headers" } },
                { "block_id": "set_request_header", "values": { "name": "x-added", "value": "yes" } },
                { "block_id": "add_request_header", "values": { "name": "x-added", "value": "yes-2" } },
                { "block_id": "clear_request_header", "values": { "name": "x-clear" } },
                { "block_id": "set_request_method", "values": { "method": "$method_next" } },
                { "block_id": "set_request_path", "values": { "path": "$path_next" } },
                { "block_id": "set_request_query", "values": { "query": "$query_next" } },
                { "block_id": "set_request_query_arg", "values": { "name": "token", "value": "$req_id" } },
                { "block_id": "set_request_body", "values": { "value": "$req_body" } },
                { "block_id": "set_header", "values": { "name": "x-vm", "value": "$req_method" } },
                { "block_id": "runtime_sleep", "values": { "millis": "5" } },
                { "block_id": "clear_response_header", "values": { "name": "x-clear" } },
                { "block_id": "add_response_header", "values": { "name": "set-cookie", "value": "a=1" } },
                { "block_id": "get_response_status", "values": { "var": "resp_status" } },
                { "block_id": "get_response_header", "values": { "var": "resp_header", "name": "x-vm" } },
                { "block_id": "get_response_headers", "values": { "var": "resp_headers" } },
                { "block_id": "get_response_body", "values": { "var": "resp_body" } },
                {
                    "block_id": "rate_limit_allow",
                    "values": { "var": "allowed", "key_expr": "$req_id", "limit": "5", "window_seconds": "60" }
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
    assert!(rustscript.contains("use edge::http::upstream::request as upstream_request;"));
    assert!(rustscript.contains("use runtime;"));
    assert!(rustscript.contains("use rate_limit;"));
    assert!(rustscript.contains("let req_id = vm::http::request::get_id();"));
    assert!(rustscript.contains("let req_method = vm::http::request::get_method();"));
    assert!(rustscript.contains("let req_path = vm::http::request::get_path();"));
    assert!(rustscript.contains("let req_query = vm::http::request::get_query();"));
    assert!(rustscript.contains("let req_path_query = vm::http::request::get_path_with_query();"));
    assert!(rustscript.contains("let req_token = vm::http::request::get_query_arg(\"token\");"));
    assert!(rustscript.contains("let req_query_args = vm::http::request::get_query_args();"));
    assert!(rustscript.contains("let req_scheme = vm::http::request::get_scheme();"));
    assert!(rustscript.contains("let req_host = vm::http::request::get_host();"));
    assert!(rustscript.contains("let req_http_version = vm::http::request::get_http_version();"));
    assert!(rustscript.contains("let req_port = vm::http::request::get_port();"));
    assert!(rustscript.contains("let req_ip = vm::http::request::get_client_ip();"));
    assert!(rustscript.contains("let req_body = vm::http::request::get_body();"));
    assert!(rustscript.contains("let req_chunk = vm::http::request::body::next_chunk(8);"));
    assert!(rustscript.contains("let req_body_done = vm::http::request::body::eof();"));
    assert!(rustscript.contains("let req_headers = vm::http::request::get_headers();"));
    assert!(rustscript.contains("upstream_request::set_header(\"x-added\", \"yes\");"));
    assert!(rustscript.contains("upstream_request::add_header(\"x-added\", \"yes-2\");"));
    assert!(rustscript.contains("upstream_request::clear_header(\"x-clear\");"));
    assert!(rustscript.contains("upstream_request::set_method(method_next);"));
    assert!(rustscript.contains("upstream_request::set_path(path_next);"));
    assert!(rustscript.contains("upstream_request::set_query(query_next);"));
    assert!(rustscript.contains("upstream_request::set_query_arg(\"token\", req_id);"));
    assert!(rustscript.contains("upstream_request::set_body(req_body);"));
    assert!(rustscript.contains("vm::http::response::set_header(\"x-vm\", req_method);"));
    assert!(rustscript.contains("runtime::sleep(5);"));
    assert!(rustscript.contains("vm::http::response::clear_header(\"x-clear\");"));
    assert!(rustscript.contains("vm::http::response::add_header(\"set-cookie\", \"a=1\");"));
    assert!(rustscript.contains("let resp_status = vm::http::response::get_status();"));
    assert!(rustscript.contains("let resp_header = vm::http::response::get_header(\"x-vm\");"));
    assert!(rustscript.contains("let resp_headers = vm::http::response::get_headers();"));
    assert!(rustscript.contains("let resp_body = vm::http::response::get_body();"));
    assert!(rustscript.contains("let allowed = rate_limit::allow(req_id, 5, 60);"));

    let javascript = render_json["source"]["javascript"]
        .as_str()
        .expect("javascript source should be a string");
    assert!(
        javascript
            .contains("import * as upstream_request from \"edge/http/upstream/request.rss\";")
    );
    assert!(javascript.contains("upstream_request.set_path(path_next);"));
    assert!(javascript.contains("vm.http.response.clear_header(\"x-clear\");"));

    handle.abort();
}

#[tokio::test]
async fn ui_render_json_blocks_generate_expected_calls() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let render = client
        .post(format!("http://{addr}/v1/ui/render"))
        .json(&serde_json::json!({
            "blocks": [
                {
                    "block_id": "const_string",
                    "values": { "var": "payload_json", "value": "{\"ok\":true,\"n\":2}" }
                },
                {
                    "block_id": "json_decode",
                    "values": { "var": "payload", "value": "$payload_json" }
                },
                {
                    "block_id": "json_encode",
                    "values": { "var": "payload_json_out", "value": "$payload" }
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
    assert!(rustscript.contains("use json;"));
    assert!(rustscript.contains("let payload = json::decode(payload_json);"));
    assert!(rustscript.contains("let payload_json_out = json::encode(payload);"));

    let javascript = render_json["source"]["javascript"]
        .as_str()
        .expect("javascript source should be a string");
    assert!(javascript.contains("import * as json from \"json\";"));
    assert!(javascript.contains("let payload = json.decode(payload_json);"));
    assert!(javascript.contains("let payload_json_out = json.encode(payload);"));

    let lua = render_json["source"]["lua"]
        .as_str()
        .expect("lua source should be a string");
    assert!(lua.contains("local json = require(\"json\")"));
    assert!(lua.contains("local payload = json.decode(payload_json)"));
    assert!(lua.contains("local payload_json_out = json.encode(payload)"));

    let scheme = render_json["source"]["scheme"]
        .as_str()
        .expect("scheme source should be a string");
    assert!(scheme.contains("(require (prefix-in json. \"json\"))"));
    assert!(scheme.contains("(define payload (json.decode payload_json))"));
    assert!(scheme.contains("(define payload_json_out (json.encode payload))"));

    handle.abort();
}

#[tokio::test]
async fn ui_render_extended_abi_blocks_generate_expected_calls() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let render = client
        .post(format!("http://{addr}/v1/ui/render"))
        .json(&serde_json::json!({
            "blocks": [
                { "block_id": "http_exchange_new", "values": { "var": "exchange" } },
                { "block_id": "http_exchange_set_target", "values": { "exchange": "$exchange", "target": "127.0.0.1:8080" } },
                { "block_id": "http_exchange_send", "values": { "exchange": "$exchange" } },
                { "block_id": "http_exchange_get_status", "values": { "var": "exchange_status", "exchange": "$exchange" } },
                { "block_id": "tcp_stream_new", "values": { "var": "stream" } },
                { "block_id": "tcp_stream_set_target", "values": { "stream": "$stream", "target": "127.0.0.1:9000" } },
                { "block_id": "tcp_stream_connect", "values": { "var": "connected", "stream": "$stream" } },
                { "block_id": "tls_session_from_socket", "values": { "var": "session", "stream": "$stream" } },
                { "block_id": "tls_session_set_verify", "values": { "session": "$session", "verify": "false" } },
                { "block_id": "websocket_connection_new", "values": { "var": "ws" } },
                { "block_id": "websocket_connection_set_target", "values": { "connection": "$ws", "target": "ws://127.0.0.1:8081" } },
                { "block_id": "webrtc_connection_new", "values": { "var": "rtc" } },
                { "block_id": "udp_socket_new", "values": { "var": "udp" } },
                { "block_id": "proxy_stream_exchange", "values": { "var": "proxy_exchange", "exchange": "$exchange" } },
                { "block_id": "http_upstream_as_stream", "values": { "var": "upstream_proxy" } },
                { "block_id": "read_upstream_response_all", "values": { "var": "upstream_all" } }
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
    assert!(rustscript.contains("use edge::http::upstream as upstream;"));
    assert!(rustscript.contains("use edge::http::upstream::response as upstream_response;"));
    assert!(rustscript.contains("let exchange = vm::http::exchange::new();"));
    assert!(rustscript.contains("vm::http::exchange::set_target(exchange, \"127.0.0.1:8080\");"));
    assert!(rustscript.contains("vm::http::exchange::send(exchange);"));
    assert!(rustscript.contains("let exchange_status = vm::http::exchange::get_status(exchange);"));
    assert!(rustscript.contains("let stream = vm::tcp::stream::new();"));
    assert!(rustscript.contains("vm::tcp::stream::set_target(stream, \"127.0.0.1:9000\");"));
    assert!(rustscript.contains("let connected = vm::tcp::stream::connect(stream);"));
    assert!(rustscript.contains("let session = vm::tls::session::from_socket(stream);"));
    assert!(rustscript.contains("vm::tls::session::set_verify(session, false);"));
    assert!(rustscript.contains("let ws = vm::websocket::connection::new();"));
    assert!(
        rustscript.contains("vm::websocket::connection::set_target(ws, \"ws://127.0.0.1:8081\");")
    );
    assert!(rustscript.contains("let rtc = vm::webrtc::connection::new();"));
    assert!(rustscript.contains("let udp = vm::udp::socket::new();"));
    assert!(rustscript.contains("let proxy_exchange = vm::proxy::stream::exchange(exchange);"));
    assert!(rustscript.contains("let upstream_proxy = upstream::as_stream();"));
    assert!(rustscript.contains("let upstream_all = upstream_response::read_all();"));
    if let Err(err) = edge::compile_edge_source_with_flavor(rustscript, SourceFlavor::RustScript) {
        panic!("expected rustscript ABI render to compile, got: {err}\nsource:\n{rustscript}");
    }

    let javascript = render_json["source"]["javascript"]
        .as_str()
        .expect("javascript source should be a string");
    assert!(javascript.contains("import * as upstream from \"edge/http/upstream.rss\";"));
    assert!(
        javascript
            .contains("import * as upstream_response from \"edge/http/upstream/response.rss\";")
    );
    assert!(javascript.contains("let exchange = vm.http.exchange.new();"));
    assert!(javascript.contains("let upstream_proxy = upstream.as_stream();"));

    handle.abort();
}

#[tokio::test]
async fn ui_render_extended_abi_flow_blocks_work_with_graph() {
    let (addr, handle, _state) = spawn_controller(ControllerConfig::default()).await;
    let client = reqwest::Client::new();

    let render = client
        .post(format!("http://{addr}/v1/ui/render"))
        .json(&serde_json::json!({
            "nodes": [
                { "id": "n1", "block_id": "tcp_stream_new", "values": { "var": "stream" } },
                { "id": "n2", "block_id": "tcp_stream_set_target", "values": { "stream": "1", "target": "127.0.0.1:9000" } },
                { "id": "n3", "block_id": "tcp_stream_connect", "values": { "var": "connected", "stream": "1" } },
                { "id": "n4", "block_id": "set_response_status", "values": { "status": "204" } }
            ],
            "edges": [
                { "source": "n1", "source_output": "value", "target": "n2", "target_input": "stream" },
                { "source": "n1", "source_output": "value", "target": "n3", "target_input": "stream" },
                { "source": "n2", "source_output": "next", "target": "n3", "target_input": "__flow" },
                { "source": "n3", "source_output": "next", "target": "n4", "target_input": "__flow" }
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
    let stream_pos = rustscript
        .find("let stream = vm::tcp::stream::new();")
        .expect("stream constructor should be rendered");
    let target_pos = rustscript
        .find("vm::tcp::stream::set_target(stream, \"127.0.0.1:9000\");")
        .expect("set_target should be rendered");
    let connect_pos = rustscript
        .find("let connected = vm::tcp::stream::connect(stream);")
        .expect("connect should be rendered");
    let status_pos = rustscript
        .find("vm::http::response::set_status(204);")
        .expect("status action should be rendered");

    assert!(
        stream_pos < target_pos,
        "stream should be created before target set: {rustscript}"
    );
    assert!(
        target_pos < connect_pos,
        "target should be set before connect: {rustscript}"
    );
    assert!(
        connect_pos < status_pos,
        "connect should occur before final action: {rustscript}"
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
        rustscript.contains("if rate_limit::allow(client_id, 3, 60) {"),
        "expected rate limit branch in rustscript, got: {rustscript}"
    );
    assert!(
        rustscript.contains("vm::http::response::set_status(429);"),
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
        rustscript.contains("for (let mut i = 0; i < 2; i = i + 1) {"),
        "expected plain loop in rustscript, got: {rustscript}"
    );
    assert!(
        rustscript.contains("vm::http::response::set_header(\"x-loop\", \"tick\");"),
        "expected loop body action in rustscript, got: {rustscript}"
    );
    assert!(
        rustscript.contains("vm::http::response::set_body(\"if true done\");"),
        "expected loop done action in rustscript, got: {rustscript}"
    );
    assert!(
        rustscript.contains("vm::http::response::set_status(403);"),
        "expected if false branch action in rustscript, got: {rustscript}"
    );
    if let Err(err) = compile_source_with_flavor(rustscript, SourceFlavor::RustScript) {
        panic!("expected flow rustscript to compile, got: {err}\nsource:\n{rustscript}");
    }

    let javascript = render_json["source"]["javascript"]
        .as_str()
        .expect("javascript source should be a string");
    assert!(
        javascript.contains("for (let i = 0; i < 2; i = i + 1) {"),
        "expected plain loop in javascript, got: {javascript}"
    );

    let lua = render_json["source"]["lua"]
        .as_str()
        .expect("lua source should be a string");
    assert!(
        lua.contains("for i = 1, 2, 1 do"),
        "expected explicit-step loop in lua, got: {lua}"
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
