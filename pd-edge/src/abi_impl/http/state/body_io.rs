use super::*;

pub(super) type BufferedByteSourceFuture<'a> =
    Pin<Box<dyn Future<Output = Result<BufferedByteStreamPull, VmError>> + Send + 'a>>;

pub(super) trait BufferedByteSource {
    fn pull_next<'a>(&'a mut self) -> BufferedByteSourceFuture<'a>;
}

pub(super) enum BufferedByteStreamPull {
    Chunk(Bytes),
    Skip,
    Eof,
}

#[derive(Default)]
pub(super) struct BufferedByteStream {
    pub(super) buffered: Vec<u8>,
    pub(super) read_offset: usize,
    pub(super) eof: bool,
}

impl std::fmt::Debug for BufferedByteStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferedByteStream")
            .field("buffered_len", &self.buffered.len())
            .field("read_offset", &self.read_offset)
            .field("eof", &self.eof)
            .finish()
    }
}

impl BufferedByteStream {
    fn apply_pull(&mut self, pull: BufferedByteStreamPull) {
        match pull {
            BufferedByteStreamPull::Chunk(chunk) => {
                if !chunk.is_empty() {
                    self.buffered.extend_from_slice(&chunk);
                }
            }
            BufferedByteStreamPull::Skip => {}
            BufferedByteStreamPull::Eof => {
                self.eof = true;
            }
        }
    }

    pub(super) async fn read_next_chunk<S: BufferedByteSource>(
        &mut self,
        source: &mut S,
        max_bytes: usize,
    ) -> Result<Vec<u8>, VmError> {
        self.ensure_readable_byte(source).await?;
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

    pub(super) async fn read_next_line<S: BufferedByteSource>(
        &mut self,
        source: &mut S,
    ) -> Result<Vec<u8>, VmError> {
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

            self.apply_pull(source.pull_next().await?);
        }
    }

    pub(super) async fn read_all<S: BufferedByteSource>(
        &mut self,
        source: &mut S,
    ) -> Result<Vec<u8>, VmError> {
        while !self.eof {
            self.apply_pull(source.pull_next().await?);
        }
        Ok(self.buffered.clone())
    }

    pub(super) async fn read_all_and_consume<S: BufferedByteSource>(
        &mut self,
        source: &mut S,
    ) -> Result<Vec<u8>, VmError> {
        let body = self.read_all(source).await?;
        self.read_offset = self.buffered.len();
        Ok(body)
    }

    pub(super) async fn eof<S: BufferedByteSource>(
        &mut self,
        source: &mut S,
    ) -> Result<bool, VmError> {
        while self.read_offset >= self.buffered.len() && !self.eof {
            self.apply_pull(source.pull_next().await?);
        }
        Ok(self.eof && self.read_offset >= self.buffered.len())
    }

    async fn ensure_readable_byte<S: BufferedByteSource>(
        &mut self,
        source: &mut S,
    ) -> Result<(), VmError> {
        while self.read_offset >= self.buffered.len() && !self.eof {
            self.apply_pull(source.pull_next().await?);
        }
        Ok(())
    }
}

struct InboundRequestBodySource {
    body: Option<Body>,
}

impl BufferedByteSource for InboundRequestBodySource {
    fn pull_next<'a>(&'a mut self) -> BufferedByteSourceFuture<'a> {
        Box::pin(async move {
            let Some(body) = self.body.as_mut() else {
                return Ok(BufferedByteStreamPull::Eof);
            };

            match body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(chunk) = frame.into_data() {
                        Ok(BufferedByteStreamPull::Chunk(chunk))
                    } else {
                        Ok(BufferedByteStreamPull::Skip)
                    }
                }
                Some(Err(err)) => Err(VmError::HostError(format!(
                    "failed to read inbound request body frame: {err}",
                ))),
                None => {
                    self.body = None;
                    Ok(BufferedByteStreamPull::Eof)
                }
            }
        })
    }
}

pub(crate) struct InboundRequestBodyState {
    source: InboundRequestBodySource,
    stream: BufferedByteStream,
}

impl std::fmt::Debug for InboundRequestBodyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboundRequestBodyState")
            .field("stream", &self.stream)
            .finish()
    }
}

impl InboundRequestBodyState {
    pub(super) fn new(body: Body) -> Self {
        Self {
            source: InboundRequestBodySource { body: Some(body) },
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

    pub(super) async fn read_all_and_consume(&mut self) -> Result<Vec<u8>, VmError> {
        self.stream.read_all_and_consume(&mut self.source).await
    }

    pub(super) async fn read_all(&mut self) -> Result<Vec<u8>, VmError> {
        self.stream.read_all(&mut self.source).await
    }

    pub(super) async fn eof(&mut self) -> Result<bool, VmError> {
        self.stream.eof(&mut self.source).await
    }

    pub(super) fn is_drained(&self) -> bool {
        self.stream.eof && self.stream.read_offset >= self.stream.buffered.len()
    }

    pub(super) fn is_pristine_unread(&self) -> bool {
        self.stream.buffered.is_empty() && self.stream.read_offset == 0 && !self.stream.eof
    }
}

fn finalize_downstream_body_all_result(
    context: &SharedProxyVmContext,
    result: Result<Vec<u8>, VmError>,
) -> Result<Vec<u8>, VmError> {
    context.note_downstream_request_body_read();
    match result {
        Ok(bytes) => {
            mark_downstream_transport_closed(context);
            Ok(bytes)
        }
        Err(err) => {
            let message = err.to_string();
            mark_downstream_transport_failed(context, &message);
            Err(err)
        }
    }
}

fn finalize_downstream_body_read_result(
    context: &SharedProxyVmContext,
    inbound: &InboundRequestBodyState,
    result: Result<Vec<u8>, VmError>,
) -> Result<Vec<u8>, VmError> {
    context.note_downstream_request_body_read();
    match result {
        Ok(bytes) => {
            if bytes.is_empty() || inbound.is_drained() {
                mark_downstream_transport_closed(context);
            }
            Ok(bytes)
        }
        Err(err) => {
            let message = err.to_string();
            mark_downstream_transport_failed(context, &message);
            Err(err)
        }
    }
}

fn finalize_downstream_body_eof_result(
    context: &SharedProxyVmContext,
    result: Result<bool, VmError>,
) -> Result<bool, VmError> {
    context.note_downstream_request_body_read();
    match result {
        Ok(eof) => {
            if eof {
                mark_downstream_transport_closed(context);
            }
            Ok(eof)
        }
        Err(err) => {
            let message = err.to_string();
            mark_downstream_transport_failed(context, &message);
            Err(err)
        }
    }
}

pub(crate) async fn read_request_body_all(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let mut inbound = context.inbound_request_body.lock().await;
    finalize_downstream_body_all_result(context, inbound.read_all().await)
}

pub(crate) async fn consume_request_body_all(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let mut inbound = context.inbound_request_body.lock().await;
    finalize_downstream_body_all_result(context, inbound.read_all_and_consume().await)
}

pub(crate) async fn read_request_body_next_chunk(
    context: &SharedProxyVmContext,
    max_bytes: usize,
) -> Result<Vec<u8>, VmError> {
    let mut inbound = context.inbound_request_body.lock().await;
    let result = inbound.read_next_chunk(max_bytes).await;
    finalize_downstream_body_read_result(context, &inbound, result)
}

pub(crate) async fn read_request_body_next_line(
    context: &SharedProxyVmContext,
) -> Result<Vec<u8>, VmError> {
    let mut inbound = context.inbound_request_body.lock().await;
    let result = inbound.read_next_line().await;
    finalize_downstream_body_read_result(context, &inbound, result)
}

pub(crate) async fn request_body_eof(context: &SharedProxyVmContext) -> Result<bool, VmError> {
    let mut inbound = context.inbound_request_body.lock().await;
    finalize_downstream_body_eof_result(context, inbound.eof().await)
}
