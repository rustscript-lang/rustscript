#[pd_host_function(name = "udp::socket::new")]
fn udp_socket_new() -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "udp::socket::downstream")]
fn udp_socket_downstream() -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "udp::socket::default_upstream")]
fn udp_socket_default_upstream() -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "udp::socket::is_present")]
fn udp_socket_is_present(socket: i64) -> bool {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "udp::socket::bind")]
fn udp_socket_bind(socket: i64, local_addr: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "udp::socket::set_target")]
fn udp_socket_set_target(socket: i64, target: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "udp::socket::connect")]
fn udp_socket_connect(socket: i64) -> bool {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "udp::socket::get_phase")]
fn udp_socket_get_phase(socket: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "udp::socket::get_local_addr")]
fn udp_socket_get_local_addr(socket: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "udp::socket::get_peer_addr")]
fn udp_socket_get_peer_addr(socket: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "udp::socket::send_text")]
fn udp_socket_send_text(socket: i64, text: &str) -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "udp::socket::recv_text")]
fn udp_socket_recv_text(socket: i64, max_bytes: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "udp::socket::send_binary_base64")]
fn udp_socket_send_binary_base64(socket: i64, payload: &str) -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "udp::socket::recv_binary_base64")]
fn udp_socket_recv_binary_base64(socket: i64, max_bytes: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "udp::socket::close")]
fn udp_socket_close(socket: i64) {
    unreachable!("abi declaration only")
}
