#[pd_host_function(name = "tcp::stream::downstream")]
fn tcp_stream_downstream() -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tcp::stream::default_upstream")]
fn tcp_stream_default_upstream() -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tcp::stream::read")]
fn tcp_stream_read(stream: i64, max_bytes: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tcp::stream::write")]
fn tcp_stream_write(stream: i64, text: &str) -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tcp::stream::eof")]
fn tcp_stream_eof(stream: i64) -> bool {
    unreachable!("abi declaration only")
}
