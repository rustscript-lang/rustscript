use axum::http::HeaderName;
use edge_abi::symbols::http::request as http_request;
use pd_edge_host_function::pd_edge_host_function;
use vm::{CallOutcome, Value, Vm, VmError};

use super::{
    SharedProxyVmContext, headers_to_value_map, query_to_value_map, read_request_body_all,
    read_request_body_next_chunk, request_body_eof, request_path_with_query,
    schedule_downstream_http_handoff,
};
use crate::{
    abi_impl::schedule_current_future_call, runtime::promote_transport_context_into_http_request,
};

#[derive(Clone, Copy)]
enum RequestField {
    Id,
    Method,
    Path,
    Query,
    PathWithQuery,
    HttpVersion,
    Scheme,
    Host,
    ClientIp,
}

fn request_field_outcome(
    context: SharedProxyVmContext,
    field: RequestField,
) -> Result<CallOutcome, VmError> {
    let value = context.with_request_head(|request_head| match field {
        RequestField::Id => request_head.request_id().to_string(),
        RequestField::Method => request_head.method().as_str().to_string(),
        RequestField::Path => request_head.path().to_string(),
        RequestField::Query => request_head.query().to_string(),
        RequestField::PathWithQuery => {
            request_path_with_query(request_head.path(), request_head.query())
        }
        RequestField::HttpVersion => request_head.http_version().to_string(),
        RequestField::Scheme => request_head.scheme().to_string(),
        RequestField::Host => request_head.host().to_string(),
        RequestField::ClientIp => request_head.client_ip().to_string(),
    });
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

/// Returns the current downstream request id.
#[pd_edge_host_function(name = http_request::GET_ID.name, scope = http)]
fn get_request_id(context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    request_field_outcome(context, RequestField::Id)
}

/// Returns the HTTP method for the downstream HTTP request.
#[pd_edge_host_function(name = http_request::GET_METHOD.name, scope = http)]
fn get_request_method(context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    request_field_outcome(context, RequestField::Method)
}

/// Returns the request path for the downstream HTTP request.
#[pd_edge_host_function(name = http_request::GET_PATH.name, scope = http)]
fn get_request_path(context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    request_field_outcome(context, RequestField::Path)
}

/// Returns the decoded query string for the downstream HTTP request.
#[pd_edge_host_function(name = http_request::GET_QUERY.name, scope = http)]
fn get_request_query(context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    request_field_outcome(context, RequestField::Query)
}

/// Returns the URL scheme for the downstream HTTP request.
#[pd_edge_host_function(name = http_request::GET_SCHEME.name, scope = http)]
fn get_request_scheme(context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    request_field_outcome(context, RequestField::Scheme)
}

/// Returns the host name for the downstream HTTP request.
#[pd_edge_host_function(name = http_request::GET_HOST.name, scope = http)]
fn get_request_host(context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    request_field_outcome(context, RequestField::Host)
}

/// Returns the downstream client IP address.
#[pd_edge_host_function(name = http_request::GET_CLIENT_IP.name, scope = http)]
fn get_request_client_ip(context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    request_field_outcome(context, RequestField::ClientIp)
}

/// Returns the request path and query string for the downstream HTTP request.
#[pd_edge_host_function(name = http_request::GET_PATH_WITH_QUERY.name, scope = http)]
fn get_request_path_with_query(context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    request_field_outcome(context, RequestField::PathWithQuery)
}

/// Returns the HTTP version for the downstream HTTP request.
#[pd_edge_host_function(name = http_request::GET_HTTP_VERSION.name, scope = http)]
fn get_request_http_version(context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    request_field_outcome(context, RequestField::HttpVersion)
}

/// Returns the first value for a header on the downstream HTTP request.
#[pd_edge_host_function(name = http_request::GET_HEADER.name, scope = http)]
fn get_request_header(context: SharedProxyVmContext, name: String) -> Result<CallOutcome, VmError> {
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;
    let value = context.with_request_head(|request_head| {
        request_head
            .headers()
            .get(&header_name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string()
    });
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

/// Returns all headers on the downstream HTTP request as a map.
#[pd_edge_host_function(name = http_request::GET_HEADERS.name, scope = http)]
fn get_request_headers(context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![context.with_request_head(
        |request_head| headers_to_value_map(request_head.headers()),
    )]))
}

/// Returns a query parameter from the downstream HTTP request.
#[pd_edge_host_function(name = http_request::GET_QUERY_ARG.name, scope = http)]
fn get_request_query_arg(
    context: SharedProxyVmContext,
    name: String,
) -> Result<CallOutcome, VmError> {
    let value = context.with_request_head(|request_head| {
        url::form_urlencoded::parse(request_head.query().as_bytes())
            .find_map(|(key, value)| {
                if key == name {
                    Some(value.into_owned())
                } else {
                    None
                }
            })
            .unwrap_or_default()
    });
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

/// Returns all query parameters from the downstream HTTP request as a map.
#[pd_edge_host_function(name = http_request::GET_QUERY_ARGS.name, scope = http)]
fn get_request_query_args(context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![context.with_request_head(
        |request_head| query_to_value_map(request_head.query()),
    )]))
}

/// Returns the full body for the downstream HTTP request as text.
#[pd_edge_host_function(name = http_request::GET_BODY.name, scope = http)]
async fn get_request_body(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let body = read_request_body_all(&context).await?;
    Ok(CallOutcome::Return(vec![Value::string(
        String::from_utf8_lossy(&body).into_owned(),
    )]))
}

/// Reads the next body chunk from the downstream HTTP request.
#[pd_edge_host_function(name = "http::request::body::next_chunk", scope = http_extension)]
async fn get_request_body_chunk(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    max_bytes: i64,
) -> Result<CallOutcome, VmError> {
    if max_bytes <= 0 {
        return Err(VmError::HostError(format!(
            "body chunk size must be > 0, got '{max_bytes}'",
        )));
    }
    let chunk = read_request_body_next_chunk(&context, max_bytes as usize).await?;
    Ok(CallOutcome::Return(vec![Value::string(
        String::from_utf8_lossy(&chunk).into_owned(),
    )]))
}

/// Returns whether the body stream for the downstream HTTP request is exhausted.
#[pd_edge_host_function(name = "http::request::body::eof", scope = http_extension)]
async fn get_request_body_eof(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let eof = request_body_eof(&context).await?;
    Ok(CallOutcome::Return(vec![Value::Bool(eof)]))
}

/// Returns the local destination port for the downstream HTTP request.
#[pd_edge_host_function(name = http_request::GET_PORT.name, scope = http)]
fn get_request_port(context: SharedProxyVmContext) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::Int(
        context.with_request_head(|request_head| request_head.port() as i64),
    )]))
}

/// Attaches the untouched downstream transport to the HTTP stack and resumes
/// the current VM invocation with HTTP request semantics.
#[pd_edge_host_function(name = "http::downstream::attach_transport", scope = http)]
fn attach_downstream_transport_to_http(
    vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    schedule_downstream_http_handoff(&context)?;
    schedule_current_future_call(vm, async move {
        promote_transport_context_into_http_request(context).await?;
        Ok(vec![])
    })
}
