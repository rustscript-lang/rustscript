edge_abi_functions![
    edge_abi_function!("http::response::set_header", 2),
    edge_abi_function!("http::response::remove_header", 1),
    edge_abi_function!("http::response::set_body", 1),
    edge_abi_function!("http::response::set_status", 1),
    edge_abi_function!("http::response::get_status", 0),
    edge_abi_function!("http::response::get_body", 0),
    edge_abi_function!("http::response::get_header", 1),
    edge_abi_function!("http::response::get_headers", 0),
    edge_abi_function!("http::response::add_header", 2),
    edge_abi_function!("http::response::clear_header", 1),
    edge_abi_function!("http::response::set_headers", 1),
];
