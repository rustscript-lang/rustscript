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
