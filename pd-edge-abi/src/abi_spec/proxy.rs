#[pd_host_function(name = "proxy::stream::downstream")]
fn proxy_stream_downstream() -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "proxy::stream::default_upstream")]
fn proxy_stream_default_upstream() -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "proxy::stream::exchange")]
fn proxy_stream_exchange(exchange: i64) -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "proxy::stream::from_tcp")]
fn proxy_stream_from_tcp(stream: i64) -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "proxy::stream::from_tls_plaintext")]
fn proxy_stream_from_tls_plaintext(session: i64) -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "proxy::stream::from_websocket_binary")]
fn proxy_stream_from_websocket_binary(connection: i64) -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "proxy::pipe")]
fn proxy_pipe(source: i64, destination: i64, max_bytes: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "proxy::tunnel")]
fn proxy_tunnel(left: i64, right: i64, max_bytes: i64) -> String {
    unreachable!("abi declaration only")
}
