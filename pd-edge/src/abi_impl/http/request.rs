use axum::http::HeaderName;
use edge_abi::symbols::http::request as http_request;
use pd_edge_host_function::pd_edge_host_function;
use vm::{CallOutcome, Value, Vm, VmError};

use super::{
    SharedProxyVmContext, headers_to_value_map, query_to_value_map, read_request_body_all,
    read_request_body_next_chunk, request_body_eof, request_path_with_query,
};

#[derive(Clone, Copy)]
enum RequestField {
    Id,
    Method,
    Path,
    Query,
    RawQuery,
    PathWithQuery,
    HttpVersion,
    Scheme,
    Host,
    ClientIp,
}

async fn request_field_outcome(
    context: SharedProxyVmContext,
    field: RequestField,
) -> Result<CallOutcome, VmError> {
    let context = context.lock().expect("vm context lock poisoned");
    let value = match field {
        RequestField::Id => context.request_head.request_id.clone(),
        RequestField::Method => context.request_head.method.as_str().to_string(),
        RequestField::Path => context.request_head.path.clone(),
        RequestField::Query => context.request_head.query.clone(),
        RequestField::RawQuery => context.request_head.query.clone(),
        RequestField::PathWithQuery => request_path_with_query(
            context.request_head.path.as_str(),
            context.request_head.query.as_str(),
        ),
        RequestField::HttpVersion => context.request_head.http_version.clone(),
        RequestField::Scheme => context.request_head.scheme.clone(),
        RequestField::Host => context.request_head.host.clone(),
        RequestField::ClientIp => context.request_head.client_ip.clone(),
    };
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

#[pd_edge_host_function(name = http_request::GET_ID.name, scope = http)]
async fn get_request_id(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    request_field_outcome(context, RequestField::Id).await
}

#[pd_edge_host_function(name = http_request::GET_METHOD.name, scope = http)]
async fn get_request_method(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    request_field_outcome(context, RequestField::Method).await
}

#[pd_edge_host_function(name = http_request::GET_PATH.name, scope = http)]
async fn get_request_path(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    request_field_outcome(context, RequestField::Path).await
}

#[pd_edge_host_function(name = http_request::GET_QUERY.name, scope = http)]
async fn get_request_query(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    request_field_outcome(context, RequestField::Query).await
}

#[pd_edge_host_function(name = http_request::GET_SCHEME.name, scope = http)]
async fn get_request_scheme(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    request_field_outcome(context, RequestField::Scheme).await
}

#[pd_edge_host_function(name = http_request::GET_HOST.name, scope = http)]
async fn get_request_host(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    request_field_outcome(context, RequestField::Host).await
}

#[pd_edge_host_function(name = http_request::GET_CLIENT_IP.name, scope = http)]
async fn get_request_client_ip(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    request_field_outcome(context, RequestField::ClientIp).await
}

#[pd_edge_host_function(name = http_request::GET_PATH_WITH_QUERY.name, scope = http)]
async fn get_request_path_with_query(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    request_field_outcome(context, RequestField::PathWithQuery).await
}

#[pd_edge_host_function(name = http_request::GET_RAW_QUERY.name, scope = http)]
async fn get_request_raw_query(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    request_field_outcome(context, RequestField::RawQuery).await
}

#[pd_edge_host_function(name = http_request::GET_HTTP_VERSION.name, scope = http)]
async fn get_request_http_version(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    request_field_outcome(context, RequestField::HttpVersion).await
}

#[pd_edge_host_function(name = http_request::GET_HEADER.name, scope = http)]
async fn get_request_header(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
) -> Result<CallOutcome, VmError> {
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;
    let context = context.lock().expect("vm context lock poisoned");
    let value = context
        .request_head
        .headers
        .get(&header_name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

#[pd_edge_host_function(name = http_request::GET_HEADERS.name, scope = http)]
async fn get_request_headers(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let context = context.lock().expect("vm context lock poisoned");
    Ok(CallOutcome::Return(vec![headers_to_value_map(
        &context.request_head.headers,
    )]))
}

#[pd_edge_host_function(name = http_request::GET_QUERY_ARG.name, scope = http)]
async fn get_request_query_arg(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    name: String,
) -> Result<CallOutcome, VmError> {
    let context = context.lock().expect("vm context lock poisoned");
    let value = url::form_urlencoded::parse(context.request_head.query.as_bytes())
        .find_map(|(key, value)| {
            if key == name {
                Some(value.into_owned())
            } else {
                None
            }
        })
        .unwrap_or_default();
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}

#[pd_edge_host_function(name = http_request::GET_QUERY_ARGS.name, scope = http)]
async fn get_request_query_args(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let context = context.lock().expect("vm context lock poisoned");
    Ok(CallOutcome::Return(vec![query_to_value_map(
        &context.request_head.query,
    )]))
}

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

#[pd_edge_host_function(name = "http::request::body::eof", scope = http_extension)]
async fn get_request_body_eof(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let eof = request_body_eof(&context).await?;
    Ok(CallOutcome::Return(vec![Value::Bool(eof)]))
}

#[pd_edge_host_function(name = http_request::GET_PORT.name, scope = http)]
async fn get_request_port(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let context = context.lock().expect("vm context lock poisoned");
    Ok(CallOutcome::Return(vec![Value::Int(
        context.request_head.port as i64,
    )]))
}
