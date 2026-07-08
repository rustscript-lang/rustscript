/// Host function `http::response::set_header`.
#[pd_host_function(name = "http::response::set_header")]
fn http_response_set_header(name: &str, value: &str) {
    unreachable!("abi declaration only")
}

/// Host function `http::response::set_headers`.
#[pd_host_function(name = "http::response::set_headers")]
fn http_response_set_headers(headers: Value) {
    unreachable!("abi declaration only")
}

/// Host function `http::response::set_body`.
#[pd_host_function(name = "http::response::set_body")]
fn http_response_set_body(body: &str) {
    unreachable!("abi declaration only")
}

/// Host function `http::response::stream::start`.
#[pd_host_function(name = "http::response::stream::start")]
fn http_response_stream_start() {
    unreachable!("abi declaration only")
}

/// Host function `http::response::stream::write`.
#[pd_host_function(name = "http::response::stream::write")]
fn http_response_stream_write(chunk: &str) -> i64 {
    unreachable!("abi declaration only")
}

/// Host function `http::response::stream::finish`.
#[pd_host_function(name = "http::response::stream::finish")]
fn http_response_stream_finish() {
    unreachable!("abi declaration only")
}

/// Host function `http::response::set_status`.
#[pd_host_function(name = "http::response::set_status")]
fn http_response_set_status(status: i64) {
    unreachable!("abi declaration only")
}

/// Host function `http::response::apply_exchange`.
#[pd_host_function(name = "http::response::apply_exchange")]
fn http_response_apply_exchange(exchange: i64) {
    unreachable!("abi declaration only")
}

/// Host function `http::response::apply_exchange_with_headers`.
#[pd_host_function(name = "http::response::apply_exchange_with_headers")]
fn http_response_apply_exchange_with_headers(exchange: i64, headers: Value) {
    unreachable!("abi declaration only")
}

/// Host function `http::response::get_status`.
#[pd_host_function(name = "http::response::get_status")]
fn http_response_get_status() -> i64 {
    unreachable!("abi declaration only")
}

/// Host function `http::response::get_body`.
#[pd_host_function(name = "http::response::get_body")]
fn http_response_get_body() -> String {
    unreachable!("abi declaration only")
}

/// Host function `http::response::get_trailer`.
#[pd_host_function(name = "http::response::get_trailer")]
fn http_response_get_trailer(name: &str) -> String {
    unreachable!("abi declaration only")
}

/// Host function `http::response::get_trailers`.
#[pd_host_function(name = "http::response::get_trailers")]
fn http_response_get_trailers() -> Map {
    unreachable!("abi declaration only")
}

/// Host function `http::response::get_header`.
#[pd_host_function(name = "http::response::get_header")]
fn http_response_get_header(name: &str) -> String {
    unreachable!("abi declaration only")
}

/// Host function `http::response::get_headers`.
#[pd_host_function(name = "http::response::get_headers")]
fn http_response_get_headers() -> Map {
    unreachable!("abi declaration only")
}

/// Host function `http::response::add_header`.
#[pd_host_function(name = "http::response::add_header")]
fn http_response_add_header(name: &str, value: &str) {
    unreachable!("abi declaration only")
}

/// Host function `http::response::clear_header`.
#[pd_host_function(name = "http::response::clear_header")]
fn http_response_clear_header(name: &str) {
    unreachable!("abi declaration only")
}
