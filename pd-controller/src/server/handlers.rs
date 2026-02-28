use super::*;

pub(super) async fn access_log_middleware(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let uri = request.uri().to_string();
    let started = Instant::now();
    let response = next.run(request).await;
    let status = response.status();
    let elapsed_ms = started.elapsed().as_millis();
    if path != "/rpc/v1/edge/poll" {
        info!(
            method = %method,
            uri = %uri,
            status = status.as_u16(),
            elapsed_ms = elapsed_ms,
            "http access"
        );
    }
    response
}

pub(super) async fn healthz_handler() -> Json<StatusResponse> {
    Json(StatusResponse { status: "ok" })
}

pub(super) async fn metrics_handler(State(state): State<ControllerState>) -> impl IntoResponse {
    let (connected_edges, pending_commands) = {
        let guard = state.inner.read().await;
        let connected_edges = guard.edges.len();
        let pending_commands = guard
            .edges
            .values()
            .map(|record| record.pending_commands.len())
            .sum::<usize>();
        (connected_edges, pending_commands)
    };
    let metrics = format!(
        concat!(
            "pd_controller_uptime_seconds {}\n",
            "pd_controller_connected_edges {}\n",
            "pd_controller_pending_commands {}\n",
            "pd_controller_poll_requests_total {}\n",
            "pd_controller_result_posts_total {}\n",
            "pd_controller_commands_enqueued_total {}\n",
            "pd_controller_commands_delivered_total {}\n",
            "pd_controller_command_results_ok_total {}\n",
            "pd_controller_command_results_error_total {}\n"
        ),
        state.metrics.started_at.elapsed().as_secs(),
        connected_edges,
        pending_commands,
        state.metrics.poll_requests_total.load(Ordering::Relaxed),
        state.metrics.result_posts_total.load(Ordering::Relaxed),
        state
            .metrics
            .commands_enqueued_total
            .load(Ordering::Relaxed),
        state
            .metrics
            .commands_delivered_total
            .load(Ordering::Relaxed),
        state
            .metrics
            .command_results_ok_total
            .load(Ordering::Relaxed),
        state
            .metrics
            .command_results_error_total
            .load(Ordering::Relaxed),
    );
    (
        StatusCode::OK,
        [(CONTENT_TYPE, "text/plain; version=0.0.4")],
        metrics,
    )
}

pub(super) async fn ui_index_handler() -> impl IntoResponse {
    ui_asset_response("index.html")
}

pub(super) async fn ui_asset_handler(Path(path): Path<String>) -> impl IntoResponse {
    let normalized = path.trim_start_matches('/');
    if normalized.is_empty() {
        return ui_asset_response("index.html");
    }
    ui_asset_response(normalized)
}

fn ui_asset_response(path: &str) -> axum::response::Response {
    if let Some(bytes) = embedded_webui::get_asset(path) {
        return (
            StatusCode::OK,
            [(CONTENT_TYPE, webui_content_type(path))],
            bytes.to_vec(),
        )
            .into_response();
    }

    if let Some(index) = embedded_webui::get_asset("index.html") {
        return (
            StatusCode::OK,
            [(CONTENT_TYPE, "text/html; charset=utf-8")],
            index.to_vec(),
        )
            .into_response();
    }

    let message = if embedded_webui::has_assets() {
        "webui asset not found"
    } else {
        "webui assets are not embedded; build pd-controller/webui before compiling pd-controller"
    };
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: message.to_string(),
        }),
    )
        .into_response()
}

pub(super) async fn ui_blocks_handler() -> Json<UiBlocksResponse> {
    Json(UiBlocksResponse {
        blocks: ui_block_catalog(),
    })
}

pub(super) async fn ui_render_handler(
    Json(request): Json<UiRenderRequest>,
) -> Result<Json<UiRenderResponse>, (StatusCode, Json<ErrorResponse>)> {
    let source = render_ui_sources(&request.blocks, &request.nodes, &request.edges)?;
    Ok(Json(UiRenderResponse { source }))
}

pub(super) async fn ui_deploy_handler(
    State(state): State<ControllerState>,
    Json(request): Json<UiDeployRequest>,
) -> Result<(StatusCode, Json<UiDeployResponse>), (StatusCode, Json<ErrorResponse>)> {
    if request.edge_id.trim().is_empty() {
        return Err(bad_request("edge_id cannot be empty"));
    }

    let source = render_ui_sources(&request.blocks, &request.nodes, &request.edges)?;
    let (flavor, flavor_label) = parse_ui_flavor(request.flavor.as_deref())?;
    let source_text = source_for_flavor(&source, flavor);
    let compiled = compile_source_with_flavor(&source_text, flavor)
        .map_err(|err| bad_request(&format!("source compile failed: {err}")))?;
    let program_bytes = encode_program(&compiled.program)
        .map_err(|err| bad_request(&format!("bytecode encode failed: {err}")))?;

    let command = ControlPlaneCommand::ApplyProgram {
        command_id: state.next_command_id(),
        program_base64: STANDARD.encode(program_bytes),
    };
    let queued = state.enqueue_command(request.edge_id, command).await;
    let response = UiDeployResponse {
        command_id: queued.command_id,
        pending_commands: queued.pending_commands,
        flavor: flavor_label.to_string(),
        source,
    };
    Ok((StatusCode::ACCEPTED, Json(response)))
}

pub(super) async fn list_programs_handler(
    State(state): State<ControllerState>,
) -> Json<ProgramListResponse> {
    let mut programs = {
        let guard = state.inner.read().await;
        guard
            .programs
            .values()
            .map(map_program_summary)
            .collect::<Vec<_>>()
    };
    programs.sort_by(|lhs, rhs| lhs.name.cmp(&rhs.name));
    Json(ProgramListResponse { programs })
}

pub(super) async fn create_program_handler(
    State(state): State<ControllerState>,
    Json(request): Json<CreateProgramRequest>,
) -> Result<(StatusCode, Json<ProgramDetailResponse>), (StatusCode, Json<ErrorResponse>)> {
    if request.name.trim().is_empty() {
        return Err(bad_request("program name cannot be empty"));
    }
    let now = now_unix_ms();
    let program = StoredProgram {
        program_id: state.next_program_id(),
        name: request.name.trim().to_string(),
        created_unix_ms: now,
        updated_unix_ms: now,
        versions: Vec::new(),
    };
    let response = {
        let mut guard = state.inner.write().await;
        let detail = map_program_detail(&program);
        guard.programs.insert(program.program_id.clone(), program);
        detail
    };
    state.persist_snapshot().await.map_err(internal_error)?;
    Ok((StatusCode::CREATED, Json(response)))
}

pub(super) async fn get_program_handler(
    State(state): State<ControllerState>,
    Path(program_id): Path<String>,
) -> Result<Json<ProgramDetailResponse>, (StatusCode, Json<ErrorResponse>)> {
    let detail = {
        let guard = state.inner.read().await;
        let Some(program) = guard.programs.get(&program_id) else {
            return Err(not_found("program not found"));
        };
        map_program_detail(program)
    };
    Ok(Json(detail))
}

pub(super) async fn rename_program_handler(
    State(state): State<ControllerState>,
    Path(program_id): Path<String>,
    Json(request): Json<RenameProgramRequest>,
) -> Result<Json<ProgramDetailResponse>, (StatusCode, Json<ErrorResponse>)> {
    if request.name.trim().is_empty() {
        return Err(bad_request("program name cannot be empty"));
    }
    let detail = {
        let mut guard = state.inner.write().await;
        let Some(program) = guard.programs.get_mut(&program_id) else {
            return Err(not_found("program not found"));
        };
        program.name = request.name.trim().to_string();
        program.updated_unix_ms = now_unix_ms();
        map_program_detail(program)
    };
    state.persist_snapshot().await.map_err(internal_error)?;
    Ok(Json(detail))
}

pub(super) async fn delete_program_handler(
    State(state): State<ControllerState>,
    Path(program_id): Path<String>,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    {
        let mut guard = state.inner.write().await;
        if guard.programs.remove(&program_id).is_none() {
            return Err(not_found("program not found"));
        }
        for record in guard.edges.values_mut() {
            if record
                .applied_program
                .as_ref()
                .map(|applied| applied.program_id == program_id)
                .unwrap_or(false)
            {
                record.applied_program = None;
            }
            record
                .pending_apply_programs
                .retain(|_, applied| applied.program_id != program_id);
        }
    }
    state.persist_snapshot().await.map_err(internal_error)?;
    Ok(Json(StatusResponse { status: "deleted" }))
}

pub(super) async fn create_program_version_handler(
    State(state): State<ControllerState>,
    Path(program_id): Path<String>,
    Json(request): Json<CreateProgramVersionRequest>,
) -> Result<(StatusCode, Json<ProgramVersionResponse>), (StatusCode, Json<ErrorResponse>)> {
    // Backward/compat guard: if client sends source-only payload with no graph,
    // accept it as code-only save even when flow_synced is absent/stale.
    let inferred_code_only =
        request.source.is_some() && request.nodes.is_empty() && request.blocks.is_empty();
    let flow_synced = if inferred_code_only {
        false
    } else {
        request.flow_synced
    };
    let (nodes, edges, source) = if flow_synced {
        let nodes = if !request.nodes.is_empty() {
            request.nodes.clone()
        } else {
            request
                .blocks
                .iter()
                .enumerate()
                .map(|(index, block)| UiGraphNode {
                    id: format!("b{}", index + 1),
                    block_id: block.block_id.clone(),
                    values: block.values.clone(),
                    position: None,
                })
                .collect::<Vec<_>>()
        };
        let edges = request.edges.clone();
        if nodes.is_empty() {
            return Err(bad_request(
                "program version must include at least one node",
            ));
        }
        let source = render_ui_sources(&request.blocks, &nodes, &edges)?;
        (nodes, edges, source)
    } else {
        let Some(source) = request.source.clone() else {
            return Err(bad_request("source is required when flow_synced is false"));
        };
        (Vec::new(), Vec::new(), source)
    };
    let (_, flavor_label) = parse_ui_flavor(request.flavor.as_deref())?;
    let detail = {
        let mut guard = state.inner.write().await;
        let Some(program) = guard.programs.get_mut(&program_id) else {
            return Err(not_found("program not found"));
        };
        let version = (program.versions.len() as u32) + 1;
        let created_unix_ms = now_unix_ms();
        let stored_version = StoredProgramVersion {
            version,
            created_unix_ms,
            flavor: flavor_label.to_string(),
            flow_synced,
            nodes: nodes.clone(),
            edges: edges.clone(),
            source: source.clone(),
        };
        program.versions.push(stored_version.clone());
        program.updated_unix_ms = created_unix_ms;
        ProgramVersionResponse {
            program_id: program.program_id.clone(),
            name: program.name.clone(),
            detail: map_program_version_detail(&stored_version),
        }
    };
    state.persist_snapshot().await.map_err(internal_error)?;
    Ok((StatusCode::CREATED, Json(detail)))
}

pub(super) async fn get_program_version_handler(
    State(state): State<ControllerState>,
    Path((program_id, version)): Path<(String, u32)>,
) -> Result<Json<ProgramVersionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let response = {
        let guard = state.inner.read().await;
        let Some(program) = guard.programs.get(&program_id) else {
            return Err(not_found("program not found"));
        };
        let Some(stored_version) = program.versions.iter().find(|item| item.version == version)
        else {
            return Err(not_found("program version not found"));
        };
        ProgramVersionResponse {
            program_id: program.program_id.clone(),
            name: program.name.clone(),
            detail: map_program_version_detail(stored_version),
        }
    };
    Ok(Json(response))
}

pub(super) async fn rpc_poll_handler(
    State(state): State<ControllerState>,
    Json(request): Json<EdgePollRequest>,
) -> Json<EdgePollResponse> {
    state
        .metrics
        .poll_requests_total
        .fetch_add(1, Ordering::Relaxed);

    let debug_session_active = request.telemetry.debug_session_active;
    let debug_session_attached = request.telemetry.debug_session_attached;
    let debug_session_current_line = request.telemetry.debug_session_current_line;
    let debug_session_request_id = request.telemetry.debug_session_request_id.clone();
    let (resolved_edge_id, command) = {
        let mut guard = state.inner.write().await;
        let edge_id = guard.resolve_or_create_edge_id(&request.edge_id);
        let record = guard.edges.entry(edge_id.clone()).or_default();
        let reported_name = request
            .edge_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if let Some(name) = reported_name {
            record.edge_name = name.to_string();
        } else if record.edge_name.is_empty() {
            record.edge_name = request.edge_id.clone();
        }
        let now = now_unix_ms();
        record.last_poll_unix_ms = Some(now);
        record.last_telemetry = Some(request.telemetry);
        if let Some(sample) = request.traffic_sample {
            append_traffic_sample(record, sample, now);
        }
        record.total_polls += 1;
        let command = record.pending_commands.pop_front();
        (edge_id, command)
    };

    {
        let mut sessions = state.debug_sessions.write().await;
        for session in sessions
            .values_mut()
            .filter(|item| item.edge_id == resolved_edge_id || item.edge_id == request.edge_id)
        {
            if matches!(
                session.phase,
                DebugSessionPhase::Stopped | DebugSessionPhase::Failed
            ) {
                continue;
            }
            if session.mode == DebugSessionMode::Recording {
                if session.edge_id != resolved_edge_id {
                    session.edge_id = resolved_edge_id.clone();
                }
                continue;
            }
            if session.edge_id != resolved_edge_id {
                session.edge_id = resolved_edge_id.clone();
            }
            if let Some(request_id) = debug_session_request_id.clone() {
                session.request_id = Some(request_id);
            }
            if !debug_session_active {
                session.phase = DebugSessionPhase::Stopped;
                session.current_line = None;
                session.last_resume_command_unix_ms = None;
                session.updated_unix_ms = now_unix_ms();
                session.message = Some("debug session is no longer active on edge".to_string());
                continue;
            }
            if debug_session_attached {
                session.phase = DebugSessionPhase::Attached;
                session.last_resume_command_unix_ms = None;
                if session.attached_unix_ms.is_none() {
                    session.attached_unix_ms = Some(now_unix_ms());
                }
                if let Some(line) = debug_session_current_line {
                    session.current_line = Some(line);
                }
                session.updated_unix_ms = now_unix_ms();
                session.message = Some("debugger attached".to_string());
            } else if session.phase != DebugSessionPhase::WaitingForStartResult {
                let now = now_unix_ms();
                if let Some(last_resume) = session.last_resume_command_unix_ms
                    && now.saturating_sub(last_resume) <= DEBUG_RESUME_GRACE_MS
                {
                    session.updated_unix_ms = now;
                    continue;
                }
                session.phase = DebugSessionPhase::WaitingForAttach;
                session.current_line = None;
                session.updated_unix_ms = now;
            }
        }
    }

    if command.is_some() {
        state
            .metrics
            .commands_delivered_total
            .fetch_add(1, Ordering::Relaxed);
    }

    if let Err(err) = state.persist_snapshot().await {
        warn!("failed to persist controller state after poll update: {err}");
    }

    let poll_interval_ms = if debug_session_active {
        200
    } else {
        state.config.default_poll_interval_ms
    };
    Json(EdgePollResponse {
        command,
        poll_interval_ms,
    })
}

pub(super) async fn rpc_result_handler(
    State(state): State<ControllerState>,
    Json(result): Json<EdgeCommandResult>,
) -> StatusCode {
    state
        .metrics
        .result_posts_total
        .fetch_add(1, Ordering::Relaxed);

    let is_ok = result.ok;
    let command_id = result.command_id.clone();
    let result_payload = result.result.clone();
    let edge_name_for_debug = result.edge_name.clone();
    let resolved_edge_id_for_debug = {
        let mut guard = state.inner.write().await;
        let edge_id = guard.resolve_or_create_edge_id(&result.edge_id);
        let resolved_for_debug = edge_id.clone();
        let record = guard.edges.entry(edge_id).or_default();
        let reported_name = result
            .edge_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if let Some(name) = reported_name {
            record.edge_name = name.to_string();
        } else if record.edge_name.is_empty() {
            record.edge_name = result.edge_id.clone();
        }
        record.last_result_unix_ms = Some(now_unix_ms());
        record.total_results += 1;
        record.recent_results.push_back(result);
        while record.recent_results.len() > state.config.max_result_history {
            let _ = record.recent_results.pop_front();
        }
        if let Some(program_ref) = record.pending_apply_programs.remove(&command_id)
            && is_ok
        {
            record.applied_program = Some(program_ref);
        }
        resolved_for_debug
    };

    process_debug_session_result(
        state.clone(),
        &command_id,
        &resolved_edge_id_for_debug,
        edge_name_for_debug,
        is_ok,
        &result_payload,
    )
    .await;

    if let Err(err) = state.persist_snapshot().await {
        warn!("failed to persist controller state after command result: {err}");
    }

    if is_ok {
        state
            .metrics
            .command_results_ok_total
            .fetch_add(1, Ordering::Relaxed);
    } else {
        state
            .metrics
            .command_results_error_total
            .fetch_add(1, Ordering::Relaxed);
    }

    StatusCode::NO_CONTENT
}

pub(super) async fn list_edges_handler(
    State(state): State<ControllerState>,
) -> Json<EdgeListResponse> {
    let mut edges = {
        let guard = state.inner.read().await;
        guard
            .edges
            .iter()
            .map(|(id, record)| map_summary(id, record))
            .collect::<Vec<_>>()
    };
    edges.sort_by(|lhs, rhs| lhs.edge_name.cmp(&rhs.edge_name));
    Json(EdgeListResponse { edges })
}

pub(super) async fn get_edge_handler(
    State(state): State<ControllerState>,
    Path(edge_id): Path<String>,
) -> Result<Json<EdgeDetailResponse>, (StatusCode, Json<ErrorResponse>)> {
    let detail = {
        let guard = state.inner.read().await;
        let Some(resolved_id) = guard.resolve_edge_id(&edge_id) else {
            return Err(not_found("edge not found"));
        };
        let Some(record) = guard.edges.get(&resolved_id) else {
            return Err(not_found("edge not found"));
        };
        EdgeDetailResponse {
            summary: map_summary(&resolved_id, record),
            pending_command_types: record
                .pending_commands
                .iter()
                .map(command_kind)
                .map(str::to_string)
                .collect(),
            traffic_series: record.traffic_points.iter().cloned().collect(),
        }
    };
    Ok(Json(detail))
}

pub(super) async fn get_edge_results_handler(
    State(state): State<ControllerState>,
    Path(edge_id): Path<String>,
    Query(query): Query<ResultsQuery>,
) -> Result<Json<EdgeResultsResponse>, (StatusCode, Json<ErrorResponse>)> {
    let limit = query
        .limit
        .unwrap_or(state.config.max_result_history)
        .max(1);
    let response = {
        let guard = state.inner.read().await;
        let Some(resolved_id) = guard.resolve_edge_id(&edge_id) else {
            return Err(not_found("edge not found"));
        };
        let Some(record) = guard.edges.get(&resolved_id) else {
            return Err(not_found("edge not found"));
        };
        let results = record
            .recent_results
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        EdgeResultsResponse { results }
    };
    Ok(Json(response))
}

pub(super) async fn list_debug_sessions_handler(
    State(state): State<ControllerState>,
) -> Json<DebugSessionListResponse> {
    let mut sessions = {
        let guard = state.debug_sessions.read().await;
        guard
            .values()
            .map(DebugSessionRecord::to_summary)
            .collect::<Vec<_>>()
    };
    sessions.sort_by(|lhs, rhs| rhs.updated_unix_ms.cmp(&lhs.updated_unix_ms));
    Json(DebugSessionListResponse { sessions })
}

pub(super) async fn get_debug_session_handler(
    State(state): State<ControllerState>,
    Path(session_id): Path<String>,
) -> Result<Json<DebugSessionDetail>, (StatusCode, Json<ErrorResponse>)> {
    let detail = {
        let guard = state.debug_sessions.read().await;
        let Some(session) = guard.get(&session_id) else {
            return Err(not_found("debug session not found"));
        };
        session.to_detail()
    };
    Ok(Json(detail))
}

pub(super) async fn create_debug_session_handler(
    State(state): State<ControllerState>,
    Json(request): Json<CreateDebugSessionRequest>,
) -> Result<(StatusCode, Json<DebugSessionDetail>), (StatusCode, Json<ErrorResponse>)> {
    let mode = request.mode.clone();
    let tcp_addr = request
        .tcp_addr
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let request_path = request
        .request_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let record_count = request.record_count.unwrap_or(DEFAULT_RECORDING_COUNT);
    if mode == DebugSessionMode::Recording && request_path.is_none() {
        return Err(bad_request(
            "recording mode requires request_path (for example: /api/foo)",
        ));
    }
    if mode == DebugSessionMode::Recording && record_count == 0 {
        return Err(bad_request("record_count must be >= 1"));
    }

    let (resolved_edge_id, edge_name, source_flavor, source_code) = {
        let guard = state.inner.read().await;
        let Some(resolved_edge_id) = guard.resolve_edge_id(&request.edge_id) else {
            return Err(not_found("edge not found"));
        };
        let Some(record) = guard.edges.get(&resolved_edge_id) else {
            return Err(not_found("edge not found"));
        };
        if !record
            .last_telemetry
            .as_ref()
            .map(|telemetry| telemetry.program_loaded)
            .unwrap_or(false)
        {
            return Err(bad_request(
                "edge has no loaded program yet; apply a program before starting a debug session",
            ));
        }
        let edge_name = if record.edge_name.trim().is_empty() {
            resolved_edge_id.clone()
        } else {
            record.edge_name.clone()
        };
        let (source_flavor, source_code) = resolve_edge_debug_source(&guard, &resolved_edge_id);
        (resolved_edge_id, edge_name, source_flavor, source_code)
    };

    let now = now_unix_ms();
    let command_id = state.next_command_id();
    let session_id = Uuid::new_v4().to_string();
    let stop_on_entry = request.stop_on_entry.unwrap_or(true);
    let requested_header_name = request
        .header_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let command = ControlPlaneCommand::StartDebugSession {
        command_id: command_id.clone(),
        session_id: session_id.clone(),
        tcp_addr: if mode == DebugSessionMode::Interactive {
            tcp_addr.clone()
        } else {
            None
        },
        header_name: if mode == DebugSessionMode::Interactive {
            requested_header_name.clone()
        } else {
            None
        },
        stop_on_entry: Some(stop_on_entry),
        mode: mode.clone(),
        request_path: request_path.clone(),
        record_count: Some(record_count),
    };
    let _queued = state
        .enqueue_command(resolved_edge_id.clone(), command)
        .await;

    let record = DebugSessionRecord {
        session_id: session_id.clone(),
        edge_id: resolved_edge_id,
        edge_name,
        phase: DebugSessionPhase::WaitingForStartResult,
        mode: mode.clone(),
        requested_header_name,
        header_name: None,
        nonce_header_value: None,
        request_id: None,
        tcp_addr: tcp_addr.unwrap_or_default(),
        request_path,
        recording_target_count: if mode == DebugSessionMode::Recording {
            Some(record_count)
        } else {
            None
        },
        recordings: Vec::new(),
        selected_recording_id: None,
        start_command_id: command_id.clone(),
        stop_command_id: None,
        current_line: None,
        source_flavor,
        source_code,
        breakpoints: HashSet::new(),
        created_unix_ms: now,
        updated_unix_ms: now,
        attached_unix_ms: None,
        last_resume_command_unix_ms: None,
        message: Some(match mode {
            DebugSessionMode::Interactive => "start-debug command queued".to_string(),
            DebugSessionMode::Recording => "start-recording command queued".to_string(),
        }),
        last_output: None,
        replay_states: HashMap::new(),
    };

    {
        let mut sessions = state.debug_sessions.write().await;
        sessions.insert(session_id.clone(), record.clone());
    }
    {
        let mut lookup = state.debug_start_lookup.write().await;
        lookup.insert(command_id, session_id);
    }
    state.persist_snapshot().await.map_err(internal_error)?;

    Ok((StatusCode::ACCEPTED, Json(record.to_detail())))
}

pub(super) async fn stop_debug_session_handler(
    State(state): State<ControllerState>,
    Path(session_id): Path<String>,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let mut stop_command_to_queue: Option<(String, String)> = None;
    {
        let mut sessions = state.debug_sessions.write().await;
        let Some(session) = sessions.get_mut(&session_id) else {
            return Err(not_found("debug session not found"));
        };
        if !matches!(
            session.phase,
            DebugSessionPhase::Stopped | DebugSessionPhase::Failed
        ) {
            let command_id = state.next_command_id();
            stop_command_to_queue = Some((session.edge_id.clone(), command_id.clone()));
            session.stop_command_id = Some(command_id);
            session.phase = DebugSessionPhase::Stopped;
            session.current_line = None;
            session.last_resume_command_unix_ms = None;
            session.message = if session.mode == DebugSessionMode::Recording {
                Some("recording session stop requested".to_string())
            } else {
                Some("debug session stop requested".to_string())
            };
        }
        session.updated_unix_ms = now_unix_ms();
    }
    if let Some((edge_id, command_id)) = stop_command_to_queue {
        let command = ControlPlaneCommand::StopDebugSession { command_id };
        let _queued = state.enqueue_command(edge_id, command).await;
    }
    state.persist_snapshot().await.map_err(internal_error)?;
    Ok(Json(StatusResponse { status: "stopped" }))
}

pub(super) async fn delete_debug_session_handler(
    State(state): State<ControllerState>,
    Path(session_id): Path<String>,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let (edge_id, should_stop_first) = {
        let guard = state.debug_sessions.read().await;
        let Some(session) = guard.get(&session_id) else {
            return Err(not_found("debug session not found"));
        };
        (
            session.edge_id.clone(),
            !matches!(
                session.phase,
                DebugSessionPhase::Stopped | DebugSessionPhase::Failed
            ),
        )
    };

    if should_stop_first {
        let command_id = state.next_command_id();
        let command = ControlPlaneCommand::StopDebugSession { command_id };
        let _queued = state.enqueue_command(edge_id, command).await;
    }

    {
        let mut sessions = state.debug_sessions.write().await;
        if sessions.remove(&session_id).is_none() {
            return Err(not_found("debug session not found"));
        }
    }
    {
        let mut lookup = state.debug_start_lookup.write().await;
        lookup.retain(|_, value| value != &session_id);
    }
    {
        let mut recordings = state.debug_recordings.write().await;
        recordings.remove(&session_id);
    }
    state.persist_snapshot().await.map_err(internal_error)?;

    Ok(Json(StatusResponse { status: "deleted" }))
}

pub(super) async fn run_debug_command_handler(
    State(state): State<ControllerState>,
    Path(session_id): Path<String>,
    Json(request): Json<DebugCommandRequest>,
) -> Result<Json<DebugCommandResponse>, (StatusCode, Json<ErrorResponse>)> {
    let request_for_state = request.clone();
    let (edge_id, phase, mode, selected_recording_id) = {
        let guard = state.debug_sessions.read().await;
        let Some(session) = guard.get(&session_id) else {
            return Err(not_found("debug session not found"));
        };
        (
            session.edge_id.clone(),
            session.phase.clone(),
            session.mode.clone(),
            session.selected_recording_id.clone(),
        )
    };
    if mode == DebugSessionMode::Interactive {
        if phase != DebugSessionPhase::Attached {
            return Err(bad_request(
                "debug session is not attached yet; wait for a matching request",
            ));
        }

        let rpc_command = match request {
            DebugCommandRequest::Where => RemoteDebugCommand::Where,
            DebugCommandRequest::Step => RemoteDebugCommand::Step,
            DebugCommandRequest::Next => RemoteDebugCommand::Next,
            DebugCommandRequest::Continue => RemoteDebugCommand::Continue,
            DebugCommandRequest::Out => RemoteDebugCommand::Out,
            DebugCommandRequest::SelectRecording { .. } => {
                return Err(bad_request(
                    "recording selection is only available for recording sessions",
                ));
            }
            DebugCommandRequest::BreakLine { line } => RemoteDebugCommand::BreakLine { line },
            DebugCommandRequest::ClearLine { line } => RemoteDebugCommand::ClearLine { line },
            DebugCommandRequest::PrintVar { name } => {
                if name.trim().is_empty() {
                    return Err(bad_request("variable name cannot be empty"));
                }
                RemoteDebugCommand::PrintVar {
                    name: name.trim().to_string(),
                }
            }
            DebugCommandRequest::Locals => RemoteDebugCommand::Locals,
            DebugCommandRequest::Stack => RemoteDebugCommand::Stack,
        };

        let command_id = state.next_command_id();
        let (response_tx, response_rx) = oneshot::channel();
        {
            let mut waiters = state.debug_command_waiters.lock().await;
            waiters.insert(command_id.clone(), response_tx);
        }

        let command = ControlPlaneCommand::DebugCommand {
            command_id: command_id.clone(),
            session_id: session_id.clone(),
            command: rpc_command,
        };
        let _queued = state.enqueue_command(edge_id, command).await;

        let response = match timeout(Duration::from_secs(20), response_rx).await {
            Ok(Ok(Ok(response))) => response,
            Ok(Ok(Err(message))) => {
                return Err(bad_request(&message));
            }
            Ok(Err(_)) => {
                return Err(bad_request("debug command response channel closed"));
            }
            Err(_) => {
                let mut waiters = state.debug_command_waiters.lock().await;
                waiters.remove(&command_id);
                return Err(bad_request(
                    "debug command timed out waiting for edge result",
                ));
            }
        };

        {
            let mut sessions = state.debug_sessions.write().await;
            if let Some(session) = sessions.get_mut(&session_id) {
                match request_for_state {
                    DebugCommandRequest::BreakLine { line } => {
                        session.breakpoints.insert(line);
                    }
                    DebugCommandRequest::ClearLine { line } => {
                        session.breakpoints.remove(&line);
                    }
                    _ => {}
                }
            }
        }
        state.persist_snapshot().await.map_err(internal_error)?;

        return Ok(Json(response));
    }

    if phase == DebugSessionPhase::WaitingForStartResult
        || phase == DebugSessionPhase::WaitingForRecordings
    {
        return Err(bad_request(
            "recordings are not available yet; send matching requests first",
        ));
    }

    let target_recording_id = match request.clone() {
        DebugCommandRequest::SelectRecording { recording_id } => recording_id,
        _ => selected_recording_id
            .ok_or_else(|| bad_request("no recording selected; select a recording first"))?
            .clone(),
    };

    let stored_recording = {
        let recordings = state.debug_recordings.read().await;
        recordings
            .get(&session_id)
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item.recording_id == target_recording_id)
                    .cloned()
            })
            .ok_or_else(|| bad_request("recording not found"))?
    };

    let recording_bytes = STANDARD
        .decode(stored_recording.recording_base64.as_bytes())
        .map_err(|err| bad_request(&format!("invalid recording payload: {err}")))?;
    let recording = VmRecording::decode(&recording_bytes)
        .map_err(|err| bad_request(&format!("failed to decode recording: {err}")))?;

    let command_text = match request.clone() {
        DebugCommandRequest::SelectRecording { .. } => "where".to_string(),
        DebugCommandRequest::Where => "where".to_string(),
        DebugCommandRequest::Step => "step".to_string(),
        DebugCommandRequest::Next => "next".to_string(),
        DebugCommandRequest::Continue => "continue".to_string(),
        DebugCommandRequest::Out => "out".to_string(),
        DebugCommandRequest::BreakLine { line } => format!("break line {line}"),
        DebugCommandRequest::ClearLine { line } => format!("clear line {line}"),
        DebugCommandRequest::PrintVar { name } => {
            if name.trim().is_empty() {
                return Err(bad_request("variable name cannot be empty"));
            }
            format!("print {}", name.trim())
        }
        DebugCommandRequest::Locals => "locals".to_string(),
        DebugCommandRequest::Stack => "stack".to_string(),
    };

    let response = {
        let mut sessions = state.debug_sessions.write().await;
        let Some(session) = sessions.get_mut(&session_id) else {
            return Err(not_found("debug session not found"));
        };
        if !session
            .recordings
            .iter()
            .any(|item| item.recording_id == target_recording_id)
        {
            return Err(bad_request("recording is not part of this session"));
        }

        if matches!(
            request_for_state,
            DebugCommandRequest::SelectRecording { .. }
        ) {
            session.replay_states.insert(
                target_recording_id.clone(),
                VmRecordingReplayState::default(),
            );
        }
        let replay_state = session
            .replay_states
            .entry(target_recording_id.clone())
            .or_insert_with(VmRecordingReplayState::default);
        let replay = run_recording_replay_command(&recording, replay_state, &command_text);

        match request_for_state {
            DebugCommandRequest::BreakLine { line } => {
                session.breakpoints.insert(line);
            }
            DebugCommandRequest::ClearLine { line } => {
                session.breakpoints.remove(&line);
            }
            _ => {}
        }

        session.phase = DebugSessionPhase::ReplayReady;
        session.current_line = replay.current_line;
        session.selected_recording_id = Some(target_recording_id);
        session.request_id = stored_recording.request_id.clone();
        session.updated_unix_ms = now_unix_ms();
        session.last_output = Some(replay.output.clone());
        session.message = if replay.at_end {
            Some("replay cursor reached end of recording".to_string())
        } else {
            Some("replay command completed".to_string())
        };
        DebugCommandResponse {
            phase: session.phase.clone(),
            output: replay.output,
            current_line: replay.current_line,
            attached: !replay.exited,
        }
    };
    state.persist_snapshot().await.map_err(internal_error)?;

    Ok(Json(response))
}

pub(super) async fn enqueue_program_binary_handler(
    State(state): State<ControllerState>,
    Path(edge_id): Path<String>,
    request: Request,
) -> Result<(StatusCode, Json<EnqueueCommandResponse>), (StatusCode, Json<ErrorResponse>)> {
    if !is_octet_stream(request.headers().get(CONTENT_TYPE)) {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(ErrorResponse {
                error: "content-type must be application/octet-stream".to_string(),
            }),
        ));
    }

    let bytes = match to_bytes(request.into_body(), MAX_UPLOAD_BYTES + 1).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(ErrorResponse {
                    error: "payload too large".to_string(),
                }),
            ));
        }
    };
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ErrorResponse {
                error: "payload too large".to_string(),
            }),
        ));
    }

    let command_id = state.next_command_id();
    let command = ControlPlaneCommand::ApplyProgram {
        command_id,
        program_base64: STANDARD.encode(bytes),
    };
    let queued = state.enqueue_command(edge_id, command).await;
    Ok((StatusCode::ACCEPTED, Json(queued)))
}

pub(super) async fn enqueue_apply_program_handler(
    State(state): State<ControllerState>,
    Path(edge_id): Path<String>,
    Json(request): Json<EnqueueApplyProgramRequest>,
) -> (StatusCode, Json<EnqueueCommandResponse>) {
    let command = ControlPlaneCommand::ApplyProgram {
        command_id: request
            .command_id
            .unwrap_or_else(|| state.next_command_id()),
        program_base64: request.program_base64,
    };
    let queued = state.enqueue_command(edge_id, command).await;
    (StatusCode::ACCEPTED, Json(queued))
}

pub(super) async fn enqueue_apply_program_version_handler(
    State(state): State<ControllerState>,
    Path(edge_id): Path<String>,
    Json(request): Json<ApplyProgramVersionRequest>,
) -> Result<(StatusCode, Json<EnqueueCommandResponse>), (StatusCode, Json<ErrorResponse>)> {
    let (source, flavor, program_name, selected_version) = {
        let guard = state.inner.read().await;
        let Some(program) = guard.programs.get(&request.program_id) else {
            return Err(not_found("program not found"));
        };
        let selected = if let Some(version) = request.version {
            program.versions.iter().find(|item| item.version == version)
        } else {
            program.versions.last()
        };
        let Some(version) = selected else {
            return Err(bad_request("program has no versions"));
        };
        let source = version.source.clone();
        let flavor = request
            .flavor
            .clone()
            .unwrap_or_else(|| version.flavor.clone());
        (source, flavor, program.name.clone(), version.version)
    };

    let (parsed_flavor, _) = parse_ui_flavor(Some(flavor.as_str()))?;
    let source_text = source_for_flavor(&source, parsed_flavor);
    let compiled = compile_source_with_flavor(&source_text, parsed_flavor)
        .map_err(|err| bad_request(&format!("source compile failed: {err}")))?;
    let program_bytes = encode_program(&compiled.program)
        .map_err(|err| bad_request(&format!("bytecode encode failed: {err}")))?;

    let command_id = state.next_command_id();
    let command = ControlPlaneCommand::ApplyProgram {
        command_id: command_id.clone(),
        program_base64: STANDARD.encode(program_bytes),
    };
    let queued = state
        .enqueue_command_tracked(
            edge_id,
            command,
            Some(AppliedProgramRef {
                program_id: request.program_id,
                name: program_name,
                version: selected_version,
            }),
        )
        .await;
    Ok((StatusCode::ACCEPTED, Json(queued)))
}

pub(super) async fn enqueue_start_debug_handler(
    State(state): State<ControllerState>,
    Path(edge_id): Path<String>,
    Json(request): Json<EnqueueStartDebugRequest>,
) -> Result<(StatusCode, Json<EnqueueCommandResponse>), (StatusCode, Json<ErrorResponse>)> {
    let mode = request.mode.clone();
    let tcp_addr = request
        .tcp_addr
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let request_path = request
        .request_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let record_count = request.record_count.unwrap_or(DEFAULT_RECORDING_COUNT);
    if mode == DebugSessionMode::Recording && request_path.is_none() {
        return Err(bad_request(
            "recording mode requires request_path (for example: /api/foo)",
        ));
    }
    if mode == DebugSessionMode::Recording && record_count == 0 {
        return Err(bad_request("record_count must be >= 1"));
    }
    let session_id = Uuid::new_v4().to_string();
    let command = ControlPlaneCommand::StartDebugSession {
        command_id: request
            .command_id
            .unwrap_or_else(|| state.next_command_id()),
        session_id,
        tcp_addr: if mode == DebugSessionMode::Interactive {
            tcp_addr
        } else {
            None
        },
        header_name: if mode == DebugSessionMode::Interactive {
            request.header_name
        } else {
            None
        },
        stop_on_entry: request.stop_on_entry,
        mode,
        request_path,
        record_count: Some(record_count),
    };
    let queued = state.enqueue_command(edge_id, command).await;
    Ok((StatusCode::ACCEPTED, Json(queued)))
}

pub(super) async fn enqueue_stop_debug_handler(
    State(state): State<ControllerState>,
    Path(edge_id): Path<String>,
    Json(request): Json<OptionalCommandIdRequest>,
) -> (StatusCode, Json<EnqueueCommandResponse>) {
    let command = ControlPlaneCommand::StopDebugSession {
        command_id: request
            .command_id
            .unwrap_or_else(|| state.next_command_id()),
    };
    let queued = state.enqueue_command(edge_id, command).await;
    (StatusCode::ACCEPTED, Json(queued))
}

pub(super) async fn enqueue_get_health_handler(
    State(state): State<ControllerState>,
    Path(edge_id): Path<String>,
    Json(request): Json<OptionalCommandIdRequest>,
) -> (StatusCode, Json<EnqueueCommandResponse>) {
    let command = ControlPlaneCommand::GetHealth {
        command_id: request
            .command_id
            .unwrap_or_else(|| state.next_command_id()),
    };
    let queued = state.enqueue_command(edge_id, command).await;
    (StatusCode::ACCEPTED, Json(queued))
}

pub(super) async fn enqueue_get_metrics_handler(
    State(state): State<ControllerState>,
    Path(edge_id): Path<String>,
    Json(request): Json<OptionalCommandIdRequest>,
) -> (StatusCode, Json<EnqueueCommandResponse>) {
    let command = ControlPlaneCommand::GetMetrics {
        command_id: request
            .command_id
            .unwrap_or_else(|| state.next_command_id()),
    };
    let queued = state.enqueue_command(edge_id, command).await;
    (StatusCode::ACCEPTED, Json(queued))
}

pub(super) async fn enqueue_get_telemetry_handler(
    State(state): State<ControllerState>,
    Path(edge_id): Path<String>,
    Json(request): Json<OptionalCommandIdRequest>,
) -> (StatusCode, Json<EnqueueCommandResponse>) {
    let command = ControlPlaneCommand::GetTelemetry {
        command_id: request
            .command_id
            .unwrap_or_else(|| state.next_command_id()),
    };
    let queued = state.enqueue_command(edge_id, command).await;
    (StatusCode::ACCEPTED, Json(queued))
}

pub(super) async fn enqueue_ping_handler(
    State(state): State<ControllerState>,
    Path(edge_id): Path<String>,
    Json(request): Json<EnqueuePingRequest>,
) -> (StatusCode, Json<EnqueueCommandResponse>) {
    let command = ControlPlaneCommand::Ping {
        command_id: request
            .command_id
            .unwrap_or_else(|| state.next_command_id()),
        payload: request.payload,
    };
    let queued = state.enqueue_command(edge_id, command).await;
    (StatusCode::ACCEPTED, Json(queued))
}

async fn process_debug_session_result(
    state: ControllerState,
    command_id: &str,
    edge_id: &str,
    edge_name: Option<String>,
    is_ok: bool,
    payload: &CommandResultPayload,
) {
    match payload {
        CommandResultPayload::StartDebugSession {
            status,
            nonce_header_name,
            nonce_header_value,
            message,
        } => {
            let session_id = {
                let mut lookup = state.debug_start_lookup.write().await;
                lookup.remove(command_id)
            };
            let Some(session_id) = session_id else {
                return;
            };

            {
                let mut sessions = state.debug_sessions.write().await;
                let Some(session) = sessions.get_mut(&session_id) else {
                    return;
                };
                session.edge_id = edge_id.to_string();
                if let Some(name) = edge_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                {
                    session.edge_name = name.to_string();
                }
                session.updated_unix_ms = now_unix_ms();
                if is_ok && status.is_some() {
                    let reported_status = status.as_ref();
                    session.phase = if session.mode == DebugSessionMode::Interactive {
                        if reported_status.map(|item| item.attached).unwrap_or(false) {
                            DebugSessionPhase::Attached
                        } else {
                            DebugSessionPhase::WaitingForAttach
                        }
                    } else {
                        DebugSessionPhase::WaitingForRecordings
                    };
                    session.header_name = nonce_header_name
                        .clone()
                        .or_else(|| reported_status.and_then(|item| item.header_name.clone()))
                        .or_else(|| session.requested_header_name.clone());
                    session.nonce_header_value = nonce_header_value
                        .clone()
                        .or_else(|| reported_status.and_then(|item| item.header_value.clone()));
                    session.request_id = reported_status
                        .and_then(|item| item.request_id.clone())
                        .or_else(|| session.request_id.clone());
                    if let Some(addr) = reported_status
                        .and_then(|item| item.tcp_addr.clone())
                        .filter(|value| !value.trim().is_empty())
                    {
                        session.tcp_addr = addr;
                    }
                    session.current_line = reported_status.and_then(|item| item.current_line);
                    if session.mode == DebugSessionMode::Interactive {
                        session.message = Some(
                            "debug session active on edge; waiting for a matching request to attach"
                                .to_string(),
                        );
                    } else {
                        session.request_path = reported_status
                            .and_then(|item| item.request_path.clone())
                            .or_else(|| session.request_path.clone());
                        session.recording_target_count = reported_status
                            .and_then(|item| item.target_recordings)
                            .or(session.recording_target_count);
                        session.message = Some(
                            "recording session active on edge; waiting for matching requests"
                                .to_string(),
                        );
                    }
                } else {
                    session.phase = DebugSessionPhase::Failed;
                    session.message =
                        Some(message.clone().unwrap_or_else(|| {
                            "failed to start debug session on edge".to_string()
                        }));
                }
            }
        }
        CommandResultPayload::DebugCommand {
            session_id,
            response,
            message,
        } => {
            let target_session_id = if let Some(session_id) = session_id.clone() {
                Some(session_id)
            } else {
                let sessions = state.debug_sessions.read().await;
                sessions
                    .values()
                    .find(|item| item.edge_id == edge_id)
                    .map(|item| item.session_id.clone())
            };
            let mut response_for_waiter: Result<DebugCommandResponse, String> = Err(message
                .clone()
                .unwrap_or_else(|| "debug command failed".to_string()));

            if let Some(target_session_id) = target_session_id {
                let mut sessions = state.debug_sessions.write().await;
                if let Some(session) = sessions.get_mut(&target_session_id) {
                    session.updated_unix_ms = now_unix_ms();
                    if let Some(name) = edge_name
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                    {
                        session.edge_name = name.to_string();
                    }
                    if is_ok {
                        if let Some(remote) = response {
                            if remote.attached {
                                session.phase = DebugSessionPhase::Attached;
                                session.last_resume_command_unix_ms = None;
                                if let Some(line) = remote.current_line {
                                    session.current_line = Some(line);
                                }
                                session.message = Some("debugger attached".to_string());
                            } else {
                                session.last_resume_command_unix_ms = Some(now_unix_ms());
                                if session.phase != DebugSessionPhase::Attached {
                                    session.phase = DebugSessionPhase::WaitingForAttach;
                                }
                                // Keep current line until we positively observe detached/not-attached.
                                session.message =
                                    Some("resume command sent; waiting for next stop".to_string());
                            }
                            session.last_output = Some(remote.output.clone());
                            response_for_waiter = Ok(DebugCommandResponse {
                                phase: session.phase.clone(),
                                output: remote.output.clone(),
                                current_line: session.current_line,
                                attached: remote.attached,
                            });
                        }
                    } else {
                        let error_message = message
                            .clone()
                            .unwrap_or_else(|| "debug command failed".to_string());
                        if error_message.contains("not attached") {
                            session.phase = DebugSessionPhase::WaitingForAttach;
                            session.last_resume_command_unix_ms = None;
                        } else {
                            session.phase = DebugSessionPhase::Failed;
                            session.last_resume_command_unix_ms = None;
                        }
                        session.message = Some(error_message.clone());
                        response_for_waiter = Err(error_message);
                    }
                }
            }

            let waiter = {
                let mut waiters = state.debug_command_waiters.lock().await;
                waiters.remove(command_id)
            };
            if let Some(waiter) = waiter {
                let _ = waiter.send(response_for_waiter);
            }
        }
        CommandResultPayload::DebugRecording {
            session_id,
            recording_id,
            request_id,
            request_path,
            recording_base64,
            frame_count,
            terminal_status,
            sequence,
            completed,
            message,
        } => {
            {
                let sessions = state.debug_sessions.read().await;
                if !sessions.contains_key(session_id) {
                    return;
                }
            }
            let created_unix_ms = now_unix_ms();
            let stored = StoredDebugRecording {
                recording_id: recording_id.clone(),
                session_id: session_id.clone(),
                edge_id: edge_id.to_string(),
                edge_name: edge_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .unwrap_or(edge_id)
                    .to_string(),
                sequence: *sequence,
                created_unix_ms,
                frame_count: *frame_count,
                terminal_status: terminal_status.clone(),
                request_id: request_id.clone(),
                request_path: request_path.clone(),
                recording_base64: recording_base64.clone(),
            };
            {
                let mut recordings = state.debug_recordings.write().await;
                let items = recordings.entry(session_id.clone()).or_default();
                if let Some(existing) = items
                    .iter_mut()
                    .find(|item| item.recording_id == *recording_id)
                {
                    *existing = stored.clone();
                } else {
                    items.push(stored);
                }
                items.sort_by_key(|item| item.sequence);
            }

            let initial_line = STANDARD
                .decode(recording_base64.as_bytes())
                .ok()
                .and_then(|bytes| VmRecording::decode(&bytes).ok())
                .and_then(|recording| {
                    let mut state = VmRecordingReplayState::default();
                    run_recording_replay_command(&recording, &mut state, "where").current_line
                });

            let mut stop_command_to_queue: Option<(String, String)> = None;
            let mut sessions = state.debug_sessions.write().await;
            let Some(session) = sessions.get_mut(session_id) else {
                return;
            };
            session.edge_id = edge_id.to_string();
            if let Some(name) = edge_name
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                session.edge_name = name.to_string();
            }
            if let Some(path) = request_path.clone() {
                session.request_path = Some(path);
            }
            if let Some(id) = request_id.clone() {
                session.request_id = Some(id);
            }
            if let Some(target) = session.recording_target_count
                && target > 0
            {
                session.recording_target_count = Some(target);
            }

            let summary = DebugRecordingSummary {
                recording_id: recording_id.clone(),
                sequence: *sequence,
                created_unix_ms,
                frame_count: *frame_count,
                terminal_status: terminal_status.clone(),
                request_id: request_id.clone(),
                request_path: request_path.clone(),
            };
            if let Some(existing) = session
                .recordings
                .iter_mut()
                .find(|item| item.recording_id == *recording_id)
            {
                *existing = summary;
            } else {
                session.recordings.push(summary);
                session.recordings.sort_by_key(|item| item.sequence);
            }

            if session.selected_recording_id.is_none() {
                session.selected_recording_id = Some(recording_id.clone());
                session
                    .replay_states
                    .insert(recording_id.clone(), VmRecordingReplayState::default());
            }
            if session.current_line.is_none() {
                session.current_line = initial_line;
            }
            if *completed {
                session.phase = DebugSessionPhase::Stopped;
                if session.stop_command_id.is_none() {
                    let command_id = state.next_command_id();
                    session.stop_command_id = Some(command_id.clone());
                    stop_command_to_queue = Some((session.edge_id.clone(), command_id));
                }
            } else {
                session.phase = DebugSessionPhase::ReplayReady;
            }
            session.updated_unix_ms = created_unix_ms;
            session.message = if let Some(custom) = message.clone() {
                Some(custom)
            } else if *completed {
                Some("recording collection complete".to_string())
            } else {
                Some(format!(
                    "captured recording {} ({} frame{})",
                    sequence,
                    frame_count,
                    if *frame_count == 1 { "" } else { "s" }
                ))
            };
            drop(sessions);
            if let Some((edge_id, command_id)) = stop_command_to_queue {
                let command = ControlPlaneCommand::StopDebugSession { command_id };
                let _queued = state.enqueue_command(edge_id, command).await;
            }
        }
        CommandResultPayload::StopDebugSession { .. } => {
            let mut sessions = state.debug_sessions.write().await;
            if let Some(session) = sessions
                .values_mut()
                .find(|session| session.stop_command_id.as_deref() == Some(command_id))
            {
                session.phase = DebugSessionPhase::Stopped;
                session.updated_unix_ms = now_unix_ms();
                session.message = if session.mode == DebugSessionMode::Recording {
                    Some("recording session completed".to_string())
                } else {
                    Some("debug session stopped".to_string())
                };
            }
        }
        _ => {}
    }
}
