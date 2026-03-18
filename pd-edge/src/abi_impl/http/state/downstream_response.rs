use super::*;

pub(crate) struct HttpResponseOutputNode {
    pub(crate) headers: HeaderMap,
    pub(crate) body: Option<Vec<u8>>,
    pub(crate) body_stream: Option<DownstreamResponseBodyStream>,
    pub(crate) status: Option<u16>,
    pub(crate) body_source_exchange: Option<i64>,
    stream_ready_notify: Arc<Notify>,
}

const IMPLICIT_DOWNSTREAM_RESPONSE_STREAM_CHUNK_BYTES: usize = 8 * 1024;

pub(crate) enum DownstreamResponseStreamWriteMode {
    ImplicitBuffered,
    ExplicitImmediate,
}

pub(crate) struct DownstreamResponseBodyStream {
    sender: Option<mpsc::UnboundedSender<Result<Bytes, io::Error>>>,
    receiver: Option<mpsc::UnboundedReceiver<Result<Bytes, io::Error>>>,
    buffered: BytesMut,
    committed: bool,
    finished: bool,
    wrote_any_bytes: bool,
}

impl DownstreamResponseBodyStream {
    fn new() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self {
            sender: Some(sender),
            receiver: Some(receiver),
            buffered: BytesMut::new(),
            committed: false,
            finished: false,
            wrote_any_bytes: false,
        }
    }

    fn committed(&self) -> bool {
        self.committed
    }

    fn start(&mut self) -> Result<bool, VmError> {
        if self.finished {
            return Err(VmError::HostError(
                "downstream response stream is already finished".to_string(),
            ));
        }
        if self.committed {
            return Ok(false);
        }
        self.committed = true;
        Ok(true)
    }

    fn flush_buffered(&mut self) -> Result<bool, VmError> {
        if self.buffered.is_empty() {
            return Ok(false);
        }
        let chunk = self.buffered.split().freeze();
        let committed_now = self.start()?;
        self.wrote_any_bytes = true;
        self.sender
            .as_ref()
            .ok_or_else(|| {
                VmError::HostError(
                    "downstream response stream is unavailable for further writes".to_string(),
                )
            })?
            .send(Ok(chunk))
            .map_err(|_| {
                VmError::HostError(
                    "downstream response stream receiver closed before writes completed"
                        .to_string(),
                )
            })?;
        Ok(committed_now)
    }

    fn write_bytes(
        &mut self,
        bytes: &[u8],
        mode: DownstreamResponseStreamWriteMode,
    ) -> Result<bool, VmError> {
        if self.finished {
            return Err(VmError::HostError(
                "downstream response stream is already finished".to_string(),
            ));
        }
        if bytes.is_empty() {
            return Ok(false);
        }
        self.buffered.extend_from_slice(bytes);
        match mode {
            DownstreamResponseStreamWriteMode::ExplicitImmediate => self.flush_buffered(),
            DownstreamResponseStreamWriteMode::ImplicitBuffered => {
                if self.buffered.len() >= IMPLICIT_DOWNSTREAM_RESPONSE_STREAM_CHUNK_BYTES {
                    self.flush_buffered()
                } else {
                    Ok(false)
                }
            }
        }
    }

    fn finish(&mut self) -> Result<bool, VmError> {
        if self.finished {
            return Ok(false);
        }
        let committed_now = self.flush_buffered()?;
        self.finished = true;
        self.sender.take();
        Ok(committed_now)
    }

    fn take_receiver(
        &mut self,
    ) -> Result<mpsc::UnboundedReceiver<Result<Bytes, io::Error>>, VmError> {
        self.receiver.take().ok_or_else(|| {
            VmError::HostError(
                "downstream response stream receiver has already been consumed".to_string(),
            )
        })
    }
}

impl std::fmt::Debug for DownstreamResponseBodyStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownstreamResponseBodyStream")
            .field("buffered_len", &self.buffered.len())
            .field("committed", &self.committed)
            .field("finished", &self.finished)
            .field("wrote_any_bytes", &self.wrote_any_bytes)
            .finish()
    }
}

impl Default for HttpResponseOutputNode {
    fn default() -> Self {
        Self {
            headers: HeaderMap::new(),
            body: None,
            body_stream: None,
            status: None,
            body_source_exchange: None,
            stream_ready_notify: Arc::new(Notify::new()),
        }
    }
}

impl HttpResponseOutputNode {
    pub(crate) fn has_local_body(&self) -> bool {
        self.body.is_some() || self.body_stream.is_some()
    }

    pub(crate) fn stream_committed(&self) -> bool {
        self.body_stream
            .as_ref()
            .is_some_and(DownstreamResponseBodyStream::committed)
    }

    fn ensure_stream_mut(&mut self) -> &mut DownstreamResponseBodyStream {
        self.body = None;
        self.body_source_exchange = None;
        self.body_stream
            .get_or_insert_with(DownstreamResponseBodyStream::new)
    }

    pub(crate) fn stream_ready_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.stream_ready_notify)
    }
}

impl std::fmt::Debug for HttpResponseOutputNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpResponseOutputNode")
            .field("headers_len", &self.headers.len())
            .field("buffered_body_len", &self.body.as_ref().map(Vec::len))
            .field("has_body_stream", &self.body_stream.is_some())
            .field("status", &self.status)
            .field("body_source_exchange", &self.body_source_exchange)
            .finish()
    }
}

pub(crate) fn start_downstream_response_stream(
    context: &SharedProxyVmContext,
) -> Result<(), VmError> {
    let notify = {
        let mut downstream = context.lock_downstream();
        downstream.vm_touches.response_body_mutated = true;
        let response = &mut downstream.response_output;
        if response.body.is_some() || response.body_source_exchange.is_some() {
            return Err(VmError::HostError(
                "downstream response stream cannot begin after a buffered body or exchange body is already selected".to_string(),
            ));
        }
        sync_response_output_stream_headers(response);
        if response.ensure_stream_mut().start()? {
            Some(response.stream_ready_notify())
        } else {
            None
        }
    };
    if let Some(notify) = notify {
        notify.notify_waiters();
    }
    Ok(())
}

pub(crate) fn write_downstream_response_stream_bytes(
    context: &SharedProxyVmContext,
    bytes: &[u8],
    mode: DownstreamResponseStreamWriteMode,
) -> Result<(), VmError> {
    context.lock_transport().tcp_dag.downstream.note_write();
    let notify = {
        let mut downstream = context.lock_downstream();
        downstream.vm_touches.response_body_mutated = true;
        let response = &mut downstream.response_output;
        if response.body.is_some() || response.body_source_exchange.is_some() {
            return Err(VmError::HostError(
                "downstream response stream cannot accept writes after a buffered body or exchange body is already selected".to_string(),
            ));
        }
        sync_response_output_stream_headers(response);
        if !bytes.is_empty() && !response.headers.contains_key(CONTENT_TYPE) {
            response
                .headers
                .insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        }
        if response.ensure_stream_mut().write_bytes(bytes, mode)? {
            Some(response.stream_ready_notify())
        } else {
            None
        }
    };
    if let Some(notify) = notify {
        notify.notify_waiters();
    }
    Ok(())
}

pub(crate) fn finish_downstream_response_stream(
    context: &SharedProxyVmContext,
) -> Result<(), VmError> {
    let notify = {
        let mut downstream = context.lock_downstream();
        let response = &mut downstream.response_output;
        if let Some(stream) = response.body_stream.as_mut() {
            if stream.finish()? {
                Some(response.stream_ready_notify())
            } else {
                None
            }
        } else {
            None
        }
    };
    if let Some(notify) = notify {
        notify.notify_waiters();
    }
    Ok(())
}

pub(crate) fn append_response_output_body_bytes(
    context: &SharedProxyVmContext,
    bytes: &[u8],
) -> Result<(), VmError> {
    write_downstream_response_stream_bytes(
        context,
        bytes,
        DownstreamResponseStreamWriteMode::ImplicitBuffered,
    )
}

fn sync_response_output_stream_headers(response: &mut HttpResponseOutputNode) {
    response.headers.remove(CONTENT_LENGTH);
}

pub(crate) fn sync_response_output_body_headers(response: &mut HttpResponseOutputNode) {
    response.body_stream = None;
    if let Some(body) = response.body.as_ref() {
        response.headers.remove(TRANSFER_ENCODING);
        if let Ok(value) = HeaderValue::from_str(&body.len().to_string()) {
            response.headers.insert(CONTENT_LENGTH, value);
        }
    }
}

pub(crate) fn current_upstream_latency_ms(context: &SharedProxyVmContext) -> u64 {
    if let Some(latency_ms) = context.native_default_upstream_forward_latency_ms() {
        return latency_ms;
    }
    context
        .lock_exchanges()
        .exchanges
        .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
        .map(|exchange| exchange.upstream_latency_ms)
        .unwrap_or(0)
}

pub(crate) fn outbound_exchange_latency_ms(context: &SharedProxyVmContext, handle: i64) -> u64 {
    context
        .lock_exchanges()
        .exchanges
        .get(&handle)
        .map(|exchange| exchange.upstream_latency_ms)
        .unwrap_or(0)
}

pub(crate) fn merge_headers(target: &mut HeaderMap, overlay: &HeaderMap) {
    for (name, value) in overlay {
        target.insert(name, value.clone());
    }
}

pub(crate) fn downstream_snapshot_response_head(
    snapshot: &HttpUpstreamResponseSnapshot,
    response_headers: HeaderMap,
    response_status: Option<u16>,
) -> (u16, SnapshotHttp1DownstreamHeaders) {
    (
        response_status.unwrap_or(snapshot.status),
        SnapshotHttp1DownstreamHeaders::Snapshot {
            base: snapshot.headers.clone(),
            overlay: response_headers,
        },
    )
}

pub(crate) fn explicit_snapshot_downstream_response_head(
    snapshot: &HttpUpstreamResponseSnapshot,
    response_headers: HeaderMap,
    response_status: Option<u16>,
) -> (u16, SnapshotHttp1DownstreamHeaders) {
    (
        response_status.unwrap_or(snapshot.status),
        SnapshotHttp1DownstreamHeaders::Explicit(response_headers),
    )
}

pub(crate) fn text_response(status: StatusCode, text: &str) -> Response<Body> {
    let mut response = Response::new(Body::from(text.to_string()));
    *response.status_mut() = status;
    response
}

fn response_from_output(
    body: Vec<u8>,
    headers: HeaderMap,
    status_code: Option<u16>,
) -> Response<Body> {
    let body_is_empty = body.is_empty();
    let mut response = if body_is_empty {
        Response::new(Body::empty())
    } else {
        Response::new(Body::from(body))
    };
    let status = status_code
        .and_then(|code| StatusCode::from_u16(code).ok())
        .unwrap_or(StatusCode::OK);
    *response.status_mut() = status;
    merge_headers(response.headers_mut(), &headers);
    if body_is_empty {
        response
            .headers_mut()
            .entry(CONTENT_LENGTH)
            .or_insert_with(|| HeaderValue::from_static("0"));
    } else if !response.headers().contains_key(CONTENT_TYPE) {
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    }
    response
}

fn response_body_from_downstream_stream(
    receiver: mpsc::UnboundedReceiver<Result<Bytes, io::Error>>,
) -> Body {
    Body::from_stream(try_unfold(receiver, |mut receiver| async move {
        match receiver.recv().await {
            Some(Ok(chunk)) => Ok::<_, io::Error>(Some((chunk, receiver))),
            Some(Err(err)) => Err(err),
            None => Ok(None),
        }
    }))
}

fn response_from_output_stream(
    receiver: mpsc::UnboundedReceiver<Result<Bytes, io::Error>>,
    headers: HeaderMap,
    status_code: Option<u16>,
) -> Response<Body> {
    let mut response = Response::new(response_body_from_downstream_stream(receiver));
    *response.status_mut() = status_code
        .and_then(|code| StatusCode::from_u16(code).ok())
        .unwrap_or(StatusCode::OK);
    merge_headers(response.headers_mut(), &headers);
    response
}

pub(crate) async fn materialize_downstream_response_body_source(
    context: &SharedProxyVmContext,
) -> Result<(), VmError> {
    let exchange = {
        let downstream = context.lock_downstream();
        if downstream.response_output.has_local_body() {
            return Ok(());
        }
        downstream.response_output.body_source_exchange
    };
    let Some(exchange) = exchange else {
        return Ok(());
    };
    let body = read_outbound_exchange_response_all(context, exchange).await?;
    let mut downstream = context.lock_downstream();
    if !downstream.response_output.has_local_body()
        && downstream.response_output.body_source_exchange == Some(exchange)
    {
        downstream.response_output.body_source_exchange = None;
        downstream.response_output.body = Some(body);
        sync_response_output_body_headers(&mut downstream.response_output);
    }
    Ok(())
}

fn response_from_connect_tunnel(headers: HeaderMap) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::OK;
    merge_headers(response.headers_mut(), &headers);
    response.headers_mut().remove(CONTENT_TYPE);
    response.headers_mut().remove(CONTENT_LENGTH);
    response
}

#[cfg(feature = "websocket")]
fn response_from_websocket_tunnel(
    request_headers: &HeaderMap,
    headers: HeaderMap,
    selected_subprotocol: Option<&str>,
) -> Result<Response<Body>, VmError> {
    let request_key = request_headers
        .get("sec-websocket-key")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            VmError::HostError(
                "downstream websocket tunnel requires a valid sec-websocket-key header".to_string(),
            )
        })?;
    let accept = derive_accept_key(request_key.as_bytes());
    let accept = HeaderValue::from_str(&accept).map_err(|err| {
        VmError::HostError(format!(
            "failed to encode websocket accept header for downstream tunnel: {err}",
        ))
    })?;

    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    merge_headers(response.headers_mut(), &headers);
    response
        .headers_mut()
        .insert("connection", HeaderValue::from_static("Upgrade"));
    response
        .headers_mut()
        .insert("upgrade", HeaderValue::from_static("websocket"));
    response
        .headers_mut()
        .insert("sec-websocket-accept", accept);
    if let Some(subprotocol) = selected_subprotocol {
        let value = HeaderValue::from_str(subprotocol).map_err(|err| {
            VmError::HostError(format!(
                "invalid negotiated websocket subprotocol '{subprotocol}': {err}",
            ))
        })?;
        response
            .headers_mut()
            .insert("sec-websocket-protocol", value);
    }
    response.headers_mut().remove(CONTENT_TYPE);
    response.headers_mut().remove(CONTENT_LENGTH);
    Ok(response)
}

async fn streaming_body_from_upstream_snapshot(
    snapshot: &HttpUpstreamResponseSnapshot,
) -> Result<Body, VmError> {
    let mut upstream_body = snapshot.body.lock().await;
    if upstream_body.is_known_empty() {
        return Ok(Body::empty());
    }
    let passthrough = upstream_body.take_streaming_passthrough();
    Ok(Body::new(StreamBody::new(try_unfold(
        passthrough,
        |mut state| async move {
            let frame: Option<Frame<Bytes>> = state
                .next_frame()
                .await
                .map_err(|err: VmError| io::Error::other(err.to_string()))?;
            Ok::<_, io::Error>(frame.map(|frame| (frame, state)))
        },
    ))))
}

fn filtered_snapshot_headers(snapshot: &HttpUpstreamResponseSnapshot) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in snapshot.headers.iter() {
        if !is_hop_by_hop_header(name) {
            headers.insert(name, value.clone());
        }
    }
    headers
}

async fn response_from_upstream_snapshot_head(
    snapshot: HttpUpstreamResponseSnapshot,
    status: u16,
    headers: HeaderMap,
) -> Result<Response<Body>, VmError> {
    let body = streaming_body_from_upstream_snapshot(&snapshot).await?;
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    *response.headers_mut() = headers;
    Ok(response)
}

pub(crate) async fn response_from_upstream_snapshot(
    snapshot: HttpUpstreamResponseSnapshot,
    response_headers: HeaderMap,
    response_status: Option<u16>,
) -> Result<Response<Body>, VmError> {
    let status = response_status.unwrap_or(snapshot.status);
    let mut headers = filtered_snapshot_headers(&snapshot);
    merge_headers(&mut headers, &response_headers);
    response_from_upstream_snapshot_head(snapshot, status, headers).await
}

async fn resolve_http_graph_response_inner(
    context: &SharedProxyVmContext,
    finish_response_stream: bool,
) -> ResolvedHttpGraphResponse {
    let native_fast_path = {
        let mut downstream = context.lock_downstream();
        if downstream.post_response_plan.is_none() && !downstream.response_output.has_local_body() {
            downstream
                .native_default_upstream_forward_response
                .take()
                .map(|response| {
                    downstream.native_default_upstream_http_forward = false;
                    (
                        response,
                        std::mem::take(&mut downstream.response_output.headers),
                        downstream.response_output.status.take(),
                    )
                })
        } else {
            None
        }
    };
    if let Some((response, response_headers, response_status)) = native_fast_path {
        let upstream_latency_ms = response.upstream_latency_ms;
        return ResolvedHttpGraphResponse {
            response: response_from_started_upstream_response(
                response,
                response_headers,
                response_status,
            )
            .await,
            upstream_latency_ms,
            post_response_plan: None,
        };
    }

    let (
        response_body,
        response_stream,
        body_source_exchange,
        response_headers,
        response_status,
        has_post_response_plan,
        has_upstream_target,
        default_upstream_websocket_mode,
        upstream_response,
        native_default_upstream_http_forward,
    ) = match (|| -> Result<_, VmError> {
        let mut downstream = context.lock_downstream();
        if finish_response_stream
            && let Some(stream) = downstream.response_output.body_stream.as_mut()
        {
            stream.finish()?;
        }
        let response_stream = if downstream.response_output.stream_committed() {
            downstream
                .response_output
                .body_stream
                .as_mut()
                .map(DownstreamResponseBodyStream::take_receiver)
                .transpose()?
        } else {
            None
        };
        let exchanges = context.lock_exchanges();
        let default_exchange = exchanges
            .exchanges
            .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
            .expect("default upstream exchange should exist");
        Ok((
            downstream.response_output.body.clone(),
            response_stream,
            downstream.response_output.body_source_exchange,
            downstream.response_output.headers.clone(),
            downstream.response_output.status,
            downstream.post_response_plan.is_some(),
            default_exchange.request.target.is_some(),
            default_exchange.websocket_dag.is_websocket_mode(),
            match &default_exchange.response {
                HttpUpstreamResponseNode::Ready(snapshot) => Some(snapshot.clone()),
                HttpUpstreamResponseNode::NotStarted => None,
            },
            downstream.native_default_upstream_http_forward,
        ))
    })() {
        Ok(values) => values,
        Err(err) => {
            return ResolvedHttpGraphResponse {
                response: text_response(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
                upstream_latency_ms: current_upstream_latency_ms(context),
                post_response_plan: None,
            };
        }
    };

    if has_post_response_plan {
        let plan = context
            .take_downstream_post_response_plan()
            .expect("downstream post-response plan should exist");
        let response = match &plan {
            DownstreamPostResponsePlan::ConnectTunnel(_) => {
                Ok(response_from_connect_tunnel(response_headers))
            }
            #[cfg(feature = "websocket")]
            DownstreamPostResponsePlan::WebSocketTunnel(plan) => {
                context.with_request_head(|request_head| {
                    response_from_websocket_tunnel(
                        request_head.headers(),
                        response_headers,
                        plan.selected_subprotocol.as_deref(),
                    )
                })
            }
        };
        let response = match response {
            Ok(response) => response,
            Err(_) => {
                return ResolvedHttpGraphResponse {
                    response: text_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal server error",
                    ),
                    upstream_latency_ms: current_upstream_latency_ms(context),
                    post_response_plan: None,
                };
            }
        };
        return ResolvedHttpGraphResponse {
            response,
            upstream_latency_ms: current_upstream_latency_ms(context),
            post_response_plan: Some(plan),
        };
    }

    if let Some(body) = response_body {
        return ResolvedHttpGraphResponse {
            response: response_from_output(body, response_headers, response_status),
            upstream_latency_ms: current_upstream_latency_ms(context),
            post_response_plan: None,
        };
    }

    if let Some(receiver) = response_stream {
        return ResolvedHttpGraphResponse {
            response: response_from_output_stream(receiver, response_headers, response_status),
            upstream_latency_ms: current_upstream_latency_ms(context),
            post_response_plan: None,
        };
    }

    if let Some(exchange) = body_source_exchange {
        let snapshot = if exchange == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
            match start_upstream_response(context).await {
                Ok(snapshot) => snapshot,
                Err(UpstreamResponseStartError::MissingTarget) => {
                    return ResolvedHttpGraphResponse {
                        response: text_response(StatusCode::NOT_FOUND, "not found"),
                        upstream_latency_ms: 0,
                        post_response_plan: None,
                    };
                }
                Err(
                    err @ (UpstreamResponseStartError::UnknownExchangeHandle(_)
                    | UpstreamResponseStartError::MissingClient
                    | UpstreamResponseStartError::Protocol(_)
                    | UpstreamResponseStartError::ResolveOutboundBody(_)),
                ) => {
                    return ResolvedHttpGraphResponse {
                        response: text_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &err.as_vm_error().to_string(),
                        ),
                        upstream_latency_ms: current_upstream_latency_ms(context),
                        post_response_plan: None,
                    };
                }
                Err(UpstreamResponseStartError::UpstreamRequest(_)) => {
                    return ResolvedHttpGraphResponse {
                        response: text_response(StatusCode::BAD_GATEWAY, "bad gateway"),
                        upstream_latency_ms: current_upstream_latency_ms(context),
                        post_response_plan: None,
                    };
                }
            }
        } else {
            match start_outbound_exchange_response(context, exchange).await {
                Ok(snapshot) => snapshot,
                Err(UpstreamResponseStartError::MissingTarget) => {
                    return ResolvedHttpGraphResponse {
                        response: text_response(StatusCode::NOT_FOUND, "not found"),
                        upstream_latency_ms: 0,
                        post_response_plan: None,
                    };
                }
                Err(
                    err @ (UpstreamResponseStartError::UnknownExchangeHandle(_)
                    | UpstreamResponseStartError::MissingClient
                    | UpstreamResponseStartError::Protocol(_)
                    | UpstreamResponseStartError::ResolveOutboundBody(_)),
                ) => {
                    return ResolvedHttpGraphResponse {
                        response: text_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &err.as_vm_error().to_string(),
                        ),
                        upstream_latency_ms: outbound_exchange_latency_ms(context, exchange),
                        post_response_plan: None,
                    };
                }
                Err(UpstreamResponseStartError::UpstreamRequest(_)) => {
                    return ResolvedHttpGraphResponse {
                        response: text_response(StatusCode::BAD_GATEWAY, "bad gateway"),
                        upstream_latency_ms: outbound_exchange_latency_ms(context, exchange),
                        post_response_plan: None,
                    };
                }
            }
        };

        let explicit_status = response_status.unwrap_or(snapshot.status);
        let response =
            response_from_upstream_snapshot_head(snapshot, explicit_status, response_headers).await;

        return match response {
            Ok(response) => ResolvedHttpGraphResponse {
                response,
                upstream_latency_ms: outbound_exchange_latency_ms(context, exchange),
                post_response_plan: None,
            },
            Err(_) => ResolvedHttpGraphResponse {
                response: text_response(StatusCode::BAD_GATEWAY, "bad gateway"),
                upstream_latency_ms: outbound_exchange_latency_ms(context, exchange),
                post_response_plan: None,
            },
        };
    }

    if native_default_upstream_http_forward && upstream_response.is_none() {
        if let Ok(Some(resolved)) =
            try_resolve_ready_or_pending_native_default_upstream_forward_response(
                context,
                response_headers.clone(),
                response_status,
            )
            .await
        {
            context.clear_native_default_upstream_http_forward();
            return resolved;
        }
        match try_resolve_native_default_upstream_http_forward_response(
            context,
            response_headers.clone(),
            response_status,
        )
        .await
        {
            Ok(Some(resolved)) => {
                context.clear_native_default_upstream_http_forward();
                return resolved;
            }
            Ok(None) => {}
            Err(UpstreamResponseStartError::MissingTarget) => {}
            Err(
                err @ (UpstreamResponseStartError::UnknownExchangeHandle(_)
                | UpstreamResponseStartError::MissingClient
                | UpstreamResponseStartError::Protocol(_)
                | UpstreamResponseStartError::ResolveOutboundBody(_)),
            ) => {
                context.clear_native_default_upstream_http_forward();
                return ResolvedHttpGraphResponse {
                    response: text_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &err.as_vm_error().to_string(),
                    ),
                    upstream_latency_ms: current_upstream_latency_ms(context),
                    post_response_plan: None,
                };
            }
            Err(UpstreamResponseStartError::UpstreamRequest(_)) => {
                context.clear_native_default_upstream_http_forward();
                return ResolvedHttpGraphResponse {
                    response: text_response(StatusCode::BAD_GATEWAY, "bad gateway"),
                    upstream_latency_ms: current_upstream_latency_ms(context),
                    post_response_plan: None,
                };
            }
        }
    }

    let snapshot = if let Some(snapshot) = upstream_response {
        Some(snapshot)
    } else if has_upstream_target && !default_upstream_websocket_mode {
        match start_upstream_response(context).await {
            Ok(snapshot) => Some(snapshot),
            Err(UpstreamResponseStartError::MissingTarget) => None,
            Err(
                err @ (UpstreamResponseStartError::UnknownExchangeHandle(_)
                | UpstreamResponseStartError::MissingClient
                | UpstreamResponseStartError::Protocol(_)
                | UpstreamResponseStartError::ResolveOutboundBody(_)),
            ) => {
                return ResolvedHttpGraphResponse {
                    response: text_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &err.as_vm_error().to_string(),
                    ),
                    upstream_latency_ms: current_upstream_latency_ms(context),
                    post_response_plan: None,
                };
            }
            Err(UpstreamResponseStartError::UpstreamRequest(_)) => {
                return ResolvedHttpGraphResponse {
                    response: text_response(StatusCode::BAD_GATEWAY, "bad gateway"),
                    upstream_latency_ms: current_upstream_latency_ms(context),
                    post_response_plan: None,
                };
            }
        }
    } else {
        None
    };

    let Some(snapshot) = snapshot else {
        return ResolvedHttpGraphResponse {
            response: text_response(StatusCode::NOT_FOUND, "not found"),
            upstream_latency_ms: 0,
            post_response_plan: None,
        };
    };

    match response_from_upstream_snapshot(snapshot, response_headers, response_status).await {
        Ok(response) => ResolvedHttpGraphResponse {
            response,
            upstream_latency_ms: current_upstream_latency_ms(context),
            post_response_plan: None,
        },
        Err(_) => ResolvedHttpGraphResponse {
            response: text_response(StatusCode::BAD_GATEWAY, "bad gateway"),
            upstream_latency_ms: current_upstream_latency_ms(context),
            post_response_plan: None,
        },
    }
}

pub(crate) async fn resolve_http_graph_response(
    context: &SharedProxyVmContext,
) -> ResolvedHttpGraphResponse {
    resolve_http_graph_response_inner(context, true).await
}

pub(crate) async fn resolve_committed_http_graph_response(
    context: &SharedProxyVmContext,
) -> ResolvedHttpGraphResponse {
    resolve_http_graph_response_inner(context, false).await
}
