use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use hyper::body::Bytes;
use pd_host_function::pd_host_function;

use super::request::{
    HttpRequest, OwnedResponse, open_stream_response, parse_request, response_header_entries,
    validate_request_header_budget,
};
use super::{
    CaptureAsyncHostContext, HostFutureOutput, HttpClientLease, HttpRequestContext, policy,
};
use crate::builtins::runtime::typed::{VmCallable, VmMap, VmMapHandle};
use crate::vm::async_host::{HostStreamAction, HostStreamDriver, HostStreamPoll};
use crate::vm::operation::OperationCancelReason;
use crate::vm::{CallOutcome, Value, Vm, VmError, VmResult};

/// The error surfaced when the absolute stream deadline is exceeded.
const SSE_TOTAL_DEADLINE_ERROR: &str = "SSE total deadline exceeded";

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

    fn finish(&mut self) -> VmResult<Option<SseEvent>> {
        if self.finished {
            return Ok(None);
        }
        self.finished = true;
        let mut event = None;
        if !self.prefix.is_empty() {
            let prefix = std::mem::take(&mut self.prefix);
            for byte in prefix {
                if let Some(next) = self.process_byte(byte)? {
                    event = Some(next);
                }
            }
        }
        if !self.line.is_empty()
            && let Some(next) = self.process_line()?
        {
            event = Some(next);
        }
        // EventSource dispatches only on a blank line. EOF discards a partial
        // event, including a final unterminated data line.
        self.data.clear();
        self.has_data = false;
        self.event = None;
        Ok(event)
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
            // The WHATWG dispatch algorithm clears both data and event type
            // buffers even when empty data causes dispatch to return early.
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

fn map_value(entries: Vec<(&'static str, Value)>) -> Value {
    Value::Map(std::sync::Arc::new(VmMap::from_entries(
        entries
            .into_iter()
            .map(|(key, value)| (Value::string(key), value))
            .collect(),
    )))
}

fn sse_open_event(status: u16, headers: Arc<Vec<Value>>, url: &str) -> Value {
    map_value(vec![
        ("kind", Value::string("open")),
        ("status", Value::Int(i64::from(status))),
        ("headers", Value::Array(headers)),
        ("url", Value::string(url)),
        ("event", Value::Null),
        ("data", Value::Null),
        ("id", Value::Null),
        ("retry_ms", Value::Null),
    ])
}

fn sse_data_event(event: SseEvent) -> Value {
    map_value(vec![
        ("kind", Value::string("event")),
        ("status", Value::Null),
        ("headers", Value::Null),
        ("url", Value::Null),
        ("event", event.event.map_or(Value::Null, Value::string)),
        ("data", Value::string(event.data)),
        ("id", event.id.map_or(Value::Null, Value::string)),
        ("retry_ms", event.retry_ms.map_or(Value::Null, Value::Int)),
    ])
}

fn sse_end_event() -> Value {
    map_value(vec![
        ("kind", Value::string("end")),
        ("status", Value::Null),
        ("headers", Value::Null),
        ("url", Value::Null),
        ("event", Value::Null),
        ("data", Value::Null),
        ("id", Value::Null),
        ("retry_ms", Value::Null),
    ])
}

fn parse_stream_timeout(request: &VmMap) -> VmResult<Option<Duration>> {
    match request.get(&Value::string("timeout_ms")) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Int(milliseconds)) => {
            let milliseconds = u64::try_from(*milliseconds)
                .ok()
                .filter(|milliseconds| *milliseconds > 0)
                .ok_or_else(|| VmError::HostError("SSE timeout_ms must be positive".to_string()))?;
            Ok(Some(Duration::from_millis(milliseconds)))
        }
        Some(_) => Err(VmError::TypeMismatch("SSE timeout_ms")),
    }
}

fn prepare_sse_request(request: &VmMap, config: &super::HttpConfig) -> VmResult<HttpRequest> {
    let mut request = parse_request(request, config)?;
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
    validate_request_header_budget(&request.headers, config)?;
    Ok(request)
}

/// Per-call state captured before the macro submits the async SSE future.
pub(super) struct SseRequestContext {
    http: HttpRequestContext,
    deadline: Instant,
    request: HttpRequest,
}

impl CaptureAsyncHostContext for SseRequestContext {
    fn capture(_vm: &mut Vm) -> VmResult<Self> {
        Err(VmError::HostError(
            "SSE context requires call arguments".to_string(),
        ))
    }

    fn capture_with_args(vm: &mut Vm, args: &[Value]) -> VmResult<Self> {
        let request = match args.first() {
            Some(Value::Map(request)) => request,
            Some(_) => return Err(VmError::TypeMismatch("SSE request")),
            None => return Err(VmError::HostError("missing SSE request".to_string())),
        };
        let callback = args
            .get(1)
            .ok_or_else(|| VmError::HostError("missing SSE callback".to_string()))?;
        vm.validate_sse_callback_value(callback)?;
        let script_timeout = parse_stream_timeout(request)?;
        let (http, deadline) = HttpRequestContext::capture_for(vm, script_timeout, "SSE")?;
        let request = prepare_sse_request(request, &http.config)?;
        Ok(Self {
            http,
            deadline,
            request,
        })
    }
}

struct RetainedSseFrame {
    data: Bytes,
    offset: usize,
}

impl RetainedSseFrame {
    fn new(data: Bytes) -> Self {
        Self { data, offset: 0 }
    }

    fn next_event(&mut self, parser: &mut SseParser) -> VmResult<Option<SseEvent>> {
        let (consumed, event) = parser.push_until_event(&self.data[self.offset..])?;
        self.offset += consumed;
        Ok(event)
    }

    fn is_consumed(&self) -> bool {
        self.offset == self.data.len()
    }
}

/// Generic callable-stream continuation that owns only the Hyper response,
/// parser, one retained body frame, deadlines, and the in-flight permit.
struct SseStreamDriver {
    response: OwnedResponse,
    parser: SseParser,
    open_item: Option<Value>,
    retained_frame: Option<RetainedSseFrame>,
    eof_event: Option<Value>,
    status: u16,
    headers: Arc<Vec<Value>>,
    url: String,
    items: usize,
    bytes_received: usize,
    deadline: Instant,
    total_sleep: Pin<Box<tokio::time::Sleep>>,
    idle_timeout: Duration,
    idle_sleep: Pin<Box<tokio::time::Sleep>>,
    body_started: bool,
    eof: bool,
    end_emitted: bool,
    /// Keeps the embedding-owned worker resource alive for the full response
    /// and every generic stream termination path.
    _client: HttpClientLease,
    _permit: super::policy::ConnectionPermit,
}

impl SseStreamDriver {
    fn new(
        response: OwnedResponse,
        url: url::Url,
        config: &super::HttpConfig,
        deadline: Instant,
        client: HttpClientLease,
        permit: super::policy::ConnectionPermit,
    ) -> VmResult<Self> {
        let status = response.response().status();
        if !status.is_success() {
            return Err(VmError::HostError(format!(
                "SSE response status {} is not successful",
                status.as_u16()
            )));
        }
        response
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
        let headers = Arc::new(response_header_entries(response.response().headers()));
        let open_item = sse_open_event(status.as_u16(), Arc::clone(&headers), url.as_str());
        let total_at = tokio::time::Instant::from_std(deadline);
        Ok(Self {
            response,
            parser: SseParser::new(
                config.max_sse_line_bytes,
                config.max_stream_item_bytes,
                config.max_stream_total_bytes,
            ),
            open_item: Some(open_item),
            retained_frame: None,
            eof_event: None,
            status: status.as_u16(),
            headers,
            url: url.to_string(),
            items: 0,
            bytes_received: 0,
            deadline,
            total_sleep: Box::pin(tokio::time::sleep_until(total_at)),
            idle_timeout: config.stream_idle_timeout,
            // Opening callback time is excluded from the first body idle window.
            idle_sleep: Box::pin(tokio::time::sleep_until(total_at)),
            body_started: false,
            eof: false,
            end_emitted: false,
            _client: client,
            _permit: permit,
        })
    }

    fn summary(&self, outcome: &str) -> Value {
        map_value(vec![
            ("outcome", Value::string(outcome)),
            ("status", Value::Int(i64::from(self.status))),
            ("headers", Value::Array(Arc::clone(&self.headers))),
            ("url", Value::string(&self.url)),
            ("items", Value::Int(self.items as i64)),
            ("bytes_received", Value::Int(self.bytes_received as i64)),
            ("bytes_sent", Value::Int(0)),
        ])
    }

    fn reset_idle_deadline(&mut self) {
        let idle_at = policy::phase_deadline(self.deadline, self.idle_timeout);
        self.idle_sleep
            .as_mut()
            .reset(tokio::time::Instant::from_std(idle_at));
    }

    fn item(&mut self, item: Value) -> HostStreamPoll {
        self.items = self.items.saturating_add(1);
        HostStreamPoll::Item(item)
    }

    fn poll_retained_frame(&mut self) -> VmResult<Option<HostStreamPoll>> {
        let Some(frame) = self.retained_frame.as_mut() else {
            return Ok(None);
        };
        let event = frame.next_event(&mut self.parser)?;
        if frame.is_consumed() {
            self.retained_frame = None;
        }
        Ok(event.map(|event| self.item(sse_data_event(event))))
    }

    fn poll_eof_item(&mut self) -> Option<HostStreamPoll> {
        if let Some(event) = self.eof_event.take() {
            return Some(self.item(event));
        }
        if !self.end_emitted {
            self.end_emitted = true;
            return Some(self.item(sse_end_event()));
        }
        None
    }
}

impl HostStreamDriver for SseStreamDriver {
    fn acknowledge_item(&mut self) {
        if !self.body_started {
            self.body_started = true;
            self.reset_idle_deadline();
        }
    }

    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<HostStreamPoll>> {
        if self.total_sleep.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(VmError::HostError(
                SSE_TOTAL_DEADLINE_ERROR.to_string(),
            )));
        }
        if let Some(item) = self.open_item.take() {
            return Poll::Ready(Ok(self.item(item)));
        }
        match self.poll_retained_frame() {
            Ok(Some(item)) => return Poll::Ready(Ok(item)),
            Err(error) => return Poll::Ready(Err(error)),
            Ok(None) => {}
        }
        if self.eof {
            if let Some(item) = self.poll_eof_item() {
                return Poll::Ready(Ok(item));
            }
            return Poll::Ready(Ok(HostStreamPoll::Complete(self.summary("eof"))));
        }
        if self.body_started && self.idle_sleep.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(VmError::HostError(
                "SSE stream idle timeout".to_string(),
            )));
        }

        match self.response.poll_next_frame(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(Some(frame))) => {
                let Ok(data) = frame.into_data() else {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                };
                self.reset_idle_deadline();
                if let Err(error) = self.parser.admit_chunk(data.len()) {
                    return Poll::Ready(Err(error));
                }
                self.bytes_received = self.bytes_received.saturating_add(data.len());
                self.retained_frame = Some(RetainedSseFrame::new(data));
                match self.poll_retained_frame() {
                    Ok(Some(item)) => Poll::Ready(Ok(item)),
                    Ok(None) => {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                    Err(error) => Poll::Ready(Err(error)),
                }
            }
            Poll::Ready(Ok(None)) => {
                let event = match self.parser.finish() {
                    Ok(event) => event,
                    Err(error) => return Poll::Ready(Err(error)),
                };
                self.eof = true;
                self.eof_event = event.map(sse_data_event);
                Poll::Ready(Ok(self
                    .poll_eof_item()
                    .expect("EOF always produces a pending event or end item")))
            }
        }
    }

    fn apply_action(&mut self, action: Value) -> VmResult<HostStreamAction> {
        if Instant::now() >= self.deadline {
            return Err(VmError::HostError(SSE_TOTAL_DEADLINE_ERROR.to_string()));
        }
        let Value::Map(action) = action else {
            return Err(VmError::HostError(
                "SSE callback action must be a map".to_string(),
            ));
        };
        let Some(Value::String(action)) = action.get(&Value::string("action")) else {
            return Err(VmError::HostError(
                "SSE callback action must contain string 'action'".to_string(),
            ));
        };
        match action.as_str() {
            "continue" => Ok(HostStreamAction::Continue),
            "stop" => Ok(HostStreamAction::Cancel(
                self.summary("stopped"),
                OperationCancelReason::Requested,
            )),
            other => Err(VmError::HostError(format!(
                "invalid SSE callback action '{other}'"
            ))),
        }
    }
}

async fn open_sse_response(
    context: &SseRequestContext,
    request: &HttpRequest,
) -> VmResult<(OwnedResponse, url::Url)> {
    let opening_idle_deadline =
        policy::phase_deadline(context.deadline, context.http.config.stream_idle_timeout);
    tokio::select! {
        biased;
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(opening_idle_deadline)) => {
            if context.deadline <= opening_idle_deadline {
                Err(VmError::HostError(SSE_TOTAL_DEADLINE_ERROR.to_string()))
            } else {
                Err(VmError::HostError(
                    "SSE stream idle timeout while opening response".to_string(),
                ))
            }
        }
        opened = open_stream_response(
            context.http.client.client(),
            &context.http.config,
            request,
            context.deadline,
        ) => opened.map_err(|error| {
            if error.to_string().contains("HTTP request deadline exceeded") {
                let now = Instant::now();
                if now >= context.deadline {
                    VmError::HostError(SSE_TOTAL_DEADLINE_ERROR.to_string())
                } else if now >= opening_idle_deadline {
                    VmError::HostError(
                        "SSE stream idle timeout while opening response".to_string(),
                    )
                } else {
                    error
                }
            } else {
                error
            }
        })
    }
}

/// Opens an SSE response with the shared client, then transfers the response
/// body into the VM's generic callable-stream continuation.
#[pd_host_function(name = "http::client::sse", contract = super::http_sse_contract)]
pub(super) async fn builtin_http_client_sse(
    #[pd_host_context] context: SseRequestContext,
    request: VmMapHandle,
    on_event: VmCallable<fn(VmMap) -> VmMap>,
) -> VmResult<HostFutureOutput<VmMap>> {
    let callback = on_event.into_value();
    let _ = request;
    let (response, url) = open_sse_response(&context, &context.request).await?;
    let SseRequestContext {
        http,
        deadline,
        request: _,
    } = context;
    let driver = SseStreamDriver::new(
        response,
        url,
        &http.config,
        deadline,
        http.client.clone(),
        http.permit,
    )?;
    Ok(HostFutureOutput::continue_with(move |vm| {
        match vm.submit_callable_stream(callback, driver) {
            Ok(CallOutcome::Pending(op_id)) => Ok(CallOutcome::Pending(op_id)),
            Ok(_) => Err(VmError::InvalidFrameState(
                "callable stream admission returned a non-pending outcome",
            )),
            Err(rejection) => Err(vm.rollback_rejected_callable_stream(rejection)),
        }
    }))
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
    fn parser_clears_event_type_at_empty_data_dispatch_boundary() {
        assert_eq!(
            parse_fragments(
                &[b"event: custom\nid: 7\nretry: 25\n\ndata: payload\n\n"],
                64,
                128,
                256
            )
            .unwrap(),
            vec![event("payload", None, Some("7"), Some(25))]
        );
    }

    #[test]
    fn parser_clears_fragmented_event_type_at_crlf_boundaries() {
        let fragments: &[&[u8]] = &[
            b"event: custom\r",
            b"\nid: 7\r\nretry: 25\r",
            b"\n\r\ndata: pay",
            b"load\r\n\r",
            b"\nevent: named\r\ndata: second\r\n\r\n",
            b"data: next\r\n\r\n",
        ];
        assert_eq!(
            parse_fragments(fragments, 64, 128, 256).unwrap(),
            vec![
                event("payload", None, Some("7"), Some(25)),
                event("second", Some("named"), Some("7"), Some(25)),
                event("next", None, Some("7"), Some(25)),
            ]
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
        for input in [
            b"data: \xff\n\n".as_slice(),
            b"data: \xc3".as_slice(),
            // A BOM prefix that never completes is still invalid UTF-8 and
            // must surface from `finish` at EOF instead of being dropped.
            b"\xef".as_slice(),
            b"\xef\xbb".as_slice(),
        ] {
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
    fn parser_enforces_line_item_and_total_limits_across_one_byte_chunks() {
        let exact = b"data: x\n\n";
        let exact_fragments: Vec<&[u8]> = exact.chunks(1).collect();
        assert_eq!(
            parse_fragments(&exact_fragments, 7, 1, exact.len()).unwrap(),
            vec![event("x", None, None, None)]
        );

        let line_over = b"data: abc\n\n";
        let line_fragments: Vec<&[u8]> = line_over.chunks(1).collect();
        assert!(
            parse_fragments(&line_fragments, 8, 16, 64)
                .unwrap_err()
                .contains("line")
        );

        let item_over = b"data: ab\ndata: c\n\n";
        let item_fragments: Vec<&[u8]> = item_over.chunks(1).collect();
        assert!(
            parse_fragments(&item_fragments, 16, 3, 64)
                .unwrap_err()
                .contains("item")
        );

        let total_over = b"data: ab\n\n";
        let total_fragments: Vec<&[u8]> = total_over.chunks(1).collect();
        assert!(
            parse_fragments(&total_fragments, 16, 16, total_over.len() - 1)
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
