#[pd_host_function(name = "tcp::stream::downstream")]
fn tcp_stream_downstream() -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tcp::stream::default_upstream")]
fn tcp_stream_default_upstream() -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tcp::stream::new")]
fn tcp_stream_new() -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tcp::stream::is_present")]
fn tcp_stream_is_present(stream: i64) -> bool {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tcp::stream::bind")]
fn tcp_stream_bind(stream: i64, local_addr: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tcp::stream::set_target")]
fn tcp_stream_set_target(stream: i64, host: &str, port: i64) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tcp::stream::connect")]
fn tcp_stream_connect(stream: i64) -> bool {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tcp::stream::get_phase")]
fn tcp_stream_get_phase(stream: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tcp::stream::get_local_addr")]
fn tcp_stream_get_local_addr(stream: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tcp::stream::get_peer_addr")]
fn tcp_stream_get_peer_addr(stream: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tcp::stream::read")]
fn tcp_stream_read(stream: i64, max_bytes: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tcp::stream::peek")]
fn tcp_stream_peek(stream: i64, max_bytes: i64) -> String {
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

#[pd_host_function(name = "tcp::stream::close")]
fn tcp_stream_close(stream: i64) {
    unreachable!("abi declaration only")
}
