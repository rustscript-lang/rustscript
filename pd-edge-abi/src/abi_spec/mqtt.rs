/// Host function `mqtt::connection::new`.
#[pd_host_function(name = "mqtt::connection::new")]
fn mqtt_connection_new() -> i64 {
    unreachable!("abi declaration only")
}

/// Host function `mqtt::connection::default_upstream`.
#[pd_host_function(name = "mqtt::connection::default_upstream")]
fn mqtt_connection_default_upstream() -> i64 {
    unreachable!("abi declaration only")
}

/// Host function `mqtt::connection::is_present`.
#[pd_host_function(name = "mqtt::connection::is_present")]
fn mqtt_connection_is_present(connection: i64) -> bool {
    unreachable!("abi declaration only")
}

/// Host function `mqtt::connection::set_scheme`.
#[pd_host_function(name = "mqtt::connection::set_scheme")]
fn mqtt_connection_set_scheme(connection: i64, scheme: &str) {
    unreachable!("abi declaration only")
}

/// Host function `mqtt::connection::set_target`.
#[pd_host_function(name = "mqtt::connection::set_target")]
fn mqtt_connection_set_target(connection: i64, host: &str, port: i64) {
    unreachable!("abi declaration only")
}

/// Host function `mqtt::connection::set_client_id`.
#[pd_host_function(name = "mqtt::connection::set_client_id")]
fn mqtt_connection_set_client_id(connection: i64, client_id: &str) {
    unreachable!("abi declaration only")
}

/// Host function `mqtt::connection::set_username`.
#[pd_host_function(name = "mqtt::connection::set_username")]
fn mqtt_connection_set_username(connection: i64, username: &str) {
    unreachable!("abi declaration only")
}

/// Host function `mqtt::connection::set_password`.
#[pd_host_function(name = "mqtt::connection::set_password")]
fn mqtt_connection_set_password(connection: i64, password: &str) {
    unreachable!("abi declaration only")
}

/// Host function `mqtt::connection::set_keep_alive_secs`.
#[pd_host_function(name = "mqtt::connection::set_keep_alive_secs")]
fn mqtt_connection_set_keep_alive_secs(connection: i64, keep_alive_secs: i64) {
    unreachable!("abi declaration only")
}

/// Host function `mqtt::connection::set_clean_start`.
#[pd_host_function(name = "mqtt::connection::set_clean_start")]
fn mqtt_connection_set_clean_start(connection: i64, enabled: bool) {
    unreachable!("abi declaration only")
}

/// Host function `mqtt::connection::connect`.
#[pd_host_function(name = "mqtt::connection::connect")]
fn mqtt_connection_connect(connection: i64) -> bool {
    unreachable!("abi declaration only")
}

/// Host function `mqtt::connection::get_phase`.
#[pd_host_function(name = "mqtt::connection::get_phase")]
fn mqtt_connection_get_phase(connection: i64) -> String {
    unreachable!("abi declaration only")
}

/// Host function `mqtt::connection::disconnect`.
#[pd_host_function(name = "mqtt::connection::disconnect")]
fn mqtt_connection_disconnect(connection: i64, reason_code: i64, reason_text: &str) {
    unreachable!("abi declaration only")
}

/// Host function `mqtt::connection::publish_text`.
#[pd_host_function(name = "mqtt::connection::publish_text")]
fn mqtt_connection_publish_text(
    connection: i64,
    topic: &str,
    payload: &str,
    qos: i64,
    retain: bool,
) -> bool {
    unreachable!("abi declaration only")
}

/// Host function `mqtt::connection::publish_binary_base64`.
#[pd_host_function(name = "mqtt::connection::publish_binary_base64")]
fn mqtt_connection_publish_binary_base64(
    connection: i64,
    topic: &str,
    payload: &str,
    qos: i64,
    retain: bool,
) -> bool {
    unreachable!("abi declaration only")
}

/// Host function `mqtt::connection::publish_binary`.
#[pd_host_function(name = "mqtt::connection::publish_binary")]
fn mqtt_connection_publish_binary(
    connection: i64,
    topic: &str,
    payload: Bytes,
    qos: i64,
    retain: bool,
) -> bool {
    unreachable!("abi declaration only")
}

/// Host function `mqtt::connection::subscribe`.
#[pd_host_function(name = "mqtt::connection::subscribe")]
fn mqtt_connection_subscribe(connection: i64, filter: &str, qos: i64) -> bool {
    unreachable!("abi declaration only")
}

/// Host function `mqtt::connection::unsubscribe`.
#[pd_host_function(name = "mqtt::connection::unsubscribe")]
fn mqtt_connection_unsubscribe(connection: i64, filter: &str) -> bool {
    unreachable!("abi declaration only")
}

/// Host function `mqtt::connection::read_event`.
#[pd_host_function(name = "mqtt::connection::read_event")]
fn mqtt_connection_read_event(connection: i64) -> Map {
    unreachable!("abi declaration only")
}
