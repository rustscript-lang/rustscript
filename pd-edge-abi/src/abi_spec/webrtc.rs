#[pd_host_function(name = "webrtc::connection::new")]
fn webrtc_connection_new() -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "webrtc::connection::downstream")]
fn webrtc_connection_downstream() -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "webrtc::connection::default_upstream")]
fn webrtc_connection_default_upstream() -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "webrtc::connection::is_present")]
fn webrtc_connection_is_present(connection: i64) -> bool {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "webrtc::connection::set_ice_servers")]
fn webrtc_connection_set_ice_servers(connection: i64, urls: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "webrtc::connection::set_data_channel_label")]
fn webrtc_connection_set_data_channel_label(connection: i64, label: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "webrtc::connection::set_remote_description")]
fn webrtc_connection_set_remote_description(connection: i64, description_json: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "webrtc::connection::create_offer")]
fn webrtc_connection_create_offer(connection: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "webrtc::connection::create_answer")]
fn webrtc_connection_create_answer(connection: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "webrtc::connection::connect")]
fn webrtc_connection_connect(connection: i64) -> bool {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "webrtc::connection::get_phase")]
fn webrtc_connection_get_phase(connection: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "webrtc::connection::send_text")]
fn webrtc_connection_send_text(connection: i64, text: &str) -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "webrtc::connection::read_text")]
fn webrtc_connection_read_text(connection: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "webrtc::connection::send_binary_base64")]
fn webrtc_connection_send_binary_base64(connection: i64, payload: &str) -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "webrtc::connection::send_binary")]
fn webrtc_connection_send_binary(connection: i64, payload: Bytes) -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "webrtc::connection::read_binary_base64")]
fn webrtc_connection_read_binary_base64(connection: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "webrtc::connection::read_binary")]
fn webrtc_connection_read_binary(connection: i64) -> Bytes {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "webrtc::connection::eof")]
fn webrtc_connection_eof(connection: i64) -> bool {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "webrtc::connection::close")]
fn webrtc_connection_close(connection: i64) {
    unreachable!("abi declaration only")
}
