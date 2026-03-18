use super::*;

enum UpstreamResponseSource {
    #[cfg_attr(not(feature = "http2"), allow(dead_code))]
    Hyper(hyper::body::Incoming),
    PlainHttp1(PlainHttp1ResponseBody),
    #[cfg(feature = "http3")]
    Http3(Box<h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>>),
    Exhausted,
}

struct UpstreamResponseBodySource {
    source: UpstreamResponseSource,
    http2_tracker: Option<http2::Http2ResponseBodyTracker>,
    http3_tracker: Option<http3::Http3ResponseBodyTracker>,
    plain_http1_sender_lease: Option<PlainHttp1SenderLease>,
    remaining_body_bytes: Option<u64>,
    body_started: bool,
    body_finished: bool,
    trailers: Option<HeaderMap>,
}

impl Default for UpstreamResponseBodySource {
    fn default() -> Self {
        Self {
            source: UpstreamResponseSource::Exhausted,
            http2_tracker: None,
            http3_tracker: None,
            plain_http1_sender_lease: None,
            remaining_body_bytes: None,
            body_started: false,
            body_finished: false,
            trailers: None,
        }
    }
}

impl UpstreamResponseBodySource {
    fn note_body_ready(&mut self) {
        if !self.body_started {
            if let Some(tracker) = &self.http2_tracker {
                tracker.note_response_body_ready();
            }
            if let Some(tracker) = &self.http3_tracker {
                tracker.note_response_body_ready();
            }
            self.body_started = true;
        }
    }

    fn note_body_complete(&mut self) {
        self.note_body_ready();
        if !self.body_finished {
            if let Some(tracker) = &self.http2_tracker {
                tracker.note_body_eof();
            }
            if let Some(tracker) = &self.http3_tracker {
                tracker.note_body_eof();
            }
            if let Some(lease) = self.plain_http1_sender_lease.as_mut() {
                lease.release();
            }
            self.body_finished = true;
        }
    }

    fn note_chunk_delivered(&mut self, chunk_len: usize) {
        if chunk_len == 0 {
            return;
        }
        self.note_body_ready();
        if let Some(remaining) = self.remaining_body_bytes.as_mut() {
            let consumed = u64::try_from(chunk_len).unwrap_or(u64::MAX);
            *remaining = remaining.saturating_sub(consumed);
            if *remaining == 0 {
                self.note_body_complete();
            }
        }
    }
}

impl BufferedByteSource for UpstreamResponseBodySource {
    fn pull_next<'a>(&'a mut self) -> BufferedByteSourceFuture<'a> {
        Box::pin(async move {
            match &mut self.source {
                UpstreamResponseSource::Hyper(body) => match body.frame().await {
                    Some(Ok(frame)) => match frame.into_data() {
                        Ok(chunk) => {
                            self.note_chunk_delivered(chunk.len());
                            Ok(BufferedByteStreamPull::Chunk(chunk))
                        }
                        Err(frame) => match frame.into_trailers() {
                            Ok(trailers) => {
                                self.trailers = Some(trailers);
                                self.note_body_complete();
                                self.source = UpstreamResponseSource::Exhausted;
                                Ok(BufferedByteStreamPull::Eof)
                            }
                            Err(_) => Ok(BufferedByteStreamPull::Skip),
                        },
                    },
                    Some(Err(err)) => {
                        let observed = http2::classify_http2_error(&err);
                        if let Some(tracker) = &self.http2_tracker {
                            tracker.note_body_error(&observed);
                        }
                        Err(VmError::HostError(format!(
                            "failed to read upstream response frame: {}",
                            observed.message,
                        )))
                    }
                    None => {
                        self.note_body_complete();
                        self.source = UpstreamResponseSource::Exhausted;
                        Ok(BufferedByteStreamPull::Eof)
                    }
                },
                UpstreamResponseSource::PlainHttp1(body) => match body.pull_next().await? {
                    Some(chunk) => {
                        self.note_chunk_delivered(chunk.len());
                        Ok(BufferedByteStreamPull::Chunk(chunk))
                    }
                    None => {
                        self.trailers = body.take_trailers();
                        self.note_body_complete();
                        self.source = UpstreamResponseSource::Exhausted;
                        Ok(BufferedByteStreamPull::Eof)
                    }
                },
                #[cfg(feature = "http3")]
                UpstreamResponseSource::Http3(request_stream) => {
                    match request_stream.as_mut().recv_data().await {
                        Ok(Some(mut chunk)) => {
                            let bytes = chunk.copy_to_bytes(chunk.remaining());
                            self.note_chunk_delivered(bytes.len());
                            Ok(BufferedByteStreamPull::Chunk(bytes))
                        }
                        Ok(None) => {
                            self.note_body_complete();
                            self.source = UpstreamResponseSource::Exhausted;
                            Ok(BufferedByteStreamPull::Eof)
                        }
                        Err(err) => {
                            let observed = http3::classify_http3_error(&err);
                            if let Some(tracker) = &self.http3_tracker {
                                tracker.note_body_error(&observed);
                            }
                            Err(VmError::HostError(format!(
                                "failed to read upstream http3 response frame: {}",
                                observed.message,
                            )))
                        }
                    }
                }
                UpstreamResponseSource::Exhausted => Ok(BufferedByteStreamPull::Eof),
            }
        })
    }
}

pub(super) struct UpstreamResponseBodyState {
    source: UpstreamResponseBodySource,
    stream: BufferedByteStream,
}

pub(super) struct StreamingUpstreamResponseBodyState {
    prefix: Option<Bytes>,
    source: UpstreamResponseBodySource,
    trailers_sent: bool,
}

impl StreamingUpstreamResponseBodyState {
    pub(super) async fn next_frame(&mut self) -> Result<Option<Frame<Bytes>>, VmError> {
        if let Some(prefix) = self.prefix.take()
            && !prefix.is_empty()
        {
            return Ok(Some(Frame::data(prefix)));
        }

        loop {
            match self.source.pull_next().await? {
                BufferedByteStreamPull::Chunk(chunk) => {
                    if !chunk.is_empty() {
                        return Ok(Some(Frame::data(chunk)));
                    }
                }
                BufferedByteStreamPull::Skip => {}
                BufferedByteStreamPull::Eof => {
                    if !self.trailers_sent
                        && let Some(trailers) = self.source.trailers.take()
                    {
                        self.trailers_sent = true;
                        return Ok(Some(Frame::trailers(trailers)));
                    }
                    return Ok(None);
                }
            }
        }
    }
}

impl std::fmt::Debug for UpstreamResponseBodyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamResponseBodyState")
            .field("stream", &self.stream)
            .finish()
    }
}

fn upstream_response_body_source(
    source: UpstreamResponseSource,
    http2_tracker: Option<http2::Http2ResponseBodyTracker>,
    http3_tracker: Option<http3::Http3ResponseBodyTracker>,
    plain_http1_sender_lease: Option<PlainHttp1SenderLease>,
    content_length: Option<u64>,
) -> UpstreamResponseBodySource {
    let mut source = UpstreamResponseBodySource {
        source,
        http2_tracker,
        http3_tracker,
        plain_http1_sender_lease,
        remaining_body_bytes: content_length,
        body_started: false,
        body_finished: false,
        trailers: None,
    };
    if matches!(content_length, Some(0)) {
        source.note_body_complete();
    }
    source
}

impl UpstreamResponseBodyState {
    pub(super) fn empty() -> Self {
        Self {
            source: UpstreamResponseBodySource::default(),
            stream: BufferedByteStream {
                eof: true,
                ..BufferedByteStream::default()
            },
        }
    }

    #[cfg_attr(not(feature = "http2"), allow(dead_code))]
    pub(super) fn from_hyper(
        body: hyper::body::Incoming,
        http2_tracker: Option<http2::Http2ResponseBodyTracker>,
        plain_http1_sender_lease: Option<PlainHttp1SenderLease>,
        content_length: Option<u64>,
    ) -> Self {
        if matches!(content_length, Some(0)) {
            if let Some(mut lease) = plain_http1_sender_lease {
                lease.release();
            }
            return Self::empty();
        }
        Self {
            source: upstream_response_body_source(
                UpstreamResponseSource::Hyper(body),
                http2_tracker,
                None,
                plain_http1_sender_lease,
                content_length,
            ),
            stream: BufferedByteStream::default(),
        }
    }

    pub(super) fn from_plain_http1(
        body: PlainHttp1ResponseBody,
        content_length: Option<u64>,
    ) -> Self {
        if matches!(content_length, Some(0)) {
            return Self::empty();
        }
        Self {
            source: upstream_response_body_source(
                UpstreamResponseSource::PlainHttp1(body),
                None,
                None,
                None,
                content_length,
            ),
            stream: BufferedByteStream::default(),
        }
    }

    #[cfg(feature = "http3")]
    pub(super) fn from_http3(
        request_stream: h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
        http3_tracker: Option<http3::Http3ResponseBodyTracker>,
        content_length: Option<u64>,
    ) -> Self {
        if matches!(content_length, Some(0)) {
            return Self::empty();
        }
        Self {
            source: upstream_response_body_source(
                UpstreamResponseSource::Http3(Box::new(request_stream)),
                None,
                http3_tracker,
                None,
                content_length,
            ),
            stream: BufferedByteStream::default(),
        }
    }

    pub(super) async fn read_next_chunk(&mut self, max_bytes: usize) -> Result<Vec<u8>, VmError> {
        self.stream
            .read_next_chunk(&mut self.source, max_bytes)
            .await
    }

    pub(super) async fn read_next_line(&mut self) -> Result<Vec<u8>, VmError> {
        self.stream.read_next_line(&mut self.source).await
    }

    pub(super) async fn read_all(&mut self) -> Result<Vec<u8>, VmError> {
        self.stream.read_all(&mut self.source).await
    }

    pub(super) async fn eof(&mut self) -> Result<bool, VmError> {
        self.stream.eof(&mut self.source).await
    }

    pub(super) async fn read_trailers(&mut self) -> Result<HeaderMap, VmError> {
        let _ = self.stream.read_all(&mut self.source).await?;
        Ok(self.source.trailers.clone().unwrap_or_default())
    }

    pub(super) fn is_known_empty(&self) -> bool {
        self.stream.eof && self.stream.buffered.is_empty()
    }

    pub(super) fn take_streaming_passthrough(&mut self) -> StreamingUpstreamResponseBodyState {
        let stream = std::mem::take(&mut self.stream);
        StreamingUpstreamResponseBodyState {
            prefix: if stream.buffered.is_empty() {
                None
            } else {
                Some(Bytes::from(stream.buffered))
            },
            source: std::mem::take(&mut self.source),
            trailers_sent: false,
        }
    }
}

pub(super) type SharedUpstreamResponseBody = Arc<tokio::sync::Mutex<UpstreamResponseBodyState>>;
pub(super) type SharedHttpHeaders = Arc<HeaderMap>;
