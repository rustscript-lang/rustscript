edge_abi_functions![
    edge_abi_function!("http::upstream::request::set_header", 2, Null),
    edge_abi_function!("http::upstream::request::remove_header", 1, Null),
    edge_abi_function!("http::upstream::request::set_method", 1, Null),
    edge_abi_function!("http::upstream::request::set_path", 1, Null),
    edge_abi_function!("http::upstream::request::set_query", 1, Null),
    edge_abi_function!("http::upstream::request::set_target", 1, Null),
    edge_abi_function!("http::upstream::request::set_body", 1, Null),
    edge_abi_function!("http::upstream::request::add_header", 2, Null),
    edge_abi_function!("http::upstream::request::clear_header", 1, Null),
    edge_abi_function!("http::upstream::request::set_headers", 1, Null),
    edge_abi_function!("http::upstream::request::set_raw_query", 1, Null),
    edge_abi_function!("http::upstream::request::set_query_arg", 2, Null),
];
