use pd_host_function::pd_host_function;

use super::request::validate_request_header_budget;
use super::{HttpConfig, HttpHostState};
use crate::builtins::runtime::typed::VmBytes;
use crate::host_api::{
    HostFunctionSchema, HostParamPassing, HostParamSchema, HostTypeSchema, ResourceTypeKey,
};
use crate::host_extension::HostResourceType;
use crate::vm::resource::{
    CloseProgress, HostResource, ResourceCloseReason, ResourceMut, ResourceRef, ResourceResult,
};
use crate::vm::{Value, Vm, VmError, VmResult};

#[derive(Clone)]
pub(crate) struct HttpRequest {
    pub(super) method: hyper::Method,
    pub(super) url: url::Url,
    pub(super) headers: Vec<(hyper::header::HeaderName, hyper::header::HeaderValue)>,
    pub(super) body: HttpBody,
    pub(super) limits: HttpConfig,
}

#[derive(Clone)]
pub(super) enum HttpBody {
    Empty,
    Text(String),
    Bytes(Vec<u8>),
}

impl HttpBody {
    pub(super) fn len(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::Text(text) => text.len(),
            Self::Bytes(bytes) => bytes.len(),
        }
    }

    pub(super) fn into_bytes(self) -> Option<Vec<u8>> {
        match self {
            Self::Empty => None,
            Self::Text(text) => Some(text.into_bytes()),
            Self::Bytes(bytes) => Some(bytes),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct HttpHeaders(
    pub(super) Vec<(hyper::header::HeaderName, hyper::header::HeaderValue)>,
);

impl HttpHeaders {
    pub(super) fn from_map(headers: &hyper::HeaderMap) -> Self {
        let mut entries = headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Vec<_>>();
        entries.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));
        Self(entries)
    }

    fn values(&self, name: &str) -> VmResult<Vec<Value>> {
        let name = hyper::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| VmError::HostError(format!("invalid HTTP header name '{name}'")))?;
        Ok(self
            .0
            .iter()
            .filter(|(entry, _)| entry == name)
            // One Unicode scalar U+0000..U+00FF per raw byte; scalar <= 255
            // re-encoding recovers the original octet, even for valid UTF-8 bytes.
            .map(|(_, value)| {
                Value::string(
                    value
                        .as_bytes()
                        .iter()
                        .copied()
                        .map(char::from)
                        .collect::<String>(),
                )
            })
            .collect())
    }

    fn names(&self) -> Vec<Value> {
        self.0
            .iter()
            .map(|(name, _)| Value::string(name.as_str()))
            .collect()
    }
}

pub(super) struct HttpResponse {
    pub(super) status: i64,
    pub(super) url: String,
    pub(super) headers: HttpHeaders,
    pub(super) body: Vec<u8>,
}

macro_rules! memory_resource {
    ($type:ty, $key:literal, $description:literal) => {
        impl HostResource for $type {
            fn resource_type_key() -> Option<ResourceTypeKey> {
                Some(ResourceTypeKey::new($key).expect("static HTTP resource key"))
            }
            fn begin_close(
                &mut self,
                _reason: ResourceCloseReason,
            ) -> ResourceResult<CloseProgress> {
                Ok(CloseProgress::Ready)
            }
        }
        impl HostResourceType for $type {
            const KEY: &'static str = $key;
            const DESCRIPTION: &'static str = $description;
        }
    };
}

memory_resource!(
    HttpRequest,
    "http.request",
    "An unsent HTTP request builder"
);
memory_resource!(HttpResponse, "http.response", "A buffered HTTP response");

pub(super) struct SseSummary {
    pub(super) outcome: String,
    pub(super) status: i64,
    pub(super) headers: HttpHeaders,
    pub(super) url: String,
    pub(super) items: i64,
    pub(super) bytes_received: i64,
    pub(super) bytes_sent: i64,
}

memory_resource!(
    SseSummary,
    "http.sse_summary",
    "An immutable SSE stream summary"
);
memory_resource!(
    HttpHeaders,
    "http.headers",
    "An immutable HTTP header collection"
);

fn request_resource_type() -> HostTypeSchema {
    HostTypeSchema::Resource(ResourceTypeKey::new("http.request").expect("static key"))
}

pub(super) fn request_new_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "http::request::new",
        vec![
            HostParamSchema::value("method", HostTypeSchema::String),
            HostParamSchema::value("url", HostTypeSchema::String),
        ],
        request_resource_type(),
    )
}

fn string_array() -> HostTypeSchema {
    HostTypeSchema::Array(Box::new(HostTypeSchema::String))
}

pub(super) fn header_values_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "http::headers::values",
        vec![
            HostParamSchema::with_passing(
                "headers",
                HostTypeSchema::Resource(ResourceTypeKey::new("http.headers").expect("static key")),
                HostParamPassing::Borrow,
            ),
            HostParamSchema::value("name", HostTypeSchema::String),
        ],
        string_array(),
    )
}

pub(super) fn header_names_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "http::headers::names",
        vec![HostParamSchema::with_passing(
            "headers",
            HostTypeSchema::Resource(ResourceTypeKey::new("http.headers").expect("static key")),
            HostParamPassing::Borrow,
        )],
        string_array(),
    )
}

pub(super) fn response_values_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "http::response::header_values",
        vec![
            HostParamSchema::with_passing(
                "response",
                HostTypeSchema::Resource(
                    ResourceTypeKey::new("http.response").expect("static key"),
                ),
                HostParamPassing::Borrow,
            ),
            HostParamSchema::value("name", HostTypeSchema::String),
        ],
        string_array(),
    )
}

pub(super) fn response_names_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "http::response::header_names",
        vec![HostParamSchema::with_passing(
            "response",
            HostTypeSchema::Resource(ResourceTypeKey::new("http.response").expect("static key")),
            HostParamPassing::Borrow,
        )],
        string_array(),
    )
}

/// Construct an unsent HTTP request.
#[pd_host_function(name = "http::request::new", contract = request_new_contract)]
pub(super) fn new(vm: &mut Vm, method: String, url: String) -> VmResult<i64> {
    let method = method.to_ascii_uppercase();
    if !matches!(
        method.as_str(),
        "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
    ) {
        return Err(VmError::HostError(format!(
            "HTTP method '{method}' is not allowed"
        )));
    }
    let method = hyper::Method::from_bytes(method.as_bytes())
        .map_err(|_| VmError::HostError("invalid HTTP method".to_string()))?;
    let url = url
        .parse::<url::Url>()
        .map_err(|error| VmError::HostError(format!("invalid HTTP URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err(VmError::HostError(
            "invalid HTTP URL scheme or host".to_string(),
        ));
    }
    let limits = vm
        .host_context()
        .module_state::<HttpHostState>()
        .and_then(|state| state.config.clone())
        .unwrap_or_default();
    let request = HttpRequest {
        method,
        url,
        headers: Vec::new(),
        body: HttpBody::Empty,
        limits,
    };
    let token = vm
        .host_context()
        .push_resource(request)
        .map_err(super::request::host_boundary_error)?;
    Ok(token.handle().raw() as i64)
}

/// Append a validated request header.
#[pd_host_function(name = "http::request::set_header")]
pub(super) fn set_header(
    mut request: ResourceMut<'_, HttpRequest>,
    name: String,
    value: String,
) -> VmResult<()> {
    if matches!(
        name.to_ascii_lowercase().as_str(),
        "host" | "content-length" | "transfer-encoding" | "connection"
    ) {
        return Err(VmError::HostError(format!(
            "HTTP header '{name}' is managed by the client"
        )));
    }
    let parsed_name = hyper::header::HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid HTTP header name '{name}'")))?;
    let parsed_value = hyper::header::HeaderValue::from_str(&value)
        .map_err(|_| VmError::HostError(format!("invalid HTTP header value for '{name}'")))?;
    let request = request.get();
    let mut headers = request.headers.clone();
    headers.push((parsed_name, parsed_value));
    validate_request_header_budget(&headers, &request.limits)?;
    request.headers = headers;
    Ok(())
}

fn validate_body_limit(request: &HttpRequest, length: usize) -> VmResult<()> {
    if length > request.limits.max_request_body_bytes {
        return Err(VmError::HostError(
            "HTTP request body exceeds limit".to_string(),
        ));
    }
    Ok(())
}

/// Set a UTF-8 request body.
#[pd_host_function(name = "http::request::set_body_text")]
pub(super) fn set_body_text(
    mut request: ResourceMut<'_, HttpRequest>,
    body: String,
) -> VmResult<()> {
    let request = request.get();
    validate_body_limit(request, body.len())?;
    request.body = HttpBody::Text(body);
    Ok(())
}

/// Set a binary request body.
#[pd_host_function(name = "http::request::set_body_bytes")]
pub(super) fn set_body_bytes(
    mut request: ResourceMut<'_, HttpRequest>,
    body: &[u8],
) -> VmResult<()> {
    let request = request.get();
    validate_body_limit(request, body.len())?;
    request.body = HttpBody::Bytes(body.to_vec());
    Ok(())
}

/// Read the buffered response status.
#[pd_host_function(name = "http::response::status")]
pub(super) fn status(response: ResourceRef<'_, HttpResponse>) -> i64 {
    response.status
}

/// Read the final response URL.
#[pd_host_function(name = "http::response::url")]
pub(super) fn url(response: ResourceRef<'_, HttpResponse>) -> String {
    response.url.clone()
}

/// Read repeated header values from the buffered response.
/// Each raw byte becomes one Unicode scalar U+0000..U+00FF; converting each
/// scalar <= 255 back to an octet recovers the original header value.
#[pd_host_function(name = "http::response::header_values", contract = response_values_contract)]
pub(super) fn response_header_values(
    response: ResourceRef<'_, HttpResponse>,
    name: String,
) -> VmResult<Vec<Value>> {
    response.headers.values(&name)
}

/// Read ordered header names from the buffered response.
#[pd_host_function(name = "http::response::header_names", contract = response_names_contract)]
pub(super) fn response_header_names(response: ResourceRef<'_, HttpResponse>) -> Vec<Value> {
    response.headers.names()
}

/// Copy the buffered response body.
#[pd_host_function(name = "http::response::body")]
pub(super) fn body(response: ResourceRef<'_, HttpResponse>) -> VmBytes {
    response.body.clone()
}

/// Read repeated values from an independent headers resource.
/// Each raw byte becomes one Unicode scalar U+0000..U+00FF; converting each
/// scalar <= 255 back to an octet recovers the original header value.
#[pd_host_function(name = "http::headers::values", contract = header_values_contract)]
pub(super) fn headers_values(
    headers: ResourceRef<'_, HttpHeaders>,
    name: String,
) -> VmResult<Vec<Value>> {
    headers.values(&name)
}

/// Read ordered names from an independent headers resource.
#[pd_host_function(name = "http::headers::names", contract = header_names_contract)]
pub(super) fn headers_names(headers: ResourceRef<'_, HttpHeaders>) -> Vec<Value> {
    headers.names()
}

/// Read the stream termination outcome.
#[pd_host_function(name = "http::sse_summary::outcome")]
pub(super) fn sse_outcome(summary: ResourceRef<'_, SseSummary>) -> String {
    summary.outcome.clone()
}

/// Read the final HTTP status.
#[pd_host_function(name = "http::sse_summary::status")]
pub(super) fn sse_status(summary: ResourceRef<'_, SseSummary>) -> i64 {
    summary.status
}

/// Read the final response URL.
#[pd_host_function(name = "http::sse_summary::url")]
pub(super) fn sse_url(summary: ResourceRef<'_, SseSummary>) -> String {
    summary.url.clone()
}

/// Read the count of dispatched events.
#[pd_host_function(name = "http::sse_summary::items")]
pub(super) fn sse_items(summary: ResourceRef<'_, SseSummary>) -> i64 {
    summary.items
}

/// Read the body octet count.
#[pd_host_function(name = "http::sse_summary::bytes_received")]
pub(super) fn sse_bytes_received(summary: ResourceRef<'_, SseSummary>) -> i64 {
    summary.bytes_received
}

/// Read the request body octet count.
#[pd_host_function(name = "http::sse_summary::bytes_sent")]
pub(super) fn sse_bytes_sent(summary: ResourceRef<'_, SseSummary>) -> i64 {
    summary.bytes_sent
}

fn sse_headers_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "http::sse_summary::headers",
        vec![HostParamSchema::with_passing(
            "summary",
            HostTypeSchema::Resource(ResourceTypeKey::new("http.sse_summary").expect("static key")),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::Resource(ResourceTypeKey::new("http.headers").expect("static key")),
    )
}

/// Copy response headers into an independent immutable resource.
#[pd_host_function(name = "http::sse_summary::headers", contract = sse_headers_contract)]
pub(super) fn sse_headers(vm: &mut Vm, summary: i64) -> VmResult<i64> {
    let handle = crate::vm::resource::ResourceHandle::from_raw(summary as u64)
        .map_err(|error| VmError::HostError(error.to_string()))?;
    let headers = vm
        .host_context()
        .borrow_resource_with_key::<SseSummary>(
            handle,
            &ResourceTypeKey::new("http.sse_summary").expect("static key"),
        )
        .map_err(super::request::host_boundary_error)?
        .headers
        .clone();
    let token = vm
        .host_context()
        .push_resource(headers)
        .map_err(super::request::host_boundary_error)?;
    Ok(token.handle().raw() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_accessors_reject_wrong_kind_and_stale_handles() {
        let mut vm = Vm::new(crate::vm::Program::new(
            Vec::new(),
            vec![crate::vm::OpCode::Ret as u8],
        ));
        let summary = vm
            .host_context()
            .push_resource(SseSummary {
                outcome: "eof".into(),
                status: 200,
                headers: HttpHeaders(Vec::new()),
                url: "http://example.test/".into(),
                items: 0,
                bytes_received: 0,
                bytes_sent: 0,
            })
            .unwrap();
        let wrong = vm
            .host_context()
            .push_resource(HttpHeaders(Vec::new()))
            .unwrap();
        let raw = Value::Int(summary.handle().raw() as i64);
        assert_eq!(sse_outcome(&mut vm, &[raw.clone()]).unwrap(), "eof");
        assert!(sse_outcome(&mut vm, &[Value::Int(wrong.handle().raw() as i64)]).is_err());
        assert!(sse_headers(&mut vm, &[Value::Int(wrong.handle().raw() as i64)]).is_err());
        vm.host_context()
            .close_resource::<SseSummary>(summary.handle(), ResourceCloseReason::Requested)
            .unwrap();
        assert!(sse_outcome(&mut vm, &[raw.clone()]).is_err());
        assert!(sse_headers(&mut vm, &[raw]).is_err());
    }

    #[test]
    fn independent_headers_resource_preserves_duplicate_values_and_closes() {
        let mut vm = Vm::new(crate::vm::Program::new(
            Vec::new(),
            vec![crate::vm::OpCode::Ret as u8],
        ));
        let token = vm
            .host_context()
            .push_resource(HttpHeaders(vec![
                (
                    hyper::header::HeaderName::from_static("x-repeat"),
                    hyper::header::HeaderValue::from_static("first"),
                ),
                (
                    hyper::header::HeaderName::from_static("x-repeat"),
                    hyper::header::HeaderValue::from_static("second"),
                ),
            ]))
            .unwrap();
        let handle = Value::Int(token.handle().raw() as i64);
        assert_eq!(
            headers_values(&mut vm, &[handle.clone(), Value::string("X-Repeat")]).unwrap(),
            vec![Value::string("first"), Value::string("second")],
        );
        assert_eq!(
            headers_names(&mut vm, &[handle.clone()]).unwrap(),
            vec![Value::string("x-repeat"), Value::string("x-repeat")]
        );
        assert!(matches!(
            vm.host_context()
                .close_resource::<HttpHeaders>(token.handle(), ResourceCloseReason::Requested)
                .unwrap(),
            CloseProgress::Ready
        ));
        assert!(headers_names(&mut vm, &[handle]).is_err());
    }

    #[test]
    fn independent_headers_resource_maps_each_octet_to_one_scalar() {
        let mut vm = Vm::new(crate::vm::Program::new(
            Vec::new(),
            vec![crate::vm::OpCode::Ret as u8],
        ));
        let token = vm
            .host_context()
            .push_resource(HttpHeaders(vec![
                (
                    hyper::header::HeaderName::from_static("x-raw"),
                    hyper::header::HeaderValue::from_bytes(&[0x80]).unwrap(),
                ),
                (
                    hyper::header::HeaderName::from_static("x-raw"),
                    hyper::header::HeaderValue::from_bytes(&[0xc3, 0xa9]).unwrap(),
                ),
            ]))
            .unwrap();
        let handle = Value::Int(token.handle().raw() as i64);
        assert_eq!(
            headers_values(&mut vm, &[handle, Value::string("X-Raw")]).unwrap(),
            vec![Value::string("\u{80}"), Value::string("\u{c3}\u{a9}")],
        );
    }
}
