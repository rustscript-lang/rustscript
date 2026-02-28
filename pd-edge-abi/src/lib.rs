#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbiFunction {
    pub index: u16,
    pub name: &'static str,
    pub arity: u8,
}

pub const ABI_VERSION: u16 = 7;

pub const FN_HTTP_REQUEST_GET_ID: u16 = 0;
pub const FN_HTTP_REQUEST_GET_METHOD: u16 = 1;
pub const FN_HTTP_REQUEST_GET_PATH: u16 = 2;
pub const FN_HTTP_REQUEST_GET_QUERY: u16 = 3;
pub const FN_HTTP_REQUEST_GET_SCHEME: u16 = 4;
pub const FN_HTTP_REQUEST_GET_HOST: u16 = 5;
pub const FN_HTTP_REQUEST_GET_HEADER: u16 = 6;
pub const FN_HTTP_UPSTREAM_REQUEST_SET_HEADER: u16 = 7;
pub const FN_HTTP_UPSTREAM_REQUEST_REMOVE_HEADER: u16 = 8;
pub const FN_HTTP_UPSTREAM_REQUEST_SET_METHOD: u16 = 9;
pub const FN_HTTP_UPSTREAM_REQUEST_SET_PATH: u16 = 10;
pub const FN_HTTP_UPSTREAM_REQUEST_SET_QUERY: u16 = 11;
pub const FN_HTTP_REQUEST_GET_CLIENT_IP: u16 = 12;
pub const FN_HTTP_RESPONSE_SET_HEADER: u16 = 13;
pub const FN_HTTP_RESPONSE_REMOVE_HEADER: u16 = 14;
pub const FN_HTTP_RESPONSE_SET_BODY: u16 = 15;
pub const FN_HTTP_RESPONSE_SET_STATUS: u16 = 16;
pub const FN_HTTP_UPSTREAM_REQUEST_SET_TARGET: u16 = 17;
pub const FN_HTTP_RATE_LIMIT_ALLOW: u16 = 18;
pub const FN_HTTP_REQUEST_GET_HEADERS: u16 = 19;
pub const FN_HTTP_REQUEST_GET_QUERY_ARG: u16 = 20;
pub const FN_HTTP_REQUEST_GET_QUERY_ARGS: u16 = 21;
pub const FN_HTTP_REQUEST_GET_PATH_WITH_QUERY: u16 = 22;
pub const FN_HTTP_REQUEST_GET_RAW_QUERY: u16 = 23;
pub const FN_HTTP_REQUEST_GET_BODY: u16 = 24;
pub const FN_HTTP_UPSTREAM_REQUEST_SET_BODY: u16 = 25;
pub const FN_HTTP_UPSTREAM_REQUEST_ADD_HEADER: u16 = 26;
pub const FN_HTTP_UPSTREAM_REQUEST_CLEAR_HEADER: u16 = 27;
pub const FN_HTTP_UPSTREAM_REQUEST_SET_HEADERS: u16 = 28;
pub const FN_HTTP_UPSTREAM_REQUEST_SET_RAW_QUERY: u16 = 29;
pub const FN_HTTP_UPSTREAM_REQUEST_SET_QUERY_ARG: u16 = 30;
pub const FN_HTTP_REQUEST_GET_HTTP_VERSION: u16 = 31;
pub const FN_HTTP_REQUEST_GET_PORT: u16 = 32;
pub const FN_HTTP_RESPONSE_GET_STATUS: u16 = 33;
pub const FN_HTTP_RESPONSE_GET_BODY: u16 = 34;
pub const FN_HTTP_RESPONSE_GET_HEADER: u16 = 35;
pub const FN_HTTP_RESPONSE_GET_HEADERS: u16 = 36;
pub const FN_HTTP_RESPONSE_ADD_HEADER: u16 = 37;
pub const FN_HTTP_RESPONSE_CLEAR_HEADER: u16 = 38;
pub const FN_HTTP_RESPONSE_SET_HEADERS: u16 = 39;
pub const FN_HTTP_UPSTREAM_RESPONSE_GET_STATUS: u16 = 40;
pub const FN_HTTP_UPSTREAM_RESPONSE_GET_HEADER: u16 = 41;
pub const FN_HTTP_UPSTREAM_RESPONSE_GET_HEADERS: u16 = 42;
pub const FN_HTTP_UPSTREAM_RESPONSE_GET_BODY: u16 = 43;

pub const FUNCTIONS: [AbiFunction; 44] = [
    AbiFunction {
        index: FN_HTTP_REQUEST_GET_ID,
        name: "http::request::get_id",
        arity: 0,
    },
    AbiFunction {
        index: FN_HTTP_REQUEST_GET_METHOD,
        name: "http::request::get_method",
        arity: 0,
    },
    AbiFunction {
        index: FN_HTTP_REQUEST_GET_PATH,
        name: "http::request::get_path",
        arity: 0,
    },
    AbiFunction {
        index: FN_HTTP_REQUEST_GET_QUERY,
        name: "http::request::get_query",
        arity: 0,
    },
    AbiFunction {
        index: FN_HTTP_REQUEST_GET_SCHEME,
        name: "http::request::get_scheme",
        arity: 0,
    },
    AbiFunction {
        index: FN_HTTP_REQUEST_GET_HOST,
        name: "http::request::get_host",
        arity: 0,
    },
    AbiFunction {
        index: FN_HTTP_REQUEST_GET_HEADER,
        name: "http::request::get_header",
        arity: 1,
    },
    AbiFunction {
        index: FN_HTTP_UPSTREAM_REQUEST_SET_HEADER,
        name: "http::upstream::request::set_header",
        arity: 2,
    },
    AbiFunction {
        index: FN_HTTP_UPSTREAM_REQUEST_REMOVE_HEADER,
        name: "http::upstream::request::remove_header",
        arity: 1,
    },
    AbiFunction {
        index: FN_HTTP_UPSTREAM_REQUEST_SET_METHOD,
        name: "http::upstream::request::set_method",
        arity: 1,
    },
    AbiFunction {
        index: FN_HTTP_UPSTREAM_REQUEST_SET_PATH,
        name: "http::upstream::request::set_path",
        arity: 1,
    },
    AbiFunction {
        index: FN_HTTP_UPSTREAM_REQUEST_SET_QUERY,
        name: "http::upstream::request::set_query",
        arity: 1,
    },
    AbiFunction {
        index: FN_HTTP_REQUEST_GET_CLIENT_IP,
        name: "http::request::get_client_ip",
        arity: 0,
    },
    AbiFunction {
        index: FN_HTTP_RESPONSE_SET_HEADER,
        name: "http::response::set_header",
        arity: 2,
    },
    AbiFunction {
        index: FN_HTTP_RESPONSE_REMOVE_HEADER,
        name: "http::response::remove_header",
        arity: 1,
    },
    AbiFunction {
        index: FN_HTTP_RESPONSE_SET_BODY,
        name: "http::response::set_body",
        arity: 1,
    },
    AbiFunction {
        index: FN_HTTP_RESPONSE_SET_STATUS,
        name: "http::response::set_status",
        arity: 1,
    },
    AbiFunction {
        index: FN_HTTP_UPSTREAM_REQUEST_SET_TARGET,
        name: "http::upstream::request::set_target",
        arity: 1,
    },
    AbiFunction {
        index: FN_HTTP_RATE_LIMIT_ALLOW,
        name: "http::rate_limit::allow",
        arity: 3,
    },
    AbiFunction {
        index: FN_HTTP_REQUEST_GET_HEADERS,
        name: "http::request::get_headers",
        arity: 0,
    },
    AbiFunction {
        index: FN_HTTP_REQUEST_GET_QUERY_ARG,
        name: "http::request::get_query_arg",
        arity: 1,
    },
    AbiFunction {
        index: FN_HTTP_REQUEST_GET_QUERY_ARGS,
        name: "http::request::get_query_args",
        arity: 0,
    },
    AbiFunction {
        index: FN_HTTP_REQUEST_GET_PATH_WITH_QUERY,
        name: "http::request::get_path_with_query",
        arity: 0,
    },
    AbiFunction {
        index: FN_HTTP_REQUEST_GET_RAW_QUERY,
        name: "http::request::get_raw_query",
        arity: 0,
    },
    AbiFunction {
        index: FN_HTTP_REQUEST_GET_BODY,
        name: "http::request::get_body",
        arity: 0,
    },
    AbiFunction {
        index: FN_HTTP_UPSTREAM_REQUEST_SET_BODY,
        name: "http::upstream::request::set_body",
        arity: 1,
    },
    AbiFunction {
        index: FN_HTTP_UPSTREAM_REQUEST_ADD_HEADER,
        name: "http::upstream::request::add_header",
        arity: 2,
    },
    AbiFunction {
        index: FN_HTTP_UPSTREAM_REQUEST_CLEAR_HEADER,
        name: "http::upstream::request::clear_header",
        arity: 1,
    },
    AbiFunction {
        index: FN_HTTP_UPSTREAM_REQUEST_SET_HEADERS,
        name: "http::upstream::request::set_headers",
        arity: 1,
    },
    AbiFunction {
        index: FN_HTTP_UPSTREAM_REQUEST_SET_RAW_QUERY,
        name: "http::upstream::request::set_raw_query",
        arity: 1,
    },
    AbiFunction {
        index: FN_HTTP_UPSTREAM_REQUEST_SET_QUERY_ARG,
        name: "http::upstream::request::set_query_arg",
        arity: 2,
    },
    AbiFunction {
        index: FN_HTTP_REQUEST_GET_HTTP_VERSION,
        name: "http::request::get_http_version",
        arity: 0,
    },
    AbiFunction {
        index: FN_HTTP_REQUEST_GET_PORT,
        name: "http::request::get_port",
        arity: 0,
    },
    AbiFunction {
        index: FN_HTTP_RESPONSE_GET_STATUS,
        name: "http::response::get_status",
        arity: 0,
    },
    AbiFunction {
        index: FN_HTTP_RESPONSE_GET_BODY,
        name: "http::response::get_body",
        arity: 0,
    },
    AbiFunction {
        index: FN_HTTP_RESPONSE_GET_HEADER,
        name: "http::response::get_header",
        arity: 1,
    },
    AbiFunction {
        index: FN_HTTP_RESPONSE_GET_HEADERS,
        name: "http::response::get_headers",
        arity: 0,
    },
    AbiFunction {
        index: FN_HTTP_RESPONSE_ADD_HEADER,
        name: "http::response::add_header",
        arity: 2,
    },
    AbiFunction {
        index: FN_HTTP_RESPONSE_CLEAR_HEADER,
        name: "http::response::clear_header",
        arity: 1,
    },
    AbiFunction {
        index: FN_HTTP_RESPONSE_SET_HEADERS,
        name: "http::response::set_headers",
        arity: 1,
    },
    AbiFunction {
        index: FN_HTTP_UPSTREAM_RESPONSE_GET_STATUS,
        name: "http::upstream::response::get_status",
        arity: 0,
    },
    AbiFunction {
        index: FN_HTTP_UPSTREAM_RESPONSE_GET_HEADER,
        name: "http::upstream::response::get_header",
        arity: 1,
    },
    AbiFunction {
        index: FN_HTTP_UPSTREAM_RESPONSE_GET_HEADERS,
        name: "http::upstream::response::get_headers",
        arity: 0,
    },
    AbiFunction {
        index: FN_HTTP_UPSTREAM_RESPONSE_GET_BODY,
        name: "http::upstream::response::get_body",
        arity: 0,
    },
];

pub const HOST_FUNCTION_COUNT: u16 = FUNCTIONS.len() as u16;

fn functions_by_name() -> &'static std::collections::HashMap<&'static str, &'static AbiFunction> {
    static LOOKUP: std::sync::OnceLock<
        std::collections::HashMap<&'static str, &'static AbiFunction>,
    > = std::sync::OnceLock::new();
    LOOKUP.get_or_init(|| {
        let mut map = std::collections::HashMap::with_capacity(FUNCTIONS.len());
        for function in FUNCTIONS.iter() {
            map.insert(function.name, function);
        }
        map
    })
}

pub fn function_by_index(index: u16) -> Option<&'static AbiFunction> {
    FUNCTIONS.iter().find(|function| function.index == index)
}

pub fn function_by_name(name: &str) -> Option<&'static AbiFunction> {
    functions_by_name().get(name).copied()
}

pub fn abi_json() -> &'static str {
    include_str!("../abi.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn functions_are_dense_and_ordered() {
        for (position, function) in FUNCTIONS.iter().enumerate() {
            assert_eq!(function.index as usize, position);
        }
        assert_eq!(HOST_FUNCTION_COUNT as usize, FUNCTIONS.len());
    }

    #[test]
    fn abi_json_contains_declared_functions() {
        let manifest = abi_json();
        assert!(manifest.contains("\"abi_version\": 7"));
        for function in FUNCTIONS {
            assert!(manifest.contains(function.name));
        }
    }
}
