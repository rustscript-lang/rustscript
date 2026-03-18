use std::{io, net::SocketAddr, sync::Arc, time::Instant};

use axum::{
    body::Body,
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode, Uri, Version,
        header::{CONNECTION, CONTENT_LENGTH, TRANSFER_ENCODING},
    },
};
use bytes::{Buf, BytesMut};
use futures_util::stream::try_unfold;
use http_body_util::BodyExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info, warn};

use super::super::SharedState;
use super::proxy_path::{
    execute_data_plane_http_request_context, finalize_data_plane_response,
    program_may_stream_downstream_http_response, record_data_plane_response_metrics,
    record_stage_metrics, record_stage_metrics_if_enabled, serve_http_connection,
    serve_http1_connection_via_hyper, stage_metrics_active,
};
use crate::{
    abi_impl::ReplayPrefixedIo,
    abi_impl::http::{
        fast_path::{
            DownstreamHttp1FastBodyKind, MAX_DOWNSTREAM_HTTP1_FAST_BODY_BYTES,
            classify_downstream_http1_fast_body_lazy, downstream_http1_fast_path_eligible_lazy,
            downstream_http1_fast_path_expects_continue_lazy,
        },
        outbound_http1::{OutboundHttp1ForwardBody, OutboundHttp1ForwardResponse},
        state::{
            DownstreamConnectionMetadata, Http1DownstreamResolution, HttpRequestContext,
            LazyHttpHeaders, LazyRequestId, ResolvedHttpGraphResponse,
            ResolvedNativeHttp1DownstreamResponse, ResolvedNativeLocalHttp1DownstreamResponse,
            ResolvedSnapshotHttp1DownstreamResponse,
            build_downstream_http_request_context_from_components, is_hop_by_hop_header,
            resolve_http1_downstream_response,
        },
    },
    logging::{
        category_access, category_program, enabled as logging_enabled, method_label, status_label,
    },
};

#[cfg(feature = "http2")]
const HTTP2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const HTTP1_CHUNK_WRITE_OVERHEAD_BYTES: usize = 32;

struct FastHttp1Request {
    method: Method,
    uri: Uri,
    version: Version,
    headers: LazyHttpHeaders,
    keep_alive: bool,
    body: Body,
}

struct FastHttp1ConnectionIo<S> {
    stream: Option<S>,
    buffered: BytesMut,
    write_scratch: BytesMut,
}

impl<S> FastHttp1ConnectionIo<S> {
    fn new(stream: S) -> Self {
        Self {
            stream: Some(stream),
            buffered: BytesMut::with_capacity(8 * 1024),
            write_scratch: BytesMut::with_capacity(8 * 1024),
        }
    }

    fn stream_mut(&mut self) -> io::Result<&mut S> {
        self.stream.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "downstream http/1.1 connection stream is unavailable",
            )
        })
    }

    fn parts_mut(&mut self) -> io::Result<(&mut S, &mut BytesMut)> {
        let stream = self.stream.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "downstream http/1.1 connection stream is unavailable",
            )
        })?;
        Ok((stream, &mut self.buffered))
    }

    fn take_owned(&mut self) -> io::Result<Self> {
        Ok(Self {
            stream: Some(self.stream.take().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "downstream http/1.1 connection stream is unavailable",
                )
            })?),
            buffered: std::mem::take(&mut self.buffered),
            write_scratch: std::mem::take(&mut self.write_scratch),
        })
    }

    fn restore(&mut self, mut other: Self) {
        self.stream = other.stream.take();
        self.buffered = other.buffered;
        self.write_scratch = other.write_scratch;
    }

    fn into_replay(self) -> ReplayPrefixedIo<S> {
        ReplayPrefixedIo::new(
            self.buffered.to_vec(),
            self.stream.expect("stream should exist"),
        )
    }

    fn stream_and_scratch_mut(&mut self) -> io::Result<(&mut S, &mut BytesMut)> {
        let Self {
            stream,
            write_scratch,
            ..
        } = self;
        let stream = stream.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "downstream http/1.1 connection stream is unavailable",
            )
        })?;
        Ok((stream, write_scratch))
    }
}

impl<S> FastHttp1ConnectionIo<S>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    async fn write_response(
        &mut self,
        request_method: &Method,
        request_keep_alive: bool,
        response: Response<Body>,
    ) -> io::Result<bool> {
        let (stream, scratch) = self.stream_and_scratch_mut()?;
        write_http1_response(
            stream,
            request_method,
            request_keep_alive,
            response,
            scratch,
        )
        .await
    }

    async fn write_native_response(
        &mut self,
        request_method: &Method,
        request_keep_alive: bool,
        native: ResolvedNativeHttp1DownstreamResponse,
    ) -> io::Result<bool> {
        let (stream, scratch) = self.stream_and_scratch_mut()?;
        write_native_http1_response(stream, request_method, request_keep_alive, native, scratch)
            .await
    }

    async fn write_snapshot_response(
        &mut self,
        request_method: &Method,
        request_keep_alive: bool,
        snapshot: ResolvedSnapshotHttp1DownstreamResponse,
    ) -> io::Result<bool> {
        let (stream, scratch) = self.stream_and_scratch_mut()?;
        write_snapshot_http1_response(
            stream,
            request_method,
            request_keep_alive,
            snapshot,
            scratch,
        )
        .await
    }

    async fn write_native_local_response(
        &mut self,
        request_method: &Method,
        request_keep_alive: bool,
        request_version: Version,
        native: ResolvedNativeLocalHttp1DownstreamResponse,
    ) -> io::Result<bool> {
        let (stream, scratch) = self.stream_and_scratch_mut()?;
        write_native_local_http1_response(
            stream,
            request_method,
            request_keep_alive,
            request_version,
            native,
            scratch,
        )
        .await
    }
}

struct FastHttp1ParsedRequest<S> {
    request: FastHttp1Request,
    body_lease: Option<FastHttp1RequestBodyLease<S>>,
}

enum FastHttp1Decision<S> {
    Request(FastHttp1ParsedRequest<S>),
    EndOfStream,
    FallbackToHyper,
}

enum FastHttp1ReadError {
    Io(io::Error),
    BadRequest(String),
    PayloadTooLarge,
}

#[derive(Clone)]
enum StoredFastHttp1ReadError {
    Io(String),
    BadRequest(String),
    PayloadTooLarge,
}

impl From<io::Error> for FastHttp1ReadError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl FastHttp1ReadError {
    fn into_io_error(self) -> io::Error {
        match self {
            FastHttp1ReadError::Io(err) => err,
            FastHttp1ReadError::BadRequest(message) => {
                io::Error::new(io::ErrorKind::InvalidData, message)
            }
            FastHttp1ReadError::PayloadTooLarge => io::Error::new(
                io::ErrorKind::InvalidData,
                "downstream http/1.1 request body exceeds fast-path limit",
            ),
        }
    }

    fn to_stored(&self) -> StoredFastHttp1ReadError {
        match self {
            FastHttp1ReadError::Io(err) => StoredFastHttp1ReadError::Io(err.to_string()),
            FastHttp1ReadError::BadRequest(message) => {
                StoredFastHttp1ReadError::BadRequest(message.clone())
            }
            FastHttp1ReadError::PayloadTooLarge => StoredFastHttp1ReadError::PayloadTooLarge,
        }
    }
}

impl StoredFastHttp1ReadError {
    fn to_read_error(&self) -> FastHttp1ReadError {
        match self {
            StoredFastHttp1ReadError::Io(message) => {
                FastHttp1ReadError::Io(io::Error::other(message.clone()))
            }
            StoredFastHttp1ReadError::BadRequest(message) => {
                FastHttp1ReadError::BadRequest(message.clone())
            }
            StoredFastHttp1ReadError::PayloadTooLarge => FastHttp1ReadError::PayloadTooLarge,
        }
    }
}

enum FastHttp1ChunkedState {
    NeedSize,
    ReadData { remaining: usize },
    ExpectChunkTerminator,
    ReadTrailers,
}

enum FastHttp1StreamingBodyKind {
    Fixed { remaining: usize },
    Chunked { state: FastHttp1ChunkedState },
}

struct FastHttp1RequestBodyState<S> {
    connection: Option<FastHttp1ConnectionIo<S>>,
    kind: FastHttp1StreamingBodyKind,
    remaining_budget: usize,
    terminal_error: Option<StoredFastHttp1ReadError>,
}

struct FastHttp1RequestBodyLease<S> {
    shared: Arc<tokio::sync::Mutex<FastHttp1RequestBodyState<S>>>,
}

fn http_header_contains_token(headers: &HeaderMap, name: HeaderName, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|value| value.eq_ignore_ascii_case(token))
}

fn http1_connection_keep_alive(version: Version, headers: &HeaderMap) -> bool {
    let connection_close = http_header_contains_token(headers, CONNECTION, "close");
    let connection_keep_alive = http_header_contains_token(headers, CONNECTION, "keep-alive");
    match version {
        Version::HTTP_10 => connection_keep_alive && !connection_close,
        _ => !connection_close,
    }
}

fn http1_connection_keep_alive_lazy(version: Version, headers: &LazyHttpHeaders) -> bool {
    let connection_close = headers.header_contains_token(CONNECTION.as_str(), "close");
    let connection_keep_alive = headers.header_contains_token(CONNECTION.as_str(), "keep-alive");
    match version {
        Version::HTTP_10 => connection_keep_alive && !connection_close,
        _ => !connection_close,
    }
}

fn http1_response_has_no_body(status: StatusCode, method: &Method) -> bool {
    method == Method::HEAD
        || (100..200).contains(&status.as_u16())
        || status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED
}

fn merge_http_headers(target: &mut HeaderMap, overlay: &HeaderMap) {
    for (name, value) in overlay {
        target.insert(name, value.clone());
    }
}

fn find_crlf(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|window| window == b"\r\n")
}

impl<S> FastHttp1RequestBodyState<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    fn from_connection(
        connection: FastHttp1ConnectionIo<S>,
        kind: FastHttp1StreamingBodyKind,
        remaining_budget: usize,
    ) -> Self {
        Self {
            connection: Some(connection),
            kind,
            remaining_budget,
            terminal_error: None,
        }
    }

    fn remember_error(&mut self, err: FastHttp1ReadError) -> FastHttp1ReadError {
        self.terminal_error = Some(err.to_stored());
        err
    }

    fn connection_mut(&mut self) -> Result<&mut FastHttp1ConnectionIo<S>, FastHttp1ReadError> {
        self.connection.as_mut().ok_or_else(|| {
            FastHttp1ReadError::Io(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "downstream http/1.1 request body connection is unavailable",
            ))
        })
    }

    async fn pull_next(&mut self) -> Result<Option<bytes::Bytes>, FastHttp1ReadError> {
        if let Some(err) = self.terminal_error.clone() {
            return Err(err.to_read_error());
        }
        loop {
            let kind = std::mem::replace(
                &mut self.kind,
                FastHttp1StreamingBodyKind::Fixed { remaining: 0 },
            );
            match kind {
                FastHttp1StreamingBodyKind::Fixed { mut remaining } => {
                    if remaining == 0 {
                        return Ok(None);
                    }
                    let available = self.connection_mut()?.buffered.len();
                    if available == 0 {
                        let read = {
                            let (stream, buffered) = self.connection_mut()?.parts_mut()?;
                            stream.read_buf(buffered).await?
                        };
                        if read == 0 {
                            let err = FastHttp1ReadError::BadRequest(
                                "downstream http/1.1 request body closed before content-length completed"
                                    .to_string(),
                            );
                            return Err(self.remember_error(err));
                        }
                        self.kind = FastHttp1StreamingBodyKind::Fixed { remaining };
                        continue;
                    }
                    let take = available.min(remaining);
                    remaining -= take;
                    let chunk = self.connection_mut()?.buffered.split_to(take).freeze();
                    self.kind = FastHttp1StreamingBodyKind::Fixed { remaining };
                    return Ok(Some(chunk));
                }
                FastHttp1StreamingBodyKind::Chunked { mut state } => match state {
                    FastHttp1ChunkedState::NeedSize => {
                        let Some(line) = ({
                            let (stream, buffered) = self.connection_mut()?.parts_mut()?;
                            read_fast_http1_line(stream, buffered).await?
                        }) else {
                            let err = FastHttp1ReadError::BadRequest(
                                "chunked downstream http/1.1 request closed before chunk size"
                                    .to_string(),
                            );
                            return Err(self.remember_error(err));
                        };
                        let line = std::str::from_utf8(&line).map_err(|err| {
                            let err = FastHttp1ReadError::BadRequest(format!(
                                "invalid utf-8 in downstream http/1.1 chunk size: {err}",
                            ));
                            self.remember_error(err)
                        })?;
                        let size = line
                            .split(';')
                            .next()
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .ok_or_else(|| {
                                let err = FastHttp1ReadError::BadRequest(
                                    "missing downstream http/1.1 chunk size".to_string(),
                                );
                                self.remember_error(err)
                            })
                            .and_then(|value| {
                                usize::from_str_radix(value, 16).map_err(|err| {
                                    let err = FastHttp1ReadError::BadRequest(format!(
                                        "invalid downstream http/1.1 chunk size `{value}`: {err}",
                                    ));
                                    self.remember_error(err)
                                })
                            })?;
                        if size == 0 {
                            state = FastHttp1ChunkedState::ReadTrailers;
                        } else {
                            if size > self.remaining_budget {
                                return Err(FastHttp1ReadError::PayloadTooLarge);
                            }
                            self.remaining_budget -= size;
                            state = FastHttp1ChunkedState::ReadData { remaining: size };
                        }
                        self.kind = FastHttp1StreamingBodyKind::Chunked { state };
                    }
                    FastHttp1ChunkedState::ReadData { mut remaining } => {
                        if remaining == 0 {
                            self.kind = FastHttp1StreamingBodyKind::Chunked {
                                state: FastHttp1ChunkedState::ExpectChunkTerminator,
                            };
                            continue;
                        }
                        let available = self.connection_mut()?.buffered.len();
                        if available == 0 {
                            let read = {
                                let (stream, buffered) = self.connection_mut()?.parts_mut()?;
                                stream.read_buf(buffered).await?
                            };
                            if read == 0 {
                                let err = FastHttp1ReadError::BadRequest(
                                    "chunked downstream http/1.1 request closed before chunk data completed"
                                        .to_string(),
                                );
                                return Err(self.remember_error(err));
                            }
                            self.kind = FastHttp1StreamingBodyKind::Chunked {
                                state: FastHttp1ChunkedState::ReadData { remaining },
                            };
                            continue;
                        }
                        let take = available.min(remaining);
                        remaining -= take;
                        let chunk = self.connection_mut()?.buffered.split_to(take).freeze();
                        self.kind = if remaining == 0 {
                            FastHttp1StreamingBodyKind::Chunked {
                                state: FastHttp1ChunkedState::ExpectChunkTerminator,
                            }
                        } else {
                            FastHttp1StreamingBodyKind::Chunked {
                                state: FastHttp1ChunkedState::ReadData { remaining },
                            }
                        };
                        return Ok(Some(chunk));
                    }
                    FastHttp1ChunkedState::ExpectChunkTerminator => {
                        let has_bytes = {
                            let (stream, buffered) = self.connection_mut()?.parts_mut()?;
                            ensure_fast_http1_buffered_bytes(stream, buffered, 2).await?
                        };
                        if !has_bytes {
                            let err = FastHttp1ReadError::BadRequest(
                                "chunked downstream http/1.1 request closed before chunk terminator"
                                    .to_string(),
                            );
                            return Err(self.remember_error(err));
                        }
                        if self.connection_mut()?.buffered.split_to(2).as_ref() != b"\r\n" {
                            let err = FastHttp1ReadError::BadRequest(
                                "invalid downstream http/1.1 chunk terminator".to_string(),
                            );
                            return Err(self.remember_error(err));
                        }
                        self.kind = FastHttp1StreamingBodyKind::Chunked {
                            state: FastHttp1ChunkedState::NeedSize,
                        };
                    }
                    FastHttp1ChunkedState::ReadTrailers => {
                        let Some(trailer) = ({
                            let (stream, buffered) = self.connection_mut()?.parts_mut()?;
                            read_fast_http1_line(stream, buffered).await?
                        }) else {
                            let err = FastHttp1ReadError::BadRequest(
                                "chunked downstream http/1.1 request closed before trailers completed"
                                    .to_string(),
                            );
                            return Err(self.remember_error(err));
                        };
                        if trailer.is_empty() {
                            return Ok(None);
                        }
                        self.kind = FastHttp1StreamingBodyKind::Chunked {
                            state: FastHttp1ChunkedState::ReadTrailers,
                        };
                    }
                },
            }
        }
    }

    async fn drain_remaining(&mut self) -> Result<(), FastHttp1ReadError> {
        while self.pull_next().await?.is_some() {}
        Ok(())
    }

    fn take_connection(&mut self) -> Option<FastHttp1ConnectionIo<S>> {
        self.connection.take()
    }
}

impl<S> FastHttp1RequestBodyLease<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    fn new(
        connection: FastHttp1ConnectionIo<S>,
        kind: FastHttp1StreamingBodyKind,
        remaining_budget: usize,
    ) -> (Body, Self) {
        let shared = Arc::new(tokio::sync::Mutex::new(
            FastHttp1RequestBodyState::from_connection(connection, kind, remaining_budget),
        ));
        let body = Body::from_stream(try_unfold(shared.clone(), |shared| async move {
            let chunk = {
                let mut state = shared.lock().await;
                state
                    .pull_next()
                    .await
                    .map_err(FastHttp1ReadError::into_io_error)?
            };
            Ok::<_, io::Error>(chunk.map(|chunk| (chunk, shared)))
        }));
        (body, Self { shared })
    }

    async fn finish(
        self,
    ) -> Result<FastHttp1ConnectionIo<S>, (FastHttp1ReadError, FastHttp1ConnectionIo<S>)> {
        let mut state = self.shared.lock().await;
        let drain_result = state.drain_remaining().await;
        let connection = state.take_connection().ok_or_else(|| {
            (
                FastHttp1ReadError::Io(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "downstream http/1.1 request body connection is unavailable",
                )),
                FastHttp1ConnectionIo {
                    stream: None,
                    buffered: BytesMut::new(),
                    write_scratch: BytesMut::new(),
                },
            )
        })?;
        match drain_result {
            Ok(()) => Ok(connection),
            Err(err) => Err((err, connection)),
        }
    }
}

async fn ensure_fast_http1_buffered_bytes<S>(
    stream: &mut S,
    buffered: &mut BytesMut,
    count: usize,
) -> Result<bool, FastHttp1ReadError>
where
    S: tokio::io::AsyncRead + Unpin,
{
    loop {
        if buffered.len() >= count {
            return Ok(true);
        }
        let read = stream.read_buf(buffered).await?;
        if read == 0 {
            return Ok(false);
        }
    }
}

async fn read_fast_http1_line<S>(
    stream: &mut S,
    buffered: &mut BytesMut,
) -> Result<Option<bytes::Bytes>, FastHttp1ReadError>
where
    S: tokio::io::AsyncRead + Unpin,
{
    loop {
        if let Some(line_end) = find_crlf(buffered) {
            let line = buffered.split_to(line_end).freeze();
            buffered.advance(2);
            return Ok(Some(line));
        }
        let read = stream.read_buf(buffered).await?;
        if read == 0 {
            return Ok(None);
        }
    }
}

fn fast_http1_error_response(status: StatusCode, body: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(CONTENT_LENGTH, body.len().to_string())
        .header(CONNECTION, "close")
        .body(Body::from(body.to_string()))
        .expect("fast http/1.1 error response should build")
}

async fn read_next_fast_http1_request<S>(
    connection: &mut FastHttp1ConnectionIo<S>,
) -> Result<FastHttp1Decision<S>, FastHttp1ReadError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    const MAX_HTTP1_HEAD_BYTES: usize = 64 * 1024;
    loop {
        if connection.buffered.len() > MAX_HTTP1_HEAD_BYTES {
            return Ok(FastHttp1Decision::FallbackToHyper);
        }

        let mut header_storage = [httparse::EMPTY_HEADER; 64];
        let mut request = httparse::Request::new(&mut header_storage);
        match request.parse(connection.buffered.as_ref()) {
            Ok(httparse::Status::Complete(consumed)) => {
                let Some(method) = request.method else {
                    return Ok(FastHttp1Decision::FallbackToHyper);
                };
                let Ok(method) = Method::from_bytes(method.as_bytes()) else {
                    return Ok(FastHttp1Decision::FallbackToHyper);
                };
                let version = match request.version {
                    Some(0) => Version::HTTP_10,
                    Some(1) => Version::HTTP_11,
                    _ => return Ok(FastHttp1Decision::FallbackToHyper),
                };
                let Some(uri) = request.path else {
                    return Ok(FastHttp1Decision::FallbackToHyper);
                };
                let Ok(uri) = uri.parse::<Uri>() else {
                    return Ok(FastHttp1Decision::FallbackToHyper);
                };
                let headers = LazyHttpHeaders::from_raw_header_bytes(
                    request
                        .headers
                        .iter()
                        .map(|header| {
                            (
                                bytes::Bytes::copy_from_slice(header.name.as_bytes()),
                                bytes::Bytes::copy_from_slice(header.value),
                            )
                        })
                        .collect(),
                );
                if !downstream_http1_fast_path_eligible_lazy(&method, &headers) {
                    return Ok(FastHttp1Decision::FallbackToHyper);
                }
                if downstream_http1_fast_path_expects_continue_lazy(&headers) {
                    connection
                        .stream_mut()?
                        .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                        .await?;
                }
                let Some(body_kind) = classify_downstream_http1_fast_body_lazy(&headers) else {
                    return Ok(FastHttp1Decision::FallbackToHyper);
                };
                connection.buffered.advance(consumed);
                let (body, body_lease) = match body_kind {
                    DownstreamHttp1FastBodyKind::Empty => (Body::empty(), None),
                    DownstreamHttp1FastBodyKind::Fixed(content_length) => {
                        let leased_io = connection.take_owned()?;
                        let (body, lease) = FastHttp1RequestBodyLease::new(
                            leased_io,
                            FastHttp1StreamingBodyKind::Fixed {
                                remaining: content_length,
                            },
                            MAX_DOWNSTREAM_HTTP1_FAST_BODY_BYTES,
                        );
                        (body, Some(lease))
                    }
                    DownstreamHttp1FastBodyKind::Chunked => {
                        let leased_io = connection.take_owned()?;
                        let (body, lease) = FastHttp1RequestBodyLease::new(
                            leased_io,
                            FastHttp1StreamingBodyKind::Chunked {
                                state: FastHttp1ChunkedState::NeedSize,
                            },
                            MAX_DOWNSTREAM_HTTP1_FAST_BODY_BYTES,
                        );
                        (body, Some(lease))
                    }
                };
                return Ok(FastHttp1Decision::Request(FastHttp1ParsedRequest {
                    request: FastHttp1Request {
                        body,
                        keep_alive: http1_connection_keep_alive_lazy(version, &headers),
                        method,
                        uri,
                        version,
                        headers,
                    },
                    body_lease,
                }));
            }
            Ok(httparse::Status::Partial) => {
                let read = {
                    let (stream, buffered) = connection.parts_mut()?;
                    stream.read_buf(buffered).await?
                };
                if read == 0 {
                    if connection.buffered.is_empty() {
                        return Ok(FastHttp1Decision::EndOfStream);
                    }
                    return Ok(FastHttp1Decision::FallbackToHyper);
                }
            }
            Err(_) => return Ok(FastHttp1Decision::FallbackToHyper),
        }
    }
}

fn build_fast_http_request_context(
    request_id: LazyRequestId,
    request: FastHttp1Request,
    connection_metadata: Option<&DownstreamConnectionMetadata>,
) -> Result<HttpRequestContext, Response<Body>> {
    let FastHttp1Request {
        method,
        uri,
        version,
        headers,
        keep_alive: _,
        body,
    } = request;
    Ok(build_downstream_http_request_context_from_components(
        request_id,
        method,
        uri,
        version,
        body,
        headers,
        connection_metadata,
    ))
}

fn write_http1_status_line(version: Version, status: StatusCode, encoded: &mut BytesMut) {
    let version_label = match version {
        Version::HTTP_10 => "HTTP/1.0",
        _ => "HTTP/1.1",
    };
    encoded.extend_from_slice(version_label.as_bytes());
    encoded.extend_from_slice(b" ");
    encoded.extend_from_slice(status.as_str().as_bytes());
    encoded.extend_from_slice(b" ");
    encoded.extend_from_slice(status.canonical_reason().unwrap_or("OK").as_bytes());
    encoded.extend_from_slice(b"\r\n");
}

fn append_chunk_prefix(encoded: &mut BytesMut, len: usize) {
    let mut value = len;
    let mut digits = [0u8; usize::BITS as usize / 4];
    let mut index = digits.len();
    loop {
        index -= 1;
        let digit = (value & 0x0f) as u8;
        digits[index] = match digit {
            0..=9 => b'0' + digit,
            _ => b'A' + (digit - 10),
        };
        value >>= 4;
        if value == 0 {
            break;
        }
    }
    encoded.extend_from_slice(&digits[index..]);
    encoded.extend_from_slice(b"\r\n");
}

async fn write_http1_response<S>(
    stream: &mut S,
    request_method: &Method,
    request_keep_alive: bool,
    response: Response<Body>,
    scratch: &mut BytesMut,
) -> io::Result<bool>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let (mut parts, mut body) = response.into_parts();
    let version = match parts.version {
        Version::HTTP_10 => Version::HTTP_10,
        _ => Version::HTTP_11,
    };
    let status = parts.status;
    let has_body = !http1_response_has_no_body(status, request_method);
    let has_content_length = parts.headers.contains_key(CONTENT_LENGTH);
    let mut use_chunked = http_header_contains_token(&parts.headers, TRANSFER_ENCODING, "chunked");
    let mut keep_alive = request_keep_alive && http1_connection_keep_alive(version, &parts.headers);

    if has_body && !has_content_length && !use_chunked {
        if version == Version::HTTP_10 {
            keep_alive = false;
        } else {
            use_chunked = true;
            parts
                .headers
                .insert(TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        }
    }

    if !keep_alive {
        parts
            .headers
            .insert(CONNECTION, HeaderValue::from_static("close"));
    } else if version == Version::HTTP_10 && !parts.headers.contains_key(CONNECTION) {
        parts
            .headers
            .insert(CONNECTION, HeaderValue::from_static("keep-alive"));
    }

    scratch.clear();
    write_http1_status_line(version, status, scratch);
    for (name, value) in &parts.headers {
        scratch.extend_from_slice(name.as_str().as_bytes());
        scratch.extend_from_slice(b": ");
        scratch.extend_from_slice(value.as_bytes());
        scratch.extend_from_slice(b"\r\n");
    }
    scratch.extend_from_slice(b"\r\n");
    stream.write_all(scratch).await?;

    if !has_body {
        return Ok(keep_alive);
    }

    let mut trailers = HeaderMap::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|err| io::Error::other(err.to_string()))?;
        match frame.into_data() {
            Ok(data) => {
                if use_chunked {
                    scratch.clear();
                    scratch.reserve(HTTP1_CHUNK_WRITE_OVERHEAD_BYTES + data.len());
                    append_chunk_prefix(scratch, data.len());
                    scratch.extend_from_slice(&data);
                    scratch.extend_from_slice(b"\r\n");
                    stream.write_all(scratch).await?;
                } else {
                    stream.write_all(&data).await?;
                }
            }
            Err(frame) => {
                if let Ok(frame_trailers) = frame.into_trailers() {
                    trailers = frame_trailers;
                } else {
                    keep_alive = false;
                }
            }
        }
    }

    if use_chunked {
        scratch.clear();
        scratch.reserve(HTTP1_CHUNK_WRITE_OVERHEAD_BYTES);
        scratch.extend_from_slice(b"0\r\n");
        for (name, value) in &trailers {
            scratch.extend_from_slice(name.as_str().as_bytes());
            scratch.extend_from_slice(b": ");
            scratch.extend_from_slice(value.as_bytes());
            scratch.extend_from_slice(b"\r\n");
        }
        scratch.extend_from_slice(b"\r\n");
        stream.write_all(scratch).await?;
    }

    Ok(keep_alive)
}

async fn write_native_http1_response<S>(
    stream: &mut S,
    request_method: &Method,
    request_keep_alive: bool,
    native: ResolvedNativeHttp1DownstreamResponse,
    scratch: &mut BytesMut,
) -> io::Result<bool>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let ResolvedNativeHttp1DownstreamResponse {
        response,
        response_headers,
        response_status,
        upstream_latency_ms: _,
    } = native;
    let OutboundHttp1ForwardResponse {
        status,
        mut headers,
        version,
        body,
        upstream_latency_ms: _,
        negotiated_alpn: _,
        peer_certificate_der: _,
    } = response;

    let hop_by_hop_headers = headers
        .keys()
        .filter(|name| is_hop_by_hop_header(name))
        .cloned()
        .collect::<Vec<_>>();
    for header in hop_by_hop_headers {
        headers.remove(header);
    }
    merge_http_headers(&mut headers, &response_headers);

    let status = response_status
        .and_then(|code| StatusCode::from_u16(code).ok())
        .unwrap_or_else(|| StatusCode::from_u16(status).unwrap_or(StatusCode::OK));
    let version = match version {
        Version::HTTP_10 => Version::HTTP_10,
        _ => Version::HTTP_11,
    };
    let has_body = !http1_response_has_no_body(status, request_method);
    let body_content_length = match &body {
        OutboundHttp1ForwardBody::Empty => Some(0),
        OutboundHttp1ForwardBody::Raw { content_length, .. } => *content_length,
    };
    if !headers.contains_key(CONTENT_LENGTH)
        && !http_header_contains_token(&headers, TRANSFER_ENCODING, "chunked")
        && let Some(content_length) = body_content_length
        && let Ok(value) = HeaderValue::from_str(&content_length.to_string())
    {
        headers.insert(CONTENT_LENGTH, value);
    }

    let has_content_length = headers.contains_key(CONTENT_LENGTH);
    let mut use_chunked = http_header_contains_token(&headers, TRANSFER_ENCODING, "chunked");
    let mut keep_alive = request_keep_alive && http1_connection_keep_alive(version, &headers);

    if has_body && !has_content_length && !use_chunked {
        if version == Version::HTTP_10 {
            keep_alive = false;
        } else {
            use_chunked = true;
            headers.insert(TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        }
    }

    if !keep_alive {
        headers.insert(CONNECTION, HeaderValue::from_static("close"));
    } else if version == Version::HTTP_10 && !headers.contains_key(CONNECTION) {
        headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
    }

    scratch.clear();
    write_http1_status_line(version, status, scratch);
    for (name, value) in &headers {
        scratch.extend_from_slice(name.as_str().as_bytes());
        scratch.extend_from_slice(b": ");
        scratch.extend_from_slice(value.as_bytes());
        scratch.extend_from_slice(b"\r\n");
    }
    scratch.extend_from_slice(b"\r\n");
    stream.write_all(scratch).await?;

    if has_body {
        let mut trailers = HeaderMap::new();
        match body {
            OutboundHttp1ForwardBody::Empty => {}
            OutboundHttp1ForwardBody::Raw { mut body, .. } => {
                while let Some(chunk) = body
                    .pull_next()
                    .await
                    .map_err(|err| io::Error::other(err.to_string()))?
                {
                    if use_chunked {
                        scratch.clear();
                        scratch.reserve(HTTP1_CHUNK_WRITE_OVERHEAD_BYTES + chunk.len());
                        append_chunk_prefix(scratch, chunk.len());
                        scratch.extend_from_slice(&chunk);
                        scratch.extend_from_slice(b"\r\n");
                        stream.write_all(scratch).await?;
                    } else {
                        stream.write_all(&chunk).await?;
                    }
                }
                if let Some(body_trailers) = body.take_trailers() {
                    trailers = body_trailers;
                }
            }
        }
        if use_chunked {
            scratch.clear();
            scratch.reserve(HTTP1_CHUNK_WRITE_OVERHEAD_BYTES);
            scratch.extend_from_slice(b"0\r\n");
            for (name, value) in &trailers {
                scratch.extend_from_slice(name.as_str().as_bytes());
                scratch.extend_from_slice(b": ");
                scratch.extend_from_slice(value.as_bytes());
                scratch.extend_from_slice(b"\r\n");
            }
            scratch.extend_from_slice(b"\r\n");
            stream.write_all(scratch).await?;
        }
    }

    Ok(keep_alive)
}

async fn write_snapshot_http1_response<S>(
    stream: &mut S,
    request_method: &Method,
    request_keep_alive: bool,
    snapshot: ResolvedSnapshotHttp1DownstreamResponse,
    scratch: &mut BytesMut,
) -> io::Result<bool>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let mut body = snapshot.take_body_passthrough().await;
    let (status, mut headers, version, _) = snapshot.into_head();
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let version = match version {
        Version::HTTP_10 => Version::HTTP_10,
        _ => Version::HTTP_11,
    };
    let has_body = !http1_response_has_no_body(status, request_method);
    let has_content_length = headers.contains_name(CONTENT_LENGTH);
    let mut use_chunked = headers.header_contains_token(TRANSFER_ENCODING, "chunked");
    let mut keep_alive = request_keep_alive && headers.connection_keep_alive(version);

    if has_body && !has_content_length && !use_chunked && body.is_some() {
        if version == Version::HTTP_10 {
            keep_alive = false;
        } else {
            use_chunked = true;
            headers.insert_override(TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        }
    }

    if !keep_alive {
        headers.insert_override(CONNECTION, HeaderValue::from_static("close"));
    } else if version == Version::HTTP_10 && !headers.contains_name(CONNECTION) {
        headers.insert_override(CONNECTION, HeaderValue::from_static("keep-alive"));
    }

    scratch.clear();
    write_http1_status_line(version, status, scratch);
    headers.write_http1_lines(scratch);
    scratch.extend_from_slice(b"\r\n");
    stream.write_all(scratch).await?;

    if has_body && let Some(body_passthrough) = body.as_mut() {
        let mut trailers = HeaderMap::new();
        while let Some(frame) = body_passthrough
            .next_frame()
            .await
            .map_err(|err: vm::VmError| io::Error::other(err.to_string()))?
        {
            match frame.into_data() {
                Ok(chunk) => {
                    if use_chunked {
                        scratch.clear();
                        scratch.reserve(HTTP1_CHUNK_WRITE_OVERHEAD_BYTES + chunk.len());
                        append_chunk_prefix(scratch, chunk.len());
                        scratch.extend_from_slice(&chunk);
                        scratch.extend_from_slice(b"\r\n");
                        stream.write_all(scratch).await?;
                    } else {
                        stream.write_all(&chunk).await?;
                    }
                }
                Err(frame) => {
                    if let Ok(frame_trailers) = frame.into_trailers() {
                        trailers = frame_trailers;
                    } else {
                        keep_alive = false;
                    }
                }
            }
        }

        if use_chunked {
            scratch.clear();
            scratch.reserve(HTTP1_CHUNK_WRITE_OVERHEAD_BYTES);
            scratch.extend_from_slice(b"0\r\n");
            for (name, value) in &trailers {
                scratch.extend_from_slice(name.as_str().as_bytes());
                scratch.extend_from_slice(b": ");
                scratch.extend_from_slice(value.as_bytes());
                scratch.extend_from_slice(b"\r\n");
            }
            scratch.extend_from_slice(b"\r\n");
            stream.write_all(scratch).await?;
        }
    }

    Ok(keep_alive)
}

async fn write_native_local_http1_response<S>(
    stream: &mut S,
    request_method: &Method,
    request_keep_alive: bool,
    request_version: Version,
    native: ResolvedNativeLocalHttp1DownstreamResponse,
    scratch: &mut BytesMut,
) -> io::Result<bool>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let ResolvedNativeLocalHttp1DownstreamResponse {
        status,
        mut headers,
        body,
        default_content_type,
    } = native;
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let version = match request_version {
        Version::HTTP_10 => Version::HTTP_10,
        _ => Version::HTTP_11,
    };
    let has_body = !http1_response_has_no_body(status, request_method);
    let body_len = if has_body { body.len() } else { 0 };

    headers.remove(TRANSFER_ENCODING);
    if let Ok(value) = HeaderValue::from_str(&body_len.to_string()) {
        headers.insert(CONTENT_LENGTH, value);
    }
    if default_content_type
        && body_len > 0
        && !headers.contains_key(axum::http::header::CONTENT_TYPE)
    {
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain"),
        );
    }

    let keep_alive = request_keep_alive && http1_connection_keep_alive(version, &headers);
    if !keep_alive {
        headers.insert(CONNECTION, HeaderValue::from_static("close"));
    } else if version == Version::HTTP_10 && !headers.contains_key(CONNECTION) {
        headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
    }

    scratch.clear();
    scratch.reserve(1024);
    write_http1_status_line(version, status, scratch);
    for (name, value) in &headers {
        scratch.extend_from_slice(name.as_str().as_bytes());
        scratch.extend_from_slice(b": ");
        scratch.extend_from_slice(value.as_bytes());
        scratch.extend_from_slice(b"\r\n");
    }
    scratch.extend_from_slice(b"\r\n");
    stream.write_all(scratch).await?;
    if body_len > 0 {
        stream.write_all(&body).await?;
    }
    Ok(keep_alive)
}

async fn serve_http1_fast_connection<S>(
    state: SharedState,
    stream: S,
    peer_addr: SocketAddr,
    connection_metadata: Option<DownstreamConnectionMetadata>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut connection = FastHttp1ConnectionIo::new(stream);
    loop {
        let decision = match read_next_fast_http1_request(&mut connection).await {
            Ok(decision) => decision,
            Err(FastHttp1ReadError::Io(err)) => {
                warn!(
                    "{} downstream http/1.1 fast path read failed for {peer_addr}: {err}",
                    category_program()
                );
                return;
            }
            Err(FastHttp1ReadError::BadRequest(message)) => {
                let response = fast_http1_error_response(StatusCode::BAD_REQUEST, &message);
                let _ = connection
                    .write_response(&Method::GET, false, response)
                    .await;
                return;
            }
            Err(FastHttp1ReadError::PayloadTooLarge) => {
                let response = fast_http1_error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request body exceeds fast http/1.1 limit",
                );
                let _ = connection
                    .write_response(&Method::GET, false, response)
                    .await;
                return;
            }
        };
        let FastHttp1Decision::Request(parsed_request) = decision else {
            match decision {
                FastHttp1Decision::EndOfStream => return,
                FastHttp1Decision::FallbackToHyper => {
                    let replay = connection.into_replay();
                    serve_http1_connection_via_hyper(state, replay, peer_addr, connection_metadata)
                        .await;
                    return;
                }
                FastHttp1Decision::Request(_) => unreachable!(),
            }
        };
        let FastHttp1ParsedRequest {
            request,
            body_lease,
        } = parsed_request;

        let request_method = request.method.clone();
        let request_uri = request.uri.clone();
        let request_version = request.version;
        let request_keep_alive = request.keep_alive;
        let mut body_lease = body_lease;
        if state.loaded_program_snapshot().is_none() {
            let started =
                (stage_metrics_active() || state.metrics_collection_enabled()).then(Instant::now);
            if let Some(body_lease) = body_lease.take() {
                match body_lease.finish().await {
                    Ok(returned) => connection.restore(returned),
                    Err((FastHttp1ReadError::Io(err), returned)) => {
                        connection.restore(returned);
                        warn!(
                            "{} downstream http/1.1 request body finalize failed for {peer_addr}: {err}",
                            category_program()
                        );
                        return;
                    }
                    Err((FastHttp1ReadError::BadRequest(message), returned)) => {
                        connection.restore(returned);
                        let response = fast_http1_error_response(StatusCode::BAD_REQUEST, &message);
                        let _ = connection
                            .write_response(&request_method, false, response)
                            .await;
                        return;
                    }
                    Err((FastHttp1ReadError::PayloadTooLarge, returned)) => {
                        connection.restore(returned);
                        let response = fast_http1_error_response(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "request body exceeds fast http/1.1 limit",
                        );
                        let _ = connection
                            .write_response(&request_method, false, response)
                            .await;
                        return;
                    }
                }
            }
            if stage_metrics_active() {
                let finished = Instant::now();
                record_stage_metrics(
                    0,
                    0,
                    0,
                    u64::try_from(
                        finished
                            .duration_since(started.expect("stage metrics start should exist"))
                            .as_micros(),
                    )
                    .unwrap_or(u64::MAX),
                );
            }
            record_data_plane_response_metrics(&state, started, StatusCode::NOT_FOUND.as_u16(), 0);
            match connection
                .write_native_local_response(
                    &request_method,
                    request_keep_alive,
                    request_version,
                    ResolvedNativeLocalHttp1DownstreamResponse {
                        status: StatusCode::NOT_FOUND.as_u16(),
                        headers: HeaderMap::new(),
                        body: b"not found".to_vec(),
                        default_content_type: false,
                    },
                )
                .await
            {
                Ok(keep_alive) => {
                    if logging_enabled() {
                        info!(
                            "{} {} {} {}",
                            category_access(),
                            method_label(request_method.as_str()),
                            status_label(StatusCode::NOT_FOUND.as_u16()),
                            request_uri,
                        );
                    }
                    if !keep_alive {
                        return;
                    }
                    continue;
                }
                Err(err) => {
                    warn!(
                        "{} downstream http/1.1 fast path write failed for {peer_addr}: {err}",
                        category_program()
                    );
                    return;
                }
            }
        }
        let request_id = LazyRequestId::deferred();
        let vm_request = match build_fast_http_request_context(
            request_id,
            request,
            connection_metadata.as_ref(),
        ) {
            Ok(vm_request) => vm_request,
            Err(response) => {
                let _ = connection
                    .write_response(&request_method, false, response)
                    .await;
                return;
            }
        };
        state.record_data_plane_request();
        let started =
            (stage_metrics_active() || state.metrics_collection_enabled()).then(Instant::now);
        let execution = execute_data_plane_http_request_context(
            &state,
            vm_request,
            connection_metadata.clone(),
            None,
            #[cfg(feature = "http3")]
            None,
            None,
        )
        .await;
        let delay_body_finalize = matches!(
            &execution,
            Ok((vm_context, ..)) if vm_context.native_default_upstream_http_forward_active()
        );
        if !delay_body_finalize && let Some(body_lease) = body_lease.take() {
            match body_lease.finish().await {
                Ok(returned) => connection.restore(returned),
                Err((FastHttp1ReadError::Io(err), returned)) => {
                    connection.restore(returned);
                    warn!(
                        "{} downstream http/1.1 request body finalize failed for {peer_addr}: {err}",
                        category_program()
                    );
                    return;
                }
                Err((FastHttp1ReadError::BadRequest(message), returned)) => {
                    connection.restore(returned);
                    let response = fast_http1_error_response(StatusCode::BAD_REQUEST, &message);
                    let _ = connection
                        .write_response(&request_method, false, response)
                        .await;
                    return;
                }
                Err((FastHttp1ReadError::PayloadTooLarge, returned)) => {
                    connection.restore(returned);
                    let response = fast_http1_error_response(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "request body exceeds fast http/1.1 limit",
                    );
                    let _ = connection
                        .write_response(&request_method, false, response)
                        .await;
                    return;
                }
            }
        }
        let (keep_alive, response_status) = match execution {
            Ok((vm_context, pre_vm_finished, after_vm)) => {
                match resolve_http1_downstream_response(&vm_context).await {
                    Http1DownstreamResolution::NativeLocal(native_local) => {
                        if let Some(body_lease) = body_lease.take() {
                            match body_lease.finish().await {
                                Ok(returned) => connection.restore(returned),
                                Err((FastHttp1ReadError::Io(err), returned)) => {
                                    connection.restore(returned);
                                    warn!(
                                        "{} downstream http/1.1 request body finalize failed for {peer_addr}: {err}",
                                        category_program()
                                    );
                                    return;
                                }
                                Err((FastHttp1ReadError::BadRequest(message), returned)) => {
                                    connection.restore(returned);
                                    let response = fast_http1_error_response(
                                        StatusCode::BAD_REQUEST,
                                        &message,
                                    );
                                    let _ = connection
                                        .write_response(&request_method, false, response)
                                        .await;
                                    return;
                                }
                                Err((FastHttp1ReadError::PayloadTooLarge, returned)) => {
                                    connection.restore(returned);
                                    let response = fast_http1_error_response(
                                        StatusCode::PAYLOAD_TOO_LARGE,
                                        "request body exceeds fast http/1.1 limit",
                                    );
                                    let _ = connection
                                        .write_response(&request_method, false, response)
                                        .await;
                                    return;
                                }
                            }
                        }
                        record_stage_metrics_if_enabled(started, pre_vm_finished, after_vm);
                        let response_status =
                            StatusCode::from_u16(native_local.status).unwrap_or(StatusCode::OK);
                        record_data_plane_response_metrics(
                            &state,
                            started,
                            response_status.as_u16(),
                            0,
                        );
                        match connection
                            .write_native_local_response(
                                &request_method,
                                request_keep_alive,
                                request_version,
                                native_local,
                            )
                            .await
                        {
                            Ok(keep_alive) => (keep_alive, response_status),
                            Err(err) => {
                                warn!(
                                    "{} downstream http/1.1 fast path write failed for {peer_addr}: {err}",
                                    category_program()
                                );
                                return;
                            }
                        }
                    }
                    Http1DownstreamResolution::Native(native_result) => {
                        if let Some(body_lease) = body_lease.take() {
                            match body_lease.finish().await {
                                Ok(returned) => connection.restore(returned),
                                Err((FastHttp1ReadError::Io(err), returned)) => {
                                    connection.restore(returned);
                                    warn!(
                                        "{} downstream http/1.1 request body finalize failed for {peer_addr}: {err}",
                                        category_program()
                                    );
                                    return;
                                }
                                Err((FastHttp1ReadError::BadRequest(message), returned)) => {
                                    connection.restore(returned);
                                    let response = fast_http1_error_response(
                                        StatusCode::BAD_REQUEST,
                                        &message,
                                    );
                                    let _ = connection
                                        .write_response(&request_method, false, response)
                                        .await;
                                    return;
                                }
                                Err((FastHttp1ReadError::PayloadTooLarge, returned)) => {
                                    connection.restore(returned);
                                    let response = fast_http1_error_response(
                                        StatusCode::PAYLOAD_TOO_LARGE,
                                        "request body exceeds fast http/1.1 limit",
                                    );
                                    let _ = connection
                                        .write_response(&request_method, false, response)
                                        .await;
                                    return;
                                }
                            }
                        }
                        record_stage_metrics_if_enabled(started, pre_vm_finished, after_vm);
                        match native_result {
                            Ok(native_response) => {
                                let response_status = native_response
                                    .response_status
                                    .and_then(|code| StatusCode::from_u16(code).ok())
                                    .unwrap_or_else(|| {
                                        StatusCode::from_u16(native_response.response.status)
                                            .unwrap_or(StatusCode::OK)
                                    });
                                record_data_plane_response_metrics(
                                    &state,
                                    started,
                                    response_status.as_u16(),
                                    native_response.upstream_latency_ms,
                                );
                                match connection
                                    .write_native_response(
                                        &request_method,
                                        request_keep_alive,
                                        native_response,
                                    )
                                    .await
                                {
                                    Ok(keep_alive) => (keep_alive, response_status),
                                    Err(err) => {
                                        warn!(
                                            "{} downstream http/1.1 fast path write failed for {peer_addr}: {err}",
                                            category_program()
                                        );
                                        return;
                                    }
                                }
                            }
                            Err(response) => {
                                let response =
                                    finalize_data_plane_response(&state, started, response, 0);
                                let response_status = response.status();
                                match connection
                                    .write_response(&request_method, request_keep_alive, response)
                                    .await
                                {
                                    Ok(keep_alive) => (keep_alive, response_status),
                                    Err(err) => {
                                        warn!(
                                            "{} downstream http/1.1 fast path write failed for {peer_addr}: {err}",
                                            category_program()
                                        );
                                        return;
                                    }
                                }
                            }
                        }
                    }
                    Http1DownstreamResolution::Snapshot(snapshot_result) => {
                        if let Some(body_lease) = body_lease.take() {
                            match body_lease.finish().await {
                                Ok(returned) => connection.restore(returned),
                                Err((FastHttp1ReadError::Io(err), returned)) => {
                                    connection.restore(returned);
                                    warn!(
                                        "{} downstream http/1.1 request body finalize failed for {peer_addr}: {err}",
                                        category_program()
                                    );
                                    return;
                                }
                                Err((FastHttp1ReadError::BadRequest(message), returned)) => {
                                    connection.restore(returned);
                                    let response = fast_http1_error_response(
                                        StatusCode::BAD_REQUEST,
                                        &message,
                                    );
                                    let _ = connection
                                        .write_response(&request_method, false, response)
                                        .await;
                                    return;
                                }
                                Err((FastHttp1ReadError::PayloadTooLarge, returned)) => {
                                    connection.restore(returned);
                                    let response = fast_http1_error_response(
                                        StatusCode::PAYLOAD_TOO_LARGE,
                                        "request body exceeds fast http/1.1 limit",
                                    );
                                    let _ = connection
                                        .write_response(&request_method, false, response)
                                        .await;
                                    return;
                                }
                            }
                        }
                        record_stage_metrics_if_enabled(started, pre_vm_finished, after_vm);
                        match snapshot_result {
                            Ok(snapshot_response) => {
                                let response_status =
                                    StatusCode::from_u16(snapshot_response.status)
                                        .unwrap_or(StatusCode::OK);
                                record_data_plane_response_metrics(
                                    &state,
                                    started,
                                    response_status.as_u16(),
                                    snapshot_response.upstream_latency_ms,
                                );
                                match connection
                                    .write_snapshot_response(
                                        &request_method,
                                        request_keep_alive,
                                        snapshot_response,
                                    )
                                    .await
                                {
                                    Ok(keep_alive) => (keep_alive, response_status),
                                    Err(err) => {
                                        warn!(
                                            "{} downstream http/1.1 fast path write failed for {peer_addr}: {err}",
                                            category_program()
                                        );
                                        return;
                                    }
                                }
                            }
                            Err(response) => {
                                let response =
                                    finalize_data_plane_response(&state, started, response, 0);
                                let response_status = response.status();
                                match connection
                                    .write_response(&request_method, request_keep_alive, response)
                                    .await
                                {
                                    Ok(keep_alive) => (keep_alive, response_status),
                                    Err(err) => {
                                        warn!(
                                            "{} downstream http/1.1 fast path write failed for {peer_addr}: {err}",
                                            category_program()
                                        );
                                        return;
                                    }
                                }
                            }
                        }
                    }
                    Http1DownstreamResolution::Graph(resolved) => {
                        if let Some(body_lease) = body_lease.take() {
                            match body_lease.finish().await {
                                Ok(returned) => connection.restore(returned),
                                Err((FastHttp1ReadError::Io(err), returned)) => {
                                    connection.restore(returned);
                                    warn!(
                                        "{} downstream http/1.1 request body finalize failed for {peer_addr}: {err}",
                                        category_program()
                                    );
                                    return;
                                }
                                Err((FastHttp1ReadError::BadRequest(message), returned)) => {
                                    connection.restore(returned);
                                    let response = fast_http1_error_response(
                                        StatusCode::BAD_REQUEST,
                                        &message,
                                    );
                                    let _ = connection
                                        .write_response(&request_method, false, response)
                                        .await;
                                    return;
                                }
                                Err((FastHttp1ReadError::PayloadTooLarge, returned)) => {
                                    connection.restore(returned);
                                    let response = fast_http1_error_response(
                                        StatusCode::PAYLOAD_TOO_LARGE,
                                        "request body exceeds fast http/1.1 limit",
                                    );
                                    let _ = connection
                                        .write_response(&request_method, false, response)
                                        .await;
                                    return;
                                }
                            }
                        }
                        let ResolvedHttpGraphResponse {
                            response,
                            upstream_latency_ms,
                            post_response_plan,
                        } = resolved;
                        if let Some(plan) = post_response_plan {
                            tokio::spawn(async move {
                                if let Err(err) = plan.run().await {
                                    warn!(
                                        "{} downstream post-response transport failed: {err}",
                                        category_program()
                                    );
                                }
                            });
                        }
                        record_stage_metrics_if_enabled(started, pre_vm_finished, after_vm);
                        let response = finalize_data_plane_response(
                            &state,
                            started,
                            response,
                            upstream_latency_ms,
                        );
                        let response_status = response.status();
                        match connection
                            .write_response(&request_method, request_keep_alive, response)
                            .await
                        {
                            Ok(keep_alive) => (keep_alive, response_status),
                            Err(err) => {
                                warn!(
                                    "{} downstream http/1.1 fast path write failed for {peer_addr}: {err}",
                                    category_program()
                                );
                                return;
                            }
                        }
                    }
                }
            }
            Err(response) => {
                let response = finalize_data_plane_response(&state, started, response, 0);
                let response_status = response.status();
                match connection
                    .write_response(&request_method, request_keep_alive, response)
                    .await
                {
                    Ok(keep_alive) => (keep_alive, response_status),
                    Err(err) => {
                        warn!(
                            "{} downstream http/1.1 fast path write failed for {peer_addr}: {err}",
                            category_program()
                        );
                        return;
                    }
                }
            }
        };
        if logging_enabled() {
            info!(
                "{} {} {} {}",
                category_access(),
                method_label(request_method.as_str()),
                status_label(response_status.as_u16()),
                request_uri,
            );
        }
        if !keep_alive {
            return;
        }
    }
}

pub(super) async fn serve_http1_connection<S>(
    state: SharedState,
    stream: S,
    peer_addr: SocketAddr,
    connection_metadata: Option<DownstreamConnectionMetadata>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    if program_may_stream_downstream_http_response(state.loaded_program_snapshot().as_deref()) {
        serve_http1_connection_via_hyper(state, stream, peer_addr, connection_metadata).await;
        return;
    }
    serve_http1_fast_connection(state, stream, peer_addr, connection_metadata).await;
}

#[cfg(feature = "http2")]
#[cfg(feature = "http2")]
pub(super) async fn serve_http_auto_connection(
    state: SharedState,
    stream: tokio::net::TcpStream,
    peer_addr: SocketAddr,
    connection_metadata: Option<DownstreamConnectionMetadata>,
) {
    let mut preface = [0u8; 24];
    let preface_len = stream.peek(&mut preface).await.unwrap_or(0);
    if preface_len >= HTTP2_PREFACE.len() && &preface[..HTTP2_PREFACE.len()] == HTTP2_PREFACE {
        serve_http_connection(state, stream, peer_addr, connection_metadata).await;
    } else {
        serve_http1_connection(state, stream, peer_addr, connection_metadata).await;
    }
}
