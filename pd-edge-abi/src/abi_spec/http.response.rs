#[pd_host_function(name = "http::response::set_header")]
fn http_response_set_header(name: &str, value: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::response::set_body")]
fn http_response_set_body(body: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::response::set_status")]
fn http_response_set_status(status: i64) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::response::get_status")]
fn http_response_get_status() -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::response::get_body")]
fn http_response_get_body() -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::response::get_header")]
fn http_response_get_header(name: &str) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::response::get_headers")]
fn http_response_get_headers() -> Map {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::response::add_header")]
fn http_response_add_header(name: &str, value: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::response::clear_header")]
fn http_response_clear_header(name: &str) {
    unreachable!("abi declaration only")
}
