use pd_edge_host_function::pd_edge_host_function;
use tokio::{fs::OpenOptions, io::AsyncWriteExt};
use vm::{CallOutcome, Value, Vm, VmError};

use super::http::{
    consume_request_body_all, read_request_body_next_line, read_upstream_response_all,
    read_upstream_response_next_line, resolve_outbound_request_body, upstream_response_available,
};
use super::{
    EDGE_IO_HANDLE_DYNAMIC_BASE, EdgeVirtualIoHandle, ProxyVmContext, SharedProxyVmContext,
    SharedVmAsyncOps,
};

pub(crate) const EDGE_IO_HANDLE_REQUEST_BODY: i64 = 1;
pub(crate) const EDGE_IO_HANDLE_RESPONSE_BODY: i64 = 2;
pub(crate) const EDGE_IO_HANDLE_UPSTREAM_REQUEST_BODY: i64 = 3;
pub(crate) const EDGE_IO_HANDLE_UPSTREAM_RESPONSE_BODY: i64 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EdgeIoHandleKind {
    Request,
    Response,
    UpstreamRequest,
    UpstreamResponse,
}

#[derive(Clone, Debug)]
enum EdgeIoWriteTarget {
    Builtin(EdgeIoHandleKind),
    FilePath { path: String, append: bool },
    Ignore,
}

pub(super) fn register_builtin_io_overrides(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) -> Result<(), VmError> {
    super::registry::register_host_scope(
        vm,
        &context,
        &async_ops,
        super::registry::EdgeHostScope::Io,
    );
    Ok(())
}

fn decode_edge_io_handle(handle: i64) -> Result<EdgeIoHandleKind, VmError> {
    match handle {
        EDGE_IO_HANDLE_REQUEST_BODY => Ok(EdgeIoHandleKind::Request),
        EDGE_IO_HANDLE_RESPONSE_BODY => Ok(EdgeIoHandleKind::Response),
        EDGE_IO_HANDLE_UPSTREAM_REQUEST_BODY => Ok(EdgeIoHandleKind::UpstreamRequest),
        EDGE_IO_HANDLE_UPSTREAM_RESPONSE_BODY => Ok(EdgeIoHandleKind::UpstreamResponse),
        _ => Err(VmError::HostError(format!(
            "edge io handle {handle} is invalid; expected request/response/upstream request/upstream response handle",
        ))),
    }
}

fn path_targets_upstream_request(path: &str) -> bool {
    let normalized = path.trim().to_ascii_lowercase();
    normalized.contains("upstream")
}

fn edge_io_target_from_string(value: &str) -> Option<EdgeIoHandleKind> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "request_body" | "request.body" | "request" | "body" | "http.request.body"
        | "inbound.body" => Some(EdgeIoHandleKind::Request),
        "response_body"
        | "response.body"
        | "response"
        | "http.response.body"
        | "outbound.response.body" => Some(EdgeIoHandleKind::Response),
        "upstream_body"
        | "upstream.body"
        | "upstream_request_body"
        | "upstream.request.body"
        | "outbound.body"
        | "http.upstream.request.body" => Some(EdgeIoHandleKind::UpstreamRequest),
        "upstream_response_body"
        | "upstream.response.body"
        | "http.upstream.response.body"
        | "outbound.upstream.response.body" => Some(EdgeIoHandleKind::UpstreamResponse),
        _ => None,
    }
}

fn edge_io_readable_path(path: &str) -> bool {
    let normalized = path.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "request_body" | "request.body" | "request" | "body" | "http.request.body" | "inbound.body"
    )
}

fn allocate_edge_virtual_io_handle(
    context: &mut ProxyVmContext,
    handle: EdgeVirtualIoHandle,
) -> i64 {
    let handle_id = context.edge_io_next_handle.max(EDGE_IO_HANDLE_DYNAMIC_BASE);
    context.edge_io_next_handle = handle_id.saturating_add(1);
    context.edge_io_handles.insert(handle_id, handle);
    handle_id
}

fn read_edge_virtual_handle_all(
    context: &mut ProxyVmContext,
    handle: i64,
) -> Result<String, VmError> {
    match context.edge_io_handles.get_mut(&handle) {
        Some(EdgeVirtualIoHandle::BufferedRead { text, offset }) => {
            *offset = text.len();
            Ok(text.clone())
        }
        Some(EdgeVirtualIoHandle::FileWrite { .. }) => Err(VmError::HostError(format!(
            "edge io handle {handle} is write-only",
        ))),
        None => Err(VmError::HostError(format!(
            "edge io handle {handle} is invalid",
        ))),
    }
}

fn read_edge_virtual_handle_line(
    context: &mut ProxyVmContext,
    handle: i64,
) -> Result<String, VmError> {
    match context.edge_io_handles.get_mut(&handle) {
        Some(EdgeVirtualIoHandle::BufferedRead { text, offset }) => {
            let start = (*offset).min(text.len());
            if start >= text.len() {
                return Ok(String::new());
            }
            let bytes = text.as_bytes();
            let mut end = start;
            while end < bytes.len() && bytes[end] != b'\n' {
                end += 1;
            }
            let line = text[start..end].to_string();
            if end < bytes.len() && bytes[end] == b'\n' {
                end += 1;
            }
            *offset = end;
            Ok(line)
        }
        Some(EdgeVirtualIoHandle::FileWrite { .. }) => Err(VmError::HostError(format!(
            "edge io handle {handle} is write-only",
        ))),
        None => Err(VmError::HostError(format!(
            "edge io handle {handle} is invalid",
        ))),
    }
}

fn resolve_edge_io_write_target(
    context: &ProxyVmContext,
    value: &Value,
) -> Result<EdgeIoWriteTarget, VmError> {
    match value {
        Value::Int(handle) => {
            if let Ok(target) = decode_edge_io_handle(*handle) {
                return Ok(EdgeIoWriteTarget::Builtin(target));
            }
            match context.edge_io_handles.get(handle) {
                Some(EdgeVirtualIoHandle::FileWrite { path, append }) => {
                    Ok(EdgeIoWriteTarget::FilePath {
                        path: path.clone(),
                        append: *append,
                    })
                }
                Some(EdgeVirtualIoHandle::BufferedRead { .. }) => Err(VmError::HostError(format!(
                    "edge io handle {handle} is read-only",
                ))),
                None => Err(VmError::HostError(format!(
                    "edge io handle {handle} is invalid",
                ))),
            }
        }
        Value::String(name) => {
            if let Some(target) = edge_io_target_from_string(name) {
                Ok(EdgeIoWriteTarget::Builtin(target))
            } else {
                Ok(EdgeIoWriteTarget::Ignore)
            }
        }
        _ => Err(VmError::TypeMismatch("string/int")),
    }
}

async fn write_edge_file_path(path: &str, append: bool, text: &str) -> Result<(), VmError> {
    if append {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
            .map_err(|err| VmError::HostError(format!("edge io::write open failed: {err}")))?;
        file.write_all(text.as_bytes())
            .await
            .map_err(|err| VmError::HostError(format!("edge io::write append failed: {err}")))?;
        file.flush()
            .await
            .map_err(|err| VmError::HostError(format!("edge io::write flush failed: {err}")))?;
        return Ok(());
    }

    tokio::fs::write(path, text.as_bytes())
        .await
        .map_err(|err| VmError::HostError(format!("edge io::write failed: {err}")))
}

async fn read_io_target_all(
    context: &SharedProxyVmContext,
    target: EdgeIoHandleKind,
) -> Result<String, VmError> {
    match target {
        EdgeIoHandleKind::Request => {
            let body = consume_request_body_all(context).await?;
            Ok(String::from_utf8_lossy(&body).into_owned())
        }
        EdgeIoHandleKind::Response => {
            let guard = context.lock().expect("vm context lock poisoned");
            Ok(
                String::from_utf8_lossy(guard.response_output.body.as_deref().unwrap_or_default())
                    .into_owned(),
            )
        }
        EdgeIoHandleKind::UpstreamRequest => {
            let body = resolve_outbound_request_body(context).await?;
            Ok(String::from_utf8_lossy(&body).into_owned())
        }
        EdgeIoHandleKind::UpstreamResponse => {
            Ok(String::from_utf8_lossy(&read_upstream_response_all(context).await?).into_owned())
        }
    }
}

async fn read_io_target_line(
    context: &SharedProxyVmContext,
    target: EdgeIoHandleKind,
) -> Result<String, VmError> {
    match target {
        EdgeIoHandleKind::Request => {
            let line = read_request_body_next_line(context).await?;
            Ok(String::from_utf8_lossy(&line).into_owned())
        }
        EdgeIoHandleKind::Response => {
            let guard = context.lock().expect("vm context lock poisoned");
            Ok(
                String::from_utf8_lossy(guard.response_output.body.as_deref().unwrap_or_default())
                    .into_owned(),
            )
        }
        EdgeIoHandleKind::UpstreamRequest => {
            let body = resolve_outbound_request_body(context).await?;
            Ok(String::from_utf8_lossy(&body).into_owned())
        }
        EdgeIoHandleKind::UpstreamResponse => Ok(String::from_utf8_lossy(
            &read_upstream_response_next_line(context).await?,
        )
        .into_owned()),
    }
}

fn write_io_target(
    context: &mut ProxyVmContext,
    target: EdgeIoHandleKind,
    text: &str,
) -> Result<(), VmError> {
    match target {
        EdgeIoHandleKind::Request => Err(VmError::HostError(
            "edge io::write does not support request body read handle".to_string(),
        )),
        EdgeIoHandleKind::Response => {
            context.response_output.body = Some(text.as_bytes().to_vec());
            Ok(())
        }
        EdgeIoHandleKind::UpstreamRequest => {
            context.outbound_request.body_override = Some(text.as_bytes().to_vec());
            Ok(())
        }
        EdgeIoHandleKind::UpstreamResponse => Err(VmError::HostError(
            "edge io::write does not support upstream response body handles".to_string(),
        )),
    }
}

#[pd_edge_host_function(name = "io::open", scope = io)]
async fn io_open(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    path: String,
    mode: String,
) -> Result<CallOutcome, VmError> {
    let explicit_target = edge_io_target_from_string(&path);
    let mode = mode.trim().to_ascii_lowercase();
    let values = match mode.as_str() {
        "r" => {
            if let Some(target) = explicit_target {
                let handle = match target {
                    EdgeIoHandleKind::Request => EDGE_IO_HANDLE_REQUEST_BODY,
                    EdgeIoHandleKind::Response => EDGE_IO_HANDLE_RESPONSE_BODY,
                    EdgeIoHandleKind::UpstreamRequest => EDGE_IO_HANDLE_UPSTREAM_REQUEST_BODY,
                    EdgeIoHandleKind::UpstreamResponse => EDGE_IO_HANDLE_UPSTREAM_RESPONSE_BODY,
                };
                return Ok(CallOutcome::Return(vec![Value::Int(handle)]));
            }

            let buffered = match tokio::fs::read_to_string(&path).await {
                Ok(content) => content,
                Err(_) => path.clone(),
            };
            let mut guard = context.lock().expect("vm context lock poisoned");
            let handle = allocate_edge_virtual_io_handle(
                &mut guard,
                EdgeVirtualIoHandle::BufferedRead {
                    text: buffered,
                    offset: 0,
                },
            );
            vec![Value::Int(handle)]
        }
        "w" | "a" => {
            if let Some(target) = explicit_target {
                let handle = match target {
                    EdgeIoHandleKind::Request => {
                        return Err(VmError::HostError(
                            "edge io::open does not allow write mode on request body".to_string(),
                        ));
                    }
                    EdgeIoHandleKind::Response => EDGE_IO_HANDLE_RESPONSE_BODY,
                    EdgeIoHandleKind::UpstreamRequest => EDGE_IO_HANDLE_UPSTREAM_REQUEST_BODY,
                    EdgeIoHandleKind::UpstreamResponse => {
                        return Err(VmError::HostError(
                            "edge io::open does not allow write mode on upstream response body"
                                .to_string(),
                        ));
                    }
                };
                return Ok(CallOutcome::Return(vec![Value::Int(handle)]));
            }

            let target = if path_targets_upstream_request(&path) {
                EDGE_IO_HANDLE_UPSTREAM_REQUEST_BODY
            } else {
                let mut guard = context.lock().expect("vm context lock poisoned");
                allocate_edge_virtual_io_handle(
                    &mut guard,
                    EdgeVirtualIoHandle::FileWrite {
                        path: path.clone(),
                        append: mode == "a",
                    },
                )
            };
            vec![Value::Int(target)]
        }
        _ => {
            return Err(VmError::HostError(format!(
                "edge io::open only supports modes 'r', 'w', or 'a', got '{mode}'",
            )));
        }
    };
    Ok(CallOutcome::Return(values))
}

#[pd_edge_host_function(name = "io::popen", scope = io)]
async fn io_popen(_vm: &mut Vm, _command: String, _mode: String) -> Result<CallOutcome, VmError> {
    Err(VmError::HostError(
        "io::popen is disabled in edge runtime; use protocol-specific async host APIs".to_string(),
    ))
}

#[pd_edge_host_function(name = "io::read_all", scope = io)]
async fn io_read_all(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    source: Value,
) -> Result<CallOutcome, VmError> {
    let text = match &source {
        Value::String(literal) => match edge_io_target_from_string(literal) {
            Some(target) => read_io_target_all(&context, target).await?,
            None => literal.to_string(),
        },
        Value::Int(handle) => {
            let mut guard = context.lock().expect("vm context lock poisoned");
            match decode_edge_io_handle(*handle) {
                Ok(target) => {
                    drop(guard);
                    read_io_target_all(&context, target).await?
                }
                Err(_) => read_edge_virtual_handle_all(&mut guard, *handle)?,
            }
        }
        _ => return Err(VmError::TypeMismatch("string/int")),
    };
    Ok(CallOutcome::Return(vec![Value::string(text)]))
}

#[pd_edge_host_function(name = "io::read_line", scope = io)]
async fn io_read_line(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    source: Value,
) -> Result<CallOutcome, VmError> {
    let text = match &source {
        Value::String(literal) => match edge_io_target_from_string(literal) {
            Some(target) => read_io_target_line(&context, target).await?,
            None => {
                let mut lines = literal.lines();
                lines.next().unwrap_or_default().to_string()
            }
        },
        Value::Int(handle) => {
            let mut guard = context.lock().expect("vm context lock poisoned");
            match decode_edge_io_handle(*handle) {
                Ok(target) => {
                    drop(guard);
                    read_io_target_line(&context, target).await?
                }
                Err(_) => read_edge_virtual_handle_line(&mut guard, *handle)?,
            }
        }
        _ => return Err(VmError::TypeMismatch("string/int")),
    };
    Ok(CallOutcome::Return(vec![Value::string(text)]))
}

#[pd_edge_host_function(name = "io::write", scope = io)]
async fn io_write(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    target_arg: Value,
    text: String,
) -> Result<CallOutcome, VmError> {
    let target = {
        let guard = context.lock().expect("vm context lock poisoned");
        resolve_edge_io_write_target(&guard, &target_arg)?
    };
    match target {
        EdgeIoWriteTarget::Builtin(kind) => {
            let mut guard = context.lock().expect("vm context lock poisoned");
            write_io_target(&mut guard, kind, &text)?;
        }
        EdgeIoWriteTarget::FilePath { path, append } => {
            write_edge_file_path(&path, append, &text).await?;
        }
        EdgeIoWriteTarget::Ignore => {}
    }
    Ok(CallOutcome::Return(vec![Value::Int(text.len() as i64)]))
}

#[pd_edge_host_function(name = "io::flush", scope = io)]
async fn io_flush(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    target: Value,
) -> Result<CallOutcome, VmError> {
    match target {
        Value::Int(handle) => {
            if decode_edge_io_handle(handle).is_err() {
                let guard = context.lock().expect("vm context lock poisoned");
                if !guard.edge_io_handles.contains_key(&handle) {
                    return Err(VmError::HostError(format!(
                        "edge io handle {handle} is invalid",
                    )));
                }
            }
        }
        Value::String(_) => {}
        _ => return Err(VmError::TypeMismatch("string/int")),
    }
    Ok(CallOutcome::Return(vec![Value::Bool(true)]))
}

#[pd_edge_host_function(name = "io::close", scope = io)]
async fn io_close(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    target: Value,
) -> Result<CallOutcome, VmError> {
    match target {
        Value::Int(handle) => {
            if decode_edge_io_handle(handle).is_err() {
                let mut guard = context.lock().expect("vm context lock poisoned");
                if guard.edge_io_handles.remove(&handle).is_none() {
                    return Err(VmError::HostError(format!(
                        "edge io handle {handle} is invalid",
                    )));
                }
            }
        }
        Value::String(_) => {}
        _ => return Err(VmError::TypeMismatch("string/int")),
    }
    Ok(CallOutcome::Return(vec![Value::Bool(true)]))
}

#[pd_edge_host_function(name = "io::exists", scope = io)]
async fn io_exists(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    path: String,
) -> Result<CallOutcome, VmError> {
    let exists = match edge_io_target_from_string(&path) {
        Some(EdgeIoHandleKind::UpstreamResponse) => upstream_response_available(&context),
        Some(_) => true,
        None if edge_io_readable_path(&path) || path_targets_upstream_request(&path) => true,
        None => tokio::fs::metadata(path.as_str()).await.is_ok(),
    };
    Ok(CallOutcome::Return(vec![Value::Bool(exists)]))
}
