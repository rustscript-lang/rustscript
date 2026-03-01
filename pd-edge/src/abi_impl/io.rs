use tokio::{fs::OpenOptions, io::AsyncWriteExt};
use vm::{CallOutcome, HostFunction, Value, Vm, VmError};

use super::{
    EDGE_IO_HANDLE_DYNAMIC_BASE, EdgeVirtualIoHandle, ProxyVmContext, SharedProxyVmContext,
    SharedVmAsyncOps, bind_async_host, expect_arg_count, expect_string, schedule_future_call,
};

const EDGE_IO_HANDLE_REQUEST_BODY: i64 = 1;
const EDGE_IO_HANDLE_RESPONSE_BODY: i64 = 2;
const EDGE_IO_HANDLE_UPSTREAM_REQUEST_BODY: i64 = 3;
const EDGE_IO_HANDLE_UPSTREAM_RESPONSE_BODY: i64 = 4;

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
    bind_async_host(
        vm,
        &async_ops,
        "io::open",
        Box::new(BuiltinIoOpenFunction::new(
            context.clone(),
            async_ops.clone(),
        )),
    );
    bind_async_host(
        vm,
        &async_ops,
        "io::popen",
        Box::new(BuiltinIoPopenFunction::new(async_ops.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "io::read_all",
        Box::new(BuiltinIoReadAllFunction::new(
            context.clone(),
            async_ops.clone(),
        )),
    );
    bind_async_host(
        vm,
        &async_ops,
        "io::read_line",
        Box::new(BuiltinIoReadLineFunction::new(
            context.clone(),
            async_ops.clone(),
        )),
    );
    bind_async_host(
        vm,
        &async_ops,
        "io::write",
        Box::new(BuiltinIoWriteFunction::new(
            context.clone(),
            async_ops.clone(),
        )),
    );
    bind_async_host(
        vm,
        &async_ops,
        "io::flush",
        Box::new(BuiltinIoFlushFunction::new(
            context.clone(),
            async_ops.clone(),
        )),
    );
    bind_async_host(
        vm,
        &async_ops,
        "io::close",
        Box::new(BuiltinIoCloseFunction::new(
            context.clone(),
            async_ops.clone(),
        )),
    );
    bind_async_host(
        vm,
        &async_ops,
        "io::exists",
        Box::new(BuiltinIoExistsFunction::new(async_ops.clone())),
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

fn read_io_target_all(context: &mut ProxyVmContext, target: EdgeIoHandleKind) -> String {
    match target {
        EdgeIoHandleKind::Request => {
            context.inbound_request_body_offset = context.inbound_request_body.len();
            String::from_utf8_lossy(&context.inbound_request_body).into_owned()
        }
        EdgeIoHandleKind::Response => context.response_content.clone().unwrap_or_default(),
        EdgeIoHandleKind::UpstreamRequest => {
            String::from_utf8_lossy(&context.outbound_request_body).into_owned()
        }
        EdgeIoHandleKind::UpstreamResponse => context
            .upstream_response_content
            .clone()
            .unwrap_or_default(),
    }
}

fn read_io_target_line(context: &mut ProxyVmContext, target: EdgeIoHandleKind) -> String {
    match target {
        EdgeIoHandleKind::Request => {
            let start = context
                .inbound_request_body_offset
                .min(context.inbound_request_body.len());
            if start >= context.inbound_request_body.len() {
                return String::new();
            }
            let bytes = &context.inbound_request_body;
            let mut end = start;
            while end < bytes.len() && bytes[end] != b'\n' {
                end += 1;
            }
            let line = String::from_utf8_lossy(&bytes[start..end]).into_owned();
            if end < bytes.len() && bytes[end] == b'\n' {
                end += 1;
            }
            context.inbound_request_body_offset = end;
            line
        }
        EdgeIoHandleKind::Response => context.response_content.clone().unwrap_or_default(),
        EdgeIoHandleKind::UpstreamRequest => {
            String::from_utf8_lossy(&context.outbound_request_body).into_owned()
        }
        EdgeIoHandleKind::UpstreamResponse => context
            .upstream_response_content
            .clone()
            .unwrap_or_default(),
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
            context.response_content = Some(text.to_string());
            Ok(())
        }
        EdgeIoHandleKind::UpstreamRequest => {
            context.outbound_request_body = text.as_bytes().to_vec();
            Ok(())
        }
        EdgeIoHandleKind::UpstreamResponse => Err(VmError::HostError(
            "edge io::write does not support upstream response body handles".to_string(),
        )),
    }
}

struct BuiltinIoOpenFunction {
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
}

impl BuiltinIoOpenFunction {
    fn new(context: SharedProxyVmContext, async_ops: SharedVmAsyncOps) -> Self {
        Self { context, async_ops }
    }
}

impl HostFunction for BuiltinIoOpenFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 2)?;
        let path = expect_string(args, 0)?;
        let mode = expect_string(args, 1)?;
        let explicit_target = edge_io_target_from_string(&path);
        let context = self.context.clone();
        schedule_future_call(vm, &self.async_ops, async move {
            tokio::task::yield_now().await;
            let mode = mode.trim().to_ascii_lowercase();
            match mode.as_str() {
                "r" => {
                    if let Some(target) = explicit_target {
                        let handle = match target {
                            EdgeIoHandleKind::Request => EDGE_IO_HANDLE_REQUEST_BODY,
                            EdgeIoHandleKind::Response => EDGE_IO_HANDLE_RESPONSE_BODY,
                            EdgeIoHandleKind::UpstreamRequest => {
                                EDGE_IO_HANDLE_UPSTREAM_REQUEST_BODY
                            }
                            EdgeIoHandleKind::UpstreamResponse => {
                                EDGE_IO_HANDLE_UPSTREAM_RESPONSE_BODY
                            }
                        };
                        return Ok(vec![Value::Int(handle)]);
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
                    Ok(vec![Value::Int(handle)])
                }
                "w" | "a" => {
                    if let Some(target) = explicit_target {
                        let handle = match target {
                            EdgeIoHandleKind::Request => {
                                return Err(VmError::HostError(
                                    "edge io::open does not allow write mode on request body"
                                        .to_string(),
                                ));
                            }
                            EdgeIoHandleKind::Response => EDGE_IO_HANDLE_RESPONSE_BODY,
                            EdgeIoHandleKind::UpstreamRequest => {
                                EDGE_IO_HANDLE_UPSTREAM_REQUEST_BODY
                            }
                            EdgeIoHandleKind::UpstreamResponse => {
                                return Err(VmError::HostError(
                                    "edge io::open does not allow write mode on upstream response body"
                                        .to_string(),
                                ));
                            }
                        };
                        return Ok(vec![Value::Int(handle)]);
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
                    Ok(vec![Value::Int(target)])
                }
                _ => Err(VmError::HostError(format!(
                    "edge io::open only supports modes 'r', 'w', or 'a', got '{mode}'",
                ))),
            }
        })
    }
}

struct BuiltinIoPopenFunction {
    async_ops: SharedVmAsyncOps,
}

impl BuiltinIoPopenFunction {
    fn new(async_ops: SharedVmAsyncOps) -> Self {
        Self { async_ops }
    }
}

impl HostFunction for BuiltinIoPopenFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 2)?;
        schedule_future_call(vm, &self.async_ops, async move {
            tokio::task::yield_now().await;
            Err(VmError::HostError(
                "io::popen is disabled in edge runtime; use protocol-specific async host APIs"
                    .to_string(),
            ))
        })
    }
}

struct BuiltinIoReadAllFunction {
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
}

impl BuiltinIoReadAllFunction {
    fn new(context: SharedProxyVmContext, async_ops: SharedVmAsyncOps) -> Self {
        Self { context, async_ops }
    }
}

impl HostFunction for BuiltinIoReadAllFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let source = args
            .first()
            .cloned()
            .ok_or(VmError::TypeMismatch("string/int"))?;
        let context = self.context.clone();
        schedule_future_call(vm, &self.async_ops, async move {
            tokio::task::yield_now().await;
            let text = match &source {
                Value::String(literal) => match edge_io_target_from_string(literal) {
                    Some(target) => {
                        let mut guard = context.lock().expect("vm context lock poisoned");
                        read_io_target_all(&mut guard, target)
                    }
                    None => literal.clone(),
                },
                Value::Int(handle) => {
                    let mut guard = context.lock().expect("vm context lock poisoned");
                    match decode_edge_io_handle(*handle) {
                        Ok(target) => read_io_target_all(&mut guard, target),
                        Err(_) => read_edge_virtual_handle_all(&mut guard, *handle)?,
                    }
                }
                _ => return Err(VmError::TypeMismatch("string/int")),
            };
            Ok(vec![Value::String(text)])
        })
    }
}

struct BuiltinIoReadLineFunction {
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
}

impl BuiltinIoReadLineFunction {
    fn new(context: SharedProxyVmContext, async_ops: SharedVmAsyncOps) -> Self {
        Self { context, async_ops }
    }
}

impl HostFunction for BuiltinIoReadLineFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let source = args
            .first()
            .cloned()
            .ok_or(VmError::TypeMismatch("string/int"))?;
        let context = self.context.clone();
        schedule_future_call(vm, &self.async_ops, async move {
            tokio::task::yield_now().await;
            let text = match &source {
                Value::String(literal) => match edge_io_target_from_string(literal) {
                    Some(target) => {
                        let mut guard = context.lock().expect("vm context lock poisoned");
                        read_io_target_line(&mut guard, target)
                    }
                    None => {
                        let mut lines = literal.lines();
                        lines.next().unwrap_or_default().to_string()
                    }
                },
                Value::Int(handle) => {
                    let mut guard = context.lock().expect("vm context lock poisoned");
                    match decode_edge_io_handle(*handle) {
                        Ok(target) => read_io_target_line(&mut guard, target),
                        Err(_) => read_edge_virtual_handle_line(&mut guard, *handle)?,
                    }
                }
                _ => return Err(VmError::TypeMismatch("string/int")),
            };
            Ok(vec![Value::String(text)])
        })
    }
}

struct BuiltinIoWriteFunction {
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
}

impl BuiltinIoWriteFunction {
    fn new(context: SharedProxyVmContext, async_ops: SharedVmAsyncOps) -> Self {
        Self { context, async_ops }
    }
}

impl HostFunction for BuiltinIoWriteFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 2)?;
        let target_arg = args
            .first()
            .cloned()
            .ok_or(VmError::TypeMismatch("string/int"))?;
        let text = expect_string(args, 1)?;
        let context = self.context.clone();
        schedule_future_call(vm, &self.async_ops, async move {
            tokio::task::yield_now().await;
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
            Ok(vec![Value::Int(text.len() as i64)])
        })
    }
}

struct BuiltinIoFlushFunction {
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
}

impl BuiltinIoFlushFunction {
    fn new(context: SharedProxyVmContext, async_ops: SharedVmAsyncOps) -> Self {
        Self { context, async_ops }
    }
}

impl HostFunction for BuiltinIoFlushFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let target = args
            .first()
            .cloned()
            .ok_or(VmError::TypeMismatch("string/int"))?;
        let context = self.context.clone();
        schedule_future_call(vm, &self.async_ops, async move {
            tokio::task::yield_now().await;
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
            Ok(vec![Value::Bool(true)])
        })
    }
}

struct BuiltinIoCloseFunction {
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
}

impl BuiltinIoCloseFunction {
    fn new(context: SharedProxyVmContext, async_ops: SharedVmAsyncOps) -> Self {
        Self { context, async_ops }
    }
}

impl HostFunction for BuiltinIoCloseFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let target = args
            .first()
            .cloned()
            .ok_or(VmError::TypeMismatch("string/int"))?;
        let context = self.context.clone();
        schedule_future_call(vm, &self.async_ops, async move {
            tokio::task::yield_now().await;
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
            Ok(vec![Value::Bool(true)])
        })
    }
}

struct BuiltinIoExistsFunction {
    async_ops: SharedVmAsyncOps,
}

impl BuiltinIoExistsFunction {
    fn new(async_ops: SharedVmAsyncOps) -> Self {
        Self { async_ops }
    }
}

impl HostFunction for BuiltinIoExistsFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let path = expect_string(args, 0)?;
        schedule_future_call(vm, &self.async_ops, async move {
            tokio::task::yield_now().await;
            let exists = if edge_io_readable_path(&path)
                || edge_io_target_from_string(&path).is_some()
                || path_targets_upstream_request(&path)
            {
                true
            } else {
                tokio::fs::metadata(path.as_str()).await.is_ok()
            };
            Ok(vec![Value::Bool(exists)])
        })
    }
}
