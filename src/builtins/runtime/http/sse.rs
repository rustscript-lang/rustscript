use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use pd_host_function::pd_host_function;

use super::request::{
    HttpRequest, OwnedResponse, ResponseReadObserver, open_stream_response, parse_request,
    response_header_entries,
};
use super::{HttpRequestContext, policy};
use crate::builtins::runtime::typed::VmMapHandle;
use crate::builtins::runtime::{HostCallResult, VmCallable, VmMap};
use crate::vm::{
    CallOutcome, HostStreamAction, HostStreamDriver, HostStreamPoll, Value, Vm, VmError, VmResult,
};

#[derive(Debug, PartialEq, Eq)]
struct SseEvent {
    event: Option<String>,
    data: String,
    id: Option<String>,
    retry_ms: Option<i64>,
}

/// Incremental EventSource parser. `max_total_bytes` counts raw response-body
/// octets, including a BOM and line terminators. `max_item_bytes` counts the
/// UTF-8 bytes retained in data (including inserted joins), event, and id.
struct SseParser {
    max_line_bytes: usize,
    max_item_bytes: usize,
    max_total_bytes: usize,
    total_bytes: usize,
    prefix: Vec<u8>,
    bom_decided: bool,
    line: Vec<u8>,
    after_cr: bool,
    data: String,
    has_data: bool,
    event: Option<String>,
    id: Option<String>,
    retry_ms: Option<i64>,
    finished: bool,
}

impl SseParser {
    fn new(max_line_bytes: usize, max_item_bytes: usize, max_total_bytes: usize) -> Self {
        Self {
            max_line_bytes,
            max_item_bytes,
            max_total_bytes,
            total_bytes: 0,
            prefix: Vec::with_capacity(3),
            bom_decided: false,
            line: Vec::with_capacity(max_line_bytes.min(1024)),
            after_cr: false,
            data: String::new(),
            has_data: false,
            event: None,
            id: None,
            retry_ms: None,
            finished: false,
        }
    }

    #[cfg(test)]
    fn push(&mut self, bytes: &[u8]) -> VmResult<Vec<SseEvent>> {
        self.admit_chunk(bytes.len())?;
        let mut events = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let (consumed, event) = self.push_until_event(&bytes[offset..])?;
            offset += consumed;
            if let Some(event) = event {
                events.push(event);
            }
        }
        Ok(events)
    }

    fn admit_chunk(&mut self, bytes: usize) -> VmResult<()> {
        self.total_bytes = self
            .total_bytes
            .checked_add(bytes)
            .filter(|total| *total <= self.max_total_bytes)
            .ok_or_else(|| VmError::HostError("SSE stream exceeds total byte limit".to_string()))?;
        Ok(())
    }

    fn push_until_event(&mut self, bytes: &[u8]) -> VmResult<(usize, Option<SseEvent>)> {
        if self.finished {
            return Err(VmError::HostError(
                "SSE parser received bytes after EOF".to_string(),
            ));
        }
        let mut consumed = 0;
        while consumed < bytes.len() {
            let byte = bytes[consumed];
            consumed += 1;
            if !self.bom_decided {
                self.prefix.push(byte);
                if self.prefix == b"\xef\xbb\xbf" {
                    self.prefix.clear();
                    self.bom_decided = true;
                    continue;
                }
                if b"\xef\xbb\xbf".starts_with(&self.prefix) {
                    continue;
                }
                let prefix = std::mem::take(&mut self.prefix);
                self.bom_decided = true;
                for byte in prefix {
                    if let Some(event) = self.process_byte(byte)? {
                        return Ok((consumed, Some(event)));
                    }
                }
                continue;
            }
            if let Some(event) = self.process_byte(byte)? {
                return Ok((consumed, Some(event)));
            }
        }
        Ok((consumed, None))
    }

    fn finish(&mut self) -> VmResult<Vec<SseEvent>> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let mut events = Vec::new();
        if !self.prefix.is_empty() {
            let prefix = std::mem::take(&mut self.prefix);
            for byte in prefix {
                if let Some(event) = self.process_byte(byte)? {
                    events.push(event);
                }
            }
        }
        if !self.line.is_empty()
            && let Some(event) = self.process_line()?
        {
            events.push(event);
        }
        // EventSource dispatches only on a blank line. EOF discards a partial
        // event, including a final unterminated data line.
        self.data.clear();
        self.has_data = false;
        self.event = None;
        Ok(events)
    }

    fn process_byte(&mut self, byte: u8) -> VmResult<Option<SseEvent>> {
        if self.after_cr {
            self.after_cr = false;
            if byte == b'\n' {
                return Ok(None);
            }
        }
        match byte {
            b'\r' => {
                let event = self.process_line()?;
                self.after_cr = true;
                Ok(event)
            }
            b'\n' => self.process_line(),
            _ => {
                if self.line.len() == self.max_line_bytes {
                    return Err(VmError::HostError(
                        "SSE line exceeds byte limit".to_string(),
                    ));
                }
                self.line.push(byte);
                Ok(None)
            }
        }
    }

    fn process_line(&mut self) -> VmResult<Option<SseEvent>> {
        let bytes = std::mem::take(&mut self.line);
        let line = std::str::from_utf8(&bytes)
            .map_err(|_| VmError::HostError("SSE stream contains malformed UTF-8".to_string()))?;
        if line.is_empty() {
            if self.data_seen() {
                return Ok(Some(self.dispatch_event()));
            }
            self.event = None;
            return Ok(None);
        }
        if line.starts_with(':') {
            return Ok(None);
        }
        let (field, mut value) = line.split_once(':').unwrap_or((line, ""));
        if let Some(rest) = value.strip_prefix(' ') {
            value = rest;
        }
        match field {
            "data" => {
                let added = value.len() + usize::from(self.has_data);
                self.ensure_item_growth(added, self.event.as_deref(), self.id.as_deref())?;
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "event" => {
                self.ensure_item_size(self.data.len(), Some(value), self.id.as_deref())?;
                self.event = Some(value.to_string());
            }
            "id" if !value.contains('\0') => {
                self.ensure_item_size(self.data.len(), self.event.as_deref(), Some(value))?;
                self.id = Some(value.to_string());
            }
            "retry" if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
                if let Ok(retry) = value.parse::<i64>() {
                    self.retry_ms = Some(retry);
                }
            }
            _ => {}
        }
        Ok(None)
    }

    fn data_seen(&self) -> bool {
        self.has_data
    }

    fn ensure_item_growth(
        &self,
        added: usize,
        event: Option<&str>,
        id: Option<&str>,
    ) -> VmResult<()> {
        let data = self
            .data
            .len()
            .checked_add(added)
            .ok_or_else(item_limit_error)?;
        self.ensure_item_size(data, event, id)
    }

    fn ensure_item_size(
        &self,
        data_bytes: usize,
        event: Option<&str>,
        id: Option<&str>,
    ) -> VmResult<()> {
        let size = data_bytes
            .checked_add(event.map_or(0, str::len))
            .and_then(|size| size.checked_add(id.map_or(0, str::len)))
            .ok_or_else(item_limit_error)?;
        if size > self.max_item_bytes {
            return Err(item_limit_error());
        }
        Ok(())
    }

    fn dispatch_event(&mut self) -> SseEvent {
        let data = std::mem::take(&mut self.data);
        self.has_data = false;
        SseEvent {
            event: self.event.take(),
            data,
            id: self.id.clone(),
            retry_ms: self.retry_ms,
        }
    }
}

fn item_limit_error() -> VmError {
    VmError::HostError("SSE item exceeds byte limit".to_string())
}

type OpenFuture = Pin<Box<dyn Future<Output = VmResult<(OwnedResponse, url::Url)>> + Send>>;
type FrameFuture = Pin<
    Box<
        dyn Future<
                Output = (
                    OwnedResponse,
                    VmResult<Option<hyper::body::Frame<hyper::body::Bytes>>>,
                ),
            > + Send,
    >,
>;

enum DriverState {
    Opening {
        future: OpenFuture,
        idle_deadline: Instant,
        timeout: Option<Pin<Box<tokio::time::Sleep>>>,
    },
    Reading {
        future: FrameFuture,
        idle_deadline: Instant,
        timeout: Pin<Box<tokio::time::Sleep>>,
    },
    Ready(OwnedResponse),
    Closed,
}

struct SseDriver {
    state: DriverState,
    parser: SseParser,
    chunk: Option<hyper::body::Bytes>,
    chunk_offset: usize,
    eof_pending: bool,
    config: super::HttpConfig,
    observer: ResponseReadObserver,
    permit: Option<super::ConnectionPermit>,
    deadline: Instant,
    status: Option<hyper::StatusCode>,
    headers: Option<std::sync::Arc<VmMap>>,
    url: Option<url::Url>,
    items: i64,
    bytes_received: i64,
}

impl Drop for SseDriver {
    fn drop(&mut self) {
        self.retire();
    }
}

impl SseDriver {
    fn new(context: HttpRequestContext, request: HttpRequest, deadline: Instant) -> Self {
        let super::HttpRequestContext { config, _permit } = context;
        let observer = ResponseReadObserver::default();
        let open_config = config.clone();
        let open_observer = observer.clone();
        let future = Box::pin(async move {
            open_stream_response(&open_config, &request, open_observer, Some(deadline)).await
        });
        let idle_deadline = Instant::now()
            .checked_add(config.stream_idle_timeout)
            .expect("validated idle timeout");
        Self {
            state: DriverState::Opening {
                future,
                idle_deadline,
                timeout: None,
            },
            parser: SseParser::new(
                config.max_sse_line_bytes,
                config.max_stream_item_bytes,
                config.max_stream_total_bytes,
            ),
            chunk: None,
            chunk_offset: 0,
            eof_pending: false,
            config,
            observer,
            permit: Some(_permit),
            deadline,
            status: None,
            headers: None,
            url: None,
            items: 0,
            bytes_received: 0,
        }
    }

    fn validate_response(&mut self, response: &OwnedResponse, url: url::Url) -> VmResult<Value> {
        let status = response.response().status();
        if !status.is_success() {
            return Err(VmError::HostError(format!(
                "SSE response status {} is not successful",
                status.as_u16()
            )));
        }
        let content_type = response
            .response()
            .headers()
            .get(hyper::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .filter(|value| value.eq_ignore_ascii_case("text/event-stream"))
            .ok_or_else(|| {
                VmError::HostError(
                    "SSE response Content-Type must be text/event-stream".to_string(),
                )
            })?;
        debug_assert!(content_type.eq_ignore_ascii_case("text/event-stream"));
        let headers = std::sync::Arc::new(VmMap::from_entries(response_header_entries(
            response.response().headers(),
        )));
        self.status = Some(status);
        self.headers = Some(std::sync::Arc::clone(&headers));
        self.url = Some(url.clone());
        self.observer.admit_body(self.config.max_stream_total_bytes);
        Ok(map_value(vec![
            ("kind", Value::string("open")),
            ("status", Value::Int(i64::from(status.as_u16()))),
            ("headers", Value::Map(headers)),
            ("url", Value::string(url.as_str())),
        ]))
    }

    fn event_value(event: SseEvent) -> Value {
        map_value(vec![
            ("kind", Value::string("event")),
            ("event", event.event.map_or(Value::Null, Value::string)),
            ("data", Value::string(event.data)),
            ("id", event.id.map_or(Value::Null, Value::string)),
            ("retry_ms", event.retry_ms.map_or(Value::Null, Value::Int)),
        ])
    }

    fn summary(&self, outcome: &str) -> Value {
        map_value(vec![
            ("outcome", Value::string(outcome)),
            (
                "status",
                Value::Int(i64::from(
                    self.status.expect("summary requires open status").as_u16(),
                )),
            ),
            (
                "headers",
                Value::Map(
                    self.headers
                        .as_ref()
                        .expect("summary requires headers")
                        .clone(),
                ),
            ),
            (
                "url",
                Value::string(self.url.as_ref().expect("summary requires URL").as_str()),
            ),
            ("items", Value::Int(self.items)),
            ("bytes_received", Value::Int(self.bytes_received)),
            ("bytes_sent", Value::Int(0)),
        ])
    }

    fn retire(&mut self) {
        self.state = DriverState::Closed;
        self.chunk = None;
        self.eof_pending = false;
        self.permit.take();
    }

    fn ensure_before_deadline(&mut self) -> VmResult<()> {
        if Instant::now() >= self.deadline {
            self.retire();
            return Err(VmError::HostError(
                "SSE total deadline exceeded".to_string(),
            ));
        }
        Ok(())
    }
}

impl HostStreamDriver for SseDriver {
    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<HostStreamPoll>> {
        loop {
            if let Err(error) = self.ensure_before_deadline() {
                return Poll::Ready(Err(error));
            }
            if let Some(chunk) = self.chunk.as_ref() {
                let (consumed, event) =
                    self.parser.push_until_event(&chunk[self.chunk_offset..])?;
                self.chunk_offset += consumed;
                if self.chunk_offset == chunk.len() {
                    self.chunk = None;
                    self.chunk_offset = 0;
                }
                if let Some(event) = event {
                    return Poll::Ready(Ok(HostStreamPoll::Item(Self::event_value(event))));
                }
            }
            if self.eof_pending {
                if let Some(event) = self.parser.finish()?.into_iter().next() {
                    return Poll::Ready(Ok(HostStreamPoll::Item(Self::event_value(event))));
                }
                self.eof_pending = false;
                self.state = DriverState::Closed;
                return Poll::Ready(Ok(HostStreamPoll::Item(map_value(vec![(
                    "kind",
                    Value::string("end"),
                )]))));
            }
            match &mut self.state {
                DriverState::Opening {
                    future,
                    idle_deadline,
                    timeout,
                } => {
                    let open_deadline = self.deadline.min(*idle_deadline);
                    let timeout = timeout.get_or_insert_with(|| {
                        Box::pin(tokio::time::sleep_until(tokio::time::Instant::from_std(
                            open_deadline,
                        )))
                    });
                    if timeout.as_mut().poll(cx).is_ready() {
                        let total_expired = self.deadline <= *idle_deadline;
                        self.retire();
                        return Poll::Ready(Err(VmError::HostError(
                            if total_expired {
                                "SSE total deadline exceeded"
                            } else {
                                "SSE stream idle timeout while opening response"
                            }
                            .to_string(),
                        )));
                    }
                    match future.as_mut().poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(error)) => {
                            self.retire();
                            return Poll::Ready(Err(error));
                        }
                        Poll::Ready(Ok((response, url))) => {
                            let open = match self.validate_response(&response, url) {
                                Ok(open) => open,
                                Err(error) => {
                                    self.retire();
                                    return Poll::Ready(Err(error));
                                }
                            };
                            self.state = DriverState::Ready(response);
                            return Poll::Ready(Ok(HostStreamPoll::Item(open)));
                        }
                    }
                }
                DriverState::Ready(_) => {
                    let DriverState::Ready(mut response) =
                        std::mem::replace(&mut self.state, DriverState::Closed)
                    else {
                        unreachable!()
                    };
                    let idle_deadline = Instant::now()
                        .checked_add(self.config.stream_idle_timeout)
                        .expect("validated idle timeout");
                    let deadline = self.deadline.min(idle_deadline);
                    self.state = DriverState::Reading {
                        future: Box::pin(async move {
                            let frame = response.next_frame().await;
                            (response, frame)
                        }),
                        idle_deadline,
                        timeout: Box::pin(tokio::time::sleep_until(
                            tokio::time::Instant::from_std(deadline),
                        )),
                    };
                }
                DriverState::Reading {
                    future,
                    idle_deadline,
                    timeout,
                } => {
                    if timeout.as_mut().poll(cx).is_ready() {
                        let total_expired = self.deadline <= *idle_deadline;
                        self.retire();
                        return Poll::Ready(Err(VmError::HostError(
                            if total_expired {
                                "SSE total deadline exceeded"
                            } else {
                                "SSE stream idle timeout"
                            }
                            .to_string(),
                        )));
                    }
                    match future.as_mut().poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready((response, Err(error))) => {
                            drop(response);
                            self.retire();
                            return Poll::Ready(Err(error));
                        }
                        Poll::Ready((response, Ok(Some(frame)))) => {
                            self.state = DriverState::Ready(response);
                            if let Ok(data) = frame.into_data() {
                                self.parser.admit_chunk(data.len())?;
                                self.observer.observe_application_chunk(data.len());
                                self.bytes_received = self
                                    .bytes_received
                                    .checked_add(i64::try_from(data.len()).map_err(|_| {
                                        VmError::HostError(
                                            "SSE byte count exceeds script int".into(),
                                        )
                                    })?)
                                    .ok_or_else(|| {
                                        VmError::HostError(
                                            "SSE byte count exceeds script int".into(),
                                        )
                                    })?;
                                self.chunk = Some(data);
                                self.chunk_offset = 0;
                            }
                        }
                        Poll::Ready((response, Ok(None))) => {
                            drop(response);
                            self.eof_pending = true;
                        }
                    }
                }
                DriverState::Closed => {
                    self.permit.take();
                    return Poll::Ready(Ok(HostStreamPoll::Complete(self.summary("eof"))));
                }
            }
        }
    }

    fn apply_action(&mut self, action: Value) -> VmResult<HostStreamAction> {
        self.ensure_before_deadline()?;
        let Value::Map(action) = action else {
            self.retire();
            return Err(VmError::HostError(
                "SSE callback action must be a map".to_string(),
            ));
        };
        let Some(Value::String(action)) = action.get(&Value::string("action")) else {
            self.retire();
            return Err(VmError::HostError(
                "SSE callback action must contain string 'action'".to_string(),
            ));
        };
        self.items = self
            .items
            .checked_add(1)
            .ok_or_else(|| VmError::HostError("SSE item count exceeds script int".to_string()))?;
        match action.as_str() {
            "continue" => Ok(HostStreamAction::Continue),
            "stop" => {
                let summary = self.summary("stopped");
                self.retire();
                Ok(HostStreamAction::Complete(summary))
            }
            other => {
                let error = VmError::HostError(format!("invalid SSE callback action '{other}'"));
                self.retire();
                Err(error)
            }
        }
    }
}

fn map_value(entries: Vec<(&'static str, Value)>) -> Value {
    Value::Map(std::sync::Arc::new(VmMap::from_entries(
        entries
            .into_iter()
            .map(|(key, value)| (Value::string(key), value))
            .collect(),
    )))
}

fn parse_stream_timeout(request: &VmMap) -> VmResult<Option<Duration>> {
    let Some(value) = request.get(&Value::string("timeout_ms")) else {
        return Ok(None);
    };
    let Value::Int(milliseconds) = value else {
        return Err(VmError::TypeMismatch("SSE timeout_ms"));
    };
    let milliseconds = u64::try_from(*milliseconds)
        .ok()
        .filter(|milliseconds| *milliseconds > 0)
        .ok_or_else(|| VmError::HostError("SSE timeout_ms must be positive".to_string()))?;
    Ok(Some(Duration::from_millis(milliseconds)))
}

/// Streams one bounded SSE item into one script callback at a time.
#[pd_host_function(name = "http::client::sse")]
pub(super) fn builtin_http_client_sse_impl(
    vm: &mut Vm,
    request: VmMapHandle,
    on_event: VmCallable<fn(VmMap) -> VmMap>,
) -> VmResult<HostCallResult<VmMap>> {
    let callback = on_event.into_value();
    vm.validate_stream_callback_value(&callback)?;
    let script_timeout = parse_stream_timeout(request.as_ref())?;
    let (context, deadline) = HttpRequestContext::capture_stream(vm, script_timeout)?;
    let mut request = parse_request(request.as_ref(), &context.config)?;
    policy::validate_url_policy(&context.config, policy::SchemeFamily::Http, &request.url)?;
    if request.method != hyper::Method::GET && request.method != hyper::Method::POST {
        return Err(VmError::HostError(
            "SSE requests require GET or POST".to_string(),
        ));
    }
    if !request
        .headers
        .iter()
        .any(|(name, _)| name == hyper::header::ACCEPT)
    {
        request.headers.push((
            hyper::header::ACCEPT,
            hyper::header::HeaderValue::from_static("text/event-stream"),
        ));
    }
    match vm.submit_callable_stream(callback, SseDriver::new(context, request, deadline))? {
        CallOutcome::Pending(op_id) => Ok(HostCallResult::Pending(op_id)),
        _ => Err(VmError::InvalidFrameState(
            "callable stream admission returned a non-pending outcome",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{SseEvent, SseParser};

    fn event(data: &str, event: Option<&str>, id: Option<&str>, retry_ms: Option<i64>) -> SseEvent {
        SseEvent {
            event: event.map(str::to_string),
            data: data.to_string(),
            id: id.map(str::to_string),
            retry_ms,
        }
    }

    fn parse_fragments(
        fragments: &[&[u8]],
        line: usize,
        item: usize,
        total: usize,
    ) -> Result<Vec<SseEvent>, String> {
        let mut parser = SseParser::new(line, item, total);
        let mut events = Vec::new();
        for fragment in fragments {
            events.extend(parser.push(fragment).map_err(|error| error.to_string())?);
        }
        events.extend(parser.finish().map_err(|error| error.to_string())?);
        Ok(events)
    }

    #[test]
    fn parser_accepts_fragmented_bom_utf8_and_every_line_ending() {
        let fragments: &[&[u8]] = &[
            b"\xef",
            b"\xbb\xbfdata: h\xc3",
            b"\xa9\r",
            b"data: two\n",
            b"event:first\r\nevent: final\r",
            b"id: 7\nretry: 25\n\n",
        ];
        assert_eq!(
            parse_fragments(fragments, 64, 128, 256).unwrap(),
            vec![event("hé\ntwo", Some("final"), Some("7"), Some(25))]
        );
    }

    #[test]
    fn parser_uses_first_colon_removes_one_space_and_ignores_comments_unknown_fields() {
        let input = b": comment\ndata:a:b\ndata:  two\ndata: \nunknown: value\n\n";
        assert_eq!(
            parse_fragments(&[input], 64, 128, 256).unwrap(),
            vec![event("a:b\n two\n", None, None, None)]
        );
    }

    #[test]
    fn parser_handles_empty_fields_id_nul_and_retry_rules() {
        let input = b"id: keep\nretry: 42\ndata: one\n\nretry: 99\n\nid:\nid: bad\0id\nretry: -1\nretry: 4x\nretry: 9223372036854775808\nevent:\ndata: two\n\n";
        assert_eq!(
            parse_fragments(&[input], 64, 128, 512).unwrap(),
            vec![
                event("one", None, Some("keep"), Some(42)),
                event("two", Some(""), Some(""), Some(99)),
            ]
        );
    }

    #[test]
    fn parser_persists_retry_state_across_empty_blocks_events_and_invalid_values() {
        let input = b"retry:5000\n\ndata:ready\n\ndata:next\n\nretry:\nretry: -1\nretry: 5x\nretry: 9223372036854775808\n\ndata:still\n\n";
        assert_eq!(
            parse_fragments(&[input], 64, 128, 512).unwrap(),
            vec![
                event("ready", None, None, Some(5000)),
                event("next", None, None, Some(5000)),
                event("still", None, None, Some(5000)),
            ]
        );
    }

    #[test]
    fn parser_discards_incomplete_event_at_eof_and_ignores_field_only_blocks() {
        assert!(
            parse_fragments(&[b"event: named\nid: x\n\ndata: tail"], 64, 128, 256)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            parse_fragments(&[b"id: x\n\ndata: complete\n\n"], 64, 128, 256).unwrap(),
            vec![event("complete", None, Some("x"), None)]
        );
        assert!(
            parse_fragments(&[b"event: unused"], 64, 128, 256)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn parser_rejects_malformed_and_incomplete_utf8() {
        for input in [b"data: \xff\n\n".as_slice(), b"data: \xc3".as_slice()] {
            assert!(
                parse_fragments(&[input], 64, 128, 256)
                    .unwrap_err()
                    .contains("UTF-8")
            );
        }
    }

    #[test]
    fn parser_enforces_exact_line_item_and_total_boundaries() {
        assert_eq!(
            parse_fragments(&[b"data: ab\n\n"], 8, 2, 10).unwrap(),
            vec![event("ab", None, None, None)]
        );
        assert!(
            parse_fragments(&[b"data: abc\n\n"], 8, 3, 12)
                .unwrap_err()
                .contains("line")
        );
        assert!(
            parse_fragments(&[b"data: ab\ndata: c\n\n"], 16, 3, 64)
                .unwrap_err()
                .contains("item")
        );
        assert!(
            parse_fragments(&[b"data: ab\n\n"], 8, 2, 9)
                .unwrap_err()
                .contains("total")
        );
    }

    #[test]
    fn parser_rejects_a_single_fragment_before_unbounded_growth() {
        let mut parser = SseParser::new(4, 16, 64);
        assert!(parser.push(b"data: a very large fragment").is_err());
    }

    #[test]
    fn parser_only_strips_a_bom_at_the_start_of_the_stream() {
        assert_eq!(
            parse_fragments(
                &[b"data: first\n\ndata: \xef\xbb\xbfsecond\n\n"],
                64,
                128,
                256
            )
            .unwrap(),
            vec![
                event("first", None, None, None),
                event("\u{feff}second", None, None, None),
            ]
        );
    }
}
