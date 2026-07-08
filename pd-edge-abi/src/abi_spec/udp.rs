/// Host function `udp::socket::new`.
#[pd_host_function(name = "udp::socket::new")]
fn udp_socket_new() -> i64 {
    unreachable!("abi declaration only")
}

/// Host function `udp::socket::downstream`.
#[pd_host_function(name = "udp::socket::downstream")]
fn udp_socket_downstream() -> i64 {
    unreachable!("abi declaration only")
}

/// Host function `udp::socket::default_upstream`.
#[pd_host_function(name = "udp::socket::default_upstream")]
fn udp_socket_default_upstream() -> i64 {
    unreachable!("abi declaration only")
}

/// Host function `udp::socket::is_present`.
#[pd_host_function(name = "udp::socket::is_present")]
fn udp_socket_is_present(socket: i64) -> bool {
    unreachable!("abi declaration only")
}

/// Host function `udp::socket::bind`.
#[pd_host_function(name = "udp::socket::bind")]
fn udp_socket_bind(socket: i64, local_addr: &str) {
    unreachable!("abi declaration only")
}

/// Host function `udp::socket::set_target`.
#[pd_host_function(name = "udp::socket::set_target")]
fn udp_socket_set_target(socket: i64, host: &str, port: i64) {
    unreachable!("abi declaration only")
}

/// Host function `udp::socket::connect`.
#[pd_host_function(name = "udp::socket::connect")]
fn udp_socket_connect(socket: i64) -> bool {
    unreachable!("abi declaration only")
}

/// Host function `udp::socket::get_phase`.
#[pd_host_function(name = "udp::socket::get_phase")]
fn udp_socket_get_phase(socket: i64) -> String {
    unreachable!("abi declaration only")
}

/// Host function `udp::socket::get_local_addr`.
#[pd_host_function(name = "udp::socket::get_local_addr")]
fn udp_socket_get_local_addr(socket: i64) -> String {
    unreachable!("abi declaration only")
}

/// Host function `udp::socket::get_peer_addr`.
#[pd_host_function(name = "udp::socket::get_peer_addr")]
fn udp_socket_get_peer_addr(socket: i64) -> String {
    unreachable!("abi declaration only")
}

/// Host function `udp::socket::send_text`.
#[pd_host_function(name = "udp::socket::send_text")]
fn udp_socket_send_text(socket: i64, text: &str) -> i64 {
    unreachable!("abi declaration only")
}

/// Host function `udp::socket::recv_text`.
#[pd_host_function(name = "udp::socket::recv_text")]
fn udp_socket_recv_text(socket: i64, max_bytes: i64) -> String {
    unreachable!("abi declaration only")
}

/// Host function `udp::socket::send_binary`.
#[pd_host_function(name = "udp::socket::send_binary")]
fn udp_socket_send_binary(socket: i64, payload: Bytes) -> i64 {
    unreachable!("abi declaration only")
}

/// Host function `udp::socket::recv_binary`.
#[pd_host_function(name = "udp::socket::recv_binary")]
fn udp_socket_recv_binary(socket: i64, max_bytes: i64) -> Bytes {
    unreachable!("abi declaration only")
}

/// Host function `udp::socket::send_binary_base64`.
#[pd_host_function(name = "udp::socket::send_binary_base64")]
fn udp_socket_send_binary_base64(socket: i64, payload: &str) -> i64 {
    unreachable!("abi declaration only")
}

/// Host function `udp::socket::recv_binary_base64`.
#[pd_host_function(name = "udp::socket::recv_binary_base64")]
fn udp_socket_recv_binary_base64(socket: i64, max_bytes: i64) -> String {
    unreachable!("abi declaration only")
}

/// Host function `udp::socket::close`.
#[pd_host_function(name = "udp::socket::close")]
fn udp_socket_close(socket: i64) {
    unreachable!("abi declaration only")
}
