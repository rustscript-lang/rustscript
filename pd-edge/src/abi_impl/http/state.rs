use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};

use axum::{
    body::Body,
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode,
        header::{CONTENT_LENGTH, CONTENT_TYPE, HOST},
    },
};
use http_body_util::BodyExt;
use url::Url;
use vm::VmError;

use super::super::{EDGE_IO_HANDLE_DYNAMIC_BASE, EdgeVirtualIoHandle, SharedRateLimiter};

#[derive(Debug)]
pub struct HttpRequestContext {
    pub request_id: String,
    pub method: Method,
    pub path: String,
    pub query: String,
    pub http_version: String,
    pub port: u16,
    pub scheme: String,
    pub host: String,
    pub client_ip: String,
    pub body: Body,
    pub headers: HeaderMap,
}

#[derive(Clone, Debug)]
pub(crate) struct HttpRequestHead {
    pub(crate) request_id: String,
    pub(crate) method: Method,
    pub(crate) path: String,
    pub(crate) query: String,
    pub(crate) http_version: String,
    pub(crate) port: u16,
    pub(crate) scheme: String,
    pub(crate) host: String,
    pub(crate) client_ip: String,
    pub(crate) headers: HeaderMap,
}

pub(crate) struct InboundRequestBodyState {
    body: Option<Body>,
    buffered: Vec<u8>,
    read_offset: usize,
    eof: bool,
}

impl std::fmt::Debug for InboundRequestBodyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboundRequestBodyState")
            .field("buffered_len", &self.buffered.len())
            .field("read_offset", &self.read_offset)
            .field("eof", &self.eof)
            .finish()
    }
}

impl InboundRequestBodyState {
    fn new(body: Body) -> Self {
        Self {
            body: Some(body),
            buffered: Vec::new(),
            read_offset: 0,
            eof: false,
        }
    }

    async fn pull_next_frame(&mut self) -> Result<(), VmError> {
        if self.eof {
            return Ok(());
        }
        let Some(body) = self.body.as_mut() else {
            self.eof = true;
            return Ok(());
        };

        match body.frame().await {
            Some(Ok(frame)) => {
                if let Ok(chunk) = frame.into_data()
                    && !chunk.is_empty()
                {
                    self.buffered.extend_from_slice(&chunk);
                }
            }
            Some(Err(err)) => {
                return Err(VmError::HostError(format!(
                    "failed to read inbound request body frame: {err}",
                )));
            }
            None => {
                self.eof = true;
                self.body = None;
            }
        }
        Ok(())
    }

    async fn ensure_readable_byte(&mut self) -> Result<(), VmError> {
        while self.read_offset >= self.buffered.len() && !self.eof {
            self.pull_next_frame().await?;
        }
        Ok(())
    }

    async fn read_next_chunk(&mut self, max_bytes: usize) -> Result<Vec<u8>, VmError> {
        self.ensure_readable_byte().await?;
        if self.read_offset >= self.buffered.len() {
            return Ok(Vec::new());
        }
        let end = self
            .read_offset
            .saturating_add(max_bytes)
            .min(self.buffered.len());
        let chunk = self.buffered[self.read_offset..end].to_vec();
        self.read_offset = end;
        Ok(chunk)
    }

    async fn read_next_line(&mut self) -> Result<Vec<u8>, VmError> {
        loop {
            let start = self.read_offset.min(self.buffered.len());
            if start < self.buffered.len() {
                if let Some(rel_end) = self.buffered[start..]
                    .iter()
                    .position(|byte| *byte == b'\n')
                {
                    let end = start + rel_end;
                    let line = self.buffered[start..end].to_vec();
                    self.read_offset = end.saturating_add(1);
                    return Ok(line);
                }
                if self.eof {
                    let line = self.buffered[start..].to_vec();
                    self.read_offset = self.buffered.len();
                    return Ok(line);
                }
            } else if self.eof {
                return Ok(Vec::new());
            }

            self.pull_next_frame().await?;
        }
    }

    async fn read_all_and_consume(&mut self) -> Result<Vec<u8>, VmError> {
        let body = self.read_all().await?;
        self.read_offset = self.buffered.len();
        Ok(body)
    }

    async fn read_all(&mut self) -> Result<Vec<u8>, VmError> {
        while !self.eof {
            self.pull_next_frame().await?;
        }
        Ok(self.buffered.clone())
    }

    async fn eof(&mut self) -> Result<bool, VmError> {
        while self.read_offset >= self.buffered.len() && !self.eof {
            self.pull_next_frame().await?;
        }
        Ok(self.eof && self.read_offset >= self.buffered.len())
    }
}

type SharedInboundRequestBody = Arc<tokio::sync::Mutex<InboundRequestBodyState>>;

#[derive(Clone, Debug)]
pub(crate) struct HttpOutboundRequestNode {
    pub(crate) method: Method,
    pub(crate) path: String,
    pub(crate) query: String,
    pub(crate) headers: HeaderMap,
    pub(crate) body_override: Option<Vec<u8>>,
    pub(crate) target: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct HttpResponseOutputNode {
    pub(crate) headers: HeaderMap,
    pub(crate) body: Option<String>,
    pub(crate) status: Option<u16>,
}

struct UpstreamResponseBodyState {
    response: Option<reqwest::Response>,
    buffered: Vec<u8>,
    read_offset: usize,
    eof: bool,
}

impl std::fmt::Debug for UpstreamResponseBodyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamResponseBodyState")
            .field("buffered_len", &self.buffered.len())
            .field("read_offset", &self.read_offset)
            .field("eof", &self.eof)
            .finish()
    }
}

impl UpstreamResponseBodyState {
    fn new(response: reqwest::Response) -> Self {
        Self {
            response: Some(response),
            buffered: Vec::new(),
            read_offset: 0,
            eof: false,
        }
    }

    async fn pull_next_chunk(&mut self) -> Result<(), VmError> {
        if self.eof {
            return Ok(());
        }
        let Some(response) = self.response.as_mut() else {
            self.eof = true;
            return Ok(());
        };

        match response.chunk().await {
            Ok(Some(chunk)) => {
                if !chunk.is_empty() {
                    self.buffered.extend_from_slice(&chunk);
                }
            }
            Ok(None) => {
                self.eof = true;
                self.response = None;
            }
            Err(err) => {
                return Err(VmError::HostError(format!(
                    "failed to read upstream response chunk: {err}",
                )));
            }
        }
        Ok(())
    }

    async fn ensure_readable_byte(&mut self) -> Result<(), VmError> {
        while self.read_offset >= self.buffered.len() && !self.eof {
            self.pull_next_chunk().await?;
        }
        Ok(())
    }

    async fn read_next_chunk(&mut self, max_bytes: usize) -> Result<Vec<u8>, VmError> {
        self.ensure_readable_byte().await?;
        if self.read_offset >= self.buffered.len() {
            return Ok(Vec::new());
        }
        let end = self
            .read_offset
            .saturating_add(max_bytes)
            .min(self.buffered.len());
        let chunk = self.buffered[self.read_offset..end].to_vec();
        self.read_offset = end;
        Ok(chunk)
    }

    async fn read_next_line(&mut self) -> Result<Vec<u8>, VmError> {
        loop {
            let start = self.read_offset.min(self.buffered.len());
            if start < self.buffered.len() {
                if let Some(rel_end) = self.buffered[start..]
                    .iter()
                    .position(|byte| *byte == b'\n')
                {
                    let end = start + rel_end;
                    let line = self.buffered[start..end].to_vec();
                    self.read_offset = end.saturating_add(1);
                    return Ok(line);
                }
                if self.eof {
                    let line = self.buffered[start..].to_vec();
                    self.read_offset = self.buffered.len();
                    return Ok(line);
                }
            } else if self.eof {
                return Ok(Vec::new());
            }

            self.pull_next_chunk().await?;
        }
    }

    async fn read_all(&mut self) -> Result<Vec<u8>, VmError> {
        while !self.eof {
            self.pull_next_chunk().await?;
        }
        Ok(self.buffered.clone())
    }

    async fn eof(&mut self) -> Result<bool, VmError> {
        while self.read_offset >= self.buffered.len() && !self.eof {
            self.pull_next_chunk().await?;
        }
        Ok(self.eof && self.read_offset >= self.buffered.len())
    }
}

type SharedUpstreamResponseBody = Arc<tokio::sync::Mutex<UpstreamResponseBodyState>>;

#[derive(Clone)]
pub(crate) struct HttpUpstreamResponseSnapshot {
    pub(crate) status: u16,
    pub(crate) headers: HeaderMap,
    body: SharedUpstreamResponseBody,
}

impl std::fmt::Debug for HttpUpstreamResponseSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpUpstreamResponseSnapshot")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .finish()
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) enum HttpUpstreamResponseNode {
    #[default]
    NotStarted,
    Ready(HttpUpstreamResponseSnapshot),
}

#[derive(Clone, Debug)]
pub struct ProxyVmContext {
    pub(crate) request_head: HttpRequestHead,
    pub(crate) inbound_request_body: SharedInboundRequestBody,
    pub(crate) outbound_request: HttpOutboundRequestNode,
    pub(crate) response_output: HttpResponseOutputNode,
    pub(crate) upstream_response: HttpUpstreamResponseNode,
    pub(crate) upstream_client: Option<reqwest::Client>,
    pub(crate) upstream_latency_ms: u64,
    pub(crate) rate_limiter: SharedRateLimiter,
    pub(crate) edge_io_next_handle: i64,
    pub(crate) edge_io_handles: HashMap<i64, EdgeVirtualIoHandle>,
}

impl ProxyVmContext {
    pub fn from_http_request(request: HttpRequestContext, rate_limiter: SharedRateLimiter) -> Self {
        let request_headers = request.headers;
        let request_head = HttpRequestHead {
            request_id: request.request_id,
            method: request.method,
            path: request.path,
            query: request.query,
            http_version: request.http_version,
            port: request.port,
            scheme: request.scheme,
            host: request.host,
            client_ip: request.client_ip,
            headers: HeaderMap::new(),
        };
        Self {
            outbound_request: HttpOutboundRequestNode {
                method: request_head.method.clone(),
                path: request_head.path.clone(),
                query: request_head.query.clone(),
                headers: request_headers.clone(),
                body_override: None,
                target: None,
            },
            request_head: HttpRequestHead {
                headers: request_headers,
                ..request_head
            },
            inbound_request_body: Arc::new(tokio::sync::Mutex::new(InboundRequestBodyState::new(
                request.body,
            ))),
            response_output: HttpResponseOutputNode::default(),
            upstream_response: HttpUpstreamResponseNode::NotStarted,
            upstream_client: None,
            upstream_latency_ms: 0,
            rate_limiter,
            edge_io_next_handle: EDGE_IO_HANDLE_DYNAMIC_BASE,
            edge_io_handles: HashMap::new(),
        }
    }

    pub fn from_request_headers(
        request_headers: HeaderMap,
        rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self::from_http_request(
            HttpRequestContext {
                request_id: String::new(),
                method: Method::GET,
                path: "/".to_string(),
                query: String::new(),
                http_version: "1.1".to_string(),
                port: 80,
                scheme: "http".to_string(),
                host: String::new(),
                client_ip: String::new(),
                body: Body::empty(),
                headers: request_headers,
            },
            rate_limiter,
        )
    }

    pub fn attach_upstream_client(&mut self, client: reqwest::Client) {
        self.upstream_client = Some(client);
    }

    fn upstream_response(&self) -> Result<HttpUpstreamResponseSnapshot, VmError> {
        match &self.upstream_response {
            HttpUpstreamResponseNode::Ready(snapshot) => Ok(snapshot.clone()),
            HttpUpstreamResponseNode::NotStarted => Err(VmError::HostError(
                "upstream response is unavailable before upstream exchange".to_string(),
            )),
        }
    }

    fn upstream_response_ready(&self) -> bool {
        matches!(self.upstream_response, HttpUpstreamResponseNode::Ready(_))
    }

    fn store_upstream_response(
        &mut self,
        status: u16,
        headers: HeaderMap,
        body: SharedUpstreamResponseBody,
        latency_ms: u64,
    ) {
        self.upstream_response = HttpUpstreamResponseNode::Ready(HttpUpstreamResponseSnapshot {
            status,
            headers,
            body,
        });
        self.upstream_latency_ms = latency_ms;
    }
}

pub type SharedProxyVmContext = Arc<Mutex<ProxyVmContext>>;

#[derive(Debug)]
enum UpstreamResponseStartError {
    MissingTarget,
    MissingClient,
    ResolveOutboundBody(String),
    UpstreamRequest(String),
}

impl UpstreamResponseStartError {
    fn as_vm_error(&self) -> VmError {
        match self {
            Self::MissingTarget => VmError::HostError(
                "upstream target is unavailable before http::upstream::request::set_target"
                    .to_string(),
            ),
            Self::MissingClient => VmError::HostError(
                "upstream client is unavailable outside the HTTP data plane".to_string(),
            ),
            Self::ResolveOutboundBody(message) | Self::UpstreamRequest(message) => {
                VmError::HostError(message.clone())
            }
        }
    }
}

#[derive(Clone, Debug)]
struct PreparedUpstreamRequest {
    client: reqwest::Client,
    method: Method,
    path: String,
    query: String,
    headers: HeaderMap,
    target: String,
}

#[derive(Debug)]
pub(crate) struct ResolvedHttpGraphResponse {
    pub response: Response<Body>,
    pub upstream_latency_ms: u64,
}

pub async fn resolve_outbound_request_body(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let (body_override, inbound_body) = {
        let guard = context.lock().expect("vm context lock poisoned");
        (
            guard.outbound_request.body_override.clone(),
            guard.inbound_request_body.clone(),
        )
    };

    if let Some(body) = body_override {
        return Ok(body);
    }

    let mut inbound = inbound_body.lock().await;
    inbound.read_all().await
}

pub(crate) fn build_upstream_url(
    upstream: &str,
    request_path: &str,
    request_query: &str,
) -> (String, Option<String>) {
    let path = if request_path.is_empty() {
        "/"
    } else {
        request_path
    };
    let path_and_query = if request_query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{request_query}")
    };

    if let Ok(url) = Url::parse(upstream) {
        let mut final_url = url;
        let needs_path = final_url.path() == "/" && final_url.query().is_none();
        if needs_path && path_and_query != "/" {
            let base = final_url[..url::Position::AfterPort].to_string();
            let merged = format!("{base}{path_and_query}");
            if let Ok(joined) = Url::parse(&merged) {
                final_url = joined;
            }
        }
        let host = final_url.host_str().map(|host| {
            if let Some(port) = final_url.port() {
                format!("{host}:{port}")
            } else {
                host.to_string()
            }
        });
        return (final_url.to_string(), host);
    }

    let upstream_url = format!("http://{}{path_and_query}", upstream);
    (upstream_url, Some(upstream.to_string()))
}

pub(crate) fn is_hop_by_hop_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn prepared_upstream_request(
    context: &SharedProxyVmContext,
) -> Result<PreparedUpstreamRequest, UpstreamResponseStartError> {
    let guard = context.lock().expect("vm context lock poisoned");
    Ok(PreparedUpstreamRequest {
        client: guard
            .upstream_client
            .clone()
            .ok_or(UpstreamResponseStartError::MissingClient)?,
        method: guard.outbound_request.method.clone(),
        path: guard.outbound_request.path.clone(),
        query: guard.outbound_request.query.clone(),
        headers: guard.outbound_request.headers.clone(),
        target: guard
            .outbound_request
            .target
            .clone()
            .ok_or(UpstreamResponseStartError::MissingTarget)?,
    })
}

async fn start_upstream_response(
    context: &SharedProxyVmContext,
) -> Result<HttpUpstreamResponseSnapshot, UpstreamResponseStartError> {
    {
        let guard = context.lock().expect("vm context lock poisoned");
        if let Ok(snapshot) = guard.upstream_response() {
            return Ok(snapshot);
        }
    }

    let prepared = prepared_upstream_request(context)?;
    let request_body = resolve_outbound_request_body(context)
        .await
        .map_err(|err| {
            UpstreamResponseStartError::ResolveOutboundBody(format!(
                "failed to resolve outbound request body: {err}",
            ))
        })?;
    let (upstream_url, host_header) =
        build_upstream_url(&prepared.target, &prepared.path, &prepared.query);

    let mut outbound = prepared
        .client
        .request(prepared.method.clone(), upstream_url)
        .body(request_body);
    for (name, value) in &prepared.headers {
        if name != HOST && name != CONTENT_LENGTH && !is_hop_by_hop_header(name) {
            outbound = outbound.header(name, value);
        }
    }
    if let Some(host) = host_header {
        outbound = outbound.header(HOST, host);
    }

    let started = Instant::now();
    let upstream_response = outbound.send().await.map_err(|err| {
        UpstreamResponseStartError::UpstreamRequest(format!(
            "upstream request failed while evaluating host call: {err}",
        ))
    })?;
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let snapshot = HttpUpstreamResponseSnapshot {
        status: upstream_response.status().as_u16(),
        headers: upstream_response.headers().clone(),
        body: Arc::new(tokio::sync::Mutex::new(UpstreamResponseBodyState::new(
            upstream_response,
        ))),
    };

    let mut guard = context.lock().expect("vm context lock poisoned");
    if let Ok(existing) = guard.upstream_response() {
        return Ok(existing);
    }
    guard.store_upstream_response(
        snapshot.status,
        snapshot.headers.clone(),
        snapshot.body.clone(),
        latency_ms,
    );
    Ok(snapshot)
}

pub(crate) async fn ensure_upstream_response_started(
    context: &SharedProxyVmContext,
) -> Result<HttpUpstreamResponseSnapshot, VmError> {
    start_upstream_response(context)
        .await
        .map_err(|err| err.as_vm_error())
}

pub(crate) fn upstream_response_available(context: &SharedProxyVmContext) -> bool {
    let guard = context.lock().expect("vm context lock poisoned");
    guard.upstream_response_ready()
}

pub(crate) async fn read_upstream_response_all(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let snapshot = ensure_upstream_response_started(context).await?;
    let mut body = snapshot.body.lock().await;
    body.read_all().await
}

pub(crate) async fn read_upstream_response_next_chunk(
    context: &SharedProxyVmContext,
    max_bytes: usize,
) -> Result<Vec<u8>, VmError> {
    let snapshot = ensure_upstream_response_started(context).await?;
    let mut body = snapshot.body.lock().await;
    body.read_next_chunk(max_bytes).await
}

pub(crate) async fn read_upstream_response_next_line(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let snapshot = ensure_upstream_response_started(context).await?;
    let mut body = snapshot.body.lock().await;
    body.read_next_line().await
}

pub(crate) async fn upstream_response_eof(context: &SharedProxyVmContext) -> Result<bool, VmError> {
    let snapshot = ensure_upstream_response_started(context).await?;
    let mut body = snapshot.body.lock().await;
    body.eof().await
}

fn current_upstream_latency_ms(context: &SharedProxyVmContext) -> u64 {
    let guard = context.lock().expect("vm context lock poisoned");
    guard.upstream_latency_ms
}

fn merge_headers(target: &mut HeaderMap, overlay: &HeaderMap) {
    for (name, value) in overlay {
        target.insert(name, value.clone());
    }
}

fn text_response(status: StatusCode, text: &str) -> Response<Body> {
    let mut response = Response::new(Body::from(text.to_string()));
    *response.status_mut() = status;
    response
}

fn response_from_output(
    body: String,
    headers: HeaderMap,
    status_code: Option<u16>,
) -> Response<Body> {
    let mut response = Response::new(Body::from(body));
    let status = status_code
        .and_then(|code| StatusCode::from_u16(code).ok())
        .unwrap_or(StatusCode::OK);
    *response.status_mut() = status;
    merge_headers(response.headers_mut(), &headers);
    if !response.headers().contains_key(CONTENT_TYPE) {
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    }
    response
}

async fn response_from_upstream_snapshot(
    snapshot: HttpUpstreamResponseSnapshot,
    response_headers: HeaderMap,
    response_status: Option<u16>,
) -> Result<Response<Body>, VmError> {
    let body = {
        let mut upstream_body = snapshot.body.lock().await;
        upstream_body.read_all().await?
    };
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::from_u16(snapshot.status).unwrap_or(StatusCode::OK);
    for (name, value) in &snapshot.headers {
        if !is_hop_by_hop_header(name) {
            response.headers_mut().insert(name, value.clone());
        }
    }
    if let Some(status) = response_status.and_then(|code| StatusCode::from_u16(code).ok()) {
        *response.status_mut() = status;
    }
    merge_headers(response.headers_mut(), &response_headers);
    Ok(response)
}

pub(crate) async fn resolve_http_graph_response(
    context: &SharedProxyVmContext,
) -> ResolvedHttpGraphResponse {
    let (response_body, response_headers, response_status, has_upstream_target, upstream_response) = {
        let guard = context.lock().expect("vm context lock poisoned");
        (
            guard.response_output.body.clone(),
            guard.response_output.headers.clone(),
            guard.response_output.status,
            guard.outbound_request.target.is_some(),
            match &guard.upstream_response {
                HttpUpstreamResponseNode::Ready(snapshot) => Some(snapshot.clone()),
                HttpUpstreamResponseNode::NotStarted => None,
            },
        )
    };

    if let Some(body) = response_body {
        return ResolvedHttpGraphResponse {
            response: response_from_output(body, response_headers, response_status),
            upstream_latency_ms: current_upstream_latency_ms(context),
        };
    }

    let snapshot = if let Some(snapshot) = upstream_response {
        Some(snapshot)
    } else if has_upstream_target {
        match start_upstream_response(context).await {
            Ok(snapshot) => Some(snapshot),
            Err(UpstreamResponseStartError::MissingTarget) => None,
            Err(UpstreamResponseStartError::MissingClient)
            | Err(UpstreamResponseStartError::ResolveOutboundBody(_)) => {
                return ResolvedHttpGraphResponse {
                    response: text_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal server error",
                    ),
                    upstream_latency_ms: current_upstream_latency_ms(context),
                };
            }
            Err(UpstreamResponseStartError::UpstreamRequest(_)) => {
                return ResolvedHttpGraphResponse {
                    response: text_response(StatusCode::BAD_GATEWAY, "bad gateway"),
                    upstream_latency_ms: current_upstream_latency_ms(context),
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
        };
    };

    match response_from_upstream_snapshot(snapshot, response_headers, response_status).await {
        Ok(response) => ResolvedHttpGraphResponse {
            response,
            upstream_latency_ms: current_upstream_latency_ms(context),
        },
        Err(_) => ResolvedHttpGraphResponse {
            response: text_response(StatusCode::BAD_GATEWAY, "bad gateway"),
            upstream_latency_ms: current_upstream_latency_ms(context),
        },
    }
}

pub(crate) async fn read_request_body_all(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let body = {
        let guard = context.lock().expect("vm context lock poisoned");
        guard.inbound_request_body.clone()
    };
    let mut inbound = body.lock().await;
    inbound.read_all().await
}

pub(crate) async fn consume_request_body_all(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let body = {
        let guard = context.lock().expect("vm context lock poisoned");
        guard.inbound_request_body.clone()
    };
    let mut inbound = body.lock().await;
    inbound.read_all_and_consume().await
}

pub(crate) async fn read_request_body_next_chunk(
    context: &SharedProxyVmContext,
    max_bytes: usize,
) -> Result<Vec<u8>, VmError> {
    let body = {
        let guard = context.lock().expect("vm context lock poisoned");
        guard.inbound_request_body.clone()
    };
    let mut inbound = body.lock().await;
    inbound.read_next_chunk(max_bytes).await
}

pub(crate) async fn read_request_body_next_line(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let body = {
        let guard = context.lock().expect("vm context lock poisoned");
        guard.inbound_request_body.clone()
    };
    let mut inbound = body.lock().await;
    inbound.read_next_line().await
}

pub(crate) async fn request_body_eof(context: &SharedProxyVmContext) -> Result<bool, VmError> {
    let body = {
        let guard = context.lock().expect("vm context lock poisoned");
        guard.inbound_request_body.clone()
    };
    let mut inbound = body.lock().await;
    inbound.eof().await
}
