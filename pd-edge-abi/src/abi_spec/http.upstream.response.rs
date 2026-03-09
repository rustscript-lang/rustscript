edge_abi_functions![
    edge_abi_function!("http::upstream::response::get_status", 0, [], Int),
    edge_abi_function!("http::upstream::response::get_header", 1, [String], String),
    edge_abi_function!("http::upstream::response::get_headers", 0, [], Map),
    edge_abi_function!("http::upstream::response::get_body", 0, [], String),
];
