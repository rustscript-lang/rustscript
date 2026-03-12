#[pd_host_function(name = "http::upstream::response::enable_processing")]
fn http_upstream_response_enable_processing() {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::upstream::response::get_status")]
fn http_upstream_response_get_status() -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::upstream::response::get_header")]
fn http_upstream_response_get_header(name: &str) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::upstream::response::get_headers")]
fn http_upstream_response_get_headers() -> Map {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::upstream::response::get_body")]
fn http_upstream_response_get_body() -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::upstream::response::body::next_chunk")]
fn http_upstream_response_body_next_chunk(max_bytes: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "http::upstream::response::body::eof")]
fn http_upstream_response_body_eof() -> bool {
    unreachable!("abi declaration only")
}
