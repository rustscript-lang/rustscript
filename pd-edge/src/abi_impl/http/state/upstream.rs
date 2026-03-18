use super::super::helpers::request_path_with_query;
use super::*;

pub(super) enum UpstreamResponseStartError {
    UnknownExchangeHandle(i64),
    MissingTarget,
    MissingClient,
    Protocol(String),
    ResolveOutboundBody(String),
    UpstreamRequest(String),
}

impl UpstreamResponseStartError {
    pub(super) fn as_vm_error(&self) -> VmError {
        match self {
            Self::UnknownExchangeHandle(handle) => {
                VmError::HostError(format!("unknown outbound exchange handle {handle}"))
            }
            Self::MissingTarget => VmError::HostError(
                "upstream target is unavailable before configuring the default upstream exchange target"
                    .to_string(),
            ),
            Self::MissingClient => VmError::HostError(
                "upstream client is unavailable outside the HTTP data plane".to_string(),
            ),
            Self::Protocol(message)
            | Self::ResolveOutboundBody(message)
            | Self::UpstreamRequest(message) => VmError::HostError(message.clone()),
        }
    }
}

#[derive(Clone, Debug)]
struct PreparedUpstreamRequest {
    http2_sessions: Option<SharedHttpUpstreamSessions>,
    http3_sessions: Option<SharedHttp3UpstreamSessions>,
    version_preference: HttpVersionPreference,
    http2_mode: http2::Http2UpstreamMode,
    http3_mode: http3::Http3UpstreamMode,
    tls_flow: TlsFlowState,
    attached_transport: Option<AttachedHttpTransport>,
    method: Method,
    path: String,
    query: String,
    headers: HeaderMap,
    target: Arc<str>,
    target_host: Option<Arc<str>>,
    target_port: Option<u16>,
    target_host_header: Option<Arc<str>>,
    target_authority: Option<Arc<str>>,
    target_plain_http1_pool_key: Option<Arc<str>>,
    target_inherits_request_path: bool,
    target_scheme: HttpUpstreamScheme,
}

#[derive(Clone, Debug)]
enum DefaultUpstreamRequestHead {
    Inherit,
    InheritOverrides {
        header_overrides: HeaderMap,
    },
    Explicit {
        method: Method,
        path: String,
        query: String,
        headers: HeaderMap,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct DefaultUpstreamRequestSnapshot {
    version_preference: HttpVersionPreference,
    target: Option<Arc<str>>,
    target_host_arc: Option<Arc<str>>,
    target_port: Option<u16>,
    target_authority: Option<Arc<str>>,
    target_plain_http1_pool_key: Option<Arc<str>>,
    target_scheme: HttpUpstreamScheme,
    head: DefaultUpstreamRequestHead,
}

impl DefaultUpstreamRequestSnapshot {
    pub(crate) fn from_request(request: &HttpOutboundRequestNode) -> Self {
        let head = if request.inherits_request_head {
            if request.inherited_header_overrides.is_empty() {
                DefaultUpstreamRequestHead::Inherit
            } else {
                DefaultUpstreamRequestHead::InheritOverrides {
                    header_overrides: request.inherited_header_overrides.clone(),
                }
            }
        } else {
            DefaultUpstreamRequestHead::Explicit {
                method: request.method.clone(),
                path: request.path.clone(),
                query: request.query.clone(),
                headers: request.headers.clone(),
            }
        };
        Self {
            version_preference: request.version_preference,
            target: request.target.as_deref().map(Arc::from),
            target_host_arc: request
                .target_host_arc
                .clone()
                .or_else(|| request.target_host.as_deref().map(Arc::from)),
            target_port: request.target_port,
            target_authority: request.target_authority.clone(),
            target_plain_http1_pool_key: request.target_plain_http1_pool_key.clone(),
            target_scheme: request.target_scheme,
            head,
        }
    }

    fn method_or_request_head<'a>(&'a self, request_head: &'a HttpRequestHead) -> &'a Method {
        match &self.head {
            DefaultUpstreamRequestHead::Inherit
            | DefaultUpstreamRequestHead::InheritOverrides { .. } => request_head.method(),
            DefaultUpstreamRequestHead::Explicit { method, .. } => method,
        }
    }

    fn path_or_request_head<'a>(&'a self, request_head: &'a HttpRequestHead) -> &'a str {
        match &self.head {
            DefaultUpstreamRequestHead::Inherit
            | DefaultUpstreamRequestHead::InheritOverrides { .. } => request_head.path(),
            DefaultUpstreamRequestHead::Explicit { path, .. } => path,
        }
    }

    fn query_or_request_head<'a>(&'a self, request_head: &'a HttpRequestHead) -> &'a str {
        match &self.head {
            DefaultUpstreamRequestHead::Inherit
            | DefaultUpstreamRequestHead::InheritOverrides { .. } => request_head.query(),
            DefaultUpstreamRequestHead::Explicit { query, .. } => query,
        }
    }

    fn cloned_headers_or_request_head(&self, request_head: &HttpRequestHead) -> HeaderMap {
        match &self.head {
            DefaultUpstreamRequestHead::Inherit => request_head.headers().clone(),
            DefaultUpstreamRequestHead::InheritOverrides { header_overrides } => {
                let mut headers = request_head.headers().clone();
                merge_headers(&mut headers, header_overrides);
                headers
            }
            DefaultUpstreamRequestHead::Explicit { headers, .. } => headers.clone(),
        }
    }

    fn filtered_headers_or_request_head(
        &self,
        request_head: &HttpRequestHead,
        host_header: Option<&str>,
    ) -> HeaderMap {
        match &self.head {
            DefaultUpstreamRequestHead::Inherit => {
                filtered_upstream_headers(request_head.headers(), host_header)
            }
            DefaultUpstreamRequestHead::InheritOverrides { header_overrides } => {
                let mut headers = filtered_upstream_headers(request_head.headers(), host_header);
                merge_headers(&mut headers, header_overrides);
                headers
            }
            DefaultUpstreamRequestHead::Explicit { headers, .. } => {
                filtered_upstream_headers(headers, host_header)
            }
        }
    }

    fn outbound_http1_headers_or_request_head(
        &self,
        request_head: &HttpRequestHead,
        host_header: Option<Arc<str>>,
    ) -> OutboundHttp1RequestHeaders {
        match &self.head {
            DefaultUpstreamRequestHead::Inherit => OutboundHttp1RequestHeaders::InheritedFiltered {
                headers: request_head.lazy_headers().clone(),
                host_header,
            },
            _ => self
                .filtered_headers_or_request_head(request_head, host_header.as_deref())
                .into(),
        }
    }
}

struct StartedUpstreamResponse {
    status: u16,
    headers: HeaderMap,
    version: Version,
    carrier_ref: HttpCarrierRef,
    peer_addr: Option<String>,
    negotiated_alpn: Option<String>,
    peer_certificate_der: Option<Vec<u8>>,
    body: SharedUpstreamResponseBody,
}

impl std::fmt::Debug for StartedUpstreamResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartedUpstreamResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("version", &self.version)
            .field("carrier_ref", &self.carrier_ref)
            .field("peer_addr", &self.peer_addr)
            .field("negotiated_alpn", &self.negotiated_alpn)
            .field(
                "peer_certificate_der_len",
                &self.peer_certificate_der.as_ref().map(Vec::len),
            )
            .finish()
    }
}

type NativeDefaultUpstreamForwardBody = OutboundHttp1ForwardBody;

pub(super) type NativeDefaultUpstreamForwardResponse = OutboundHttp1ForwardResponse;

#[derive(Debug)]
pub(crate) struct ResolvedNativeHttp1DownstreamResponse {
    pub(crate) response: NativeDefaultUpstreamForwardResponse,
    pub(crate) response_headers: HeaderMap,
    pub(crate) response_status: Option<u16>,
    pub(crate) upstream_latency_ms: u64,
}

#[derive(Debug)]
pub(crate) struct ResolvedNativeLocalHttp1DownstreamResponse {
    pub(crate) status: u16,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Vec<u8>,
    pub(crate) default_content_type: bool,
}

pub(crate) struct DownstreamHttpBodyPassthrough {
    inner: StreamingUpstreamResponseBodyState,
}

impl DownstreamHttpBodyPassthrough {
    pub(crate) async fn next_frame(&mut self) -> Result<Option<Frame<Bytes>>, VmError> {
        self.inner.next_frame().await
    }
}

pub(crate) enum SnapshotHttp1DownstreamHeaders {
    Snapshot {
        base: Arc<HeaderMap>,
        overlay: HeaderMap,
    },
    Explicit(HeaderMap),
}

impl SnapshotHttp1DownstreamHeaders {
    pub(crate) fn contains_name(&self, name: HeaderName) -> bool {
        match self {
            Self::Snapshot { base, overlay } => {
                overlay.contains_key(&name)
                    || (!is_hop_by_hop_header(&name) && base.contains_key(&name))
            }
            Self::Explicit(headers) => headers.contains_key(&name),
        }
    }

    pub(crate) fn header_contains_token(&self, name: HeaderName, token: &str) -> bool {
        let contains = |headers: &HeaderMap| {
            headers
                .get_all(&name)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .flat_map(|value| value.split(','))
                .map(str::trim)
                .any(|value| value.eq_ignore_ascii_case(token))
        };
        match self {
            Self::Snapshot { base, overlay } => {
                if overlay.contains_key(&name) {
                    contains(overlay)
                } else if is_hop_by_hop_header(&name) {
                    false
                } else {
                    contains(base)
                }
            }
            Self::Explicit(headers) => contains(headers),
        }
    }

    pub(crate) fn connection_keep_alive(&self, version: Version) -> bool {
        let connection_close = self.header_contains_token(CONNECTION, "close");
        let connection_keep_alive = self.header_contains_token(CONNECTION, "keep-alive");
        match version {
            Version::HTTP_10 => connection_keep_alive && !connection_close,
            _ => !connection_close,
        }
    }

    pub(crate) fn insert_override(&mut self, name: HeaderName, value: HeaderValue) {
        match self {
            Self::Snapshot { overlay, .. } => {
                overlay.insert(name, value);
            }
            Self::Explicit(headers) => {
                headers.insert(name, value);
            }
        }
    }

    pub(crate) fn write_http1_lines(&self, head: &mut bytes::BytesMut) {
        match self {
            Self::Snapshot { base, overlay } => {
                let overridden: HashSet<HeaderName> = overlay.keys().cloned().collect();
                for (name, value) in base.iter() {
                    if overridden.contains(name) || is_hop_by_hop_header(name) {
                        continue;
                    }
                    head.extend_from_slice(name.as_str().as_bytes());
                    head.extend_from_slice(b": ");
                    head.extend_from_slice(value.as_bytes());
                    head.extend_from_slice(b"\r\n");
                }
                for (name, value) in overlay.iter() {
                    head.extend_from_slice(name.as_str().as_bytes());
                    head.extend_from_slice(b": ");
                    head.extend_from_slice(value.as_bytes());
                    head.extend_from_slice(b"\r\n");
                }
            }
            Self::Explicit(headers) => {
                for (name, value) in headers.iter() {
                    head.extend_from_slice(name.as_str().as_bytes());
                    head.extend_from_slice(b": ");
                    head.extend_from_slice(value.as_bytes());
                    head.extend_from_slice(b"\r\n");
                }
            }
        }
    }
}

pub(crate) struct ResolvedSnapshotHttp1DownstreamResponse {
    pub(crate) status: u16,
    pub(crate) headers: SnapshotHttp1DownstreamHeaders,
    pub(crate) version: Version,
    pub(crate) upstream_latency_ms: u64,
    pub(super) body: SharedUpstreamResponseBody,
}

impl ResolvedSnapshotHttp1DownstreamResponse {
    pub(crate) async fn take_body_passthrough(&self) -> Option<DownstreamHttpBodyPassthrough> {
        let mut body = self.body.lock().await;
        if body.is_known_empty() {
            None
        } else {
            Some(DownstreamHttpBodyPassthrough {
                inner: body.take_streaming_passthrough(),
            })
        }
    }

    pub(crate) fn into_head(self) -> (u16, SnapshotHttp1DownstreamHeaders, Version, u64) {
        (
            self.status,
            self.headers,
            self.version,
            self.upstream_latency_ms,
        )
    }
}

#[derive(Debug)]
pub(crate) struct ResolvedHttpGraphResponse {
    pub response: Response<Body>,
    pub upstream_latency_ms: u64,
    pub post_response_plan: Option<DownstreamPostResponsePlan>,
}

pub(crate) enum Http1DownstreamResolution {
    NativeLocal(ResolvedNativeLocalHttp1DownstreamResponse),
    Native(Result<ResolvedNativeHttp1DownstreamResponse, Response<Body>>),
    Snapshot(Result<ResolvedSnapshotHttp1DownstreamResponse, Response<Body>>),
    Graph(ResolvedHttpGraphResponse),
}

#[derive(Clone, Debug)]
struct DownstreamHttp1ResolutionState {
    response_headers: HeaderMap,
    response_status: Option<u16>,
    has_post_response_plan: bool,
    has_response_body: bool,
    body_source_exchange: Option<i64>,
    has_upstream_target: bool,
    default_upstream_websocket_mode: bool,
    native_forward_active: bool,
    default_upstream_response_body_read: bool,
    body_source_exchange_response_read: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeDefaultUpstreamRequestBodyMode {
    Empty,
    BufferedImmediate,
    StreamInbound,
    BufferRemaining,
}

#[derive(Clone, Debug)]
enum NativeDefaultUpstreamRequestBodyTemplate {
    Empty,
    Bytes(Bytes),
    Streaming { content_length: Option<u64> },
}

type StreamingInboundBodyStream =
    Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send + 'static>>;

struct StreamingInboundHttpBody {
    stream: ParkingMutex<StreamingInboundBodyStream>,
    content_length: Option<u64>,
}

impl StreamingInboundHttpBody {
    fn new(stream: StreamingInboundBodyStream, content_length: Option<u64>) -> Self {
        Self {
            stream: ParkingMutex::new(stream),
            content_length,
        }
    }
}

impl std::fmt::Debug for StreamingInboundHttpBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamingInboundHttpBody")
            .field("content_length", &self.content_length)
            .finish()
    }
}

impl hyper::body::Body for StreamingInboundHttpBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut stream = self.stream.lock();
        match Stream::poll_next(stream.as_mut(), cx) {
            Poll::Ready(Some(Ok(chunk))) => Poll::Ready(Some(Ok(Frame::data(chunk)))),
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(err))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        false
    }

    fn size_hint(&self) -> SizeHint {
        let mut hint = SizeHint::new();
        if let Some(content_length) = self.content_length {
            hint.set_exact(content_length);
        }
        hint
    }
}

const OUTBOUND_HTTP1_REQUEST_BODY_STREAM_CHUNK_BYTES: usize = 16 * 1024;

pub async fn resolve_outbound_request_body(
    context: &SharedProxyVmContext,
) -> Result<Bytes, VmError> {
    let (body_override, request_body_known_empty) = {
        let exchanges = context.lock_exchanges();
        let exchange = exchanges
            .exchanges
            .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
            .expect("default upstream exchange should exist");
        let request_body_known_empty = request_body_known_empty_for_exchange(context, exchange);
        (
            exchange.request.body_override.clone(),
            request_body_known_empty,
        )
    };

    if let Some(body) = body_override {
        return Ok(Bytes::from(body));
    }

    if request_body_known_empty {
        return Ok(Bytes::new());
    }

    let mut inbound = context.inbound_request_body.lock().await;
    inbound.read_all().await.map(Bytes::from)
}

async fn native_default_upstream_request_body_mode(
    context: &SharedProxyVmContext,
) -> NativeDefaultUpstreamRequestBodyMode {
    let (body_override, request_body_known_empty) = {
        let exchanges = context.lock_exchanges();
        let exchange = exchanges
            .exchanges
            .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
            .expect("default upstream exchange should exist");
        (
            exchange.request.body_override.clone(),
            request_body_known_empty_for_exchange(context, exchange),
        )
    };

    if let Some(body_override) = body_override {
        if body_override.is_empty() {
            return NativeDefaultUpstreamRequestBodyMode::Empty;
        }
        return NativeDefaultUpstreamRequestBodyMode::BufferedImmediate;
    }

    if request_body_known_empty {
        return NativeDefaultUpstreamRequestBodyMode::Empty;
    }

    let inbound = context.inbound_request_body.lock().await;
    if inbound.is_pristine_unread() {
        NativeDefaultUpstreamRequestBodyMode::StreamInbound
    } else {
        NativeDefaultUpstreamRequestBodyMode::BufferRemaining
    }
}

fn stream_remaining_inbound_request_body(
    context: SharedProxyVmContext,
) -> impl futures_util::Stream<Item = Result<Bytes, VmError>> + Send + 'static {
    try_unfold(context, |context| async move {
        let (chunk, drained) = {
            let mut inbound = context.inbound_request_body.lock().await;
            let chunk = inbound
                .read_next_chunk(OUTBOUND_HTTP1_REQUEST_BODY_STREAM_CHUNK_BYTES)
                .await;
            let drained = chunk.as_ref().is_ok_and(|_| inbound.is_drained());
            (chunk, drained)
        };

        match chunk {
            Ok(chunk) => {
                if chunk.is_empty() {
                    mark_downstream_transport_closed(&context);
                    Ok(None)
                } else {
                    if drained {
                        mark_downstream_transport_closed(&context);
                    }
                    Ok(Some((Bytes::from(chunk), context)))
                }
            }
            Err(err) => {
                mark_downstream_transport_failed(&context, &err.to_string());
                Err(err)
            }
        }
    })
}

async fn default_upstream_outbound_http1_request_body_template(
    context: &SharedProxyVmContext,
) -> Result<NativeDefaultUpstreamRequestBodyTemplate, UpstreamResponseStartError> {
    let (body_override, request_body_known_empty, content_length) = {
        let exchanges = context.lock_exchanges();
        let exchange = exchanges
            .exchanges
            .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
            .expect("default upstream exchange should exist");
        (
            exchange.request.body_override.clone(),
            request_body_known_empty_for_exchange(context, exchange),
            context.with_request_head(|request_head| request_head.lazy_headers().content_length()),
        )
    };

    if let Some(body_override) = body_override {
        return Ok(if body_override.is_empty() {
            NativeDefaultUpstreamRequestBodyTemplate::Empty
        } else {
            NativeDefaultUpstreamRequestBodyTemplate::Bytes(Bytes::from(body_override))
        });
    }

    if request_body_known_empty {
        return Ok(NativeDefaultUpstreamRequestBodyTemplate::Empty);
    }

    match native_default_upstream_request_body_mode(context).await {
        NativeDefaultUpstreamRequestBodyMode::Empty => {
            Ok(NativeDefaultUpstreamRequestBodyTemplate::Empty)
        }
        NativeDefaultUpstreamRequestBodyMode::BufferedImmediate
        | NativeDefaultUpstreamRequestBodyMode::BufferRemaining => {
            let request_body = resolve_outbound_request_body(context)
                .await
                .map_err(|err| {
                    UpstreamResponseStartError::ResolveOutboundBody(format!(
                        "failed to resolve outbound exchange body: {err}",
                    ))
                })?;
            Ok(if request_body.is_empty() {
                NativeDefaultUpstreamRequestBodyTemplate::Empty
            } else {
                NativeDefaultUpstreamRequestBodyTemplate::Bytes(request_body)
            })
        }
        NativeDefaultUpstreamRequestBodyMode::StreamInbound => {
            Ok(NativeDefaultUpstreamRequestBodyTemplate::Streaming { content_length })
        }
    }
}

fn serialize_default_upstream_http1_request_into(
    context: &SharedProxyVmContext,
    request: &DefaultUpstreamRequestSnapshot,
    request_head: &HttpRequestHead,
    authority: &Arc<str>,
    body_template: &NativeDefaultUpstreamRequestBodyTemplate,
    encoded: &mut BytesMut,
) -> Result<SerializedOutboundHttp1Request, VmError> {
    let method = request.method_or_request_head(request_head).clone();
    let headers =
        request.outbound_http1_headers_or_request_head(request_head, Some(authority.clone()));
    let body = match body_template {
        NativeDefaultUpstreamRequestBodyTemplate::Empty => OutboundHttp1RequestBody::Empty,
        NativeDefaultUpstreamRequestBodyTemplate::Bytes(body) => {
            OutboundHttp1RequestBody::Bytes(body.clone())
        }
        NativeDefaultUpstreamRequestBodyTemplate::Streaming { content_length } => {
            OutboundHttp1RequestBody::Streaming {
                content_length: *content_length,
                stream: Box::pin(stream_remaining_inbound_request_body(context.clone())),
            }
        }
    };
    let use_chunked_body = serialize_request_head_parts_into(
        &method,
        request.path_or_request_head(request_head),
        request.query_or_request_head(request_head),
        &headers,
        authority.as_ref(),
        body.content_length(),
        encoded,
    );
    Ok(SerializedOutboundHttp1Request {
        method,
        body,
        use_chunked_body,
    })
}

fn stream_remaining_inbound_request_body_io(
    context: SharedProxyVmContext,
) -> impl futures_util::Stream<Item = Result<Bytes, io::Error>> + Send + 'static {
    stream_remaining_inbound_request_body(context)
        .map(|result| result.map_err(|err| io::Error::other(err.to_string())))
}

fn streaming_inbound_http_body(
    context: &SharedProxyVmContext,
    content_length: Option<u64>,
) -> StreamingInboundHttpBody {
    StreamingInboundHttpBody::new(
        Box::pin(stream_remaining_inbound_request_body_io(context.clone())),
        content_length,
    )
}

fn into_http_body_from_default_upstream_template(
    context: &SharedProxyVmContext,
    request_body: NativeDefaultUpstreamRequestBodyTemplate,
) -> (Body, Option<u64>, bool) {
    match request_body {
        NativeDefaultUpstreamRequestBodyTemplate::Empty => (Body::empty(), Some(0), false),
        NativeDefaultUpstreamRequestBodyTemplate::Bytes(body) => {
            let content_length = u64::try_from(body.len()).unwrap_or(u64::MAX);
            (Body::from(body), Some(content_length), content_length > 0)
        }
        NativeDefaultUpstreamRequestBodyTemplate::Streaming { content_length } => {
            let body_present = !matches!(content_length, Some(0));
            (
                Body::new(streaming_inbound_http_body(context, content_length)),
                content_length,
                body_present,
            )
        }
    }
}

#[cfg(feature = "http3")]
fn into_http3_request_body_from_default_upstream_template(
    context: &SharedProxyVmContext,
    request_body: NativeDefaultUpstreamRequestBodyTemplate,
) -> http3::Http3RequestBody {
    match request_body {
        NativeDefaultUpstreamRequestBodyTemplate::Empty => http3::Http3RequestBody::Empty,
        NativeDefaultUpstreamRequestBodyTemplate::Bytes(body) => {
            http3::Http3RequestBody::Bytes(body)
        }
        NativeDefaultUpstreamRequestBodyTemplate::Streaming { .. } => {
            http3::Http3RequestBody::Streaming(Box::pin(stream_remaining_inbound_request_body_io(
                context.clone(),
            )))
        }
    }
}

fn request_body_known_empty_for_exchange(
    context: &SharedProxyVmContext,
    exchange: &HttpOutboundExchangeState,
) -> bool {
    if let Some(body_override) = exchange.request.body_override.as_ref() {
        return body_override.is_empty();
    }

    context.with_request_head(|request_head| {
        request_headers_indicate_empty_body_lazy(request_head.lazy_headers())
    })
}

pub(crate) fn build_configured_upstream_url(
    upstream: &str,
    inherits_request_path: bool,
    host_header: Option<&str>,
    request_path: &str,
    request_query: &str,
) -> (String, Option<String>) {
    if !inherits_request_path {
        return (upstream.to_string(), host_header.map(str::to_string));
    }

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
    (
        format!("{upstream}{path_and_query}"),
        host_header.map(str::to_string),
    )
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

fn filtered_upstream_headers(headers: &HeaderMap, host_header: Option<&str>) -> HeaderMap {
    let mut filtered = HeaderMap::new();
    for (name, value) in headers {
        if name != HOST && name != CONTENT_LENGTH && !is_hop_by_hop_header(name) {
            filtered.insert(name.clone(), value.clone());
        }
    }
    if let Some(host) = host_header
        && let Ok(value) = HeaderValue::from_str(host)
    {
        filtered.insert(HOST, value);
    }
    filtered
}

fn snapshot_default_upstream_request(
    context: &SharedProxyVmContext,
) -> Result<
    (
        DefaultUpstreamRequestSnapshot,
        Option<AttachedHttpTransport>,
    ),
    UpstreamResponseStartError,
> {
    let exchanges = context.lock_exchanges();
    let exchange = exchanges
        .exchanges
        .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
        .expect("default upstream exchange should exist");
    if exchange.websocket_dag.is_websocket_mode() {
        return Err(UpstreamResponseStartError::Protocol(
            "default upstream exchange is already owned by the websocket DAG".to_string(),
        ));
    }
    Ok((
        DefaultUpstreamRequestSnapshot::from_request(&exchange.request),
        exchange.transport.attached_transport,
    ))
}

fn prepared_upstream_request(
    context: &SharedProxyVmContext,
) -> Result<PreparedUpstreamRequest, UpstreamResponseStartError> {
    let (request, attached_transport) = snapshot_default_upstream_request(context)?;
    if request.target.is_none() {
        return Err(UpstreamResponseStartError::MissingTarget);
    }
    let target = request
        .target
        .as_ref()
        .cloned()
        .ok_or(UpstreamResponseStartError::MissingTarget)?;
    let tls_flow = match attached_transport {
        #[cfg(feature = "tls")]
        Some(AttachedHttpTransport::Tls(session)) => context
            .lock_transport()
            .dynamic_tls_sessions
            .get(&session)
            .cloned()
            .unwrap_or_else(TlsFlowState::for_dynamic_socket),
        _ => context.lock_transport().tls_dag.default_upstream.clone(),
    };
    let (method, path, query, headers) = context.with_request_head(|request_head| {
        (
            request.method_or_request_head(request_head).clone(),
            request.path_or_request_head(request_head).to_string(),
            request.query_or_request_head(request_head).to_string(),
            request.cloned_headers_or_request_head(request_head),
        )
    });
    Ok(PreparedUpstreamRequest {
        http2_sessions: context.services().upstream_http_sessions(),
        http3_sessions: context.services().upstream_http3_sessions(),
        version_preference: request.version_preference,
        http2_mode: http2::select_upstream_mode(
            request.target_scheme,
            &tls_flow,
            request.version_preference,
        ),
        http3_mode: http3::select_upstream_mode(
            request.target_scheme,
            &tls_flow,
            request.version_preference,
        ),
        tls_flow,
        attached_transport,
        method,
        path,
        query,
        headers,
        target,
        target_host: request.target_host_arc.clone(),
        target_port: request.target_port,
        target_host_header: request.target_authority.clone(),
        target_authority: request.target_authority.clone(),
        target_plain_http1_pool_key: request.target_plain_http1_pool_key.clone(),
        target_inherits_request_path: true,
        target_scheme: request.target_scheme,
    })
}

pub(super) async fn start_upstream_response(
    context: &SharedProxyVmContext,
) -> Result<HttpUpstreamResponseSnapshot, UpstreamResponseStartError> {
    start_outbound_exchange_response(context, DEFAULT_UPSTREAM_EXCHANGE_HANDLE).await
}

pub(crate) async fn ensure_upstream_response_started(
    context: &SharedProxyVmContext,
) -> Result<HttpUpstreamResponseSnapshot, VmError> {
    start_upstream_response(context)
        .await
        .map_err(|err| err.as_vm_error())
}

fn prepared_outbound_exchange_request(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<PreparedUpstreamRequest, UpstreamResponseStartError> {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return prepared_upstream_request(context);
    }

    let (request, attached_transport, tls_flow) = {
        let exchanges = context.lock_exchanges();
        let exchange = exchanges
            .exchanges
            .get(&handle)
            .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
        if exchange.websocket_dag.is_websocket_mode() {
            return Err(UpstreamResponseStartError::Protocol(format!(
                "outbound exchange handle {handle} is already owned by the websocket DAG",
            )));
        }
        let tls_flow = match exchange.transport.attached_transport {
            #[cfg(feature = "tls")]
            Some(AttachedHttpTransport::Tls(session)) => context
                .lock_transport()
                .dynamic_tls_sessions
                .get(&session)
                .cloned()
                .unwrap_or_else(TlsFlowState::for_dynamic_socket),
            _ => exchange.transport.tls_flow.clone(),
        };
        (
            DefaultUpstreamRequestSnapshot::from_request(&exchange.request),
            exchange.transport.attached_transport,
            tls_flow,
        )
    };
    if request.target.is_none() {
        return Err(UpstreamResponseStartError::MissingTarget);
    }
    let target = request
        .target
        .as_ref()
        .cloned()
        .ok_or(UpstreamResponseStartError::MissingTarget)?;
    let (method, path, query, headers) = context.with_request_head(|request_head| {
        (
            request.method_or_request_head(request_head).clone(),
            request.path_or_request_head(request_head).to_string(),
            request.query_or_request_head(request_head).to_string(),
            request.cloned_headers_or_request_head(request_head),
        )
    });
    Ok(PreparedUpstreamRequest {
        http2_sessions: context.services().upstream_http_sessions(),
        http3_sessions: context.services().upstream_http3_sessions(),
        version_preference: request.version_preference,
        http2_mode: http2::select_upstream_mode(
            request.target_scheme,
            &tls_flow,
            request.version_preference,
        ),
        http3_mode: http3::select_upstream_mode(
            request.target_scheme,
            &tls_flow,
            request.version_preference,
        ),
        tls_flow,
        attached_transport,
        method,
        path,
        query,
        headers,
        target,
        target_host: request.target_host_arc.clone(),
        target_port: request.target_port,
        target_host_header: request.target_authority.clone(),
        target_authority: request.target_authority.clone(),
        target_plain_http1_pool_key: request.target_plain_http1_pool_key.clone(),
        target_inherits_request_path: true,
        target_scheme: request.target_scheme,
    })
}

async fn resolve_outbound_exchange_body(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<Bytes, VmError> {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return resolve_outbound_request_body(context).await;
    }

    let guard = context.lock_exchanges();
    let exchange = guard
        .exchanges
        .get(&handle)
        .ok_or_else(|| VmError::HostError(format!("unknown outbound exchange handle {handle}")))?;
    Ok(exchange
        .request
        .body_override
        .clone()
        .map(Bytes::from)
        .unwrap_or_default())
}

async fn take_dynamic_tcp_stream_for_http(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<tokio::net::TcpStream, UpstreamResponseStartError> {
    let io = {
        let mut guard = context.lock_transport();
        let Some(state) = guard.tcp_streams.get_mut(&handle) else {
            return Err(UpstreamResponseStartError::Protocol(format!(
                "dynamic tcp stream handle {handle} is unavailable for http attachment",
            )));
        };
        state.mark_http_attached();
        guard.tcp_stream_ios.remove(&handle).ok_or_else(|| {
            UpstreamResponseStartError::Protocol(format!(
                "dynamic tcp stream handle {handle} has no active transport",
            ))
        })?
    };

    let mut guard = io.lock().await;
    guard.take().ok_or_else(|| {
        UpstreamResponseStartError::Protocol(format!(
            "dynamic tcp stream handle {handle} is already in use",
        ))
    })
}

#[cfg(feature = "tls")]
async fn take_dynamic_tls_stream_for_http(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>, UpstreamResponseStartError> {
    let io = {
        let mut guard = context.lock_transport();
        let Some(state) = guard.tcp_streams.get_mut(&handle) else {
            return Err(UpstreamResponseStartError::Protocol(format!(
                "dynamic tls session handle {handle} is unavailable for http attachment",
            )));
        };
        state.mark_http_attached();
        guard
            .dynamic_tls_session_ios
            .remove(&handle)
            .ok_or_else(|| {
                UpstreamResponseStartError::Protocol(format!(
                    "dynamic tls session handle {handle} has no active plaintext transport",
                ))
            })?
    };

    let mut guard = io.lock().await;
    guard.take().ok_or_else(|| {
        UpstreamResponseStartError::Protocol(format!(
            "dynamic tls session handle {handle} is already in use",
        ))
    })
}

fn with_outbound_tls_flow_mut<T>(
    context: &SharedProxyVmContext,
    handle: i64,
    mutate: impl FnOnce(&mut TlsFlowState) -> T,
) -> Result<T, UpstreamResponseStartError> {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return Ok(mutate(
            &mut context.lock_transport().tls_dag.default_upstream,
        ));
    }

    let mut exchanges = context.lock_exchanges();
    let exchange = exchanges
        .exchanges
        .get_mut(&handle)
        .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
    Ok(mutate(&mut exchange.transport.tls_flow))
}

pub(crate) fn header_content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

fn request_headers_indicate_empty_body_lazy(headers: &LazyHttpHeaders) -> bool {
    matches!(headers.content_length(), Some(0))
        || (!headers.contains_name(CONTENT_LENGTH.as_str())
            && !headers.contains_name(TRANSFER_ENCODING.as_str()))
}

async fn start_upstream_response_via_plain_http1_sender_pool(
    context: &SharedProxyVmContext,
    handle: i64,
    prepared: &PreparedUpstreamRequest,
    request_body: Bytes,
) -> Result<StartedUpstreamResponse, UpstreamResponseStartError> {
    let services = context.services();
    let pool = services
        .plain_http1_sender_pool()
        .ok_or(UpstreamResponseStartError::MissingClient)?;
    let sender_pool_capacity = services.upstream_http_reuse_entries();
    let host = prepared.target_host.clone().ok_or_else(|| {
        UpstreamResponseStartError::Protocol(
            "outbound exchange host should be configured for plain http/1.1 forwarding".to_string(),
        )
    })?;
    let port = prepared.target_port.ok_or_else(|| {
        UpstreamResponseStartError::Protocol(
            "outbound exchange port should be configured for plain http/1.1 forwarding".to_string(),
        )
    })?;
    let authority = prepared
        .target_authority
        .clone()
        .unwrap_or_else(|| Arc::from(format_upstream_authority(host.as_ref(), port)));
    let target = OutboundHttp1Target {
        scheme: match prepared.target_scheme {
            HttpUpstreamScheme::Http => OutboundHttp1Scheme::Http,
            #[cfg(feature = "tls")]
            HttpUpstreamScheme::Https => OutboundHttp1Scheme::Https,
            #[cfg(not(feature = "tls"))]
            HttpUpstreamScheme::Https => {
                return Err(UpstreamResponseStartError::Protocol(
                    "https http/1.1 forwarding requires the tls feature".to_string(),
                ));
            }
        },
        authority: authority.clone(),
        host,
        port,
        plain_pool_key: prepared.target_plain_http1_pool_key.clone(),
        #[cfg(feature = "tls")]
        tls_flow: (prepared.target_scheme == HttpUpstreamScheme::Https)
            .then_some(prepared.tls_flow.clone()),
    };
    let request_body = (!request_body.is_empty()).then_some(request_body);
    let started = Instant::now();
    let response = forward_serialized_via_sender_pool(
        &pool,
        sender_pool_capacity,
        &target,
        started,
        |encoded, authority| {
            let headers: OutboundHttp1RequestHeaders =
                filtered_upstream_headers(&prepared.headers, Some(authority)).into();
            let body = request_body
                .as_ref()
                .map_or(OutboundHttp1RequestBody::Empty, |body| {
                    OutboundHttp1RequestBody::Bytes(body.clone())
                });
            let use_chunked_body = serialize_request_head_parts_into(
                &prepared.method,
                &prepared.path,
                &prepared.query,
                &headers,
                authority,
                body.content_length(),
                encoded,
            );
            Ok(SerializedOutboundHttp1Request {
                method: prepared.method.clone(),
                body,
                use_chunked_body,
            })
        },
    )
    .await
    .map_err(|err| UpstreamResponseStartError::UpstreamRequest(err.to_string()))?;
    Ok(started_upstream_response_from_plain_http1_forward(
        handle, response,
    ))
}

async fn start_default_upstream_response_via_plain_http1_sender_pool(
    context: &SharedProxyVmContext,
    request: &DefaultUpstreamRequestSnapshot,
) -> Result<StartedUpstreamResponse, UpstreamResponseStartError> {
    let services = context.services();
    let pool = services
        .plain_http1_sender_pool()
        .ok_or(UpstreamResponseStartError::MissingClient)?;
    let sender_pool_capacity = services.upstream_http_reuse_entries();
    let host_arc = request.target_host_arc.clone().ok_or_else(|| {
        UpstreamResponseStartError::Protocol(
            "default upstream host should be configured for plain http/1.1 forwarding".to_string(),
        )
    })?;
    let port = request.target_port.ok_or_else(|| {
        UpstreamResponseStartError::Protocol(
            "default upstream port should be configured for plain http/1.1 forwarding".to_string(),
        )
    })?;
    let authority = request
        .target_authority
        .clone()
        .unwrap_or_else(|| Arc::from(format_upstream_authority(host_arc.as_ref(), port)));
    let tls_flow = context.lock_transport().tls_dag.default_upstream.clone();
    let target = OutboundHttp1Target {
        scheme: match request.target_scheme {
            HttpUpstreamScheme::Http => OutboundHttp1Scheme::Http,
            #[cfg(feature = "tls")]
            HttpUpstreamScheme::Https => OutboundHttp1Scheme::Https,
            #[cfg(not(feature = "tls"))]
            HttpUpstreamScheme::Https => {
                return Err(UpstreamResponseStartError::Protocol(
                    "https http/1.1 forwarding requires the tls feature".to_string(),
                ));
            }
        },
        authority: authority.clone(),
        host: host_arc,
        port,
        plain_pool_key: request.target_plain_http1_pool_key.clone(),
        #[cfg(feature = "tls")]
        tls_flow: (request.target_scheme == HttpUpstreamScheme::Https).then_some(tls_flow),
    };
    let request_body = default_upstream_outbound_http1_request_body_template(context).await?;
    let started = Instant::now();
    let response = forward_serialized_via_sender_pool(
        &pool,
        sender_pool_capacity,
        &target,
        started,
        |encoded, _authority| {
            context.with_request_head(|request_head| {
                serialize_default_upstream_http1_request_into(
                    context,
                    request,
                    request_head,
                    &authority,
                    &request_body,
                    encoded,
                )
            })
        },
    )
    .await
    .map_err(|err| UpstreamResponseStartError::UpstreamRequest(err.to_string()))?;
    Ok(started_upstream_response_from_plain_http1_forward(
        DEFAULT_UPSTREAM_EXCHANGE_HANDLE,
        response,
    ))
}

fn started_upstream_response_from_plain_http1_forward(
    handle: i64,
    upstream_response: OutboundHttp1ForwardResponse,
) -> StartedUpstreamResponse {
    StartedUpstreamResponse {
        status: upstream_response.status,
        headers: upstream_response.headers,
        version: upstream_response.version,
        carrier_ref: if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
            HttpCarrierRef::Http1DefaultUpstream
        } else {
            HttpCarrierRef::Http1DynamicExchange(handle)
        },
        peer_addr: None,
        negotiated_alpn: upstream_response.negotiated_alpn,
        peer_certificate_der: upstream_response.peer_certificate_der,
        body: Arc::new(tokio::sync::Mutex::new(match upstream_response.body {
            OutboundHttp1ForwardBody::Empty => UpstreamResponseBodyState::empty(),
            OutboundHttp1ForwardBody::Raw {
                body,
                content_length,
            } => UpstreamResponseBodyState::from_plain_http1(body, content_length),
        })),
    }
}

fn started_upstream_response_into_snapshot(
    started: StartedUpstreamResponse,
) -> (HttpUpstreamResponseSnapshot, Version, Option<String>) {
    let StartedUpstreamResponse {
        status,
        headers,
        version,
        carrier_ref,
        peer_addr,
        negotiated_alpn: _,
        peer_certificate_der: _,
        body,
    } = started;
    let snapshot = HttpUpstreamResponseSnapshot {
        status,
        headers: Arc::new(headers),
        http_version: http_version_label(version),
        carrier_kind: carrier_ref.kind(),
        carrier_ref: carrier_ref.clone(),
        body,
    };
    (snapshot, version, peer_addr)
}

pub(super) async fn response_from_started_upstream_response(
    native_response: NativeDefaultUpstreamForwardResponse,
    response_headers: HeaderMap,
    response_status: Option<u16>,
) -> Response<Body> {
    let mut response = Response::new(match native_response.body {
        NativeDefaultUpstreamForwardBody::Empty => Body::empty(),
        NativeDefaultUpstreamForwardBody::Raw {
            body,
            content_length,
        } => {
            let passthrough = UpstreamResponseBodyState::from_plain_http1(body, content_length)
                .take_streaming_passthrough();
            Body::new(StreamBody::new(try_unfold(
                passthrough,
                |mut state| async move {
                    let frame: Option<Frame<Bytes>> = state
                        .next_frame()
                        .await
                        .map_err(|err: VmError| io::Error::other(err.to_string()))?;
                    Ok::<_, io::Error>(frame.map(|frame| (frame, state)))
                },
            )))
        }
    });
    *response.status_mut() = StatusCode::from_u16(native_response.status).unwrap_or(StatusCode::OK);
    *response.version_mut() = native_response.version;
    *response.headers_mut() = native_response.headers;
    let hop_by_hop_headers = response
        .headers()
        .keys()
        .filter(|name| is_hop_by_hop_header(name))
        .cloned()
        .collect::<Vec<_>>();
    for header in hop_by_hop_headers {
        response.headers_mut().remove(header);
    }
    if let Some(status) = response_status.and_then(|code| StatusCode::from_u16(code).ok()) {
        *response.status_mut() = status;
    }
    merge_headers(response.headers_mut(), &response_headers);
    response
}

async fn start_default_upstream_plain_http1_fast_path(
    context: &SharedProxyVmContext,
) -> Result<Option<HttpUpstreamResponseSnapshot>, UpstreamResponseStartError> {
    let (request, attached_transport) = snapshot_default_upstream_request(context)?;
    if attached_transport.is_some() {
        return Ok(None);
    }

    if request.target.is_none() {
        return Err(UpstreamResponseStartError::MissingTarget);
    }
    let tls_flow = context.lock_transport().tls_dag.default_upstream.clone();
    let services = context.services();
    let http2_sessions = services.upstream_http_sessions();
    let http3_sessions = services.upstream_http3_sessions();
    let http2_mode =
        http2::select_upstream_mode(request.target_scheme, &tls_flow, request.version_preference);
    let http3_mode =
        http3::select_upstream_mode(request.target_scheme, &tls_flow, request.version_preference);
    if tls_flow.requires_custom_client() {
        return Ok(None);
    }
    let use_http2 =
        http2::should_use_explicit_upstream_transport(http2_mode, http2_sessions.as_ref());
    let use_http3 =
        http3::should_use_explicit_upstream_transport(http3_mode, http3_sessions.as_ref());
    if !outbound_http1_fast_path_eligible(
        request.version_preference,
        request.target.is_some(),
        false,
        services.plain_http1_sender_pool().is_some(),
        use_http2,
        use_http3,
    ) {
        return Ok(None);
    }

    let response = forward_native_default_upstream_http_via_sender_pool(context, &request).await?;
    materialize_native_default_upstream_forward_response(context, response)
}

fn materialize_native_default_upstream_forward_response(
    context: &SharedProxyVmContext,
    response: NativeDefaultUpstreamForwardResponse,
) -> Result<Option<HttpUpstreamResponseSnapshot>, UpstreamResponseStartError> {
    let NativeDefaultUpstreamForwardResponse {
        status,
        headers,
        version,
        body,
        upstream_latency_ms,
        negotiated_alpn,
        peer_certificate_der,
    } = response;
    let started = StartedUpstreamResponse {
        status,
        headers,
        version,
        carrier_ref: HttpCarrierRef::Http1DefaultUpstream,
        peer_addr: None,
        negotiated_alpn,
        peer_certificate_der,
        body: Arc::new(tokio::sync::Mutex::new(match body {
            NativeDefaultUpstreamForwardBody::Empty => UpstreamResponseBodyState::empty(),
            NativeDefaultUpstreamForwardBody::Raw {
                body,
                content_length,
            } => UpstreamResponseBodyState::from_plain_http1(body, content_length),
        })),
    };
    let (snapshot, upstream_response_version, peer_addr) =
        started_upstream_response_into_snapshot(started);
    let mut exchanges = context.lock_exchanges();
    let exchange = exchanges
        .exchanges
        .get_mut(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
        .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(
            DEFAULT_UPSTREAM_EXCHANGE_HANDLE,
        ))?;
    if let Ok(existing) = exchange.response_snapshot() {
        return Ok(Some(existing));
    }
    exchange.store_response(StoredUpstreamResponse::new(
        snapshot.clone(),
        upstream_latency_ms,
    ));
    exchange
        .transport
        .mark_response_ready(upstream_response_version, snapshot.carrier_ref.clone());
    exchange.transport.set_peer_addr(peer_addr);
    context.clear_native_default_upstream_http_forward();
    Ok(Some(snapshot))
}

async fn take_or_start_native_default_upstream_forward_response(
    context: &SharedProxyVmContext,
) -> Result<Option<NativeDefaultUpstreamForwardResponse>, UpstreamResponseStartError> {
    if let Some(response) = context.take_native_default_upstream_forward_response() {
        return Ok(Some(response));
    }

    let Some(request) = context.take_native_default_upstream_forward_request() else {
        return Ok(None);
    };
    match forward_native_default_upstream_http_via_sender_pool(context, &request).await {
        Ok(response) => Ok(Some(response)),
        Err(err) => {
            context.clear_native_default_upstream_http_forward();
            Err(err)
        }
    }
}

async fn try_materialize_ready_or_pending_native_default_upstream_forward_response(
    context: &SharedProxyVmContext,
) -> Result<Option<HttpUpstreamResponseSnapshot>, UpstreamResponseStartError> {
    let Some(response) = take_or_start_native_default_upstream_forward_response(context).await?
    else {
        return Ok(None);
    };
    materialize_native_default_upstream_forward_response(context, response)
}

pub(super) async fn try_resolve_ready_or_pending_native_default_upstream_forward_response(
    context: &SharedProxyVmContext,
    response_headers: HeaderMap,
    response_status: Option<u16>,
) -> Result<Option<ResolvedHttpGraphResponse>, UpstreamResponseStartError> {
    let Some(response) = take_or_start_native_default_upstream_forward_response(context).await?
    else {
        return Ok(None);
    };
    let upstream_latency_ms = response.upstream_latency_ms;
    Ok(Some(ResolvedHttpGraphResponse {
        response: response_from_started_upstream_response(
            response,
            response_headers,
            response_status,
        )
        .await,
        upstream_latency_ms,
        post_response_plan: None,
    }))
}

fn capture_downstream_http1_resolution_state(
    context: &SharedProxyVmContext,
) -> DownstreamHttp1ResolutionState {
    let downstream = context.lock_downstream();
    let exchanges = context.lock_exchanges();
    let default_exchange = exchanges
        .exchanges
        .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
        .expect("default upstream exchange should exist");
    let body_source_exchange = downstream.response_output.body_source_exchange;
    DownstreamHttp1ResolutionState {
        response_headers: downstream.response_output.headers.clone(),
        response_status: downstream.response_output.status,
        has_post_response_plan: downstream.post_response_plan.is_some(),
        has_response_body: downstream.response_output.has_local_body(),
        body_source_exchange,
        has_upstream_target: default_exchange.request.target.is_some(),
        default_upstream_websocket_mode: default_exchange.websocket_dag.is_websocket_mode(),
        native_forward_active: downstream.native_default_upstream_http_forward,
        default_upstream_response_body_read: downstream
            .vm_touches
            .exchange_response_body_reads
            .contains(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE),
        body_source_exchange_response_read: body_source_exchange.is_some_and(|exchange| {
            downstream
                .vm_touches
                .exchange_response_body_reads
                .contains(&exchange)
        }),
    }
}

pub(crate) async fn try_resolve_native_http1_downstream_response(
    context: &SharedProxyVmContext,
) -> Option<Result<ResolvedNativeHttp1DownstreamResponse, Response<Body>>> {
    let state = capture_downstream_http1_resolution_state(context);
    if state.has_post_response_plan
        || state.has_response_body
        || !state.native_forward_active
        || state.default_upstream_response_body_read
    {
        return None;
    }

    match take_or_start_native_default_upstream_forward_response(context).await {
        Ok(Some(response)) => {
            let upstream_latency_ms = response.upstream_latency_ms;
            context.clear_native_default_upstream_http_forward();
            Some(Ok(ResolvedNativeHttp1DownstreamResponse {
                response,
                response_headers: state.response_headers,
                response_status: state.response_status,
                upstream_latency_ms,
            }))
        }
        Ok(None) | Err(UpstreamResponseStartError::MissingTarget) => None,
        Err(UpstreamResponseStartError::UpstreamRequest(_)) => {
            context.clear_native_default_upstream_http_forward();
            Some(Err(text_response(StatusCode::BAD_GATEWAY, "bad gateway")))
        }
        Err(
            err @ (UpstreamResponseStartError::UnknownExchangeHandle(_)
            | UpstreamResponseStartError::MissingClient
            | UpstreamResponseStartError::Protocol(_)
            | UpstreamResponseStartError::ResolveOutboundBody(_)),
        ) => {
            context.clear_native_default_upstream_http_forward();
            Some(Err(text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &err.as_vm_error().to_string(),
            )))
        }
    }
}

pub(crate) async fn try_resolve_snapshot_http1_downstream_response(
    context: &SharedProxyVmContext,
) -> Option<Result<ResolvedSnapshotHttp1DownstreamResponse, Response<Body>>> {
    let state = capture_downstream_http1_resolution_state(context);
    if state.has_post_response_plan || state.has_response_body {
        return None;
    }

    if state.body_source_exchange_response_read {
        return None;
    }

    let (snapshot, upstream_latency_ms) = if let Some(exchange) = state.body_source_exchange {
        let snapshot = if exchange == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
            match start_upstream_response(context).await {
                Ok(snapshot) => snapshot,
                Err(UpstreamResponseStartError::MissingTarget) => return None,
                Err(
                    err @ (UpstreamResponseStartError::UnknownExchangeHandle(_)
                    | UpstreamResponseStartError::MissingClient
                    | UpstreamResponseStartError::Protocol(_)
                    | UpstreamResponseStartError::ResolveOutboundBody(_)),
                ) => {
                    return Some(Err(text_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &err.as_vm_error().to_string(),
                    )));
                }
                Err(UpstreamResponseStartError::UpstreamRequest(_)) => {
                    return Some(Err(text_response(StatusCode::BAD_GATEWAY, "bad gateway")));
                }
            }
        } else {
            match start_outbound_exchange_response(context, exchange).await {
                Ok(snapshot) => snapshot,
                Err(UpstreamResponseStartError::MissingTarget) => return None,
                Err(
                    err @ (UpstreamResponseStartError::UnknownExchangeHandle(_)
                    | UpstreamResponseStartError::MissingClient
                    | UpstreamResponseStartError::Protocol(_)
                    | UpstreamResponseStartError::ResolveOutboundBody(_)),
                ) => {
                    return Some(Err(text_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &err.as_vm_error().to_string(),
                    )));
                }
                Err(UpstreamResponseStartError::UpstreamRequest(_)) => {
                    return Some(Err(text_response(StatusCode::BAD_GATEWAY, "bad gateway")));
                }
            }
        };
        (snapshot, outbound_exchange_latency_ms(context, exchange))
    } else {
        let snapshot = {
            let exchanges = context.lock_exchanges();
            exchanges
                .exchanges
                .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
                .and_then(|exchange| match &exchange.response {
                    HttpUpstreamResponseNode::Ready(snapshot) => Some(snapshot.clone()),
                    HttpUpstreamResponseNode::NotStarted => None,
                })
        };
        let snapshot = if let Some(snapshot) = snapshot {
            snapshot
        } else if state.has_upstream_target && !state.default_upstream_websocket_mode {
            match start_upstream_response(context).await {
                Ok(snapshot) => snapshot,
                Err(UpstreamResponseStartError::MissingTarget) => return None,
                Err(
                    err @ (UpstreamResponseStartError::UnknownExchangeHandle(_)
                    | UpstreamResponseStartError::MissingClient
                    | UpstreamResponseStartError::Protocol(_)
                    | UpstreamResponseStartError::ResolveOutboundBody(_)),
                ) => {
                    return Some(Err(text_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &err.as_vm_error().to_string(),
                    )));
                }
                Err(UpstreamResponseStartError::UpstreamRequest(_)) => {
                    return Some(Err(text_response(StatusCode::BAD_GATEWAY, "bad gateway")));
                }
            }
        } else {
            return None;
        };
        (snapshot, current_upstream_latency_ms(context))
    };

    let (status, headers) = if state.body_source_exchange.is_some() {
        explicit_snapshot_downstream_response_head(
            &snapshot,
            state.response_headers,
            state.response_status,
        )
    } else {
        downstream_snapshot_response_head(&snapshot, state.response_headers, state.response_status)
    };
    Some(Ok(ResolvedSnapshotHttp1DownstreamResponse {
        status,
        headers,
        version: Version::HTTP_11,
        upstream_latency_ms,
        body: snapshot.body.clone(),
    }))
}

pub(crate) async fn resolve_http1_downstream_response(
    context: &SharedProxyVmContext,
) -> Http1DownstreamResolution {
    if let Some(native_local) = try_take_native_local_http1_downstream_response(context) {
        return Http1DownstreamResolution::NativeLocal(native_local);
    }
    if let Some(native_result) = try_resolve_native_http1_downstream_response(context).await {
        return Http1DownstreamResolution::Native(native_result);
    }
    if let Some(snapshot_result) = try_resolve_snapshot_http1_downstream_response(context).await {
        return Http1DownstreamResolution::Snapshot(snapshot_result);
    }
    Http1DownstreamResolution::Graph(resolve_http_graph_response(context).await)
}

pub(crate) fn try_take_native_local_http1_downstream_response(
    context: &SharedProxyVmContext,
) -> Option<ResolvedNativeLocalHttp1DownstreamResponse> {
    let mut downstream = context.lock_downstream();
    if downstream.post_response_plan.is_some() {
        return None;
    }
    if downstream.response_output.stream_committed() {
        return None;
    }
    if downstream.response_output.body_source_exchange.is_some() {
        return None;
    }
    let body = downstream.response_output.body.take()?;
    downstream.native_default_upstream_http_forward = false;
    downstream.native_default_upstream_forward_request = None;
    downstream.native_default_upstream_forward_response = None;
    Some(ResolvedNativeLocalHttp1DownstreamResponse {
        status: downstream
            .response_output
            .status
            .take()
            .unwrap_or(StatusCode::OK.as_u16()),
        headers: std::mem::take(&mut downstream.response_output.headers),
        body,
        default_content_type: true,
    })
}

async fn forward_native_default_upstream_http_via_sender_pool(
    context: &SharedProxyVmContext,
    request: &DefaultUpstreamRequestSnapshot,
) -> Result<NativeDefaultUpstreamForwardResponse, UpstreamResponseStartError> {
    let services = context.services();
    let pool = services
        .plain_http1_sender_pool()
        .ok_or(UpstreamResponseStartError::MissingClient)?;
    let sender_pool_capacity = services.upstream_http_reuse_entries();
    let request_body = default_upstream_outbound_http1_request_body_template(context).await?;
    let started_at = Instant::now();
    let host_arc = request.target_host_arc.clone().ok_or_else(|| {
        UpstreamResponseStartError::Protocol(
            "default upstream host should be configured".to_string(),
        )
    })?;
    let port = request.target_port.ok_or_else(|| {
        UpstreamResponseStartError::Protocol(
            "default upstream port should be configured".to_string(),
        )
    })?;
    let authority = request
        .target_authority
        .clone()
        .unwrap_or_else(|| Arc::from(format_upstream_authority(host_arc.as_ref(), port)));
    let tls_flow = context.lock_transport().tls_dag.default_upstream.clone();
    let target = OutboundHttp1Target {
        scheme: match request.target_scheme {
            HttpUpstreamScheme::Http => OutboundHttp1Scheme::Http,
            #[cfg(feature = "tls")]
            HttpUpstreamScheme::Https => OutboundHttp1Scheme::Https,
            #[cfg(not(feature = "tls"))]
            HttpUpstreamScheme::Https => {
                return Err(UpstreamResponseStartError::Protocol(
                    "https http/1.1 forwarding requires the tls feature".to_string(),
                ));
            }
        },
        authority: authority.clone(),
        host: host_arc,
        port,
        plain_pool_key: request.target_plain_http1_pool_key.clone(),
        #[cfg(feature = "tls")]
        tls_flow: (request.target_scheme == HttpUpstreamScheme::Https).then_some(tls_flow),
    };
    let response = forward_serialized_via_sender_pool(
        &pool,
        sender_pool_capacity,
        &target,
        started_at,
        |encoded, _authority| {
            context.with_request_head(|request_head| {
                serialize_default_upstream_http1_request_into(
                    context,
                    request,
                    request_head,
                    &authority,
                    &request_body,
                    encoded,
                )
            })
        },
    )
    .await
    .map_err(|err| UpstreamResponseStartError::UpstreamRequest(err.to_string()))?;
    mark_outbound_tcp_connected(context, DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
        .map_err(|err| UpstreamResponseStartError::Protocol(err.as_vm_error().to_string()))?;
    if request.target_scheme == HttpUpstreamScheme::Https {
        finalize_outbound_tls_handshake(
            context,
            DEFAULT_UPSTREAM_EXCHANGE_HANDLE,
            response.negotiated_alpn.clone(),
            response.peer_certificate_der.clone(),
        )?;
        cache_outbound_tls_session(
            context,
            DEFAULT_UPSTREAM_EXCHANGE_HANDLE,
            response.negotiated_alpn.clone(),
            response.peer_certificate_der.clone(),
        )?;
    }
    Ok(response)
}

pub(crate) async fn start_native_default_upstream_http_forward_response(
    context: &SharedProxyVmContext,
) -> Result<bool, VmError> {
    if context.native_default_upstream_forward_response_ready()
        || context.native_default_upstream_forward_request_pending()
        || context.native_default_upstream_http_forward_active()
    {
        return Ok(true);
    }

    let Ok((request, attached_transport)) = snapshot_default_upstream_request(context) else {
        return Ok(false);
    };
    if attached_transport.is_some() {
        return Ok(false);
    }

    let services = context.services();
    let tls_flow = context.lock_transport().tls_dag.default_upstream.clone();
    let http2_mode =
        http2::select_upstream_mode(request.target_scheme, &tls_flow, request.version_preference);
    let http3_mode =
        http3::select_upstream_mode(request.target_scheme, &tls_flow, request.version_preference);
    let use_http2 = http2::should_use_explicit_upstream_transport(
        http2_mode,
        services.upstream_http_sessions().as_ref(),
    );
    let use_http3 = http3::should_use_explicit_upstream_transport(
        http3_mode,
        services.upstream_http3_sessions().as_ref(),
    );
    if !outbound_http1_fast_path_eligible(
        request.version_preference,
        request.target.is_some(),
        false,
        services.plain_http1_sender_pool().is_some(),
        use_http2,
        use_http3,
    ) {
        return Ok(false);
    }

    match native_default_upstream_request_body_mode(context).await {
        NativeDefaultUpstreamRequestBodyMode::Empty
        | NativeDefaultUpstreamRequestBodyMode::BufferedImmediate => {
            context.store_native_default_upstream_forward_request(request);
            Ok(true)
        }
        NativeDefaultUpstreamRequestBodyMode::StreamInbound => {
            context.store_native_default_upstream_forward_request(request);
            Ok(true)
        }
        NativeDefaultUpstreamRequestBodyMode::BufferRemaining => Ok(false),
    }
}

pub(super) async fn try_resolve_native_default_upstream_http_forward_response(
    context: &SharedProxyVmContext,
    response_headers: HeaderMap,
    response_status: Option<u16>,
) -> Result<Option<ResolvedHttpGraphResponse>, UpstreamResponseStartError> {
    let (request, attached_transport) = snapshot_default_upstream_request(context)?;
    if attached_transport.is_some() {
        return Ok(None);
    }

    if request.target.is_none() {
        return Err(UpstreamResponseStartError::MissingTarget);
    }
    let tls_flow = context.lock_transport().tls_dag.default_upstream.clone();
    let services = context.services();
    let http2_sessions = services.upstream_http_sessions();
    let http3_sessions = services.upstream_http3_sessions();
    let http2_mode =
        http2::select_upstream_mode(request.target_scheme, &tls_flow, request.version_preference);
    let http3_mode =
        http3::select_upstream_mode(request.target_scheme, &tls_flow, request.version_preference);
    let use_http2 =
        http2::should_use_explicit_upstream_transport(http2_mode, http2_sessions.as_ref());
    let use_http3 =
        http3::should_use_explicit_upstream_transport(http3_mode, http3_sessions.as_ref());
    if use_http2 || use_http3 {
        return Ok(None);
    }

    let started = Instant::now();
    if outbound_http1_fast_path_eligible(
        request.version_preference,
        request.target.is_some(),
        false,
        services.plain_http1_sender_pool().is_some(),
        use_http2,
        use_http3,
    ) && let Ok(response) =
        forward_native_default_upstream_http_via_sender_pool(context, &request).await
    {
        let upstream_latency_ms = response.upstream_latency_ms;
        return Ok(Some(ResolvedHttpGraphResponse {
            response: response_from_started_upstream_response(
                response,
                response_headers,
                response_status,
            )
            .await,
            upstream_latency_ms,
            post_response_plan: None,
        }));
    }
    let upstream_response =
        start_default_upstream_response_via_plain_http1_sender_pool(context, &request).await?;
    let (snapshot, _, _) = started_upstream_response_into_snapshot(upstream_response);
    let response =
        match response_from_upstream_snapshot(snapshot, response_headers, response_status).await {
            Ok(response) => response,
            Err(_) => text_response(StatusCode::BAD_GATEWAY, "bad gateway"),
        };
    mark_outbound_tcp_connected(context, DEFAULT_UPSTREAM_EXCHANGE_HANDLE)?;
    let upstream_latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(Some(ResolvedHttpGraphResponse {
        response,
        upstream_latency_ms,
        post_response_plan: None,
    }))
}

async fn start_upstream_response_via_attached_http1<I>(
    handle: i64,
    prepared: &PreparedUpstreamRequest,
    request_path: &str,
    headers: HeaderMap,
    request_body: Body,
    content_length: Option<u64>,
    io: I,
) -> Result<StartedUpstreamResponse, UpstreamResponseStartError>
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(io))
            .await
            .map_err(|err| {
                UpstreamResponseStartError::UpstreamRequest(format!(
                    "failed to establish attached http/1.1 client connection: {err}",
                ))
            })?;
    tokio::spawn(async move {
        let _ = connection.await;
    });

    let mut request = hyper::Request::builder()
        .method(prepared.method.clone())
        .uri(request_path)
        .version(Version::HTTP_11)
        .body(request_body)
        .map_err(|err| {
            UpstreamResponseStartError::Protocol(format!(
                "failed to build attached http request: {err}",
            ))
        })?;
    for (name, value) in &headers {
        request.headers_mut().insert(name, value.clone());
    }
    if let Some(content_length) = content_length {
        request.headers_mut().insert(
            CONTENT_LENGTH,
            HeaderValue::from_str(&content_length.to_string()).map_err(|err| {
                UpstreamResponseStartError::Protocol(format!(
                    "failed to encode attached http content-length: {err}",
                ))
            })?,
        );
    }

    let response = sender.send_request(request).await.map_err(|err| {
        UpstreamResponseStartError::UpstreamRequest(format!(
            "attached http request failed while evaluating host call: {err}",
        ))
    })?;
    let version = response.version();
    let content_length = header_content_length(response.headers());
    Ok(StartedUpstreamResponse {
        status: response.status().as_u16(),
        headers: response.headers().clone(),
        version,
        carrier_ref: if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
            HttpCarrierRef::Http1DefaultUpstream
        } else {
            HttpCarrierRef::Http1DynamicExchange(handle)
        },
        peer_addr: None,
        negotiated_alpn: Some(HTTP11_ALPN_PROTOCOL.to_string()),
        peer_certificate_der: None,
        body: Arc::new(tokio::sync::Mutex::new(
            UpstreamResponseBodyState::from_hyper(response.into_body(), None, None, content_length),
        )),
    })
}

async fn start_upstream_response_via_attached_transport(
    context: &SharedProxyVmContext,
    handle: i64,
    prepared: &PreparedUpstreamRequest,
    headers: HeaderMap,
    request_body: Body,
    content_length: Option<u64>,
) -> Result<StartedUpstreamResponse, UpstreamResponseStartError> {
    if matches!(prepared.version_preference, HttpVersionPreference::Http3) {
        return Err(UpstreamResponseStartError::Protocol(
            "http3 cannot use an attached tcp or tls plaintext transport".to_string(),
        ));
    }

    let request_path = request_path_with_query(&prepared.path, &prepared.query);
    match prepared.attached_transport {
        Some(AttachedHttpTransport::Tcp(stream)) => {
            let stream_handle = stream;
            if matches!(prepared.target_scheme, HttpUpstreamScheme::Https) {
                return Err(UpstreamResponseStartError::Protocol(
                    "attached tcp transports cannot be used with https targets; attach a tls plaintext transport instead"
                        .to_string(),
                ));
            }
            let stream = take_dynamic_tcp_stream_for_http(context, stream_handle).await?;
            let mut started = start_upstream_response_via_attached_http1(
                handle,
                prepared,
                &request_path,
                headers,
                request_body,
                content_length,
                stream,
            )
            .await?;
            started.peer_addr = context
                .lock_transport()
                .tcp_streams
                .get(&stream_handle)
                .map(|state| state.peer_address().to_string())
                .filter(|peer_addr| !peer_addr.is_empty());
            Ok(started)
        }
        #[cfg(feature = "tls")]
        Some(AttachedHttpTransport::Tls(session)) => {
            let stream = take_dynamic_tls_stream_for_http(context, session).await?;
            let mut started = start_upstream_response_via_attached_http1(
                handle,
                prepared,
                &request_path,
                headers,
                request_body,
                content_length,
                stream,
            )
            .await?;
            started.negotiated_alpn = {
                let guard = context.lock_transport();
                guard
                    .dynamic_tls_sessions
                    .get(&session)
                    .and_then(|flow| (!flow.alpn().is_empty()).then(|| flow.alpn().to_string()))
            };
            started.peer_certificate_der = {
                let guard = context.lock_transport();
                guard
                    .dynamic_tls_sessions
                    .get(&session)
                    .and_then(|flow| flow.peer_certificate_der().map(|bytes| bytes.to_vec()))
            };
            started.peer_addr = context
                .lock_transport()
                .tcp_streams
                .get(&session)
                .map(|state| state.peer_address().to_string())
                .filter(|peer_addr| !peer_addr.is_empty());
            Ok(started)
        }
        None => Err(UpstreamResponseStartError::Protocol(
            "attached transport is unavailable".to_string(),
        )),
    }
}

#[cfg(feature = "http2")]
async fn start_upstream_response_via_http2(
    handle: i64,
    prepared: &PreparedUpstreamRequest,
    headers: HeaderMap,
    request_body: Body,
    request_body_present: bool,
) -> Result<StartedUpstreamResponse, http2::Http2RequestError> {
    let sessions = prepared
        .http2_sessions
        .as_ref()
        .expect("explicit http2 transport requires shared sessions");
    let started = http2::send_request(http2::Http2SendRequest {
        sessions,
        exchange_handle: handle,
        target_scheme: prepared.target_scheme,
        target_host: prepared
            .target_host
            .as_deref()
            .expect("http2 upstream target host should exist"),
        target_port: prepared
            .target_port
            .expect("http2 upstream target port should exist"),
        target_host_header: prepared.target_host_header.as_deref(),
        request_path: &prepared.path,
        request_query: &prepared.query,
        mode: prepared.http2_mode,
        tls_flow: &prepared.tls_flow,
        method: prepared.method.clone(),
        headers,
        request_body,
        request_body_present,
    })
    .await?;
    let version = started.response.version();
    let content_length = header_content_length(started.response.headers());
    Ok(StartedUpstreamResponse {
        status: started.response.status().as_u16(),
        headers: started.response.headers().clone(),
        version,
        carrier_ref: HttpCarrierRef::UpstreamHttp2Stream(started.stream_ref),
        peer_addr: started.peer_addr,
        negotiated_alpn: started.negotiated_alpn,
        peer_certificate_der: started.peer_certificate_der,
        body: Arc::new(tokio::sync::Mutex::new(
            UpstreamResponseBodyState::from_hyper(
                started.response.into_body(),
                Some(started.body_tracker),
                None,
                content_length,
            ),
        )),
    })
}

#[cfg(feature = "http3")]
async fn start_upstream_response_via_http3(
    handle: i64,
    prepared: &PreparedUpstreamRequest,
    upstream_url: &str,
    headers: HeaderMap,
    request_body: http3::Http3RequestBody,
) -> Result<StartedUpstreamResponse, http3::Http3RequestError> {
    let sessions = prepared
        .http3_sessions
        .clone()
        .expect("explicit http3 transport requires shared sessions");
    let started = http3::send_request(http3::Http3SendRequestOptions {
        exchange_handle: handle,
        target_scheme: prepared.target_scheme,
        target_host: prepared
            .target_host
            .as_deref()
            .expect("http3 upstream target host should exist")
            .to_string(),
        target_port: prepared
            .target_port
            .expect("http3 upstream target port should exist"),
        target_host_header: prepared.target_host_header.as_deref().map(str::to_string),
        upstream_url: upstream_url.to_string(),
        method: prepared.method.clone(),
        headers,
        request_body,
        tls_flow: prepared.tls_flow.clone(),
        mode: prepared.http3_mode,
        sessions,
    })
    .await?;
    let version = started.response.version();
    let content_length = header_content_length(started.response.headers());
    Ok(StartedUpstreamResponse {
        status: started.response.status().as_u16(),
        headers: started.response.headers().clone(),
        version,
        carrier_ref: HttpCarrierRef::UpstreamHttp3Stream(started.stream_ref),
        peer_addr: started.peer_addr,
        negotiated_alpn: started.negotiated_alpn,
        peer_certificate_der: started.peer_certificate_der,
        body: Arc::new(tokio::sync::Mutex::new(
            UpstreamResponseBodyState::from_http3(
                started.request_stream,
                Some(started.body_tracker),
                content_length,
            ),
        )),
    })
}

fn note_outbound_tls_prepared(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<(), UpstreamResponseStartError> {
    with_outbound_tls_flow_mut(context, handle, |flow| {
        flow.note_handshake_prepared();
        flow.note_client_hello_sent();
    })?;
    Ok(())
}

fn outbound_tls_handshake_complete(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<bool, UpstreamResponseStartError> {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return Ok(context
            .lock_transport()
            .tls_dag
            .default_upstream
            .handshake_complete());
    }

    let guard = context.lock_exchanges();
    let exchange = guard
        .exchanges
        .get(&handle)
        .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
    Ok(exchange.transport.tls_flow.handshake_complete())
}

fn cache_outbound_tls_session(
    context: &SharedProxyVmContext,
    handle: i64,
    negotiated_alpn: Option<String>,
    peer_certificate_der: Option<Vec<u8>>,
) -> Result<(), UpstreamResponseStartError> {
    let (cache, key, cached) = {
        let Some(cache) = context.services().tls_session_cache() else {
            return Ok(());
        };
        let (scheme, target_host, target_port, flow) = if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE
        {
            let (scheme, target_host, target_port) = context
                .lock_exchanges()
                .exchanges
                .get(&handle)
                .map(|exchange| {
                    (
                        exchange.request.target_scheme,
                        exchange.request.target_host.clone(),
                        exchange.request.target_port,
                    )
                })
                .unwrap_or((HttpUpstreamScheme::Http, None, None));
            let flow = context.lock_transport().tls_dag.default_upstream.clone();
            (scheme, target_host, target_port, flow)
        } else {
            let exchanges = context.lock_exchanges();
            let exchange = exchanges
                .exchanges
                .get(&handle)
                .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
            (
                exchange.request.target_scheme,
                exchange.request.target_host.clone(),
                exchange.request.target_port,
                exchange.transport.tls_flow.clone(),
            )
        };
        let Some(target_host) = target_host else {
            return Ok(());
        };
        let Some(target_port) = target_port else {
            return Ok(());
        };
        let Some(key) = tls_session_cache_key(scheme.as_str(), &target_host, target_port, &flow)
        else {
            return Ok(());
        };
        let cached = CachedTlsSession {
            negotiated_alpn,
            peer_name: (!flow.peer_name().is_empty()).then(|| flow.peer_name().to_string()),
            server_name: (!flow.server_name().is_empty()).then(|| flow.server_name().to_string()),
            peer_certificate_der,
        };
        (cache, key, cached)
    };

    let _ = cache.insert(
        key,
        cached,
        LockMetricKey::TlsSessionCache,
        "tls session cache lock poisoned",
    );
    Ok(())
}

fn note_outbound_tls_failure(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<(), UpstreamResponseStartError> {
    with_outbound_tls_flow_mut(context, handle, TlsFlowState::mark_failed)?;
    Ok(())
}

fn finalize_outbound_tls_handshake(
    context: &SharedProxyVmContext,
    handle: i64,
    negotiated_alpn: Option<String>,
    peer_certificate_der: Option<Vec<u8>>,
) -> Result<(), UpstreamResponseStartError> {
    with_outbound_tls_flow_mut(context, handle, |flow| {
        flow.note_server_hello_received();
        flow.note_server_certificate_received(peer_certificate_der);
        if flow.verify_peer() && flow.verify_hostname() {
            flow.note_server_certificate_verified();
        } else {
            flow.note_verification_skipped();
        }
        if !flow.accepts_negotiated_alpn(negotiated_alpn.as_deref()) {
            flow.mark_failed();
            return Err(UpstreamResponseStartError::UpstreamRequest(format!(
                "tls ALPN mismatch: requested [{}], negotiated {}",
                flow.desired_alpn().join(", "),
                negotiated_alpn.as_deref().unwrap_or("none"),
            )));
        }
        flow.mark_handshake_complete(negotiated_alpn);
        Ok(())
    })?
}

fn mark_outbound_tcp_connected(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<(), UpstreamResponseStartError> {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        context
            .lock_transport()
            .tcp_dag
            .default_upstream
            .mark_connected();
        return Ok(());
    }

    let mut exchanges = context.lock_exchanges();
    let exchange = exchanges
        .exchanges
        .get_mut(&handle)
        .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
    exchange.transport.tcp_flow.mark_connected();
    Ok(())
}

pub(super) async fn start_outbound_exchange_response(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<HttpUpstreamResponseSnapshot, UpstreamResponseStartError> {
    {
        let guard = context.lock_exchanges();
        let exchange = guard
            .exchanges
            .get(&handle)
            .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
        if let Ok(snapshot) = exchange.response_snapshot() {
            return Ok(snapshot);
        }
    }

    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE
        && let Ok(Some(snapshot)) =
            try_materialize_ready_or_pending_native_default_upstream_forward_response(context).await
    {
        return Ok(snapshot);
    }

    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE
        && let Some(snapshot) = start_default_upstream_plain_http1_fast_path(context).await?
    {
        return Ok(snapshot);
    }

    let prepared = prepared_outbound_exchange_request(context, handle)?;
    let (upstream_url, host_header) = build_configured_upstream_url(
        &prepared.target,
        prepared.target_inherits_request_path,
        prepared.target_host_header.as_deref(),
        &prepared.path,
        &prepared.query,
    );
    let outbound_headers = filtered_upstream_headers(&prepared.headers, host_header.as_deref());

    let is_attached_transport = prepared.attached_transport.is_some();
    if is_attached_transport {
        let (request_body, content_length, _) = if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
            into_http_body_from_default_upstream_template(
                context,
                default_upstream_outbound_http1_request_body_template(context).await?,
            )
        } else {
            let request_body = resolve_outbound_exchange_body(context, handle)
                .await
                .map_err(|err| {
                    UpstreamResponseStartError::ResolveOutboundBody(format!(
                        "failed to resolve outbound exchange body: {err}",
                    ))
                })?;
            if request_body.is_empty() {
                (Body::empty(), Some(0), false)
            } else {
                let content_length = u64::try_from(request_body.len()).unwrap_or(u64::MAX);
                (Body::from(request_body), Some(content_length), true)
            }
        };
        let started = Instant::now();
        let upstream_response = start_upstream_response_via_attached_transport(
            context,
            handle,
            &prepared,
            outbound_headers,
            request_body,
            content_length,
        )
        .await?;
        let StartedUpstreamResponse {
            status,
            headers,
            version: upstream_response_version,
            carrier_ref,
            peer_addr,
            negotiated_alpn: _,
            peer_certificate_der: _,
            body,
        } = upstream_response;
        mark_outbound_tcp_connected(context, handle)?;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let snapshot = HttpUpstreamResponseSnapshot {
            status,
            headers: Arc::new(headers),
            http_version: http_version_label(upstream_response_version),
            carrier_kind: carrier_ref.kind(),
            carrier_ref: carrier_ref.clone(),
            body,
        };

        let mut guard = context.lock_exchanges();
        let exchange = guard
            .exchanges
            .get_mut(&handle)
            .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
        exchange.transport.attached_transport = None;
        exchange.store_response(StoredUpstreamResponse::new(snapshot.clone(), latency_ms));
        exchange
            .transport
            .mark_response_ready(upstream_response_version, snapshot.carrier_ref.clone());
        exchange.transport.set_peer_addr(peer_addr);
        return Ok(snapshot);
    }

    let handshake_already_complete = outbound_tls_handshake_complete(context, handle)?;
    if !handshake_already_complete {
        note_outbound_tls_prepared(context, handle)?;
    }
    let started = Instant::now();
    let use_http3 = http3::should_use_explicit_upstream_transport(
        prepared.http3_mode,
        prepared.http3_sessions.as_ref(),
    );
    let use_http2 = http2::should_use_explicit_upstream_transport(
        prepared.http2_mode,
        prepared.http2_sessions.as_ref(),
    );
    let default_request_body_template =
        if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE && (use_http3 || use_http2) {
            Some(default_upstream_outbound_http1_request_body_template(context).await?)
        } else {
            None
        };
    let request_body = if default_request_body_template.is_none() {
        Some(
            resolve_outbound_exchange_body(context, handle)
                .await
                .map_err(|err| {
                    UpstreamResponseStartError::ResolveOutboundBody(format!(
                        "failed to resolve outbound exchange body: {err}",
                    ))
                })?,
        )
    } else {
        None
    };
    let upstream_response = if use_http3 {
        #[cfg(feature = "http3")]
        {
            match start_upstream_response_via_http3(
                handle,
                &prepared,
                &upstream_url,
                outbound_headers.clone(),
                if let Some(template) = default_request_body_template.clone() {
                    into_http3_request_body_from_default_upstream_template(context, template)
                } else {
                    http3::Http3RequestBody::Bytes(
                        request_body
                            .clone()
                            .expect("non-default outbound body should be resolved"),
                    )
                },
            )
            .await
            {
                Ok(started) => started,
                Err(http3::Http3RequestError::FallbackToHttp2 { .. }) => {
                    if use_http2 {
                        #[cfg(feature = "http2")]
                        {
                            let (http2_request_body, _, request_body_present) =
                                if let Some(template) = default_request_body_template.clone() {
                                    into_http_body_from_default_upstream_template(context, template)
                                } else {
                                    let request_body = request_body
                                        .clone()
                                        .expect("non-default outbound body should be resolved");
                                    if request_body.is_empty() {
                                        (Body::empty(), Some(0), false)
                                    } else {
                                        let content_length =
                                            u64::try_from(request_body.len()).unwrap_or(u64::MAX);
                                        (Body::from(request_body), Some(content_length), true)
                                    }
                                };
                            match start_upstream_response_via_http2(
                                handle,
                                &prepared,
                                outbound_headers.clone(),
                                http2_request_body,
                                request_body_present,
                            )
                            .await
                            {
                                Ok(started) => started,
                                Err(http2::Http2RequestError::FallbackToHttp1 { .. }) => {
                                    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
                                        let (request, _) =
                                            snapshot_default_upstream_request(context)?;
                                        match start_default_upstream_response_via_plain_http1_sender_pool(
                                            context,
                                            &request,
                                        )
                                        .await
                                        {
                                            Ok(started) => started,
                                            Err(err) => {
                                                let _ = note_outbound_tls_failure(context, handle);
                                                return Err(err);
                                            }
                                        }
                                    } else {
                                        match start_upstream_response_via_plain_http1_sender_pool(
                                            context,
                                            handle,
                                            &prepared,
                                            request_body.expect(
                                                "non-default outbound body should be resolved",
                                            ),
                                        )
                                        .await
                                        {
                                            Ok(started) => started,
                                            Err(err) => {
                                                let _ = note_outbound_tls_failure(context, handle);
                                                return Err(err);
                                            }
                                        }
                                    }
                                }
                                Err(err) => {
                                    let _ = note_outbound_tls_failure(context, handle);
                                    return Err(UpstreamResponseStartError::UpstreamRequest(
                                        format!(
                                            "outbound exchange {handle} failed while evaluating host call: {}",
                                            err.into_message(),
                                        ),
                                    ));
                                }
                            }
                        }
                        #[cfg(not(feature = "http2"))]
                        {
                            unreachable!("explicit http2 transport requires the http2 feature");
                        }
                    } else if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
                        let (request, _) = snapshot_default_upstream_request(context)?;
                        match start_default_upstream_response_via_plain_http1_sender_pool(
                            context, &request,
                        )
                        .await
                        {
                            Ok(started) => started,
                            Err(err) => {
                                let _ = note_outbound_tls_failure(context, handle);
                                return Err(err);
                            }
                        }
                    } else {
                        match start_upstream_response_via_plain_http1_sender_pool(
                            context,
                            handle,
                            &prepared,
                            request_body.expect("non-default outbound body should be resolved"),
                        )
                        .await
                        {
                            Ok(started) => started,
                            Err(err) => {
                                let _ = note_outbound_tls_failure(context, handle);
                                return Err(err);
                            }
                        }
                    }
                }
                Err(err) => {
                    let _ = note_outbound_tls_failure(context, handle);
                    return Err(UpstreamResponseStartError::UpstreamRequest(format!(
                        "outbound exchange {handle} failed while evaluating host call: {}",
                        err.into_message(),
                    )));
                }
            }
        }
        #[cfg(not(feature = "http3"))]
        {
            unreachable!("explicit http3 transport requires the http3 feature");
        }
    } else if use_http2 {
        #[cfg(feature = "http2")]
        {
            let (http2_request_body, _, request_body_present) =
                if let Some(template) = default_request_body_template.clone() {
                    into_http_body_from_default_upstream_template(context, template)
                } else {
                    let request_body = request_body
                        .clone()
                        .expect("non-default outbound body should be resolved");
                    if request_body.is_empty() {
                        (Body::empty(), Some(0), false)
                    } else {
                        let content_length = u64::try_from(request_body.len()).unwrap_or(u64::MAX);
                        (Body::from(request_body), Some(content_length), true)
                    }
                };
            match start_upstream_response_via_http2(
                handle,
                &prepared,
                outbound_headers.clone(),
                http2_request_body,
                request_body_present,
            )
            .await
            {
                Ok(started) => started,
                Err(http2::Http2RequestError::FallbackToHttp1 { .. }) => {
                    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
                        let (request, _) = snapshot_default_upstream_request(context)?;
                        match start_default_upstream_response_via_plain_http1_sender_pool(
                            context, &request,
                        )
                        .await
                        {
                            Ok(started) => started,
                            Err(err) => {
                                let _ = note_outbound_tls_failure(context, handle);
                                return Err(err);
                            }
                        }
                    } else {
                        match start_upstream_response_via_plain_http1_sender_pool(
                            context,
                            handle,
                            &prepared,
                            request_body.expect("non-default outbound body should be resolved"),
                        )
                        .await
                        {
                            Ok(started) => started,
                            Err(err) => {
                                let _ = note_outbound_tls_failure(context, handle);
                                return Err(err);
                            }
                        }
                    }
                }
                Err(err) => {
                    let _ = note_outbound_tls_failure(context, handle);
                    return Err(UpstreamResponseStartError::UpstreamRequest(format!(
                        "outbound exchange {handle} failed while evaluating host call: {}",
                        err.into_message(),
                    )));
                }
            }
        }
        #[cfg(not(feature = "http2"))]
        {
            unreachable!("explicit http2 transport requires the http2 feature");
        }
    } else if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        let (request, _) = snapshot_default_upstream_request(context)?;
        match start_default_upstream_response_via_plain_http1_sender_pool(context, &request).await {
            Ok(started) => started,
            Err(err) => {
                let _ = note_outbound_tls_failure(context, handle);
                return Err(err);
            }
        }
    } else {
        match start_upstream_response_via_plain_http1_sender_pool(
            context,
            handle,
            &prepared,
            request_body.expect("outbound body should be resolved for http/1.1 sender pool"),
        )
        .await
        {
            Ok(started) => started,
            Err(err) => {
                let _ = note_outbound_tls_failure(context, handle);
                return Err(err);
            }
        }
    };
    let upstream_response_version = upstream_response.version;
    let negotiated_alpn = upstream_response.negotiated_alpn.clone();
    if !handshake_already_complete {
        finalize_outbound_tls_handshake(
            context,
            handle,
            negotiated_alpn.clone(),
            upstream_response.peer_certificate_der.clone(),
        )?;
        cache_outbound_tls_session(
            context,
            handle,
            negotiated_alpn.clone(),
            upstream_response.peer_certificate_der.clone(),
        )?;
    }
    if !http3::supports_response_version(upstream_response_version) {
        mark_outbound_tcp_connected(context, handle)?;
    }
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let StartedUpstreamResponse {
        status,
        headers,
        version: _,
        carrier_ref,
        peer_addr,
        negotiated_alpn: _,
        peer_certificate_der: _,
        body,
    } = upstream_response;
    let snapshot = HttpUpstreamResponseSnapshot {
        status,
        headers: Arc::new(headers),
        http_version: http_version_label(upstream_response_version),
        carrier_kind: carrier_ref.kind(),
        carrier_ref: carrier_ref.clone(),
        body,
    };

    let mut guard = context.lock_exchanges();
    let exchange = guard
        .exchanges
        .get_mut(&handle)
        .ok_or(UpstreamResponseStartError::UnknownExchangeHandle(handle))?;
    if let Ok(existing) = exchange.response_snapshot() {
        return Ok(existing);
    }
    exchange.store_response(StoredUpstreamResponse::new(snapshot.clone(), latency_ms));
    exchange
        .transport
        .mark_response_ready(upstream_response_version, snapshot.carrier_ref.clone());
    exchange.transport.set_peer_addr(peer_addr);
    Ok(snapshot)
}

pub(crate) async fn ensure_outbound_exchange_response_started(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<HttpUpstreamResponseSnapshot, VmError> {
    start_outbound_exchange_response(context, handle)
        .await
        .map_err(|err| err.as_vm_error())
}

pub(crate) fn upstream_response_available(context: &SharedProxyVmContext) -> bool {
    if context.native_default_upstream_forward_response_ready() {
        return true;
    }
    context
        .lock_exchanges()
        .exchanges
        .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
        .map(HttpOutboundExchangeState::response_ready)
        .unwrap_or(false)
}

#[allow(dead_code)]
pub(crate) fn outbound_exchange_response_available(
    context: &SharedProxyVmContext,
    handle: i64,
) -> bool {
    let guard = context.lock_exchanges();
    guard
        .exchanges
        .get(&handle)
        .map(HttpOutboundExchangeState::response_ready)
        .unwrap_or(false)
}

pub(crate) async fn read_upstream_response_all(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    context.note_exchange_response_body_read(DEFAULT_UPSTREAM_EXCHANGE_HANDLE);
    let snapshot = ensure_upstream_response_started(context).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.read_all().await
}

pub(crate) async fn read_upstream_response_trailers(
    context: &SharedProxyVmContext,
) -> Result<HeaderMap, VmError> {
    context.note_exchange_response_body_read(DEFAULT_UPSTREAM_EXCHANGE_HANDLE);
    let snapshot = ensure_upstream_response_started(context).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.read_trailers().await
}

pub(crate) async fn read_outbound_exchange_response_all(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<Vec<u8>, VmError> {
    context.note_exchange_response_body_read(handle);
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return read_upstream_response_all(context).await;
    }
    let snapshot = ensure_outbound_exchange_response_started(context, handle).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.read_all().await
}

pub(crate) async fn read_outbound_exchange_response_trailers(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<HeaderMap, VmError> {
    context.note_exchange_response_body_read(handle);
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return read_upstream_response_trailers(context).await;
    }
    let snapshot = ensure_outbound_exchange_response_started(context, handle).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.read_trailers().await
}

pub(crate) async fn read_upstream_response_next_chunk(
    context: &SharedProxyVmContext,
    max_bytes: usize,
) -> Result<Vec<u8>, VmError> {
    context.note_exchange_response_body_read(DEFAULT_UPSTREAM_EXCHANGE_HANDLE);
    let snapshot = ensure_upstream_response_started(context).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.read_next_chunk(max_bytes).await
}

pub(crate) async fn read_outbound_exchange_response_next_chunk(
    context: &SharedProxyVmContext,
    handle: i64,
    max_bytes: usize,
) -> Result<Vec<u8>, VmError> {
    context.note_exchange_response_body_read(handle);
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return read_upstream_response_next_chunk(context, max_bytes).await;
    }
    let snapshot = ensure_outbound_exchange_response_started(context, handle).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.read_next_chunk(max_bytes).await
}

pub(crate) async fn read_upstream_response_next_line(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    context.note_exchange_response_body_read(DEFAULT_UPSTREAM_EXCHANGE_HANDLE);
    let snapshot = ensure_upstream_response_started(context).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.read_next_line().await
}

#[allow(dead_code)]
pub(crate) async fn read_outbound_exchange_response_next_line(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<Vec<u8>, VmError> {
    context.note_exchange_response_body_read(handle);
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return read_upstream_response_next_line(context).await;
    }
    let snapshot = ensure_outbound_exchange_response_started(context, handle).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.read_next_line().await
}

pub(crate) async fn upstream_response_eof(context: &SharedProxyVmContext) -> Result<bool, VmError> {
    let snapshot = ensure_upstream_response_started(context).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.eof().await
}

pub(crate) async fn outbound_exchange_response_eof(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<bool, VmError> {
    if handle == DEFAULT_UPSTREAM_EXCHANGE_HANDLE {
        return upstream_response_eof(context).await;
    }
    let snapshot = ensure_outbound_exchange_response_started(context, handle).await?;
    let body = snapshot.body;
    let mut body = body.lock().await;
    body.eof().await
}

pub(crate) async fn read_downstream_response_trailers(
    context: &SharedProxyVmContext,
) -> Result<HeaderMap, VmError> {
    let (
        has_local_body,
        body_source_exchange,
        has_post_response_plan,
        has_upstream_target,
        default_upstream_websocket_mode,
    ) = {
        let downstream = context.lock_downstream();
        let exchanges = context.lock_exchanges();
        let default_exchange = exchanges
            .exchanges
            .get(&DEFAULT_UPSTREAM_EXCHANGE_HANDLE)
            .expect("default upstream exchange should exist");
        (
            downstream.response_output.has_local_body(),
            downstream.response_output.body_source_exchange,
            downstream.post_response_plan.is_some(),
            default_exchange.request.target.is_some(),
            default_exchange.websocket_dag.is_websocket_mode(),
        )
    };

    if has_local_body || has_post_response_plan {
        return Ok(HeaderMap::new());
    }
    if let Some(exchange) = body_source_exchange {
        return read_outbound_exchange_response_trailers(context, exchange).await;
    }
    if has_upstream_target && !default_upstream_websocket_mode {
        return read_upstream_response_trailers(context).await;
    }
    Ok(HeaderMap::new())
}
