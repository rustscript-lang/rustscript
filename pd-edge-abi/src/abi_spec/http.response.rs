edge_abi_functions![
    edge_abi_function!("http::response::set_header", 2, [String, String], Null),
    edge_abi_function!("http::response::remove_header", 1, [String], Null),
    edge_abi_function!("http::response::set_body", 1, [String], Null),
    edge_abi_function!("http::response::set_status", 1, [Int], Null),
    edge_abi_function!("http::response::get_status", 0, [], Int),
    edge_abi_function!("http::response::get_body", 0, [], String),
    edge_abi_function!("http::response::get_header", 1, [String], String),
    edge_abi_function!("http::response::get_headers", 0, [], Map),
    edge_abi_function!("http::response::add_header", 2, [String, String], Null),
    edge_abi_function!("http::response::clear_header", 1, [String], Null),
    edge_abi_function!("http::response::set_headers", 1, [Map], Null),
];
