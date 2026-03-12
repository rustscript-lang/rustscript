#[pd_host_function(name = "http::exchange::new")]
fn http_exchange_new() -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::exchange::default_upstream")]
fn http_exchange_default_upstream() -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::exchange::send")]
fn http_exchange_send(exchange: i64) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::exchange::set_header")]
fn http_exchange_set_header(exchange: i64, name: &str, value: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::exchange::remove_header")]
fn http_exchange_remove_header(exchange: i64, name: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::exchange::set_method")]
fn http_exchange_set_method(exchange: i64, method: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::exchange::set_path")]
fn http_exchange_set_path(exchange: i64, path: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::exchange::set_query")]
fn http_exchange_set_query(exchange: i64, query: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::exchange::set_target")]
fn http_exchange_set_target(exchange: i64, target: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::exchange::set_body")]
fn http_exchange_set_body(exchange: i64, body: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::exchange::add_header")]
fn http_exchange_add_header(exchange: i64, name: &str, value: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::exchange::clear_header")]
fn http_exchange_clear_header(exchange: i64, name: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::exchange::set_headers")]
fn http_exchange_set_headers(exchange: i64, headers: Map) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::exchange::set_raw_query")]
fn http_exchange_set_raw_query(exchange: i64, query: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::exchange::set_query_arg")]
fn http_exchange_set_query_arg(exchange: i64, name: &str, value: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::exchange::get_status")]
fn http_exchange_get_status(exchange: i64) -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::exchange::get_header")]
fn http_exchange_get_header(exchange: i64, name: &str) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::exchange::get_headers")]
fn http_exchange_get_headers(exchange: i64) -> Map {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::exchange::get_body")]
fn http_exchange_get_body(exchange: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::exchange::body::next_chunk")]
fn http_exchange_body_next_chunk(exchange: i64, max_bytes: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::exchange::body::eof")]
fn http_exchange_body_eof(exchange: i64) -> bool {
    unreachable!("abi declaration only")
}
