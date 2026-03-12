use pd_edge_host_function::pd_edge_host_function;
use tokio::{fs::OpenOptions, io::AsyncWriteExt};
use vm::{CallOutcome, Value, Vm, VmError};

use super::http::{
    append_outbound_exchange_body, append_response_output_body_bytes, consume_request_body_all,
    default_upstream_exchange_handle, read_outbound_exchange_response_all,
    read_outbound_exchange_response_next_line, read_request_body_next_line,
    read_upstream_response_all, read_upstream_response_next_line,
};
use super::{
    EDGE_IO_HANDLE_DYNAMIC_BASE, EdgeVirtualIoHandle, ProxyVmContext, SharedProxyVmContext,
    SharedVmAsyncOps,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EdgeProtocolIoHandle {
    Downstream,
    OutboundExchange(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EdgeIoReadSource {
    Protocol(EdgeProtocolIoHandle),
    VirtualHandle(i64),
}

#[derive(Clone, Debug)]
enum EdgeIoWriteTarget {
    Protocol(EdgeProtocolIoHandle),
    FilePath { path: String, append: bool },
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

fn decode_protocol_io_handle(
    context: &ProxyVmContext,
    handle: i64,
) -> Option<EdgeProtocolIoHandle> {
    if handle == 0 {
        return Some(EdgeProtocolIoHandle::Downstream);
    }
    if handle == default_upstream_exchange_handle()
        || context.outbound_exchanges.contains_key(&handle)
    {
        return Some(EdgeProtocolIoHandle::OutboundExchange(handle));
    }
    None
}

fn invalid_io_handle_error(handle: i64) -> VmError {
    VmError::HostError(format!(
        "edge io handle {handle} is invalid; use io::open for file handles or pass a handle returned by http/tcp/tls",
    ))
}

fn requires_io_handle_error(function_name: &str) -> VmError {
    VmError::HostError(format!(
        "edge {function_name} requires a handle; use io::open for files or pass a handle returned by http/tcp/tls",
    ))
}

fn resolve_edge_io_read_source(
    context: &ProxyVmContext,
    value: &Value,
    function_name: &str,
) -> Result<EdgeIoReadSource, VmError> {
    match value {
        Value::Int(handle) => {
            if let Some(target) = decode_protocol_io_handle(context, *handle) {
                return Ok(EdgeIoReadSource::Protocol(target));
            }
            if context.edge_io_handles.contains_key(handle) {
                return Ok(EdgeIoReadSource::VirtualHandle(*handle));
            }
            Err(invalid_io_handle_error(*handle))
        }
        Value::String(_) => Err(requires_io_handle_error(function_name)),
        _ => Err(VmError::TypeMismatch("int")),
    }
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
            if let Some(target) = decode_protocol_io_handle(context, *handle) {
                return Ok(EdgeIoWriteTarget::Protocol(target));
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
        Value::String(_) => Err(requires_io_handle_error("io::write")),
        _ => Err(VmError::TypeMismatch("int")),
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
    target: EdgeProtocolIoHandle,
) -> Result<String, VmError> {
    match target {
        EdgeProtocolIoHandle::Downstream => {
            let body = consume_request_body_all(context).await?;
            Ok(String::from_utf8_lossy(&body).into_owned())
        }
        EdgeProtocolIoHandle::OutboundExchange(handle)
            if handle == default_upstream_exchange_handle() =>
        {
            Ok(String::from_utf8_lossy(&read_upstream_response_all(context).await?).into_owned())
        }
        EdgeProtocolIoHandle::OutboundExchange(handle) => Ok(String::from_utf8_lossy(
            &read_outbound_exchange_response_all(context, handle).await?,
        )
        .into_owned()),
    }
}

async fn read_io_target_line(
    context: &SharedProxyVmContext,
    target: EdgeProtocolIoHandle,
) -> Result<String, VmError> {
    match target {
        EdgeProtocolIoHandle::Downstream => {
            let line = read_request_body_next_line(context).await?;
            Ok(String::from_utf8_lossy(&line).into_owned())
        }
        EdgeProtocolIoHandle::OutboundExchange(handle)
            if handle == default_upstream_exchange_handle() =>
        {
            Ok(
                String::from_utf8_lossy(&read_upstream_response_next_line(context).await?)
                    .into_owned(),
            )
        }
        EdgeProtocolIoHandle::OutboundExchange(handle) => Ok(String::from_utf8_lossy(
            &read_outbound_exchange_response_next_line(context, handle).await?,
        )
        .into_owned()),
    }
}

fn write_io_target(
    context: &SharedProxyVmContext,
    target: EdgeProtocolIoHandle,
    text: &str,
) -> Result<(), VmError> {
    match target {
        EdgeProtocolIoHandle::Downstream => {
            append_response_output_body_bytes(context, text.as_bytes());
            Ok(())
        }
        EdgeProtocolIoHandle::OutboundExchange(handle) => {
            append_outbound_exchange_body(context, handle, text)
        }
    }
}

#[pd_edge_host_function(name = "io::open", scope = io)]
async fn io_open(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    path: String,
    mode: String,
) -> Result<CallOutcome, VmError> {
    let mode = mode.trim().to_ascii_lowercase();
    let values = match mode.as_str() {
        "r" => {
            let buffered = tokio::fs::read_to_string(&path)
                .await
                .map_err(|err| VmError::HostError(format!("edge io::open read failed: {err}")))?;
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
            let mut guard = context.lock().expect("vm context lock poisoned");
            let handle = allocate_edge_virtual_io_handle(
                &mut guard,
                EdgeVirtualIoHandle::FileWrite {
                    path: path.clone(),
                    append: mode == "a",
                },
            );
            vec![Value::Int(handle)]
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
    let source = {
        let guard = context.lock().expect("vm context lock poisoned");
        resolve_edge_io_read_source(&guard, &source, "io::read_all")?
    };
    let text = match source {
        EdgeIoReadSource::Protocol(target) => read_io_target_all(&context, target).await?,
        EdgeIoReadSource::VirtualHandle(handle) => {
            let mut guard = context.lock().expect("vm context lock poisoned");
            read_edge_virtual_handle_all(&mut guard, handle)?
        }
    };
    Ok(CallOutcome::Return(vec![Value::string(text)]))
}

#[pd_edge_host_function(name = "io::read_line", scope = io)]
async fn io_read_line(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    source: Value,
) -> Result<CallOutcome, VmError> {
    let source = {
        let guard = context.lock().expect("vm context lock poisoned");
        resolve_edge_io_read_source(&guard, &source, "io::read_line")?
    };
    let text = match source {
        EdgeIoReadSource::Protocol(target) => read_io_target_line(&context, target).await?,
        EdgeIoReadSource::VirtualHandle(handle) => {
            let mut guard = context.lock().expect("vm context lock poisoned");
            read_edge_virtual_handle_line(&mut guard, handle)?
        }
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
        EdgeIoWriteTarget::Protocol(kind) => write_io_target(&context, kind, &text)?,
        EdgeIoWriteTarget::FilePath { path, append } => {
            write_edge_file_path(&path, append, &text).await?;
        }
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
            let guard = context.lock().expect("vm context lock poisoned");
            if decode_protocol_io_handle(&guard, handle).is_none()
                && !guard.edge_io_handles.contains_key(&handle)
            {
                return Err(invalid_io_handle_error(handle));
            }
        }
        Value::String(_) => return Err(requires_io_handle_error("io::flush")),
        _ => return Err(VmError::TypeMismatch("int")),
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
            let mut guard = context.lock().expect("vm context lock poisoned");
            if decode_protocol_io_handle(&guard, handle).is_some() {
                return Ok(CallOutcome::Return(vec![Value::Bool(true)]));
            }
            if guard.edge_io_handles.remove(&handle).is_none() {
                return Err(invalid_io_handle_error(handle));
            }
        }
        Value::String(_) => return Err(requires_io_handle_error("io::close")),
        _ => return Err(VmError::TypeMismatch("int")),
    }
    Ok(CallOutcome::Return(vec![Value::Bool(true)]))
}

#[pd_edge_host_function(name = "io::exists", scope = io)]
async fn io_exists(
    _vm: &mut Vm,
    _context: SharedProxyVmContext,
    path: String,
) -> Result<CallOutcome, VmError> {
    let exists = tokio::fs::metadata(path.as_str()).await.is_ok();
    Ok(CallOutcome::Return(vec![Value::Bool(exists)]))
}
