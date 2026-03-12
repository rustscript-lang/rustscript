use edge_abi::symbols::tcp;
use pd_edge_host_function::pd_edge_host_function;
use vm::{CallOutcome, Value, Vm, VmError};

use super::super::SharedProxyVmContext;
use super::super::http::{
    append_outbound_exchange_body, append_response_output_body_bytes, outbound_exchange_exists,
    outbound_exchange_response_eof, read_outbound_exchange_response_next_chunk,
    read_request_body_next_chunk, read_upstream_response_next_chunk, request_body_eof,
    upstream_response_eof,
};
use super::state::{TcpStreamRef, decode_tcp_stream_handle};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TcpStreamHandle {
    Reserved(TcpStreamRef),
    OutboundExchange(i64),
}

fn decode_stream(context: &SharedProxyVmContext, stream: i64) -> Result<TcpStreamHandle, VmError> {
    if let Some(reserved) = decode_tcp_stream_handle(stream) {
        return Ok(TcpStreamHandle::Reserved(reserved));
    }
    if outbound_exchange_exists(context, stream) {
        return Ok(TcpStreamHandle::OutboundExchange(stream));
    }
    Err(VmError::HostError(format!(
        "invalid tcp stream handle {stream}; reserved handles are 0 (downstream), 1 (default upstream), and allocated outbound exchange handles start at 2",
    )))
}

fn decode_chunk_size(max_bytes: i64) -> Result<usize, VmError> {
    if max_bytes <= 0 {
        return Err(VmError::HostError(format!(
            "tcp::stream::read max_bytes must be positive, got {max_bytes}",
        )));
    }
    usize::try_from(max_bytes).map_err(|_| {
        VmError::HostError(format!(
            "tcp::stream::read max_bytes is too large for this runtime: {max_bytes}",
        ))
    })
}

fn note_stream_read(context: &SharedProxyVmContext, stream: TcpStreamHandle) {
    let mut guard = context.lock().expect("vm context lock poisoned");
    match stream {
        TcpStreamHandle::Reserved(TcpStreamRef::Downstream) => guard.tcp_dag.downstream.note_read(),
        TcpStreamHandle::Reserved(TcpStreamRef::DefaultUpstream) => {
            guard.tcp_dag.default_upstream.note_read()
        }
        TcpStreamHandle::OutboundExchange(handle) => {
            let exchange = guard
                .outbound_exchanges
                .get_mut(&handle)
                .expect("exchange handle should exist while stream is in use");
            exchange.transport.tcp_flow.note_read();
        }
    }
}

fn append_downstream_response(context: &SharedProxyVmContext, text: &str) {
    append_response_output_body_bytes(context, text.as_bytes());
}

#[pd_edge_host_function(name = tcp::stream::DOWNSTREAM.name, scope = transport)]
async fn stream_downstream(
    _vm: &mut Vm,
    _context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::Int(
        TcpStreamRef::Downstream.handle(),
    )]))
}

#[pd_edge_host_function(name = tcp::stream::DEFAULT_UPSTREAM.name, scope = transport)]
async fn stream_default_upstream(
    _vm: &mut Vm,
    _context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::Int(
        TcpStreamRef::DefaultUpstream.handle(),
    )]))
}

#[pd_edge_host_function(name = tcp::stream::READ.name, scope = transport)]
async fn stream_read(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    stream: i64,
    max_bytes: i64,
) -> Result<CallOutcome, VmError> {
    let stream = decode_stream(&context, stream)?;
    let max_bytes = decode_chunk_size(max_bytes)?;
    note_stream_read(&context, stream);

    let chunk = match stream {
        TcpStreamHandle::Reserved(TcpStreamRef::Downstream) => {
            read_request_body_next_chunk(&context, max_bytes).await?
        }
        TcpStreamHandle::Reserved(TcpStreamRef::DefaultUpstream) => {
            read_upstream_response_next_chunk(&context, max_bytes).await?
        }
        TcpStreamHandle::OutboundExchange(handle) => {
            read_outbound_exchange_response_next_chunk(&context, handle, max_bytes).await?
        }
    };
    Ok(CallOutcome::Return(vec![Value::string(
        String::from_utf8_lossy(&chunk).into_owned(),
    )]))
}

#[pd_edge_host_function(name = tcp::stream::WRITE.name, scope = transport)]
async fn stream_write(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    stream: i64,
    text: String,
) -> Result<CallOutcome, VmError> {
    match decode_stream(&context, stream)? {
        TcpStreamHandle::Reserved(TcpStreamRef::Downstream) => {
            append_downstream_response(&context, &text)
        }
        TcpStreamHandle::Reserved(TcpStreamRef::DefaultUpstream) => {
            append_outbound_exchange_body(&context, TcpStreamRef::DefaultUpstream.handle(), &text)?
        }
        TcpStreamHandle::OutboundExchange(handle) => {
            append_outbound_exchange_body(&context, handle, &text)?
        }
    }
    Ok(CallOutcome::Return(vec![Value::Int(text.len() as i64)]))
}

#[pd_edge_host_function(name = tcp::stream::EOF.name, scope = transport)]
async fn stream_eof(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    stream: i64,
) -> Result<CallOutcome, VmError> {
    let eof = match decode_stream(&context, stream)? {
        TcpStreamHandle::Reserved(TcpStreamRef::Downstream) => request_body_eof(&context).await?,
        TcpStreamHandle::Reserved(TcpStreamRef::DefaultUpstream) => {
            upstream_response_eof(&context).await?
        }
        TcpStreamHandle::OutboundExchange(handle) => {
            outbound_exchange_response_eof(&context, handle).await?
        }
    };
    Ok(CallOutcome::Return(vec![Value::Bool(eof)]))
}
