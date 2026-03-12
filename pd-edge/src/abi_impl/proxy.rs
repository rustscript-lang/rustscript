use std::time::Duration;

use edge_abi::symbols::proxy as proxy_symbols;
use pd_edge_host_function::pd_edge_host_function;
use tokio::{task::yield_now, time::sleep, try_join};
use vm::{CallOutcome, Value, Vm, VmError};

use super::{
    SharedProxyVmContext, SharedVmAsyncOps, http, registry,
    transport::{TcpStreamRef, TlsSessionRef, decode_tcp_stream_handle, decode_tls_session_handle},
    websocket::{
        close_websocket_binary_stream, read_websocket_binary_bytes,
        validate_outbound_websocket_binary_connection, write_websocket_binary_bytes,
    },
};

const BLOCKED_RETRY_DELAY: Duration = Duration::from_millis(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProxyByteStreamEndpoint {
    HttpDownstream,
    HttpExchange(i64),
    WebSocketBinary(i64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProxyByteStreamState {
    endpoint: ProxyByteStreamEndpoint,
    write_observed: bool,
    write_closed: bool,
}

impl ProxyByteStreamState {
    fn new(endpoint: ProxyByteStreamEndpoint) -> Self {
        Self {
            endpoint,
            write_observed: false,
            write_closed: false,
        }
    }
}

enum ProxyReadStep {
    Data(Vec<u8>),
    Eof,
    Blocked,
}

pub(super) fn register_proxy_extensions(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    registry::register_host_scope(vm, &context, &async_ops, registry::EdgeHostScope::Proxy);
}

fn unknown_proxy_stream_handle(handle: i64) -> VmError {
    VmError::HostError(format!("unknown proxy byte-stream handle {handle}"))
}

fn decode_chunk_size(max_bytes: i64) -> Result<usize, VmError> {
    if max_bytes <= 0 {
        return Err(VmError::HostError(format!(
            "proxy chunk size must be positive, got {max_bytes}",
        )));
    }
    usize::try_from(max_bytes).map_err(|_| {
        VmError::HostError(format!(
            "proxy chunk size is too large for this runtime: {max_bytes}",
        ))
    })
}

fn allocate_proxy_stream_handle(
    context: &SharedProxyVmContext,
    endpoint: ProxyByteStreamEndpoint,
) -> Result<i64, VmError> {
    let mut guard = context.lock().expect("vm context lock poisoned");
    let handle = guard.next_proxy_stream_handle;
    if handle == i64::MAX {
        return Err(VmError::HostError(
            "proxy byte-stream handle space exhausted".to_string(),
        ));
    }
    guard.next_proxy_stream_handle = handle.saturating_add(1);
    guard
        .proxy_stream_handles
        .insert(handle, ProxyByteStreamState::new(endpoint));
    Ok(handle)
}

fn proxy_stream_state(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<ProxyByteStreamState, VmError> {
    let guard = context.lock().expect("vm context lock poisoned");
    guard
        .proxy_stream_handles
        .get(&handle)
        .cloned()
        .ok_or_else(|| unknown_proxy_stream_handle(handle))
}

fn prepare_proxy_stream_write(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<ProxyByteStreamEndpoint, VmError> {
    let mut guard = context.lock().expect("vm context lock poisoned");
    let stream = guard
        .proxy_stream_handles
        .get_mut(&handle)
        .ok_or_else(|| unknown_proxy_stream_handle(handle))?;
    if stream.write_closed {
        return Err(VmError::HostError(format!(
            "proxy byte-stream handle {handle} is write-closed",
        )));
    }
    stream.write_observed = true;
    Ok(stream.endpoint.clone())
}

fn mark_proxy_stream_write_closed(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<ProxyByteStreamEndpoint, VmError> {
    let mut guard = context.lock().expect("vm context lock poisoned");
    let stream = guard
        .proxy_stream_handles
        .get_mut(&handle)
        .ok_or_else(|| unknown_proxy_stream_handle(handle))?;
    stream.write_closed = true;
    Ok(stream.endpoint.clone())
}

fn endpoint_from_tcp_stream(
    context: &SharedProxyVmContext,
    stream: i64,
) -> Result<ProxyByteStreamEndpoint, VmError> {
    if let Some(stream_ref) = decode_tcp_stream_handle(stream) {
        return Ok(match stream_ref {
            TcpStreamRef::Downstream => ProxyByteStreamEndpoint::HttpDownstream,
            TcpStreamRef::DefaultUpstream => {
                ProxyByteStreamEndpoint::HttpExchange(http::default_upstream_exchange_handle())
            }
        });
    }
    if http::outbound_exchange_exists(context, stream) {
        return Ok(ProxyByteStreamEndpoint::HttpExchange(stream));
    }
    Err(VmError::HostError(format!(
        "invalid tcp stream handle {stream}; expected 0 (downstream), 1 (default upstream), or an allocated outbound exchange handle",
    )))
}

fn tls_present_for_endpoint(
    context: &SharedProxyVmContext,
    endpoint: &ProxyByteStreamEndpoint,
) -> Result<bool, VmError> {
    match endpoint {
        ProxyByteStreamEndpoint::HttpDownstream => {
            let guard = context.lock().expect("vm context lock poisoned");
            Ok(guard.tls_dag.downstream.is_present())
        }
        ProxyByteStreamEndpoint::HttpExchange(handle) => {
            Ok(http::outbound_exchange_tls_flow(context, *handle)?.is_present())
        }
        ProxyByteStreamEndpoint::WebSocketBinary(_) => Ok(false),
    }
}

fn endpoint_from_tls_plaintext(
    context: &SharedProxyVmContext,
    session: i64,
) -> Result<ProxyByteStreamEndpoint, VmError> {
    let endpoint = if let Some(session_ref) = decode_tls_session_handle(session) {
        match session_ref {
            TlsSessionRef::Downstream => ProxyByteStreamEndpoint::HttpDownstream,
            TlsSessionRef::DefaultUpstream => {
                ProxyByteStreamEndpoint::HttpExchange(http::default_upstream_exchange_handle())
            }
        }
    } else if http::outbound_exchange_exists(context, session) {
        ProxyByteStreamEndpoint::HttpExchange(session)
    } else {
        return Err(VmError::HostError(format!(
            "invalid tls session handle {session}; expected 0 (downstream), 1 (default upstream), or an allocated outbound exchange handle",
        )));
    };

    if !tls_present_for_endpoint(context, &endpoint)? {
        return Err(VmError::HostError(format!(
            "tls plaintext stream is unavailable for handle {session} before a TLS session is present",
        )));
    }

    Ok(endpoint)
}

fn endpoint_from_websocket_binary(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<ProxyByteStreamEndpoint, VmError> {
    validate_outbound_websocket_binary_connection(context, connection)?;
    if connection != http::default_upstream_exchange_handle()
        && !http::outbound_exchange_exists(context, connection)
    {
        return Err(VmError::HostError(format!(
            "invalid websocket connection handle {connection}; expected 1 (default upstream) or an allocated outbound exchange handle",
        )));
    }
    Ok(ProxyByteStreamEndpoint::WebSocketBinary(connection))
}

async fn proxy_stream_read_step(
    context: &SharedProxyVmContext,
    handle: i64,
    max_bytes: usize,
) -> Result<ProxyReadStep, VmError> {
    let stream = proxy_stream_state(context, handle)?;
    match stream.endpoint {
        ProxyByteStreamEndpoint::HttpDownstream => {
            let chunk = http::read_request_body_next_chunk(context, max_bytes).await?;
            if chunk.is_empty() {
                Ok(ProxyReadStep::Eof)
            } else {
                Ok(ProxyReadStep::Data(chunk))
            }
        }
        ProxyByteStreamEndpoint::HttpExchange(exchange) => {
            if !http::outbound_exchange_response_available(context, exchange)
                && stream.write_observed
                && !stream.write_closed
            {
                return Ok(ProxyReadStep::Blocked);
            }
            let chunk =
                http::read_outbound_exchange_response_next_chunk(context, exchange, max_bytes)
                    .await?;
            if chunk.is_empty() {
                if http::outbound_exchange_response_eof(context, exchange).await? {
                    Ok(ProxyReadStep::Eof)
                } else {
                    Ok(ProxyReadStep::Blocked)
                }
            } else {
                Ok(ProxyReadStep::Data(chunk))
            }
        }
        ProxyByteStreamEndpoint::WebSocketBinary(connection) => {
            match read_websocket_binary_bytes(context, connection).await? {
                Some(bytes) => Ok(ProxyReadStep::Data(bytes)),
                None => Ok(ProxyReadStep::Eof),
            }
        }
    }
}

async fn proxy_stream_write_bytes(
    context: &SharedProxyVmContext,
    handle: i64,
    bytes: &[u8],
) -> Result<(), VmError> {
    if bytes.is_empty() {
        return Ok(());
    }
    let endpoint = prepare_proxy_stream_write(context, handle)?;
    match endpoint {
        ProxyByteStreamEndpoint::HttpDownstream => {
            http::append_response_output_body_bytes(context, bytes);
            Ok(())
        }
        ProxyByteStreamEndpoint::HttpExchange(exchange) => {
            http::append_outbound_exchange_body_bytes(context, exchange, bytes)
        }
        ProxyByteStreamEndpoint::WebSocketBinary(connection) => {
            write_websocket_binary_bytes(context, connection, bytes).await?;
            Ok(())
        }
    }
}

async fn proxy_stream_close_write(
    context: &SharedProxyVmContext,
    handle: i64,
) -> Result<(), VmError> {
    let endpoint = mark_proxy_stream_write_closed(context, handle)?;
    if let ProxyByteStreamEndpoint::WebSocketBinary(connection) = endpoint {
        close_websocket_binary_stream(context, connection).await?;
    }
    Ok(())
}

async fn drive_pipe_direction(
    context: SharedProxyVmContext,
    source: i64,
    destination: i64,
    max_bytes: usize,
) -> Result<(), VmError> {
    loop {
        match proxy_stream_read_step(&context, source, max_bytes).await? {
            ProxyReadStep::Data(chunk) => {
                if chunk.is_empty() {
                    continue;
                }
                proxy_stream_write_bytes(&context, destination, &chunk).await?;
                yield_now().await;
            }
            ProxyReadStep::Eof => {
                proxy_stream_close_write(&context, destination).await?;
                return Ok(());
            }
            ProxyReadStep::Blocked => {
                sleep(BLOCKED_RETRY_DELAY).await;
            }
        }
    }
}

async fn drive_pipe(
    context: SharedProxyVmContext,
    source: i64,
    destination: i64,
    max_bytes: usize,
) -> Result<String, VmError> {
    let source_state = proxy_stream_state(&context, source)?;
    if let ProxyByteStreamEndpoint::HttpExchange(exchange) = source_state.endpoint
        && !http::outbound_exchange_response_available(&context, exchange)
        && source_state.write_observed
        && !source_state.write_closed
    {
        return Err(VmError::HostError(format!(
            "proxy byte-stream handle {source} cannot be piped yet because its read side is waiting for its write side to close; use proxy::tunnel or finish writing before piping from it",
        )));
    }
    drive_pipe_direction(context, source, destination, max_bytes).await?;
    Ok("eof".to_string())
}

async fn drive_tunnel(
    context: SharedProxyVmContext,
    left: i64,
    right: i64,
    max_bytes: usize,
) -> Result<String, VmError> {
    try_join!(
        drive_pipe_direction(context.clone(), left, right, max_bytes),
        drive_pipe_direction(context, right, left, max_bytes)
    )?;
    Ok("closed".to_string())
}

#[pd_edge_host_function(name = proxy_symbols::stream::DOWNSTREAM.name, scope = proxy)]
async fn stream_downstream(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let handle = allocate_proxy_stream_handle(&context, ProxyByteStreamEndpoint::HttpDownstream)?;
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

#[pd_edge_host_function(name = proxy_symbols::stream::DEFAULT_UPSTREAM.name, scope = proxy)]
async fn stream_default_upstream(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let handle = allocate_proxy_stream_handle(
        &context,
        ProxyByteStreamEndpoint::HttpExchange(http::default_upstream_exchange_handle()),
    )?;
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

#[pd_edge_host_function(name = proxy_symbols::stream::EXCHANGE.name, scope = proxy)]
async fn stream_exchange(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    exchange: i64,
) -> Result<CallOutcome, VmError> {
    if !http::outbound_exchange_exists(&context, exchange) {
        return Err(VmError::HostError(format!(
            "unknown outbound exchange handle {exchange}",
        )));
    }
    let handle =
        allocate_proxy_stream_handle(&context, ProxyByteStreamEndpoint::HttpExchange(exchange))?;
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

#[pd_edge_host_function(name = proxy_symbols::stream::FROM_TCP.name, scope = proxy)]
async fn stream_from_tcp(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    stream: i64,
) -> Result<CallOutcome, VmError> {
    let endpoint = endpoint_from_tcp_stream(&context, stream)?;
    let handle = allocate_proxy_stream_handle(&context, endpoint)?;
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

#[pd_edge_host_function(name = proxy_symbols::stream::FROM_TLS_PLAINTEXT.name, scope = proxy)]
async fn stream_from_tls_plaintext(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    session: i64,
) -> Result<CallOutcome, VmError> {
    let endpoint = endpoint_from_tls_plaintext(&context, session)?;
    let handle = allocate_proxy_stream_handle(&context, endpoint)?;
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

#[pd_edge_host_function(name = proxy_symbols::stream::FROM_WEBSOCKET_BINARY.name, scope = proxy)]
async fn stream_from_websocket_binary(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    let endpoint = endpoint_from_websocket_binary(&context, connection)?;
    let handle = allocate_proxy_stream_handle(&context, endpoint)?;
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

#[pd_edge_host_function(name = proxy_symbols::PIPE.name, scope = proxy)]
async fn proxy_pipe(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    source: i64,
    destination: i64,
    max_bytes: i64,
) -> Result<CallOutcome, VmError> {
    let max_bytes = decode_chunk_size(max_bytes)?;
    let status = drive_pipe(context, source, destination, max_bytes).await?;
    Ok(CallOutcome::Return(vec![Value::string(status)]))
}

#[pd_edge_host_function(name = proxy_symbols::TUNNEL.name, scope = proxy)]
async fn proxy_tunnel(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    left: i64,
    right: i64,
    max_bytes: i64,
) -> Result<CallOutcome, VmError> {
    let max_bytes = decode_chunk_size(max_bytes)?;
    let status = drive_tunnel(context, left, right, max_bytes).await?;
    Ok(CallOutcome::Return(vec![Value::string(status)]))
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{Arc, Mutex},
    };

    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{HeaderMap, Request},
        routing::any,
    };
    use reqwest::Client;

    use super::{
        ProxyByteStreamEndpoint, ProxyReadStep, allocate_proxy_stream_handle, drive_pipe,
        drive_tunnel, endpoint_from_tls_plaintext, proxy_stream_close_write,
        proxy_stream_read_step, proxy_stream_write_bytes,
    };
    use crate::abi_impl::{
        RateLimiterStore,
        http::{self as edge_http, HttpRequestContext, ProxyVmContext, SharedProxyVmContext},
        transport::TlsSessionRef,
    };

    fn test_context(body: &str) -> SharedProxyVmContext {
        Arc::new(Mutex::new(ProxyVmContext::from_http_request(
            HttpRequestContext {
                request_id: "req-1".to_string(),
                method: axum::http::Method::POST,
                path: "/".to_string(),
                query: String::new(),
                http_version: "1.1".to_string(),
                port: 80,
                scheme: "http".to_string(),
                host: "example.com".to_string(),
                client_ip: "127.0.0.1".to_string(),
                body: Body::from(body.to_string()),
                headers: HeaderMap::new(),
            },
            Arc::new(Mutex::new(RateLimiterStore::new())),
        )))
    }

    async fn spawn_server(app: Router) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("listener should have addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server should run");
        });
        addr
    }

    fn configure_default_upstream(context: &SharedProxyVmContext, target: String, client: Client) {
        let mut guard = context.lock().expect("vm context lock poisoned");
        guard.attach_upstream_client(client);
        guard.outbound_request.target = Some(target.clone());
        guard.tcp_dag.default_upstream.configure();
        guard.tls_dag.default_upstream.observe_target(&target);
    }

    #[test]
    fn proxy_stream_handles_allocate_in_dynamic_range() {
        let context = test_context("");
        let first = allocate_proxy_stream_handle(&context, ProxyByteStreamEndpoint::HttpDownstream)
            .expect("first stream should allocate");
        let second = allocate_proxy_stream_handle(
            &context,
            ProxyByteStreamEndpoint::HttpExchange(edge_http::default_upstream_exchange_handle()),
        )
        .expect("second stream should allocate");

        assert!(first >= 4096);
        assert_eq!(second, first + 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn http_exchange_source_blocks_until_proxy_write_side_is_closed() {
        let upstream_addr = spawn_server(Router::new().fallback(any(
            |request: Request<Body>| async move {
                let body = to_bytes(request.into_body(), usize::MAX)
                    .await
                    .expect("body should read");
                Body::from(format!("echo:{}", String::from_utf8_lossy(&body)))
            },
        )))
        .await;

        let context = test_context("");
        configure_default_upstream(
            &context,
            format!("http://{upstream_addr}/echo"),
            Client::new(),
        );

        let upstream = allocate_proxy_stream_handle(
            &context,
            ProxyByteStreamEndpoint::HttpExchange(edge_http::default_upstream_exchange_handle()),
        )
        .expect("upstream stream should allocate");
        proxy_stream_write_bytes(&context, upstream, b"payload")
            .await
            .expect("proxy write should succeed");

        assert!(matches!(
            proxy_stream_read_step(&context, upstream, 64)
                .await
                .expect("read step should succeed"),
            ProxyReadStep::Blocked
        ));

        proxy_stream_close_write(&context, upstream)
            .await
            .expect("write close should succeed");
        let next = proxy_stream_read_step(&context, upstream, 64)
            .await
            .expect("read step should succeed");
        match next {
            ProxyReadStep::Data(chunk) => {
                assert_eq!(String::from_utf8_lossy(&chunk), "echo:payload");
            }
            ProxyReadStep::Eof | ProxyReadStep::Blocked => {
                panic!("expected upstream body bytes after write close")
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tunnel_round_trips_request_body_through_default_upstream_stream() {
        let upstream_addr = spawn_server(Router::new().fallback(any(
            |request: Request<Body>| async move {
                let body = to_bytes(request.into_body(), usize::MAX)
                    .await
                    .expect("body should read");
                Body::from(format!("echo:{}", String::from_utf8_lossy(&body)))
            },
        )))
        .await;

        let context = test_context("abcdefgh");
        configure_default_upstream(
            &context,
            format!("http://{upstream_addr}/echo"),
            Client::new(),
        );

        let downstream =
            allocate_proxy_stream_handle(&context, ProxyByteStreamEndpoint::HttpDownstream)
                .expect("downstream stream should allocate");
        let upstream = allocate_proxy_stream_handle(
            &context,
            ProxyByteStreamEndpoint::HttpExchange(edge_http::default_upstream_exchange_handle()),
        )
        .expect("upstream stream should allocate");

        let status = drive_tunnel(context.clone(), downstream, upstream, 3)
            .await
            .expect("tunnel should succeed");
        assert_eq!(status, "closed");

        let guard = context.lock().expect("vm context lock poisoned");
        assert_eq!(
            guard.response_output.body.as_deref(),
            Some("echo:abcdefgh".as_bytes())
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pipe_can_forward_dynamic_exchange_response_without_proxy_writes() {
        let upstream_addr = spawn_server(Router::new().fallback(any(
            |request: Request<Body>| async move {
                let body = to_bytes(request.into_body(), usize::MAX)
                    .await
                    .expect("body should read");
                Body::from(format!("dyn:{}", String::from_utf8_lossy(&body)))
            },
        )))
        .await;

        let context = test_context("");
        {
            let mut guard = context.lock().expect("vm context lock poisoned");
            guard.attach_upstream_client(Client::new());
        }
        let exchange = edge_http::allocate_outbound_exchange_handle(&context)
            .expect("exchange should allocate");
        {
            let mut guard = context.lock().expect("vm context lock poisoned");
            let exchange_state = guard
                .outbound_exchanges
                .get_mut(&exchange)
                .expect("exchange should exist");
            exchange_state.request.target = Some(format!("http://{upstream_addr}/dyn"));
            exchange_state.request.body_override = Some(b"payload".to_vec());
            exchange_state.tcp_dag.configure();
            exchange_state
                .tls_dag
                .observe_target(&format!("http://{upstream_addr}/dyn"));
        }

        let source =
            allocate_proxy_stream_handle(&context, ProxyByteStreamEndpoint::HttpExchange(exchange))
                .expect("source stream should allocate");
        let downstream =
            allocate_proxy_stream_handle(&context, ProxyByteStreamEndpoint::HttpDownstream)
                .expect("downstream stream should allocate");

        let status = drive_pipe(context.clone(), source, downstream, 64)
            .await
            .expect("pipe should succeed");
        assert_eq!(status, "eof");

        let guard = context.lock().expect("vm context lock poisoned");
        assert_eq!(
            guard.response_output.body.as_deref(),
            Some("dyn:payload".as_bytes())
        );
    }

    #[test]
    fn tls_plaintext_stream_requires_tls_presence() {
        let context = test_context("");
        let error = endpoint_from_tls_plaintext(&context, TlsSessionRef::Downstream.handle())
            .expect_err("plaintext stream should reject plain downstream");
        assert!(
            error
                .to_string()
                .contains("tls plaintext stream is unavailable")
        );
    }
}
