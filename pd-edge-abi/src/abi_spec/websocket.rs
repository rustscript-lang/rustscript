#[pd_host_function(name = "websocket::connection::new")]
fn websocket_connection_new() -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "websocket::connection::downstream")]
fn websocket_connection_downstream() -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "websocket::connection::default_upstream")]
fn websocket_connection_default_upstream() -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "websocket::connection::is_present")]
fn websocket_connection_is_present(connection: i64) -> bool {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "websocket::connection::set_target")]
fn websocket_connection_set_target(connection: i64, host: &str, port: i64) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "websocket::connection::set_scheme")]
fn websocket_connection_set_scheme(connection: i64, scheme: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "websocket::connection::set_path")]
fn websocket_connection_set_path(connection: i64, path: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "websocket::connection::set_query")]
fn websocket_connection_set_query(connection: i64, query: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "websocket::connection::set_header")]
fn websocket_connection_set_header(connection: i64, name: &str, value: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "websocket::connection::set_subprotocols")]
fn websocket_connection_set_subprotocols(connection: i64, protocols: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "websocket::connection::connect")]
fn websocket_connection_connect(connection: i64) -> bool {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "websocket::connection::get_phase")]
fn websocket_connection_get_phase(connection: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "websocket::connection::get_subprotocol")]
fn websocket_connection_get_subprotocol(connection: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "websocket::connection::send_text")]
fn websocket_connection_send_text(connection: i64, text: &str) -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "websocket::connection::read_text")]
fn websocket_connection_read_text(connection: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "websocket::connection::send_binary_base64")]
fn websocket_connection_send_binary_base64(connection: i64, payload: &str) -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "websocket::connection::read_binary_base64")]
fn websocket_connection_read_binary_base64(connection: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "websocket::connection::eof")]
fn websocket_connection_eof(connection: i64) -> bool {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "websocket::connection::close")]
fn websocket_connection_close(connection: i64, code: i64, reason: &str) {
    unreachable!("abi declaration only")
}
