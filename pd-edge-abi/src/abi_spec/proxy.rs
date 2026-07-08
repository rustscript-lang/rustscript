/// Host function `proxy::stream::downstream`.
#[pd_host_function(name = "proxy::stream::downstream")]
fn proxy_stream_downstream() -> i64 {
    unreachable!("abi declaration only")
}

/// Host function `proxy::stream::exchange`.
#[pd_host_function(name = "proxy::stream::exchange")]
fn proxy_stream_exchange(exchange: i64) -> i64 {
    unreachable!("abi declaration only")
}

/// Host function `proxy::stream::from_tcp`.
#[pd_host_function(name = "proxy::stream::from_tcp")]
fn proxy_stream_from_tcp(stream: i64) -> i64 {
    unreachable!("abi declaration only")
}

/// Host function `proxy::stream::from_tls_plaintext`.
#[pd_host_function(name = "proxy::stream::from_tls_plaintext")]
fn proxy_stream_from_tls_plaintext(session: i64) -> i64 {
    unreachable!("abi declaration only")
}

/// Host function `proxy::stream::from_websocket_binary`.
#[pd_host_function(name = "proxy::stream::from_websocket_binary")]
fn proxy_stream_from_websocket_binary(connection: i64) -> i64 {
    unreachable!("abi declaration only")
}

/// Host function `proxy::pipe`.
#[pd_host_function(name = "proxy::pipe")]
fn proxy_pipe(source: i64, destination: i64, max_bytes: i64) -> String {
    unreachable!("abi declaration only")
}

/// Host function `proxy::forward`.
#[pd_host_function(name = "proxy::forward")]
fn proxy_forward(left: i64, right: i64, max_bytes: i64) -> String {
    unreachable!("abi declaration only")
}

/// Host function `proxy::forward_native`.
#[pd_host_function(name = "proxy::forward_native")]
fn proxy_forward_native(left: i64, right: i64) -> String {
    unreachable!("abi declaration only")
}
